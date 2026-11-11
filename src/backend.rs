//! The terminal backend: what Shepherd needs from the program that runs panes.
//!
//! The MCP tools in `tools.rs` talk only to [`TerminalBackend`]. Every type here
//! is Shepherd's own vocabulary; an adapter (`hc::HcBackend`, and the native
//! core in `native/`) maps its backend's wire format onto it. Safety rules (input validation, the pane
//! lock, refusing to type into a blocked pane, the answer_dialog checks) live in
//! Shepherd, not in a backend. [`Capabilities`] tells Shepherd which of them a
//! backend can also enforce atomically.

use serde_json::{json, Value};
use std::fmt;
use std::time::Duration;

/// What a backend can do. Shepherd picks a native call or a fallback per flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// `send` enforces its [`InputGuard`] in the same step as the write, or
    /// refuses with [`ErrorKind::Unsupported`] for a guard field it cannot
    /// enforce. When false, `send` ignores the guard and Shepherd checks the
    /// pane's status under the pane lock first.
    pub conditional_input: bool,
    /// The same for `keys`.
    pub conditional_keys: bool,
    /// `run` accepts a client-chosen program (`argv` or `line`).
    pub client_command: bool,
    /// `run` accepts a client-chosen working directory.
    pub client_cwd: bool,
    /// `run` accepts extra environment variables for that one run.
    pub client_env: bool,
    /// `read` returns rendered screen text (not raw output bytes).
    pub screen_text: bool,
    /// The backend reports the program's OSC 7501 status and counts turns.
    pub program_status: bool,
    /// `read` with `history_lines > 0` returns scrollback above the screen.
    /// When false, `read_output` with `lines > 0` returns `unsupported`.
    pub history: bool,
    /// `wait` blocks inside the backend instead of Shepherd polling it.
    pub native_wait: bool,
    /// Panes outlive a Shepherd restart.
    pub survives_restart: bool,
}

/// A pane as the caller named it, or, after Shepherd resolved the name, the
/// backend's own id for it ([`TerminalInfo::reference`]). Opaque: the backend
/// resolves it. `get` must accept both; Shepherd uses the caller's name only
/// for that first `get`, and checks the pane found carries the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRef(String);

impl TerminalRef {
    pub fn new(reference: impl Into<String>) -> Self {
        Self(reference.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The program's own status word. A word Shepherd does not know is `Other`,
/// shown to callers as `other`: a backend's own words never reach them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    Other,
}

impl State {
    pub fn parse(word: &str) -> Self {
        match word {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "error" => Self::Error,
            _ => Self::Other,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
            Self::Other => "other",
        }
    }
}

/// The dialog kinds Shepherd names. A word it does not know is `other`, so a
/// backend's own words never reach callers. Adapters pass every kind through
/// this, and Shepherd applies it again wherever a kind leaves for a caller.
pub fn dialog_kind(word: &str) -> String {
    match word {
        "permission" | "question" | "auth" => word,
        _ => "other",
    }
    .to_string()
}

/// What the program reports it is doing. `kind` is set only for blocked
/// (`permission`, `question`, `auth`, or `other`), and only when the program
/// said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    pub state: State,
    pub kind: Option<String>,
}

impl ProgramStatus {
    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state.as_str(),
            "kind": self.kind.as_deref().map(dialog_kind),
        })
    }

    pub fn is_blocked_with(&self, kind: &str) -> bool {
        self.state == State::Blocked && self.kind.as_deref() == Some(kind)
    }
}

/// `null` when there is no status, else `{state, kind}`.
pub fn status_json(status: Option<&ProgramStatus>) -> Value {
    status.map_or(Value::Null, ProgramStatus::to_json)
}

/// One pane. `id` is the backend's own stable reference, used inside Shepherd
/// (the pane lock keys on it) and never shown to callers: they use `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalInfo {
    pub id: String,
    pub name: Option<String>,
    /// False once its program has ended.
    pub live: bool,
    pub exit_code: Option<i64>,
    /// Null when the program never reported a status, or after it ended.
    pub status: Option<ProgramStatus>,
    /// Whether the program ever reported a status.
    pub reported: bool,
    /// Goes up each time the status leaves working.
    pub turn: Option<u64>,
    /// When the status last changed, ms since the epoch.
    pub status_ms: Option<u64>,
    pub argv: Option<Vec<String>>,
    pub cwd: Option<String>,
}

impl TerminalInfo {
    /// The MCP shape of a pane. It has no `id`.
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "live": self.live,
            "exit_code": self.exit_code,
            "status": status_json(self.status.as_ref()),
            "reported": self.reported,
            "turn": self.turn,
            "status_ms": self.status_ms,
            "argv": self.argv,
            "cwd": self.cwd,
        })
    }

    pub fn reference(&self) -> TerminalRef {
        TerminalRef::new(self.id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalList {
    pub terminals: Vec<TerminalInfo>,
    /// Finished panes left out because `all` was false, when the backend says.
    pub hidden_finished: Option<u64>,
}

/// What to start. Exactly one of `argv` or `line` is set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSpec {
    pub argv: Option<Vec<String>>,
    pub line: Option<String>,
    pub cwd: Option<String>,
    pub name: Option<String>,
    /// Extra environment variables for this run only, in order. Empty means
    /// none. Backends without `client_env` refuse a non-empty list.
    pub env: Vec<(String, String)>,
    /// The operator-defined launch profile Shepherd resolved `argv` and `cwd`
    /// from (`profiles.rs`), or None for a caller-chosen command.
    pub profile: Option<String>,
    /// The profile's environment, values already read from their sources.
    /// Only Shepherd sets it, and only from a profile; a caller's command
    /// never gets one. Backends without `client_env` refuse a non-empty list.
    pub profile_env: Vec<(String, Secret)>,
}

/// A value that may be a credential. Its `Debug` never shows it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the child's environment only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Where a read came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadSource {
    Screen,
    History,
    /// A full-screen program read by scrolling it.
    Stitched,
    Other,
}

impl ReadSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Screen => "screen",
            Self::History => "history",
            Self::Stitched => "stitched",
            Self::Other => "other",
        }
    }
}

/// Screen text, with escape sequences still in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenText {
    pub text: String,
    /// False when part of what was asked for may be missing.
    pub complete: Option<bool>,
    pub source: Option<ReadSource>,
    /// Why only the screen was read.
    pub limit: Option<ReadLimit>,
}

/// Why a read returned only the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadLimit {
    /// The program is blocked on a dialog.
    Blocked,
    Other,
}

impl ReadLimit {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Other => "other",
        }
    }
}

/// Conditions a backend with conditional input or keys enforces atomically
/// with the write. A backend without the matching capability ignores it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputGuard {
    /// Refuse with [`ErrorKind::Blocked`] while the program is blocked.
    pub refuse_when_blocked: bool,
    /// Refuse with [`ErrorKind::StatusMismatch`] unless the state is this.
    pub if_state: Option<State>,
    /// ... and the blocked kind is this.
    pub if_kind: Option<String>,
    /// ... and the turn is this.
    pub expect_turn: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendResult {
    /// The turn when the text went in.
    pub turn: Option<u64>,
    /// Whether it went working soon after; None when nobody can tell.
    pub started: Option<bool>,
    /// The status word when the text went in.
    pub status: Option<State>,
}

/// A key. Encoding it for the program is the backend's job. `Other` is a key
/// a backend reported that Shepherd does not know; it cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Enter,
    Up,
    Down,
    Esc,
    CtrlC,
    Other,
}

impl Key {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Enter => "enter",
            Self::Up => "up",
            Self::Down => "down",
            Self::Esc => "esc",
            Self::CtrlC => "ctrl+c",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeysResult {
    pub status: Option<State>,
    pub turn: Option<u64>,
}

impl KeysResult {
    pub fn to_json(&self) -> Value {
        json!({"status": self.status.as_ref().map(State::as_str), "turn": self.turn})
    }
}

/// Why a wait returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReason {
    /// The program ended.
    Exit,
    /// The program reported a status other than working.
    Status,
    /// The timeout passed; the pane is still running.
    Timeout,
    Other,
}

impl WaitReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exit => "exit",
            Self::Status => "status",
            Self::Timeout => "timeout",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitResult {
    /// False on timeout.
    pub ended: bool,
    /// None when the backend gave no reason.
    pub reason: Option<WaitReason>,
    /// The status when it returned. `kind` may be missing even when blocked;
    /// Shepherd then reads it with `get`.
    pub status: Option<ProgramStatus>,
    pub exit_code: Option<i64>,
    pub turn: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptOutcome {
    Stopped,
    Exited,
    StillWorking,
    Unknown,
    Other,
}

impl InterruptOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Exited => "exited",
            Self::StillWorking => "still_working",
            Self::Unknown => "unknown",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptResult {
    pub key: Key,
    pub outcome: InterruptOutcome,
}

/// What went wrong, in Shepherd's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// The backend cannot be reached.
    Unavailable,
    /// The backend did not answer in time.
    Timeout,
    NotFound,
    NameTaken,
    /// The program is blocked on a dialog; nothing was typed.
    Blocked {
        kind: Option<String>,
    },
    /// An [`InputGuard`] condition did not hold; nothing was sent.
    StatusMismatch,
    /// The pane's program has ended.
    Finished,
    /// The backend cannot do this.
    Unsupported(String),
    /// The backend refused the request's parameters.
    InvalidInput,
    /// The backend's reply could not be used.
    Protocol,
    Other,
}

/// A backend failure. `detail` is the backend's own text, for debugging only;
/// it may use the backend's vocabulary and is never shown as Shepherd's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub kind: ErrorKind,
    pub detail: String,
}

impl BackendError {
    pub fn new(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// Shepherd's error code for this failure.
    pub fn code(&self) -> &'static str {
        match self.kind {
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::Timeout => "timeout",
            ErrorKind::NotFound => "not_found",
            ErrorKind::NameTaken => "name_taken",
            ErrorKind::Blocked { .. } => "pane_blocked",
            ErrorKind::StatusMismatch => "status_mismatch",
            ErrorKind::Finished => "pane_finished",
            ErrorKind::Unsupported(_) => "unsupported",
            ErrorKind::InvalidInput => "invalid_input",
            ErrorKind::Protocol | ErrorKind::Other => "backend_error",
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.detail)
    }
}

impl std::error::Error for BackendError {}

/// A synchronous terminal backend. Shepherd is blocking (`tiny_http`, Unix
/// sockets), so adapters wrap their I/O behind plain calls.
pub trait TerminalBackend: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    fn run(&self, spec: &RunSpec) -> Result<TerminalInfo, BackendError>;
    fn get(&self, t: &TerminalRef) -> Result<TerminalInfo, BackendError>;
    fn list(&self, all: bool) -> Result<TerminalList, BackendError>;
    /// `history_lines` 0 reads the screen.
    fn read(&self, t: &TerminalRef, history_lines: u32) -> Result<ScreenText, BackendError>;
    /// Paste `text` and press Enter.
    fn send(
        &self,
        t: &TerminalRef,
        text: &str,
        guard: &InputGuard,
    ) -> Result<SendResult, BackendError>;
    fn keys(
        &self,
        t: &TerminalRef,
        keys: &[Key],
        guard: &InputGuard,
    ) -> Result<KeysResult, BackendError>;
    /// Block until the program stops working, ends, or `timeout` passes.
    fn wait(&self, t: &TerminalRef, timeout: Duration) -> Result<WaitResult, BackendError>;
    /// Send the stop key and report what happened. Never escalates.
    fn interrupt(
        &self,
        t: &TerminalRef,
        timeout: Option<Duration>,
    ) -> Result<InterruptResult, BackendError>;
}
