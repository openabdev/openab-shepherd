//! [`LocalHost`]: the process half in this process. The code is the native
//! core's former pane process handling, moved here unchanged; the screen half
//! now reaches it through [`PtyHost`] and hears from its threads through
//! [`HostEvents`].
//!
//! Locking: a pane's `tree` is taken before its `state`; nothing here holds a
//! lock across a sleep, a poll or a wait, except the condvar waits, which
//! release `state`. The shared process-table snapshot (`procs::Shared`) has a
//! lock of its own, taken under a pane's `tree` and never with anything else
//! held; only the readers' periodic look uses it, never a stop. The registry lock is held only to look a pane up or
//! change the list, never across a call into the sink or a wait.

use super::tail::TailRing;
use super::{
    procs, pty, Attached, HolderHost, HostEvents, HostPane, KillOutcome, PtyHost, SpawnReq,
    Spawned, TailRead, KILL_GRACE, META_CAP,
};
use crate::backend::{BackendError, ErrorKind};
use crate::native::term::{COLS, ROWS};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    /// Set to make the next pane thread started on this thread fail.
    pub(crate) static FAIL_NEXT_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The pid of the last child spawned on this thread.
    pub(crate) static LAST_SPAWN_PID: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

const READ_CHUNK: usize = 16 * 1024;
/// How often the reader wakes to check for removal.
const READ_POLL_MS: libc::c_int = 200;
/// After the leader exits, how long the reaper waits for the reader to reach
/// the end of the terminal before it decides nothing more is coming: a
/// descendant that keeps the slave open without writing never gives the
/// reader an end of file. The wait goes on past this while output the leader
/// left is still being taken (see [`EXIT_DRAIN_CAP`]).
pub(super) const EXIT_DRAIN: Duration = Duration::from_millis(500);
/// The most the reaper waits for the reader after the leader exits, however
/// slowly the sink takes the output or however long a descendant keeps
/// writing. `exited` follows at the latest this long after the exit.
pub(super) const EXIT_DRAIN_CAP: Duration = Duration::from_secs(5);
/// How often the reaper looks again while it waits for the reader.
const EXIT_DRAIN_POLL: Duration = Duration::from_millis(10);
/// How often the reader refreshes the pane's process tree while the leader
/// runs, so a descendant is known before its parent can exit (ADR §11).
const TREE_POLL: Duration = Duration::from_millis(250);
/// How often a stop re-checks whether the pane's descendants have exited.
const TREE_WAIT_POLL: Duration = Duration::from_millis(50);
/// At most this many refresh-and-stop rounds before the final SIGKILL.
const FREEZE_ROUNDS: usize = 8;

/// The panes this host runs, in creation order.
#[derive(Default)]
struct Registry {
    next_id: u64,
    panes: Vec<Arc<Proc>>,
}

/// Finished panes a host that keeps tails holds before it forgets the
/// oldest: a backstop for a host nobody is attached to. The front's own cap
/// (`MAX_FINISHED_PANES`) is the policy and is lower.
const FINISHED_BACKSTOP: usize = 64;

/// Where the host's threads report. Shared with them, and read at every
/// event, so a front that starts later and calls `set_sink` is the one that
/// hears of panes already running.
type SinkCell = Arc<Mutex<Option<Arc<dyn HostEvents>>>>;

fn current(cell: &SinkCell) -> Option<Arc<dyn HostEvents>> {
    cell.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

pub(crate) struct LocalHost {
    registry: Mutex<Registry>,
    sink: SinkCell,
    /// The size of each pane's tail, or None for no tail. Off for the
    /// single-process server, which has no later front to serve, so it pays
    /// no memory and no copy per chunk.
    tail_cap: Option<usize>,
    finished_backstop: usize,
    /// One snapshot of the process table for every pane's periodic look.
    scans: Arc<procs::Shared>,
}

pub(crate) struct Proc {
    pub(crate) id: String,
    /// The leader's pid, which is also the group id.
    pub(crate) pid: i32,
    pub(crate) master: OwnedFd,
    /// The size last set on the terminal, `(cols, rows)`.
    size: Mutex<(u16, u16)>,
    pub(crate) state: Mutex<ProcState>,
    changed: Condvar,
    /// Every descendant seen so far (ADR §11). Taken before `state`.
    tree: Mutex<procs::Tree>,
    /// The host's snapshot, for the reader's periodic look at the tree.
    scans: Arc<procs::Shared>,
    /// When the pane was created: a snapshot that began earlier is not used.
    born: Instant,
    /// A removal or shutdown is ending the pane: the reaper leaves the
    /// descendants' grace to it and reaps once it has released the pane.
    stopping: AtomicBool,
    /// Stops the reader. Set only once the leader is reaped, because a
    /// process exiting with output still queued on the terminal waits
    /// (on macOS forever) for someone to read it. The parent's copy of the
    /// slave stays open until the leader exits (issue #54), so macOS cannot
    /// lose a quick program's output; the reaper closes it at the exit.
    shutdown: AtomicBool,
    reader: Mutex<Option<JoinHandle<()>>>,
    /// The reader is taking a chunk: set once the terminal has output ready,
    /// before the read, and cleared once the sink has the chunk. With the
    /// terminal's queue it says whether output is still on its way to the
    /// sink; see `Proc::output_pending`.
    reading: AtomicBool,
    /// The last output bytes, when the host keeps a tail.
    tail: Option<Mutex<TailRing>>,
    /// Opaque metadata the front keeps with the pane.
    meta: Mutex<Vec<u8>>,
}

pub(crate) struct ProcState {
    /// The leader has exited and the reader has drained the terminal (or the
    /// drain wait ran out).
    live: bool,
    /// The leader has exited: set as soon as its status is read, before the
    /// reaper waits for the reader to drain, so a stop decides `killed` from
    /// when the leader exited, not from when `live` was cleared.
    exited: bool,
    reader_done: bool,
    /// How the leader ended, set with `live = false`.
    exit_code: Option<i64>,
    /// The leader has been reaped: its pid, so the group and session ids,
    /// are no longer reserved for this pane (ADR §11).
    pub(crate) reaped: bool,
    /// The pane's group and descendants were killed for good (`release`);
    /// the reaper reaps the leader only after this.
    released: bool,
    /// The reaper fell back to a plain wait, which frees the group id: the
    /// group is never signalled again.
    pub(crate) group_lost: bool,
}

impl LocalHost {
    pub(crate) fn new() -> Self {
        Self {
            registry: Mutex::new(Registry::default()),
            sink: Arc::new(Mutex::new(None)),
            tail_cap: None,
            finished_backstop: FINISHED_BACKSTOP,
            scans: Arc::new(procs::Shared::new()),
        }
    }

    /// A host that keeps the last `cap` bytes of each pane's output, for the
    /// holder (with [`super::tail::TAIL_CAP`]) and for tests.
    pub(crate) fn with_tail(cap: usize) -> Self {
        Self {
            tail_cap: Some(cap),
            ..Self::new()
        }
    }

    /// Test hook: forget the oldest finished pane beyond `n` at a spawn.
    #[cfg(test)]
    pub(crate) fn with_finished_backstop(mut self, n: usize) -> Self {
        self.finished_backstop = n;
        self
    }

    /// Test hook: the age limit of the shared process-table snapshot.
    #[cfg(test)]
    pub(crate) fn with_scan_max_age(mut self, max_age: Duration) -> Self {
        self.scans = Arc::new(procs::Shared::with_max_age(max_age));
        self
    }

    /// Test hook: how many scans of the process table the host has taken.
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn scans(&self) -> usize {
        self.scans.scans()
    }

    /// With a tail kept, make room for one more finished pane: forget the
    /// oldest finished ones beyond the backstop.
    fn evict_finished(&self) {
        if self.tail_cap.is_none() {
            return;
        }
        let panes = self.registry().panes.clone();
        let finished: Vec<&Arc<Proc>> = panes.iter().filter(|p| p.has_exited()).collect();
        let excess = finished.len().saturating_sub(self.finished_backstop);
        for pane in finished.into_iter().take(excess) {
            self.forget(&pane.id);
        }
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn find(&self, id: &str) -> Option<Arc<Proc>> {
        self.registry().panes.iter().find(|p| p.id == id).cloned()
    }

    fn take(&self, id: &str) -> Option<Arc<Proc>> {
        let mut registry = self.registry();
        let at = registry.panes.iter().position(|p| p.id == id)?;
        Some(registry.panes.remove(at))
    }

    fn unknown(id: &str) -> io::Error {
        io::Error::new(io::ErrorKind::NotFound, format!("no such pane: {id}"))
    }

    /// The pane's process state, for tests that read or set a flag.
    #[cfg(test)]
    pub(crate) fn proc(&self, id: &str) -> Option<Arc<Proc>> {
        self.find(id)
    }
}

impl PtyHost for LocalHost {
    fn set_sink(&self, sink: Arc<dyn HostEvents>) {
        *self.sink.lock().unwrap_or_else(PoisonError::into_inner) = Some(sink);
    }

    fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError> {
        if current(&self.sink).is_none() {
            return Err(BackendError::new(
                ErrorKind::Other,
                "native: the host has no sink",
            ));
        }
        let sink = Arc::clone(&self.sink);
        self.evict_finished();
        let pty::Spawned {
            master,
            slave,
            child,
        } = pty::spawn(&req.argv, Path::new(&req.cwd), &req.env)?;
        let pid = child.id() as i32;
        #[cfg(test)]
        LAST_SPAWN_PID.with(|last| last.set(pid));
        let mut registry = self.registry();
        registry.next_id += 1;
        let id = format!("n:{}", registry.next_id);
        let pane = Arc::new(Proc {
            id: id.clone(),
            pid,
            master,
            size: Mutex::new((COLS, ROWS)),
            state: Mutex::new(ProcState {
                live: true,
                exited: false,
                reader_done: false,
                exit_code: None,
                reaped: false,
                released: false,
                group_lost: false,
            }),
            changed: Condvar::new(),
            tree: Mutex::new(procs::Tree::default()),
            scans: Arc::clone(&self.scans),
            born: Instant::now(),
            stopping: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            reader: Mutex::new(None),
            reading: AtomicBool::new(false),
            tail: self.tail_cap.map(|cap| Mutex::new(TailRing::new(cap))),
            meta: Mutex::new(Vec::new()),
        });
        registry.panes.push(Arc::clone(&pane));
        drop(registry);
        // The reaper takes the child from this slot, so if its thread cannot
        // start, the child is still here to be reaped (review SOL-B2).
        let child_slot = Arc::new(Mutex::new(Some(child)));
        let reaper = {
            let pane = Arc::clone(&pane);
            let child_slot = Arc::clone(&child_slot);
            let sink = Arc::clone(&sink);
            spawn_thread(format!("native-reap-{}", pane.id), move || {
                let child = child_slot
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(child) = child {
                    reap(pane, child, slave, sink);
                }
            })
        };
        let reader = reaper.and_then(|_| {
            let pane = Arc::clone(&pane);
            spawn_thread(format!("native-read-{}", pane.id), move || {
                read_loop(pane, sink);
            })
        });
        match reader {
            Ok(handle) => {
                if let Ok(mut slot) = pane.reader.lock() {
                    *slot = Some(handle);
                }
                Ok(Spawned { id, pid })
            }
            Err(err) => {
                pane.stopping.store(true, Ordering::Release);
                pane.shutdown.store(true, Ordering::Release);
                pane.release();
                // No reaper took the child: reap the killed leader here.
                let orphan = child_slot
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(mut child) = orphan {
                    {
                        let mut st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
                        st.live = false;
                        st.exited = true;
                        st.group_lost = true;
                    }
                    let _ = child.wait();
                    pane.state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .reaped = true;
                    pane.changed.notify_all();
                }
                self.registry().panes.retain(|p| !Arc::ptr_eq(p, &pane));
                Err(BackendError::new(
                    ErrorKind::Other,
                    format!("native: cannot start the pane's threads: {err}"),
                ))
            }
        }
    }

    fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        pty::try_write(pane.master.as_raw_fd(), bytes)
    }

    fn wait_writable(&self, id: &str, timeout: Duration) -> io::Result<bool> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        pty::poll_writable(pane.master.as_raw_fd(), timeout)
    }

    fn resize(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        pty::set_winsize(pane.master.as_raw_fd(), cols, rows)?;
        *pane.size.lock().unwrap_or_else(PoisonError::into_inner) = (cols, rows);
        Ok(())
    }

    fn signal(&self, id: &str, signal: i32) -> io::Result<()> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        let st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
        if st.group_lost || st.reaped {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the pane's group is no longer its own",
            ));
        }
        pty::killpg(pane.pid, signal)
    }

    /// SIGHUP and SIGTERM to the pane's group and every descendant, up to
    /// `grace` for the leader and descendants to exit, then SIGKILL to the
    /// group and every descendant left (ADR §11). The leader is reaped and
    /// its exit reported before this returns. No key is sent first.
    fn kill(&self, id: &str, grace: Duration) -> Option<KillOutcome> {
        let pane = self.find(id)?;
        pane.stopping.store(true, Ordering::Release);
        pane.soft_stop();
        let deadline = Instant::now() + grace;
        pane.await_exit(deadline);
        let killed = !pane.has_exited();
        pane.await_tree_gone(deadline);
        pane.release();
        // The reaper records the exit code (128 + SIGKILL when killed) and
        // reaps; the reader drains the terminal until then.
        pane.await_reaped(Instant::now() + KILL_GRACE);
        pane.shutdown.store(true, Ordering::Release);
        let reader = pane.reader.lock().ok().and_then(|mut slot| slot.take());
        if let Some(reader) = reader {
            let _ = reader.join();
        }
        self.registry().panes.retain(|p| !Arc::ptr_eq(p, &pane));
        Some(KillOutcome { killed })
    }

    fn fg_pgrp(&self, id: &str) -> Option<i32> {
        let pane = self.find(id)?;
        pty::tcgetpgrp(pane.master.as_raw_fd())
    }

    fn list(&self) -> Vec<HostPane> {
        let panes = self.registry().panes.clone();
        panes
            .iter()
            .map(|pane| {
                let (cols, rows) = *pane.size.lock().unwrap_or_else(PoisonError::into_inner);
                let st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
                HostPane {
                    id: pane.id.clone(),
                    pid: pane.pid,
                    cols,
                    rows,
                    exited: st.exited,
                    finished: !st.live,
                    exit_code: st.exit_code,
                }
            })
            .collect()
    }

    fn forget(&self, id: &str) {
        if let Some(pane) = self.take(id) {
            pane.stopping.store(true, Ordering::Release);
            pane.release();
        }
    }

    /// SIGHUP and SIGTERM to every pane's group and descendants, one wait of
    /// up to 2 s for all of them, then kill what is left of each pane's tree
    /// and wait (up to 2 s more) for the leaders to be reaped.
    fn end_all(&self) {
        let panes = std::mem::take(&mut self.registry().panes);
        for pane in &panes {
            pane.stopping.store(true, Ordering::Release);
            pane.soft_stop();
        }
        let deadline = Instant::now() + KILL_GRACE;
        for pane in &panes {
            pane.await_exit(deadline);
        }
        for pane in &panes {
            pane.await_tree_gone(deadline);
        }
        for pane in &panes {
            pane.release();
        }
        let deadline = Instant::now() + KILL_GRACE;
        for pane in &panes {
            pane.await_reaped(deadline);
            pane.shutdown.store(true, Ordering::Release);
        }
    }

    fn attach(&self, id: &str) -> io::Result<Attached> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        let tail = pane
            .tail
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "this host keeps no tail"))?;
        let ring = tail.lock().unwrap_or_else(PoisonError::into_inner);
        let tail_start = ring.start();
        Ok(Attached {
            tail_start,
            tail_end: ring.end(),
            bytes: ring.read_from(tail_start).unwrap_or_default(),
        })
    }

    fn meta_set(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        if bytes.len() > META_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("meta of {} bytes exceeds {META_CAP}", bytes.len()),
            ));
        }
        *pane.meta.lock().unwrap_or_else(PoisonError::into_inner) = bytes.to_vec();
        Ok(())
    }

    fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        let meta = pane.meta.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(meta.clone())
    }

    fn survives_restart(&self) -> bool {
        false
    }
}

impl HolderHost for LocalHost {
    fn tail_read(&self, id: &str, from: u64, max: usize) -> io::Result<TailRead> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        let tail = pane
            .tail
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "this host keeps no tail"))?;
        let ring = tail.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(match ring.read_chunk(from, max) {
            Ok(bytes) => TailRead::Bytes(bytes),
            Err(start) => TailRead::Dropped { start },
        })
    }

    fn tail_bounds(&self, id: &str) -> io::Result<(u64, u64)> {
        let pane = self.find(id).ok_or_else(|| Self::unknown(id))?;
        let tail = pane
            .tail
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "this host keeps no tail"))?;
        let ring = tail.lock().unwrap_or_else(PoisonError::into_inner);
        Ok((ring.start(), ring.end()))
    }
}

impl Proc {
    fn has_exited(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .exited
    }

    /// Output is still on its way to the sink: bytes wait in the terminal, or
    /// the reader holds a chunk it has not handed over. The queue is read
    /// first: the reader marks itself busy before it takes bytes out of the
    /// queue and clears the mark after the sink has them, so bytes that were
    /// queued when the queue was read are either still queued or covered by
    /// the mark read after it. Output a descendant writes later may be missed.
    fn output_pending(&self) -> bool {
        pty::queued(self.master.as_raw_fd()) > 0 || self.reading.load(Ordering::SeqCst)
    }

    fn is_live(&self) -> bool {
        self.state.lock().is_ok_and(|st| st.live)
    }

    /// Whether the group id is still reserved for this pane: the leader is
    /// not reaped yet (it runs, or is a zombie). Only then may Shepherd
    /// signal the group or look up the group and session by the leader's pid
    /// (POSIX does not reuse a pid while its process exists).
    pub(crate) fn owns_group(&self) -> bool {
        let st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        !st.group_lost && !st.reaped
    }

    /// Send `signals` to the group if it is still ours, under `state`, so the
    /// reservation cannot end between the check and the kill. Whether it was.
    fn signal_group(&self, signals: &[libc::c_int]) -> bool {
        let st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if st.group_lost || st.reaped {
            return false;
        }
        for &signal in signals {
            self.signal(signal, st.live);
        }
        true
    }

    /// SIGHUP and SIGTERM to the group and to every known descendant, after
    /// a fresh look for descendants. No key is typed (ADR §11).
    fn soft_stop(&self) {
        let mut tree = self.tree.lock().unwrap_or_else(PoisonError::into_inner);
        if self.owns_group() {
            tree.refresh(self.pid);
        }
        self.signal_group(&[libc::SIGHUP, libc::SIGTERM]);
        tree.signal_all(libc::SIGHUP);
        tree.signal_all(libc::SIGTERM);
    }

    /// Wait until no known descendant runs, or `deadline` passes.
    fn await_tree_gone(&self, deadline: Instant) {
        loop {
            {
                let mut tree = self.tree.lock().unwrap_or_else(PoisonError::into_inner);
                if self.owns_group() {
                    tree.refresh(self.pid);
                }
                if !tree.any_alive() {
                    return;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            thread::sleep(left.min(TREE_WAIT_POLL));
        }
    }

    /// End the pane's processes for good: look for descendants and SIGSTOP
    /// them and the group, again until a look finds nobody new (a stopped
    /// process cannot fork), then SIGKILL the group and every descendant.
    /// After this the reaper reaps the leader, and the group id is no longer
    /// reserved. Idempotent.
    pub(crate) fn release(&self) {
        let mut tree = self.tree.lock().unwrap_or_else(PoisonError::into_inner);
        for round in 1..=FREEZE_ROUNDS {
            let owned = self.owns_group();
            let added = if owned { tree.refresh(self.pid) } else { 0 };
            self.signal_group(&[libc::SIGSTOP]);
            tree.signal_all(libc::SIGSTOP);
            if !owned || (round > 1 && added == 0) {
                break;
            }
        }
        self.signal_group(&[libc::SIGKILL]);
        tree.signal_all(libc::SIGKILL);
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .released = true;
        drop(tree);
        self.changed.notify_all();
    }

    /// Signal the group. EPERM once the leader has exited is not logged: on
    /// macOS a group left with only its zombie leader answers EPERM.
    fn signal(&self, signal: libc::c_int, live: bool) {
        match pty::killpg(self.pid, signal) {
            Err(err) if !live && err.raw_os_error() == Some(libc::EPERM) => {}
            Err(err) => eprintln!("openab-shepherd: native: {}: kill failed: {err}", self.id),
            Ok(()) => {}
        }
    }

    /// Wait until the reaper has reaped the leader or `deadline` passes.
    fn await_reaped(&self, deadline: Instant) {
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        while !st.reaped && !st.group_lost {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            st = match self.changed.wait_timeout(st, left) {
                Ok((next, _)) => next,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    /// Wait until the leader has exited or `deadline` passes.
    fn await_exit(&self, deadline: Instant) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        while !st.exited {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.changed.wait_timeout(st, left) {
                Ok((next, _)) => st = next,
                Err(_) => return,
            }
        }
    }
}

/// The reader thread: output to the sink until EOF, EIO or removal, and every
/// `TREE_POLL` a fresh look for the pane's descendants.
fn read_loop(pane: Arc<Proc>, sink: SinkCell) {
    let fd = pane.master.as_raw_fd();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut next_tree = Instant::now();
    // The offset of the first byte of the next chunk; counted, not stored.
    let mut seq: u64 = 0;
    while !pane.shutdown.load(Ordering::Acquire) {
        match pty::poll_read(fd, &mut buf, READ_POLL_MS, || {
            pane.reading.store(true, Ordering::SeqCst);
        }) {
            Ok(None) => {
                if let Some(sink) = current(&sink) {
                    sink.idle(&pane.id);
                }
            }
            Ok(Some(0)) => break,
            Ok(Some(n)) => {
                // The tail first, so a front that attaches after hearing of
                // this chunk finds it there.
                if let Some(tail) = &pane.tail {
                    tail.lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(&buf[..n]);
                }
                if current(&sink).is_some_and(|sink| !sink.output(&pane.id, seq, &buf[..n])) {
                    break;
                }
                seq += n as u64;
            }
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            Err(err) => {
                eprintln!("openab-shepherd: native: {}: read failed: {err}", pane.id);
                break;
            }
        }
        pane.reading.store(false, Ordering::SeqCst);
        if Instant::now() >= next_tree {
            // try_lock: a stop holds the tree, and the reader must keep
            // draining the terminal meanwhile. The snapshot is shared with
            // the other panes' readers; a stop takes its own fresh scan.
            if let Ok(mut tree) = pane.tree.try_lock() {
                if pane.is_live() && pane.owns_group() {
                    let _ = tree.refresh_shared(pane.pid, &pane.scans, pane.born);
                }
            }
            next_tree = Instant::now() + TREE_POLL;
        }
    }
    pane.reading.store(false, Ordering::SeqCst);
    if let Ok(mut st) = pane.state.lock() {
        st.reader_done = true;
    }
    pane.changed.notify_all();
}

/// The reaper thread: waits for the leader to exit without reaping it (the
/// zombie keeps the group and session ids ours) and records the exit. Then
/// it ends what the leader left behind: on its own exit, SIGHUP and SIGTERM
/// to the group and descendants, up to 2 s, and `release`; during a removal
/// or shutdown, it waits for that to `release`. Then it reaps the leader,
/// under the tree lock, so no look for descendants runs on a freed pid.
fn reap(pane: Arc<Proc>, mut child: Child, slave: OwnedFd, sink: SinkCell) {
    let (code, reaped) = match pty::wait_exited(pane.pid) {
        Ok(code) => (Some(code), false),
        Err(err) => {
            // Not expected; fall back to a plain wait, which gives up the id.
            // Marked lost under `state` first, so no signal can reach the
            // group once the id may belong to another process (review N9).
            eprintln!("openab-shepherd: native: {}: waitid failed: {err}", pane.id);
            pane.state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .group_lost = true;
            let code = child.wait().ok().and_then(|status| {
                status
                    .code()
                    .map(i64::from)
                    .or_else(|| status.signal().map(|signal| 128 + i64::from(signal)))
            });
            (code, true)
        }
    };
    // The leader has exited: the parent's slave copy is no longer needed.
    drop(slave);
    let mut st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
    st.exited = true;
    pane.changed.notify_all();
    // Wait for the reader to take what the leader left. It is done at the
    // end of file; or, once `EXIT_DRAIN` has passed, when no output is on its
    // way to the sink; or at the hard cap. So a reader that is slow, or a
    // sink that is, gets time while it makes progress, and a descendant that
    // holds the terminal open in silence costs `EXIT_DRAIN`.
    let start = Instant::now();
    while !st.reader_done {
        let elapsed = start.elapsed();
        if elapsed >= EXIT_DRAIN_CAP || (elapsed >= EXIT_DRAIN && !pane.output_pending()) {
            break;
        }
        let left = EXIT_DRAIN_CAP.saturating_sub(elapsed).min(EXIT_DRAIN_POLL);
        st = match pane.changed.wait_timeout(st, left) {
            Ok((next, _)) => next,
            Err(poisoned) => poisoned.into_inner().0,
        };
    }
    st.live = false;
    st.exit_code = code;
    drop(st);
    // After the drain: every `output` of this pane has been delivered.
    if let Some(sink) = current(&sink) {
        sink.exited(&pane.id, code);
    }
    pane.changed.notify_all();
    if pane.stopping.load(Ordering::Acquire) {
        let mut st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
        while !st.released {
            st = match pane.changed.wait_timeout(st, Duration::from_secs(1)) {
                Ok((next, _)) => next,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    } else {
        pane.soft_stop();
        pane.await_tree_gone(Instant::now() + KILL_GRACE);
        pane.release();
    }
    let tree = pane.tree.lock().unwrap_or_else(PoisonError::into_inner);
    let mut st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
    if !reaped {
        let _ = child.wait();
    }
    st.reaped = true;
    drop(st);
    drop(tree);
    pane.changed.notify_all();
}

/// Start a named thread. In tests, `FAIL_NEXT_THREAD` makes the next start
/// on this thread fail, dropping `f` as a failed spawn does.
fn spawn_thread(name: String, f: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    #[cfg(test)]
    if FAIL_NEXT_THREAD.with(|fail| fail.replace(false)) {
        drop(f);
        return Err(io::Error::other("injected thread start failure"));
    }
    thread::Builder::new().name(name).spawn(f)
}
