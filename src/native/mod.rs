//! Shepherd's own terminal core: a PTY per pane, a VT emulator over its
//! output, and the OSC 7501 program status read from that same emulator.
//! See docs/adr/native-core.md.
//!
//! The core has two halves. The process half (spawn, the output pump, the
//! reaper, signals and the process tree) is behind [`host::PtyHost`]; the
//! screen half (emulator, tracker, guard, the tools' view of a pane) is here.
//! [`host::local::LocalHost`] runs the process half in this process.
//!
//! Locking (ADR §9): the pane table, then a pane's `input`, then its `state`;
//! the host takes a pane's `tree` before its `state` and never with `input`.
//! Nothing takes `input` while holding `state`, and no lock is held across a
//! sleep, a poll or a wait, except the condvar wait, which releases `state`.
//! The host's threads hold only their pane and the event sink (which holds
//! the table, not the backend), so dropping the last [`NativeBackend`]
//! handle ends every pane.

mod host;
mod human;
pub(crate) mod keys;
mod lease;
mod meta;
mod replay;
pub(crate) mod status;
pub(crate) mod term;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_forged;
#[cfg(test)]
mod tests_link;
#[cfg(test)]
mod tests_output;
#[cfg(test)]
mod tests_replay;
// The same suites again with each front reaching the host through a holder
// over a socketpair link (issue #76, PR5). The module name ends in `_held`,
// which `is_held` reads.
#[cfg(test)]
#[path = "tests.rs"]
#[allow(clippy::duplicate_mod)]
mod tests_held;
#[cfg(test)]
#[path = "tests_output.rs"]
#[allow(clippy::duplicate_mod)]
mod tests_output_held;
#[cfg(test)]
#[path = "tests.rs"]
#[allow(clippy::duplicate_mod)]
mod tests_readopt_held;
#[cfg(test)]
mod tests_replay_fixtures;
#[cfg(test)]
#[path = "tests_replay.rs"]
#[allow(clippy::duplicate_mod)]
mod tests_replay_held;
#[cfg(test)]
mod tests_tail;

pub use host::front::{
    connect_front, exit_on_signals_leaving_panes, front_version, FrontError, FrontLink, LinkWatch,
    EXIT_LINK_CLOSED, EXIT_VERSION_MISMATCH, TEST_VERSION_ENV,
};
pub(crate) use host::pty::{default_signals_on_exec, effective_uid, SPAWN_LOCK};
pub use host::supervisor::run_holder;
pub use keys::KeyboardMode;
pub use status::LogEntry as StatusLogEntry;

use crate::backend::{
    dialog_kind, BackendError, Capabilities, ErrorKind, InputGuard, InterruptOutcome,
    InterruptResult, Key, KeysResult, ReadLimit, ReadSource, RemoveResult, RunSpec, ScreenCheck,
    ScreenText, SendResult, State, StopHow, TerminalBackend, TerminalInfo, TerminalList,
    TerminalRef, WaitReason, WaitResult,
};
use crate::input::MAX_INPUT_BYTES;
use host::local::LocalHost;
use host::{pty, HostEvents, KillOutcome, PtyHost, SpawnReq, KILL_GRACE};
use keys::encode as encode_key;
use lease::{Lease, LeaseClock};
use meta::{MetaStore, PaneMeta};
use replay::replay;
use status::{LogEntry, Report, StatusLog, Tracker};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use term::{Emulator, COLS, ROWS};

// Bounds and defaults (ADR §13). The screen size and scrollback are
// `term::{ROWS, COLS, SCROLLBACK}`, the status log length is
// `status::STATUS_LOG_LEN`.
const MAX_LIVE_PANES: usize = 8;
const MAX_FINISHED_PANES: usize = 32;
const RAW_RING: usize = 256 * 1024;
const PENDING_REPLIES: usize = 4 * 1024;
const MAX_ARGV: usize = 256;
pub(crate) const MAX_EXTRA_ENV: usize = 40;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SETTLE_QUIET: Duration = Duration::from_millis(100);
const SETTLE_MAX: Duration = Duration::from_secs(1);
const START_WAIT: Duration = Duration::from_secs(2);
const READ_SYNC_WAIT: Duration = Duration::from_millis(150);
const INTERRUPT_DEFAULT: Duration = Duration::from_secs(10);
/// How often interrupt re-reads the foreground group.
const INTERRUPT_POLL: Duration = Duration::from_millis(50);
/// How long an adopted pane keeps the one-row-smaller terminal before the
/// return resize, so a program that repaints on SIGWINCH has painted the
/// smaller size by the time the offset that counts is taken.
const NUDGE_PAUSE: Duration = Duration::from_millis(150);

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
    // Overridden from the host in `capabilities()`.
    survives_restart: false,
};

/// Settings fixed when the backend is built.
#[derive(Debug, Clone, Default)]
pub struct NativeConfig {
    /// Extra variables for every child, set by code only (tests, the probe).
    /// Keys match `[A-Z_][A-Z0-9_]*`, values have no NUL; together with a
    /// run's own `RunSpec.env` and `SHEPHERD_PANE`, at most 40 (a profile's
    /// 32 and a caller's 32 are each limited before they get here).
    pub extra_env: Vec<(OsString, OsString)>,
    /// The clock the input lease's idle rule reads. Tests move it.
    pub(crate) lease_clock: LeaseClock,
}

/// What [`NativeBackend::adopt`] picked up, by pane id.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Adoption {
    pub adopted: Vec<String>,
    /// The adopted panes that are live and were rebuilt from a wrapped tail
    /// (or lost output), so they refuse writes until they repaint.
    pub replayed: Vec<String>,
}

type EnvLookup = Box<dyn Fn(&str) -> Option<OsString> + Send + Sync>;

/// The native backend. A cheap handle: clones share the same panes.
#[derive(Clone)]
pub struct NativeBackend {
    inner: Arc<Inner>,
}

struct Inner {
    /// Shared with the host's event sink, which finds a pane by id.
    table: Arc<Mutex<Table>>,
    host: Arc<dyn PtyHost>,
    /// The same host as `host`, for the tests that reach into its panes.
    #[cfg(test)]
    host_any: Arc<dyn std::any::Any + Send + Sync>,
    config: NativeConfig,
    /// Reads the parent's environment for the whitelist.
    env: EnvLookup,
    /// Keep each pane's metadata in the host (see [`meta`]). Off for the
    /// single-process server, which has no later front to read it.
    keep_meta: bool,
    /// Dropping the last handle leaves the panes running in the host (the
    /// front is going away, not the panes) instead of ending them.
    leave_on_drop: AtomicBool,
    /// Test rig: after each `run`, drop the pane from the table and adopt it
    /// from the host again, as a front that restarted right after the spawn
    /// would. Catches state that adoption forgets.
    #[cfg(test)]
    readopt: AtomicBool,
}

#[derive(Default)]
struct Table {
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
    host: Arc<dyn PtyHost>,
    state: Mutex<PaneState>,
    changed: Condvar,
    /// Held for a whole write sequence (send, keys, interrupt, reply flush),
    /// and by every lease, draft and human-write operation, so none of them
    /// lands in the middle of a bot's paste and Enter.
    input: Mutex<()>,
    /// The clock of the lease's idle rule.
    clock: LeaseClock,
    /// The only writer of this pane's host metadata, when kept.
    meta: Option<MetaStore>,
}

struct PaneState {
    emu: Emulator,
    tracker: Tracker,
    live: bool,
    exit_code: Option<i64>,
    /// The turn of a send that has not gone working yet.
    pending_send: Option<u64>,
    pending_replies: Vec<u8>,
    replies_overflowed: bool,
    raw: VecDeque<u8>,
    last_output: Instant,
    log: StatusLog,
    /// The tracker has no trustworthy state after adoption: treated as
    /// working until a fresh report or the exit (not `reported` with no
    /// status, which `wait` reads as finished).
    unknown: bool,
    /// The pane was rebuilt from a wrapped tail or lost output: the guard
    /// refuses writes until the program repaints.
    replayed: bool,
    /// Output at or after this offset is the program's answer to the return
    /// resize and clears `replayed`. None: nothing clears it.
    clear_at: Option<u64>,
    /// The offset after the last output byte the emulator has taken.
    fed_end: u64,
    /// The person's input lease, if one is held. Never persisted: a front
    /// that restarts has none.
    lease: Option<Lease>,
    /// A person typed into the pane and has not pressed Enter. Authoritative
    /// here (the bare command keeps no meta); written through to the host's
    /// meta when there is one, and restored from it by adoption.
    draft: bool,
}

impl PaneState {
    fn fresh(emu: Emulator, tracker: Tracker) -> Self {
        Self {
            emu,
            tracker,
            live: true,
            exit_code: None,
            pending_send: None,
            pending_replies: Vec::new(),
            replies_overflowed: false,
            raw: VecDeque::new(),
            last_output: Instant::now(),
            log: StatusLog::default(),
            unknown: false,
            replayed: false,
            clear_at: None,
            fed_end: 0,
            lease: None,
            draft: false,
        }
    }
}

impl NativeBackend {
    pub fn new(config: NativeConfig) -> Self {
        Self::with_env(config, Box::new(|key| std::env::var_os(key)))
    }

    fn with_env(config: NativeConfig, env: EnvLookup) -> Self {
        Self::with_host(config, env, Arc::new(LocalHost::new()))
    }

    /// A backend over `host`, which gets this backend's table as its sink.
    fn with_host<H: PtyHost + 'static>(config: NativeConfig, env: EnvLookup, host: Arc<H>) -> Self {
        Self::with_host_meta(config, env, host, false)
    }

    /// A backend over `host` that also keeps each pane's metadata there.
    /// Nothing in the server builds one yet; the holder will.
    fn with_host_meta<H: PtyHost + 'static>(
        config: NativeConfig,
        env: EnvLookup,
        host: Arc<H>,
        keep_meta: bool,
    ) -> Self {
        let table = Arc::new(Mutex::new(Table::default()));
        host.set_sink(Arc::new(FrontEvents {
            table: Arc::clone(&table),
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
        }));
        Self {
            inner: Arc::new(Inner {
                table,
                #[cfg(test)]
                host_any: Arc::clone(&host) as Arc<dyn std::any::Any + Send + Sync>,
                host,
                config,
                env,
                keep_meta,
                leave_on_drop: AtomicBool::new(false),
                #[cfg(test)]
                readopt: AtomicBool::new(false),
            }),
        }
    }

    /// A backend over the holder `client` reaches: its panes live in the
    /// holder and outlive this process, so each one's metadata is kept there.
    pub(crate) fn over_holder(
        config: NativeConfig,
        client: Arc<host::client::HolderClient>,
    ) -> Self {
        Self::with_host_meta(config, Box::new(|key| std::env::var_os(key)), client, true)
    }

    /// Drop the pane from the table and adopt it from the host again.
    #[cfg(test)]
    fn readopt(&self, id: &str) -> Result<(), BackendError> {
        // Let the program say its first things, so there is a state to lose.
        thread::sleep(Duration::from_millis(250));
        self.inner.table()?.panes.retain(|p| p.id != id);
        self.adopt().map(drop)
    }

    /// End every pane now, as dropping the last handle does: what Shepherd
    /// runs before it exits on a signal.
    pub fn shutdown(&self) {
        self.inner.end_all();
    }

    /// Make dropping the last handle leave the panes running in the host
    /// instead of ending them: the front is going away, the panes are not.
    pub(crate) fn leave_panes_on_drop(&self) {
        self.inner.leave_on_drop.store(true, Ordering::Release);
    }

    /// Pick up every pane the host holds that this front does not know yet:
    /// rebuild its screen and tracker from the host's tail without answering
    /// anything the tail asks, decide whether it is `unknown` or `replayed`,
    /// and nudge a replayed pane to repaint. Emits no status events and
    /// writes nothing to any pane (except the two resizes of the nudge).
    ///
    /// A pane enters the table only when its replay is done, and the table
    /// stays locked from the host's tail read to that moment, so no output
    /// can be lost or fed twice (the front skips bytes already replayed).
    pub(crate) fn adopt(&self) -> Result<Adoption, BackendError> {
        let host = &self.inner.host;
        let mut done = Adoption::default();
        let mut nudge = Vec::new();
        for listed in host.list() {
            let mut table = self.inner.table()?;
            if table.exiting {
                break;
            }
            if table.panes.iter().any(|p| p.id == listed.id) {
                continue;
            }
            let attached = match host.attach(&listed.id) {
                Ok(attached) => attached,
                // Ended since the list.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) if err.kind() == io::ErrorKind::Unsupported => {
                    return Err(BackendError::new(
                        ErrorKind::Unsupported("adopt".to_string()),
                        format!("native: the host keeps no output tail: {err}"),
                    ))
                }
                Err(err) => {
                    return Err(BackendError::new(
                        ErrorKind::Other,
                        format!("native: {}: cannot read the tail: {err}", listed.id),
                    ))
                }
            };
            // The pane may have exited since the list, and its exit event went
            // to a table that did not hold it yet. Read the state again now
            // that the table is locked: from here on the event is not lost.
            let Some(listed) = host.list().into_iter().find(|p| p.id == listed.id) else {
                continue;
            };
            let meta = meta::load(&**host, &listed.id);
            let adopted = replay(
                &attached.bytes,
                attached.tail_start,
                meta.as_ref(),
                now_ms(),
            );
            for note in &adopted.notes {
                eprintln!("openab-shepherd: native: {}: {note}", listed.id);
            }
            let live = !listed.finished;
            let wrong_size = (listed.cols, listed.rows) != (COLS, ROWS);
            let replayed = adopted.replayed || wrong_size;
            let snapshot = adopted.tracker.snapshot();
            let mut tracker = adopted.tracker;
            if !live {
                tracker.on_exit();
            }
            let known = meta.clone().unwrap_or_default();
            let store = self.inner.keep_meta.then(|| {
                let store = MetaStore::new(Arc::clone(host), listed.id.clone(), known.clone());
                // Keep the snapshot exact at the end of the tail, so the next
                // adoption replays less. Not for a pane with no state.
                let stale = known.tracker.as_ref() != Some(&snapshot)
                    || known.tracker_seq != attached.tail_end;
                if !adopted.unknown && stale {
                    if let Err(err) = store.set_tracker(snapshot.clone(), attached.tail_end) {
                        eprintln!(
                            "openab-shepherd: native: {}: meta write failed: {err}",
                            listed.id
                        );
                    }
                }
                store
            });
            let mut state = PaneState::fresh(adopted.emu, tracker);
            state.live = live;
            state.exit_code = listed.exit_code.filter(|_| !live);
            state.unknown = adopted.unknown;
            state.replayed = replayed;
            state.fed_end = attached.tail_end;
            state.draft = known.human_draft;
            state.raw = attached.bytes[attached.bytes.len().saturating_sub(RAW_RING)..]
                .iter()
                .copied()
                .collect();
            let pane = Arc::new(Pane {
                id: listed.id.clone(),
                name: known.name.clone(),
                argv: known.argv.clone(),
                cwd: known.cwd.clone(),
                pid: listed.pid,
                host: Arc::clone(host),
                state: Mutex::new(state),
                changed: Condvar::new(),
                input: Mutex::new(()),
                clock: self.inner.config.lease_clock.clone(),
                meta: store,
            });
            table.panes.push(Arc::clone(&pane));
            drop(table);
            done.adopted.push(listed.id.clone());
            if replayed && live {
                done.replayed.push(listed.id.clone());
                nudge.push(pane);
            }
        }
        self.inner.nudge(&nudge);
        Ok(done)
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

    /// Start the child on the host and record it. The table stays locked
    /// from the spawn to the insertion, so no output or exit of the new
    /// pane can reach the sink before the pane is in the table.
    fn insert(
        &self,
        spec: &RunSpec,
        argv: Vec<String>,
        cwd: String,
        env: Vec<(OsString, OsString)>,
    ) -> Result<Arc<Pane>, BackendError> {
        let mut dropped: Vec<Arc<Pane>> = Vec::new();
        let mut table = self.inner.table()?;
        let spawned = self.inner.host.spawn(SpawnReq {
            argv: argv.clone(),
            cwd: cwd.clone(),
            env,
        })?;
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
            self.inner.host.kill(&spawned.id, Duration::ZERO);
            return Err(err);
        }
        if let Some(name) = spec.name.as_deref() {
            let (same, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut table.panes)
                .into_iter()
                .partition(|p| p.name.as_deref() == Some(name) && !p.is_live());
            table.panes = rest;
            dropped = same;
        }
        let meta = self.inner.keep_meta.then(|| {
            let store = MetaStore::new(
                Arc::clone(&self.inner.host),
                spawned.id.clone(),
                PaneMeta {
                    name: spec.name.clone(),
                    argv: argv.clone(),
                    cwd: cwd.clone(),
                    ..PaneMeta::default()
                },
            );
            // Before the pane is in the table, so no output can write first.
            if let Err(err) = store.update(|_| ()) {
                eprintln!(
                    "openab-shepherd: native: {}: meta write failed: {err}",
                    spawned.id
                );
            }
            store
        });
        let pane = Arc::new(Pane {
            id: spawned.id,
            name: spec.name.clone(),
            argv,
            cwd,
            pid: spawned.pid,
            host: Arc::clone(&self.inner.host),
            state: Mutex::new(PaneState::fresh(Emulator::new(), Tracker::default())),
            changed: Condvar::new(),
            input: Mutex::new(()),
            clock: self.inner.config.lease_clock.clone(),
            meta,
        });
        table.panes.push(Arc::clone(&pane));
        dropped.extend(evict_finished(&mut table));
        drop(table);
        // Finished panes leave the table only with their group ended.
        for old in dropped {
            self.inner.host.forget(&old.id);
        }
        Ok(pane)
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
    /// Make each pane repaint: shrink its terminal by one row, give the
    /// program a moment, then restore the size. Only the terminal changes;
    /// the emulator of an adopted pane is new, at 200x120. Output at or after the offset
    /// the host has when the return resize goes out clears `replayed`, so a
    /// repaint answering the smaller size never does.
    fn nudge(&self, panes: &[Arc<Pane>]) {
        let mut shrunk = Vec::new();
        for pane in panes {
            match self.host.resize(&pane.id, COLS, ROWS - 1) {
                Ok(()) => shrunk.push(pane),
                Err(err) => eprintln!("openab-shepherd: native: {}: nudge failed: {err}", pane.id),
            }
        }
        if shrunk.is_empty() {
            return;
        }
        thread::sleep(NUDGE_PAUSE);
        for pane in shrunk {
            match self.host.attach(&pane.id) {
                Ok(now) => {
                    let mut st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
                    if st.replayed {
                        st.clear_at = Some(now.tail_end);
                    }
                }
                Err(err) => eprintln!(
                    "openab-shepherd: native: {}: nudge cannot read the tail: {err}",
                    pane.id
                ),
            }
            if let Err(err) = self.host.resize(&pane.id, COLS, ROWS) {
                eprintln!("openab-shepherd: native: {}: nudge failed: {err}", pane.id);
            }
        }
    }
}

impl Inner {
    /// End every pane now (the host runs the shared-grace sequence). A child
    /// spawned after this is ended at once instead of entering the table.
    fn end_all(&self) {
        {
            let mut table = self.table.lock().unwrap_or_else(PoisonError::into_inner);
            table.exiting = true;
            table.panes.clear();
        }
        self.host.end_all();
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if self.leave_on_drop.load(Ordering::Acquire) {
            // The panes outlive this front: stop hearing of them, end none.
            self.host.set_sink(Arc::new(NoFront));
            return;
        }
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
    fn ingest(&self, seq: u64, chunk: &[u8]) -> bool {
        let Ok(mut guard) = self.state.lock() else {
            return false;
        };
        let st = &mut *guard;
        // Adoption has fed the emulator up to `fed_end`; a chunk the host
        // delivers for bytes already in the replay is not fed twice.
        let (seq, chunk) = if seq < st.fed_end {
            let skip = (st.fed_end - seq).min(chunk.len() as u64);
            (seq + skip, &chunk[skip as usize..])
        } else {
            (seq, chunk)
        };
        if chunk.is_empty() {
            return true;
        }
        st.fed_end = seq + chunk.len() as u64;
        let before = self.meta.as_ref().map(|_| st.tracker.snapshot());
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
                    st.unknown = false;
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
        if st.replayed && st.clear_at.is_some_and(|at| seq >= at) {
            st.replayed = false;
            st.clear_at = None;
        }
        let after = before.and_then(|before| {
            let now = st.tracker.snapshot();
            (now != before).then_some(now)
        });
        drop(guard);
        self.changed.notify_all();
        // Only a chunk that changed the tracker writes: the snapshot is exact
        // at the end of this chunk. The chunk is already in the emulator, so
        // a crash before this write only replays it once more.
        if let (Some(meta), Some(snapshot)) = (&self.meta, after) {
            if let Err(err) = meta.set_tracker(snapshot, seq + chunk.len() as u64) {
                eprintln!(
                    "openab-shepherd: native: {}: meta write failed: {err}",
                    self.id
                );
            }
        }
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
        if let Ok(n) = self.host.try_write(&self.id, &st.pending_replies) {
            st.pending_replies.drain(..n);
        }
    }

    /// Check who may write, then `guard` (or only liveness, when None), and
    /// write the bytes `bytes_for` gives, in one critical section on `state`,
    /// so no status change lands between the check and the bytes entering the
    /// PTY. Call with `input` held.
    ///
    /// The order of refusals for a bot is: the program has ended, a person
    /// holds the lease, a person has a draft, then the guard's own conditions
    /// (replayed, blocked, status, turn, screen). A person's write instead
    /// needs the lease it names, and the status guards do not apply to it.
    fn write_guarded(
        &self,
        origin: WriteOrigin,
        guard: Option<&InputGuard>,
        mut bytes_for: impl FnMut(&PaneState) -> Result<Vec<u8>, BackendError>,
    ) -> Result<(), BackendError> {
        let deadline = Instant::now() + WRITE_TIMEOUT;
        loop {
            let (bytes, written) = {
                let mut st = self.lock_state()?;
                if !st.live {
                    return Err(finished(&self.id));
                }
                match origin {
                    WriteOrigin::Bot => self.refuse_while_human(&st)?,
                    WriteOrigin::Human { lease_id } => self.check_lease(&st, lease_id)?,
                }
                if let Some(guard) = guard {
                    check_guard(&st, guard, &self.id)?;
                }
                let bytes = bytes_for(&st)?;
                // Query replies go first and whole, so caller bytes never
                // land inside a half-written reply.
                let replies_left = if st.pending_replies.is_empty() {
                    false
                } else {
                    match self.host.try_write(&self.id, &st.pending_replies) {
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
                    match self.host.try_write(&self.id, &bytes) {
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
            let _ = self.host.wait_writable(&self.id, left);
        }
    }

    /// The rest of a write that has started; no further guard.
    fn finish_write(&self, mut rest: &[u8], deadline: Instant) -> Result<(), BackendError> {
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
            match self.host.try_write(&self.id, rest) {
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
                    let _ = self.host.wait_writable(&self.id, left);
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
            } else if fg0 != Some(self.pid) && self.host.fg_pgrp(&self.id) == Some(self.pid) {
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

/// Who is writing. A bot's write is refused while a person holds the lease or
/// has a draft; a person's write needs the lease. Not defaulted, so the
/// compiler finds every caller.
#[derive(Clone, Copy)]
enum WriteOrigin {
    Bot,
    Human { lease_id: u64 },
}

/// The guard's conditions, in the ADR §9 order.
fn check_guard(st: &PaneState, guard: &InputGuard, id: &str) -> Result<(), BackendError> {
    if !st.live {
        return Err(finished(id));
    }
    // Same critical section as the write: a pane rebuilt from a wrapped tail
    // may sit in a mode the emulator never saw, so no key goes to it.
    if st.replayed {
        return Err(BackendError::new(
            ErrorKind::Replayed,
            format!(
                "native: {id}: the pane was picked up after a restart and has not repainted; nothing was written"
            ),
        ));
    }
    // A pane with no trustworthy status counts as working.
    let working = State::Working;
    let state = if st.unknown {
        Some(&working)
    } else {
        st.tracker.state()
    };
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

fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        [b"\x1b[200~", text.as_bytes(), b"\x1b[201~"].concat()
    } else {
        text.as_bytes().to_vec()
    }
}

/// Test support: whether the suite in module `module` (a `module_path!()`)
/// runs its fronts over a holder link.
#[cfg(test)]
fn is_held(module: &str) -> bool {
    module.ends_with("_held")
}

/// Test support: a front for the suite in module `module`: in process, or
/// over a holder link that keeps its metadata like the front under `hold`.
#[cfg(test)]
fn test_front(module: &str, config: NativeConfig, env: EnvLookup) -> NativeBackend {
    if is_held(module) {
        let client = host::held::Held::new().connect();
        let front = NativeBackend::with_host_meta(config, env, client, true);
        // A module with `readopt` in its name adopts every pane at once.
        front
            .inner
            .readopt
            .store(module.contains("readopt"), Ordering::Relaxed);
        front
    } else {
        NativeBackend::with_env(config, env)
    }
}

/// What the host's threads tell the screen half. Finds the pane by id in the
/// table; an id the table no longer has (a pane being ended or removed) is
/// dropped, since nothing can observe its screen any more.
struct FrontEvents {
    table: Arc<Mutex<Table>>,
    /// Which front this sink belongs to, so a host thread's cached pane from
    /// an earlier front is never used for this one.
    epoch: u64,
}

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The pane the current host thread last resolved, with the epoch of the
    /// front it belongs to. A host thread serves one pane, so the table is
    /// entered once per thread, not once per chunk.
    ///
    /// Weak, so a thread that outlives its front (the link's events thread)
    /// does not keep the pane, and through it the host, alive.
    static RESOLVED: RefCell<Option<(u64, Weak<Pane>)>> = const { RefCell::new(None) };
}

/// The sink of a host whose front is gone: the panes run on, the output
/// stays in the host's tail, and nobody is told.
struct NoFront;

impl HostEvents for NoFront {
    fn output(&self, _: &str, _: u64, _: &[u8]) -> bool {
        true
    }
    fn idle(&self, _: &str) {}
    fn exited(&self, _: &str, _: Option<i64>) {}
    fn gap(&self, _: &str, _: u64) {}
}

impl FrontEvents {
    fn pane(&self, id: &str) -> Option<Arc<Pane>> {
        if let Some(hit) = RESOLVED.with(|r| {
            r.borrow()
                .as_ref()
                .filter(|(epoch, _)| *epoch == self.epoch)
                .and_then(|(_, p)| p.upgrade())
                .filter(|p| p.id == id)
        }) {
            return Some(hit);
        }
        let found = self
            .table
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .panes
            .iter()
            .find(|p| p.id == id)
            .cloned();
        RESOLVED.with(|r| {
            *r.borrow_mut() = found
                .as_ref()
                .map(|pane| (self.epoch, Arc::downgrade(pane)));
        });
        found
    }
}

impl HostEvents for FrontEvents {
    fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool {
        let Some(pane) = self.pane(id) else {
            return true;
        };
        if !pane.ingest(seq, bytes) {
            return false;
        }
        if let Ok(_input) = pane.input.try_lock() {
            pane.flush_replies();
        }
        true
    }

    fn idle(&self, id: &str) {
        if let Some(pane) = self.pane(id) {
            if let Ok(_input) = pane.input.try_lock() {
                pane.flush_replies();
            }
        }
    }

    fn exited(&self, id: &str, code: Option<i64>) {
        let Some(pane) = self.pane(id) else {
            return;
        };
        let mut st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.live = false;
        st.exit_code = code;
        st.pending_send = None;
        st.tracker.on_exit();
        drop(st);
        pane.changed.notify_all();
    }

    fn gap(&self, id: &str, _new_start: u64) {
        let Some(pane) = self.pane(id) else {
            return;
        };
        // Output was dropped: the screen and the tracker are no longer what
        // the program has painted. Like a wrapped tail, until it repaints.
        let mut st = pane.state.lock().unwrap_or_else(PoisonError::into_inner);
        st.replayed = true;
        st.unknown = true;
        st.clear_at = None;
        drop(st);
        pane.changed.notify_all();
    }
}

/// The tools layer's record of each pane lives in the pane's host metadata,
/// through the pane's [`MetaStore`], so a front that starts later gets the
/// agent kind, the routine's counters and the answered flag back. A front
/// that keeps no metadata records nothing and reads nothing.
impl crate::registry::PaneRegistry for NativeBackend {
    fn set_agent(
        &self,
        pane_id: &str,
        agent: Option<&str>,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(), BackendError> {
        self.meta_update(pane_id, |m| {
            m.agent = agent.map(str::to_string);
            m.profile = profile.map(str::to_string);
            if let Some(cwd) = cwd {
                m.cwd = cwd.to_string();
            }
        })
    }

    fn set_answered(&self, pane_id: &str, answered: bool) -> Result<(), BackendError> {
        self.meta_update(pane_id, |m| m.answered = answered)
    }

    fn set_routine(
        &self,
        pane_id: &str,
        routine: Option<&crate::registry::RoutineState>,
    ) -> Result<(), BackendError> {
        self.meta_update(pane_id, |m| {
            m.routine = routine.map(|r| meta::RoutineMeta {
                consecutive: r.consecutive,
                gave_up: r.gave_up,
                nudge_allowance: r.nudge_allowance,
            });
        })
    }

    fn set_routines(
        &self,
        pane_id: &str,
        routines: &crate::registry::RoutineSet,
    ) -> Result<(), BackendError> {
        let pane = match self.inner.resolve(&TerminalRef::new(pane_id)) {
            Ok(pane) if pane.id == pane_id => pane,
            _ => return Ok(()),
        };
        let Some(store) = pane.meta.as_ref() else {
            return Ok(());
        };
        store.set_routines(routines).map_err(|err| {
            BackendError::new(
                ErrorKind::Other,
                format!("native: {pane_id}: cannot write the pane metadata: {err}"),
            )
        })
    }

    fn forget(&self, pane_id: &str) -> Result<(), BackendError> {
        self.meta_update(pane_id, |m| {
            m.routines = crate::registry::RoutineSet::default();
            m.agent = None;
            m.profile = None;
            m.answered = false;
            m.routine = None;
        })
    }

    fn registered(&self) -> Vec<crate::registry::RegisteredPane> {
        let panes: Vec<Arc<Pane>> = match self.inner.table() {
            Ok(table) => table.panes.clone(),
            Err(_) => return Vec::new(),
        };
        panes
            .iter()
            .filter_map(|pane| {
                let meta = pane.meta.as_ref()?.get();
                if meta.agent.is_none() && meta.routine.is_none() && meta.routines.is_empty() {
                    return None;
                }
                Some(crate::registry::RegisteredPane {
                    pane_id: pane.id.clone(),
                    name: pane.name.clone(),
                    cwd: pane.cwd.clone(),
                    agent: meta.agent,
                    profile: meta.profile,
                    answered: meta.answered,
                    routine: meta.routine.map(|r| crate::registry::RoutineState {
                        consecutive: r.consecutive,
                        gave_up: r.gave_up,
                        nudge_allowance: r.nudge_allowance,
                    }),
                    routines: meta.routines,
                    live: pane.is_live(),
                })
            })
            .collect()
    }
}

impl NativeBackend {
    /// Change one pane's metadata through its store. A pane that is not in
    /// the table, or a front that keeps no metadata, is not an error: there
    /// is nothing to keep it in.
    fn meta_update(
        &self,
        pane_id: &str,
        change: impl FnOnce(&mut PaneMeta),
    ) -> Result<(), BackendError> {
        let pane = match self.inner.resolve(&TerminalRef::new(pane_id)) {
            Ok(pane) if pane.id == pane_id => pane,
            _ => return Ok(()),
        };
        let Some(store) = pane.meta.as_ref() else {
            return Ok(());
        };
        store.update(change).map_err(|err| {
            BackendError::new(
                ErrorKind::Other,
                format!("native: {pane_id}: cannot write the pane metadata: {err}"),
            )
        })
    }
}

impl TerminalBackend for NativeBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            survives_restart: self.inner.host.survives_restart(),
            ..CAPABILITIES
        }
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
        // Last, so nothing before it can override it: the pane's own name,
        // which the agent passes as `terminal`. Never a credential.
        if let Some(name) = &spec.name {
            extra.push((OsString::from("SHEPHERD_PANE"), OsString::from(name)));
        }
        let env = pty::child_env(&*self.inner.env, &extra)?;
        let cwd = self.cwd_for(spec.cwd.as_deref())?;
        check_room(&*self.inner.table()?, spec.name.as_deref())?;
        let pane = self.insert(spec, argv, cwd, env)?;
        let info = pane.info();
        #[cfg(test)]
        if self.inner.readopt.load(Ordering::Relaxed) {
            let id = pane.id.clone();
            drop(pane);
            self.readopt(&id)?;
            return self.inner.resolve(&TerminalRef::new(id))?.info();
        }
        info
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
        pane.write_guarded(WriteOrigin::Bot, Some(guard), |st| {
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
        pane.write_guarded(WriteOrigin::Bot, Some(&enter_guard), |st| {
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
            pane.write_guarded(WriteOrigin::Bot, Some(guard), |st| {
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
            pane.write_guarded(WriteOrigin::Bot, Some(guard), |st| {
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
            let working =
                st.unknown || !st.tracker.reported() || st.tracker.state() == Some(&State::Working);
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
        let fg0 = pane.host.fg_pgrp(&pane.id);
        pane.write_guarded(WriteOrigin::Bot, None, |st| match &key {
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
        // Ending a pane someone is typing into is a write of the worst kind,
        // so it is refused like one. Under `input`, so no lease can be taken
        // between the check and the kill. A finished pane is always removable.
        let input = pane.lock_input()?;
        {
            let st = pane.lock_state()?;
            if st.live {
                pane.refuse_while_human(&st)?;
            }
        }
        let was_live = pane.is_live();
        // The host ends the pane and reaps the leader; its exit is reported
        // to the pane (the exit code) before this returns.
        let killed = self
            .inner
            .host
            .kill(&pane.id, grace.unwrap_or(KILL_GRACE))
            .is_some_and(|outcome: KillOutcome| outcome.killed);
        drop(input);
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
