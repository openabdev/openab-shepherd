//! The herdr-core (hc) backend: maps [`TerminalBackend`] onto herdr-core 0.1.19's
//! JSON-RPC methods. This module and `rpc.rs` are the only places that know hc
//! method names, hc JSON field names, and hc error shapes.
//!
//! Trait call -> herdr-core method:
//!   run       -> terminal.run
//!   get       -> terminal.get
//!   list      -> terminal.list
//!   read      -> terminal.read
//!   send      -> terminal.send (never `force`)
//!   keys      -> terminal.keys (no condition: hc has none)
//!   wait      -> terminal.wait
//!   interrupt -> terminal.interrupt

mod rpc;

#[cfg(test)]
pub mod fake;
#[cfg(test)]
mod tests;

use crate::backend::{
    dialog_kind, BackendError, Capabilities, ErrorKind, InputGuard, InterruptOutcome,
    InterruptResult, Key, KeysResult, ProgramStatus, ReadLimit, ReadSource, RunSpec, ScreenText,
    SendResult, State, TerminalBackend, TerminalInfo, TerminalList, TerminalRef, WaitReason,
    WaitResult,
};
use rpc::{HerdrClient, HerdrError, DEFAULT_TIMEOUT, WAIT_SOCKET_SLACK};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::Duration;

/// The interrupt socket timeout when the caller gives none (hc's own default
/// wait is shorter).
const INTERRUPT_DEFAULT_SOCKET: Duration = Duration::from_secs(10);

/// hc 0.1.19: `terminal.send` refuses while blocked unless forced, and takes an
/// `if` limited to idle or working. `terminal.keys` takes no condition.
const CAPABILITIES: Capabilities = Capabilities {
    conditional_input: true,
    conditional_keys: false,
    client_command: true,
    client_cwd: true,
    screen_text: true,
};

#[derive(Debug, Clone)]
pub struct HcBackend {
    client: HerdrClient,
}

impl HcBackend {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            client: HerdrClient::new(socket),
        }
    }

    fn call(
        &self,
        method: &str,
        params: Vec<(&str, Value)>,
        timeout: Duration,
    ) -> Result<Value, BackendError> {
        let params: Map<String, Value> = params
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect();
        self.client.call(method, params, timeout).map_err(map_error)
    }
}

impl TerminalBackend for HcBackend {
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn run(&self, spec: &RunSpec) -> Result<TerminalInfo, BackendError> {
        if let Some(profile) = &spec.profile {
            return Err(unsupported(format!("launch profile {profile:?}")));
        }
        let result = self.call(
            "terminal.run",
            vec![
                ("argv", json!(spec.argv)),
                ("line", json!(spec.line)),
                ("cwd", json!(spec.cwd)),
                ("name", json!(spec.name)),
            ],
            DEFAULT_TIMEOUT,
        )?;
        terminal_info(&result)
    }

    fn get(&self, t: &TerminalRef) -> Result<TerminalInfo, BackendError> {
        let result = self.call(
            "terminal.get",
            vec![("terminal", json!(t.as_str()))],
            DEFAULT_TIMEOUT,
        )?;
        terminal_info(&result)
    }

    fn list(&self, all: bool) -> Result<TerminalList, BackendError> {
        let result = self.call("terminal.list", vec![("all", json!(all))], DEFAULT_TIMEOUT)?;
        let terminals = result
            .get("terminals")
            .and_then(Value::as_array)
            .ok_or_else(|| protocol("terminal.list reply has no `terminals` array"))?
            .iter()
            .map(terminal_info)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(TerminalList {
            terminals,
            hidden_finished: result.get("hidden_finished").and_then(Value::as_u64),
        })
    }

    fn read(&self, t: &TerminalRef, history_lines: u32) -> Result<ScreenText, BackendError> {
        let result = self.call(
            "terminal.read",
            vec![
                ("terminal", json!(t.as_str())),
                ("lines", json!(history_lines)),
            ],
            DEFAULT_TIMEOUT,
        )?;
        Ok(ScreenText {
            text: str_field(&result, "text").unwrap_or_default(),
            complete: result.get("complete").and_then(Value::as_bool),
            source: str_field(&result, "how").map(|how| match how.as_str() {
                "screen" => ReadSource::Screen,
                "history" => ReadSource::History,
                "stitched" => ReadSource::Stitched,
                _ => ReadSource::Other,
            }),
            limit: str_field(&result, "why").map(|why| match why.as_str() {
                "blocked" => ReadLimit::Blocked,
                _ => ReadLimit::Other,
            }),
        })
    }

    fn send(
        &self,
        t: &TerminalRef,
        text: &str,
        guard: &InputGuard,
    ) -> Result<SendResult, BackendError> {
        // hc refuses while blocked by default; `force` would lift that, and is
        // never sent.
        if !guard.refuse_when_blocked {
            return Err(unsupported(
                "typing into a pane without refusing while it is blocked",
            ));
        }
        if guard.if_kind.is_some() || guard.expect_turn.is_some() {
            return Err(unsupported(
                "a blocked-kind or turn condition on typed input",
            ));
        }
        let if_state = match &guard.if_state {
            None => Value::Null,
            Some(State::Idle) => json!("idle"),
            Some(State::Working) => json!("working"),
            Some(other) => {
                return Err(unsupported(format!(
                    "typing only when the status is {}",
                    other.as_str()
                )))
            }
        };
        let result = self.call(
            "terminal.send",
            vec![
                ("terminal", json!(t.as_str())),
                ("text", json!(text)),
                ("if", if_state),
            ],
            DEFAULT_TIMEOUT,
        )?;
        Ok(SendResult {
            turn: result.get("turn").and_then(Value::as_u64),
            started: result.get("started").and_then(Value::as_bool),
            status: str_field(&result, "status").map(|word| State::parse(&word)),
        })
    }

    /// hc's terminal.keys has no condition, so `guard` is not enforced here
    /// (`conditional_keys` is false); Shepherd checks before calling.
    fn keys(
        &self,
        t: &TerminalRef,
        keys: &[Key],
        _guard: &InputGuard,
    ) -> Result<KeysResult, BackendError> {
        let names = keys
            .iter()
            .map(|key| match key {
                Key::Enter => Ok("enter"),
                Key::Up => Ok("up"),
                Key::Down => Ok("down"),
                Key::Esc => Ok("esc"),
                Key::CtrlC => Ok("ctrl+c"),
                Key::Other => Err(unsupported("sending an unknown key")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let result = self.call(
            "terminal.keys",
            vec![("terminal", json!(t.as_str())), ("keys", json!(names))],
            DEFAULT_TIMEOUT,
        )?;
        Ok(KeysResult {
            status: str_field(&result, "status").map(|word| State::parse(&word)),
            turn: result.get("turn").and_then(Value::as_u64),
        })
    }

    fn wait(&self, t: &TerminalRef, timeout: Duration) -> Result<WaitResult, BackendError> {
        let result = self.call(
            "terminal.wait",
            vec![
                ("terminal", json!(t.as_str())),
                ("timeout", json!(timeout.as_secs_f64())),
            ],
            timeout + WAIT_SOCKET_SLACK,
        )?;
        // A reply without a boolean `ended` is not evidence of idle.
        let Some(ended) = result.get("ended").and_then(Value::as_bool) else {
            return Err(protocol("terminal.wait reply has no boolean `ended`"));
        };
        let reason = str_field(&result, "reason").map(|reason| match reason.as_str() {
            "exit" => WaitReason::Exit,
            "status" => WaitReason::Status,
            "timeout" => WaitReason::Timeout,
            _ => WaitReason::Other,
        });
        // terminal.wait reports the status word only, never its kind.
        let status = str_field(&result, "status").map(|word| ProgramStatus {
            state: State::parse(&word),
            kind: None,
        });
        Ok(WaitResult {
            ended,
            reason,
            status,
            exit_code: result.get("exit_code").and_then(Value::as_i64),
            turn: result.get("turn").and_then(Value::as_u64),
        })
    }

    fn interrupt(
        &self,
        t: &TerminalRef,
        timeout: Option<Duration>,
    ) -> Result<InterruptResult, BackendError> {
        let socket = timeout.unwrap_or(INTERRUPT_DEFAULT_SOCKET) + WAIT_SOCKET_SLACK;
        let result = self.call(
            "terminal.interrupt",
            vec![
                ("terminal", json!(t.as_str())),
                ("timeout", json!(timeout.map(|d| d.as_secs_f64()))),
            ],
            socket,
        )?;
        let key = match str_field(&result, "key").as_deref() {
            Some("esc") => Key::Esc,
            Some("ctrl+c") => Key::CtrlC,
            Some(_) => Key::Other,
            None => return Err(protocol("terminal.interrupt reply has no `key`")),
        };
        let outcome = match str_field(&result, "outcome").as_deref() {
            Some("stopped") => InterruptOutcome::Stopped,
            Some("exited") => InterruptOutcome::Exited,
            Some("still_working") => InterruptOutcome::StillWorking,
            Some("unknown") => InterruptOutcome::Unknown,
            Some(_) => InterruptOutcome::Other,
            None => return Err(protocol("terminal.interrupt reply has no `outcome`")),
        };
        Ok(InterruptResult { key, outcome })
    }
}

/// An hc terminal object (terminal.get, terminal.run, terminal.list entries).
fn terminal_info(value: &Value) -> Result<TerminalInfo, BackendError> {
    let id = str_field(value, "address")
        .filter(|address| !address.is_empty())
        .ok_or_else(|| protocol(format!("terminal object has no `address`: {value}")))?;
    Ok(TerminalInfo {
        id,
        name: str_field(value, "name"),
        live: value.get("state").and_then(Value::as_str) == Some("live"),
        exit_code: value.get("exit_code").and_then(Value::as_i64),
        status: program_status(value.get("status")),
        reported: value
            .get("reported")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        turn: value.get("turn").and_then(Value::as_u64),
        status_ms: value.get("status_ms").and_then(Value::as_u64),
        argv: value.get("argv").and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
        cwd: str_field(value, "cwd"),
    })
}

/// An hc Status record (`{state, kind, app, msg, ...}`) or null.
fn program_status(value: Option<&Value>) -> Option<ProgramStatus> {
    let value = value.filter(|v| v.is_object())?;
    Some(ProgramStatus {
        state: State::parse(value.get("state").and_then(Value::as_str).unwrap_or("")),
        kind: str_field(value, "kind").map(|kind| dialog_kind(&kind)),
    })
}

/// hc error kind (`data.kind`, or a local class from `rpc.rs`) -> Shepherd's.
fn map_error(err: HerdrError) -> BackendError {
    let kind = match err.kind.as_deref() {
        Some("unavailable" | "unreachable") => ErrorKind::Unavailable,
        Some("timeout") => ErrorKind::Timeout,
        Some("not_found" | "machine_unknown") => ErrorKind::NotFound,
        Some("name_taken") => ErrorKind::NameTaken,
        // hc's blocked refusal names the dialog kind only in its message text.
        Some("blocked") => ErrorKind::Blocked { kind: None },
        Some("status_mismatch") => ErrorKind::StatusMismatch,
        Some("finished") => ErrorKind::Finished,
        Some("invalid_params") => ErrorKind::InvalidInput,
        Some("transport" | "protocol") => ErrorKind::Protocol,
        _ => ErrorKind::Other,
    };
    let detail = match err.code {
        Some(code) => format!("herdr-core: {err} [code {code}]"),
        None => format!("herdr-core: {err}"),
    };
    BackendError::new(kind, detail)
}

fn unsupported(op: impl Into<String>) -> BackendError {
    let op = op.into();
    BackendError::new(
        ErrorKind::Unsupported(op.clone()),
        format!("herdr-core: not supported: {op}"),
    )
}

fn protocol(detail: impl Into<String>) -> BackendError {
    BackendError::new(
        ErrorKind::Protocol,
        format!("herdr-core: {}", detail.into()),
    )
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[cfg(test)]
mod mapping_tests {
    use super::*;

    #[test]
    fn hc_terminal_object_is_normalized() {
        let hc = json!({
            "address": "t9", "name": "work", "state": "live", "argv": ["kiro-cli", "chat"],
            "cwd": "/tmp/w", "cols": 80, "rows": 24, "created_ms": 1, "screen": "exact",
            "labels": {}, "identity": "ghostty 1.4.0", "exit_code": null, "reported": true,
            "turn": 4, "status_ms": 1700000000000u64,
            "status": {"app": "kiro-cli", "state": "blocked", "kind": "permission", "msg": "x"},
            "foreground": {"pid": 1, "name": "kiro-cli", "argv": [], "cwd": "/tmp/w"}
        });
        let info = terminal_info(&hc).unwrap();
        assert_eq!(
            info,
            TerminalInfo {
                id: "t9".to_string(),
                name: Some("work".to_string()),
                live: true,
                exit_code: None,
                status: Some(ProgramStatus {
                    state: State::Blocked,
                    kind: Some("permission".to_string()),
                }),
                reported: true,
                turn: Some(4),
                status_ms: Some(1_700_000_000_000),
                argv: Some(vec!["kiro-cli".to_string(), "chat".to_string()]),
                cwd: Some("/tmp/w".to_string()),
            }
        );
        // The MCP shape has Shepherd's keys only.
        let out = info.to_json();
        let mut keys: Vec<&str> = out
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "argv",
                "cwd",
                "exit_code",
                "live",
                "name",
                "reported",
                "status",
                "status_ms",
                "turn"
            ]
        );
        assert_eq!(
            out["status"],
            json!({"state": "blocked", "kind": "permission"})
        );

        let finished = terminal_info(&json!({
            "address": "t3", "name": null, "state": "finished", "exit_code": 2,
            "status": null, "reported": false, "turn": 0}))
        .unwrap();
        assert!(!finished.live);
        assert_eq!(finished.exit_code, Some(2));
        assert_eq!(finished.status, None);
        assert_eq!(finished.name, None);

        let other = program_status(Some(&json!({"state": "thinking"}))).unwrap();
        assert_eq!(other.state, State::Other);
        assert_eq!(other.to_json()["state"], "other");

        let err = terminal_info(&json!({"name": "x"})).unwrap_err();
        assert_eq!(err.code(), "backend_error");
    }

    #[test]
    fn send_guards_hc_cannot_enforce_are_unsupported_without_a_call() {
        // Nothing listens here: a call would fail as unavailable, not unsupported.
        let backend = HcBackend::new("/tmp/shep-hc-guard-absent.sock");
        let t = TerminalRef::new("work");
        for guard in [
            InputGuard::default(),
            InputGuard {
                refuse_when_blocked: true,
                expect_turn: Some(3),
                ..InputGuard::default()
            },
            InputGuard {
                refuse_when_blocked: true,
                if_state: Some(State::Blocked),
                ..InputGuard::default()
            },
        ] {
            let err = backend.send(&t, "hi", &guard).unwrap_err();
            assert_eq!(err.code(), "unsupported", "{guard:?}");
        }
        let spec = RunSpec {
            profile: Some("kiro".to_string()),
            ..RunSpec::default()
        };
        assert_eq!(backend.run(&spec).unwrap_err().code(), "unsupported");
    }
}
