//! MCP message handling (JSON-RPC 2.0 over Streamable HTTP, JSON responses only).
//!
//! The server is stateless: each POST carries one message and gets one JSON reply,
//! or 202 for a notification. There is no SSE stream and no session id.

use crate::clients::{self, Caller};
use crate::tools::{self, Shepherd};
use serde_json::{json, Map, Value};

pub const SERVER_NAME: &str = "openab-shepherd";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: [&str; 2] = ["2025-06-18", "2025-03-26"];

const CODE_INVALID_REQUEST: i64 = -32600;
const CODE_METHOD_NOT_FOUND: i64 = -32601;
const CODE_INVALID_PARAMS: i64 = -32602;

/// Handle one parsed JSON-RPC message for an authenticated `caller`. Returns
/// `None` when no reply is due. This is the only entry production code uses,
/// so no tool call runs without a caller and the scope check. A batch is
/// refused outright; nothing else reaches `tools::call` except `tools/call`.
pub fn handle_message_as(sh: &Shepherd, caller: &Caller, message: &Value) -> Option<Value> {
    if message.is_array() {
        return Some(rpc_error(
            Value::Null,
            CODE_INVALID_REQUEST,
            "batch requests are not supported",
        ));
    }
    let Some(object) = message.as_object() else {
        return Some(rpc_error(
            Value::Null,
            CODE_INVALID_REQUEST,
            "request must be a JSON object",
        ));
    };
    let id = object.get("id").cloned();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A reply from the client (result or error, no method) needs no answer.
        if object.contains_key("result") || object.contains_key("error") {
            return None;
        }
        return Some(rpc_error(
            id.unwrap_or(Value::Null),
            CODE_INVALID_REQUEST,
            "missing method",
        ));
    };
    let Some(id) = id else {
        // A notification, such as notifications/initialized: no reply.
        return None;
    };
    Some(match dispatch(sh, caller, method, object.get("params")) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, text)) => rpc_error(id, code, &text),
    })
}

/// [`handle_message_as`] for a caller holding every scope, so the tests of the
/// tools themselves run unchanged. Not compiled into the server.
#[cfg(test)]
pub fn handle_message(sh: &Shepherd, message: &Value) -> Option<Value> {
    handle_message_as(sh, &Caller::legacy(), message)
}

fn dispatch(
    sh: &Shepherd,
    caller: &Caller,
    method: &str,
    params: Option<&Value>,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": visible_tools(caller)})),
        "tools/call" => call_tool(sh, caller, params),
        other => Err((CODE_METHOD_NOT_FOUND, format!("method not found: {other}"))),
    }
}

/// The tool definitions the caller may use at all. A convenience for the
/// client's model: `authorize` in [`call_tool`] is the check.
fn visible_tools(caller: &Caller) -> Value {
    let all = tools::definitions();
    let shown: Vec<Value> = all
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|tool| {
                    tool.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| caller.may_list(name))
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Value::Array(shown)
}

fn initialize(params: Option<&Value>) -> Value {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str);
    let version = match requested {
        Some(v) if SUPPORTED_PROTOCOLS.contains(&v) => v,
        _ => LATEST_PROTOCOL,
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
    })
}

fn call_tool(
    sh: &Shepherd,
    caller: &Caller,
    params: Option<&Value>,
) -> Result<Value, (i64, String)> {
    let params = params.and_then(Value::as_object).ok_or((
        CODE_INVALID_PARAMS,
        "tools/call needs an object of params".to_string(),
    ))?;
    let name = params.get("name").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "tools/call needs a tool name".to_string(),
    ))?;
    if !tools::is_tool(name) {
        return Err((CODE_INVALID_PARAMS, format!("unknown tool: {name}")));
    }
    let args: Map<String, Value> = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(args)) => args.clone(),
        Some(_) => {
            return Err((
                CODE_INVALID_PARAMS,
                "arguments must be an object".to_string(),
            ))
        }
    };
    let audit = sh.audit();
    if let Err(denied) = clients::authorize(caller, name, &args) {
        if let Some(audit) = audit {
            audit.denied(caller.client(), name, &args, &denied.reason);
        }
        return Ok(tool_result(Err(denied.to_tool_error())));
    }
    let pending = match audit {
        Some(audit) => match audit.before(caller.client(), name, &args) {
            Ok(pending) => Some(pending),
            Err(unavailable) => return Ok(tool_result(Err(unavailable.to_tool_error()))),
        },
        None => None,
    };
    let outcome = tools::call(sh, name, &args);
    if let (Some(audit), Some(pending)) = (audit, pending) {
        audit.after(caller.client(), name, &args, pending, &outcome);
    }
    Ok(tool_result(outcome))
}

/// Tool failures are results with `isError: true`, not JSON-RPC errors, so the
/// client can show the reason to the model.
fn tool_result(outcome: Result<Value, tools::ToolError>) -> Value {
    match outcome {
        Ok(value) => {
            let structured = if value.is_object() {
                value
            } else {
                json!({ "result": value })
            };
            let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
            json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": structured,
                "isError": false,
            })
        }
        Err(tools::ToolError { text, data }) => json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": data,
            "isError": true,
        }),
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::LaunchPolicy;
    use crate::testutil::FakeBackend;

    fn call_msg(name: &str, arguments: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}})
    }

    /// The tool result object from a `tools/call` reply.
    fn tool_out(reply: Option<Value>) -> Value {
        reply.expect("reply")["result"].clone()
    }

    /// A Shepherd over a backend with no panes. The fake records every call, so
    /// a test can show validation ran before any backend call.
    fn fake_client() -> (Shepherd, FakeBackend) {
        let fake = FakeBackend::new(true);
        (
            Shepherd::with_policy(fake.clone(), LaunchPolicy::arbitrary()),
            fake,
        )
    }

    fn text_of(out: &Value) -> String {
        out["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn initialize_negotiates_protocol_and_advertises_tools() {
        let (client, _fake) = fake_client();
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                         "params": {"protocolVersion": "2025-03-26"}});
        let out = handle_message(&client, &msg).unwrap();
        assert_eq!(out["result"]["protocolVersion"], "2025-03-26");
        assert!(out["result"]["capabilities"]["tools"].is_object());
        assert_eq!(out["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[test]
    fn initialized_notification_gets_no_reply() {
        let (client, _fake) = fake_client();
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_message(&client, &msg).is_none());
    }

    #[test]
    fn tools_list_has_the_nine_tools() {
        let (client, _fake) = fake_client();
        let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let out = handle_message(&client, &msg).unwrap();
        let names: Vec<&str> = out["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, tools::TOOL_NAMES);
    }

    #[test]
    fn unknown_method_and_unknown_tool_are_jsonrpc_errors() {
        let (client, _fake) = fake_client();
        let out = handle_message(
            &client,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}),
        )
        .unwrap();
        assert_eq!(out["error"]["code"], CODE_METHOD_NOT_FOUND);
        let out = handle_message(&client, &call_msg("delete_everything", json!({}))).unwrap();
        assert_eq!(out["error"]["code"], CODE_INVALID_PARAMS);
    }

    #[test]
    fn start_pane_requires_exactly_one_of_argv_or_line() {
        let (client, _fake) = fake_client();
        let none = tool_out(handle_message(&client, &call_msg("start_pane", json!({}))));
        assert_eq!(none["isError"], true);
        assert!(text_of(&none).contains("exactly one"));
        let both = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"argv": ["sh"], "line": "echo hi"})),
        ));
        assert_eq!(both["isError"], true);
        assert!(text_of(&both).contains("exactly one"));
    }

    #[test]
    fn start_pane_rejects_whitespace_only_line_before_any_call() {
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": "   "})),
        ));
        assert_eq!(out["isError"], true);
        let text = text_of(&out);
        assert!(text.contains("empty"), "{text}");
        // Validation ran first: no backend call was made.
        assert!(fake.calls().is_empty(), "{text}");
    }

    #[test]
    fn start_pane_rejects_oversized_and_control_only_argv_before_any_call() {
        let (client, fake) = fake_client();
        let big = "x".repeat(4097);
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": big})),
        ));
        assert!(text_of(&out).contains("limit is 4096"));
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"argv": ["sh", "\u{1b}"]})),
        ));
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("control character U+001B"));
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn start_pane_refuses_control_character_in_line_before_any_call() {
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": "echo \u{1b}[31mhi\u{1b}[0m"})),
        ));
        assert_eq!(out["isError"], true);
        let text = text_of(&out);
        assert!(
            text.contains("line: input contains control character U+001B"),
            "{text}"
        );
        assert!(fake.calls().is_empty(), "{text}");
    }

    #[test]
    fn start_pane_refuses_newline_in_line_before_any_call() {
        let (client, _fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": "echo a\necho b"})),
        ));
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("U+000A"), "{}", text_of(&out));
    }

    #[test]
    fn send_input_refuses_control_character_before_any_call() {
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("send_input", json!({"terminal": "work", "text": "ls\u{7}"})),
        ));
        assert_eq!(out["isError"], true);
        let text = text_of(&out);
        assert!(
            text.contains("text: input contains control character U+0007"),
            "{text}"
        );
        assert!(fake.calls().is_empty(), "{text}");
    }

    #[test]
    fn wait_status_rejects_out_of_range_timeout() {
        let (client, _fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("wait_status", json!({"terminal": "work", "timeout": 0})),
        ));
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("between"));
    }

    #[test]
    fn answer_dialog_refuses_rule_choices_before_any_call() {
        let (client, fake) = fake_client();
        for choice in ["create_allow_rule", "allow_rule", "deny_rule", "Allow once"] {
            let out = tool_out(handle_message(
                &client,
                &call_msg(
                    "answer_dialog",
                    json!({"terminal": "work", "choice": choice}),
                ),
            ));
            assert_eq!(out["isError"], true);
            let text = text_of(&out);
            assert!(text.contains("allow_once or deny_once"), "{text}");
            assert!(fake.calls().is_empty(), "{text}");
        }
    }

    #[test]
    fn batch_and_non_object_are_invalid_requests() {
        let (client, _fake) = fake_client();
        let out = handle_message(&client, &json!([])).unwrap();
        assert_eq!(out["error"]["code"], CODE_INVALID_REQUEST);
        let out = handle_message(&client, &json!("text")).unwrap();
        assert_eq!(out["error"]["code"], CODE_INVALID_REQUEST);
    }

    #[test]
    fn validation_errors_carry_the_invalid_input_code() {
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("send_input", json!({"terminal": "work"})),
        ));
        assert_eq!(out["isError"], true);
        assert_eq!(out["structuredContent"]["error"], "invalid_input");
        assert_eq!(out["structuredContent"]["message"], "text is required");
        assert_eq!(text_of(&out), "invalid_input: text is required");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn backend_errors_carry_a_shepherd_code() {
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("get_status", json!({"terminal": "missing"})),
        ));
        assert_eq!(fake.calls(), ["get"]);
        assert_eq!(out["isError"], true);
        let data = &out["structuredContent"];
        assert_eq!(data["error"], "not_found");
        assert_eq!(data["terminal"], "missing");
        assert_eq!(data["backend_detail"], "fake: no such pane");
        assert!(!text_of(&out).contains("fake:"), "{}", text_of(&out));
    }

    #[test]
    fn handle_message_as_filters_the_list_and_denies_without_the_scope() {
        use crate::clients::{ProfileAccess, Scope};
        let (client, fake) = fake_client();
        let caller = Caller::new("watcher", [Scope::Observe], ProfileAccess::Only(Vec::new()));
        let list = handle_message_as(
            &client,
            &caller,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["get_status", "wait_status", "list_panes"]);
        let out = tool_out(handle_message_as(
            &client,
            &caller,
            &call_msg("send_input", json!({"terminal": "work", "text": "ls"})),
        ));
        assert_eq!(out["isError"], true);
        assert_eq!(out["structuredContent"]["error"], "forbidden");
        assert!(fake.calls().is_empty());
    }
}

/// Issue #4, PR 1: authorization and audit seen through `handle_message_as`,
/// over the project's `FakeBackend`. Run with `cargo test issue4_pr1`.
#[cfg(test)]
mod issue4_pr1_mcp {
    use super::*;
    use crate::agent::Agent;
    use crate::audit::testing::{failing_audit, fixed_clock, memory_audit};
    use crate::audit::{Audit, AuditSink};
    use crate::backend::State;
    use crate::clients::{ProfileAccess, Scope};
    use crate::dialog::fixtures::dialog_on_allow;
    use crate::profiles::LaunchPolicy;
    use crate::testutil::{FakeBackend, TempDir};
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    const ENV_CANARY: &str = "CANARY-env-5d1f";
    const TEXT_CANARY: &str = "CANARY-text-7a20";
    const ARGV_CANARY: &str = "CANARY-argv-c4e8";
    const ARG_ENV_CANARY: &str = "CANARY-argenv-91b3";
    const SCREEN_CANARY: &str = "CANARY-screen-3e6d";

    fn msg(name: &str, arguments: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}})
    }

    fn run(sh: &Shepherd, caller: &Caller, name: &str, arguments: Value) -> Value {
        handle_message_as(sh, caller, &msg(name, arguments))
            .expect("a reply")
            .get("result")
            .expect("a tool result, not a JSON-RPC error")
            .clone()
    }

    fn code(out: &Value) -> String {
        out["structuredContent"]["error"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn caller_with(scopes: &[Scope], profiles: &[&str]) -> Caller {
        Caller::new(
            "tester",
            scopes.iter().copied(),
            ProfileAccess::Only(profiles.iter().map(|p| p.to_string()).collect()),
        )
    }

    fn without(scope: Scope) -> Caller {
        caller_with(
            &Scope::ALL
                .into_iter()
                .filter(|s| *s != scope)
                .collect::<Vec<_>>(),
            &["ci", "other"],
        )
    }

    /// A pane `work` blocked on a permission dialog, with a runnable backend.
    fn dialog_fake() -> FakeBackend {
        let mut fake = FakeBackend::new(false);
        fake.caps.conditional_keys = true;
        fake.add_pane("work", State::Blocked, Some("permission"));
        {
            let mut state = fake.state.lock().unwrap();
            state.screen = Some(dialog_on_allow());
            state.runnable = true;
        }
        fake
    }

    /// An idle pane `work` whose screen carries a canary.
    fn idle_fake() -> FakeBackend {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        {
            let mut state = fake.state.lock().unwrap();
            state.screen = Some(format!("output {SCREEN_CANARY}\n"));
            state.runnable = true;
        }
        fake
    }

    fn policy(dir: &Path, arbitrary: bool) -> LaunchPolicy {
        let text = json!({
            "allow_arbitrary_command": arbitrary,
            "profiles": [
                {"name": "ci", "argv": ["/bin/sh"], "cwds": [dir],
                 "env": {"CI_KEY": "env:KEY_SOURCE"}},
                {"name": "other", "argv": ["/bin/sh"], "cwds": [dir]}
            ]
        });
        LaunchPolicy::from_json(&text.to_string())
            .unwrap()
            .with_env_lookup(|key| (key == "KEY_SOURCE").then(|| ENV_CANARY.to_string()))
    }

    fn shepherd(fake: &FakeBackend, dir: &Path, arbitrary: bool) -> Shepherd {
        let sh = Shepherd::with_policy(fake.clone(), policy(dir, arbitrary));
        sh.assume_agent("work", Agent::Kiro);
        sh
    }

    /// One call per tool with arguments that pass validation, and the scope
    /// the design table names for it. Written out here, apart from the
    /// production mapping, so a change to either one is noticed.
    fn table(dir: &Path) -> Vec<(&'static str, Value, Scope)> {
        vec![
            ("list_panes", json!({}), Scope::Observe),
            ("get_status", json!({"terminal": "work"}), Scope::Observe),
            ("wait_status", json!({"terminal": "work"}), Scope::Observe),
            ("read_output", json!({"terminal": "work"}), Scope::Read),
            (
                "send_input",
                json!({"terminal": "work", "text": "hi"}),
                Scope::Control,
            ),
            (
                "interrupt_pane",
                json!({"terminal": "work"}),
                Scope::Control,
            ),
            (
                "start_pane",
                json!({"profile": "ci", "cwd": dir, "name": "n"}),
                Scope::Start,
            ),
            (
                "start_pane",
                json!({"argv": ["true"], "name": "n"}),
                Scope::StartArbitrary,
            ),
            (
                "answer_dialog",
                json!({"terminal": "work", "choice": "allow_once"}),
                Scope::Dialog,
            ),
            ("remove_pane", json!({"terminal": "work"}), Scope::Remove),
        ]
    }

    // ---- group 1: tool x scope ----

    #[test]
    fn the_table_names_every_tool_once_or_more() {
        let dir = TempDir::new("i4-table-names");
        let mut named: Vec<&str> = table(dir.path()).iter().map(|row| row.0).collect();
        named.sort_unstable();
        named.dedup();
        let mut all = tools::TOOL_NAMES.to_vec();
        all.sort_unstable();
        assert_eq!(named, all, "a new tool needs a row and a scope");
    }

    #[test]
    fn every_tool_by_every_scope_is_allowed_only_with_its_own_scope() {
        let dir = TempDir::new("i4-table");
        for (tool, arguments, needed) in table(dir.path()) {
            for scope in Scope::ALL {
                let fake = dialog_fake();
                let sh = shepherd(&fake, dir.path(), true);
                let caller = caller_with(&[scope], &["ci"]);
                let out = run(&sh, &caller, tool, arguments.clone());
                if scope == needed {
                    assert_ne!(code(&out), "forbidden", "{tool} with {scope}: {out}");
                } else {
                    assert_eq!(out["isError"], true, "{tool} with {scope}");
                    assert_eq!(code(&out), "forbidden", "{tool} with {scope}: {out}");
                    assert!(
                        fake.calls().is_empty(),
                        "{tool} with {scope} reached the backend: {:?}",
                        fake.calls()
                    );
                }
            }
        }
    }

    #[test]
    fn no_scope_implies_another() {
        let dir = TempDir::new("i4-implied");
        for (tool, arguments, needed) in table(dir.path()) {
            // Everything except the scope the tool needs: still refused.
            let fake = dialog_fake();
            let sh = shepherd(&fake, dir.path(), true);
            let others: Vec<Scope> = Scope::ALL.into_iter().filter(|s| *s != needed).collect();
            let out = run(&sh, &caller_with(&others, &["ci"]), tool, arguments);
            assert_eq!(code(&out), "forbidden", "{tool} without {needed}");
            assert!(fake.calls().is_empty(), "{tool}");
        }
    }

    #[test]
    fn a_call_to_an_unknown_or_respelled_tool_does_nothing() {
        let dir = TempDir::new("i4-unknown");
        let fake = dialog_fake();
        let sh = shepherd(&fake, dir.path(), true);
        for name in [
            "exec",
            "Send_Input",
            " send_input",
            "send_input ",
            "send_input\0",
            "tools/call",
        ] {
            let reply = handle_message_as(
                &sh,
                &Caller::legacy(),
                &msg(name, json!({"terminal": "work", "text": "x"})),
            )
            .unwrap();
            assert_eq!(reply["error"]["code"], -32602, "{name:?}: {reply}");
        }
        // An empty name is a malformed request, not a call.
        let reply = handle_message_as(&sh, &Caller::legacy(), &msg("", json!({}))).unwrap();
        assert!(reply.get("error").is_some(), "{reply}");
        assert!(fake.calls().is_empty());
    }

    // ---- group 3: what a missing scope or allowlist entry prevents ----

    #[test]
    fn a_client_without_dialog_sends_zero_keys() {
        let dir = TempDir::new("i4-dialog");
        let fake = dialog_fake();
        let sh = shepherd(&fake, dir.path(), true);
        let args = json!({"terminal": "work", "choice": "allow_once"});
        let out = run(&sh, &without(Scope::Dialog), "answer_dialog", args.clone());
        assert_eq!(code(&out), "forbidden", "{out}");
        assert_eq!(out["structuredContent"]["scope"], "dialog");
        assert!(fake.state.lock().unwrap().keys.is_empty(), "zero keys sent");
        assert!(fake.calls().is_empty(), "no backend call at all");
        // Control of the pane is not a way around it: typing into a blocked
        // dialog is refused for every caller, and no key goes out either.
        let typed = run(
            &sh,
            &without(Scope::Dialog),
            "send_input",
            json!({"terminal": "work", "text": "1"}),
        );
        assert_eq!(typed["isError"], true);
        assert!(fake.state.lock().unwrap().sent.is_empty());
        assert!(fake.state.lock().unwrap().keys.is_empty());
        // The same call with the scope does answer, so the test can fail.
        let ok = run(
            &sh,
            &caller_with(&[Scope::Dialog], &[]),
            "answer_dialog",
            args,
        );
        assert_eq!(ok["isError"], false, "{ok}");
        assert_eq!(fake.state.lock().unwrap().keys.len(), 1);
    }

    #[test]
    fn without_start_arbitrary_argv_and_line_never_start_a_pane() {
        let dir = TempDir::new("i4-arbitrary");
        let fake = dialog_fake();
        // The server allows arbitrary commands: the client still may not.
        let sh = shepherd(&fake, dir.path(), true);
        let caller = without(Scope::StartArbitrary);
        for arguments in [
            json!({"argv": ["true"]}),
            json!({"line": "true"}),
            json!({"argv": null}),
            json!({"line": null}),
            json!({"argv": []}),
            json!({"profile": "ci", "cwd": dir.path(), "argv": ["true"]}),
            json!({"profile": null, "line": "true"}),
        ] {
            let out = run(&sh, &caller, "start_pane", arguments.clone());
            assert_eq!(code(&out), "forbidden", "{arguments}: {out}");
        }
        assert!(fake.state.lock().unwrap().runs.is_empty());
        assert!(fake.calls().is_empty());
        // With the scope, argv passes; the control a test needs to be able to fail.
        let allowed = caller_with(&[Scope::StartArbitrary], &[]);
        let out = run(
            &sh,
            &allowed,
            "start_pane",
            json!({"argv": ["true"], "name": "a"}),
        );
        assert_eq!(out["isError"], false, "{out}");
        assert_eq!(fake.state.lock().unwrap().runs.len(), 1);
    }

    #[test]
    fn start_arbitrary_does_not_override_a_server_that_forbids_arbitrary_commands() {
        let dir = TempDir::new("i4-ceiling");
        let fake = dialog_fake();
        let sh = shepherd(&fake, dir.path(), false);
        let out = run(
            &sh,
            &Caller::legacy(),
            "start_pane",
            json!({"argv": ["true"]}),
        );
        assert_eq!(code(&out), "profile_required", "{out}");
        assert!(fake.state.lock().unwrap().runs.is_empty());
    }

    #[test]
    fn a_profile_outside_the_clients_allowlist_is_denied() {
        let dir = TempDir::new("i4-profiles");
        let fake = dialog_fake();
        let sh = shepherd(&fake, dir.path(), false);
        let only_ci = caller_with(&[Scope::Start], &["ci"]);
        for profile in ["other", "", "CI", "ci ", "nope"] {
            let out = run(
                &sh,
                &only_ci,
                "start_pane",
                json!({"profile": profile, "cwd": dir.path()}),
            );
            assert_eq!(code(&out), "forbidden", "{profile:?}: {out}");
        }
        // A profile name that is not a string is no way around the list.
        let out = run(
            &sh,
            &only_ci,
            "start_pane",
            json!({"profile": ["ci"], "cwd": dir.path()}),
        );
        assert_eq!(code(&out), "forbidden", "{out}");
        assert!(fake.state.lock().unwrap().runs.is_empty());
        // An empty list allows none.
        let none = caller_with(&[Scope::Start], &[]);
        let out = run(
            &sh,
            &none,
            "start_pane",
            json!({"profile": "ci", "cwd": dir.path()}),
        );
        assert_eq!(code(&out), "forbidden", "{out}");
        assert!(fake.state.lock().unwrap().runs.is_empty());
        // The listed one starts, with the profile's own environment.
        let out = run(
            &sh,
            &only_ci,
            "start_pane",
            json!({"profile": "ci", "cwd": dir.path(), "name": "p"}),
        );
        assert_eq!(out["isError"], false, "{out}");
        let runs = fake.state.lock().unwrap().runs.clone();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].profile.as_deref(), Some("ci"));
    }

    #[test]
    fn a_denial_does_not_leak_the_arguments_in_its_reply() {
        let dir = TempDir::new("i4-denial-reply");
        let fake = idle_fake();
        let sh = shepherd(&fake, dir.path(), true);
        let out = run(
            &sh,
            &caller_with(&[Scope::Observe], &[]),
            "send_input",
            json!({"terminal": "work", "text": TEXT_CANARY}),
        );
        assert!(!out.to_string().contains(TEXT_CANARY), "{out}");
    }

    // ---- group 6: JSON-RPC shapes that must not reach a tool ----

    #[test]
    fn batches_and_other_methods_cannot_reach_a_tool() {
        let dir = TempDir::new("i4-rpc");
        let fake = dialog_fake();
        let sh = shepherd(&fake, dir.path(), true);
        let caller = caller_with(&[Scope::Observe], &[]);
        let call = msg("remove_pane", json!({"terminal": "work"}));
        // A batch, even of one call, even as a legacy caller.
        for who in [&caller, &Caller::legacy()] {
            let reply = handle_message_as(&sh, who, &json!([call.clone(), call.clone()])).unwrap();
            assert_eq!(reply["error"]["code"], -32600, "{reply}");
            let reply = handle_message_as(&sh, who, &json!([])).unwrap();
            assert_eq!(reply["error"]["code"], -32600, "{reply}");
        }
        // The tool name as a method, and near misses of `tools/call`.
        for method in [
            "remove_pane",
            "tools/remove_pane",
            "tools/call/remove_pane",
            "tools/callx",
            "Tools/Call",
            " tools/call",
            "tools/call ",
            "tools/invoke",
            "rpc.call",
            "",
        ] {
            let message = json!({"jsonrpc": "2.0", "id": 1, "method": method,
                "params": {"name": "remove_pane", "arguments": {"terminal": "work"}}});
            let reply = handle_message_as(&sh, &Caller::legacy(), &message).unwrap();
            assert_eq!(reply["error"]["code"], -32601, "{method:?}: {reply}");
        }
        // A tools/call that is a notification (no id) gets no reply and no call.
        let notification = json!({"jsonrpc": "2.0", "method": "tools/call",
            "params": {"name": "remove_pane", "arguments": {"terminal": "work"}}});
        assert!(handle_message_as(&sh, &Caller::legacy(), &notification).is_none());
        // A reply-shaped message is ignored.
        let reply_like = json!({"jsonrpc": "2.0", "id": 1, "result": {"name": "remove_pane"}});
        assert!(handle_message_as(&sh, &Caller::legacy(), &reply_like).is_none());
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        // Malformed params never reach a tool either.
        for params in [
            json!(null),
            json!([]),
            json!("x"),
            json!({}),
            json!({"name": 5}),
        ] {
            let message =
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params});
            let reply = handle_message_as(&sh, &Caller::legacy(), &message).unwrap();
            assert!(reply.get("error").is_some(), "{params}: {reply}");
        }
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn the_legacy_caller_sees_every_tool_and_the_old_wrapper_agrees() {
        let dir = TempDir::new("i4-legacy");
        let fake = idle_fake();
        let sh = shepherd(&fake, dir.path(), true);
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let reply = handle_message_as(&sh, &Caller::legacy(), &list).unwrap();
        let names: Vec<&str> = reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, tools::TOOL_NAMES);
        assert_eq!(handle_message(&sh, &list), Some(reply));
        // Every tool the old suites call is allowed for it, whatever the args.
        let dir2 = TempDir::new("i4-legacy-table");
        for (tool, arguments, _) in table(dir2.path()) {
            let out = run(&sh, &Caller::legacy(), tool, arguments);
            assert_ne!(code(&out), "forbidden", "{tool}: {out}");
        }
    }

    // ---- group 5: audit around the tools ----

    /// A sink that notes how many backend calls had happened at each write.
    struct Observing {
        fake: FakeBackend,
        seen: Arc<Mutex<Vec<(String, usize)>>>,
    }

    impl AuditSink for Observing {
        fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            let record: Value = serde_json::from_slice(line).expect("a JSON line");
            let phase = record["phase"].as_str().unwrap_or("?").to_string();
            self.seen
                .lock()
                .unwrap()
                .push((phase, self.fake.calls().len()));
            Ok(())
        }
    }

    fn state_changing(dir: &Path) -> Vec<(&'static str, Value)> {
        vec![
            ("send_input", json!({"terminal": "work", "text": "hi"})),
            (
                "answer_dialog",
                json!({"terminal": "work", "choice": "allow_once"}),
            ),
            (
                "start_pane",
                json!({"profile": "ci", "cwd": dir, "name": "n"}),
            ),
            ("remove_pane", json!({"terminal": "work"})),
        ]
    }

    #[test]
    fn the_intent_line_is_written_before_any_backend_call() {
        let dir = TempDir::new("i4-order");
        for (tool, arguments) in state_changing(dir.path()) {
            let fake = dialog_fake();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let audit = Audit::new(
                Box::new(Observing {
                    fake: fake.clone(),
                    seen: seen.clone(),
                }),
                0,
                false,
                fixed_clock(1_700_000_000),
            );
            let sh = shepherd(&fake, dir.path(), false).with_audit(Arc::new(audit));
            let out = run(&sh, &Caller::legacy(), tool, arguments);
            assert_ne!(code(&out), "audit_unavailable", "{tool}");
            assert!(!fake.calls().is_empty(), "{tool} did reach the backend");
            let seen = seen.lock().unwrap().clone();
            let first = &seen[0];
            assert!(
                first.0 == "intent" || first.0 == "call",
                "{tool}: first line is {first:?}"
            );
            assert_eq!(
                first.1, 0,
                "{tool}: the first line came after a backend call"
            );
            if tool == "answer_dialog" || tool == "start_pane" {
                assert_eq!(first.0, "intent", "{tool}");
                let last = seen.last().unwrap();
                assert_eq!(last.0, "outcome", "{tool}");
                assert!(last.1 > 0, "{tool}: the outcome came before the work");
            }
        }
    }

    #[test]
    fn a_failing_sink_means_zero_backend_calls_for_the_four_state_changing_tools() {
        let dir = TempDir::new("i4-failclosed");
        for (tool, arguments) in state_changing(dir.path()) {
            let fake = dialog_fake();
            let sh = shepherd(&fake, dir.path(), false).with_audit(Arc::new(failing_audit()));
            let out = run(&sh, &Caller::legacy(), tool, arguments);
            assert_eq!(out["isError"], true, "{tool}");
            assert_eq!(code(&out), "audit_unavailable", "{tool}: {out}");
            let state = fake.state.lock().unwrap();
            assert!(state.calls.is_empty(), "{tool}: {:?}", state.calls);
            assert!(state.keys.is_empty() && state.sent.is_empty() && state.runs.is_empty());
            assert_eq!(state.panes.len(), 1, "{tool}: the pane is untouched");
        }
    }

    #[test]
    fn a_failing_sink_still_lets_reads_and_interrupt_pane_work() {
        let dir = TempDir::new("i4-failopen");
        for (tool, arguments) in [
            ("get_status", json!({"terminal": "work"})),
            ("read_output", json!({"terminal": "work"})),
        ] {
            let fake = idle_fake();
            let sh = shepherd(&fake, dir.path(), false).with_audit(Arc::new(failing_audit()));
            let out = run(&sh, &Caller::legacy(), tool, arguments);
            assert_eq!(out["isError"], false, "{tool}: {out}");
        }
        // interrupt_pane reaches the backend (the fake does not implement it,
        // so the call ends in `unsupported`, which is not an audit refusal).
        let fake = idle_fake();
        let sh = shepherd(&fake, dir.path(), false).with_audit(Arc::new(failing_audit()));
        let out = run(
            &sh,
            &Caller::legacy(),
            "interrupt_pane",
            json!({"terminal": "work"}),
        );
        assert_ne!(code(&out), "audit_unavailable", "{out}");
        assert!(fake.calls().contains(&"interrupt"), "{:?}", fake.calls());
    }

    #[test]
    fn denied_calls_are_recorded_and_still_refused_when_the_sink_fails() {
        let dir = TempDir::new("i4-denied");
        let fake = idle_fake();
        let (audit, sink) = memory_audit();
        let sh = shepherd(&fake, dir.path(), true).with_audit(Arc::new(audit));
        let caller = caller_with(&[Scope::Observe], &[]);
        let out = run(
            &sh,
            &caller,
            "start_pane",
            json!({"argv": ["x", ARGV_CANARY], "env": {"K": ARG_ENV_CANARY}}),
        );
        assert_eq!(code(&out), "forbidden");
        let records = sink.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["phase"], "denied");
        assert_eq!(records[0]["client"], "tester");
        assert!(fake.calls().is_empty());
        for canary in [ARGV_CANARY, ARG_ENV_CANARY] {
            assert!(!sink.bytes().contains(canary), "{canary}");
        }
        let sh = shepherd(&fake, dir.path(), true).with_audit(Arc::new(failing_audit()));
        let out = run(&sh, &caller, "remove_pane", json!({"terminal": "work"}));
        assert_eq!(code(&out), "forbidden", "a denial is not an audit failure");
    }

    #[test]
    fn secrets_never_reach_the_audit_file_through_the_real_tools() {
        let dir = TempDir::new("i4-canary");
        let audit_dir = TempDir::new("i4-canary-audit");
        std::fs::set_permissions(audit_dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = audit_dir.path().join("audit.jsonl");
        let audit = Audit::open_file(&path, dir.path(), fixed_clock(1_700_000_000)).unwrap();
        let sh_idle = {
            let fake = idle_fake();
            (
                shepherd(&fake, dir.path(), true).with_audit(Arc::new(audit)),
                fake,
            )
        };
        let (sh, fake) = sh_idle;
        let who = Caller::legacy();
        // Typed text.
        let out = run(
            &sh,
            &who,
            "send_input",
            json!({"terminal": "work", "text": format!("password {TEXT_CANARY}")}),
        );
        assert_eq!(out["isError"], false, "{out}");
        // A profile start: its env value comes from the server's environment.
        let out = run(
            &sh,
            &who,
            "start_pane",
            json!({"profile": "ci", "cwd": dir.path(), "name": "p"}),
        );
        assert_eq!(out["isError"], false, "{out}");
        // Arbitrary argv, line and env.
        let out = run(
            &sh,
            &who,
            "start_pane",
            json!({"argv": ["/bin/echo", ARGV_CANARY], "name": "a",
                   "env": {"OTHER_KEY": ARG_ENV_CANARY}}),
        );
        assert_eq!(out["isError"], false, "{out}");
        let out = run(
            &sh,
            &who,
            "start_pane",
            json!({"line": format!("echo {ARGV_CANARY}"), "name": "l"}),
        );
        assert_eq!(out["isError"], false, "{out}");
        // A screen read, and a dialog answered from the screen.
        let out = run(&sh, &who, "read_output", json!({"terminal": "work"}));
        assert_eq!(out["isError"], false, "{out}");
        assert!(text_canary(&out, SCREEN_CANARY), "the reply has the screen");
        fake.state.lock().unwrap().screen =
            Some(format!("{}\n{SCREEN_CANARY}\n", dialog_on_allow()));
        fake.state.lock().unwrap().panes[0].status = Some(crate::backend::ProgramStatus {
            state: State::Blocked,
            kind: Some("permission".to_string()),
        });
        let out = run(
            &sh,
            &who,
            "answer_dialog",
            json!({"terminal": "work", "choice": "allow_once"}),
        );
        assert_eq!(out["isError"], false, "{out}");
        assert_eq!(fake.state.lock().unwrap().keys.len(), 1);
        // A failure whose message carries the argument back.
        let _ = run(
            &sh,
            &who,
            "send_input",
            json!({"terminal": "gone", "text": TEXT_CANARY}),
        );

        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(bytes.lines().count() >= 8, "{bytes}");
        for canary in [
            ENV_CANARY,
            TEXT_CANARY,
            ARGV_CANARY,
            ARG_ENV_CANARY,
            SCREEN_CANARY,
            "shepherd_marker",
            "requires approval",
        ] {
            assert!(
                !bytes.contains(canary),
                "{canary} reached the audit file:\n{bytes}"
            );
        }
        // Names are fine, and the shape is one JSON object per line.
        let mut last = 0;
        for line in bytes.lines() {
            let record: Value = serde_json::from_str(line).unwrap();
            let seq = record["seq"].as_u64().unwrap();
            assert!(seq > last);
            last = seq;
            assert_eq!(record["client"], "legacy");
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn text_canary(out: &Value, canary: &str) -> bool {
        out.to_string().contains(canary)
    }

    #[test]
    fn client_identity_in_the_audit_comes_from_the_matched_record_only() {
        let dir = TempDir::new("i4-identity");
        let fake = idle_fake();
        let (audit, sink) = memory_audit();
        let sh = shepherd(&fake, dir.path(), true).with_audit(Arc::new(audit));
        let caller = caller_with(&[Scope::Observe], &[]);
        // Arguments that claim an identity change nothing.
        let out = run(
            &sh,
            &caller,
            "get_status",
            json!({"terminal": "work", "client": "root", "actor": "system:watchdog"}),
        );
        assert_eq!(out["isError"], false, "{out}");
        let records = sink.records();
        assert_eq!(records[0]["client"], "tester");
        assert!(!sink.bytes().contains("system:watchdog"));
    }
}
