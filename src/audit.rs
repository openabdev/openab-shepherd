//! The audit file: who did what, and what came of it.
//!
//! JSON Lines, append-only, one `write` per record under a mutex, file mode
//! 0600 in a 0700 directory outside every git repository. Records carry only
//! fields on a per-tool allowlist. Typed text, screens, environment values and
//! the like appear as lengths and SHA-256 digests, never as content. Tokens,
//! token hashes, `Authorization` and request bodies are never recorded.
//!
//! Failure policy: `start_pane`, `send_input`, `answer_dialog` and
//! `remove_pane` are refused with `audit_unavailable` when the record that
//! must precede them cannot be written, so a failing sink means no backend
//! call for them. `interrupt_pane` (the human's way to stop a runaway agent)
//! and the read tools go ahead when the write fails; the failure is counted
//! and reported on stderr at most once a minute.
//!
//! Shape of a call: `answer_dialog` and `start_pane` write an `intent` line
//! before and an `outcome` line after. `send_input`, `remove_pane` and
//! `interrupt_pane` write one `call` line before. Read tools write one
//! `outcome` line after. Denied calls and failed authentications get their own
//! lines. A crash can tear the last line; the next start ends it and marks the
//! first new record `recovered`.

use crate::clients::sha256_hex;
use crate::config::{check_outside_repo, ConfigError};
use crate::tools::{Args, ToolError, TOOL_NAMES};
use serde_json::{json, Map, Value};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// The path of the audit file.
pub const AUDIT_FILE_ENV: &str = "OPENAB_SHEPHERD_AUDIT_FILE";
/// The actor of the watchdog's nudges.
pub const WATCHDOG_ACTOR: &str = "system:watchdog";
/// The tool-error code of a refused call whose audit record could not be written.
pub const UNAVAILABLE_CODE: &str = "audit_unavailable";
const RECORD_VERSION: u64 = 1;
/// How much of the file's end is read to find the last sequence number.
const TAIL_BYTES: u64 = 1024 * 1024;
const WARN_INTERVAL_SECS: u64 = 60;
/// Authentication failures recorded per second; the rest are counted.
const AUTH_FAILURES_PER_SECOND: u32 = 20;
const SHORT: usize = 128;
const LONG: usize = 256;

/// Time since the Unix epoch. Injected so tests need no sleeps.
pub type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
    })
}

/// Where lines go. One call is one record.
pub trait AuditSink: Send {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()>;
}

/// A sink over any writer. After a failed write the line may be torn, so the
/// next write starts with a newline.
pub struct WriteSink<W: Write + Send> {
    writer: W,
    dirty: bool,
}

impl<W: Write + Send> WriteSink<W> {
    pub fn new(writer: W, dirty: bool) -> Self {
        Self { writer, dirty }
    }
}

impl<W: Write + Send> AuditSink for WriteSink<W> {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        // One buffer, so one write call on an O_APPEND file.
        let mut buffer = Vec::with_capacity(line.len() + 1);
        if self.dirty {
            buffer.push(b'\n');
        }
        buffer.extend_from_slice(line);
        let result = self.writer.write_all(&buffer);
        self.dirty = result.is_err();
        result
    }
}

/// The audit write failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditError(pub String);

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A call refused because its audit record could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditUnavailable;

impl AuditUnavailable {
    pub fn to_tool_error(self) -> ToolError {
        let message =
            "the audit record could not be written, so nothing was done; tell the operator";
        ToolError {
            text: format!("{UNAVAILABLE_CODE}: {message}"),
            data: json!({"error": UNAVAILABLE_CODE, "message": message}),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Refuse the call when its record cannot be written.
    FailClosed,
    /// Go ahead, count the failure.
    FailOpen,
}

/// The failure policy of a tool. A tool not listed fails closed.
pub fn policy(tool: &str) -> Policy {
    match tool {
        "interrupt_pane" | "read_output" | "get_status" | "wait_status" | "list_panes" => {
            Policy::FailOpen
        }
        _ => Policy::FailClosed,
    }
}

/// What `before` wrote, for `after` to link to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    pub intent_seq: Option<u64>,
}

struct Inner {
    sink: Box<dyn AuditSink>,
    seq: u64,
    recovered: bool,
}

#[derive(Default)]
struct AuthWindow {
    second: u64,
    count: u32,
    suppressed: u64,
}

/// The recorder: numbers and stamps records and hands them to the sink.
pub struct Audit {
    inner: Mutex<Inner>,
    clock: Clock,
    dropped: AtomicU64,
    last_warn: Mutex<Option<u64>>,
    auth_window: Mutex<AuthWindow>,
}

impl fmt::Debug for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Audit")
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

impl Audit {
    /// A recorder over `sink`. `last_seq` is the number of the last record
    /// already in the sink; `recovered` marks the first new record.
    pub fn new(sink: Box<dyn AuditSink>, last_seq: u64, recovered: bool, clock: Clock) -> Self {
        Self {
            inner: Mutex::new(Inner {
                sink,
                seq: last_seq,
                recovered,
            }),
            clock,
            dropped: AtomicU64::new(0),
            last_warn: Mutex::new(None),
            auth_window: Mutex::new(AuthWindow::default()),
        }
    }

    /// Open the audit file. The directory must exist, be owned by this user
    /// and have no group or other bits (chmod 700); the file is created 0600,
    /// or must already be a regular file of this user with no group or other
    /// bits. The path must be outside the project folder and every git repo.
    pub fn open_file(path: &Path, project_dir: &Path, clock: Clock) -> Result<Self, ConfigError> {
        // SAFETY: geteuid has no preconditions and cannot fail.
        Self::open_file_as(path, project_dir, clock, unsafe { libc::geteuid() })
    }

    fn open_file_as(
        path: &Path,
        project_dir: &Path,
        clock: Clock,
        euid: u32,
    ) -> Result<Self, ConfigError> {
        let bad = |text: String| ConfigError(text);
        let name = path
            .file_name()
            .ok_or_else(|| bad("audit file: the path has no file name".into()))?;
        let dir = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir,
            _ => Path::new("."),
        };
        let dir = fs::canonicalize(dir).map_err(|err| {
            bad(format!(
                "audit file: its directory must exist (mkdir -m 700): {err}"
            ))
        })?;
        let dir_meta = fs::metadata(&dir)
            .map_err(|err| bad(format!("audit file: cannot read its directory: {err}")))?;
        if !dir_meta.is_dir() {
            return Err(bad("audit file: its parent must be a directory".into()));
        }
        if dir_meta.uid() != euid {
            return Err(bad(
                "audit file: its directory must be owned by the user running Shepherd".into(),
            ));
        }
        if dir_meta.permissions().mode() & 0o077 != 0 {
            return Err(bad(
                "audit file: its directory must not be accessible to group or others (chmod 700)"
                    .into(),
            ));
        }
        let full = dir.join(name);
        check_outside_repo(&full, project_dir, "audit file")?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&full)
            .map_err(|err| bad(format!("audit file: cannot open it: {err}")))?;
        let meta = file
            .metadata()
            .map_err(|err| bad(format!("audit file: cannot read it: {err}")))?;
        if !meta.is_file() {
            return Err(bad("audit file must be a regular file".into()));
        }
        if meta.uid() != euid {
            return Err(bad(
                "audit file must be owned by the user running Shepherd".into()
            ));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(bad(
                "audit file must not be readable or writable by group or others (chmod 600)".into(),
            ));
        }
        let start = meta.len().saturating_sub(TAIL_BYTES);
        let mut tail = vec![0u8; (meta.len() - start) as usize];
        file.read_exact_at(&mut tail, start)
            .map_err(|err| bad(format!("audit file: cannot read its end: {err}")))?;
        let state = recover(&tail, start > 0);
        Ok(Self::new(
            Box::new(WriteSink::new(file, state.torn)),
            state.last_seq,
            state.torn,
            clock,
        ))
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> Duration {
        (self.clock)()
    }

    /// Records that could not be written so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stamp and write one record; returns its sequence number.
    fn emit(&self, mut record: Map<String, Value>) -> Result<u64, AuditError> {
        let now = self.now();
        let mut inner = self.inner();
        let seq = inner.seq + 1;
        record.insert("v".into(), json!(RECORD_VERSION));
        record.insert("seq".into(), json!(seq));
        record.insert("ts".into(), json!(now.as_secs()));
        record.insert("ts_ms".into(), json!(now.as_millis() as u64));
        record.insert("prev".into(), Value::Null);
        if inner.recovered {
            record.insert("recovered".into(), json!(true));
        }
        let mut line = serde_json::to_vec(&Value::Object(record))
            .map_err(|err| AuditError(format!("cannot serialize a record: {err}")))?;
        line.push(b'\n');
        inner
            .sink
            .write_line(&line)
            .map_err(|err| AuditError(format!("audit write failed: {err}")))?;
        inner.seq = seq;
        inner.recovered = false;
        Ok(seq)
    }

    /// Count a record that was not written and say so on stderr, at most once
    /// a minute.
    fn note_failure(&self) {
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        if self.warn_due(self.now().as_secs()) {
            eprintln!("openab-shepherd: audit write failed; {total} record(s) not written so far");
        }
    }

    fn warn_due(&self, now_secs: u64) -> bool {
        let mut last = self
            .last_warn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match *last {
            Some(then) if now_secs.saturating_sub(then) < WARN_INTERVAL_SECS => false,
            _ => {
                *last = Some(now_secs);
                true
            }
        }
    }

    fn emit_or_count(&self, record: Map<String, Value>) -> Option<u64> {
        match self.emit(record) {
            Ok(seq) => Some(seq),
            Err(_) => {
                self.note_failure();
                None
            }
        }
    }

    /// Write the record that precedes a call. A fail-closed tool whose record
    /// cannot be written gets `Err`, and the caller must not run it.
    pub fn before(
        &self,
        actor: &str,
        tool: &str,
        args: &Args,
    ) -> Result<Pending, AuditUnavailable> {
        let phase = match tool {
            "answer_dialog" | "start_pane" => "intent",
            "send_input" | "remove_pane" | "interrupt_pane" => "call",
            // Read tools write their one line after the call.
            _ if policy(tool) == Policy::FailOpen => return Ok(Pending { intent_seq: None }),
            _ => "call",
        };
        let mut record = base(actor, tool, phase);
        record.extend(request_fields(tool, args));
        match self.emit(record) {
            Ok(seq) => Ok(Pending {
                intent_seq: Some(seq),
            }),
            Err(_) => {
                self.note_failure();
                match policy(tool) {
                    Policy::FailClosed => Err(AuditUnavailable),
                    Policy::FailOpen => Ok(Pending { intent_seq: None }),
                }
            }
        }
    }

    /// Write the record that follows a call, for the tools that have one. The
    /// call is done by now, so a write failure is only counted.
    pub fn after(
        &self,
        actor: &str,
        tool: &str,
        args: &Args,
        pending: Pending,
        result: &Result<Value, ToolError>,
    ) {
        if !matches!(
            tool,
            "answer_dialog"
                | "start_pane"
                | "read_output"
                | "get_status"
                | "wait_status"
                | "list_panes"
        ) {
            return;
        }
        let mut record = base(actor, tool, "outcome");
        record.extend(request_fields(tool, args));
        if let Some(seq) = pending.intent_seq {
            record.insert("intent_seq".into(), json!(seq));
        }
        record.insert("outcome".into(), json!(outcome_text(result)));
        record.extend(result_fields(tool, result));
        self.emit_or_count(record);
    }

    /// Record a call the scope check refused. Always attempted; never blocks.
    pub fn denied(&self, actor: &str, tool: &str, args: &Args, reason: &str) {
        let mut record = base(actor, tool, "denied");
        record.extend(request_fields(tool, args));
        record.insert("outcome".into(), json!("denied"));
        record.insert("reason".into(), json!(cap(reason, LONG)));
        self.emit_or_count(record);
    }

    /// Record a failed authentication, at most [`AUTH_FAILURES_PER_SECOND`] a
    /// second; the rest are counted in `suppressed` on the next record.
    /// `id_prefix` is cut to a short `[a-z0-9-]` string here too.
    pub fn auth_failure(&self, id_prefix: &str, reason: &str) {
        let second = self.now().as_secs();
        let suppressed = {
            let mut window = self
                .auth_window
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if window.second != second {
                window.second = second;
                window.count = 0;
            }
            if window.count >= AUTH_FAILURES_PER_SECOND {
                window.suppressed += 1;
                return;
            }
            window.count += 1;
            std::mem::take(&mut window.suppressed)
        };
        let mut record = Map::new();
        record.insert("client".into(), Value::Null);
        record.insert("tool".into(), Value::Null);
        record.insert("phase".into(), json!("auth"));
        record.insert("outcome".into(), json!("denied"));
        record.insert("reason".into(), json!(cap(reason, 32)));
        let prefix: String = id_prefix
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
            .take(crate::clients::ID_PREFIX_LEN)
            .collect();
        record.insert("id_prefix".into(), json!(prefix));
        if suppressed > 0 {
            record.insert("suppressed".into(), json!(suppressed));
        }
        self.emit_or_count(record);
    }
}

fn base(actor: &str, tool: &str, phase: &str) -> Map<String, Value> {
    let mut record = Map::new();
    record.insert("client".into(), json!(cap(actor, SHORT)));
    let tool = if TOOL_NAMES.contains(&tool) {
        json!(tool)
    } else {
        json!("unknown")
    };
    record.insert("tool".into(), tool);
    record.insert("phase".into(), json!(phase));
    record
}

/// `cap` characters at most, no more.
fn cap(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

fn str_arg<'a>(args: &'a Args, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn put_capped(record: &mut Map<String, Value>, field: &str, args: &Args, key: &str, max: usize) {
    if let Some(text) = str_arg(args, key) {
        record.insert(field.into(), json!(cap(text, max)));
    }
}

fn put_digest(record: &mut Map<String, Value>, prefix: &str, bytes: &[u8]) {
    record.insert(format!("{prefix}_len"), json!(bytes.len()));
    record.insert(format!("{prefix}_sha256"), json!(sha256_hex(bytes)));
}

/// The allowlisted fields of a request. Anything not named here is dropped.
fn request_fields(tool: &str, args: &Args) -> Map<String, Value> {
    let mut out = Map::new();
    match tool {
        "start_pane" => {
            put_capped(&mut out, "profile", args, "profile", SHORT);
            put_capped(&mut out, "cwd", args, "cwd", LONG);
            put_capped(&mut out, "pane", args, "name", SHORT);
            if let Some(env) = args.get("env").and_then(Value::as_object) {
                let mut names: Vec<String> = env.keys().map(|k| cap(k, 64)).collect();
                names.sort();
                names.truncate(32);
                out.insert("env_names".into(), json!(names));
            }
            if let Some(argv) = args.get("argv") {
                let program = argv
                    .as_array()
                    .and_then(|list| list.first())
                    .and_then(Value::as_str)
                    .map(|first| first.rsplit('/').next().unwrap_or(first))
                    .map(|name| cap(name, SHORT));
                out.insert("program".into(), json!(program));
                out.insert(
                    "argc".into(),
                    json!(argv.as_array().map(Vec::len).unwrap_or(0)),
                );
                let bytes = serde_json::to_vec(argv).unwrap_or_default();
                out.insert("argv_sha256".into(), json!(sha256_hex(&bytes)));
            }
            if let Some(line) = args.get("line") {
                let bytes = match line.as_str() {
                    Some(text) => text.as_bytes().to_vec(),
                    None => serde_json::to_vec(line).unwrap_or_default(),
                };
                put_digest(&mut out, "line", &bytes);
            }
        }
        "send_input" => {
            put_capped(&mut out, "pane", args, "terminal", SHORT);
            if let Some(text) = str_arg(args, "text") {
                put_digest(&mut out, "text", text.as_bytes());
            }
        }
        "read_output" => {
            put_capped(&mut out, "pane", args, "terminal", SHORT);
            if let Some(lines) = args.get("lines").and_then(Value::as_u64) {
                out.insert("lines".into(), json!(lines));
            }
        }
        "get_status" => put_capped(&mut out, "pane", args, "terminal", SHORT),
        "wait_status" | "interrupt_pane" | "remove_pane" => {
            put_capped(&mut out, "pane", args, "terminal", SHORT);
            if let Some(timeout) = args.get("timeout").filter(|v| v.is_number()) {
                out.insert("timeout".into(), timeout.clone());
            }
        }
        "list_panes" => {
            if let Some(all) = args.get("all").and_then(Value::as_bool) {
                out.insert("all".into(), json!(all));
            }
        }
        "answer_dialog" => {
            put_capped(&mut out, "pane", args, "terminal", SHORT);
            put_capped(&mut out, "choice", args, "choice", 32);
        }
        _ => {}
    }
    out
}

fn outcome_text(result: &Result<Value, ToolError>) -> String {
    match result {
        Ok(_) => "ok".to_string(),
        Err(error) => {
            let code = error
                .data
                .get("error")
                .and_then(Value::as_str)
                .filter(|code| {
                    !code.is_empty()
                        && code.len() <= 48
                        && code
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                })
                .unwrap_or("unknown");
            format!("error:{code}")
        }
    }
}

/// The allowlisted fields of a result: counts and digests, never content.
fn result_fields(tool: &str, result: &Result<Value, ToolError>) -> Map<String, Value> {
    let mut out = Map::new();
    match (tool, result) {
        ("start_pane", Ok(value)) => {
            if let Some(name) = value.get("name").and_then(Value::as_str) {
                out.insert("pane".into(), json!(cap(name, SHORT)));
            }
        }
        ("read_output", Ok(value)) => {
            let text = value.get("text").and_then(Value::as_str).unwrap_or("");
            out.insert("returned_lines".into(), json!(text.lines().count()));
            out.insert("returned_bytes".into(), json!(text.len()));
        }
        ("answer_dialog", Ok(value)) => {
            if let Some(selected) = value.get("selected").and_then(Value::as_str) {
                put_digest(&mut out, "highlight", selected.as_bytes());
            }
            if let Some(moves) = value.get("moves") {
                let count = moves
                    .as_u64()
                    .or_else(|| moves.as_array().map(|m| m.len() as u64));
                if let Some(count) = count {
                    out.insert("moves".into(), json!(count));
                }
            }
            if let Some(state) = value
                .get("keys_result")
                .and_then(|k| k.get("status"))
                .and_then(Value::as_str)
            {
                out.insert("status_after".into(), json!(cap(state, 16)));
            }
        }
        ("answer_dialog", Err(error)) => {
            // The screen the decision was taken on, as a digest.
            if let Some(screen) = error.data.get("screen").and_then(Value::as_str) {
                put_digest(&mut out, "screen", screen.as_bytes());
            }
        }
        _ => {}
    }
    out
}

struct Recovered {
    last_seq: u64,
    /// The file ended in the middle of a line.
    torn: bool,
}

/// Read the end of an existing file: the last sequence number and whether the
/// last line is torn. `cut` says the window starts inside the file, so its
/// first line may be partial and is not trusted.
fn recover(tail: &[u8], cut: bool) -> Recovered {
    let torn = tail.last().is_some_and(|byte| *byte != b'\n');
    let mut lines = tail.split(|byte| *byte == b'\n').collect::<Vec<_>>();
    if cut && !lines.is_empty() {
        lines.remove(0);
    }
    let last_seq = lines
        .iter()
        .rev()
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find_map(|value| value.get("seq").and_then(Value::as_u64))
        .unwrap_or(0);
    Recovered { last_seq, torn }
}

/// A sink that keeps lines, and one that always fails: for tests here and in
/// the modules that wire the audit in.
#[cfg(test)]
pub mod testing {
    use super::*;

    #[derive(Clone, Default)]
    pub struct MemorySink {
        pub lines: Arc<Mutex<Vec<String>>>,
    }

    impl MemorySink {
        pub fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }

        pub fn records(&self) -> Vec<Value> {
            self.lines()
                .iter()
                .map(|line| serde_json::from_str(line).expect("each line is JSON"))
                .collect()
        }

        pub fn bytes(&self) -> String {
            self.lines().join("")
        }
    }

    impl AuditSink for MemorySink {
        fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
            self.lines
                .lock()
                .unwrap()
                .push(String::from_utf8(line.to_vec()).expect("utf-8 line"));
            Ok(())
        }
    }

    pub struct FailingSink;

    impl AuditSink for FailingSink {
        fn write_line(&mut self, _line: &[u8]) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    pub fn fixed_clock(secs: u64) -> Clock {
        Arc::new(move || Duration::from_millis(secs * 1000 + 7))
    }

    pub fn memory_audit() -> (Audit, MemorySink) {
        let sink = MemorySink::default();
        (
            Audit::new(Box::new(sink.clone()), 0, false, fixed_clock(1_700_000_000)),
            sink,
        )
    }

    pub fn failing_audit() -> Audit {
        Audit::new(Box::new(FailingSink), 0, false, fixed_clock(1_700_000_000))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::testutil::{git_init, TempDir};

    fn args(value: Value) -> Args {
        value.as_object().unwrap().clone()
    }

    fn sample(tool: &str) -> Args {
        match tool {
            "start_pane" => args(json!({"profile": "ci", "cwd": "/work"})),
            "send_input" => args(json!({"terminal": "w", "text": "hello"})),
            "answer_dialog" => args(json!({"terminal": "w", "choice": "allow_once"})),
            "read_output" => args(json!({"terminal": "w", "lines": 50})),
            _ => args(json!({"terminal": "w"})),
        }
    }

    #[test]
    fn a_record_has_the_documented_shape() {
        let (audit, sink) = memory_audit();
        let pending = audit
            .before("alice", "send_input", &sample("send_input"))
            .unwrap();
        assert_eq!(pending.intent_seq, Some(1));
        let record = &sink.records()[0];
        assert_eq!(record["v"], 1);
        assert_eq!(record["seq"], 1);
        assert_eq!(record["ts"], 1_700_000_000u64);
        assert_eq!(record["ts_ms"], 1_700_000_000_007u64);
        assert_eq!(record["client"], "alice");
        assert_eq!(record["tool"], "send_input");
        assert_eq!(record["phase"], "call");
        assert_eq!(record["pane"], "w");
        assert_eq!(record["text_len"], 5);
        assert_eq!(record["text_sha256"], sha256_hex(b"hello"));
        assert!(record["prev"].is_null());
        assert!(sink.lines()[0].ends_with('\n'));
        assert!(record.get("recovered").is_none());
    }

    #[test]
    fn sequence_numbers_increase_and_failures_do_not_consume_one() {
        let (audit, sink) = memory_audit();
        for _ in 0..3 {
            audit
                .before("a", "remove_pane", &sample("remove_pane"))
                .unwrap();
        }
        let seqs: Vec<u64> = sink
            .records()
            .iter()
            .map(|r| r["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, [1, 2, 3]);
        let failing = Audit::new(Box::new(FailingSink), 41, false, fixed_clock(1));
        assert!(failing
            .before("a", "remove_pane", &sample("remove_pane"))
            .is_err());
        assert_eq!(failing.inner().seq, 41);
    }

    #[test]
    fn answer_dialog_and_start_pane_write_intent_then_outcome() {
        for tool in ["answer_dialog", "start_pane"] {
            let (audit, sink) = memory_audit();
            let a = sample(tool);
            let pending = audit.before("alice", tool, &a).unwrap();
            audit.after("alice", tool, &a, pending, &Ok(json!({"name": "p"})));
            let records = sink.records();
            assert_eq!(records.len(), 2, "{tool}");
            assert_eq!(records[0]["phase"], "intent");
            assert_eq!(records[1]["phase"], "outcome");
            assert_eq!(records[1]["intent_seq"], 1);
            assert_eq!(records[1]["outcome"], "ok");
        }
    }

    #[test]
    fn other_tools_write_one_line() {
        for tool in ["send_input", "remove_pane", "interrupt_pane"] {
            let (audit, sink) = memory_audit();
            let a = sample(tool);
            let pending = audit.before("a", tool, &a).unwrap();
            audit.after("a", tool, &a, pending, &Ok(json!({})));
            assert_eq!(sink.records().len(), 1, "{tool}");
            assert_eq!(sink.records()[0]["phase"], "call");
        }
        for tool in ["read_output", "get_status", "wait_status", "list_panes"] {
            let (audit, sink) = memory_audit();
            let a = sample(tool);
            let pending = audit.before("a", tool, &a).unwrap();
            assert_eq!(pending.intent_seq, None);
            assert!(sink.records().is_empty(), "{tool} writes nothing before");
            audit.after("a", tool, &a, pending, &Ok(json!({"text": "x\ny\n"})));
            assert_eq!(sink.records().len(), 1, "{tool}");
            assert_eq!(sink.records()[0]["phase"], "outcome");
        }
    }

    #[test]
    fn the_intent_line_is_written_before_the_backend_runs() {
        // One shared log: the sink and the pretend backend append to it.
        let log: Arc<Mutex<Vec<String>>> = Arc::default();
        struct Logging(Arc<Mutex<Vec<String>>>);
        impl AuditSink for Logging {
            fn write_line(&mut self, _: &[u8]) -> io::Result<()> {
                self.0.lock().unwrap().push("audit".into());
                Ok(())
            }
        }
        let audit = Audit::new(Box::new(Logging(log.clone())), 0, false, fixed_clock(1));
        for tool in ["start_pane", "send_input", "answer_dialog", "remove_pane"] {
            log.lock().unwrap().clear();
            let a = sample(tool);
            let pending = audit.before("a", tool, &a).unwrap();
            log.lock().unwrap().push("backend".into());
            audit.after("a", tool, &a, pending, &Ok(json!({})));
            let events = log.lock().unwrap().clone();
            assert_eq!(&events[..2], ["audit", "backend"], "{tool}");
        }
    }

    #[test]
    fn a_failing_sink_refuses_the_four_state_changing_tools() {
        for tool in ["start_pane", "send_input", "answer_dialog", "remove_pane"] {
            let audit = failing_audit();
            assert_eq!(policy(tool), Policy::FailClosed);
            let refused = audit.before("a", tool, &sample(tool)).unwrap_err();
            let error = refused.to_tool_error();
            assert_eq!(error.data["error"], UNAVAILABLE_CODE, "{tool}");
            assert_eq!(audit.dropped(), 1, "{tool}");
        }
    }

    #[test]
    fn a_failing_sink_lets_reads_and_interrupt_go_ahead() {
        for tool in [
            "interrupt_pane",
            "read_output",
            "get_status",
            "wait_status",
            "list_panes",
        ] {
            let audit = failing_audit();
            assert_eq!(policy(tool), Policy::FailOpen);
            let a = sample(tool);
            let pending = audit.before("a", tool, &a).expect(tool);
            assert_eq!(pending.intent_seq, None);
            audit.after("a", tool, &a, pending, &Ok(json!({"text": "x"})));
        }
        // interrupt_pane is attempted, so it counts; reads count after the call.
        let audit = failing_audit();
        audit
            .before("a", "interrupt_pane", &sample("interrupt_pane"))
            .unwrap();
        assert_eq!(audit.dropped(), 1);
        let audit = failing_audit();
        let pending = audit
            .before("a", "read_output", &sample("read_output"))
            .unwrap();
        audit.after(
            "a",
            "read_output",
            &sample("read_output"),
            pending,
            &Ok(json!({"text": "x"})),
        );
        assert_eq!(audit.dropped(), 1);
    }

    #[test]
    fn an_unknown_tool_fails_closed() {
        assert_eq!(policy("something_new"), Policy::FailClosed);
        let (audit, sink) = memory_audit();
        audit.before("a", "something_new", &Args::new()).unwrap();
        assert_eq!(sink.records()[0]["tool"], "unknown");
    }

    #[test]
    fn denials_are_recorded_with_the_reason() {
        let (audit, sink) = memory_audit();
        audit.denied(
            "ci-1",
            "answer_dialog",
            &sample("answer_dialog"),
            "answer_dialog needs the dialog scope",
        );
        let record = &sink.records()[0];
        assert_eq!(record["phase"], "denied");
        assert_eq!(record["outcome"], "denied");
        assert_eq!(record["client"], "ci-1");
        assert_eq!(record["reason"], "answer_dialog needs the dialog scope");
        // A failing sink never blocks a denial.
        failing_audit().denied("a", "remove_pane", &sample("remove_pane"), "x");
    }

    #[test]
    fn errors_record_the_code_only() {
        let (audit, sink) = memory_audit();
        let a = sample("start_pane");
        let pending = audit.before("a", "start_pane", &a).unwrap();
        let error = ToolError {
            text: "secret-in-message".into(),
            data: json!({"error": "not_found", "message": "secret-in-message"}),
        };
        audit.after("a", "start_pane", &a, pending, &Err(error));
        let record = &sink.records()[1];
        assert_eq!(record["outcome"], "error:not_found");
        assert!(!sink.bytes().contains("secret-in-message"));
        let odd = ToolError {
            text: String::new(),
            data: json!({"error": "Bad Code!"}),
        };
        assert_eq!(outcome_text(&Err(odd)), "error:unknown");
    }

    #[test]
    fn auth_failures_are_recorded_capped_and_rate_limited() {
        let (audit, sink) = memory_audit();
        audit.auth_failure(&"a".repeat(40), "unknown_id");
        let record = &sink.records()[0];
        assert_eq!(record["phase"], "auth");
        assert_eq!(
            record["id_prefix"].as_str().unwrap().len(),
            crate::clients::ID_PREFIX_LEN
        );
        assert!(record["client"].is_null());
        audit.auth_failure("Bad\"Id\n", "bad_secret");
        assert_eq!(sink.records()[1]["id_prefix"], "add");
        for _ in 0..50 {
            audit.auth_failure("x", "bad_secret");
        }
        // Two written above, then the limit stops it at 20 for this second.
        assert_eq!(sink.records().len(), AUTH_FAILURES_PER_SECOND as usize);
        // The next second reports how many were left out.
        let later = Audit::new(Box::new(MemorySink::default()), 0, false, fixed_clock(1));
        later.auth_window.lock().unwrap().suppressed = 3;
        later.auth_failure("x", "bad_secret");
        assert_eq!(later.auth_window.lock().unwrap().suppressed, 0);
    }

    #[test]
    fn suppressed_count_lands_on_the_next_record() {
        use std::sync::atomic::AtomicU64;
        let now = Arc::new(AtomicU64::new(10));
        let tick = now.clone();
        let clock: Clock = Arc::new(move || Duration::from_secs(tick.load(Ordering::Relaxed)));
        let sink = MemorySink::default();
        let audit = Audit::new(Box::new(sink.clone()), 0, false, clock);
        for _ in 0..25 {
            audit.auth_failure("x", "bad_secret");
        }
        assert_eq!(sink.records().len(), 20);
        now.store(11, Ordering::Relaxed);
        audit.auth_failure("x", "bad_secret");
        let last = sink.records().pop().unwrap();
        assert_eq!(last["suppressed"], 5);
    }

    #[test]
    fn the_stderr_warning_is_rate_limited() {
        let (audit, _) = memory_audit();
        assert!(audit.warn_due(100));
        assert!(!audit.warn_due(100));
        assert!(!audit.warn_due(159));
        assert!(audit.warn_due(160));
    }

    #[test]
    fn the_watchdog_is_an_actor_like_any_other() {
        let (audit, sink) = memory_audit();
        audit
            .before(WATCHDOG_ACTOR, "send_input", &sample("send_input"))
            .unwrap();
        assert_eq!(sink.records()[0]["client"], "system:watchdog");
    }

    // ---- redaction ----

    const CANARY: &str = "CANARY-9f3c1e";

    #[test]
    fn canaries_never_reach_the_audit_bytes() {
        let (audit, sink) = memory_audit();
        let calls: Vec<(&str, Value)> = vec![
            (
                "send_input",
                json!({"terminal": "w", "text": format!("password {CANARY}")}),
            ),
            (
                "start_pane",
                json!({"argv": ["/bin/tool".to_string(), format!("--key={CANARY}")], "env": {"API_KEY": CANARY}}),
            ),
            (
                "start_pane",
                json!({"line": format!("echo {CANARY}"), "env": {"TOKEN": CANARY}}),
            ),
            (
                "read_output",
                json!({"terminal": "w", "authorization": format!("Bearer {CANARY}"), "body": CANARY}),
            ),
            (
                "answer_dialog",
                json!({"terminal": "w", "choice": "allow_once", "token": CANARY}),
            ),
            ("get_status", json!({"terminal": "w", "password": CANARY})),
        ];
        for (tool, body) in &calls {
            let a = args(body.clone());
            let pending = audit.before("alice", tool, &a).unwrap();
            let result = match *tool {
                "read_output" => {
                    Ok(json!({"text": format!("screen {CANARY}\nmore\n"), "complete": true}))
                }
                "answer_dialog" => Ok(json!({
                    "selected": format!("Allow {CANARY}"),
                    "title": CANARY, "command": format!("curl {CANARY}"),
                    "options": [CANARY], "moves": 2, "keys_result": {"status": "working", "turn": 1},
                })),
                _ => Ok(json!({"name": "p", "argv": [CANARY], "cwd": CANARY})),
            };
            audit.after("alice", tool, &a, pending, &result);
            audit.denied("alice", tool, &a, "needs a scope");
        }
        // An answer_dialog refusal carries the screen it looked at.
        let a = args(json!({"terminal": "w", "choice": "allow_once"}));
        let pending = audit.before("alice", "answer_dialog", &a).unwrap();
        let refusal = ToolError {
            text: format!("refused {CANARY}"),
            data: json!({"error": "no_dialog", "message": CANARY, "screen": format!("screen with {CANARY}")}),
        };
        audit.after("alice", "answer_dialog", &a, pending, &Err(refusal));
        let bytes = sink.bytes();
        assert!(
            !bytes.contains(CANARY),
            "a canary reached the audit bytes:\n{bytes}"
        );
        // What may appear: names of env variables, the program name, digests.
        assert!(bytes.contains("API_KEY") && bytes.contains("TOKEN"));
        assert!(bytes.contains("argv_sha256") && bytes.contains("screen_sha256"));
    }

    #[test]
    fn tokens_and_hashes_are_not_fields_of_any_record() {
        let (audit, sink) = memory_audit();
        let hostile = args(json!({
            "terminal": "w", "text": "x",
            "token": "osh_x_y", "token_sha256": "ab", "Authorization": "Bearer q",
        }));
        audit.before("a", "send_input", &hostile).unwrap();
        audit.denied("a", "send_input", &hostile, "r");
        let bytes = sink.bytes();
        for forbidden in ["osh_", "token", "Authorization", "Bearer"] {
            assert!(!bytes.contains(forbidden), "{forbidden}");
        }
    }

    #[test]
    fn fields_are_capped() {
        let (audit, sink) = memory_audit();
        let long = "x".repeat(10_000);
        audit
            .before(
                "a",
                "start_pane",
                &args(json!({"profile": long, "cwd": long, "name": long})),
            )
            .unwrap();
        let record = &sink.records()[0];
        assert_eq!(record["profile"].as_str().unwrap().len(), SHORT);
        assert_eq!(record["cwd"].as_str().unwrap().len(), LONG);
        assert!(sink.lines()[0].len() < 2_000);
    }

    #[test]
    fn start_pane_argv_and_line_are_digests() {
        let (audit, sink) = memory_audit();
        audit
            .before(
                "a",
                "start_pane",
                &args(json!({"argv": ["/usr/bin/ls", "-la"]})),
            )
            .unwrap();
        audit
            .before("a", "start_pane", &args(json!({"line": "ls -la"})))
            .unwrap();
        let records = sink.records();
        assert_eq!(records[0]["program"], "ls");
        assert_eq!(records[0]["argc"], 2);
        assert_eq!(
            records[0]["argv_sha256"],
            sha256_hex(br#"["/usr/bin/ls","-la"]"#)
        );
        assert_eq!(records[1]["line_len"], 6);
        assert!(records[1].get("line").is_none());
    }

    #[test]
    fn read_output_records_counts_not_content() {
        let (audit, sink) = memory_audit();
        let a = sample("read_output");
        let pending = audit.before("a", "read_output", &a).unwrap();
        audit.after(
            "a",
            "read_output",
            &a,
            pending,
            &Ok(json!({"text": "one\ntwo\n"})),
        );
        let record = &sink.records()[0];
        assert_eq!(record["returned_lines"], 2);
        assert_eq!(record["returned_bytes"], 8);
        assert_eq!(record["lines"], 50);
    }

    #[test]
    fn every_tool_builds_a_record() {
        let (audit, sink) = memory_audit();
        for tool in TOOL_NAMES {
            let a = sample(tool);
            let pending = audit.before("a", tool, &a).unwrap();
            audit.after("a", tool, &a, pending, &Ok(json!({})));
        }
        assert!(sink
            .records()
            .iter()
            .all(|r| TOOL_NAMES.contains(&r["tool"].as_str().unwrap())));
    }

    // ---- concurrency ----

    #[test]
    fn concurrent_records_are_whole_lines_with_unique_numbers() {
        let (audit, sink) = memory_audit();
        let audit = Arc::new(audit);
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let audit = audit.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        audit
                            .before(&format!("c{i}"), "send_input", &sample("send_input"))
                            .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let mut seqs: Vec<u64> = sink
            .records()
            .iter()
            .map(|r| r["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(
            seqs,
            (1..=400).collect::<Vec<u64>>(),
            "written in numbered order"
        );
        seqs.dedup();
        assert_eq!(seqs.len(), 400);
    }

    // ---- sinks and recovery ----

    #[test]
    fn a_failed_write_makes_the_next_one_start_a_fresh_line() {
        struct Flaky {
            fail_next: bool,
            out: Arc<Mutex<Vec<u8>>>,
        }
        impl Write for Flaky {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.fail_next {
                    self.fail_next = false;
                    self.out.lock().unwrap().extend_from_slice(&buf[..3]);
                    return Err(io::Error::other("torn"));
                }
                self.out.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let out: Arc<Mutex<Vec<u8>>> = Arc::default();
        let writer = Flaky {
            fail_next: true,
            out: out.clone(),
        };
        let audit = Audit::new(
            Box::new(WriteSink::new(writer, false)),
            0,
            false,
            fixed_clock(1),
        );
        assert!(audit
            .before("a", "remove_pane", &sample("remove_pane"))
            .is_err());
        audit
            .before("a", "remove_pane", &sample("remove_pane"))
            .unwrap();
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            serde_json::from_str::<Value>(lines[0]).is_err(),
            "torn line"
        );
        assert_eq!(serde_json::from_str::<Value>(lines[1]).unwrap()["seq"], 1);
    }

    fn private_dir(tag: &str) -> TempDir {
        let dir = TempDir::new(tag);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn open(dir: &TempDir, project: &TempDir) -> Result<Audit, ConfigError> {
        Audit::open_file(
            &dir.path().join("audit.jsonl"),
            project.path(),
            fixed_clock(5),
        )
    }

    fn file_lines(dir: &TempDir) -> Vec<String> {
        fs::read_to_string(dir.path().join("audit.jsonl"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_file_is_created_0600_and_records_are_appended() {
        let dir = private_dir("audit-file");
        let project = TempDir::new("audit-project");
        let audit = open(&dir, &project).unwrap();
        let mode = fs::metadata(dir.path().join("audit.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        audit
            .before("a", "send_input", &sample("send_input"))
            .unwrap();
        audit
            .before("a", "send_input", &sample("send_input"))
            .unwrap();
        drop(audit);
        let lines = file_lines(&dir);
        assert_eq!(lines.len(), 2);
        // A second open continues the numbering.
        let again = open(&dir, &project).unwrap();
        again
            .before("a", "send_input", &sample("send_input"))
            .unwrap();
        let last: Value = serde_json::from_str(file_lines(&dir).last().unwrap()).unwrap();
        assert_eq!(last["seq"], 3);
        assert!(last.get("recovered").is_none());
    }

    #[test]
    fn a_torn_last_line_is_ended_and_the_next_record_is_marked() {
        let dir = private_dir("audit-torn");
        let project = TempDir::new("audit-project");
        let path = dir.path().join("audit.jsonl");
        let first = r#"{"seq":7,"v":1}"#;
        fs::write(&path, format!("{first}\n{{\"seq\":8,\"v\"")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let audit = open(&dir, &project).unwrap();
        audit
            .before("a", "send_input", &sample("send_input"))
            .unwrap();
        audit
            .before("a", "send_input", &sample("send_input"))
            .unwrap();
        let lines = file_lines(&dir);
        assert_eq!(lines.len(), 4);
        assert!(serde_json::from_str::<Value>(&lines[1]).is_err());
        let a: Value = serde_json::from_str(&lines[2]).unwrap();
        let b: Value = serde_json::from_str(&lines[3]).unwrap();
        assert_eq!(
            (a["seq"].clone(), a["recovered"].clone()),
            (json!(8), json!(true))
        );
        assert_eq!(b["seq"], 9);
        assert!(b.get("recovered").is_none());
    }

    #[test]
    fn a_clean_tail_needs_no_recovery() {
        let state = recover(b"{\"seq\":4}\n", false);
        assert_eq!((state.last_seq, state.torn), (4, false));
        let state = recover(b"", false);
        assert_eq!((state.last_seq, state.torn), (0, false));
        let state = recover(b"{\"seq\":4}\n{\"se", false);
        assert_eq!((state.last_seq, state.torn), (4, true));
        // A cut window distrusts its first, possibly partial, line.
        let state = recover(b"\"seq\":99}\n{\"seq\":5}\n", true);
        assert_eq!(state.last_seq, 5);
        let state = recover(b"\"seq\":99}\n", true);
        assert_eq!(state.last_seq, 0);
    }

    #[test]
    fn a_loose_file_or_directory_is_refused() {
        let project = TempDir::new("audit-project");
        let loose_dir = TempDir::new("audit-loose-dir");
        fs::set_permissions(loose_dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(open(&loose_dir, &project)
            .unwrap_err()
            .0
            .contains("chmod 700"));
        fs::set_permissions(loose_dir.path(), fs::Permissions::from_mode(0o770)).unwrap();
        assert!(open(&loose_dir, &project).is_err());

        let dir = private_dir("audit-loose-file");
        let path = dir.path().join("audit.jsonl");
        for mode in [0o644, 0o640, 0o602] {
            fs::write(&path, "").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                open(&dir, &project).unwrap_err().0.contains("chmod 600"),
                "{mode:o}"
            );
        }
    }

    #[test]
    fn the_directory_must_exist() {
        let project = TempDir::new("audit-project");
        let dir = private_dir("audit-missing");
        let path = dir.path().join("nope").join("audit.jsonl");
        assert!(Audit::open_file(&path, project.path(), fixed_clock(1)).is_err());
        assert!(Audit::open_file(Path::new("/"), project.path(), fixed_clock(1)).is_err());
    }

    #[test]
    fn a_file_or_directory_of_someone_else_is_refused() {
        let dir = private_dir("audit-owner");
        let project = TempDir::new("audit-project");
        let me = fs::metadata(dir.path()).unwrap().uid();
        let path = dir.path().join("audit.jsonl");
        let refused =
            Audit::open_file_as(&path, project.path(), fixed_clock(1), me.wrapping_add(1));
        assert!(refused.unwrap_err().0.contains("owned by"));
        assert!(Audit::open_file_as(&path, project.path(), fixed_clock(1), me).is_ok());
    }

    #[test]
    fn a_path_inside_a_git_repo_is_refused_and_nothing_is_created() {
        let repo = private_dir("audit-repo");
        git_init(repo.path());
        let project = TempDir::new("audit-project");
        let message = open(&repo, &project).unwrap_err().0;
        assert!(message.contains("outside the git repository"), "{message}");
        assert!(!repo.path().join("audit.jsonl").exists());
    }

    #[test]
    fn a_symlinked_audit_file_is_refused() {
        let dir = private_dir("audit-link");
        let project = TempDir::new("audit-project");
        let target = dir.path().join("real");
        fs::write(&target, "").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("audit.jsonl")).unwrap();
        assert!(open(&dir, &project).is_err());
    }
}

/// Issue #4, PR 1: the file sink as an operator meets it across restarts.
/// Run with `cargo test issue4_pr1`.
#[cfg(test)]
mod issue4_pr1_audit {
    use super::testing::fixed_clock;
    use super::*;
    use crate::testutil::TempDir;

    fn private_dir(tag: &str) -> TempDir {
        let dir = TempDir::new(tag);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn args() -> Args {
        json!({"terminal": "w", "text": "x"})
            .as_object()
            .unwrap()
            .clone()
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_numbers_and_the_modes_survive_a_restart_and_a_torn_tail() {
        let dir = private_dir("i4-audit-restart");
        let project = TempDir::new("i4-audit-restart-project");
        let path = dir.path().join("audit.jsonl");
        {
            let audit = Audit::open_file(&path, project.path(), fixed_clock(10)).unwrap();
            for _ in 0..2 {
                audit.before("a", "send_input", &args()).unwrap();
            }
        }
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(dir.path()), 0o700);
        // A crash in the middle of the third record, inside a multi-byte char.
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"{\"seq\":3,\"note\":\"\xe2\x82");
        fs::write(&path, &bytes).unwrap();
        let audit = Audit::open_file(&path, project.path(), fixed_clock(11)).unwrap();
        audit.before("a", "send_input", &args()).unwrap();
        audit.before("a", "send_input", &args()).unwrap();
        drop(audit);
        let bytes = fs::read(&path).unwrap();
        let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
        // Two clean records, the torn line, two new records, and the final newline.
        assert_eq!(lines.len(), 6, "{}", String::from_utf8_lossy(&bytes));
        assert!(lines[5].is_empty(), "the file ends with a newline");
        assert!(serde_json::from_slice::<Value>(lines[2]).is_err());
        let third: Value = serde_json::from_slice(lines[3]).unwrap();
        let fourth: Value = serde_json::from_slice(lines[4]).unwrap();
        assert_eq!(
            third["seq"], 3,
            "numbering continues from the last whole record"
        );
        assert_eq!(third["recovered"], true);
        assert_eq!(fourth["seq"], 4);
        assert!(fourth.get("recovered").is_none());
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_file_of_the_wrong_mode_is_refused_and_not_changed() {
        let dir = private_dir("i4-audit-mode");
        let project = TempDir::new("i4-audit-mode-project");
        let path = dir.path().join("audit.jsonl");
        for loose in [0o644, 0o640, 0o602] {
            fs::write(&path, "").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(loose)).unwrap();
            let err = Audit::open_file(&path, project.path(), fixed_clock(1)).unwrap_err();
            assert!(err.0.contains("chmod 600"), "{loose:o}: {err}");
            assert_eq!(
                mode(&path),
                loose,
                "the file is left as the operator set it"
            );
        }
    }
}
