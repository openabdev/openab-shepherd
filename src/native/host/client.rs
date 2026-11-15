//! [`HolderClient`]: a [`PtyHost`] that reaches its panes over the link to a
//! holder (issue #76). See `server.rs` for the shape of the link.
//!
//! Three threads' worth of work, none of which may wait for another:
//!
//! - **callers** write a request on `ops` and wait on a channel for the
//!   reply. They may hold the front's pane locks while they wait
//!   (`write_guarded` holds `state` across a `write`);
//! - the **router** thread reads `ops` and hands each reply to its caller. It
//!   takes no pane lock, no table lock and never calls the sink, so a caller
//!   that holds a lock cannot keep its own reply from arriving;
//! - the **events** thread reads `events` and calls the sink, which takes
//!   those locks. It may wait for a caller that waits for the router, which
//!   is fine because the router does not wait for it.

use super::wire::{
    backend_error_from_json, greeting, hello, io_error_from_json, read_frame, version_error,
    write_frame, Frame, Greeting, WireError, HOLDER_VERSION,
};
use super::{Attached, HostEvents, HostPane, KillOutcome, PtyHost, SpawnReq, Spawned};
use crate::backend::{BackendError, ErrorKind};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io;
use std::net::Shutdown;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

/// How long a request may go unanswered, on top of the time the op itself is
/// allowed to take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a client could not be connected.
#[derive(Debug)]
pub(crate) enum ConnectError {
    /// The holder speaks another protocol version.
    VersionMismatch {
        holder: u32,
        front: u32,
    },
    Wire(WireError),
    Io(io::Error),
}

impl std::fmt::Display for ConnectError {
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

impl std::error::Error for ConnectError {}

impl From<WireError> for ConnectError {
    fn from(err: WireError) -> Self {
        Self::Wire(err)
    }
}

impl From<io::Error> for ConnectError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Shared {
    ops_w: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Frame>>>,
    /// The link is broken: every call fails at once.
    dead: AtomicBool,
    sink: Mutex<Option<Arc<dyn HostEvents>>>,
    sink_set: Condvar,
    /// Panes whose exit the sink has been given, for the life of the link
    /// (a pane id is never reused, so an entry is never stale). Held while
    /// the sink is called, so whoever finds a pane in it knows the exit has
    /// been applied.
    exited: Mutex<HashSet<String>>,
    /// Handles that end the sockets, to wake the threads that read them.
    streams: Vec<UnixStream>,
    reply_timeout_ms: AtomicU64,
    /// Test hook: the events thread reads nothing while paused, like a front
    /// that is stuck.
    #[cfg(test)]
    paused: (Mutex<bool>, Condvar),
}

impl Shared {
    /// The link is gone: wake every waiter and refuse every later call.
    fn fail(&self) {
        self.dead.store(true, Ordering::Release);
        for stream in &self.streams {
            let _ = stream.shutdown(Shutdown::Both);
        }
        lock(&self.pending).clear();
        self.sink_set.notify_all();
        #[cfg(test)]
        {
            *lock(&self.paused.0) = false;
            self.paused.1.notify_all();
        }
    }

    fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
    }

    /// Give the sink a pane's exit, once. It comes by the holder's `exited`
    /// event and by the reply to `kill`, whichever is first; when this
    /// returns the sink has the exit, whichever of them put it there.
    fn deliver_exit(&self, id: &str, code: Option<i64>) {
        let mut seen = lock(&self.exited);
        if !seen.insert(id.to_string()) {
            return;
        }
        let sink = lock(&self.sink).clone();
        if let Some(sink) = sink {
            sink.exited(id, code);
        }
    }

    /// The sink, once there is one. None when the link died first.
    fn wait_sink(&self) -> Option<Arc<dyn HostEvents>> {
        let mut sink = lock(&self.sink);
        loop {
            if let Some(sink) = sink.as_ref() {
                return Some(Arc::clone(sink));
            }
            if self.is_dead() {
                return None;
            }
            sink = self
                .sink_set
                .wait(sink)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

fn link_down() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the holder link is closed")
}

fn wire_to_io(err: WireError) -> io::Error {
    match err {
        WireError::Io(err) => err,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

/// A pane host over a holder link. Dropping the last handle closes the link;
/// the holder keeps its panes.
pub(crate) struct HolderClient {
    shared: Arc<Shared>,
    /// Test rig: the holder this client was connected to, kept alive with it.
    #[cfg(test)]
    rig: Mutex<Option<Arc<super::held::Held>>>,
}

impl HolderClient {
    /// Greet the holder and start the router and events threads.
    #[cfg_attr(not(test), allow(dead_code))] // the front passes its own version
    pub(crate) fn connect(ops: UnixStream, events: UnixStream) -> Result<Self, ConnectError> {
        Self::connect_as(ops, events, HOLDER_VERSION)
    }

    /// As [`connect`](Self::connect), speaking protocol `version` (the
    /// front's test hook for the mismatch loop, and tests).
    pub(crate) fn connect_as(
        ops: UnixStream,
        mut events: UnixStream,
        version: u32,
    ) -> Result<Self, ConnectError> {
        let mut ops_r = ops.try_clone()?;
        let mut ops_w = ops.try_clone()?;
        let streams = vec![ops.try_clone()?, events.try_clone()?];
        // Before the hello goes out: a holder that refuses it closes at once,
        // and setting an option on a socket whose peer is gone fails on macOS.
        ops_r.set_read_timeout(Some(REPLY_TIMEOUT))?;
        events.set_read_timeout(Some(REPLY_TIMEOUT))?;
        write_frame(&mut ops_w, &hello(version, "ops"), &[])?;
        // Read both greetings before judging either, so nothing the holder
        // wrote is left unread when this side closes on a mismatch (an
        // unread byte makes the close a reset on some systems, and the
        // holder would then not hear why).
        let mut holders = Vec::new();
        for stream in [&mut ops_r, &mut events] {
            let first = read_frame(stream)?;
            holders.push(match greeting(&first)? {
                Greeting::Hello(holder) => holder,
                Greeting::Refused { holder, .. } => holder,
            });
        }
        if let Some(&holder) = holders.iter().find(|&&h| h != version) {
            // Tell the holder why we are leaving (it may have told us).
            let _ = write_frame(&mut ops_w, &version_error(holder, version), &[]);
            for stream in &streams {
                let _ = stream.shutdown(Shutdown::Both);
            }
            return Err(ConnectError::VersionMismatch {
                holder,
                front: version,
            });
        }
        ops_r.set_read_timeout(None)?;
        events.set_read_timeout(None)?;
        let shared = Arc::new(Shared {
            ops_w: Mutex::new(ops_w),
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
            sink: Mutex::new(None),
            sink_set: Condvar::new(),
            exited: Mutex::new(HashSet::new()),
            streams,
            reply_timeout_ms: AtomicU64::new(REPLY_TIMEOUT.as_millis() as u64),
            #[cfg(test)]
            paused: (Mutex::new(false), Condvar::new()),
        });
        {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("holder-router".to_string())
                .spawn(move || route_replies(&shared, ops_r))?;
        }
        {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("holder-events-in".to_string())
                .spawn(move || deliver_events(&shared, events))?;
        }
        Ok(Self {
            shared,
            #[cfg(test)]
            rig: Mutex::new(None),
        })
    }

    /// Keep the test rig alive as long as this client (tests).
    #[cfg(test)]
    pub(crate) fn keep(&self, rig: Arc<super::held::Held>) {
        *lock(&self.rig) = Some(rig);
    }

    /// The holder's own host, for tests that reach into a pane's process.
    #[cfg(test)]
    pub(crate) fn local_for_tests(&self) -> Option<Arc<super::local::LocalHost>> {
        lock(&self.rig).as_ref().map(|rig| Arc::clone(&rig.local))
    }

    /// Wait this long for a reply (tests).
    #[cfg(test)]
    pub(crate) fn set_reply_timeout(&self, timeout: Duration) {
        self.shared
            .reply_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
    }

    /// Stop reading events, as a front stuck elsewhere does (tests).
    #[cfg(test)]
    pub(crate) fn pause_events(&self, paused: bool) {
        *lock(&self.shared.paused.0) = paused;
        self.shared.paused.1.notify_all();
    }

    /// Whether the link has broken.
    pub(crate) fn is_closed(&self) -> bool {
        self.shared.is_dead()
    }

    /// One request and its raw reply: transport failures only. The reply may
    /// say `ok: false`.
    fn call_raw(
        &self,
        op: &str,
        mut fields: Value,
        payload: &[u8],
        extra: Duration,
    ) -> io::Result<Frame> {
        let shared = &self.shared;
        if shared.is_dead() {
            return Err(link_down());
        }
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = channel();
        lock(&shared.pending).insert(id, tx);
        fields["op"] = json!(op);
        fields["id"] = json!(id);
        let sent = write_frame(&mut *lock(&shared.ops_w), &fields, payload);
        if let Err(err) = sent {
            lock(&shared.pending).remove(&id);
            // A frame half written leaves the stream unusable.
            shared.fail();
            return Err(wire_to_io(err));
        }
        // A link that died while we wrote cleared `pending` first: the
        // channel is then closed and `recv` fails at once.
        let wait = Duration::from_millis(shared.reply_timeout_ms.load(Ordering::Relaxed)) + extra;
        match rx.recv_timeout(wait) {
            Ok(frame) => Ok(frame),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                lock(&shared.pending).remove(&id);
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("the holder did not answer {op}"),
                ))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(link_down()),
        }
    }

    /// One request whose refusal is an `io::Error`.
    fn call(&self, op: &str, fields: Value, payload: &[u8], extra: Duration) -> io::Result<Frame> {
        let reply = self.call_raw(op, fields, payload, extra)?;
        if reply.header.get("ok") == Some(&Value::Bool(true)) {
            return Ok(reply);
        }
        Err(reply.header.get("err").map_or_else(
            || io::Error::other("the holder refused the request"),
            io_error_from_json,
        ))
    }

    fn log_failed(&self, what: &str, err: &io::Error) {
        if !self.shared.is_dead() {
            eprintln!("openab-shepherd: native: holder: {what} failed: {err}");
        }
    }
}

impl Drop for HolderClient {
    fn drop(&mut self) {
        self.shared.fail();
    }
}

/// The router thread: replies to their callers, nothing else.
fn route_replies(shared: &Shared, mut ops: UnixStream) {
    loop {
        match read_frame(&mut ops) {
            Ok(frame) => {
                if let Some(id) = frame.u64("id") {
                    if let Some(tx) = lock(&shared.pending).remove(&id) {
                        let _ = tx.send(frame);
                    }
                }
            }
            Err(_) => {
                shared.fail();
                return;
            }
        }
    }
}

/// The events thread: the holder's events to the sink, in order.
fn deliver_events(shared: &Shared, mut events: UnixStream) {
    // Events before a sink would have nowhere to go: leave them in the
    // socket (the holder's cursor holds them) until the front is ready.
    if shared.wait_sink().is_none() {
        return;
    }
    loop {
        let frame = match read_frame(&mut events) {
            Ok(frame) => frame,
            Err(_) => {
                shared.fail();
                return;
            }
        };
        // Holding a frame back, as a front stuck in the sink does.
        #[cfg(test)]
        {
            let mut paused = lock(&shared.paused.0);
            while *paused {
                paused = shared
                    .paused
                    .1
                    .wait(paused)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        let Some(sink) = shared.wait_sink() else {
            return;
        };
        let Some(pane) = frame.str("pane") else {
            continue;
        };
        match frame.str("t") {
            Some("output") => {
                if let Some(seq) = frame.u64("seq") {
                    sink.output(pane, seq, &frame.payload);
                }
            }
            Some("idle") => sink.idle(pane),
            Some("exited") => {
                shared.deliver_exit(pane, frame.header.get("code").and_then(Value::as_i64));
            }
            Some("gap") => {
                if let Some(resume) = frame.u64("resume") {
                    sink.gap(pane, resume);
                }
            }
            _ => {}
        }
    }
}

fn u64_field(reply: &Frame, key: &str) -> io::Result<u64> {
    reply.u64(key).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "the holder's reply is incomplete",
        )
    })
}

impl PtyHost for HolderClient {
    fn set_sink(&self, sink: Arc<dyn HostEvents>) {
        *lock(&self.shared.sink) = Some(sink);
        self.shared.sink_set.notify_all();
    }

    fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError> {
        let mut env = Vec::new();
        for (key, value) in &req.env {
            env.extend_from_slice(key.as_bytes());
            env.push(b'=');
            env.extend_from_slice(value.as_bytes());
            env.push(0);
        }
        let reply = self
            .call_raw(
                "spawn",
                json!({"argv": req.argv, "cwd": req.cwd}),
                &env,
                Duration::ZERO,
            )
            .map_err(|err| {
                BackendError::new(
                    ErrorKind::Unavailable,
                    format!("native: the holder link failed: {err}"),
                )
            })?;
        if reply.header.get("ok") != Some(&Value::Bool(true)) {
            return Err(match reply.header.get("berr") {
                Some(berr) => backend_error_from_json(berr),
                None => BackendError::new(
                    ErrorKind::Other,
                    format!(
                        "native: the holder refused the spawn: {}",
                        io_error_from_json(reply.header.get("err").unwrap_or(&Value::Null))
                    ),
                ),
            });
        }
        let (Some(id), Some(pid)) = (
            reply.str("pane").map(str::to_string),
            reply.header.get("pid").and_then(Value::as_i64),
        ) else {
            return Err(BackendError::new(
                ErrorKind::Protocol,
                "native: the holder's spawn reply is incomplete",
            ));
        };
        Ok(Spawned {
            id,
            pid: pid as i32,
        })
    }

    fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize> {
        let reply = self.call("write", json!({"pane": id}), bytes, Duration::ZERO)?;
        Ok(u64_field(&reply, "n")? as usize)
    }

    fn wait_writable(&self, id: &str, timeout: Duration) -> io::Result<bool> {
        let reply = self.call(
            "wait_writable",
            json!({"pane": id, "ms": timeout.as_millis() as u64}),
            &[],
            timeout,
        )?;
        Ok(reply.header.get("ready") == Some(&Value::Bool(true)))
    }

    fn resize(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        self.call(
            "resize",
            json!({"pane": id, "cols": cols, "rows": rows}),
            &[],
            Duration::ZERO,
        )
        .map(drop)
    }

    fn signal(&self, id: &str, signal: i32) -> io::Result<()> {
        self.call(
            "signal",
            json!({"pane": id, "sig": signal}),
            &[],
            Duration::ZERO,
        )
        .map(drop)
    }

    fn kill(&self, id: &str, grace: Duration) -> Option<KillOutcome> {
        // The holder runs the stop (its grace, then up to 2 s to reap).
        let extra = grace + Duration::from_secs(2);
        let reply = match self.call(
            "kill",
            json!({"pane": id, "grace_ms": grace.as_millis() as u64}),
            &[],
            extra,
        ) {
            Ok(reply) => reply,
            Err(err) => {
                self.log_failed("kill", &err);
                return None;
            }
        };
        if reply.header.get("found") != Some(&Value::Bool(true)) {
            return None;
        }
        // The pane's exit is its by the time `kill` returns (`remove` reads
        // its code), even when the events are still behind a flood of output.
        if let Some(exit) = reply.header.get("exit").filter(|e| e.is_object()) {
            self.shared
                .deliver_exit(id, exit.get("code").and_then(Value::as_i64));
        }
        Some(KillOutcome {
            killed: reply.header.get("killed") == Some(&Value::Bool(true)),
        })
    }

    fn fg_pgrp(&self, id: &str) -> Option<i32> {
        let reply = self
            .call("fg_pgrp", json!({"pane": id}), &[], Duration::ZERO)
            .ok()?;
        reply
            .header
            .get("pgrp")
            .and_then(Value::as_i64)
            .map(|p| p as i32)
    }

    fn list(&self) -> Vec<HostPane> {
        let reply = match self.call("list", json!({}), &[], Duration::ZERO) {
            Ok(reply) => reply,
            Err(err) => {
                self.log_failed("list", &err);
                return Vec::new();
            }
        };
        let Some(panes) = reply.header.get("panes").and_then(Value::as_array) else {
            return Vec::new();
        };
        panes
            .iter()
            .filter_map(|p| {
                Some(HostPane {
                    id: p.get("id")?.as_str()?.to_string(),
                    pid: p.get("pid")?.as_i64()? as i32,
                    cols: u16::try_from(p.get("cols")?.as_u64()?).ok()?,
                    rows: u16::try_from(p.get("rows")?.as_u64()?).ok()?,
                    exited: p.get("exited")?.as_bool()?,
                    finished: p.get("finished")?.as_bool()?,
                    exit_code: p.get("exit_code").and_then(Value::as_i64),
                })
            })
            .collect()
    }

    fn forget(&self, id: &str) {
        if let Err(err) = self.call("forget", json!({"pane": id}), &[], Duration::ZERO) {
            self.log_failed("forget", &err);
        }
    }

    fn end_all(&self) {
        let extra = Duration::from_secs(10);
        if let Err(err) = self.call("end_all", json!({}), &[], extra) {
            self.log_failed("end_all", &err);
        }
    }

    fn attach(&self, id: &str) -> io::Result<Attached> {
        let reply = self.call("attach", json!({"pane": id}), &[], Duration::ZERO)?;
        let (tail_start, tail_end) = (
            u64_field(&reply, "tail_start")?,
            u64_field(&reply, "tail_end")?,
        );
        if tail_end.checked_sub(tail_start) != Some(reply.payload.len() as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the holder's tail does not match its offsets",
            ));
        }
        Ok(Attached {
            tail_start,
            tail_end,
            bytes: reply.payload,
        })
    }

    fn meta_set(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        self.call("meta_set", json!({"pane": id}), bytes, Duration::ZERO)
            .map(drop)
    }

    fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
        Ok(self
            .call("meta_get", json!({"pane": id}), &[], Duration::ZERO)?
            .payload)
    }

    fn survives_restart(&self) -> bool {
        true
    }
}
