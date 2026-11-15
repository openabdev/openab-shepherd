//! The holder side of the link (issue #76): serves one front at a time over
//! two socketpairs.
//!
//! **Why two sockets.** The ADR's link is one anonymous socketpair. The
//! front's `write_guarded` holds a pane's `state` lock across a `write` and
//! waits for its reply, while the thread that would read that reply on a
//! shared stream may be inside `ingest`, waiting for the same `state`. So
//! requests and replies (`ops`) travel apart from the holder's events
//! (`events`), and the front reads replies on a router thread that takes no
//! pane lock. This is the one place the implementation departs from the
//! ADR's wording; it is one `Holder::serve` call over a pair of
//! `UnixStream`s, so a single-socket link would be a local change.
//!
//! **Ops** are request and reply with ids. Fast ops run on the reader
//! thread; ops that can take long (`kill`, `wait_writable`, `end_all`) run on
//! a thread each, so a slow kill of one pane never delays a write to
//! another.
//!
//! **Events** are written by one thread that walks a cursor per pane into
//! the host's tail ring. A front that reads slowly falls behind its cursor
//! and, when the cursor leaves the ring, is told `gap` and resumes at the
//! ring's start. The pump (the PTY read) never waits for the front: the
//! host's sink here only sets flags.
//!
//! The holder logs link events, protocol errors, unknown op names and pane
//! ids. It never logs a header or a payload (a spawn carries profile
//! secrets in its environment).

use super::wire::{
    backend_error_json, greeting, hello, io_error_json, read_frame, version_error, write_frame,
    Frame, Greeting, WireError, OUTPUT_CHUNK,
};
use super::{HolderHost, HostEvents, SpawnReq, TailRead};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::net::Shutdown;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

/// How long a front has to send its `hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Why [`Holder::serve`] ended abnormally. A front that went away is `Ok`.
#[derive(Debug)]
pub(crate) enum ServeError {
    /// The front speaks another protocol version; it was told and the link
    /// closed.
    VersionMismatch {
        holder: u32,
        front: u32,
    },
    /// The link broke inside a frame, or the front sent a malformed one.
    Wire(WireError),
    Io(io::Error),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionMismatch { holder, front } => {
                write!(f, "holder protocol {holder}, front {front}")
            }
            Self::Wire(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "link error: {err}"),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<WireError> for ServeError {
    fn from(err: WireError) -> Self {
        Self::Wire(err)
    }
}

impl From<io::Error> for ServeError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where the holder's log lines go: stderr, or a buffer in tests.
#[derive(Clone, Default)]
pub(crate) struct Log {
    capture: Option<Arc<Mutex<Vec<String>>>>,
}

impl Log {
    fn line(&self, msg: &str) {
        match &self.capture {
            Some(buf) => lock(buf).push(msg.to_string()),
            None => eprintln!("openab-shepherd: holder: {msg}"),
        }
    }
}

/// An op name from the wire, safe to log: short, printable ASCII.
fn printable(name: &str) -> String {
    name.chars()
        .take(32)
        .map(|c| if c.is_ascii_graphic() { c } else { '?' })
        .collect()
}

/// The holder: its host, and the front it serves now.
pub(crate) struct Holder {
    host: Arc<dyn HolderHost>,
    version: u32,
    log: Log,
    /// The link being served, so a new front can end it.
    active: Mutex<Option<Vec<UnixStream>>>,
    ended: Condvar,
}

impl Holder {
    pub(crate) fn new(host: Arc<dyn HolderHost>) -> Self {
        Self {
            host,
            version: super::wire::HOLDER_VERSION,
            log: Log::default(),
            active: Mutex::new(None),
            ended: Condvar::new(),
        }
    }

    /// A holder that speaks another protocol version (tests).
    #[cfg(test)]
    pub(crate) fn with_version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// Keep the log lines in a buffer instead of writing them (tests).
    #[cfg(test)]
    pub(crate) fn capture_log(mut self) -> (Self, Arc<Mutex<Vec<String>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        self.log.capture = Some(Arc::clone(&buf));
        (self, buf)
    }

    /// Serve one front over `ops` and `events` until it goes away. A front
    /// that connects while another is served replaces it: the old link is
    /// closed first, so a pane never has two dialers.
    ///
    /// The holder's panes are untouched when the front leaves; ending them
    /// is the caller's decision.
    pub(crate) fn serve(&self, ops: UnixStream, events: UnixStream) -> Result<(), ServeError> {
        let handles = vec![ops.try_clone()?, events.try_clone()?];
        {
            let mut active = lock(&self.active);
            while let Some(old) = active.as_ref() {
                for stream in old {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                active = self
                    .ended
                    .wait(active)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            *active = Some(handles);
        }
        let result = self.run(ops, events);
        *lock(&self.active) = None;
        self.ended.notify_all();
        result
    }

    fn run(&self, ops: UnixStream, mut events: UnixStream) -> Result<(), ServeError> {
        let mut ops_r = ops.try_clone()?;
        let ops_w = Mutex::new(ops);
        ops_r.set_read_timeout(Some(HELLO_TIMEOUT))?;
        // A front that is gone already makes these fail; its hello, if it
        // sent one, can still be read, and if not the read says so.
        let _ = write_frame(&mut *lock(&ops_w), &hello(self.version, "ops"), &[]);
        let _ = write_frame(&mut events, &hello(self.version, "events"), &[]);
        let first = match read_frame(&mut ops_r) {
            Ok(frame) => frame,
            Err(WireError::Eof | WireError::Io(_)) => {
                self.log.line("the front left during the handshake");
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };
        let front = match greeting(&first)? {
            Greeting::Hello(v) => v,
            Greeting::Refused { front, .. } => front,
        };
        if front != self.version {
            let _ = write_frame(&mut *lock(&ops_w), &version_error(self.version, front), &[]);
            self.log.line(&format!(
                "refused a front: holder protocol {}, front {front}",
                self.version
            ));
            return Err(ServeError::VersionMismatch {
                holder: self.version,
                front,
            });
        }
        // A front that left right after its hello makes this fail (EINVAL on
        // macOS): it is gone, which is not an error of the link.
        if ops_r.set_read_timeout(None).is_err() {
            self.log.line("the front left during the handshake");
            return Ok(());
        }

        let link = Arc::new(Link {
            host: Arc::clone(&self.host),
            state: Mutex::new(LinkState::default()),
            changed: Condvar::new(),
            shutdown: vec![ops_r.try_clone()?, events.try_clone()?],
        });
        // The sink first, then the cursors: output between the two is in the
        // ring below the cursor's start, which the front reads by `attach`.
        self.host.set_sink(Arc::new(LinkSink {
            link: Arc::clone(&link),
        }));
        link.start_cursors();
        let writer = {
            let link = Arc::clone(&link);
            let log = self.log.clone();
            thread::Builder::new()
                .name("holder-events".to_string())
                .spawn(move || {
                    if let Err(err) = write_events(&link, events) {
                        log.line(&format!("the events link ended: {err}"));
                    }
                    link.close();
                })?
        };
        self.log.line("a front connected");
        let ctx = Arc::new(Ops {
            host: Arc::clone(&self.host),
            link: Arc::clone(&link),
            out: ops_w,
            log: self.log.clone(),
        });
        let result = read_ops(&ctx, &mut ops_r);
        link.close();
        let _ = writer.join();
        self.host.set_sink(Arc::new(Detached));
        self.log.line("the front is gone");
        result
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What the host's sink and the writer thread share for one served front.
struct Link {
    host: Arc<dyn HolderHost>,
    state: Mutex<LinkState>,
    changed: Condvar,
    /// Handles that end both sockets, to unblock a writer stuck in `write`
    /// and the ops reader.
    shutdown: Vec<UnixStream>,
}

#[derive(Default)]
struct LinkState {
    closed: bool,
    panes: HashMap<String, PaneLink>,
}

/// One pane as this front's events see it.
#[derive(Default)]
struct PaneLink {
    /// The offset of the next output byte to send.
    cursor: u64,
    dirty: bool,
    idle: bool,
    /// The host reported the exit: `Some(code)` until it is sent.
    exited: Option<Option<i64>>,
    /// The exit needs no event: it was sent, or the pane had finished before
    /// this front came.
    exit_done: bool,
}

/// Work found for one pane by one pass.
struct Work {
    id: String,
    cursor: u64,
    idle: bool,
    exit: Option<Option<i64>>,
}

impl Link {
    fn state(&self) -> MutexGuard<'_, LinkState> {
        lock(&self.state)
    }

    /// Where each pane the host already holds starts for this front: at its
    /// end (the front reads the past by `attach`). A pane that has finished
    /// needs no exit event, the front learns it from `list`.
    fn start_cursors(&self) {
        let listed = self.host.list();
        let mut st = self.state();
        for pane in listed {
            let end = self.host.tail_bounds(&pane.id).map(|(_, end)| end);
            let entry = st.panes.entry(pane.id).or_default();
            if let Ok(end) = end {
                entry.cursor = end;
            }
            if pane.finished {
                entry.exit_done = true;
                entry.exited = None;
            }
        }
    }

    fn close(&self) {
        self.state().closed = true;
        self.changed.notify_all();
        for stream in &self.shutdown {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Wait for work: the panes with new output, an idle tick or an exit to
    /// send. None once the link is closed.
    fn next_work(&self) -> Option<Vec<Work>> {
        let mut st = self.state();
        loop {
            if st.closed {
                return None;
            }
            let mut work = Vec::new();
            for (id, pane) in &mut st.panes {
                let exit = if pane.exit_done { None } else { pane.exited };
                if pane.dirty || pane.idle || exit.is_some() {
                    work.push(Work {
                        id: id.clone(),
                        cursor: pane.cursor,
                        idle: pane.idle,
                        exit,
                    });
                    pane.dirty = false;
                    pane.idle = false;
                }
            }
            if !work.is_empty() {
                return Some(work);
            }
            st = self
                .changed
                .wait(st)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// A pass over one pane is done: remember its cursor and, when the exit
    /// event went out, that it did.
    fn done(&self, id: &str, cursor: u64, exit_sent: bool) {
        let mut st = self.state();
        let pane = st.panes.entry(id.to_string()).or_default();
        pane.cursor = cursor;
        if exit_sent {
            pane.exit_done = true;
        }
    }

    /// How the pane ended, when the host has reported it to this front's
    /// link: `Some(code)` (the code itself may be unknown).
    fn exit_of(&self, id: &str) -> Option<Option<i64>> {
        self.state().panes.get(id).and_then(|pane| pane.exited)
    }

    fn forget(&self, id: &str) {
        self.state().panes.remove(id);
    }
}

/// The host's sink while a front is served: sets flags and wakes the writer.
/// It never touches a socket and never waits, so the pump is never held back.
struct LinkSink {
    link: Arc<Link>,
}

impl HostEvents for LinkSink {
    fn output(&self, id: &str, _seq: u64, _bytes: &[u8]) -> bool {
        self.link
            .state()
            .panes
            .entry(id.to_string())
            .or_default()
            .dirty = true;
        self.link.changed.notify_all();
        true
    }

    fn idle(&self, id: &str) {
        self.link
            .state()
            .panes
            .entry(id.to_string())
            .or_default()
            .idle = true;
        self.link.changed.notify_all();
    }

    fn exited(&self, id: &str, code: Option<i64>) {
        {
            let mut st = self.link.state();
            let pane = st.panes.entry(id.to_string()).or_default();
            if !pane.exit_done {
                pane.exited = Some(code);
            }
        }
        self.link.changed.notify_all();
    }

    fn gap(&self, _id: &str, _new_start: u64) {}
}

/// The host's sink while no front is served: the panes run on and their
/// output stays in the tail.
struct Detached;

impl HostEvents for Detached {
    fn output(&self, _: &str, _: u64, _: &[u8]) -> bool {
        true
    }
    fn idle(&self, _: &str) {}
    fn exited(&self, _: &str, _: Option<i64>) {}
    fn gap(&self, _: &str, _: u64) {}
}

/// The events writer: for each pane with news, the output from its cursor,
/// then the exit. Holds no lock while it writes.
fn write_events(link: &Link, mut out: UnixStream) -> Result<(), WireError> {
    while let Some(work) = link.next_work() {
        for item in work {
            let mut cursor = item.cursor;
            loop {
                match link.host.tail_read(&item.id, cursor, OUTPUT_CHUNK) {
                    Ok(TailRead::Bytes(bytes)) if bytes.is_empty() => break,
                    Ok(TailRead::Bytes(bytes)) => {
                        let header = json!({"t": "output", "pane": item.id, "seq": cursor});
                        write_frame(&mut out, &header, &bytes)?;
                        cursor += bytes.len() as u64;
                    }
                    Ok(TailRead::Dropped { start }) => {
                        let header = json!({"t": "gap", "pane": item.id, "resume": start});
                        write_frame(&mut out, &header, &[])?;
                        cursor = start;
                    }
                    // Ended since: only its exit is left to tell.
                    Err(_) => break,
                }
            }
            let mut exit_sent = false;
            if let Some(code) = item.exit {
                let header = json!({"t": "exited", "pane": item.id, "code": code});
                write_frame(&mut out, &header, &[])?;
                exit_sent = true;
            } else if item.idle {
                write_frame(&mut out, &json!({"t": "idle", "pane": item.id}), &[])?;
            }
            link.done(&item.id, cursor, exit_sent);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

struct Ops {
    host: Arc<dyn HolderHost>,
    link: Arc<Link>,
    out: Mutex<UnixStream>,
    log: Log,
}

/// What an op answers: reply fields and a payload, or an error object.
enum Answer {
    Ok(Value, Vec<u8>),
    Io(io::Error),
    Backend(crate::backend::BackendError),
    Refused(&'static str),
}

impl Ops {
    fn reply(&self, id: u64, answer: Answer) {
        let (mut header, payload) = match answer {
            Answer::Ok(fields, payload) => {
                let mut header = fields;
                header["ok"] = json!(true);
                (header, payload)
            }
            Answer::Io(err) => (json!({"ok": false, "err": io_error_json(&err)}), Vec::new()),
            Answer::Backend(err) => (
                json!({"ok": false, "berr": backend_error_json(&err)}),
                Vec::new(),
            ),
            Answer::Refused(why) => (
                json!({"ok": false, "err": {"k": "invalid_input", "m": why}}),
                Vec::new(),
            ),
        };
        header["id"] = json!(id);
        // A failed write means the front is gone; the reader finds out.
        let _ = write_frame(&mut *lock(&self.out), &header, &payload);
    }
}

/// The reader thread: one request at a time, in order. Ends when the front
/// closes the link (`Ok`) or breaks it.
fn read_ops(ctx: &Arc<Ops>, ops: &mut UnixStream) -> Result<(), ServeError> {
    loop {
        let frame = match read_frame(ops) {
            Ok(frame) => frame,
            Err(WireError::Eof) => return Ok(()),
            // Our own close (the front replaced, or the events link broke).
            Err(WireError::Io(_)) if lock(&ctx.link.state).closed => return Ok(()),
            Err(err) => {
                ctx.log.line(&format!("the ops link ended: {err}"));
                return Err(err.into());
            }
        };
        let (Some(id), Some(op)) = (frame.u64("id"), frame.str("op").map(str::to_string)) else {
            let err = WireError::Malformed("a request needs an id and an op");
            ctx.log.line(&format!("the ops link ended: {err}"));
            return Err(err.into());
        };
        if matches!(op.as_str(), "kill" | "wait_writable" | "end_all") {
            let ctx2 = Arc::clone(ctx);
            let started = thread::Builder::new()
                .name(format!("holder-{op}"))
                .spawn(move || {
                    let answer = slow_op(&ctx2, &frame);
                    ctx2.reply(id, answer);
                });
            if let Err(err) = started {
                ctx.log
                    .line(&format!("cannot start a thread for {op}: {err}"));
                ctx.reply(id, Answer::Io(err));
            }
        } else {
            let answer = fast_op(ctx, &op, &frame);
            ctx.reply(id, answer);
        }
    }
}

fn pane_of(frame: &Frame) -> Result<&str, Answer> {
    frame
        .str("pane")
        .ok_or(Answer::Refused("a request names its pane"))
}

fn fast_op(ctx: &Ops, op: &str, frame: &Frame) -> Answer {
    let host = &ctx.host;
    match op {
        "spawn" => spawn(host, frame),
        "write" => match pane_of(frame) {
            Ok(id) => match host.try_write(id, &frame.payload) {
                Ok(n) => Answer::Ok(json!({"n": n}), Vec::new()),
                Err(err) => Answer::Io(err),
            },
            Err(answer) => answer,
        },
        "resize" => {
            let (Ok(id), Some(cols), Some(rows)) =
                (pane_of(frame), frame.u64("cols"), frame.u64("rows"))
            else {
                return Answer::Refused("resize needs a pane, cols and rows");
            };
            let (Ok(cols), Ok(rows)) = (u16::try_from(cols), u16::try_from(rows)) else {
                return Answer::Refused("a size is out of range");
            };
            unit(host.resize(id, cols, rows))
        }
        "signal" => {
            let (Ok(id), Some(sig)) = (pane_of(frame), frame.u64("sig")) else {
                return Answer::Refused("signal needs a pane and a signal");
            };
            let Ok(sig) = i32::try_from(sig) else {
                return Answer::Refused("a signal is out of range");
            };
            unit(host.signal(id, sig))
        }
        "fg_pgrp" => match pane_of(frame) {
            Ok(id) => Answer::Ok(json!({"pgrp": host.fg_pgrp(id)}), Vec::new()),
            Err(answer) => answer,
        },
        "list" => {
            let panes: Vec<Value> = host
                .list()
                .into_iter()
                .map(|p| {
                    json!({
                        "id": p.id, "pid": p.pid, "cols": p.cols, "rows": p.rows,
                        "exited": p.exited, "finished": p.finished,
                        "exit_code": p.exit_code,
                    })
                })
                .collect();
            Answer::Ok(json!({"panes": panes}), Vec::new())
        }
        "forget" => match pane_of(frame) {
            Ok(id) => {
                host.forget(id);
                ctx.link.forget(id);
                Answer::Ok(json!({}), Vec::new())
            }
            Err(answer) => answer,
        },
        "attach" => match pane_of(frame) {
            Ok(id) => match host.attach(id) {
                Ok(at) => Answer::Ok(
                    json!({"tail_start": at.tail_start, "tail_end": at.tail_end}),
                    at.bytes,
                ),
                Err(err) => Answer::Io(err),
            },
            Err(answer) => answer,
        },
        "meta_set" => match pane_of(frame) {
            Ok(id) => unit(host.meta_set(id, &frame.payload)),
            Err(answer) => answer,
        },
        "meta_get" => match pane_of(frame) {
            Ok(id) => match host.meta_get(id) {
                Ok(bytes) => Answer::Ok(json!({}), bytes),
                Err(err) => Answer::Io(err),
            },
            Err(answer) => answer,
        },
        other => {
            ctx.log
                .line(&format!("unknown op {:?} refused", printable(other)));
            Answer::Io(io::Error::new(io::ErrorKind::Unsupported, "unknown op"))
        }
    }
}

fn unit(result: io::Result<()>) -> Answer {
    match result {
        Ok(()) => Answer::Ok(json!({}), Vec::new()),
        Err(err) => Answer::Io(err),
    }
}

/// `KEY=VALUE\0` entries, as the front's `spawn` payload carries them.
fn decode_env(payload: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    if payload.is_empty() {
        return Some(Vec::new());
    }
    let body = payload.strip_suffix(&[0])?;
    body.split(|b| *b == 0)
        .map(|entry| {
            let eq = entry.iter().position(|b| *b == b'=')?;
            Some((
                OsString::from_vec(entry[..eq].to_vec()),
                OsString::from_vec(entry[eq + 1..].to_vec()),
            ))
        })
        .collect()
}

fn spawn(host: &Arc<dyn HolderHost>, frame: &Frame) -> Answer {
    let argv: Option<Vec<String>> = frame
        .header
        .get("argv")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().map(|v| v.as_str().map(str::to_string)).collect());
    let (Some(argv), Some(cwd), Some(env)) = (
        argv,
        frame.str("cwd").map(str::to_string),
        decode_env(&frame.payload),
    ) else {
        return Answer::Refused("spawn needs argv, cwd and an environment");
    };
    match host.spawn(SpawnReq { argv, cwd, env }) {
        Ok(spawned) => Answer::Ok(json!({"pane": spawned.id, "pid": spawned.pid}), Vec::new()),
        Err(err) => Answer::Backend(err),
    }
}

/// The ops that can take long, each on its own thread.
fn slow_op(ctx: &Ops, frame: &Frame) -> Answer {
    if frame.str("op") == Some("end_all") {
        ctx.host.end_all();
        return Answer::Ok(json!({}), Vec::new());
    }
    let id = match pane_of(frame) {
        Ok(id) => id,
        Err(answer) => return answer,
    };
    match frame.str("op") {
        Some("kill") => {
            let grace = Duration::from_millis(frame.u64("grace_ms").unwrap_or(0));
            match ctx.host.kill(id, grace) {
                None => Answer::Ok(json!({"found": false}), Vec::new()),
                Some(outcome) => {
                    // The exit rides in the reply too: the events may be far
                    // behind a flood of output, and `kill` must return with
                    // the pane's exit known (the front reads its code).
                    let exit = ctx.link.exit_of(id).map(|code| json!({"code": code}));
                    ctx.link.forget(id);
                    Answer::Ok(
                        json!({"found": true, "killed": outcome.killed, "exit": exit}),
                        Vec::new(),
                    )
                }
            }
        }
        _ => {
            let timeout = Duration::from_millis(frame.u64("ms").unwrap_or(0));
            match ctx.host.wait_writable(id, timeout) {
                Ok(ready) => Answer::Ok(json!({"ready": ready}), Vec::new()),
                Err(err) => Answer::Io(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::host::local::LocalHost;
    use crate::native::host::PtyHost;
    use crate::testutil::TempDir;
    use std::time::Instant;

    struct Quiet;

    impl HostEvents for Quiet {
        fn output(&self, _: &str, _: u64, _: &[u8]) -> bool {
            true
        }
        fn idle(&self, _: &str) {}
        fn exited(&self, _: &str, _: Option<i64>) {}
        fn gap(&self, _: &str, _: u64) {}
    }

    /// The writer's rule: when a pane's output and its exit are both ready
    /// in one pass, all the output goes first. The state is made by hand
    /// because a real pump races the writer and would hide a violation most
    /// of the time.
    #[test]
    fn an_exit_ready_together_with_unsent_output_goes_after_the_output() {
        let local = Arc::new(LocalHost::with_tail(4096));
        local.set_sink(Arc::new(Quiet));
        let dir = TempDir::new("server-order");
        let pane = local
            .spawn(SpawnReq {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf hello-world; exit 3".into(),
                ],
                cwd: dir.path().to_string_lossy().into_owned(),
                env: crate::native::host::pty::child_env(|k| std::env::var_os(k), &[]).unwrap(),
            })
            .unwrap();
        let end = Instant::now() + Duration::from_secs(15);
        while !local.list().iter().any(|p| p.id == pane.id && p.finished) {
            assert!(Instant::now() < end, "the pane never finished");
            thread::sleep(Duration::from_millis(10));
        }
        let (mut front_events, holder_events) = UnixStream::pair().unwrap();
        let (ops_keep, _ops_other) = UnixStream::pair().unwrap();
        let link = Arc::new(Link {
            host: Arc::clone(&local) as Arc<dyn HolderHost>,
            state: Mutex::new(LinkState::default()),
            changed: Condvar::new(),
            shutdown: vec![ops_keep, holder_events.try_clone().unwrap()],
        });
        {
            let mut st = link.state();
            let entry = st.panes.entry(pane.id.clone()).or_default();
            entry.dirty = true;
            entry.exited = Some(Some(3));
        }
        let writer = {
            let link = Arc::clone(&link);
            thread::spawn(move || write_events(&link, holder_events))
        };
        let first = read_frame(&mut front_events).unwrap();
        assert_eq!(first.str("t"), Some("output"), "{:?}", first.header);
        assert_eq!(first.payload, b"hello-world");
        let second = read_frame(&mut front_events).unwrap();
        assert_eq!(second.str("t"), Some("exited"), "{:?}", second.header);
        assert_eq!(second.header["code"], json!(3));
        link.close();
        writer.join().unwrap().unwrap();
        local.end_all();
    }
}
