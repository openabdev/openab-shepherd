//! The eight MCP tools. They talk only to [`TerminalBackend`]; the safety rules
//! (input validation, the pane lock, refusing to type into a blocked pane, and
//! answer_dialog's checks) are enforced here, whatever the backend.
//!
//! Tool -> backend calls. Every tool that takes `terminal` resolves it first:
//! a `get` by name that must return a pane with exactly that name (see
//! `resolve`). Later calls use the resolved pane's id.
//!   start_pane      -> run
//!   send_input      -> get (x2, see lock_pane), send
//!   read_output     -> get, read
//!   get_status      -> get
//!   wait_status     -> get, wait, then get when it ends blocked without a kind
//!   list_panes      -> list
//!   interrupt_pane  -> get (x2, see lock_pane), interrupt
//!   answer_dialog   -> get, read, keys

use crate::ansi::strip_terminal_escapes;
use crate::backend::{
    dialog_kind, status_json, BackendError, ErrorKind, InputGuard, Key, KeysResult, RunSpec, State,
    TerminalBackend, TerminalInfo, TerminalRef, WaitReason,
};
use crate::dialog::{self, Choice};
use crate::input::{validate_input, validate_name};
use crate::panelock::{PaneGuard, PaneLocks};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub const TOOL_NAMES: [&str; 8] = [
    "start_pane",
    "send_input",
    "read_output",
    "get_status",
    "wait_status",
    "list_panes",
    "interrupt_pane",
    "answer_dialog",
];
pub const READ_MAX_LINES: u64 = 10_000;
pub const WAIT_MIN_SECONDS: f64 = 1.0;
pub const WAIT_MAX_SECONDS: f64 = 1800.0;
pub const WAIT_DEFAULT_SECONDS: f64 = 30.0;
pub const INTERRUPT_MAX_SECONDS: f64 = 60.0;
/// answer_dialog re-reads the screen up to this many times, waiting for the
/// cursor to land on the target after the moves (the backend may deliver keys
/// after a quiet period, and the program redraws after that).
pub const CONFIRM_READS: usize = 5;
pub const CONFIRM_INTERVAL: Duration = Duration::from_millis(200);
/// kiro-cli 3.1.0 (V3) reports OSC 7501 blocked/permission before it has drawn
/// the dialog. answer_dialog waits up to this long for the dialog to parse,
/// re-reading the screen every [`RENDER_POLL_INTERVAL`].
pub const RENDER_WAIT: Duration = Duration::from_secs(3);
pub const RENDER_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Screen reads in the render wait: one at once, then one per interval.
pub const RENDER_READS: usize =
    (RENDER_WAIT.as_millis() / RENDER_POLL_INTERVAL.as_millis()) as usize + 1;
/// Lines of screen text returned with an answer_dialog refusal.
const SEEN_TAIL_LINES: usize = 40;
/// What a caller does after typing was refused because the pane is blocked.
const BLOCKED_NEXT: &str = "read_output, then answer_dialog; nothing was typed";

pub type Args = Map<String, Value>;

/// The tools' shared state: one backend, and the per-pane locks. Clones share
/// both.
#[derive(Clone)]
pub struct Shepherd {
    backend: Arc<dyn TerminalBackend>,
    locks: Arc<PaneLocks>,
}

impl Shepherd {
    pub fn new(backend: impl TerminalBackend + 'static) -> Self {
        Self {
            backend: Arc::new(backend),
            locks: Arc::new(PaneLocks::default()),
        }
    }

    #[cfg(test)]
    pub fn pane_locks(&self) -> &Arc<PaneLocks> {
        &self.locks
    }
}

/// A tool failure: the text shown to the MCP client, and structured data
/// (returned as `structuredContent` alongside `isError: true`). `data.error` is
/// Shepherd's error code and `data.message` repeats the text without it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub text: String,
    pub data: Value,
}

impl ToolError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            text: format!("{code}: {message}"),
            data: json!({"error": code, "message": message}),
        }
    }

    fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.data[key] = value.into();
        self
    }

    /// Put `prefix` in front of the message.
    fn context(self, prefix: &str) -> Self {
        let code = self.data["error"].as_str().unwrap_or_default().to_string();
        let message = format!(
            "{prefix}: {}",
            self.data["message"].as_str().unwrap_or_default()
        );
        Self {
            text: format!("{code}: {message}"),
            data: {
                let mut data = self.data;
                data["message"] = json!(message);
                data
            },
        }
    }
}

/// An argument that fails validation.
impl From<String> for ToolError {
    fn from(text: String) -> Self {
        Self::new("invalid_input", text)
    }
}

pub fn is_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// The `tools/list` payload.
pub fn definitions() -> Value {
    json!([
        {
            "name": "start_pane",
            "description": "Start a program in a new pane. Give exactly one of argv (a program and its arguments) or line (a shell line run by sh -c). Later calls address the pane by its name: give one, or Shepherd picks one (pane-<hex>) and returns it. Returns the pane (name, live, status, reported, turn, status_ms, exit_code, argv, cwd). Input is trimmed and limited to 4096 bytes per field. Control characters (except TAB) are refused, not removed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "argv": {"type": "array", "items": {"type": "string"}, "description": "Program and arguments; non-empty."},
                    "line": {"type": "string", "description": "Shell line run by sh -c."},
                    "cwd": {"type": "string", "description": "Working directory."},
                    "name": {"type": "string", "description": "Pane name: 1-32 of a-z, 0-9, '-', '_', '.'. Optional; Shepherd picks one when it is missing."}
                }
            }
        },
        {
            "name": "send_input",
            "description": "Type text into a pane and press Enter. Sendable when the pane is idle or done (kiro-cli stays in done after a turn). Input is trimmed and limited to 4096 bytes. Control characters (except TAB) are refused, not removed. While the program is blocked on a dialog, nothing is typed and the error code is pane_blocked, with kind saying which dialog (for example permission). Read the screen, then use answer_dialog. This tool never forces input.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name, as start_pane or list_panes returned it."},
                    "text": {"type": "string"}
                },
                "required": ["terminal", "text"]
            }
        },
        {
            "name": "read_output",
            "description": "Read a pane's screen (lines=0) or its last N lines of history. ANSI CSI and OSC escape sequences are removed from text. TUI redraw noise is not removed. Returns text, complete, source (screen, history, stitched, or other), and reason (why only the screen was read: blocked, other, or null).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "lines": {"type": "integer", "minimum": 0, "maximum": READ_MAX_LINES, "default": 0}
                },
                "required": ["terminal"]
            }
        },
        {
            "name": "get_status",
            "description": "Return a pane: its status ({state, kind} with state idle, working, done, blocked, error, or other; null when it never reported one), its reported flag, live, turn, status_ms, and exit_code. reported is false when the program never sent an OSC 7501 status.",
            "inputSchema": {
                "type": "object",
                "properties": {"terminal": {"type": "string"}},
                "required": ["terminal"]
            }
        },
        {
            "name": "wait_status",
            "description": "Wait until a pane stops working, or check in after timeout seconds. Branch on outcome: done (the turn finished), blocked (waiting on a dialog; kind says which, for example permission), timeout (a check-in: still running), exited (the program ended), or other (any other status or reason; read status). turn_complete is true only for done and exited; a blocked pane has not completed its turn even though its turn number advanced. A program that never reports status counts as working until it exits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "timeout": {"type": "number", "minimum": WAIT_MIN_SECONDS, "maximum": WAIT_MAX_SECONDS, "default": WAIT_DEFAULT_SECONDS}
                },
                "required": ["terminal"]
            }
        },
        {
            "name": "list_panes",
            "description": "List panes, each in the same shape start_pane returns. all=true includes finished panes; hidden_finished counts the ones left out.",
            "inputSchema": {
                "type": "object",
                "properties": {"all": {"type": "boolean", "default": false}}
            }
        },
        {
            "name": "interrupt_pane",
            "description": "Send the stop key to a pane and report which key the backend chose (key: esc, ctrl+c, or other) and the outcome: stopped, exited, still_working, unknown, or other. It does not escalate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "timeout": {"type": "number", "minimum": WAIT_MIN_SECONDS, "maximum": INTERRUPT_MAX_SECONDS}
                },
                "required": ["terminal"]
            }
        },
        {
            "name": "answer_dialog",
            "description": "Answer a permission dialog in a pane that is blocked with kind permission: allow_once or deny_once. Rule-creating options are not offered. The tool reads the screen, waiting up to 3 s (re-reading every 200 ms) for the dialog to render, because kiro-cli reports blocked before it draws the dialog; between reads it checks the pane is still blocked with kind permission in the same turn, and if the dialog never renders it refuses without sending anything. It then moves the cursor with up/down keys, reads the screen again until the cursor is confirmed on the chosen option, checks again that the pane is still blocked with kind permission in the same turn, and only then presses Enter. On any mismatch it sends nothing more and returns what it saw. Other Shepherd calls that type into the same pane wait until it finishes. A person at the terminal or another client of the terminal backend can still act between the last check and Enter, because the current backend has no conditional key press. The dialog is parsed from screen text; tested only on kiro-cli 3.1.0 (V3) shell permission dialogs. Follow with wait_status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "choice": {"type": "string", "enum": ["allow_once", "deny_once"]}
                },
                "required": ["terminal", "choice"]
            }
        }
    ])
}

/// Run one tool. `Err` carries what is shown to the MCP client with `isError`.
pub fn call(sh: &Shepherd, name: &str, args: &Args) -> Result<Value, ToolError> {
    match name {
        "start_pane" => start_pane(sh, args),
        "send_input" => send_input(sh, args),
        "read_output" => read_output(sh, args),
        "get_status" => get_status(sh, args),
        "wait_status" => wait_status(sh, args),
        "list_panes" => list_panes(sh, args),
        "interrupt_pane" => interrupt_pane(sh, args),
        "answer_dialog" => answer_dialog(sh, args),
        other => Err(format!("unknown tool: {other}").into()),
    }
}

fn start_pane(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let argv_items = match args.get("argv") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(items),
        Some(_) => return Err("argv must be a list of strings".to_string().into()),
    };
    let line = optional_str(args, "line")?;
    let has_argv = argv_items.is_some_and(|items| !items.is_empty());
    if has_argv == line.is_some() {
        return Err("give exactly one of argv (non-empty list) or line"
            .to_string()
            .into());
    }

    // Same input rules as send_input, applied to every string that reaches the pane.
    let mut spec = RunSpec::default();
    if let Some(items) = argv_items.filter(|items| !items.is_empty()) {
        let mut argv = Vec::with_capacity(items.len());
        for item in items {
            let text = item
                .as_str()
                .ok_or_else(|| "argv must be a list of strings".to_string())?;
            argv.push(validate_input(text).map_err(|err| format!("argv: {err}"))?);
        }
        spec.argv = Some(argv);
    }
    if let Some(line) = line {
        spec.line = Some(validate_input(line).map_err(|err| format!("line: {err}"))?);
    }
    if let Some(cwd) = optional_str(args, "cwd")? {
        spec.cwd = Some(validate_input(cwd).map_err(|err| format!("cwd: {err}"))?);
    }
    // Every pane Shepherd starts has a name: callers address panes by name.
    let chosen_name = optional_str(args, "name")?;
    spec.name = Some(match chosen_name {
        Some(name) => validate_name(name).map_err(|err| format!("name: {err}"))?,
        None => generated_name(),
    });

    let caps = sh.backend.capabilities();
    if !caps.client_command {
        return Err(unsupported("starting a program the caller chooses"));
    }
    if spec.cwd.is_some() && !caps.client_cwd {
        return Err(unsupported("a working directory the caller chooses"));
    }
    let mut info = sh.backend.run(&spec).map_err(|err| match err.kind {
        ErrorKind::NotFound => ToolError::new(
            "not_found",
            "the program or working directory was not found",
        )
        .with("backend_detail", err.detail),
        ErrorKind::NameTaken if chosen_name.is_none() => ToolError::new(
            "name_taken",
            "the name Shepherd generated for the pane is already in use; try again",
        )
        .with("backend_detail", err.detail),
        _ => backend_failure(spec.name.as_deref(), err),
    })?;
    if info.name.is_none() {
        info.name = spec.name;
    }
    Ok(info.to_json())
}

/// A name for a pane started without one: `pane-` and 12 hex digits.
fn generated_name() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNT.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("pane-{:012x}", hasher.finish() & 0xffff_ffff_ffff)
}

fn send_input(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let text = validate_input(required_str(args, "text")?).map_err(|err| format!("text: {err}"))?;
    let (_guard, pane, now) = lock_pane(sh, &terminal)?;
    // Idle and done are both sendable; a blocked program is never typed into.
    let guard = InputGuard {
        refuse_when_blocked: true,
        ..InputGuard::default()
    };
    if !sh.backend.capabilities().conditional_input {
        // The backend cannot refuse atomically: check the status got inside the
        // lock, then send.
        if let Some(status) = now.status.as_ref().filter(|s| s.state == State::Blocked) {
            return Err(pane_blocked(
                &terminal,
                status.kind.clone(),
                "refused by Shepherd: the status read inside the pane lock was blocked",
            ));
        }
    }
    match sh.backend.send(&pane, &text, &guard) {
        Ok(sent) => Ok(json!({
            "terminal": terminal,
            "turn": sent.turn,
            "started": sent.started,
            "status": sent.status.as_ref().map(State::as_str),
        })),
        Err(BackendError {
            kind: ErrorKind::Blocked { kind: None },
            detail,
        }) => {
            // The refusal did not say which dialog; read the pane now, best
            // effort, and fall back to the read taken before the send.
            let blocked_kind = sh
                .backend
                .get(&pane)
                .ok()
                .and_then(|info| blocked_kind_of(&info))
                .or_else(|| blocked_kind_of(&now));
            Err(pane_blocked(&terminal, blocked_kind, detail))
        }
        Err(err) => Err(backend_failure(Some(&terminal), err)),
    }
}

fn blocked_kind_of(info: &TerminalInfo) -> Option<String> {
    info.status
        .as_ref()
        .filter(|status| status.state == State::Blocked)
        .and_then(|status| status.kind.as_deref().map(dialog_kind))
}

/// Resolve `terminal`, the caller's pane name, with `get`. A backend may also
/// accept its own references (herdr-core: an address), so the pane found must
/// carry exactly this name; otherwise no pane has it. Every later backend call
/// uses the resolved pane's id ([`TerminalInfo::reference`]), never the
/// caller's string.
fn resolve(sh: &Shepherd, terminal: &str) -> Result<TerminalInfo, ToolError> {
    let info = get_pane(sh, &TerminalRef::new(terminal), terminal)?;
    if info.name.as_deref() != Some(terminal) {
        return Err(ToolError::new(
            "not_found",
            format!(
                "no pane is named {terminal}; panes are addressed by the name list_panes shows"
            ),
        )
        .with("terminal", terminal));
    }
    Ok(info)
}

/// Resolve `terminal` (a pane name), hold the lock for its id, then resolve
/// it again inside the lock and check it is still the same pane. Returns the
/// guard, a reference by id, and the pane as read inside the lock. Every tool
/// that types or presses keys into an existing pane calls this first, so
/// Shepherd's own calls on one pane run one at a time. start_pane does not:
/// `run` refuses a name that is taken (`name_taken`), so it never types into
/// an existing pane.
fn lock_pane(
    sh: &Shepherd,
    terminal: &str,
) -> Result<(PaneGuard, TerminalRef, TerminalInfo), ToolError> {
    let before = resolve(sh, terminal)?;
    let guard = sh.locks.lock(&before.id);
    let now = resolve(sh, terminal)?;
    if now.id != before.id {
        return Err(ToolError::new(
            "terminal_changed",
            format!(
                "{terminal} named a different pane while waiting for the pane lock; nothing was sent"
            ),
        )
        .with("terminal", terminal));
    }
    Ok((guard, before.reference(), now))
}

fn get_pane(sh: &Shepherd, pane: &TerminalRef, terminal: &str) -> Result<TerminalInfo, ToolError> {
    sh.backend
        .get(pane)
        .map_err(|err| backend_failure(Some(terminal), err))
}

fn read_output(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let lines = match args.get("lines") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "lines must be a non-negative integer".to_string())?,
    };
    if lines > READ_MAX_LINES {
        return Err(format!("lines must be between 0 and {READ_MAX_LINES}").into());
    }
    if !sh.backend.capabilities().screen_text {
        return Err(unsupported("reading screen text"));
    }
    let pane = resolve(sh, &terminal)?.reference();
    let screen = sh
        .backend
        .read(&pane, lines as u32)
        .map_err(|err| backend_failure(Some(&terminal), err))?;
    Ok(json!({
        "text": strip_terminal_escapes(&screen.text),
        "complete": screen.complete,
        "source": screen.source.as_ref().map(|source| source.as_str()),
        "reason": screen.limit.as_ref().map(|limit| limit.as_str()),
    }))
}

fn get_status(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let info = resolve(sh, &terminal)?;
    let mut out = info.to_json();
    out["terminal"] = json!(terminal);
    Ok(out)
}

fn wait_status(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let timeout = optional_number(args, "timeout", WAIT_MIN_SECONDS, WAIT_MAX_SECONDS)?
        .unwrap_or(WAIT_DEFAULT_SECONDS);
    let pane = resolve(sh, &terminal)?.reference();
    let result = sh
        .backend
        .wait(&pane, Duration::from_secs_f64(timeout))
        .map_err(|err| {
            let protocol = err.kind == ErrorKind::Protocol;
            let failure = backend_failure(Some(&terminal), err);
            if protocol {
                failure.context("the wait result is unknown and is not treated as idle")
            } else {
                failure
            }
        })?;
    let state = result.status.as_ref().map(|status| &status.state);
    let outcome = wait_outcome(result.ended, result.reason.as_ref(), state);
    let mut out = json!({
        "terminal": terminal,
        "outcome": outcome,
        "turn_complete": matches!(outcome, "done" | "exited"),
        "ended": result.ended,
        "reason": result.reason.as_ref().map(WaitReason::as_str),
        "status": state.map(State::as_str),
        "kind": Value::Null,
        "exit_code": result.exit_code,
        "turn": result.turn,
    });
    if outcome == "blocked" {
        match result
            .status
            .as_ref()
            .and_then(|status| status.kind.clone())
        {
            Some(kind) => out["kind"] = json!(dialog_kind(&kind)),
            None => {
                // The wait did not say which dialog; read the pane for its kind.
                let now = get_pane(sh, &pane, &terminal).map_err(|err| {
                    err.context("the wait ended blocked, but reading the dialog kind failed")
                })?;
                out["kind"] = json!(blocked_kind_of(&now));
                out["status_now"] = status_json(now.status.as_ref());
            }
        }
    }
    Ok(out)
}

/// What a wait result means. Only `done` and `exited` complete a turn.
fn wait_outcome(ended: bool, reason: Option<&WaitReason>, state: Option<&State>) -> &'static str {
    match (ended, reason, state) {
        (false, Some(WaitReason::Timeout), _) => "timeout",
        (true, Some(WaitReason::Exit), _) => "exited",
        (true, Some(WaitReason::Status), Some(State::Done)) => "done",
        (true, Some(WaitReason::Status), Some(State::Blocked)) => "blocked",
        _ => "other",
    }
}

fn list_panes(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let all = match args.get("all") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(all)) => *all,
        Some(_) => return Err("all must be a boolean".to_string().into()),
    };
    let list = sh
        .backend
        .list(all)
        .map_err(|err| backend_failure(None, err))?;
    let panes: Vec<Value> = list.terminals.iter().map(TerminalInfo::to_json).collect();
    Ok(json!({"panes": panes, "hidden_finished": list.hidden_finished}))
}

fn interrupt_pane(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let timeout = optional_number(args, "timeout", WAIT_MIN_SECONDS, INTERRUPT_MAX_SECONDS)?;
    let (_guard, pane, _) = lock_pane(sh, &terminal)?;
    let result = sh
        .backend
        .interrupt(&pane, timeout.map(Duration::from_secs_f64))
        .map_err(|err| backend_failure(Some(&terminal), err))?;
    Ok(json!({
        "terminal": terminal,
        "key": result.key.as_str(),
        "outcome": result.outcome.as_str(),
    }))
}

/// A backend failure in Shepherd's words. The backend's own text is kept only
/// under `backend_detail`.
fn backend_failure(terminal: Option<&str>, err: BackendError) -> ToolError {
    let pane = terminal.unwrap_or("the pane");
    let message = match &err.kind {
        ErrorKind::Blocked { kind } => return pane_blocked(pane, kind.clone(), err.detail),
        ErrorKind::Unavailable => "the terminal backend is not reachable".to_string(),
        ErrorKind::Timeout => "the terminal backend did not answer in time".to_string(),
        ErrorKind::NotFound => match terminal {
            Some(terminal) => format!("no pane matches {terminal}"),
            None => "the terminal backend could not find what the request named".to_string(),
        },
        ErrorKind::NameTaken => format!("a pane named {pane} already exists; choose another name"),
        ErrorKind::StatusMismatch => {
            format!("{pane} is not in the state the request required; nothing was sent")
        }
        ErrorKind::Finished => format!("{pane} has finished; its program is no longer running"),
        ErrorKind::Unsupported(op) => format!("the terminal backend does not support {op}"),
        ErrorKind::InvalidInput => "the terminal backend refused the request".to_string(),
        ErrorKind::Protocol => "the terminal backend sent a reply Shepherd cannot use".to_string(),
        ErrorKind::Other => "the terminal backend reported a failure".to_string(),
    };
    let mut failure = ToolError::new(err.code(), message);
    if let Some(terminal) = terminal {
        failure = failure.with("terminal", terminal);
    }
    failure.with("backend_detail", err.detail)
}

/// Typing was refused because the program is blocked on a dialog.
fn pane_blocked(terminal: &str, kind: Option<String>, detail: impl Into<String>) -> ToolError {
    let kind = kind.as_deref().map(dialog_kind);
    let dialog = match &kind {
        Some(kind) if kind == "other" => "a dialog of another kind".to_string(),
        Some(kind) => format!("a {kind} dialog"),
        None => "a dialog".to_string(),
    };
    ToolError::new(
        "pane_blocked",
        format!("{terminal} is blocked on {dialog}; nothing was typed. Read the screen with read_output, then answer with answer_dialog"),
    )
    .with("terminal", terminal)
    .with("kind", kind)
    .with("next", BLOCKED_NEXT)
    .with("backend_detail", detail.into())
}

fn unsupported(op: &str) -> ToolError {
    ToolError::new(
        "unsupported",
        format!("the terminal backend does not support {op}"),
    )
}

fn answer_dialog(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let choice_text = required_str(args, "choice")?;
    let choice = Choice::parse(choice_text)
        .ok_or_else(|| format!("choice must be allow_once or deny_once, not {choice_text:?}"))?;
    let caps = sh.backend.capabilities();
    if !caps.screen_text {
        return Err(unsupported("reading screen text"));
    }
    let mut sent: Vec<Vec<String>> = Vec::new();

    // Hold the pane lock from the first get through Enter.
    let (_guard, pane, got) = lock_pane(sh, &terminal)?;

    // 1. Only a pane blocked on a permission dialog.
    let status = status_json(got.status.as_ref());
    if !got
        .status
        .as_ref()
        .is_some_and(|s| s.is_blocked_with("permission"))
    {
        return Err(refusal(
            &terminal,
            choice,
            format!("the pane is not blocked on a permission dialog: status is {status}"),
            json!({"status": status}),
            &sent,
        ));
    }
    let Some(turn) = got.turn else {
        return Err(refusal(
            &terminal,
            choice,
            "the pane reported no turn number, so a change before Enter could not be detected"
                .to_string(),
            json!({"status": status, "turn": Value::Null}),
            &sent,
        ));
    };
    // Every key goes to this dialog in this turn. A backend with conditional
    // keys enforces it; for one without, the checks below do (non-atomically).
    let key_guard = InputGuard {
        refuse_when_blocked: false,
        if_state: Some(State::Blocked),
        if_kind: Some("permission".to_string()),
        expect_turn: Some(turn),
    };

    // 2. Parse the dialog from the screen, waiting for it to render. Between
    // reads, check the pane is still blocked on a permission dialog in the same
    // turn. Nothing is sent until the dialog parses.
    let mut screen = String::new();
    let mut parse_error = String::new();
    let mut parsed = None;
    for attempt in 0..RENDER_READS {
        if attempt > 0 {
            thread::sleep(RENDER_POLL_INTERVAL);
            let now = get_pane(sh, &pane, &terminal)?;
            let changes = pane_changes(&now, turn);
            if !changes.is_empty() {
                return Err(refusal(
                    &terminal,
                    choice,
                    format!(
                        "the pane changed while waiting for the dialog to render: {}",
                        changes.join("; ")
                    ),
                    json!({
                        "status": status_json(now.status.as_ref()),
                        "turn_before": turn,
                        "turn_now": now.turn,
                        "screen": tail(&screen),
                    }),
                    &sent,
                ));
            }
        }
        screen = read_screen(sh, &pane, &terminal)?;
        match dialog::parse(&screen) {
            Ok(found) => {
                parsed = Some(found);
                break;
            }
            Err(err) => parse_error = err,
        }
    }
    let Some(first) = parsed else {
        return Err(refusal(
            &terminal,
            choice,
            format!(
                "cannot parse the permission dialog: timed out waiting for the dialog to render ({RENDER_READS} reads over {} ms): {parse_error}",
                RENDER_WAIT.as_millis()
            ),
            json!({"status": status, "screen": tail(&screen)}),
            &sent,
        ));
    };

    // 3. Move the cursor, if needed.
    let target = choice.index();
    let moves = target as isize - first.cursor as isize;
    if moves != 0 {
        let key = if moves > 0 { Key::Down } else { Key::Up };
        let keys = vec![key; moves.unsigned_abs()];
        let result = press(sh, &pane, &terminal, &keys, &key_guard)?;
        sent.push(keys.iter().map(|key| key.as_str().to_string()).collect());
        if result.status != Some(State::Blocked) {
            let after = result.status.as_ref().map(State::as_str);
            return Err(refusal(
                &terminal,
                choice,
                format!("after the cursor moves the status is {after:?}, not blocked"),
                json!({"keys_result": result.to_json()}),
                &sent,
            ));
        }
    }

    // 4. Confirm the cursor is on the target in the same dialog.
    let mut last_seen = String::new();
    let mut last_problem = String::new();
    let mut confirmed = false;
    for attempt in 0..CONFIRM_READS {
        if attempt > 0 {
            thread::sleep(CONFIRM_INTERVAL);
        }
        last_seen = read_screen(sh, &pane, &terminal)?;
        match dialog::parse(&last_seen) {
            Ok(now) if now.title != first.title || now.command != first.command => {
                return Err(refusal(
                    &terminal,
                    choice,
                    format!(
                        "a different dialog is on the screen now: {:?} / {:?}",
                        now.title, now.command
                    ),
                    json!({"screen": tail(&last_seen)}),
                    &sent,
                ));
            }
            Ok(now) if now.cursor == target => {
                confirmed = true;
                break;
            }
            Ok(now) => {
                last_problem = format!("the cursor is on {:?}", now.options[now.cursor]);
            }
            Err(err) => last_problem = format!("cannot parse the dialog: {err}"),
        }
    }
    if !confirmed {
        return Err(refusal(
            &terminal,
            choice,
            format!(
                "the cursor is not confirmed on {:?} after {CONFIRM_READS} reads ({last_problem})",
                choice.label()
            ),
            json!({"screen": tail(&last_seen)}),
            &sent,
        ));
    }

    // 5. Re-check, still inside the lock, that the pane is blocked on a
    // permission dialog in the same turn. A backend without conditional keys
    // cannot do this with the Enter, so the check and the Enter are two calls:
    // a person or another client of the backend can still act between them.
    if !caps.conditional_keys {
        let again = get_pane(sh, &pane, &terminal)?;
        let changes = pane_changes(&again, turn);
        if !changes.is_empty() {
            return Err(refusal(
                &terminal,
                choice,
                format!(
                    "the pane changed after the cursor was confirmed: {}",
                    changes.join("; ")
                ),
                json!({
                    "status": status_json(again.status.as_ref()),
                    "turn_before": turn,
                    "turn_now": again.turn,
                    "screen": tail(&last_seen),
                }),
                &sent,
            ));
        }
    }

    // 6. Select it.
    let result = press(sh, &pane, &terminal, &[Key::Enter], &key_guard)?;
    Ok(json!({
        "terminal": terminal,
        "choice": choice.as_str(),
        "selected": choice.label(),
        "title": first.title,
        "command": first.command,
        "options": first.options,
        "moves": moves,
        "keys_result": result.to_json(),
    }))
}

/// What differs between a pane and "blocked with kind permission in turn
/// `turn`". Empty when nothing changed.
fn pane_changes(info: &TerminalInfo, turn: u64) -> Vec<String> {
    let mut changes = Vec::new();
    if !info
        .status
        .as_ref()
        .is_some_and(|s| s.is_blocked_with("permission"))
    {
        changes.push(format!(
            "the status is now {}, not blocked with kind permission",
            status_json(info.status.as_ref())
        ));
    }
    if info.turn != Some(turn) {
        changes.push(format!("the turn is now {}, not {turn}", json!(info.turn)));
    }
    changes
}

fn read_screen(sh: &Shepherd, pane: &TerminalRef, terminal: &str) -> Result<String, ToolError> {
    let screen = sh
        .backend
        .read(pane, 0)
        .map_err(|err| backend_failure(Some(terminal), err))?;
    Ok(strip_terminal_escapes(&screen.text))
}

fn press(
    sh: &Shepherd,
    pane: &TerminalRef,
    terminal: &str,
    keys: &[Key],
    guard: &InputGuard,
) -> Result<KeysResult, ToolError> {
    sh.backend
        .keys(pane, keys, guard)
        .map_err(|err| backend_failure(Some(terminal), err))
}

/// An answer_dialog refusal: what was seen, and which keys (if any) were sent.
/// Enter is never among them.
fn refusal(
    terminal: &str,
    choice: Choice,
    reason: String,
    seen: Value,
    sent: &[Vec<String>],
) -> ToolError {
    let message = format!(
        "answer_dialog ({}) refused: {reason}. Enter was not sent; keys sent: {sent:?}.\nSeen: {seen}",
        choice.as_str()
    );
    ToolError::new("refused", message)
        .with("terminal", terminal)
        .with("choice", choice.as_str())
        .with("reason", reason)
        .with("seen", seen)
        .with("keys_sent", json!(sent))
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(SEEN_TAIL_LINES);
    lines[start..].join("\n")
}

fn required_str<'a>(args: &'a Args, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        Some(Value::String(text)) => Ok(text.as_str()),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("{key} is required")),
    }
}

fn optional_str<'a>(args: &'a Args, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// The pane name: required and cleaned with the input rules. `resolve` then
/// checks a pane carries it.
fn terminal_arg(args: &Args) -> Result<String, String> {
    let raw = required_str(args, "terminal")?;
    validate_input(raw).map_err(|err| format!("terminal: {err}"))
}

fn optional_number(args: &Args, key: &str, min: f64, max: f64) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("{key} must be a number"))?;
            if !(min..=max).contains(&number) {
                return Err(format!("{key} must be between {min} and {max}"));
            }
            Ok(Some(number))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::FakeBackend;

    fn args(value: Value) -> Args {
        value.as_object().unwrap().clone()
    }

    /// A Shepherd over `fake`, with the fake able to see the pane locks.
    fn shepherd(fake: &FakeBackend) -> Shepherd {
        let sh = Shepherd::new(fake.clone());
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        sh
    }

    #[test]
    fn send_input_without_conditional_input_checks_status_under_the_lock() {
        let fake = FakeBackend::new(false);
        fake.add_pane("stuck", State::Blocked, Some("permission"));
        fake.add_pane("ready", State::Done, None);
        let sh = shepherd(&fake);

        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "stuck", "text": "yes"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "pane_blocked");
        assert_eq!(err.data["kind"], "permission");
        assert_eq!(err.data["terminal"], "stuck");
        // get, then get inside the lock; the status check refused, so no send.
        assert_eq!(fake.calls(), ["get", "get"]);
        assert!(fake.state.lock().unwrap().sent.is_empty());

        let sent = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "ready", "text": "next"})),
        )
        .unwrap();
        assert_eq!(sent["status"], "done");
        assert_eq!(fake.calls(), ["get", "get", "get", "get", "send"]);
        // The send ran while Shepherd held the pane lock for the pane's id.
        assert_eq!(
            fake.state.lock().unwrap().sent,
            [("next".to_string(), true)]
        );
        assert!(!sh.pane_locks().is_held("id-ready"));
    }

    #[test]
    fn send_input_with_conditional_input_leaves_the_refusal_to_the_backend() {
        let fake = FakeBackend::new(true);
        fake.add_pane("stuck", State::Blocked, Some("permission"));
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "stuck", "text": "yes"})),
        )
        .unwrap_err();
        assert_eq!(fake.calls(), ["get", "get", "send"]);
        assert_eq!(err.data["error"], "pane_blocked");
        assert_eq!(err.data["kind"], "permission");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn send_input_checks_the_status_read_inside_the_lock_not_before_it() {
        let fake = FakeBackend::new(false);
        fake.add_pane("ready", State::Done, None);
        // Done at the lookup before the lock, blocked at the get inside it.
        fake.state.lock().unwrap().block_on_get = Some(2);
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "ready", "text": "next"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "pane_blocked");
        assert_eq!(err.data["kind"], "permission");
        assert_eq!(fake.calls(), ["get", "get"]);
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn panes_are_reached_only_by_their_name() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Done, None);
        fake.add_pane("anon", State::Done, None);
        fake.state.lock().unwrap().panes[1].name = None;
        fake.state.lock().unwrap().screen = Some("screen".to_string());
        let sh = shepherd(&fake);
        // The fake's get also accepts ids, as herdr-core accepts addresses.
        for terminal in ["id-work", "id-anon"] {
            for (tool, extra) in [
                ("send_input", json!({"text": "hi"})),
                ("read_output", json!({})),
                ("get_status", json!({})),
                ("wait_status", json!({"timeout": 1})),
                ("interrupt_pane", json!({})),
                ("answer_dialog", json!({"choice": "allow_once"})),
            ] {
                let mut a = args(extra);
                a.insert("terminal".to_string(), json!(terminal));
                let err = call(&sh, tool, &a).unwrap_err();
                assert_eq!(err.data["error"], "not_found", "{tool} {terminal}");
                assert!(err.text.contains("no pane is named"), "{}", err.text);
            }
        }
        // Only the lookups ran; nothing reached either pane.
        assert!(fake.calls().iter().all(|call| *call == "get"));
        assert_eq!(fake.calls().len(), 12);
        assert!(fake.state.lock().unwrap().sent.is_empty());

        // The name works, and the result names the pane by it.
        let out = call(&sh, "read_output", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out["text"], "screen");
        let out = call(&sh, "get_status", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out["terminal"], "work");
        assert_eq!(out["name"], "work");
    }

    #[test]
    fn answer_dialog_leaves_the_pre_enter_check_to_a_backend_with_conditional_keys() {
        let mut fake = FakeBackend::new(false);
        fake.caps.conditional_keys = true;
        fake.add_pane("work", State::Blocked, Some("permission"));
        fake.state.lock().unwrap().screen = Some(crate::dialog::fixtures::dialog_on_allow());
        let sh = shepherd(&fake);
        let out = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "allow_once"})),
        )
        .unwrap();
        assert_eq!(out["selected"], "Allow once");
        // No get between the confirming read and Enter: the guard goes with it.
        assert_eq!(fake.calls(), ["get", "get", "read", "read", "keys"]);
        assert_eq!(fake.state.lock().unwrap().keys, [vec![Key::Enter]]);
    }

    #[test]
    fn backend_failures_use_shepherd_codes_and_keep_detail_aside() {
        for (kind, code) in [
            (ErrorKind::Unavailable, "unavailable"),
            (ErrorKind::Timeout, "timeout"),
            (ErrorKind::NotFound, "not_found"),
            (ErrorKind::NameTaken, "name_taken"),
            (ErrorKind::Blocked { kind: None }, "pane_blocked"),
            (ErrorKind::StatusMismatch, "status_mismatch"),
            (ErrorKind::Finished, "pane_finished"),
            (ErrorKind::Unsupported("x".to_string()), "unsupported"),
            (ErrorKind::InvalidInput, "invalid_input"),
            (ErrorKind::Protocol, "backend_error"),
            (ErrorKind::Other, "backend_error"),
        ] {
            let failure = backend_failure(
                Some("work"),
                BackendError::new(kind, "raw backend text --force"),
            );
            assert_eq!(failure.data["error"], code);
            assert_eq!(failure.data["backend_detail"], "raw backend text --force");
            assert!(failure.text.starts_with(&format!("{code}: ")));
            assert!(
                !failure.text.contains("raw backend text"),
                "{}",
                failure.text
            );
        }
    }

    #[test]
    fn generated_names_are_valid_and_distinct() {
        let names: Vec<String> = (0..50).map(|_| generated_name()).collect();
        for name in &names {
            assert_eq!(validate_name(name).as_deref(), Ok(name.as_str()));
            assert_eq!(name.len(), "pane-".len() + 12, "{name}");
        }
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn wait_outcome_only_completes_on_done_and_exit() {
        let status = Some(&WaitReason::Status);
        assert_eq!(wait_outcome(true, status, Some(&State::Done)), "done");
        assert_eq!(wait_outcome(true, status, Some(&State::Blocked)), "blocked");
        assert_eq!(wait_outcome(true, status, Some(&State::Idle)), "other");
        assert_eq!(wait_outcome(true, status, Some(&State::Other)), "other");
        assert_eq!(wait_outcome(true, Some(&WaitReason::Exit), None), "exited");
        assert_eq!(
            wait_outcome(false, Some(&WaitReason::Timeout), None),
            "timeout"
        );
        assert_eq!(wait_outcome(false, Some(&WaitReason::Other), None), "other");
        assert_eq!(wait_outcome(true, None, Some(&State::Done)), "other");
    }
}
