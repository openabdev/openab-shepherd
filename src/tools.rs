//! The eight MCP tools and the herdr-core methods each one calls.
//!
//! Tool -> herdr-core method:
//!   start_pane      -> terminal.run
//!   send_input      -> terminal.get (x2, see lock_pane), terminal.send (never `force` or `if`)
//!   read_output     -> terminal.read
//!   get_status      -> terminal.get
//!   wait_status     -> terminal.wait, then terminal.get when it ends blocked
//!   list_panes      -> terminal.list
//!   interrupt_pane  -> terminal.get (x2, see lock_pane), terminal.interrupt
//!   answer_dialog   -> terminal.get, terminal.read, terminal.keys

use crate::ansi::strip_terminal_escapes;
use crate::dialog::{self, Choice};
use crate::herdr::{HerdrClient, HerdrError, DEFAULT_TIMEOUT, WAIT_SOCKET_SLACK};
use crate::input::{validate_input, validate_name};
use crate::panelock::PaneGuard;
use serde_json::{json, Map, Value};
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
const INTERRUPT_DEFAULT_SOCKET_SECONDS: f64 = 10.0;
/// answer_dialog re-reads the screen up to this many times, waiting for the
/// cursor to land on the target after the moves (herdr-core delivers keys after
/// a quiet period, and the program redraws after that).
pub const CONFIRM_READS: usize = 5;
pub const CONFIRM_INTERVAL: Duration = Duration::from_millis(200);
/// Lines of screen text returned with an answer_dialog refusal.
const SEEN_TAIL_LINES: usize = 40;

pub type Args = Map<String, Value>;

/// A tool failure: the text shown to the MCP client, plus optional structured
/// data (returned as `structuredContent` alongside `isError: true`).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub text: String,
    pub data: Option<Value>,
}

impl From<String> for ToolError {
    fn from(text: String) -> Self {
        Self { text, data: None }
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
            "description": "Start a program in a new herdr-core pane. Give exactly one of argv (a program and its arguments) or line (a shell line run by sh -c). Returns the terminal object, including its address. Input is trimmed and limited to 4096 bytes per field. Control characters (except TAB) are refused, not removed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "argv": {"type": "array", "items": {"type": "string"}, "description": "Program and arguments; non-empty."},
                    "line": {"type": "string", "description": "Shell line run by sh -c."},
                    "cwd": {"type": "string", "description": "Working directory."},
                    "name": {"type": "string", "description": "Terminal name: 1-32 of a-z, 0-9, '-', '_', '.'."}
                }
            }
        },
        {
            "name": "send_input",
            "description": "Type text into a pane and press Enter. Sendable when the pane is idle or done (kiro-cli stays in done after a turn). Input is trimmed and limited to 4096 bytes. Control characters (except TAB) are refused, not removed. While the program is blocked on a dialog, herdr-core refuses and nothing is typed: the error carries kind \"blocked\" and herdr-core's hint. Read the screen, then use answer_dialog. This tool never forces input.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane address (for example t7) or name."},
                    "text": {"type": "string"}
                },
                "required": ["terminal", "text"]
            }
        },
        {
            "name": "read_output",
            "description": "Read a pane's screen (lines=0) or its last N lines of history. ANSI CSI and OSC escape sequences are removed from text. TUI redraw noise is not removed.",
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
            "description": "Return a pane's status (idle, working, done, blocked, error, or null) and its reported flag. reported is false when the program never sent an OSC 7501 status.",
            "inputSchema": {
                "type": "object",
                "properties": {"terminal": {"type": "string"}},
                "required": ["terminal"]
            }
        },
        {
            "name": "wait_status",
            "description": "Wait until a pane stops working, or check in after timeout seconds. Branch on outcome: done (the turn finished), blocked (waiting on a dialog; kind says which, for example permission), timeout (a check-in: still running), exited (the program ended), or other (any other status word or reason; read status). turn_complete is true only for done and exited; a blocked pane has not completed its turn even though its turn number advanced. A program that never reports status counts as working until it exits.",
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
            "description": "List herdr-core panes. all=true includes finished panes.",
            "inputSchema": {
                "type": "object",
                "properties": {"all": {"type": "boolean", "default": false}}
            }
        },
        {
            "name": "interrupt_pane",
            "description": "Send the stop key to a pane (esc for programs that report status, else ctrl+c) and report the outcome: stopped, exited, still_working, or unknown. herdr-core does not escalate.",
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
            "description": "Answer a permission dialog in a pane that is blocked with kind permission: allow_once or deny_once. Rule-creating options are not offered. The tool reads the screen, moves the cursor with up/down keys, reads the screen again until the cursor is confirmed on the chosen option, checks again that the pane is still blocked with kind permission in the same turn, and only then presses Enter. On any mismatch it sends nothing more and returns what it saw. Other Shepherd calls that type into the same pane wait until it finishes. A person at the terminal or another herdr-core client can still act between the last check and Enter, because herdr-core has no conditional key press. The dialog is parsed from screen text; tested only on kiro-cli 3.1.0 (V3) shell permission dialogs. Follow with wait_status.",
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
pub fn call(client: &HerdrClient, name: &str, args: &Args) -> Result<Value, ToolError> {
    match name {
        "start_pane" => start_pane(client, args).map_err(ToolError::from),
        "send_input" => send_input(client, args),
        "read_output" => read_output(client, args).map_err(ToolError::from),
        "get_status" => get_status(client, args).map_err(ToolError::from),
        "wait_status" => wait_status(client, args).map_err(ToolError::from),
        "list_panes" => list_panes(client, args).map_err(ToolError::from),
        "interrupt_pane" => interrupt_pane(client, args),
        "answer_dialog" => answer_dialog(client, args),
        other => Err(format!("unknown tool: {other}").into()),
    }
}

fn start_pane(client: &HerdrClient, args: &Args) -> Result<Value, String> {
    let argv_items = match args.get("argv") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(items),
        Some(_) => return Err("argv must be a list of strings".to_string()),
    };
    let line = optional_str(args, "line")?;
    let has_argv = argv_items.is_some_and(|items| !items.is_empty());
    if has_argv == line.is_some() {
        return Err("give exactly one of argv (non-empty list) or line".to_string());
    }

    // Same input rules as send_input, applied to every string that reaches the pane.
    let mut params = Args::new();
    if let Some(items) = argv_items.filter(|items| !items.is_empty()) {
        let mut argv = Vec::with_capacity(items.len());
        for item in items {
            let text = item
                .as_str()
                .ok_or_else(|| "argv must be a list of strings".to_string())?;
            let cleaned = validate_input(text).map_err(|err| format!("argv: {err}"))?;
            argv.push(Value::String(cleaned));
        }
        params.insert("argv".to_string(), Value::Array(argv));
    }
    if let Some(line) = line {
        let cleaned = validate_input(line).map_err(|err| format!("line: {err}"))?;
        params.insert("line".to_string(), Value::String(cleaned));
    }
    if let Some(cwd) = optional_str(args, "cwd")? {
        let cleaned = validate_input(cwd).map_err(|err| format!("cwd: {err}"))?;
        params.insert("cwd".to_string(), Value::String(cleaned));
    }
    if let Some(name) = optional_str(args, "name")? {
        let cleaned = validate_name(name).map_err(|err| format!("name: {err}"))?;
        params.insert("name".to_string(), Value::String(cleaned));
    }
    client
        .call("terminal.run", params, DEFAULT_TIMEOUT)
        .map_err(herdr_error)
}

fn send_input(client: &HerdrClient, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let text = validate_input(required_str(args, "text")?).map_err(|err| format!("text: {err}"))?;
    let (_guard, address, _) = lock_pane(client, &terminal)?;
    // Neither `force` nor `if` is sent: idle and done are both sendable, and a
    // blocked program refuses input (herdr-core error kind `blocked`).
    client
        .call(
            "terminal.send",
            obj(vec![("terminal", json!(address)), ("text", json!(text))]),
            DEFAULT_TIMEOUT,
        )
        .map_err(|err| herdr_tool_error(&terminal, err))
}

/// Resolve `terminal` (an address or a name) to its address with terminal.get,
/// hold that address's lock, then get it again inside the lock and check it still
/// resolves to the same address. Returns the guard, the address, and the
/// terminal.get made inside the lock. Every tool that types or presses keys into
/// an existing pane calls this first, so Shepherd's own calls on one pane run one
/// at a time. start_pane does not: terminal.run refuses a name that is taken
/// (`name_taken`), so it never types into an existing pane.
fn lock_pane(
    client: &HerdrClient,
    terminal: &str,
) -> Result<(PaneGuard, String, Value), ToolError> {
    let before = terminal_get(client, terminal)?;
    let address = address_of(&before, terminal)?;
    let guard = client.lock_terminal(&address);
    let now = terminal_get(client, terminal)?;
    let address_now = address_of(&now, terminal)?;
    if address_now != address {
        let text = format!(
            "{terminal} resolved to {address}, then to {address_now} while waiting for the pane lock; nothing was sent"
        );
        return Err(ToolError {
            data: Some(json!({
                "error": "terminal_changed",
                "terminal": terminal,
                "address_before": address,
                "address_now": address_now,
            })),
            text,
        });
    }
    Ok((guard, address, now))
}

fn terminal_get(client: &HerdrClient, terminal: &str) -> Result<Value, ToolError> {
    client
        .call(
            "terminal.get",
            obj(vec![("terminal", json!(terminal))]),
            DEFAULT_TIMEOUT,
        )
        .map_err(|err| herdr_tool_error(terminal, err))
}

fn address_of(got: &Value, terminal: &str) -> Result<String, ToolError> {
    got.get("address")
        .and_then(Value::as_str)
        .filter(|address| !address.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            ToolError::from(format!(
                "terminal.get for {terminal} returned no address; nothing was sent"
            ))
        })
}

fn read_output(client: &HerdrClient, args: &Args) -> Result<Value, String> {
    let terminal = terminal_arg(args)?;
    let lines = match args.get("lines") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "lines must be a non-negative integer".to_string())?,
    };
    if lines > READ_MAX_LINES {
        return Err(format!("lines must be between 0 and {READ_MAX_LINES}"));
    }
    let result = client
        .call(
            "terminal.read",
            obj(vec![("terminal", json!(terminal)), ("lines", json!(lines))]),
            DEFAULT_TIMEOUT,
        )
        .map_err(herdr_error)?;
    let text = result.get("text").and_then(Value::as_str).unwrap_or("");
    Ok(json!({
        "text": strip_terminal_escapes(text),
        "complete": field(&result, "complete"),
        "how": field(&result, "how"),
        "why": field(&result, "why"),
    }))
}

fn get_status(client: &HerdrClient, args: &Args) -> Result<Value, String> {
    let terminal = terminal_arg(args)?;
    let result = client
        .call(
            "terminal.get",
            obj(vec![("terminal", json!(terminal))]),
            DEFAULT_TIMEOUT,
        )
        .map_err(herdr_error)?;
    Ok(json!({
        "terminal": terminal,
        "status": field(&result, "status"),
        "reported": field(&result, "reported"),
        "state": field(&result, "state"),
        "turn": field(&result, "turn"),
        "status_ms": field(&result, "status_ms"),
    }))
}

fn wait_status(client: &HerdrClient, args: &Args) -> Result<Value, String> {
    let terminal = terminal_arg(args)?;
    let timeout = optional_number(args, "timeout", WAIT_MIN_SECONDS, WAIT_MAX_SECONDS)?
        .unwrap_or(WAIT_DEFAULT_SECONDS);
    let result = client
        .call(
            "terminal.wait",
            obj(vec![
                ("terminal", json!(terminal)),
                ("timeout", json!(timeout)),
            ]),
            Duration::from_secs_f64(timeout) + WAIT_SOCKET_SLACK,
        )
        .map_err(herdr_error)?;
    // A reply without a boolean `ended` is not evidence of idle. Report it as an error.
    let ended = match result.get("ended") {
        Some(Value::Bool(ended)) => *ended,
        _ => {
            return Err(
                "terminal.wait reply has no boolean `ended`; not treated as idle".to_string(),
            )
        }
    };
    let reason = result.get("reason").and_then(Value::as_str);
    let status = result.get("status").and_then(Value::as_str);
    let outcome = wait_outcome(ended, reason, status);
    let mut out = json!({
        "terminal": terminal,
        "outcome": outcome,
        "turn_complete": matches!(outcome, "done" | "exited"),
        "ended": ended,
        "reason": field(&result, "reason"),
        "status": field(&result, "status"),
        "kind": Value::Null,
        "exit_code": field(&result, "exit_code"),
        "turn": field(&result, "turn"),
    });
    if outcome == "blocked" {
        // terminal.wait does not report kind; terminal.get does, in status.kind.
        let now = client
            .call(
                "terminal.get",
                obj(vec![("terminal", json!(terminal))]),
                DEFAULT_TIMEOUT,
            )
            .map_err(|err| {
                format!("terminal.wait ended blocked, but terminal.get for its kind failed: herdr-core: {err}")
            })?;
        let status_now = field(&now, "status");
        if status_now.get("state").and_then(Value::as_str) == Some("blocked") {
            out["kind"] = field(&status_now, "kind");
        }
        out["status_now"] = status_now;
    }
    Ok(out)
}

/// What a terminal.wait return means. Only `done` and `exited` complete a turn.
fn wait_outcome(ended: bool, reason: Option<&str>, status: Option<&str>) -> &'static str {
    match (ended, reason, status) {
        (false, Some("timeout"), _) => "timeout",
        (true, Some("exit"), _) => "exited",
        (true, Some("status"), Some("done")) => "done",
        (true, Some("status"), Some("blocked")) => "blocked",
        _ => "other",
    }
}

fn list_panes(client: &HerdrClient, args: &Args) -> Result<Value, String> {
    let all = match args.get("all") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(all)) => *all,
        Some(_) => return Err("all must be a boolean".to_string()),
    };
    client
        .call(
            "terminal.list",
            obj(vec![("all", json!(all))]),
            DEFAULT_TIMEOUT,
        )
        .map_err(herdr_error)
}

fn interrupt_pane(client: &HerdrClient, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let timeout = optional_number(args, "timeout", WAIT_MIN_SECONDS, INTERRUPT_MAX_SECONDS)?;
    let socket_seconds =
        timeout.unwrap_or(INTERRUPT_DEFAULT_SOCKET_SECONDS) + WAIT_SOCKET_SLACK.as_secs_f64();
    let (_guard, address, _) = lock_pane(client, &terminal)?;
    client
        .call(
            "terminal.interrupt",
            obj(vec![
                ("terminal", json!(address)),
                ("timeout", json!(timeout)),
            ]),
            Duration::from_secs_f64(socket_seconds),
        )
        .map_err(|err| ToolError::from(herdr_error(err)))
}

fn herdr_error(err: HerdrError) -> String {
    format!("herdr-core: {err}")
}

/// A herdr-core failure with its kind and hint passed through as structured data.
fn herdr_tool_error(terminal: &str, err: HerdrError) -> ToolError {
    let mut data = json!({
        "error": "herdr-core",
        "terminal": terminal,
        "message": err.message,
        "kind": err.kind,
        "hint": err.hint,
        "code": err.code,
    });
    if err.kind.as_deref() == Some("blocked") {
        data["next"] = json!("read_output, then answer_dialog; nothing was typed");
    }
    ToolError {
        text: herdr_error(err),
        data: Some(data),
    }
}

fn answer_dialog(client: &HerdrClient, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let choice_text = required_str(args, "choice")?;
    let choice = Choice::parse(choice_text)
        .ok_or_else(|| format!("choice must be allow_once or deny_once, not {choice_text:?}"))?;
    let mut sent: Vec<Vec<&str>> = Vec::new();

    // Hold the pane lock from the first terminal.get through Enter.
    let (_guard, address, got) = lock_pane(client, &terminal)?;

    // 1. Only a pane blocked on a permission dialog.
    let status = field(&got, "status");
    let state = status.get("state").and_then(Value::as_str);
    let kind = status.get("kind").and_then(Value::as_str);
    if state != Some("blocked") || kind != Some("permission") {
        return Err(refusal(
            &terminal,
            choice,
            format!("the pane is not blocked on a permission dialog: status is {status}"),
            json!({"status": status}),
            &sent,
        ));
    }
    let Some(turn) = got.get("turn").and_then(Value::as_u64) else {
        return Err(refusal(
            &terminal,
            choice,
            "terminal.get reported no turn number, so a change before Enter could not be detected"
                .to_string(),
            json!({"status": status, "turn": field(&got, "turn")}),
            &sent,
        ));
    };

    // 2. Parse the dialog from the screen.
    let screen = read_screen(client, &address)?;
    let first = dialog::parse(&screen).map_err(|err| {
        refusal(
            &terminal,
            choice,
            format!("cannot parse the permission dialog: {err}"),
            json!({"status": status, "screen": tail(&screen)}),
            &sent,
        )
    })?;

    // 3. Move the cursor, if needed.
    let target = choice.index();
    let moves = target as isize - first.cursor as isize;
    if moves != 0 {
        let key = if moves > 0 { "down" } else { "up" };
        let keys = vec![key; moves.unsigned_abs()];
        let result = press(client, &address, &keys)?;
        sent.push(keys);
        let after = result.get("status").and_then(Value::as_str);
        if after != Some("blocked") {
            return Err(refusal(
                &terminal,
                choice,
                format!("after the cursor moves the status is {after:?}, not blocked"),
                json!({"keys_result": result}),
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
        last_seen = read_screen(client, &address)?;
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
    // permission dialog in the same turn. herdr-core 0.1.19's terminal.keys has
    // no status or turn condition, so this check and the Enter are two calls: a
    // person or another herdr-core client can still act between them.
    let again = terminal_get(client, &address)?;
    let status_again = field(&again, "status");
    let state_again = status_again.get("state").and_then(Value::as_str);
    let kind_again = status_again.get("kind").and_then(Value::as_str);
    let turn_again = again.get("turn").and_then(Value::as_u64);
    let mut changes = Vec::new();
    if state_again != Some("blocked") || kind_again != Some("permission") {
        changes.push(format!(
            "the status is now {status_again}, not blocked with kind permission"
        ));
    }
    if turn_again != Some(turn) {
        changes.push(format!(
            "the turn is now {}, not {turn}",
            field(&again, "turn")
        ));
    }
    if !changes.is_empty() {
        return Err(refusal(
            &terminal,
            choice,
            format!(
                "the pane changed after the cursor was confirmed: {}",
                changes.join("; ")
            ),
            json!({
                "status": status_again,
                "turn_before": turn,
                "turn_now": field(&again, "turn"),
                "screen": tail(&last_seen),
            }),
            &sent,
        ));
    }

    // 6. Select it.
    let result = press(client, &address, &["enter"])?;
    Ok(json!({
        "terminal": terminal,
        "choice": choice.as_str(),
        "selected": choice.label(),
        "title": first.title,
        "command": first.command,
        "options": first.options,
        "moves": moves,
        "keys_result": result,
    }))
}

fn read_screen(client: &HerdrClient, terminal: &str) -> Result<String, ToolError> {
    let result = client
        .call(
            "terminal.read",
            obj(vec![("terminal", json!(terminal)), ("lines", json!(0))]),
            DEFAULT_TIMEOUT,
        )
        .map_err(|err| herdr_tool_error(terminal, err))?;
    let text = result.get("text").and_then(Value::as_str).unwrap_or("");
    Ok(strip_terminal_escapes(text))
}

fn press(client: &HerdrClient, terminal: &str, keys: &[&str]) -> Result<Value, ToolError> {
    client
        .call(
            "terminal.keys",
            obj(vec![("terminal", json!(terminal)), ("keys", json!(keys))]),
            DEFAULT_TIMEOUT,
        )
        .map_err(|err| herdr_tool_error(terminal, err))
}

/// An answer_dialog refusal: what was seen, and which keys (if any) were sent.
/// Enter is never among them.
fn refusal(
    terminal: &str,
    choice: Choice,
    reason: String,
    seen: Value,
    sent: &[Vec<&str>],
) -> ToolError {
    let text = format!(
        "answer_dialog refused ({}): {reason}. Enter was not sent; keys sent: {sent:?}.\nSeen: {seen}",
        choice.as_str()
    );
    ToolError {
        text,
        data: Some(json!({
            "error": "refused",
            "terminal": terminal,
            "choice": choice.as_str(),
            "reason": reason,
            "seen": seen,
            "keys_sent": sent,
        })),
    }
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(SEEN_TAIL_LINES);
    lines[start..].join("\n")
}

fn obj(pairs: Vec<(&str, Value)>) -> Args {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn field(result: &Value, key: &str) -> Value {
    result.get(key).cloned().unwrap_or(Value::Null)
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

/// The pane address: required, cleaned with the input rules, then sent.
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
