//! The process half of the native core (issue #76, ADR
//! restart-survival-and-shared-panes): spawning a child on a PTY, pumping its
//! output, writing its input, signalling and ending its process tree, and
//! reaping it. The screen half (emulator, tracker, guard, tools) stays in
//! [`super::NativeBackend`] and talks to this half only through [`PtyHost`]
//! (calls into the host) and [`HostEvents`] (calls back from its threads).
//!
//! [`local::LocalHost`] runs the process half in this process, which is how
//! the native core has always worked. A pane is named by an id the host mints
//! (`n:N`), so the id means the same to anyone who later reaches the same
//! host.

#[cfg(test)]
mod contract;
pub(super) mod local;
pub(super) mod procs;
pub(super) mod pty;
pub(super) mod tail;

use crate::backend::BackendError;
use std::ffi::OsString;
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// How long a stop gives a pane's processes to exit after SIGHUP and SIGTERM
/// before SIGKILL.
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(2);

/// The most bytes of opaque metadata a pane's `meta` holds. The front's own
/// blob is a kilobyte or two; this only bounds a bug.
pub(crate) const META_CAP: usize = 64 * 1024;

/// What to start: a non-empty argv, an absolute working directory, and
/// exactly the child's environment.
pub(crate) struct SpawnReq {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: Vec<(OsString, OsString)>,
}

/// A started pane. `pid` is the leader's pid, which is also its process
/// group id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spawned {
    pub id: String,
    pub pid: i32,
}

/// How a [`PtyHost::kill`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KillOutcome {
    /// The leader was still running when the grace ran out, so SIGKILL ended
    /// it, as opposed to exiting on SIGHUP or SIGTERM (or on its own).
    pub killed: bool,
}

/// One pane the host holds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))] // read by the contract tests; adoption uses it next
pub(crate) struct HostPane {
    pub id: String,
    pub pid: i32,
    pub cols: u16,
    pub rows: u16,
    /// The leader has exited (the pane stays listed until `kill` or `forget`).
    pub exited: bool,
}

/// What a host that keeps a tail hands a front that starts later: the offsets
/// of the bytes it still has and the bytes themselves, read together so they
/// describe the same moment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))] // adoption uses it next
pub(crate) struct Attached {
    /// Offset of the first byte in `bytes`; 0 means nothing was dropped.
    pub tail_start: u64,
    /// Offset after the last byte in `bytes`.
    pub tail_end: u64,
    pub bytes: Vec<u8>,
}

/// The process half. Every method names its pane by the id [`spawn`] gave.
///
/// [`spawn`]: PtyHost::spawn
pub(crate) trait PtyHost: Send + Sync {
    /// Where the host reports output and exits. Set once, before `spawn`.
    fn set_sink(&self, sink: Arc<dyn HostEvents>);

    /// Start a pane and its pump and reaper threads. The id is minted here.
    fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError>;

    /// One non-blocking write to the pane's input. `WouldBlock` when the PTY
    /// input buffer is full; the caller keeps its own lock-release loop.
    fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize>;

    /// Wait up to `timeout` until the pane accepts input.
    fn wait_writable(&self, id: &str, timeout: Duration) -> io::Result<bool>;

    /// Set the terminal size.
    #[cfg_attr(not(test), allow(dead_code))] // the adoption nudge uses it next
    fn resize(&self, id: &str, cols: u16, rows: u16) -> io::Result<()>;

    /// Signal the pane's process group, while the group id is still the
    /// pane's (not once the leader is reaped).
    #[cfg_attr(not(test), allow(dead_code))] // the contract tests use it
    fn signal(&self, id: &str, signal: i32) -> io::Result<()>;

    /// End the pane: SIGHUP and SIGTERM to its group and every descendant,
    /// up to `grace` for them to exit, then SIGKILL to what is left; the
    /// leader is reaped and the pane is forgotten. None for an unknown id.
    fn kill(&self, id: &str, grace: Duration) -> Option<KillOutcome>;

    /// The terminal's foreground process group, as read on the master.
    fn fg_pgrp(&self, id: &str) -> Option<i32>;

    /// Every pane the host holds, in creation order.
    #[cfg_attr(not(test), allow(dead_code))] // adoption uses it next
    fn list(&self) -> Vec<HostPane>;

    /// Forget a pane that is no longer wanted without waiting: end what is
    /// left of its group for good and let the reaper reap the leader.
    fn forget(&self, id: &str);

    /// End every pane, as a clean shutdown does: one shared grace for all.
    fn end_all(&self);

    /// The pane's tail with its offsets, atomically. `NotFound` for an
    /// unknown id, `Unsupported` for a host that keeps no tail.
    #[cfg_attr(not(test), allow(dead_code))] // adoption uses it next
    fn attach(&self, id: &str) -> io::Result<Attached>;

    /// Replace the pane's metadata: opaque bytes the host never reads, at
    /// most [`META_CAP`]. It lives as long as the pane, so it survives the
    /// front, and is returned unchanged by `meta_get`.
    #[cfg_attr(not(test), allow(dead_code))] // the front's meta store uses it
    fn meta_set(&self, id: &str, bytes: &[u8]) -> io::Result<()>;

    /// The pane's metadata; empty when none was set.
    #[cfg_attr(not(test), allow(dead_code))] // adoption uses it next
    fn meta_get(&self, id: &str) -> io::Result<Vec<u8>>;

    /// Whether panes outlive the process that asked for them.
    fn survives_restart(&self) -> bool;
}

/// What the host's threads report. Called on the host's own threads, one pane
/// at a time and in order: every `output` of a pane comes before its
/// `exited`.
pub(crate) trait HostEvents: Send + Sync {
    /// A chunk of the pane's output. `seq` is the cumulative byte offset of
    /// the chunk's first byte (0 for the first chunk). False when the sink
    /// cannot take more and the pump should stop.
    fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool;

    /// The pump found nothing to read for one poll interval.
    fn idle(&self, id: &str);

    /// The leader has exited and the pump has drained the terminal: no more
    /// `output` follows. `code` is the exit code (128 + the signal for a
    /// signal death).
    fn exited(&self, id: &str, code: Option<i64>);

    /// The sink fell behind and bytes before offset `new_start` were dropped.
    /// A host that keeps no replay buffer never sends it.
    #[allow(dead_code)] // the in-process host never sends it; a link host and its tests do
    fn gap(&self, id: &str, new_start: u64);
}
