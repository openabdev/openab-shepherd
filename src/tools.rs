//! The nine MCP tools. They talk only to [`TerminalBackend`]; the safety rules
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
//!   remove_pane     -> get (x2, see lock_pane), remove

use crate::agent::{unmeasured, Agent, ParsedDialog};
use crate::ansi::strip_terminal_escapes;
use crate::backend::{
    dialog_kind, status_json, BackendError, ErrorKind, InputGuard, Key, KeysResult, RunSpec, State,
    StopHow, TerminalBackend, TerminalInfo, TerminalRef, WaitReason, WaitResult,
};
use crate::devin_dialog;
use crate::dialog::Choice;
use crate::input::{validate_input, validate_name};
use crate::opencode_dialog;
use crate::panelock::{PaneGuard, PaneLocks};
use crate::profiles::{CwdError, LaunchPolicy};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

pub const TOOL_NAMES: [&str; 9] = [
    "start_pane",
    "send_input",
    "read_output",
    "get_status",
    "wait_status",
    "list_panes",
    "interrupt_pane",
    "answer_dialog",
    "remove_pane",
];
pub const READ_MAX_LINES: u64 = 10_000;
pub const WAIT_MIN_SECONDS: f64 = 1.0;
pub const WAIT_MAX_SECONDS: f64 = 1800.0;
pub const WAIT_DEFAULT_SECONDS: f64 = 30.0;
pub const INTERRUPT_MAX_SECONDS: f64 = 60.0;
/// remove_pane's grace period: how long a running program gets to end after
/// the hangup and terminate signals before it is killed.
pub const REMOVE_MIN_SECONDS: f64 = 0.0;
pub const REMOVE_MAX_SECONDS: f64 = 60.0;
pub const REMOVE_DEFAULT_SECONDS: f64 = 2.0;
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
/// How long wait_status pauses before it waits again on an OpenCode pane that
/// reported `clear`.
const SETTLE_INTERVAL: Duration = Duration::from_millis(100);
/// Screen reads in the render wait: one at once, then one per interval.
pub const RENDER_READS: usize =
    (RENDER_WAIT.as_millis() / RENDER_POLL_INTERVAL.as_millis()) as usize + 1;
/// Lines of screen text returned with an answer_dialog refusal.
const SEEN_TAIL_LINES: usize = 40;
/// What a caller does after typing was refused because the pane is blocked.
const BLOCKED_NEXT: &str = "read_output, then answer_dialog; nothing was typed";
/// The coding CLI is not logged in. answer_dialog cannot fix that.
const AUTH_NEXT: &str = "tell the operator; nothing was typed";
const AUTH_BLOCKED: &str = "the CLI is not logged in; the operator must log in to it or provide its API key, then start a new pane. Shepherd does not log in or handle credentials";

pub type Args = Map<String, Value>;

/// The tools' shared state: one backend, the per-pane locks, and the agent of
/// each pane a profile started. Clones share all three.
#[derive(Clone)]
pub struct Shepherd {
    backend: Arc<dyn TerminalBackend>,
    locks: Arc<PaneLocks>,
    policy: Arc<LaunchPolicy>,
    panes: Arc<Mutex<HashMap<String, PaneAgent>>>,
}

/// What this process knows about a pane a profile started, keyed by pane name.
#[derive(Debug, Clone, Copy)]
struct PaneAgent {
    agent: Agent,
    /// A dialog was answered with Enter or Esc, so an idle state that follows
    /// is a cancel (see [`wait_outcome`]). Cleared once wait_status reaches a
    /// final outcome, so a timeout or other intermediate result keeps it.
    answered: bool,
}

impl Shepherd {
    /// A Shepherd with no launch profiles and no arbitrary commands:
    /// `start_pane` starts nothing.
    pub fn new(backend: impl TerminalBackend + 'static) -> Self {
        Self::with_policy(backend, LaunchPolicy::default())
    }

    /// A Shepherd that starts what `policy` allows.
    pub fn with_policy(backend: impl TerminalBackend + 'static, policy: LaunchPolicy) -> Self {
        Self {
            backend: Arc::new(backend),
            locks: Arc::new(PaneLocks::default()),
            policy: Arc::new(policy),
            panes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub fn pane_locks(&self) -> &Arc<PaneLocks> {
        &self.locks
    }

    /// Record that the pane `name` runs `agent`, as if a profile had started it.
    #[cfg(test)]
    pub fn assume_agent(&self, name: &str, agent: Agent) {
        self.remember_agent(name, agent);
    }

    fn panes(&self) -> MutexGuard<'_, HashMap<String, PaneAgent>> {
        self.panes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn remember_agent(&self, name: &str, agent: Agent) {
        self.panes().insert(
            name.to_string(),
            PaneAgent {
                agent,
                answered: false,
            },
        );
    }

    /// The agent of a pane this process started from a profile. None for a
    /// pane started any other way, or before this process started.
    pub(crate) fn agent_of(&self, name: &str) -> Option<Agent> {
        self.panes().get(name).map(|pane| pane.agent)
    }

    fn mark_answered(&self, name: &str) {
        if let Some(pane) = self.panes().get_mut(name) {
            pane.answered = true;
        }
    }

    /// Whether a dialog answered on this pane is still waiting for its end state.
    fn answer_pending(&self, name: &str) -> bool {
        self.panes().get(name).is_some_and(|pane| pane.answered)
    }

    /// Forget the pending answer. Called once the pane reached a final outcome.
    fn clear_answer(&self, name: &str) {
        if let Some(pane) = self.panes().get_mut(name) {
            pane.answered = false;
        }
    }

    fn forget_agent(&self, name: &str) {
        self.panes().remove(name);
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
            "description": "Start a program in a new pane. Give profile: the name of one of the operator's launch profiles (list_panes lists them with the directories each allows); the profile fixes the program, its arguments and its environment. cwd, when given, must be one of the profile's directories or below one (after resolving symlinks); without it the profile's first directory is used. argv (a program and its arguments) or line (a shell line run by sh -c) instead of profile work only when the operator allows arbitrary commands (list_panes: allow_arbitrary_command), and such a pane never gets a profile's environment. Later calls address the pane by its name: give one, or Shepherd picks one (pane-<hex>) and returns it. Returns the pane (name, live, status, reported, turn, status_ms, exit_code, argv, cwd). Input is trimmed and limited to 4096 bytes per field. Control characters (except TAB) are refused, not removed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "profile": {"type": "string", "description": "Launch profile name, as list_panes shows it."},
                    "argv": {"type": "array", "items": {"type": "string"}, "description": "Program and arguments; non-empty. Only when arbitrary commands are allowed."},
                    "line": {"type": "string", "description": "Shell line run by sh -c. Only when arbitrary commands are allowed."},
                    "cwd": {"type": "string", "description": "Working directory (absolute)."},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Extra environment variables for an argv or line pane only (refused with profile): at most 32, names [A-Z_][A-Z0-9_]*, values under the same input rules (empty allowed; leading and trailing whitespace is trimmed). A backend may refuse protected names (the native backend refuses HOME, PATH, USER, LANG, TERM and DYLD_*/LD_*); a backend without per-run environments returns unsupported."},
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
            "description": "Wait until a pane stops working, or check in after timeout seconds. Branch on outcome: done (the turn finished; for an opencode pane, idle after a turn, since OpenCode reports no done), blocked (waiting on a dialog; kind says which, for example permission), timeout (a check-in: still running), exited (the program ended), cancelled (the pane went idle after answer_dialog denied or cancelled its dialog; a Devin cancel returns to idle, not done), or other (any other status or reason; read status). turn_complete is true only for done and exited; a blocked pane has not completed its turn even though its turn number advanced. A program that never reports status counts as working until it exits.",
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
            "description": "List panes, each in the same shape start_pane returns. all=true includes finished panes; hidden_finished counts the ones left out. live_panes counts the running panes and live_pane_limit is how many may run at once (null when the backend sets no limit); start_pane is refused at the limit, so remove panes you no longer need with remove_pane. Also returns profiles (each launch profile's name and the directories it allows) and allow_arbitrary_command (whether start_pane takes argv or line).",
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
            "description": "Answer a permission dialog in a pane that is blocked with kind permission: allow_once or deny_once. Rule-creating options are not offered. The tool reads the screen, waiting up to 3 s (re-reading every 200 ms) for the dialog to render, because kiro-cli reports blocked before it draws the dialog; between reads it checks the pane is still blocked with kind permission in the same turn, and if the dialog never renders it refuses without sending anything. It then moves the cursor with up/down keys, reads the screen again until the cursor is confirmed on the chosen option, checks again that the pane is still blocked with kind permission in the same turn, and only then presses Enter. On any mismatch it sends nothing more and returns what it saw. Other Shepherd calls that type into the same pane wait until it finishes. A person at the terminal or another client of the terminal backend can still act between the last check and Enter when the terminal backend has no conditional key press; the native backend checks the status and presses Enter in one step. The dialog is parsed from screen text. The dialect is the pane's agent, set by the launch profile that started it: kiro (tested on kiro-cli 3.1.0 V3 shell permission dialogs) or devin (Devin CLI 3000.12.43 shell command dialogs; Devin never selects Bypass mode, and a highlight that does not land on the choice sends Esc, which cancels, then refuses); opencode (OpenCode v2.2.7): deny_once sends Esc, which cancels like Reject, after the pane is blocked with kind permission and the screen shows the whole expected dialog; allow_once is refused with no keys sent because the highlighted option is a background colour that the screen text does not carry, and Always allow is never selected. A pane not started from a profile by this server is refused with no keys sent. Follow with wait_status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "choice": {"type": "string", "enum": ["allow_once", "deny_once"]}
                },
                "required": ["terminal", "choice"]
            }
        },
        {
            "name": "remove_pane",
            "description": "Close a pane and forget it. It waits for any other Shepherd call that types into the same pane to finish first. If the program is running, Shepherd sends its process group and the processes it started the hangup and terminate signals, waits up to timeout seconds (default 2) for them to end, then kills whatever is left. No key is pressed first; use interrupt_pane before it if the program should stop its turn cleanly. Afterwards list_panes (even with all=true) no longer shows the pane, later calls that name it get not_found, and its name can be used again. Returns name, was_live (whether the program was running), stopped (exited: it ended within the timeout; killed: it had to be killed; other: the backend could not say; null when it had already finished) and exit_code (128 + the signal number when a signal ended it; null when unknown). A backend that times its own stop ignores timeout.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name, as start_pane or list_panes returned it."},
                    "timeout": {"type": "number", "minimum": REMOVE_MIN_SECONDS, "maximum": REMOVE_MAX_SECONDS, "default": REMOVE_DEFAULT_SECONDS}
                },
                "required": ["terminal"]
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
        "remove_pane" => remove_pane(sh, args),
        other => Err(format!("unknown tool: {other}").into()),
    }
}

fn start_pane(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let mut spec = match optional_str(args, "profile")? {
        Some(profile) => profile_spec(sh, args, profile)?,
        None => command_spec(sh, args)?,
    };
    // Every pane Shepherd starts has a name: callers address panes by name.
    let chosen_name = optional_str(args, "name")?;
    spec.name = Some(match chosen_name {
        Some(name) => validate_name(name).map_err(|err| format!("name: {err}"))?,
        None => generated_name(),
    });
    // Credentials are read last, when everything else has passed, and only
    // into the spec of this profile's pane.
    if let Some(profile) = spec.profile.as_deref().and_then(|p| sh.policy.profile(p)) {
        spec.profile_env = sh.policy.resolve_env(profile).map_err(|message| {
            ToolError::new("profile_unavailable", message).with("profile", profile.name.as_str())
        })?;
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
    // The profile's agent decides the dialect answer_dialog reads for this pane.
    let profile_agent = spec
        .profile
        .as_deref()
        .and_then(|p| sh.policy.profile(p))
        .map(|p| p.agent);
    if info.name.is_none() {
        info.name = spec.name.clone();
    }
    if profile_agent == Some(Agent::Devin) {
        let pane = info.reference();
        if trust_prompt_shown(sh, &pane) {
            // Nothing was typed into the pane, so it is closed, not left waiting.
            let _ = sh.backend.remove(&pane, Some(Duration::ZERO));
            return Err(ToolError::new(
                "trust_prompt",
                format!(
                    "Devin asked \"{TRUST_PROMPT}\" for this directory, so the pane was closed before it ran anything. Shepherd never answers this prompt. Trust the directory once in Devin (run devin there and choose 1), then start the pane again"
                ),
            )
            .with("terminal", spec.name.clone()));
        }
    }
    if let Some(name) = info.name.as_deref() {
        match profile_agent {
            Some(agent) => sh.remember_agent(name, agent),
            // A pane this profile did not start must not inherit an agent that an
            // earlier pane with the same name had.
            None => sh.forget_agent(name),
        }
    }
    Ok(info.to_json())
}

/// The Devin prompt that asks whether to trust a directory before Devin runs
/// anything there.
const TRUST_PROMPT: &str = "Do you trust the authors of this directory?";

/// Read the pane for the trust prompt for the same window answer_dialog uses to
/// wait for a dialog to render. True as soon as the prompt shows. False when it
/// never shows in that window, or when the backend has no screen text.
fn trust_prompt_shown(sh: &Shepherd, pane: &TerminalRef) -> bool {
    if !sh.backend.capabilities().screen_text {
        return false;
    }
    for attempt in 0..RENDER_READS {
        if attempt > 0 {
            thread::sleep(RENDER_POLL_INTERVAL);
        }
        if let Ok(screen) = sh.backend.read(pane, 0) {
            if strip_terminal_escapes(&screen.text).contains(TRUST_PROMPT) {
                return true;
            }
        }
    }
    false
}

/// A launch profile's run: its fixed argv, a cwd it allows, no caller env.
fn profile_spec(sh: &Shepherd, args: &Args, profile: &str) -> Result<RunSpec, ToolError> {
    let given = |key: &str| !matches!(args.get(key), None | Some(Value::Null));
    if given("argv") || given("line") {
        return Err("give profile, or argv or line, not both".to_string().into());
    }
    if given("env") {
        return Err(
            "env is not accepted with a profile; the profile sets the environment"
                .to_string()
                .into(),
        );
    }
    let name = validate_name(profile).map_err(|err| format!("profile: {err}"))?;
    let profile = sh.policy.profile(&name).ok_or_else(|| {
        ToolError::new(
            "invalid_input",
            format!("no launch profile is named {name}; list_panes shows the profiles"),
        )
        .with("profile", name.as_str())
    })?;
    let requested = match optional_str(args, "cwd")? {
        Some(cwd) => Some(validate_input(cwd).map_err(|err| format!("cwd: {err}"))?),
        None => None,
    };
    let cwd = sh
        .policy
        .cwd(profile, requested.as_deref())
        .map_err(|err| match err {
            CwdError::NotFound => {
                ToolError::new("not_found", "the working directory was not found")
            }
            CwdError::Invalid(why) => ToolError::from(format!("cwd: {why}")),
        })?;
    let caps = sh.backend.capabilities();
    if !caps.client_cwd {
        return Err(unsupported("a working directory the caller chooses"));
    }
    if !profile.env.is_empty() && !caps.client_env {
        return Err(unsupported(
            "a launch profile with an environment (use the native backend)",
        ));
    }
    // Pin the executable before any credential is read, independent of cwd.
    let program = sh.policy.program(profile).map_err(|why| {
        ToolError::new("not_found", "the program was not found")
            .with("profile", name.as_str())
            .with("backend_detail", why)
    })?;
    // The program, then the agent's permission flag, then the profile's own arguments.
    let mut argv = vec![program.to_string_lossy().into_owned()];
    argv.extend(profile.permission_args());
    argv.extend(profile.argv[1..].iter().cloned());
    Ok(RunSpec {
        argv: Some(argv),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        profile: Some(name),
        ..RunSpec::default()
    })
}

/// A caller-chosen `argv` or `line`, when the operator allows those. It never
/// gets a profile's environment.
fn command_spec(sh: &Shepherd, args: &Args) -> Result<RunSpec, ToolError> {
    if !sh.policy.allow_arbitrary_command {
        return Err(ToolError::new(
            "profile_required",
            "give profile: this server starts only its launch profiles (list_panes shows them)",
        ));
    }
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
    spec.env = env_arg(args)?;
    if let Some(name) = optional_str(args, "name")? {
        validate_name(name).map_err(|err| format!("name: {err}"))?;
    }

    let caps = sh.backend.capabilities();
    if !caps.client_command {
        return Err(unsupported("starting a program the caller chooses"));
    }
    if spec.cwd.is_some() && !caps.client_cwd {
        return Err(unsupported("a working directory the caller chooses"));
    }
    if !spec.env.is_empty() && !caps.client_env {
        return Err(unsupported("a per-run environment"));
    }
    Ok(spec)
}

/// Most variables one `start_pane` may add.
const MAX_RUN_ENV: usize = 32;

/// `env`: an object of names (`[A-Z_][A-Z0-9_]*`) to string values. Values
/// follow the input rules, except that an empty value is allowed. Which
/// names a backend accepts (it may protect some) is the backend's rule.
fn env_arg(args: &Args) -> Result<Vec<(String, String)>, String> {
    let map = match args.get("env") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(map)) => map,
        Some(_) => return Err("env must be an object of names to strings".to_string()),
    };
    if map.len() > MAX_RUN_ENV {
        return Err(format!("env: at most {MAX_RUN_ENV} variables are allowed"));
    }
    let mut env = Vec::with_capacity(map.len());
    for (key, value) in map {
        let valid_key = key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
            && key
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !valid_key {
            return Err("env: a name must match [A-Z_][A-Z0-9_]*".to_string());
        }
        let value = value
            .as_str()
            .ok_or_else(|| format!("env: {key} must be a string"))?;
        let value = if value.is_empty() {
            String::new()
        } else {
            validate_input(value).map_err(|err| format!("env: {key}: {err}"))?
        };
        env.push((key.clone(), value));
    }
    Ok(env)
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
    if sh.agent_of(&terminal) == Some(Agent::Devin) && is_devin_mode_switch(&text) {
        return Err(ToolError::new(
            "refused",
            "refused by the Devin mode-switch rule: /yolo can switch Devin into Bypass mode, which Shepherd never allows; nothing was sent",
        )
        .with("terminal", terminal.as_str()));
    }
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

/// Devin slash commands that may change the permission mode. A name-based check
/// against accidental use, not a defense against a different spelling or a wrapper.
fn is_devin_mode_switch(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("/yolo")
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
    if lines > 0 && !sh.backend.capabilities().history {
        return Err(unsupported("reading history"));
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
    let agent = sh.agent_of(&terminal);
    let result =
        wait_settled(sh, &pane, agent, Duration::from_secs_f64(timeout)).map_err(|err| {
            let protocol = err.kind == ErrorKind::Protocol;
            let failure = backend_failure(Some(&terminal), err);
            if protocol {
                failure.context("the wait result is unknown and is not treated as idle")
            } else {
                failure
            }
        })?;
    let state = result.status.as_ref().map(|status| &status.state);
    let answered = sh.answer_pending(&terminal);
    let mut outcome = wait_outcome(result.ended, result.reason.as_ref(), state, answered);
    // OpenCode reports no done state: a turn ends clear, then idle. An idle
    // after at least one turn (the turn number is 0 until the first leaves
    // working) is its done.
    if outcome == "other"
        && agent == Some(Agent::OpenCode)
        && result.ended
        && result.reason == Some(WaitReason::Status)
        && state == Some(&State::Idle)
        && result.turn.is_some_and(|turn| turn > 0)
    {
        outcome = "done";
    }
    if matches!(outcome, "done" | "cancelled" | "blocked" | "exited") {
        sh.clear_answer(&terminal);
    }
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

/// The backend's wait. OpenCode reports `clear` between `working` and `idle`
/// at the end of a turn; it maps to the unknown state, which would end the wait
/// for a moment before the idle that closes the turn. For an OpenCode pane such
/// a result is waited out again, within the caller's timeout.
fn wait_settled(
    sh: &Shepherd,
    pane: &TerminalRef,
    agent: Option<Agent>,
    timeout: Duration,
) -> Result<WaitResult, BackendError> {
    let end = Instant::now() + timeout;
    let mut remaining = timeout;
    loop {
        let result = sh.backend.wait(pane, remaining)?;
        let settling = agent == Some(Agent::OpenCode)
            && result.ended
            && result.reason == Some(WaitReason::Status)
            && result.status.as_ref().map(|status| &status.state) == Some(&State::Other);
        let left = end.saturating_duration_since(Instant::now());
        if !settling || left <= SETTLE_INTERVAL {
            return Ok(result);
        }
        thread::sleep(SETTLE_INTERVAL);
        remaining = end.saturating_duration_since(Instant::now());
    }
}

/// What a wait result means. Only `done` and `exited` complete a turn. `idle`
/// after a dialog this process answered is `cancelled`: a Devin deny or cancel
/// returns the pane to idle, not done.
fn wait_outcome(
    ended: bool,
    reason: Option<&WaitReason>,
    state: Option<&State>,
    answered: bool,
) -> &'static str {
    match (ended, reason, state) {
        (false, Some(WaitReason::Timeout), _) => "timeout",
        (true, Some(WaitReason::Exit), _) => "exited",
        (true, Some(WaitReason::Status), Some(State::Done)) => "done",
        (true, Some(WaitReason::Status), Some(State::Blocked)) => "blocked",
        (true, Some(WaitReason::Status), Some(State::Idle)) if answered => "cancelled",
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
    // Running panes are listed whether or not `all` is set.
    let live_panes = list.terminals.iter().filter(|pane| pane.live).count();
    let launch = sh.policy.to_json();
    Ok(json!({
        "panes": panes,
        "hidden_finished": list.hidden_finished,
        "live_panes": live_panes,
        "live_pane_limit": list.live_limit,
        "profiles": launch["profiles"],
        "allow_arbitrary_command": launch["allow_arbitrary_command"],
    }))
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

/// Stop the pane's program if it runs, then remove the pane, holding the pane
/// lock throughout. A call that waited for the lock and finds the name gone
/// gets not_found from `lock_pane`.
fn remove_pane(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let grace = optional_number(args, "timeout", REMOVE_MIN_SECONDS, REMOVE_MAX_SECONDS)?
        .unwrap_or(REMOVE_DEFAULT_SECONDS);
    let (_guard, pane, _) = lock_pane(sh, &terminal)?;
    let removed = sh
        .backend
        .remove(&pane, Some(Duration::from_secs_f64(grace)))
        .map_err(|err| backend_failure(Some(&terminal), err))?;
    sh.forget_agent(&terminal);
    Ok(json!({
        "name": terminal,
        "was_live": removed.was_live,
        "stopped": removed.stopped.as_ref().map(StopHow::as_str),
        "exit_code": removed.exit_code,
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
    let auth = kind.as_deref() == Some("auth");
    let (message, next) = if auth {
        (
            format!("{terminal} is blocked on an auth prompt: {AUTH_BLOCKED}. Nothing was typed"),
            AUTH_NEXT,
        )
    } else {
        (
            format!("{terminal} is blocked on {dialog}; nothing was typed. Read the screen with read_output, then answer with answer_dialog"),
            BLOCKED_NEXT,
        )
    };
    ToolError::new("pane_blocked", message)
        .with("terminal", terminal)
        .with("kind", kind)
        .with("next", next)
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
        let reason = if got
            .status
            .as_ref()
            .is_some_and(|s| s.is_blocked_with("auth"))
        {
            format!("this is an auth prompt, not a permission dialog, and answer_dialog cannot answer it: {AUTH_BLOCKED}; status is {status}")
        } else {
            format!("the pane is not blocked on a permission dialog: status is {status}")
        };
        return Err(refusal(
            &terminal,
            choice,
            reason,
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
    // The dialect comes from the profile that started the pane. A pane with
    // none, or one this process did not start, gets no keys.
    let Some(agent) = sh.agent_of(&terminal) else {
        return Err(refusal(
            &terminal,
            choice,
            "the pane was not started from a launch profile by this Shepherd process, so no dialect applies"
                .to_string(),
            json!({"status": status}),
            &sent,
        ));
    };
    // Every key goes to this dialog in this turn. A backend with conditional
    // keys enforces it; for one without, the checks below do (non-atomically).
    // OpenCode's dialog has its own path: the highlight is a colour that the
    // screen text does not carry (see opencode_dialog).
    if agent == Agent::OpenCode {
        return answer_opencode(sh, &terminal, &pane, choice, turn, caps.conditional_keys);
    }
    // An agent the generic path does not serve has no option to press: refuse
    // before reading the screen, and send nothing.
    let Some((target, label)) = agent.option(choice) else {
        return Err(refusal(
            &terminal,
            choice,
            unmeasured(agent),
            json!({"status": status}),
            &sent,
        ));
    };
    let key_guard = InputGuard {
        refuse_when_blocked: false,
        if_state: Some(State::Blocked),
        if_kind: Some("permission".to_string()),
        expect_turn: Some(turn),
    };
    let ask = Ask {
        sh,
        pane: &pane,
        terminal: &terminal,
        choice,
        agent,
        turn,
        key_guard: &key_guard,
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
        match agent.parse_dialog(&screen) {
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

    // 3. Move the cursor. Kiro: all the moves at once, then one read. Devin:
    // one key at a time, each checked against the screen, never past the target.
    let moves = if agent == Agent::Devin {
        step_devin(&ask, &mut sent, &first, &screen, target)?
    } else {
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
        moves
    };

    // 4. Confirm the cursor is on the target in the same dialog.
    let mut last_seen = String::new();
    let mut last_problem = String::new();
    let mut confirmed = false;
    for attempt in 0..CONFIRM_READS {
        if attempt > 0 {
            thread::sleep(CONFIRM_INTERVAL);
        }
        last_seen = read_screen(sh, &pane, &terminal)?;
        match agent.parse_dialog(&last_seen) {
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
            Ok(now) if !devin_scope_ok(agent, choice, &now) => {
                last_problem = format!(
                    "the Allow row does not show the Once scope: {:?}",
                    now.options[devin_dialog::INDEX_ALLOW]
                );
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
        return Err(ask.mismatch(
            &mut sent,
            format!(
                "the cursor is not confirmed on {:?} after {CONFIRM_READS} reads ({last_problem})",
                label
            ),
            json!({"screen": tail(&last_seen)}),
        )?);
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
    if agent == Agent::Devin {
        sh.mark_answered(&terminal);
    }
    Ok(json!({
        "terminal": terminal,
        "agent": agent.as_str(),
        "choice": choice.as_str(),
        "selected": label,
        "title": first.title,
        "command": first.command,
        "options": first.options,
        "moves": moves,
        "keys_result": result.to_json(),
    }))
}

/// OpenCode (v2.2.7). `deny_once` sends Esc, which cancels exactly like Reject
/// (measured), once the pane is blocked on a permission dialog in this turn and
/// the screen shows the whole expected dialog. `allow_once` is refused with no key
/// sent: Enter is safe only on the highlighted `Allow once`, and the highlight is
/// a background colour that the screen text lacks. Right and Enter are never
/// sent, so `Always allow` is never reachable.
fn answer_opencode(
    sh: &Shepherd,
    terminal: &str,
    pane: &TerminalRef,
    choice: Choice,
    turn: u64,
    conditional_keys: bool,
) -> Result<Value, ToolError> {
    let sent: Vec<Vec<String>> = Vec::new();
    if choice == Choice::AllowOnce {
        return Err(refusal(
            terminal,
            choice,
            opencode_dialog::highlight_unverifiable(),
            json!({"turn": turn}),
            &sent,
        ));
    }
    // Wait for the dialog text to render, checking between reads that the pane
    // is still blocked on a permission dialog in this turn.
    let mut screen = String::new();
    let mut parse_error = String::new();
    let mut parsed = None;
    for attempt in 0..RENDER_READS {
        if attempt > 0 {
            thread::sleep(RENDER_POLL_INTERVAL);
            let now = get_pane(sh, pane, terminal)?;
            let changes = pane_changes(&now, turn);
            if !changes.is_empty() {
                return Err(refusal(
                    terminal,
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
        screen = read_screen(sh, pane, terminal)?;
        match opencode_dialog::parse(&screen) {
            Ok(found) => {
                parsed = Some(found);
                break;
            }
            Err(err) => parse_error = err,
        }
    }
    let Some(dialog) = parsed else {
        return Err(refusal(
            terminal,
            choice,
            format!(
                "cannot parse the permission dialog: timed out waiting for the dialog to render ({RENDER_READS} reads over {} ms): {parse_error}",
                RENDER_WAIT.as_millis()
            ),
            json!({"screen": tail(&screen)}),
            &sent,
        ));
    };
    // Re-check, immediately before Esc, that the pane is still blocked on a
    // permission dialog in this turn. A backend with conditional keys also
    // enforces it with the key itself.
    if !conditional_keys {
        let again = get_pane(sh, pane, terminal)?;
        let changes = pane_changes(&again, turn);
        if !changes.is_empty() {
            return Err(refusal(
                terminal,
                choice,
                format!(
                    "the pane changed after the dialog was read: {}",
                    changes.join("; ")
                ),
                json!({
                    "status": status_json(again.status.as_ref()),
                    "turn_before": turn,
                    "turn_now": again.turn,
                    "screen": tail(&screen),
                }),
                &sent,
            ));
        }
    }
    let guard = InputGuard {
        refuse_when_blocked: false,
        if_state: Some(State::Blocked),
        if_kind: Some("permission".to_string()),
        expect_turn: Some(turn),
    };
    let result = press(sh, pane, terminal, &[Key::Esc], &guard)?;
    sh.mark_answered(terminal);
    Ok(json!({
        "terminal": terminal,
        "agent": Agent::OpenCode.as_str(),
        "choice": choice.as_str(),
        "selected": opencode_dialog::OPTION_REJECT,
        "title": opencode_dialog::HEADING,
        "command": dialog.subject,
        "options": dialog.options,
        "keys": [Key::Esc.as_str()],
        "keys_result": result.to_json(),
    }))
}

/// What one answer_dialog call checks and sends keys with, after its pane was
/// found, its agent known, and its key guard built.
struct Ask<'a> {
    sh: &'a Shepherd,
    pane: &'a TerminalRef,
    terminal: &'a str,
    choice: Choice,
    agent: Agent,
    turn: u64,
    key_guard: &'a InputGuard,
}

impl Ask<'_> {
    /// A dialog that did not land as expected. Kiro: refuse and send nothing
    /// more. Devin: when the pane is still blocked on a permission dialog in
    /// this turn, send Esc to cancel the request, then refuse. Enter is never
    /// sent here.
    fn mismatch(
        &self,
        sent: &mut Vec<Vec<String>>,
        reason: String,
        seen: Value,
    ) -> Result<ToolError, ToolError> {
        if self.agent != Agent::Devin {
            return Ok(refusal(
                self.terminal,
                self.choice,
                reason,
                seen,
                sent.as_slice(),
            ));
        }
        let again = get_pane(self.sh, self.pane, self.terminal)?;
        let changes = pane_changes(&again, self.turn);
        if !changes.is_empty() {
            let reason = format!(
                "{reason}; the pane then changed, so nothing more was sent: {}",
                changes.join("; ")
            );
            return Ok(refusal(
                self.terminal,
                self.choice,
                reason,
                seen,
                sent.as_slice(),
            ));
        }
        press(
            self.sh,
            self.pane,
            self.terminal,
            &[Key::Esc],
            self.key_guard,
        )?;
        sent.push(vec![Key::Esc.as_str().to_string()]);
        self.sh.mark_answered(self.terminal);
        let reason = format!("{reason}; Esc cancelled the dialog");
        Ok(refusal(
            self.terminal,
            self.choice,
            reason,
            seen,
            sent.as_slice(),
        ))
    }
}

/// Devin: the highlight must start on Allow. Each Down moves it one row, and the
/// screen is read after each key. The highlight must be on the next row before
/// another key is sent, so it never passes the target. Up is never sent: its
/// effect on this dialog was not measured. Any other reading is a mismatch, so
/// Enter is never sent from here. Returns the moves made.
fn step_devin(
    ask: &Ask<'_>,
    sent: &mut Vec<Vec<String>>,
    first: &ParsedDialog,
    first_screen: &str,
    target: usize,
) -> Result<isize, ToolError> {
    if first.cursor != devin_dialog::INDEX_ALLOW {
        let reason = format!(
            "the highlight starts on {:?}, not on Allow; Devin dialogs are answered only from Allow",
            first.options[first.cursor]
        );
        return Err(ask.mismatch(sent, reason, json!({"screen": tail(first_screen)}))?);
    }
    if !devin_scope_ok(ask.agent, ask.choice, first) {
        let reason = format!(
            "the Allow row does not show the Once scope: {:?}",
            first.options[devin_dialog::INDEX_ALLOW]
        );
        return Err(ask.mismatch(sent, reason, json!({"screen": tail(first_screen)}))?);
    }
    let mut cursor = first.cursor;
    let mut moves: isize = 0;
    while cursor < target {
        let next = cursor + 1;
        let result = press(
            ask.sh,
            ask.pane,
            ask.terminal,
            std::slice::from_ref(&Key::Down),
            ask.key_guard,
        )?;
        sent.push(vec![Key::Down.as_str().to_string()]);
        moves += 1;
        if result.status != Some(State::Blocked) {
            let after = result.status.as_ref().map(State::as_str);
            let reason = format!("after a move the status is {after:?}, not blocked");
            return Err(ask.mismatch(sent, reason, json!({"keys_result": result.to_json()}))?);
        }
        let screen = read_screen(ask.sh, ask.pane, ask.terminal)?;
        let now = match ask.agent.parse_dialog(&screen) {
            Ok(now) if now.title == first.title && now.command == first.command => now,
            Ok(_) => {
                let reason = "a different dialog is on the screen after a move".to_string();
                return Err(ask.mismatch(sent, reason, json!({"screen": tail(&screen)}))?);
            }
            Err(err) => {
                let reason = format!("cannot parse the dialog after a move: {err}");
                return Err(ask.mismatch(sent, reason, json!({"screen": tail(&screen)}))?);
            }
        };
        if now.cursor != next {
            let reason = format!(
                "after a move the highlight is on {:?}, not on {:?}",
                now.options[now.cursor], first.options[next]
            );
            return Err(ask.mismatch(sent, reason, json!({"screen": tail(&screen)}))?);
        }
        cursor = now.cursor;
    }
    Ok(moves)
}

/// Devin's allow_once approves the Once scope only. Any other scope on the Allow
/// row is refused. Every other choice passes.
fn devin_scope_ok(agent: Agent, choice: Choice, dialog: &ParsedDialog) -> bool {
    agent != Agent::Devin
        || choice != Choice::AllowOnce
        || devin_dialog::allow_scope_is_once(&dialog.options[devin_dialog::INDEX_ALLOW])
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
    use crate::backend::{ProgramStatus, Secret, WaitResult};
    use crate::testutil::{FakeBackend, TempDir};

    fn args(value: Value) -> Args {
        value.as_object().unwrap().clone()
    }

    /// A Shepherd over `fake`, with the fake able to see the pane locks.
    fn shepherd(fake: &FakeBackend) -> Shepherd {
        let sh = Shepherd::with_policy(fake.clone(), LaunchPolicy::arbitrary());
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
    fn auth_blocked_pane_says_not_logged_in_and_does_not_point_at_answer_dialog() {
        let fake = FakeBackend::new(false);
        fake.add_pane("stuck", State::Blocked, Some("auth"));
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "stuck", "text": "yes"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "pane_blocked");
        assert_eq!(err.data["kind"], "auth");
        let message = err.text.clone();
        assert!(message.contains("not logged in"), "{message}");
        assert!(message.contains("operator"), "{message}");
        assert!(!message.contains("answer_dialog"), "{message}");
        let next = err.data["next"].as_str().unwrap();
        assert!(!next.contains("answer_dialog"), "{next}");
        assert!(fake.state.lock().unwrap().sent.is_empty());

        let err = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "stuck", "choice": "allow_once"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "refused");
        let reason = err.data["reason"].as_str().unwrap();
        assert!(reason.contains("not logged in"), "{reason}");
        assert!(reason.contains("operator"), "{reason}");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn permission_blocked_message_and_next_are_unchanged() {
        let fake = FakeBackend::new(false);
        fake.add_pane("stuck", State::Blocked, Some("permission"));
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "stuck", "text": "yes"})),
        )
        .unwrap_err();
        assert_eq!(err.data["next"], BLOCKED_NEXT);
        let text = err.text.clone();
        assert!(
            text.contains("stuck is blocked on a permission dialog; nothing was typed. Read the screen with read_output, then answer with answer_dialog"),
            "{text}"
        );
    }

    #[test]
    fn send_input_refuses_yolo_on_a_devin_pane_and_sends_nothing() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Done, None);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Devin);
        for text in ["/yolo", "  /YOLO "] {
            let err = call(
                &sh,
                "send_input",
                &args(json!({"terminal": "work", "text": text})),
            )
            .unwrap_err();
            assert_eq!(err.data["error"], "refused");
            assert!(err.data["message"]
                .as_str()
                .unwrap()
                .contains("mode-switch"));
        }
        assert!(fake.state.lock().unwrap().sent.is_empty());
        call(
            &sh,
            "send_input",
            &args(json!({"terminal": "work", "text": "fix the tests"})),
        )
        .unwrap();
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
    }

    #[test]
    fn send_input_yolo_on_a_non_devin_pane_is_unaffected() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Done, None);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Kiro);
        call(
            &sh,
            "send_input",
            &args(json!({"terminal": "work", "text": "/yolo"})),
        )
        .unwrap();
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
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
                ("remove_pane", json!({})),
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
        assert_eq!(fake.calls().len(), 14);
        assert!(fake.state.lock().unwrap().sent.is_empty());

        // The name works, and the result names the pane by it.
        let out = call(&sh, "read_output", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out["text"], "screen");
        let out = call(&sh, "get_status", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out["terminal"], "work");
        assert_eq!(out["name"], "work");
    }

    #[test]
    fn remove_pane_checks_its_arguments_before_any_call() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        let sh = shepherd(&fake);
        for bad in [json!(-1), json!(60.5), json!("2")] {
            let err = call(
                &sh,
                "remove_pane",
                &args(json!({"terminal": "work", "timeout": bad})),
            )
            .unwrap_err();
            assert_eq!(err.data["error"], "invalid_input", "{bad}");
        }
        let err = call(&sh, "remove_pane", &args(json!({}))).unwrap_err();
        assert_eq!(err.data["error"], "invalid_input");
        assert!(fake.calls().is_empty());

        // An unknown name: only the lookup runs.
        let err = call(&sh, "remove_pane", &args(json!({"terminal": "nope"}))).unwrap_err();
        assert_eq!(err.data["error"], "not_found");
        assert_eq!(err.data["terminal"], "nope");
        assert_eq!(fake.calls(), ["get"]);
    }

    /// Another call holds the pane lock; the pane's program ends meanwhile.
    /// remove_pane waits for the lock, then reports the state it finds.
    #[test]
    fn remove_pane_waits_for_the_lock_and_acts_on_the_state_then() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Working, None);
        let sh = shepherd(&fake);
        let held = sh.pane_locks().lock("id-work");
        let remover = {
            let sh = sh.clone();
            thread::spawn(move || call(&sh, "remove_pane", &args(json!({"terminal": "work"}))))
        };
        while sh.pane_locks().waiting("id-work") == 0 {
            thread::yield_now();
        }
        assert_eq!(fake.calls(), ["get"]);
        {
            let mut state = fake.state.lock().unwrap();
            state.panes[0].live = false;
            state.panes[0].status = None;
            state.panes[0].exit_code = Some(0);
        }
        drop(held);
        let out = remover.join().unwrap().unwrap();
        assert_eq!(
            out,
            json!({"name": "work", "was_live": false, "stopped": null, "exit_code": 0})
        );
        assert_eq!(fake.calls(), ["get", "get", "remove"]);
        assert!(!sh.pane_locks().is_held("id-work"));
        let err = call(&sh, "get_status", &args(json!({"terminal": "work"}))).unwrap_err();
        assert_eq!(err.data["error"], "not_found");
    }

    /// A send_input that arrives while remove_pane holds the pane lock waits
    /// for it, then finds the pane gone: nothing is typed and nothing hangs.
    #[test]
    fn send_input_waiting_behind_remove_pane_gets_not_found() {
        use std::sync::mpsc;
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        let sh = shepherd(&fake);
        let (started_tx, started) = mpsc::channel();
        let (proceed, proceed_rx) = mpsc::channel();
        fake.state.lock().unwrap().remove_gate = Some((started_tx, proceed_rx));
        let remover = {
            let sh = sh.clone();
            thread::spawn(move || call(&sh, "remove_pane", &args(json!({"terminal": "work"}))))
        };
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("remove never started");
        assert!(sh.pane_locks().is_held("id-work"));
        let sender = {
            let sh = sh.clone();
            thread::spawn(move || {
                call(
                    &sh,
                    "send_input",
                    &args(json!({"terminal": "work", "text": "hi"})),
                )
            })
        };
        while sh.pane_locks().waiting("id-work") == 0 {
            thread::yield_now();
        }
        proceed.send(()).unwrap();
        let out = remover.join().unwrap().unwrap();
        assert_eq!(
            out,
            json!({"name": "work", "was_live": true, "stopped": "exited", "exit_code": null})
        );
        let err = sender.join().unwrap().unwrap_err();
        assert_eq!(err.data["error"], "not_found");
        assert_eq!(err.data["terminal"], "work");
        // The send's first lookup ran while the remove was in progress; its
        // lookup inside the lock found the pane gone, so no send call.
        assert_eq!(fake.calls(), ["get", "get", "remove", "get", "get"]);
        assert!(fake.state.lock().unwrap().sent.is_empty());
        assert!(!sh.pane_locks().is_held("id-work"));
        assert_eq!(sh.pane_locks().waiting("id-work"), 0);

        // Later calls on the removed name get not_found too.
        fake.state.lock().unwrap().screen = Some("screen".to_string());
        for (tool, extra) in [
            ("answer_dialog", json!({"choice": "allow_once"})),
            ("remove_pane", json!({})),
        ] {
            let mut a = args(extra);
            a.insert("terminal".to_string(), json!("work"));
            assert_eq!(call(&sh, tool, &a).unwrap_err().data["error"], "not_found");
        }
    }

    #[test]
    fn read_output_refuses_history_on_a_backend_without_it() {
        let mut fake = FakeBackend::new(false);
        fake.caps.history = false;
        fake.add_pane("work", State::Done, None);
        fake.state.lock().unwrap().screen = Some("screen".to_string());
        let sh = shepherd(&fake);

        let err = call(
            &sh,
            "read_output",
            &args(json!({"terminal": "work", "lines": 5})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "unsupported");
        assert!(!fake.calls().contains(&"read"));

        let out = call(
            &sh,
            "read_output",
            &args(json!({"terminal": "work", "lines": 0})),
        )
        .unwrap();
        assert_eq!(out["text"], "screen");
        assert!(fake.calls().contains(&"read"));
    }

    #[test]
    fn start_pane_checks_env_before_any_call() {
        let mut fake = FakeBackend::new(false);
        fake.caps.client_env = false;
        let sh = shepherd(&fake);
        let refused = |env: Value| {
            call(
                &sh,
                "start_pane",
                &args(json!({"argv": ["true"], "env": env})),
            )
            .unwrap_err()
        };
        assert_eq!(refused(json!({"A": "b"})).data["error"], "unsupported");
        for bad in [
            json!(["A=b"]),
            json!({"lower": "x"}),
            json!({"1A": "x"}),
            json!({"A-B": "x"}),
            json!({"A": 1}),
            json!({"A": "x\u{1b}y"}),
        ] {
            let err = refused(bad.clone());
            assert_ne!(err.data["error"], "unsupported", "{bad}");
        }
        let many: serde_json::Map<String, Value> = (0..=MAX_RUN_ENV)
            .map(|i| (format!("K{i}"), json!("v")))
            .collect();
        let err = refused(Value::Object(many));
        assert!(err.text.contains("at most 32"), "{}", err.text);
        assert!(fake.calls().is_empty());
        let mut env = env_arg(&args(json!({"env": {"B": " two ", "A": ""}}))).unwrap();
        env.sort();
        assert_eq!(
            env,
            [
                ("A".to_string(), String::new()),
                ("B".to_string(), "two".to_string())
            ]
        );
    }

    #[test]
    fn answer_dialog_leaves_the_pre_enter_check_to_a_backend_with_conditional_keys() {
        let mut fake = FakeBackend::new(false);
        fake.caps.conditional_keys = true;
        fake.add_pane("work", State::Blocked, Some("permission"));
        fake.state.lock().unwrap().screen = Some(crate::dialog::fixtures::dialog_on_allow());
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Kiro);
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

    /// The Devin dialog with the highlight on `cursor` (0 Allow, 1 Bypass mode, 2 Deny).
    fn devin_screen(cursor: usize) -> String {
        crate::devin_dialog::fixtures::dialog_with_cursor_on(cursor)
    }

    /// The keys each `keys` call sent, by name.
    fn key_names(fake: &FakeBackend) -> Vec<Vec<&'static str>> {
        fake.state
            .lock()
            .unwrap()
            .keys
            .iter()
            .map(|keys| keys.iter().map(Key::as_str).collect())
            .collect()
    }

    /// answer_dialog on pane `work`, which is a Devin pane.
    fn devin_answer(fake: &FakeBackend, choice: &str) -> Result<Value, ToolError> {
        let sh = shepherd(fake);
        sh.assume_agent("work", Agent::Devin);
        call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": choice})),
        )
    }

    fn blocked_devin_pane(screens: Vec<String>) -> FakeBackend {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Blocked, Some("permission"));
        let mut state = fake.state.lock().unwrap();
        let mut screens = screens.into_iter();
        state.screen = screens.next();
        state.next_screens = screens.collect();
        drop(state);
        fake
    }

    #[test]
    fn devin_allow_once_presses_enter_only_when_allow_is_highlighted() {
        let fake = blocked_devin_pane(vec![devin_screen(0)]);
        let out = devin_answer(&fake, "allow_once").unwrap();
        assert_eq!(out["agent"], "devin");
        assert_eq!(out["selected"], "Allow");
        assert_eq!(out["moves"], 0);
        assert_eq!(key_names(&fake), [["enter"]]);
    }

    #[test]
    fn devin_deny_once_moves_down_one_key_at_a_time_then_enter() {
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(1), devin_screen(2)]);
        let out = devin_answer(&fake, "deny_once").unwrap();
        assert_eq!(out["selected"], "Deny");
        assert_eq!(out["moves"], 2);
        assert_eq!(key_names(&fake), [["down"], ["down"], ["enter"]]);
    }

    #[test]
    fn devin_refuses_when_the_highlight_lands_on_bypass_mode_and_never_enters() {
        // The second Down leaves the highlight on Bypass mode.
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(1), devin_screen(1)]);
        let err = devin_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("not on"), "{}", err.text);
        assert_eq!(key_names(&fake), [["down"], ["down"], ["esc"]]);
    }

    #[test]
    fn devin_refuses_when_a_move_does_not_land_on_the_target() {
        // The Down key is ignored: the highlight stays on Allow.
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(0)]);
        let err = devin_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert_eq!(key_names(&fake), [["down"], ["esc"]]);
    }

    #[test]
    fn devin_never_sends_enter_after_a_failed_check() {
        let cases: Vec<Vec<String>> = vec![
            // Cursor stops short of the target.
            vec![devin_screen(0), devin_screen(0)],
            // The dialog stops parsing after a move.
            vec![devin_screen(0), "no dialog here\n".to_string()],
        ];
        for screens in cases {
            let fake = blocked_devin_pane(screens);
            let err = devin_answer(&fake, "deny_once").unwrap_err();
            assert_eq!(err.data["error"], "refused");
            assert!(
                key_names(&fake).iter().flatten().all(|key| *key != "enter"),
                "{:?}",
                key_names(&fake)
            );
        }
    }

    #[test]
    fn devin_refuses_without_keys_when_the_pane_has_no_recorded_agent() {
        let fake = blocked_devin_pane(vec![devin_screen(0)]);
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "allow_once"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("no dialect applies"), "{}", err.text);
        assert!(key_names(&fake).is_empty());
    }

    /// answer_dialog on pane `work`, which is an OpenCode pane.
    fn opencode_answer(fake: &FakeBackend, choice: &str) -> Result<Value, ToolError> {
        let sh = shepherd(fake);
        sh.assume_agent("work", Agent::OpenCode);
        call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": choice})),
        )
    }

    fn opencode_shell() -> String {
        crate::opencode_dialog::fixtures::shell_dialog()
    }

    #[test]
    fn opencode_deny_once_sends_only_esc_after_the_dialog_checks() {
        for screen in [
            opencode_shell(),
            crate::opencode_dialog::fixtures::edit_dialog(),
        ] {
            let fake = blocked_devin_pane(vec![screen]);
            let out = opencode_answer(&fake, "deny_once").unwrap();
            assert_eq!(out["agent"], "opencode");
            assert_eq!(out["selected"], "Reject");
            assert_eq!(out["keys"], json!(["esc"]));
            assert_eq!(key_names(&fake), [["esc"]]);
            assert_eq!(fake.calls(), ["get", "get", "read", "get", "keys"]);
        }
    }

    #[test]
    fn opencode_allow_once_is_refused_because_the_highlight_cannot_be_verified() {
        let fake = blocked_devin_pane(vec![opencode_shell()]);
        let err = opencode_answer(&fake, "allow_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(
            err.text.contains("highlighted option cannot be verified"),
            "{}",
            err.text
        );
        assert!(key_names(&fake).is_empty());
    }

    #[test]
    fn opencode_sends_nothing_unless_the_pane_is_blocked_on_a_permission_dialog() {
        let cases = [
            (State::Working, None),
            (State::Idle, None),
            (State::Blocked, Some("auth")),
            (State::Blocked, Some("question")),
        ];
        for (state, kind) in cases {
            for choice in ["allow_once", "deny_once"] {
                let fake = FakeBackend::new(false);
                fake.add_pane("work", state.clone(), kind);
                fake.state.lock().unwrap().screen = Some(opencode_shell());
                let err = opencode_answer(&fake, choice).unwrap_err();
                assert_eq!(err.data["error"], "refused", "{state:?} {kind:?}");
                assert!(key_names(&fake).is_empty(), "{state:?} {kind:?}");
            }
        }
    }

    #[test]
    fn opencode_deny_once_sends_nothing_when_the_dialog_is_not_the_expected_one() {
        // The options are in the wrong order, so the dialog never parses.
        let wrong = opencode_shell().replace(
            "Allow once   Always allow   Reject",
            "Allow once   Reject   Always allow",
        );
        let fake = blocked_devin_pane(vec![wrong]);
        let err = opencode_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("in that order"), "{}", err.text);
        assert!(key_names(&fake).is_empty());
    }

    #[test]
    fn opencode_never_sends_right_or_enter() {
        let fake = blocked_devin_pane(vec![opencode_shell()]);
        opencode_answer(&fake, "deny_once").unwrap();
        let fake2 = blocked_devin_pane(vec![opencode_shell()]);
        opencode_answer(&fake2, "allow_once").unwrap_err();
        for fake in [&fake, &fake2] {
            assert!(key_names(fake)
                .iter()
                .flatten()
                .all(|key| !matches!(*key, "enter" | "right" | "left" | "tab")));
        }
    }

    #[test]
    fn opencode_idle_after_a_denied_dialog_is_cancelled() {
        let fake = blocked_devin_pane(vec![opencode_shell()]);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::OpenCode);
        call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "deny_once"})),
        )
        .unwrap();
        fake.state
            .lock()
            .unwrap()
            .waits
            .push_back(wait_idle(Some(3)));
        let out = call(&sh, "wait_status", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out["outcome"], "cancelled");
    }

    fn wait_result(state: State, turn: Option<u64>) -> crate::backend::WaitResult {
        crate::backend::WaitResult {
            ended: true,
            reason: Some(crate::backend::WaitReason::Status),
            status: Some(crate::backend::ProgramStatus { state, kind: None }),
            exit_code: None,
            turn,
        }
    }

    fn wait_idle(turn: Option<u64>) -> crate::backend::WaitResult {
        wait_result(State::Idle, turn)
    }

    fn wait_outcome_of(agent: Agent, waits: Vec<crate::backend::WaitResult>) -> Value {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Working, None);
        fake.state.lock().unwrap().waits = waits.into();
        let sh = shepherd(&fake);
        sh.assume_agent("work", agent);
        call(
            &sh,
            "wait_status",
            &args(json!({"terminal": "work", "timeout": 5})),
        )
        .unwrap()
    }

    #[test]
    fn an_opencode_turn_with_no_dialog_ends_done_at_idle_after_clear() {
        // working, then clear (the unknown state), then idle: no blocked at all.
        let out = wait_outcome_of(
            Agent::OpenCode,
            vec![wait_result(State::Other, Some(1)), wait_idle(Some(1))],
        );
        assert_eq!(out["outcome"], "done");
        assert_eq!(out["turn_complete"], true);
        assert_eq!(out["status"], "idle");
    }

    #[test]
    fn an_opencode_idle_before_any_turn_is_not_done() {
        let out = wait_outcome_of(Agent::OpenCode, vec![wait_idle(Some(0))]);
        assert_eq!(out["outcome"], "other");
        assert_eq!(out["turn_complete"], false);
    }

    #[test]
    fn idle_is_not_done_for_kiro_or_devin() {
        for agent in [Agent::Kiro, Agent::Devin] {
            let out = wait_outcome_of(agent, vec![wait_idle(Some(3))]);
            assert_eq!(out["outcome"], "other");
            // Only OpenCode waits out the unknown state.
            let out = wait_outcome_of(agent, vec![wait_result(State::Other, Some(3))]);
            assert_eq!(out["outcome"], "other");
            assert_eq!(out["status"], "other");
        }
    }

    #[test]
    fn devin_refuses_when_the_highlight_does_not_start_on_allow() {
        // Bypass mode or Deny is highlighted before any key. Only Esc is sent.
        for start in [1, 2] {
            for choice in ["allow_once", "deny_once"] {
                let fake = blocked_devin_pane(vec![devin_screen(start)]);
                let err = devin_answer(&fake, choice).unwrap_err();
                assert_eq!(err.data["error"], "refused", "{start} {choice}");
                assert!(err.text.contains("not on Allow"), "{}", err.text);
                assert_eq!(key_names(&fake), [["esc"]], "{start} {choice}");
            }
        }
    }

    #[test]
    fn devin_allow_once_refuses_a_scope_other_than_once() {
        let screen = devin_screen(0).replace("→ Once", "→ Session");
        let fake = blocked_devin_pane(vec![screen]);
        let err = devin_answer(&fake, "allow_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("Once scope"), "{}", err.text);
        assert_eq!(key_names(&fake), [["esc"]]);
    }

    #[test]
    fn devin_deny_once_does_not_look_at_the_allow_scope() {
        let session = |cursor: usize| devin_screen(cursor).replace("→ Once", "→ Session");
        let fake = blocked_devin_pane(vec![session(0), session(1), session(2)]);
        let out = devin_answer(&fake, "deny_once").unwrap();
        assert_eq!(out["selected"], "Deny");
        assert_eq!(key_names(&fake), [["down"], ["down"], ["enter"]]);
    }

    #[test]
    fn a_devin_cancel_stays_cancelled_through_a_timeout_and_intermediate_results() {
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(1), devin_screen(2)]);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Devin);
        call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "deny_once"})),
        )
        .unwrap();
        let result = |ended, reason, state: Option<State>| WaitResult {
            ended,
            reason,
            status: state.map(|state| ProgramStatus { state, kind: None }),
            exit_code: None,
            turn: Some(2),
        };
        fake.state.lock().unwrap().waits.extend([
            result(false, Some(WaitReason::Timeout), None),
            result(true, Some(WaitReason::Status), Some(State::Other)),
            result(true, Some(WaitReason::Status), Some(State::Idle)),
            // The cancel was reported, so a later idle is not one this answer caused.
            result(true, Some(WaitReason::Status), Some(State::Idle)),
        ]);
        let outcomes: Vec<String> = (0..4)
            .map(|_| {
                let out = call(
                    &sh,
                    "wait_status",
                    &args(json!({"terminal": "work", "timeout": 1})),
                )
                .unwrap();
                out["outcome"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(outcomes, ["timeout", "other", "cancelled", "other"]);
    }

    #[test]
    fn a_pane_started_without_a_profile_forgets_the_agent_of_an_earlier_pane_with_its_name() {
        let fake = FakeBackend::new(true);
        fake.state.lock().unwrap().runnable = true;
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Devin);
        call(
            &sh,
            "start_pane",
            &args(json!({"argv": ["true"], "name": "work"})),
        )
        .unwrap();
        assert_eq!(sh.agent_of("work"), None);
    }

    #[test]
    fn kiro_mismatch_refuses_without_an_escape() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Blocked, Some("permission"));
        fake.state.lock().unwrap().screen = Some(crate::dialog::fixtures::dialog_on_allow());
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Kiro);
        let err = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "deny_once"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert_eq!(key_names(&fake), [["down"]]);
    }

    #[test]
    fn remove_pane_forgets_the_pane_agent() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Done, None);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Devin);
        call(&sh, "remove_pane", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(sh.agent_of("work"), None);
    }

    /// A policy with a devin profile (`mode`) and a kiro profile, both
    /// programs in `<dir>/bin`, which the policy's PATH names.
    fn agent_policy(dir: &std::path::Path, mode: &str) -> LaunchPolicy {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for program in ["devin", "kiro-cli"] {
            let path = bin.join(program);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let bin = bin.to_string_lossy().into_owned();
        let text = json!({"profiles": [
            {"name": "devin", "argv": ["devin"], "cwds": [dir], "agent": "devin", "permission_mode": mode},
            {"name": "kiro", "argv": ["kiro-cli", "chat"], "extra_args": ["--trust-workspace"],
             "cwds": [dir]},
        ]});
        LaunchPolicy::from_json(&text.to_string())
            .unwrap()
            .with_env_lookup(move |key| (key == "PATH").then(|| bin.clone()))
    }

    /// The argv `start_pane` asked the backend to run (the fake refuses the run).
    fn started_argv(profile: &str, mode: &str) -> Vec<String> {
        let dir = TempDir::new("tools-agent-argv");
        let fake = FakeBackend::new(true);
        let sh = Shepherd::with_policy(fake.clone(), agent_policy(dir.path(), mode));
        let _ = call(
            &sh,
            "start_pane",
            &args(json!({"profile": profile, "name": "p"})),
        );
        let runs = fake.state.lock().unwrap().runs.clone();
        runs[0].argv.clone().unwrap()
    }

    #[test]
    fn devin_profile_passes_its_permission_flag_and_no_bypass_by_default() {
        let argv = started_argv("devin", "normal");
        assert_eq!(
            argv[1..].iter().map(String::as_str).collect::<Vec<_>>(),
            ["--permission-mode", "auto"]
        );
        assert!(!argv.iter().any(|a| a == "dangerous"), "{argv:?}");
        assert_eq!(
            started_argv("devin", "accept-edits")[1..3],
            ["--permission-mode", "accept-edits"]
        );
        assert_eq!(
            started_argv("devin", "bypass")[1..3],
            ["--permission-mode", "dangerous"]
        );
    }

    #[test]
    fn kiro_profile_argv_is_unchanged_by_the_permission_field() {
        let argv = started_argv("kiro", "normal");
        assert_eq!(
            argv[1..].iter().map(String::as_str).collect::<Vec<_>>(),
            ["chat", "--trust-workspace"]
        );
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
        assert_eq!(
            wait_outcome(true, status, Some(&State::Done), false),
            "done"
        );
        assert_eq!(
            wait_outcome(true, status, Some(&State::Blocked), false),
            "blocked"
        );
        assert_eq!(
            wait_outcome(true, status, Some(&State::Idle), false),
            "other"
        );
        assert_eq!(
            wait_outcome(true, status, Some(&State::Other), false),
            "other"
        );
        assert_eq!(
            wait_outcome(true, Some(&WaitReason::Exit), None, false),
            "exited"
        );
        assert_eq!(
            wait_outcome(false, Some(&WaitReason::Timeout), None, false),
            "timeout"
        );
        assert_eq!(
            wait_outcome(false, Some(&WaitReason::Other), None, false),
            "other"
        );
        assert_eq!(wait_outcome(true, None, Some(&State::Done), false), "other");
    }

    #[test]
    fn idle_after_an_answered_dialog_is_cancelled_not_done() {
        let status = Some(&WaitReason::Status);
        assert_eq!(
            wait_outcome(true, status, Some(&State::Idle), true),
            "cancelled"
        );
        // Without an answer, idle stays what it was.
        assert_eq!(
            wait_outcome(true, status, Some(&State::Idle), false),
            "other"
        );
        // Done after an answer is still done.
        assert_eq!(wait_outcome(true, status, Some(&State::Done), true), "done");
    }

    /// An executable `kiro-cli` in `<dir>/bin`, which the policies' PATH names.
    fn fake_kiro(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join("kiro-cli");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::canonicalize(bin).unwrap()
    }

    fn kiro_policy(dir: &std::path::Path, arbitrary: bool) -> LaunchPolicy {
        let bin = fake_kiro(dir).to_string_lossy().into_owned();
        let text = json!({
            "allow_arbitrary_command": arbitrary,
            "profiles": [
                {"name": "kiro", "argv": ["kiro-cli", "chat"], "extra_args": ["--trust-workspace"],
                 "cwds": [dir], "env": {"KIRO_API_KEY": "env:KEY_SOURCE"}},
                {"name": "plain", "argv": ["/bin/sh"], "cwds": [dir]}
            ]
        });
        LaunchPolicy::from_json(&text.to_string())
            .unwrap()
            .with_env_lookup(move |key| match key {
                "KEY_SOURCE" => Some("profile-secret".to_string()),
                "PATH" => Some(bin.clone()),
                _ => None,
            })
    }

    #[test]
    fn start_pane_refuses_argv_and_line_by_default() {
        let fake = FakeBackend::new(true);
        let sh = Shepherd::new(fake.clone());
        for start in [
            json!({"argv": ["kiro-cli", "chat"]}),
            json!({"line": "echo hi"}),
            json!({}),
        ] {
            let err = call(&sh, "start_pane", &args(start.clone())).unwrap_err();
            assert_eq!(err.data["error"], "profile_required", "{start}");
        }
        assert!(fake.calls().is_empty());
        let dir = crate::testutil::TempDir::new("tools-profile-default");
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), false));
        let err = call(&sh, "start_pane", &args(json!({"argv": ["true"]}))).unwrap_err();
        assert_eq!(err.data["error"], "profile_required");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn start_pane_runs_a_profile_with_its_fixed_argv_and_env() {
        let dir = crate::testutil::TempDir::new("tools-profile-run");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let fake = FakeBackend::new(true);
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), true));
        let sub = dir.path().join("sub");
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": "k", "cwd": sub})),
        )
        .unwrap_err();
        call(
            &sh,
            "start_pane",
            &args(json!({"argv": ["kiro-cli"], "name": "a"})),
        )
        .unwrap_err();
        let runs = fake.state.lock().unwrap().runs.clone();
        assert_eq!(runs.len(), 2);
        let canonical = std::fs::canonicalize(&sub).unwrap();
        let pinned = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("bin/kiro-cli");
        assert_eq!(
            runs[0].argv.as_deref(),
            Some(
                &[
                    pinned.to_string_lossy().into_owned(),
                    "chat".into(),
                    "--trust-workspace".into()
                ][..]
            )
        );
        assert_eq!(runs[0].cwd.as_deref(), canonical.to_str());
        assert_eq!(runs[0].profile.as_deref(), Some("kiro"));
        assert_eq!(
            runs[0].profile_env,
            [("KIRO_API_KEY".to_string(), Secret::new("profile-secret"))]
        );
        assert!(runs[0].env.is_empty());
        // An arbitrary command, allowed here, gets no profile environment.
        assert_eq!(runs[1].profile, None);
        assert!(runs[1].profile_env.is_empty());
    }

    #[test]
    fn start_pane_checks_a_profile_before_any_call() {
        let dir = crate::testutil::TempDir::new("tools-profile-refuse");
        let outside = crate::testutil::TempDir::new("tools-profile-outside");
        let fake = FakeBackend::new(true);
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), true));
        for (start, code) in [
            (json!({"profile": "nope"}), "invalid_input"),
            (json!({"profile": "kiro", "argv": ["x"]}), "invalid_input"),
            (json!({"profile": "kiro", "line": "x"}), "invalid_input"),
            (
                json!({"profile": "kiro", "env": {"A": "b"}}),
                "invalid_input",
            ),
            (
                json!({"profile": "kiro", "cwd": outside.path()}),
                "invalid_input",
            ),
            (
                json!({"profile": "kiro", "cwd": "relative"}),
                "invalid_input",
            ),
            (
                json!({"profile": "kiro", "cwd": dir.path().join("missing")}),
                "not_found",
            ),
            (
                json!({"profile": "kiro", "name": "Bad Name"}),
                "invalid_input",
            ),
        ] {
            let err = call(&sh, "start_pane", &args(start.clone())).unwrap_err();
            assert_eq!(err.data["error"], code, "{start}: {err:?}");
            assert!(!err.text.contains("profile-secret"), "{err:?}");
        }
        assert!(fake.calls().is_empty());

        // A source that cannot be read: refused, no run, no value.
        let bin = fake_kiro(dir.path()).to_string_lossy().into_owned();
        let unset = kiro_policy(dir.path(), false)
            .with_env_lookup(move |key| (key == "PATH").then(|| bin.clone()));
        let sh = Shepherd::with_policy(fake.clone(), unset);
        let err = call(&sh, "start_pane", &args(json!({"profile": "kiro"}))).unwrap_err();
        assert_eq!(err.data["error"], "profile_unavailable");
        assert!(err.text.contains("env:KEY_SOURCE"), "{err:?}");
        assert!(fake.calls().is_empty());

        // A backend without per-run environments refuses a credential
        // profile before its source is read, and runs one without env.
        let mut hc_like = FakeBackend::new(true);
        hc_like.caps.client_env = false;
        let sh = Shepherd::with_policy(hc_like.clone(), kiro_policy(dir.path(), false));
        let err = call(&sh, "start_pane", &args(json!({"profile": "kiro"}))).unwrap_err();
        assert_eq!(err.data["error"], "unsupported");
        assert!(hc_like.calls().is_empty());
        call(&sh, "start_pane", &args(json!({"profile": "plain"}))).unwrap_err();
        assert_eq!(hc_like.calls(), ["run"]);
    }

    #[test]
    fn a_profile_program_is_never_found_through_the_callers_cwd() {
        // Review SOL2-B1: PATH=.:<empty>:<bin>, an impostor kiro-cli in an
        // allowed subdirectory, and a profile secret.
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testutil::TempDir::new("tools-profile-impostor");
        let sub = dir.path().join("work");
        std::fs::create_dir_all(&sub).unwrap();
        let impostor = sub.join("kiro-cli");
        std::fs::write(&impostor, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&impostor, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = fake_kiro(dir.path()).to_string_lossy().into_owned();
        let path = format!(".::work:{bin}");
        let fake = FakeBackend::new(true);
        let policy = kiro_policy(dir.path(), false).with_env_lookup(move |key| match key {
            "KEY_SOURCE" => Some("profile-secret".to_string()),
            "PATH" => Some(path.clone()),
            _ => None,
        });
        let sh = Shepherd::with_policy(fake.clone(), policy);
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": "k", "cwd": sub})),
        )
        .unwrap_err();
        let runs = fake.state.lock().unwrap().runs.clone();
        assert_eq!(runs.len(), 1);
        let argv0 = &runs[0].argv.as_ref().unwrap()[0];
        assert_eq!(argv0, &format!("{bin}/kiro-cli"));
        assert!(std::path::Path::new(argv0).is_absolute());

        // With only relative or empty entries, nothing is found and no
        // credential is read.
        let fake = FakeBackend::new(true);
        let policy = kiro_policy(dir.path(), false).with_env_lookup(|key| match key {
            "KEY_SOURCE" => panic!("a credential was read"),
            "PATH" => Some(".::work".to_string()),
            _ => None,
        });
        let sh = Shepherd::with_policy(fake.clone(), policy);
        let err = call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "cwd": sub})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "not_found", "{err:?}");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn list_panes_shows_profiles_without_their_environment() {
        let dir = crate::testutil::TempDir::new("tools-profile-list");
        let policy = kiro_policy(dir.path(), false);
        let shown = policy.to_json();
        assert_eq!(shown["allow_arbitrary_command"], false);
        assert_eq!(shown["profiles"][0]["name"], "kiro");
        assert_eq!(
            shown["profiles"][0]["cwds"][0],
            std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap()
        );
        let text = shown.to_string();
        assert!(
            !text.contains("KIRO_API_KEY") && !text.contains("KEY_SOURCE"),
            "{text}"
        );
        assert!(!text.contains("kiro-cli"), "{text}");
    }
}
