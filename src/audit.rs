//! The audit file: who did what, and what came of it.
//!
//! JSON Lines, append-only, one `write` per record under a mutex, file mode
//! 0600 in a 0700 directory outside every git repository. A record names the
//! client, the tool, the pane, the profile or the choice, and for `send_input`
//! the length of the text. Typed text, screens and environment values are
//! never recorded. Tokens, token hashes, `Authorization` and request bodies
//! are never recorded either. The audit file is accountability for remote
//! clients, not tamper-proofing.
//!
//! Shape of a call: each mutating tool (`start_pane`, `send_input`,
//! `answer_dialog`, `remove_pane`, `interrupt_pane`, `draft_clear`) writes a
//! `call` line before it runs and a best-effort `outcome` line after.
//! `read_output` writes an `outcome` line after. `list_panes`, `get_status` and `wait_status` write
//! nothing. A call refused by the scope check writes a `denied` line.
//! Failed authentications go to stderr, not to this file.
//!
//! Failure policy: `start_pane`, `send_input`, `answer_dialog`, `remove_pane`
//! and `draft_clear` are refused with `audit_unavailable` when the `call`
//! record cannot be written, so a failing sink means no backend call for them.
//! `interrupt_pane` (the human's way to stop a runaway agent) and the reads go
//! ahead when the write fails; the failure is counted and reported on stderr
//! at most once a minute.

use crate::config::{check_outside_repo, ConfigError};
use crate::tools::{Args, ToolError, TOOL_NAMES};
use serde_json::{json, Map, Value};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// The path of the audit file.
pub const AUDIT_FILE_ENV: &str = "OPENAB_SHEPHERD_AUDIT_FILE";
/// The actor of the routine's nudges.
pub const ROUTINE_ACTOR: &str = "system:routine";
/// The tool-error code of a refused call whose audit record could not be written.
pub const UNAVAILABLE_CODE: &str = "audit_unavailable";
const WARN_INTERVAL_SECS: u64 = 60;
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

impl AuditSink for fs::File {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        // One buffer, so one write call on an O_APPEND file.
        self.write_all(line)
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

/// Tools that write a `call` line before they run.
fn writes_call(tool: &str) -> bool {
    matches!(
        tool,
        "start_pane"
            | "send_input"
            | "answer_dialog"
            | "remove_pane"
            | "interrupt_pane"
            | "draft_clear"
    )
}

/// Tools that write an `outcome` line after they ran.
fn writes_outcome(tool: &str) -> bool {
    writes_call(tool) || tool == "read_output"
}

/// The recorder: stamps records and hands them to the sink.
pub struct Audit {
    sink: Mutex<Box<dyn AuditSink>>,
    clock: Clock,
    dropped: AtomicU64,
    last_warn: Mutex<Option<u64>>,
}

impl fmt::Debug for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Audit")
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

impl Audit {
    pub fn new(sink: Box<dyn AuditSink>, clock: Clock) -> Self {
        Self {
            sink: Mutex::new(sink),
            clock,
            dropped: AtomicU64::new(0),
            last_warn: Mutex::new(None),
        }
    }

    /// Open the audit file. The directory must exist and have no group or
    /// other bits (chmod 700); the file is created 0600, or must already be a
    /// regular file with no group or other bits. The path must be outside the
    /// project folder and every git repo.
    pub fn open_file(path: &Path, project_dir: &Path, clock: Clock) -> Result<Self, ConfigError> {
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
        if dir_meta.permissions().mode() & 0o077 != 0 {
            return Err(bad(
                "audit file: its directory must not be accessible to group or others (chmod 700)"
                    .into(),
            ));
        }
        let full = dir.join(name);
        check_outside_repo(&full, project_dir, "audit file")?;
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&full)
            .map_err(|err| bad(format!("audit file: cannot open it: {err}")))?;
        let meta = file
            .metadata()
            .map_err(|err| bad(format!("audit file: cannot read it: {err}")))?;
        if !meta.is_file() {
            return Err(bad("audit file must be a regular file".into()));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(bad(
                "audit file must not be readable or writable by group or others (chmod 600)".into(),
            ));
        }
        Ok(Self::new(Box::new(file), clock))
    }

    fn now(&self) -> Duration {
        (self.clock)()
    }

    /// Records that could not be written so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Stamp and write one record.
    fn emit(&self, mut record: Map<String, Value>) -> Result<(), AuditError> {
        let now = self.now();
        record.insert("ts".into(), json!(now.as_secs()));
        record.insert("ts_ms".into(), json!(now.as_millis() as u64));
        let mut line = serde_json::to_vec(&Value::Object(record))
            .map_err(|err| AuditError(format!("cannot serialize a record: {err}")))?;
        line.push(b'\n');
        self.sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_line(&line)
            .map_err(|err| AuditError(format!("audit write failed: {err}")))
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

    fn emit_or_count(&self, record: Map<String, Value>) {
        if self.emit(record).is_err() {
            self.note_failure();
        }
    }

    /// Write the `call` record that precedes a mutating tool. A fail-closed
    /// tool whose record cannot be written gets `Err`, and the caller must
    /// not run it. Other tools write nothing here.
    pub fn before(&self, actor: &str, tool: &str, args: &Args) -> Result<(), AuditUnavailable> {
        if !writes_call(tool) {
            return Ok(());
        }
        let mut record = base(actor, tool, "call");
        record.extend(request_fields(actor, tool, args));
        if self.emit(record).is_err() {
            self.note_failure();
            if policy(tool) == Policy::FailClosed {
                return Err(AuditUnavailable);
            }
        }
        Ok(())
    }

    /// Write the `outcome` record that follows a call, for the mutating tools
    /// and `read_output`. The call is done by now, so a write failure is only
    /// counted.
    pub fn after(&self, actor: &str, tool: &str, args: &Args, result: &Result<Value, ToolError>) {
        if !writes_outcome(tool) {
            return;
        }
        let mut record = base(actor, tool, "outcome");
        record.extend(request_fields(actor, tool, args));
        if tool == "start_pane" {
            if let Some(name) = result_name(result) {
                record.insert("pane".into(), json!(cap(&name, SHORT)));
            }
        }
        record.insert("outcome".into(), json!(outcome_text(result)));
        self.emit_or_count(record);
    }

    /// Record a call the scope check refused. Always attempted; never blocks.
    pub fn denied(&self, actor: &str, tool: &str, args: &Args, reason: &str) {
        let mut record = base(actor, tool, "denied");
        record.extend(request_fields(actor, tool, args));
        record.insert("outcome".into(), json!("denied"));
        record.insert("reason".into(), json!(cap(reason, LONG)));
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

/// The allowlisted fields of a request. Anything not named here is dropped.
fn request_fields(actor: &str, tool: &str, args: &Args) -> Map<String, Value> {
    let mut out = Map::new();
    match tool {
        "start_pane" => {
            put_capped(&mut out, "profile", args, "profile", SHORT);
            put_capped(&mut out, "pane", args, "name", SHORT);
        }
        "send_input" => {
            put_capped(&mut out, "pane", args, "terminal", SHORT);
            if let Some(text) = str_arg(args, "text") {
                out.insert("text_len".into(), json!(text.len()));
            }
            // Only the routine names itself: a client's own `routine` or
            // `created_by` argument is never copied into the record.
            if actor == ROUTINE_ACTOR {
                put_capped(&mut out, "routine", args, "routine", SHORT);
                put_capped(&mut out, "created_by", args, "created_by", SHORT);
            }
        }
        "answer_dialog" => {
            put_capped(&mut out, "pane", args, "terminal", SHORT);
            put_capped(&mut out, "choice", args, "choice", 32);
        }
        "read_output" | "interrupt_pane" | "remove_pane" | "draft_clear" => {
            put_capped(&mut out, "pane", args, "terminal", SHORT);
        }
        _ => {}
    }
    out
}

/// The pane name a successful `start_pane` chose.
fn result_name(result: &Result<Value, ToolError>) -> Option<String> {
    result
        .as_ref()
        .ok()?
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
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
            Audit::new(Box::new(sink.clone()), fixed_clock(1_700_000_000)),
            sink,
        )
    }

    pub fn failing_audit() -> Audit {
        Audit::new(Box::new(FailingSink), fixed_clock(1_700_000_000))
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
        audit
            .before("alice", "send_input", &sample("send_input"))
            .unwrap();
        let record = &sink.records()[0];
        assert_eq!(record["ts"], 1_700_000_000u64);
        assert_eq!(record["ts_ms"], 1_700_000_000_007u64);
        assert_eq!(record["client"], "alice");
        assert_eq!(record["tool"], "send_input");
        assert_eq!(record["phase"], "call");
        assert_eq!(record["pane"], "w");
        assert_eq!(record["text_len"], 5);
        for gone in ["v", "seq", "prev", "recovered", "text_sha256"] {
            assert!(record.get(gone).is_none(), "{gone}");
        }
        assert!(sink.lines()[0].ends_with('\n'));
    }

    #[test]
    fn mutating_tools_write_a_call_line_then_an_outcome_line() {
        for tool in [
            "start_pane",
            "send_input",
            "answer_dialog",
            "remove_pane",
            "interrupt_pane",
            "draft_clear",
        ] {
            let (audit, sink) = memory_audit();
            let a = sample(tool);
            audit.before("alice", tool, &a).unwrap();
            audit.after("alice", tool, &a, &Ok(json!({"name": "p"})));
            let records = sink.records();
            assert_eq!(records.len(), 2, "{tool}");
            assert_eq!(records[0]["phase"], "call", "{tool}");
            assert_eq!(records[1]["phase"], "outcome", "{tool}");
            assert_eq!(records[1]["outcome"], "ok", "{tool}");
        }
    }

    #[test]
    fn read_output_writes_an_outcome_line_and_the_other_reads_nothing() {
        let (audit, sink) = memory_audit();
        let a = sample("read_output");
        audit.before("a", "read_output", &a).unwrap();
        assert!(sink.records().is_empty(), "no line before a read");
        audit.after("a", "read_output", &a, &Ok(json!({"text": "x\ny\n"})));
        let records = sink.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["phase"], "outcome");
        assert_eq!(records[0]["pane"], "w");
        assert!(
            !sink.bytes().contains("x\\ny"),
            "the screen is not recorded"
        );
        for tool in ["list_panes", "get_status", "wait_status"] {
            let (audit, sink) = memory_audit();
            let a = sample(tool);
            audit.before("a", tool, &a).unwrap();
            audit.after("a", tool, &a, &Ok(json!({})));
            assert!(sink.records().is_empty(), "{tool}");
        }
    }

    #[test]
    fn start_pane_outcome_names_the_pane_it_chose() {
        let (audit, sink) = memory_audit();
        let a = sample("start_pane");
        audit.after("a", "start_pane", &a, &Ok(json!({"name": "ci-1"})));
        assert_eq!(sink.records()[0]["pane"], "ci-1");
        assert_eq!(sink.records()[0]["profile"], "ci");
    }

    #[test]
    fn a_failing_sink_refuses_start_pane() {
        assert_fail_closed("start_pane");
    }

    #[test]
    fn a_failing_sink_refuses_send_input() {
        assert_fail_closed("send_input");
    }

    #[test]
    fn a_failing_sink_refuses_answer_dialog() {
        assert_fail_closed("answer_dialog");
    }

    #[test]
    fn a_failing_sink_refuses_remove_pane() {
        assert_fail_closed("remove_pane");
    }

    #[test]
    fn a_failing_sink_refuses_draft_clear() {
        assert_fail_closed("draft_clear");
    }

    #[test]
    fn draft_clear_records_its_pane_and_nothing_else_of_the_request() {
        let (audit, sink) = memory_audit();
        let a = args(json!({"terminal": "w", "extra": "dropped"}));
        audit.before("alice", "draft_clear", &a).unwrap();
        let record = &sink.records()[0];
        assert_eq!(record["tool"], "draft_clear");
        assert_eq!(record["pane"], "w");
        assert!(record.get("extra").is_none());
    }

    fn assert_fail_closed(tool: &str) {
        let audit = failing_audit();
        assert_eq!(policy(tool), Policy::FailClosed);
        let refused = audit.before("a", tool, &sample(tool)).unwrap_err();
        let error = refused.to_tool_error();
        assert_eq!(error.data["error"], UNAVAILABLE_CODE, "{tool}");
        assert_eq!(audit.dropped(), 1, "{tool}");
    }

    #[test]
    fn a_failing_sink_lets_interrupt_pane_go_ahead() {
        let audit = failing_audit();
        assert_eq!(policy("interrupt_pane"), Policy::FailOpen);
        audit
            .before("a", "interrupt_pane", &sample("interrupt_pane"))
            .unwrap();
        audit.after(
            "a",
            "interrupt_pane",
            &sample("interrupt_pane"),
            &Ok(json!({})),
        );
        assert_eq!(audit.dropped(), 2, "both lines are attempted and counted");
    }

    #[test]
    fn a_failing_sink_lets_reads_go_ahead() {
        for tool in ["read_output", "get_status", "wait_status", "list_panes"] {
            let audit = failing_audit();
            assert_eq!(policy(tool), Policy::FailOpen);
            let a = sample(tool);
            audit.before("a", tool, &a).expect(tool);
            audit.after("a", tool, &a, &Ok(json!({"text": "x"})));
        }
    }

    #[test]
    fn an_unknown_tool_fails_closed() {
        assert_eq!(policy("something_new"), Policy::FailClosed);
        let (audit, sink) = memory_audit();
        audit.denied("a", "something_new", &Args::new(), "x");
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
        let error = ToolError {
            text: "secret-in-message".into(),
            data: json!({"error": "not_found", "message": "secret-in-message"}),
        };
        audit.after("a", "start_pane", &a, &Err(error));
        let record = &sink.records()[0];
        assert_eq!(record["outcome"], "error:not_found");
        assert!(!sink.bytes().contains("secret-in-message"));
        let odd = ToolError {
            text: String::new(),
            data: json!({"error": "Bad Code!"}),
        };
        assert_eq!(outcome_text(&Err(odd)), "error:unknown");
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
    fn the_routine_is_an_actor_like_any_other() {
        let (audit, sink) = memory_audit();
        audit
            .before(ROUTINE_ACTOR, "send_input", &sample("send_input"))
            .unwrap();
        assert_eq!(sink.records()[0]["client"], "system:routine");
    }

    /// Only the routine names a routine: a client's own `routine` or
    /// `created_by` argument on `send_input` is not copied into the record.
    #[test]
    fn a_client_cannot_name_a_routine_in_its_own_record() {
        let (audit, sink) = memory_audit();
        let forged = args(json!({"terminal": "w", "text": "hi",
            "routine": "profile", "created_by": "profile"}));
        audit.before("alice", "send_input", &forged).unwrap();
        audit.before(ROUTINE_ACTOR, "send_input", &forged).unwrap();
        let records = sink.records();
        assert!(records[0].get("routine").is_none(), "{:?}", records[0]);
        assert!(records[0].get("created_by").is_none(), "{:?}", records[0]);
        assert_eq!(records[1]["routine"], "profile");
        assert_eq!(records[1]["created_by"], "profile");
    }

    const CANARY: &str = "CANARY-9f3c1e";

    #[test]
    fn content_never_reaches_the_audit_bytes() {
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
        ];
        for (tool, body) in &calls {
            let a = args(body.clone());
            audit.before("alice", tool, &a).unwrap();
            let result = Ok(json!({
                "text": format!("screen {CANARY}\n"), "selected": CANARY,
                "name": "p", "argv": [CANARY], "cwd": CANARY,
            }));
            audit.after("alice", tool, &a, &result);
            audit.denied("alice", tool, &a, "needs a scope");
        }
        let bytes = sink.bytes();
        assert!(
            !bytes.contains(CANARY),
            "a canary reached the audit bytes:\n{bytes}"
        );
        for forbidden in ["osh_", "token", "Authorization", "Bearer", "API_KEY"] {
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
        assert_eq!(record["pane"].as_str().unwrap().len(), SHORT);
        assert!(sink.lines()[0].len() < 2_000);
    }

    #[test]
    fn every_tool_builds_a_record() {
        let (audit, sink) = memory_audit();
        for tool in TOOL_NAMES {
            let a = sample(tool);
            audit.before("a", tool, &a).unwrap();
            audit.after("a", tool, &a, &Ok(json!({})));
        }
        assert!(sink
            .records()
            .iter()
            .all(|r| TOOL_NAMES.contains(&r["tool"].as_str().unwrap())));
    }

    #[test]
    fn concurrent_records_are_whole_lines() {
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
        assert_eq!(sink.records().len(), 400);
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
        drop(audit);
        // A second open appends.
        let again = open(&dir, &project).unwrap();
        again
            .before("a", "send_input", &sample("send_input"))
            .unwrap();
        assert_eq!(file_lines(&dir).len(), 2);
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
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                mode,
                "the file is left as the operator set it"
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
    fn a_path_inside_a_git_repo_is_refused_and_nothing_is_created() {
        let repo = private_dir("audit-repo");
        git_init(repo.path());
        let project = TempDir::new("audit-project");
        let message = open(&repo, &project).unwrap_err().0;
        assert!(message.contains("outside the git repository"), "{message}");
        assert!(!repo.path().join("audit.jsonl").exists());
    }
}
