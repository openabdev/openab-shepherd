//! The seven MCP tools. Each one maps to one herdr-core method.
//!
//! Tool -> herdr-core method:
//!   start_pane      -> terminal.run
//!   send_input      -> terminal.send      (never sends `force`)
//!   read_output     -> terminal.read
//!   get_status      -> terminal.get
//!   wait_status     -> terminal.wait
//!   list_panes      -> terminal.list
//!   interrupt_pane  -> terminal.interrupt

use crate::ansi::strip_terminal_escapes;
use crate::herdr::{HerdrClient, HerdrError, DEFAULT_TIMEOUT, WAIT_SOCKET_SLACK};
use crate::input::{validate_input, validate_name};
use serde_json::{json, Map, Value};
use std::time::Duration;

pub const TOOL_NAMES: [&str; 7] = [
    "start_pane",
    "send_input",
    "read_output",
    "get_status",
    "wait_status",
    "list_panes",
    "interrupt_pane",
];
pub const READ_MAX_LINES: u64 = 10_000;
pub const WAIT_MIN_SECONDS: f64 = 1.0;
pub const WAIT_MAX_SECONDS: f64 = 1800.0;
pub const WAIT_DEFAULT_SECONDS: f64 = 30.0;
pub const INTERRUPT_MAX_SECONDS: f64 = 60.0;
const INTERRUPT_DEFAULT_SOCKET_SECONDS: f64 = 10.0;

pub type Args = Map<String, Value>;

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
            "description": "Type text into a pane and press Enter. Input is trimmed and limited to 4096 bytes. Control characters (except TAB) are refused, not removed. Refused by herdr-core while the program is blocked on a dialog; this tool never forces input.",
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
            "description": "Wait until a pane stops working, or check in after timeout seconds. ended=false is a check-in: the pane is still running. ended=true with reason \"status\" means the pane left the working state. A program that never reports status counts as working until it exits.",
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
        }
    ])
}

/// Run one tool. `Err` carries the text shown to the MCP client with `isError`.
pub fn call(client: &HerdrClient, name: &str, args: &Args) -> Result<Value, String> {
    match name {
        "start_pane" => start_pane(client, args),
        "send_input" => send_input(client, args),
        "read_output" => read_output(client, args),
        "get_status" => get_status(client, args),
        "wait_status" => wait_status(client, args),
        "list_panes" => list_panes(client, args),
        "interrupt_pane" => interrupt_pane(client, args),
        other => Err(format!("unknown tool: {other}")),
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

fn send_input(client: &HerdrClient, args: &Args) -> Result<Value, String> {
    let terminal = terminal_arg(args)?;
    let text = validate_input(required_str(args, "text")?).map_err(|err| format!("text: {err}"))?;
    // `force` is never sent: a blocked program refuses input.
    client
        .call(
            "terminal.send",
            obj(vec![("terminal", json!(terminal)), ("text", json!(text))]),
            DEFAULT_TIMEOUT,
        )
        .map_err(herdr_error)
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
            obj(vec![("terminal", json!(terminal)), ("timeout", json!(timeout))]),
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
    Ok(json!({
        "terminal": terminal,
        "ended": ended,
        "reason": field(&result, "reason"),
        "status": field(&result, "status"),
        "exit_code": field(&result, "exit_code"),
        "turn": field(&result, "turn"),
    }))
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

fn interrupt_pane(client: &HerdrClient, args: &Args) -> Result<Value, String> {
    let terminal = terminal_arg(args)?;
    let timeout = optional_number(args, "timeout", WAIT_MIN_SECONDS, INTERRUPT_MAX_SECONDS)?;
    let socket_seconds = timeout.unwrap_or(INTERRUPT_DEFAULT_SOCKET_SECONDS)
        + WAIT_SOCKET_SLACK.as_secs_f64();
    client
        .call(
            "terminal.interrupt",
            obj(vec![("terminal", json!(terminal)), ("timeout", json!(timeout))]),
            Duration::from_secs_f64(socket_seconds),
        )
        .map_err(herdr_error)
}

fn herdr_error(err: HerdrError) -> String {
    format!("herdr-core: {err}")
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
