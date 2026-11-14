//! Shepherd's own terminal core: a PTY per pane, a VT emulator over its
//! output, and the OSC 7501 program status read from that same emulator.
//! See docs/adr/native-core.md.
//!
//! Locking (ADR §9): the pane table, then a pane's `input`, then its `state`;
//! a pane's `tree` is taken before its `state` and never with `input`.
//! Nothing takes `input` while holding `state`, and no lock is held across a
//! sleep, a poll or a wait, except the condvar wait, which releases `state`.
//! The reader and reaper threads hold only their pane, so dropping the last
//! [`NativeBackend`] handle ends every pane.

pub(crate) mod keys;
mod procs;
mod pty;
pub(crate) mod status;
pub(crate) mod term;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_output;

pub use keys::KeyboardMode;
pub(crate) use pty::{default_signals_on_exec, effective_uid, SPAWN_LOCK};
pub use status::LogEntry as StatusLogEntry;

use crate::backend::{
    dialog_kind, BackendError, Capabilities, ErrorKind, InputGuard, InterruptOutcome,
    InterruptResult, Key, KeysResult, ReadLimit, ReadSource, RemoveResult, RunSpec, ScreenCheck,
    ScreenText, SendResult, State, StopHow, TerminalBackend, TerminalInfo, TerminalList,
    TerminalRef, WaitReason, WaitResult,
};
use crate::input::MAX_INPUT_BYTES;
use keys::encode as encode_key;
use status::{LogEntry, Report, StatusLog, Tracker};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use term::Emulator;

// Bounds and defaults (ADR §13). The screen size and scrollback are
// `term::{ROWS, COLS, SCROLLBACK}`, the status log length is
// `status::STATUS_LOG_LEN`.
const MAX_LIVE_PANES: usize = 8;
const MAX_FINISHED_PANES: usize = 32;
const READ_CHUNK: usize = 16 * 1024;
const RAW_RING: usize = 256 * 1024;
const PENDING_REPLIES: usize = 4 * 1024;
const MAX_ARGV: usize = 256;
pub(crate) const MAX_EXTRA_ENV: usize = 32;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SETTLE_QUIET: Duration = Duration::from_millis(100);
const SETTLE_MAX: Duration = Duration::from_secs(1);
const START_WAIT: Duration = Duration::from_secs(2);
const READ_SYNC_WAIT: Duration = Duration::from_millis(150);
const KILL_GRACE: Duration = Duration::from_secs(2);
const INTERRUPT_DEFAULT: Duration = Duration::from_secs(10);
/// How often the reader wakes to check for removal.
const READ_POLL_MS: libc::c_int = 200;
/// After the leader exits, how long the reaper waits for the reader to take
/// the last output, so a read after `wait` returns `exit` shows it.
const EXIT_DRAIN: Duration = Duration::from_millis(500);
/// How often interrupt re-reads the foreground group.
const INTERRUPT_POLL: Duration = Duration::from_millis(50);
/// How often the reader refreshes the pane's process tree while the leader
/// runs, so a descendant is known before its parent can exit (ADR §11).
const TREE_POLL: Duration = Duration::from_millis(250);
/// How often a stop re-checks whether the pane's descendants have exited.
const TREE_WAIT_POLL: Duration = Duration::from_millis(50);
/// At most this many refresh-and-stop rounds before the final SIGKILL.
const FREEZE_ROUNDS: usize = 8;

const CAPABILITIES: Capabilities = Capabilities {
    conditional_input: true,
    conditional_keys: true,
    client_command: true,
    client_cwd: true,
    client_env: true,
    screen_text: true,
    program_status: true,
    history: true,
    native_wait: true,
    survives_restart: false,
};

/// Settings fixed when the backend is built.
#[derive(Debug, Clone, Default)]
pub struct NativeConfig {
    /// Extra variables for every child, set by code only (tests, the probe).
    /// Keys match `[A-Z_][A-Z0-9_]*`, values have no NUL; together with a
    /// run's own `RunSpec.env`, at most 32.
    pub extra_env: Vec<(OsString, OsString)>,
}

type EnvLookup = Box<dyn Fn(&str) -> Option<OsString> + Send + Sync>;

/// The native backend. A cheap handle: clones share the same panes.
#[derive(Clone)]
pub struct NativeBackend {
    inner: Arc<Inner>,
}

struct Inner {
    table: Mutex<Table>,
    config: NativeConfig,
    /// Reads the parent's environment for the whitelist.
    env: EnvLookup,
}

#[derive(Default)]
struct Table {
    next_id: u64,
    /// In creation order.
    panes: Vec<Arc<Pane>>,
    /// `end_all` has taken the panes: a child spawned after that is ended
    /// at once instead of entering the table.
    exiting: bool,
}

struct Pane {
    id: String,
    name: Option<String>,
    argv: Vec<String>,
    cwd: String,
    /// The leader's pid, which is also the group id.
    pid: i32,
    master: OwnedFd,
    state: Mutex<PaneState>,
    changed: Condvar,
    /// Held for a whole write sequence (send, keys, interrupt, reply flush).
    input: Mutex<()>,
    /// Every descendant seen so far (ADR §11). Taken before `state`.
    tree: Mutex<procs::Tree>,
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
}

struct PaneState {
    emu: Emulator,
    tracker: Tracker,
    live: bool,
    /// The leader has exited: set as soon as its status is read, before the
    /// reaper waits for the reader to drain, so a removal decides `killed`
    /// from when the leader exited, not from when `live` was cleared.
    exited: bool,
    exit_code: Option<i64>,
    reader_done: bool,
    /// The turn of a send that has not gone working yet.
    pending_send: Option<u64>,
    pending_replies: Vec<u8>,
    replies_overflowed: bool,
    raw: VecDeque<u8>,
    last_output: Instant,
    log: StatusLog,
    /// The leader has been reaped: its pid, so the group and session ids,
    /// are no longer reserved for this pane (ADR §11).
    reaped: bool,
    /// The pane's group and descendants were killed for good (`release`);
    /// the reaper reaps the leader only after this.
    released: bool,
    /// The reaper fell back to a plain wait, which frees the group id: the
    /// group is never signalled again.
    group_lost: bool,
}

impl NativeBackend {
    pub fn new(config: NativeConfig) -> Self {
        Self::with_env(config, Box::new(|key| std::env::var_os(key)))
    }

    /// A backend whose children see `lookup` as the parent environment.
    #[cfg(test)]
    pub(crate) fn with_env_lookup(
        config: NativeConfig,
        lookup: impl Fn(&str) -> Option<OsString> + Send + Sync + 'static,
    ) -> Self {
        Self::with_env(config, Box::new(lookup))
    }

    fn with_env(config: NativeConfig, env: EnvLookup) -> Self {
        Self {
            inner: Arc::new(Inner {
                table: Mutex::new(Table::default()),
                config,
                env,
            }),
        }
    }

    /// End every pane now, as dropping the last handle does: what Shepherd
    /// runs before it exits on a signal.
    pub fn shutdown(&self) {
        self.inner.end_all();
    }

    /// On SIGINT, SIGTERM or SIGHUP: end every pane, then exit the process
    /// with 128 + the signal. Call before starting any other thread, so they
    /// all inherit the blocked signals.
    pub fn end_panes_on_exit_signals(&self) -> io::Result<()> {
        let set = pty::block_exit_signals()?;
        let backend = self.clone();
        let started = thread::Builder::new()
            .name("native-signals".to_string())
            .spawn(move || {
                let signal = pty::wait_exit_signal(&set);
                eprintln!("openab-shepherd: signal {signal}: ending native panes");
                backend.shutdown();
                std::process::exit(128 + signal);
            });
        if let Err(err) = started {
            pty::unblock_signals(&set);
            return Err(err);
        }
        Ok(())
    }

    /// The pane's last OSC 7501 reports, oldest first (tests and the probe).
    pub fn status_log(&self, t: &TerminalRef) -> Result<Vec<StatusLogEntry>, BackendError> {
        let pane = self.inner.resolve(t)?;
        let st = pane.lock_state()?;
        Ok(st.log.entries())
    }

    /// DECCKM and the active screen's kitty flags (the probe).
    pub fn keyboard_mode(&self, t: &TerminalRef) -> Result<KeyboardMode, BackendError> {
        let pane = self.inner.resolve(t)?;
        let st = pane.lock_state()?;
        Ok(st.emu.mode())
    }

    /// The last 256 KiB the program wrote, raw (diagnostics and the probe).
    pub fn raw_output(&self, t: &TerminalRef) -> Result<Vec<u8>, BackendError> {
        let pane = self.inner.resolve(t)?;
        let st = pane.lock_state()?;
        Ok(st.raw.iter().copied().collect())
    }

    fn cwd_for(&self, cwd: Option<&str>) -> Result<String, BackendError> {
        let Some(cwd) = cwd else {
            return Ok((self.inner.env)("HOME")
                .filter(|home| !home.is_empty())
                .map_or_else(
                    || "/".to_string(),
                    |home| home.to_string_lossy().into_owned(),
                ));
        };
        let bad = || invalid("native: cwd must be an absolute existing directory");
        if !Path::new(cwd).is_absolute() {
            return Err(bad());
        }
        match std::fs::metadata(cwd) {
            Ok(meta) if meta.is_dir() => Ok(cwd.to_string()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Err(BackendError::new(
                ErrorKind::NotFound,
                "native: cwd not found",
            )),
            _ => Err(bad()),
        }
    }

    /// Record a spawned child and start its reader and reaper.
    fn insert(
        &self,
        spec: &RunSpec,
        argv: Vec<String>,
        cwd: String,
        spawned: pty::Spawned,
    ) -> Result<Arc<Pane>, BackendError> {
        let pty::Spawned {
            master,
            slave,
            mut child,
        } = spawned;
        let pid = child.id() as i32;
        #[cfg(test)]
        tests::LAST_SPAWN_PID.with(|last| last.set(pid));
        let mut dropped: Vec<Arc<Pane>> = Vec::new();
        let pane = {
            let mut table = self.inner.table()?;
            let room = if table.exiting {
                Err(BackendError::new(
                    ErrorKind::Other,
                    "native: Shepherd is exiting",
                ))
            } else {
                check_room(&table, spec.name.as_deref())
            };
            if let Err(err) = room {
                drop(table);
                let _ = pty::killpg(pid, libc::SIGKILL);
                let _ = child.wait();
                return Err(err);
            }
            if let Some(name) = spec.name.as_deref() {
                let (same, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut table.panes)
                    .into_iter()
                    .partition(|p| p.name.as_deref() == Some(name) && !p.is_live());
                table.panes = rest;
                dropped = same;
            }
            table.next_id += 1;
            let pane = Arc::new(Pane {
                id: format!("n:{}", table.next_id),
                name: spec.name.clone(),
                argv,
                cwd,
                pid,
                master,
                state: Mutex::new(PaneState {
                    emu: Emulator::new(),
                    tracker: Tracker::default(),
                    live: true,
                    exited: false,
                    exit_code: None,
                    reader_done: false,
                    pending_send: None,
                    pending_replies: Vec::new(),
                    replies_overflowed: false,
                    raw: VecDeque::new(),
                    last_output: Instant::now(),
                    log: StatusLog::default(),
                    reaped: false,
                    released: false,
                    group_lost: false,
                }),
                changed: Condvar::new(),
                input: Mutex::new(()),
                tree: Mutex::new(procs::Tree::default()),
                stopping: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                reader: Mutex::new(None),
            });
            table.panes.push(Arc::clone(&pane));
            dropped.extend(evict_finished(&mut table));
            pane
        };
        // Finished panes leave the table only with their group ended.
        for old in dropped {
            old.stopping.store(true, Ordering::Release);
            old.release();
        }
        // The reaper takes the child from this slot, so if its thread cannot
        // start, the child is still here to be reaped (review SOL-B2).
        let child_slot = Arc::new(Mutex::new(Some(child)));
        let reaper = {
            let pane = Arc::clone(&pane);
            let child_slot = Arc::clone(&child_slot);
            spawn_thread(format!("native-reap-{}", pane.id), move || {
                let child = child_slot
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(child) = child {
                    reap(pane, child, slave);
                }
            })
        };
        let reader = reaper.and_then(|_| {
            let pane = Arc::clone(&pane);
            spawn_thread(format!("native-read-{}", pane.id), move || read_loop(pane))
        });
        match reader {
            Ok(handle) => {
                if let Ok(mut slot) = pane.reader.lock() {
                    *slot = Some(handle);
                }
                Ok(pane)
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
                self.inner.table()?.panes.retain(|p| !Arc::ptr_eq(p, &pane));
                Err(BackendError::new(
                    ErrorKind::Other,
                    format!("native: cannot start the pane's threads: {err}"),
                ))
            }
        }
    }
}

impl Inner {
    fn table(&self) -> Result<MutexGuard<'_, Table>, BackendError> {
        self.table.lock().map_err(|_| {
            BackendError::new(
                ErrorKind::Other,
                "native: the pane table is inconsistent after a panic",
            )
        })
    }

    /// An id first, then a name: the live pane, else the most recent finished.
    fn resolve(&self, t: &TerminalRef) -> Result<Arc<Pane>, BackendError> {
        let table = self.table()?;
        let wanted = t.as_str();
        if let Some(pane) = table.panes.iter().find(|p| p.id == wanted) {
            return Ok(Arc::clone(pane));
        }
        let named = || {
            table
                .panes
                .iter()
                .filter(|p| p.name.as_deref() == Some(wanted))
        };
        named()
            .find(|p| p.is_live())
            .or_else(|| named().next_back())
            .cloned()
            .ok_or_else(|| BackendError::new(ErrorKind::NotFound, "native: no such pane"))
    }
}

impl Inner {
    /// SIGHUP and SIGTERM to every pane's group and descendants, one wait of
    /// up to 2 s for all of them, then kill what is left of each pane's tree
    /// and wait (up to 2 s more) for the leaders to be reaped.
    fn end_all(&self) {
        let panes = {
            let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
            table.exiting = true;
            std::mem::take(&mut table.panes)
        };
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
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.end_all();
    }
}

/// Refuse a run whose name is live, or beyond the live-pane limit.
fn check_room(table: &Table, name: Option<&str>) -> Result<(), BackendError> {
    let live: Vec<&Arc<Pane>> = table.panes.iter().filter(|p| p.is_live()).collect();
    if name.is_some() && live.iter().any(|p| p.name.as_deref() == name) {
        return Err(BackendError::new(
            ErrorKind::NameTaken,
            "native: a live pane already has that name",
        ));
    }
    if live.len() >= MAX_LIVE_PANES {
        return Err(BackendError::new(
            ErrorKind::Other,
            format!("native: {MAX_LIVE_PANES} panes are running; the limit is {MAX_LIVE_PANES}"),
        ));
    }
    Ok(())
}

/// Take the oldest finished panes beyond the retention limit out of the
/// table; the caller ends their groups.
fn evict_finished(table: &mut Table) -> Vec<Arc<Pane>> {
    let mut excess = table
        .panes
        .iter()
        .filter(|p| !p.is_live())
        .count()
        .saturating_sub(MAX_FINISHED_PANES);
    let mut evicted = Vec::new();
    table.panes.retain(|p| {
        if excess > 0 && !p.is_live() {
            excess -= 1;
            evicted.push(Arc::clone(p));
            return false;
        }
        true
    });
    evicted
}

impl Pane {
    fn lock_state(&self) -> Result<MutexGuard<'_, PaneState>, BackendError> {
        self.state.lock().map_err(|_| self.poisoned())
    }

    fn lock_input(&self) -> Result<MutexGuard<'_, ()>, BackendError> {
        self.input.lock().map_err(|_| self.poisoned())
    }

    fn wait_changed<'a>(
        &self,
        st: MutexGuard<'a, PaneState>,
        timeout: Duration,
    ) -> Result<MutexGuard<'a, PaneState>, BackendError> {
        self.changed
            .wait_timeout(st, timeout)
            .map(|(st, _)| st)
            .map_err(|_| self.poisoned())
    }

    fn poisoned(&self) -> BackendError {
        BackendError::new(
            ErrorKind::Other,
            format!(
                "native: {}: pane state is inconsistent after a panic",
                self.id
            ),
        )
    }

    fn is_live(&self) -> bool {
        self.state.lock().is_ok_and(|st| st.live)
    }

    fn has_exited(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .exited
    }

    /// Whether the group id is still reserved for this pane: the leader is
    /// not reaped yet (it runs, or is a zombie). Only then may Shepherd
    /// signal the group or look up the group and session by the leader's pid
    /// (POSIX does not reuse a pid while its process exists).
    fn owns_group(&self) -> bool {
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
    fn release(&self) {
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

    fn info(&self) -> Result<TerminalInfo, BackendError> {
        let st = self.lock_state()?;
        Ok(TerminalInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            live: st.live,
            exit_code: st.exit_code,
            status: st.tracker.program_status(),
            reported: st.tracker.reported(),
            turn: Some(st.tracker.turn()),
            status_ms: st.tracker.status_ms(),
            argv: Some(self.argv.clone()),
            cwd: Some(self.cwd.clone()),
        })
    }

    /// Take one chunk of output (reader thread). False when the state is
    /// poisoned and the reader should stop.
    fn ingest(&self, chunk: &[u8]) -> bool {
        let Ok(mut guard) = self.state.lock() else {
            return false;
        };
        let st = &mut *guard;
        st.raw.extend(chunk);
        let excess = st.raw.len().saturating_sub(RAW_RING);
        st.raw.drain(..excess);
        st.emu.process(chunk);
        let now_ms = now_ms();
        for (body, report) in st.emu.take_reports() {
            let outcome = match report {
                Report::Status { .. } if !st.live => "ignored: the program has ended".to_string(),
                Report::Status { state_word, kind } => {
                    let state = State::parse(&state_word);
                    let working = state == State::Working;
                    let mut outcome = state_word;
                    if let Some(kind) = &kind {
                        outcome = format!("{outcome} kind={kind}");
                    }
                    let applied = st.tracker.apply(state, kind, now_ms);
                    if working || applied.turn_changed {
                        st.pending_send = None;
                    }
                    if !applied.status_changed {
                        outcome.push_str(" (unchanged)");
                    }
                    outcome
                }
                Report::Query => "query".to_string(),
                Report::Discard(why) => format!("discarded: {why}"),
            };
            st.log.push(LogEntry {
                at_ms: now_ms,
                body,
                outcome,
                turn_after: st.tracker.turn(),
            });
        }
        let replies = st.emu.take_replies();
        if st.pending_replies.len() + replies.len() <= PENDING_REPLIES {
            st.pending_replies.extend_from_slice(&replies);
        } else if !st.replies_overflowed {
            st.replies_overflowed = true;
            eprintln!(
                "openab-shepherd: native: {}: query replies dropped; the program is not reading input",
                self.id
            );
        }
        st.last_output = Instant::now();
        drop(guard);
        self.changed.notify_all();
        true
    }

    /// Write what fits of the pending query replies. Call with `input` held.
    fn flush_replies(&self) {
        let Ok(mut st) = self.state.lock() else {
            return;
        };
        if st.pending_replies.is_empty() {
            return;
        }
        if let Ok(n) = pty::try_write(self.master.as_raw_fd(), &st.pending_replies) {
            st.pending_replies.drain(..n);
        }
    }

    /// Check `guard` (or only liveness, when None) and write the bytes
    /// `bytes_for` gives, in one critical section on `state`, so no status
    /// change lands between the check and the bytes entering the PTY. Call
    /// with `input` held.
    fn write_guarded(
        &self,
        guard: Option<&InputGuard>,
        mut bytes_for: impl FnMut(&PaneState) -> Result<Vec<u8>, BackendError>,
    ) -> Result<(), BackendError> {
        let fd = self.master.as_raw_fd();
        let deadline = Instant::now() + WRITE_TIMEOUT;
        loop {
            let (bytes, written) = {
                let mut st = self.lock_state()?;
                match guard {
                    Some(guard) => check_guard(&st, guard, &self.id)?,
                    None if !st.live => return Err(finished(&self.id)),
                    None => {}
                }
                let bytes = bytes_for(&st)?;
                // Query replies go first and whole, so caller bytes never
                // land inside a half-written reply.
                let replies_left = if st.pending_replies.is_empty() {
                    false
                } else {
                    match pty::try_write(fd, &st.pending_replies) {
                        Ok(n) => {
                            st.pending_replies.drain(..n);
                        }
                        Err(err)
                            if matches!(
                                err.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) => {}
                        Err(err) => {
                            return Err(BackendError::new(
                                ErrorKind::Other,
                                format!("native: {}: write failed: {err}", self.id),
                            ))
                        }
                    }
                    !st.pending_replies.is_empty()
                };
                if replies_left {
                    (bytes, 0)
                } else {
                    match pty::try_write(fd, &bytes) {
                        Ok(n) => (bytes, n),
                        Err(err)
                            if matches!(
                                err.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                            ) =>
                        {
                            (bytes, 0)
                        }
                        Err(err) => {
                            return Err(BackendError::new(
                                ErrorKind::Other,
                                format!("native: {}: write failed: {err}", self.id),
                            ))
                        }
                    }
                }
            };
            if written == bytes.len() {
                return Ok(());
            }
            if written > 0 {
                return self.finish_write(&bytes[written..], deadline);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(BackendError::new(
                    ErrorKind::Timeout,
                    format!(
                        "native: {}: the program is not reading input; nothing was written",
                        self.id
                    ),
                ));
            }
            let _ = pty::poll_writable(fd, left);
        }
    }

    /// The rest of a write that has started; no further guard.
    fn finish_write(&self, mut rest: &[u8], deadline: Instant) -> Result<(), BackendError> {
        let fd = self.master.as_raw_fd();
        let partial = || {
            BackendError::new(
                ErrorKind::Other,
                format!(
                    "native: {}: input partly written before the program stopped reading",
                    self.id
                ),
            )
        };
        while !rest.is_empty() {
            match pty::try_write(fd, rest) {
                Ok(n) => rest = &rest[n..],
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(partial());
                    }
                    let _ = pty::poll_writable(fd, left);
                }
                Err(_) => return Err(partial()),
            }
        }
        Ok(())
    }

    /// Wait until the pane has been quiet for 100 ms since `pasted` (or its
    /// last output after it), for at most 1 s.
    fn settle(&self, pasted: Instant) -> Result<(), BackendError> {
        let end = pasted + SETTLE_MAX;
        let mut st = self.lock_state()?;
        loop {
            let now = Instant::now();
            let quiet_until = st.last_output.max(pasted) + SETTLE_QUIET;
            if now >= quiet_until || now >= end || !st.live {
                return Ok(());
            }
            st = self.wait_changed(st, quiet_until.min(end) - now)?;
        }
    }

    /// After a send's Enter: whether it went working within 2 s. None when the
    /// program never reported a status (then it does not wait).
    fn await_start(&self, turn: u64) -> Result<Option<bool>, BackendError> {
        let end = Instant::now() + START_WAIT;
        let mut st = self.lock_state()?;
        loop {
            if st.tracker.state() == Some(&State::Working) || st.tracker.turn() != turn {
                return Ok(Some(true));
            }
            if !st.tracker.reported() {
                return Ok(None);
            }
            if !st.live {
                return Ok(Some(false));
            }
            let now = Instant::now();
            if now >= end {
                st.pending_send = Some(turn);
                return Ok(Some(false));
            }
            st = self.wait_changed(st, end - now)?;
        }
    }

    fn observe_interrupt(
        &self,
        turn0: u64,
        fg0: Option<i32>,
        timeout: Duration,
    ) -> Result<InterruptOutcome, BackendError> {
        let end = Instant::now() + timeout;
        let mut st = self.lock_state()?;
        loop {
            if !st.live {
                return Ok(InterruptOutcome::Exited);
            }
            let reported = st.tracker.reported();
            if reported {
                if st.tracker.state() != Some(&State::Working) || st.tracker.turn() != turn0 {
                    return Ok(InterruptOutcome::Stopped);
                }
            } else if fg0 != Some(self.pid)
                && pty::tcgetpgrp(self.master.as_raw_fd()) == Some(self.pid)
            {
                return Ok(InterruptOutcome::Stopped);
            }
            let now = Instant::now();
            if now >= end {
                return Ok(if reported {
                    InterruptOutcome::StillWorking
                } else {
                    InterruptOutcome::Unknown
                });
            }
            st = self.wait_changed(st, (end - now).min(INTERRUPT_POLL))?;
        }
    }
}

/// The guard's conditions, in the ADR §9 order.
fn check_guard(st: &PaneState, guard: &InputGuard, id: &str) -> Result<(), BackendError> {
    if !st.live {
        return Err(finished(id));
    }
    let state = st.tracker.state();
    let kind = st.tracker.raw_kind().map(dialog_kind);
    if guard.refuse_when_blocked && state == Some(&State::Blocked) {
        return Err(BackendError::new(
            ErrorKind::Blocked { kind },
            format!("native: {id}: the program is blocked; nothing was written"),
        ));
    }
    let mismatch = |what: &str| {
        Err(BackendError::new(
            ErrorKind::StatusMismatch,
            format!("native: {id}: {what} did not match; nothing was written"),
        ))
    };
    if guard
        .if_state
        .as_ref()
        .is_some_and(|want| state != Some(want))
    {
        return mismatch("the state");
    }
    if guard
        .if_kind
        .as_deref()
        .is_some_and(|want| kind.as_deref() != Some(want))
    {
        return mismatch("the blocked kind");
    }
    if guard
        .expect_turn
        .is_some_and(|want| st.tracker.turn() != want)
    {
        return mismatch("the turn");
    }
    Ok(())
}

fn finished(id: &str) -> BackendError {
    BackendError::new(
        ErrorKind::Finished,
        format!("native: {id}: the program has ended"),
    )
}

fn invalid(detail: impl Into<String>) -> BackendError {
    BackendError::new(ErrorKind::InvalidInput, detail)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Why Enter was not pressed after a paste, in a few words.
fn enter_refusal(err: &BackendError) -> &'static str {
    match err.kind {
        ErrorKind::Blocked { .. } => "the program became blocked",
        ErrorKind::StatusMismatch => "the status changed during the paste",
        ErrorKind::Finished => "the program ended",
        ErrorKind::Timeout => "the program stopped reading input",
        _ => "the write failed",
    }
}

/// The reader thread: output into the emulator until EOF, EIO or removal,
/// and every `TREE_POLL` a fresh look for the pane's descendants.
fn read_loop(pane: Arc<Pane>) {
    let fd = pane.master.as_raw_fd();
    let mut buf = vec![0u8; READ_CHUNK];
    let mut next_tree = Instant::now();
    while !pane.shutdown.load(Ordering::Acquire) {
        match pty::poll_read(fd, &mut buf, READ_POLL_MS) {
            Ok(None) => {}
            Ok(Some(0)) => break,
            Ok(Some(n)) => {
                if !pane.ingest(&buf[..n]) {
                    break;
                }
            }
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            Err(err) => {
                eprintln!("openab-shepherd: native: {}: read failed: {err}", pane.id);
                break;
            }
        }
        if let Ok(_input) = pane.input.try_lock() {
            pane.flush_replies();
        }
        if Instant::now() >= next_tree {
            // try_lock: a stop holds the tree, and the reader must keep
            // draining the terminal meanwhile.
            if let Ok(mut tree) = pane.tree.try_lock() {
                if pane.is_live() && pane.owns_group() {
                    let _ = tree.refresh(pane.pid);
                }
            }
            next_tree = Instant::now() + TREE_POLL;
        }
    }
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
fn reap(pane: Arc<Pane>, mut child: Child, slave: OwnedFd) {
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
    let deadline = Instant::now() + EXIT_DRAIN;
    while !st.reader_done {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        st = match pane.changed.wait_timeout(st, left) {
            Ok((next, _)) => next,
            Err(poisoned) => poisoned.into_inner().0,
        };
    }
    st.live = false;
    st.exit_code = code;
    st.pending_send = None;
    st.tracker.on_exit();
    drop(st);
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

/// Start a named thread. In tests, `tests::FAIL_NEXT_THREAD` makes the next
/// start on this thread fail, dropping `f` as a failed spawn does.
fn spawn_thread(name: String, f: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    #[cfg(test)]
    if tests::FAIL_NEXT_THREAD.with(|fail| fail.replace(false)) {
        drop(f);
        return Err(io::Error::other("injected thread start failure"));
    }
    thread::Builder::new().name(name).spawn(f)
}

fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        [b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat()
    } else {
        text.as_bytes().to_vec()
    }
}

impl TerminalBackend for NativeBackend {
    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    fn run(&self, spec: &RunSpec) -> Result<TerminalInfo, BackendError> {
        let argv = match (&spec.argv, &spec.line) {
            (Some(argv), None) => {
                if argv.is_empty() || argv.len() > MAX_ARGV {
                    return Err(invalid(format!(
                        "native: argv must have 1 to {MAX_ARGV} elements"
                    )));
                }
                argv.clone()
            }
            (None, Some(line)) => vec!["/bin/sh".to_string(), "-c".to_string(), line.clone()],
            _ => return Err(invalid("native: give exactly one of argv or line")),
        };
        let mut extra = self.inner.config.extra_env.clone();
        // A profile's variables are the operator's (profiles.rs checked
        // them); only a caller's own variables meet the protected names.
        extra.extend(
            spec.profile_env
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value.expose()))),
        );
        extra.extend(pty::run_env(&spec.env)?);
        let env = pty::child_env(&*self.inner.env, &extra)?;
        let cwd = self.cwd_for(spec.cwd.as_deref())?;
        check_room(&*self.inner.table()?, spec.name.as_deref())?;
        let spawned = pty::spawn(&argv, Path::new(&cwd), &env)?;
        self.insert(spec, argv, cwd, spawned)?.info()
    }

    fn get(&self, t: &TerminalRef) -> Result<TerminalInfo, BackendError> {
        self.inner.resolve(t)?.info()
    }

    fn list(&self, all: bool) -> Result<TerminalList, BackendError> {
        let panes = self.inner.table()?.panes.clone();
        let mut terminals = Vec::new();
        let mut hidden = 0;
        for pane in panes {
            let info = pane.info()?;
            if all || info.live {
                terminals.push(info);
            } else {
                hidden += 1;
            }
        }
        Ok(TerminalList {
            terminals,
            hidden_finished: Some(hidden),
            live_limit: Some(MAX_LIVE_PANES as u64),
        })
    }

    fn read(&self, t: &TerminalRef, history_lines: u32) -> Result<ScreenText, BackendError> {
        let pane = self.inner.resolve(t)?;
        let end = Instant::now() + READ_SYNC_WAIT;
        let mut st = pane.lock_state()?;
        loop {
            let now = Instant::now();
            if now >= end || !st.emu.sync_open(now) {
                break;
            }
            st = pane.wait_changed(st, end - now)?;
        }
        if st.emu.alternate_screen() {
            return Ok(ScreenText {
                text: st.emu.screen_text(),
                complete: Some(history_lines == 0),
                source: Some(ReadSource::Screen),
                limit: Some(ReadLimit::Other),
            });
        }
        if history_lines == 0 {
            return Ok(ScreenText {
                text: st.emu.screen_text(),
                complete: Some(true),
                source: Some(ReadSource::Screen),
                limit: None,
            });
        }
        let (text, complete) = st.emu.history_text(history_lines as usize);
        Ok(ScreenText {
            text,
            complete: Some(complete),
            source: Some(ReadSource::History),
            limit: None,
        })
    }

    fn send(
        &self,
        t: &TerminalRef,
        text: &str,
        guard: &InputGuard,
    ) -> Result<SendResult, BackendError> {
        // Bracketed-paste safety depends on this: no ESC, so no `ESC [ 201 ~`.
        if text.chars().any(|c| c.is_control() && c != '\t') {
            return Err(invalid("native: text contains a control character"));
        }
        if text.len() > MAX_INPUT_BYTES {
            return Err(invalid(format!(
                "native: text is longer than {MAX_INPUT_BYTES} bytes"
            )));
        }
        let pane = self.inner.resolve(t)?;
        let input = pane.lock_input()?;
        pane.flush_replies();
        let mut at_send = (0, None);
        pane.write_guarded(Some(guard), |st| {
            at_send = (st.tracker.turn(), st.tracker.state().cloned());
            Ok(paste_bytes(text, st.emu.bracketed_paste()))
        })?;
        let (turn_at_send, status_at_send) = at_send;
        pane.settle(Instant::now())?;
        pane.flush_replies();
        let enter_guard = InputGuard {
            expect_turn: Some(turn_at_send),
            ..guard.clone()
        };
        pane.write_guarded(Some(&enter_guard), |st| {
            encode_key(&Key::Enter, st.emu.mode()).map(<[u8]>::to_vec)
        })
        .map_err(|err| crate::backend::pasted_unsubmitted(&pane.id, enter_refusal(&err)))?;
        drop(input);
        let started = pane.await_start(turn_at_send)?;
        Ok(SendResult {
            turn: Some(turn_at_send),
            started,
            status: status_at_send,
        })
    }

    fn keys(
        &self,
        t: &TerminalRef,
        keys: &[Key],
        guard: &InputGuard,
    ) -> Result<KeysResult, BackendError> {
        // Every key must encode before any byte is written.
        for key in keys {
            encode_key(key, KeyboardMode::default())?;
        }
        let pane = self.inner.resolve(t)?;
        let _input = pane.lock_input()?;
        pane.flush_replies();
        // One guard check and one write for all keys, so a refusal means no
        // key was written.
        if !keys.is_empty() {
            pane.write_guarded(Some(guard), |st| {
                let mode = st.emu.mode();
                let mut bytes = Vec::new();
                for key in keys {
                    bytes.extend_from_slice(encode_key(key, mode)?);
                }
                Ok(bytes)
            })?;
        }
        let st = pane.lock_state()?;
        Ok(KeysResult {
            status: st.tracker.state().cloned(),
            turn: Some(st.tracker.turn()),
        })
    }

    /// The status guard, the screen check and the write are one critical
    /// section on the pane state, so the check sees the cells the key meets.
    /// A synchronized update still open means the screen is mid-redraw: refuse.
    fn keys_if_screen(
        &self,
        t: &TerminalRef,
        keys: &[Key],
        guard: &InputGuard,
        check: ScreenCheck<'_>,
    ) -> Result<KeysResult, BackendError> {
        for key in keys {
            encode_key(key, KeyboardMode::default())?;
        }
        let pane = self.inner.resolve(t)?;
        let _input = pane.lock_input()?;
        pane.flush_replies();
        if !keys.is_empty() {
            pane.write_guarded(Some(guard), |st| {
                if st.emu.sync_open(Instant::now()) {
                    return Err(BackendError::new(
                        ErrorKind::ScreenRefused,
                        "the screen is in the middle of a synchronized update",
                    ));
                }
                check(&st.emu.styled_screen())
                    .map_err(|reason| BackendError::new(ErrorKind::ScreenRefused, reason))?;
                let mode = st.emu.mode();
                let mut bytes = Vec::new();
                for key in keys {
                    bytes.extend_from_slice(encode_key(key, mode)?);
                }
                Ok(bytes)
            })?;
        }
        let st = pane.lock_state()?;
        Ok(KeysResult {
            status: st.tracker.state().cloned(),
            turn: Some(st.tracker.turn()),
        })
    }

    fn wait(&self, t: &TerminalRef, timeout: Duration) -> Result<WaitResult, BackendError> {
        let pane = self.inner.resolve(t)?;
        let end = Instant::now() + timeout;
        let mut st = pane.lock_state()?;
        loop {
            let result = |ended, reason| WaitResult {
                ended,
                reason: Some(reason),
                status: st.tracker.program_status(),
                exit_code: st.exit_code,
                turn: Some(st.tracker.turn()),
            };
            if !st.live {
                return Ok(result(true, WaitReason::Exit));
            }
            // A program that never reported counts as working until it exits;
            // an unstarted send keeps the wait on its turn.
            let working = !st.tracker.reported() || st.tracker.state() == Some(&State::Working);
            if !working && st.pending_send.is_none() {
                return Ok(result(true, WaitReason::Status));
            }
            let now = Instant::now();
            if now >= end {
                return Ok(result(false, WaitReason::Timeout));
            }
            st = pane.wait_changed(st, end - now)?;
        }
    }

    fn interrupt(
        &self,
        t: &TerminalRef,
        timeout: Option<Duration>,
    ) -> Result<InterruptResult, BackendError> {
        let pane = self.inner.resolve(t)?;
        let input = pane.lock_input()?;
        let (key, turn0) = {
            let st = pane.lock_state()?;
            if !st.live {
                return Err(finished(&pane.id));
            }
            let key = if st.tracker.reported() {
                Key::Esc
            } else {
                Key::CtrlC
            };
            (key, st.tracker.turn())
        };
        let fg0 = pty::tcgetpgrp(pane.master.as_raw_fd());
        pane.write_guarded(None, |st| match &key {
            // A program that never reported status may have left keyboard
            // flags pushed when it died; the line discipline makes SIGINT
            // only from the raw byte, so send that whatever the flags say.
            Key::CtrlC => Ok(vec![0x03]),
            _ => encode_key(&key, st.emu.mode()).map(<[u8]>::to_vec),
        })?;
        drop(input);
        let outcome = pane.observe_interrupt(turn0, fg0, timeout.unwrap_or(INTERRUPT_DEFAULT))?;
        Ok(InterruptResult { key, outcome })
    }

    /// SIGHUP and SIGTERM to the pane's group and every descendant, up to
    /// `grace` (default 2 s) for the leader and descendants to exit, then
    /// SIGKILL to the group and every descendant left (ADR §11). The leader
    /// is reaped, its exit code recorded, and the pane leaves the table. No
    /// key is sent first.
    fn remove(
        &self,
        t: &TerminalRef,
        grace: Option<Duration>,
    ) -> Result<RemoveResult, BackendError> {
        let pane = self.inner.resolve(t)?;
        let was_live = pane.is_live();
        pane.stopping.store(true, Ordering::Release);
        pane.soft_stop();
        let deadline = Instant::now() + grace.unwrap_or(KILL_GRACE);
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
        self.inner.table()?.panes.retain(|p| !Arc::ptr_eq(p, &pane));
        let exit_code = pane
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .exit_code;
        Ok(RemoveResult {
            was_live,
            stopped: was_live.then_some(if killed {
                StopHow::Killed
            } else {
                StopHow::Exited
            }),
            exit_code,
        })
    }
}
