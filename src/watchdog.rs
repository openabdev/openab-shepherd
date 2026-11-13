//! The watchdog: Shepherd's own liveness check on the panes a profile
//! started with a `watchdog` section (see [`WatchdogConfig`]).
//!
//! A coding CLI's main session sometimes ends a turn with work left and
//! simply stops: not working, not blocked, not waiting on anything Shepherd
//! can see. The watchdog notices that one state — a finished turn that sits
//! untouched past the profile's idle timeout, with unfinished work left when
//! a todo file is configured — and types the profile's nudge text through
//! the same path `send_input` uses (see `tools::send_text`).
//!
//! The decision here is mechanical on purpose. Whether the remaining work is
//! worth doing, or the agent is stuck on a question for a person, is a
//! judgment for the client supervising through `wait_status`; the watchdog
//! never answers a dialog and never types into a blocked pane.

use crate::agent::Agent;
use crate::backend::{State, TerminalInfo};
use crate::profiles::WatchdogConfig;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the watchdog thread looks at the panes it watches.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// The turn advances one nudge may explain. A nudge usually makes the agent
/// run one turn, sometimes a follow-up as well; an advance larger than this
/// (or one with no nudge outstanding) is the agent making progress on its
/// own and clears the consecutive count.
pub const NUDGE_TURN_ALLOWANCE: u64 = 3;

/// How long the watchdog waits for a pane's lock before it skips that pane
/// for the tick. The lock is held for the whole of another call's send or
/// wait, and the one watchdog thread serves every watched pane.
pub const LOCK_WAIT: Duration = Duration::from_secs(2);

/// What a profile's todo file says about the work, when one is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Todo {
    /// No `todo_file` is configured: unfinished work cannot be ruled out.
    NotConfigured,
    /// The file holds at least one unchecked item (`- [ ]`).
    Unchecked,
    /// No unchecked item: every item is checked, or there are no items.
    Finished,
    /// The file could not be read. The watchdog fails closed: no nudge.
    Unreadable,
}

/// The most todo-file bytes the watchdog will read. A todo list is small;
/// anything larger is treated as unreadable rather than read into memory.
pub const MAX_TODO_BYTES: u64 = 256 * 1024;

/// A pane's working directory, resolved once when its watch is registered.
/// On Unix the resolved directory is held open as a directory fd, so every
/// later todo read starts from the directory the watch began with, not from
/// whatever the path names at that moment: an ancestor (or the directory
/// itself) swapped for a symlink after the watch started is never followed.
/// The path is canonicalized once, at open time, and kept for non-Unix
/// reads and messages.
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
    #[cfg(unix)]
    handle: Option<fs::File>,
}

impl WorkDir {
    /// Resolve `cwd` now. A directory that cannot be opened leaves a handle
    /// that reads every todo file as [`Todo::Unreadable`] (fail closed).
    pub fn open(cwd: &Path) -> Self {
        let path = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
        #[cfg(unix)]
        let handle = open_dir_fd(&path);
        Self {
            path,
            #[cfg(unix)]
            handle,
        }
    }

    /// The canonical path the directory had when it was opened.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A second handle on the same directory, for a read made outside the
    /// watch map's lock.
    pub fn try_clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            #[cfg(unix)]
            handle: self.handle.as_ref().and_then(|file| file.try_clone().ok()),
        }
    }
}

/// Open directory `path` for use as the base of `openat` walks.
#[cfg(unix)]
fn open_dir_fd(path: &Path) -> Option<fs::File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::io::FromRawFd as _;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: the fd is checked before use and its ownership passes to the
    // returned File.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOCTTY,
        )
    };
    if fd < 0 {
        return None;
    }
    Some(unsafe { fs::File::from_raw_fd(fd) })
}

/// Read the todo file named by the relative path `rel` inside the working
/// directory `cwd`, resolving `cwd` now. The watchdog itself reads through
/// the [`WorkDir`] its watch opened once ([`read_todo_in`]); this form is
/// for a caller with only a path.
pub fn read_todo(cwd: &Path, rel: &Path) -> Todo {
    read_todo_in(&WorkDir::open(cwd), rel)
}

/// Read the todo file named by the relative path `rel` inside `work`. An
/// unchecked item is a line whose first non-blank characters are `- [ ]`;
/// `- [x]`, other bullets, prose and anything inside a fenced code block
/// (a line starting with three backticks or tildes opens and closes one)
/// do not count. The check is a plain line scan, not a Markdown parse.
///
/// The file is read defensively: the supervised agent writes in the same
/// directory, so the path could be a FIFO (which would block a plain read
/// forever and stall the single watchdog thread), a symlink out of the
/// directory, or an unbounded file. The working directory is the boundary
/// the read enforces: on Unix it is the directory fd `work` holds, opened
/// once (its ancestors were resolved by that open, so a working directory
/// reached through a symlinked ancestor — `/tmp` on macOS, say — reads
/// normally, and a later swap of any ancestor is not followed), and only
/// the components of `rel` are walked with `openat` and `O_NOFOLLOW`, so a
/// symlink in any component of `rel` — an intermediate directory, not just
/// the file itself — is refused, and no component can be swapped for a
/// symlink between a check and the open. The handle must be a regular
/// file, and at most [`MAX_TODO_BYTES`] are read. Any failure, or a file
/// over the limit, maps to [`Todo::Unreadable`]: fail closed, no nudge.
/// (On non-Unix the read is a plain open of the canonical path joined with
/// `rel` and follows symlinks; the symlink refusal above is Unix-only.)
pub fn read_todo_in(work: &WorkDir, rel: &Path) -> Todo {
    use std::io::Read as _;
    #[cfg(unix)]
    let opened = work
        .handle
        .as_ref()
        .and_then(|dir| open_todo_refusing_symlinks(dir, rel));
    #[cfg(not(unix))]
    let opened = fs::File::open(work.path.join(rel)).ok();
    let Some(mut file) = opened else {
        return Todo::Unreadable;
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        _ => return Todo::Unreadable,
    }
    let mut buf = Vec::new();
    let read = file.by_ref().take(MAX_TODO_BYTES + 1).read_to_end(&mut buf);
    if read.is_err() || buf.len() as u64 > MAX_TODO_BYTES {
        return Todo::Unreadable;
    }
    let text = String::from_utf8_lossy(&buf);
    let mut fence: Option<&str> = None;
    let mut unchecked = false;
    for line in text.lines() {
        let line = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| line.starts_with(m));
        match (fence, marker) {
            (None, Some(open)) => fence = Some(open),
            (Some(open), Some(close)) if open == close => fence = None,
            (None, None) if line.starts_with("- [ ]") => unchecked = true,
            _ => {}
        }
    }
    if unchecked {
        Todo::Unchecked
    } else {
        Todo::Finished
    }
}

/// Open the todo file `rel` inside the directory fd `dir` for reading,
/// refusing a symlink in every component of `rel`. Each component is
/// opened relative to the previous one with `O_NOFOLLOW`, so the walk never
/// follows a link inside the boundary and no component can be swapped for
/// a symlink between a check and the open. `..` and absolute components
/// are refused outright; profiles already reject them, and a reader that
/// resolves them component by component has no safe meaning for them here.
#[cfg(unix)]
fn open_todo_refusing_symlinks(dir: &fs::File, rel: &Path) -> Option<fs::File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::path::Component;

    let mut parts: Vec<CString> = Vec::new();
    for part in rel.components() {
        match part {
            Component::Normal(name) => parts.push(CString::new(name.as_bytes()).ok()?),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    let (last, dirs) = parts.split_last()?;
    // The caller's fd is only borrowed; each directory opened below is owned
    // here and closed when the next replaces it.
    let mut owned: Option<OwnedFd> = None;
    for part in dirs {
        let from = owned.as_ref().map_or(dir.as_raw_fd(), |fd| fd.as_raw_fd());
        // SAFETY: `from` is a live fd; the result is checked before use.
        let next = unsafe {
            libc::openat(
                from,
                part.as_ptr(),
                libc::O_RDONLY
                    | libc::O_DIRECTORY
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC
                    | libc::O_NOCTTY,
            )
        };
        if next < 0 {
            return None;
        }
        owned = Some(unsafe { OwnedFd::from_raw_fd(next) });
    }
    let from = owned.as_ref().map_or(dir.as_raw_fd(), |fd| fd.as_raw_fd());
    let file_fd = unsafe {
        libc::openat(
            from,
            last.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NOCTTY,
        )
    };
    if file_fd < 0 {
        return None;
    }
    Some(unsafe { fs::File::from_raw_fd(file_fd) })
}

/// Why a nudge is due; the watchdog's log line names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeReason {
    /// A todo file is configured and still has an unchecked item.
    TodoUnchecked,
    /// No todo file is configured; the finished turn alone is the signal.
    NoTodoFile,
}

impl NudgeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TodoUnchecked => "the todo file still has an unchecked item",
            Self::NoTodoFile => "no todo file is configured",
        }
    }
}

/// What one observation of a pane decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchEvent {
    /// Nothing to do.
    None,
    /// Type the nudge text now.
    Nudge(NudgeReason),
    /// This observation reached `max_consecutive_nudges`: the watchdog
    /// stops for this pane. Returned once, on the crossing observation.
    GaveUp,
}

/// One pane's watchdog state: its profile's configuration, where it runs,
/// and what the watchdog has seen and done so far.
#[derive(Debug)]
pub struct PaneWatch {
    config: WatchdogConfig,
    work_dir: WorkDir,
    agent: Agent,
    /// The backend id of the pane this watch was registered for. A name
    /// reused by a later start is a different pane and never this watch's.
    pane_id: String,
    /// The turn counter at the last observation. A turn that advances on
    /// its own clears the consecutive count and a give-up: progress is the
    /// reset. Turns a nudge itself causes do not clear anything (see
    /// `nudge_allowance`) — otherwise every answered nudge would reset the
    /// cap and the watchdog would nudge forever.
    last_turn: Option<u64>,
    /// How many turn advances the nudges sent so far may still explain:
    /// set to [`NUDGE_TURN_ALLOWANCE`] by each nudge (not summed, so
    /// nudges that produced no turn do not build up credit), used up by
    /// the advances observed afterwards, and zero when no nudge is
    /// outstanding. A nudge may produce no turn, one, or a few (the agent
    /// runs a turn and then a follow-up); an advance within the allowance
    /// is the nudge's doing, a larger one is progress.
    nudge_allowance: u64,
    /// Whether the todo file was unreadable at the last observation, so a
    /// transition into unreadable can be logged once.
    todo_unreadable: bool,
    /// The turn the pane was first seen finished at, and when. The idle
    /// clock runs from here, so time spent working on that turn never
    /// counts as idle. `finished_seen` is tracked separately from the
    /// turn value: a backend may report a finished state with
    /// `turn = None`, and `None` must still count as "seen finished",
    /// not as "no finished turn yet".
    finished_turn: Option<u64>,
    finished_seen: bool,
    finished_since: Option<Instant>,
    consecutive: u32,
    last_nudge: Option<Instant>,
    gave_up: bool,
}

impl PaneWatch {
    /// A watch over a pane that runs in `cwd`. The working directory is
    /// resolved here, once, and every later todo read goes through that
    /// resolution.
    pub fn new(config: WatchdogConfig, cwd: PathBuf, agent: Agent, pane_id: String) -> Self {
        Self {
            config,
            work_dir: WorkDir::open(&cwd),
            agent,
            pane_id,
            last_turn: None,
            nudge_allowance: 0,
            todo_unreadable: false,
            finished_turn: None,
            finished_seen: false,
            finished_since: None,
            consecutive: 0,
            last_nudge: None,
            gave_up: false,
        }
    }

    pub fn config(&self) -> &WatchdogConfig {
        &self.config
    }

    /// A handle on the working directory this watch resolved when it was
    /// registered, for reading the todo file outside the map's lock.
    pub fn work_dir(&self) -> WorkDir {
        self.work_dir.try_clone()
    }

    pub fn pane_id(&self) -> &str {
        &self.pane_id
    }

    /// Record a todo reading; returns true when it is the first unreadable
    /// reading after a readable (or not-configured) one, so the caller can
    /// log the transition once instead of every tick.
    pub fn note_todo(&mut self, todo: Todo) -> bool {
        let unreadable = matches!(todo, Todo::Unreadable);
        let transition = unreadable && !self.todo_unreadable;
        self.todo_unreadable = unreadable;
        transition
    }

    /// How long the current finished turn has sat untouched, if one is
    /// being timed.
    pub fn idle_for(&self, now: Instant) -> Option<Duration> {
        self.finished_since
            .map(|since| now.saturating_duration_since(since))
    }

    /// The status state that is this watch's agent's finished turn, for
    /// conditioning a nudge send on the state that was observed.
    pub fn finished_state(&self) -> State {
        match self.agent {
            Agent::OpenCode => State::Idle,
            Agent::Kiro | Agent::Devin => State::Done,
        }
    }

    /// Whether this reading of the pane is a finished turn. Most agents
    /// report `done`; OpenCode reports no done, so an `idle` after at least
    /// one turn is its finished turn (the same rule `wait_status` applies).
    fn turn_finished(&self, info: &TerminalInfo) -> bool {
        let Some(status) = info.status.as_ref() else {
            return false;
        };
        match self.agent {
            Agent::OpenCode => {
                status.state == State::Idle && info.turn.is_some_and(|turn| turn > 0)
            }
            Agent::Kiro | Agent::Devin => status.state == State::Done,
        }
    }

    /// Observe the pane once. `todo` is the todo file's state at this
    /// observation ([`Todo::NotConfigured`] when the profile names none).
    pub fn observe(&mut self, info: &TerminalInfo, todo: Todo, now: Instant) -> WatchEvent {
        // Turn bookkeeping runs even while the watchdog is stopped for this
        // pane, because an advancing turn is what clears the stop. The one
        // exception is the turns a nudge itself causes: the agent that
        // answers a nudge runs a turn (or a few), and that advance is the
        // watchdog's own doing, not progress — it must not reset the cap,
        // or an agent that answers every nudge would be nudged forever.
        // What a nudge may explain is bounded by `nudge_allowance`; the cap
        // itself counts nudges sent, so it holds however many turns, or
        // none, each nudge produces.
        if info.turn != self.last_turn {
            if let (Some(before), Some(after)) = (self.last_turn, info.turn) {
                if after > before {
                    let advance = after - before;
                    if advance <= self.nudge_allowance {
                        self.nudge_allowance -= advance;
                    } else {
                        self.consecutive = 0;
                        self.gave_up = false;
                        self.nudge_allowance = 0;
                    }
                }
            }
            self.last_turn = info.turn;
        }
        let blocked = info
            .status
            .as_ref()
            .is_some_and(|status| status.state == State::Blocked);
        // A blocked pane is never nudged. The finished test already excludes
        // it (a finished state is never blocked); naming it here keeps the
        // rule in one visible place and restarts the idle clock afterwards.
        let finished = info.live && !blocked && self.turn_finished(info);
        if finished {
            if !self.finished_seen || self.finished_turn != info.turn {
                self.finished_turn = info.turn;
                self.finished_seen = true;
                self.finished_since = Some(now);
            }
        } else {
            self.finished_turn = None;
            self.finished_seen = false;
            self.finished_since = None;
        }
        if self.gave_up || !finished {
            return WatchEvent::None;
        }
        if matches!(todo, Todo::Finished | Todo::Unreadable) {
            return WatchEvent::None;
        }
        let Some(since) = self.finished_since else {
            return WatchEvent::None;
        };
        if now.saturating_duration_since(since) < self.config.idle_timeout {
            return WatchEvent::None;
        }
        if self
            .last_nudge
            .is_some_and(|at| now.saturating_duration_since(at) < self.config.cooldown)
        {
            return WatchEvent::None;
        }
        if self.consecutive >= self.config.max_consecutive_nudges {
            self.gave_up = true;
            return WatchEvent::GaveUp;
        }
        WatchEvent::Nudge(match todo {
            Todo::Unchecked => NudgeReason::TodoUnchecked,
            _ => NudgeReason::NoTodoFile,
        })
    }

    /// Record that the nudge text went in at `now`.
    pub fn note_nudged(&mut self, now: Instant) {
        self.consecutive += 1;
        self.last_nudge = Some(now);
        self.nudge_allowance = NUDGE_TURN_ALLOWANCE;
    }

    /// Record that a nudge was refused or failed at `now`. A failed attempt
    /// counts toward the cooldown and the consecutive cap, so a pane that
    /// refuses every nudge is not retried (and logged) on every 5 s tick
    /// forever.
    pub fn note_failed(&mut self, now: Instant) {
        self.consecutive += 1;
        self.last_nudge = Some(now);
    }

    /// Record that the nudge text was pasted into the pane but Enter was
    /// not pressed (the native backend's two-step send refused the second
    /// step). The text is sitting on the pane's input line: retrying
    /// would paste a second copy, and a person pressing Enter in the
    /// meantime would submit text the watchdog recorded as unsent. The
    /// watchdog therefore stops for this pane — as it does at the cap —
    /// until a turn advance the watchdog did not cause clears the stop.
    pub fn note_pasted_unsubmitted(&mut self, now: Instant) {
        self.consecutive += 1;
        self.last_nudge = Some(now);
        self.gave_up = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::ProgramStatus;
    use crate::testutil::TempDir;

    fn config() -> WatchdogConfig {
        WatchdogConfig {
            idle_timeout: Duration::from_secs(60),
            cooldown: Duration::from_secs(30),
            max_consecutive_nudges: 3,
            todo_file: None,
            nudge_text: "continue".to_string(),
        }
    }

    fn pane_watch(agent: Agent) -> PaneWatch {
        PaneWatch::new(config(), PathBuf::from("/work"), agent, "t1".to_string())
    }

    fn info(state: State, turn: Option<u64>) -> TerminalInfo {
        TerminalInfo {
            id: "t1".to_string(),
            name: Some("work".to_string()),
            live: true,
            exit_code: None,
            status: Some(ProgramStatus { state, kind: None }),
            reported: true,
            turn,
            status_ms: None,
            argv: None,
            cwd: None,
        }
    }

    /// `t0` plus `secs` seconds.
    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    #[test]
    fn a_done_pane_is_nudged_only_after_the_idle_timeout() {
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let done = info(State::Done, Some(3));
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, t0),
            WatchEvent::None
        );
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 59)),
            WatchEvent::None
        );
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 60)),
            WatchEvent::Nudge(NudgeReason::NoTodoFile)
        );
    }

    #[test]
    fn working_blocked_error_and_ended_panes_are_never_nudged() {
        let t0 = Instant::now();
        for state in [State::Working, State::Blocked, State::Error, State::Other] {
            let mut watch = pane_watch(Agent::Kiro);
            let pane = info(state.clone(), Some(3));
            assert_eq!(
                watch.observe(&pane, Todo::NotConfigured, t0),
                WatchEvent::None
            );
            assert_eq!(
                watch.observe(&pane, Todo::NotConfigured, at(t0, 3600)),
                WatchEvent::None,
                "{state:?}"
            );
        }
        let mut watch = pane_watch(Agent::Kiro);
        let mut ended = info(State::Done, Some(3));
        ended.live = false;
        assert_eq!(
            watch.observe(&ended, Todo::NotConfigured, t0),
            WatchEvent::None
        );
        assert_eq!(
            watch.observe(&ended, Todo::NotConfigured, at(t0, 3600)),
            WatchEvent::None
        );
        // A pane that never reported a status has no finished turn to see.
        let mut watch = pane_watch(Agent::Kiro);
        let mut silent = info(State::Done, Some(3));
        silent.status = None;
        assert_eq!(
            watch.observe(&silent, Todo::NotConfigured, at(t0, 3600)),
            WatchEvent::None
        );
    }

    #[test]
    fn an_idle_kiro_pane_is_not_a_finished_turn() {
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let idle = info(State::Idle, Some(3));
        assert_eq!(
            watch.observe(&idle, Todo::NotConfigured, at(t0, 3600)),
            WatchEvent::None
        );
    }

    #[test]
    fn an_opencode_idle_after_a_turn_is_its_finished_turn() {
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::OpenCode);
        // Idle before any turn: the pane just started.
        let fresh = info(State::Idle, Some(0));
        assert_eq!(
            watch.observe(&fresh, Todo::NotConfigured, at(t0, 3600)),
            WatchEvent::None
        );
        let mut watch = pane_watch(Agent::OpenCode);
        let done = info(State::Idle, Some(2));
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, t0),
            WatchEvent::None
        );
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 60)),
            WatchEvent::Nudge(NudgeReason::NoTodoFile)
        );
    }

    #[test]
    fn the_idle_clock_starts_when_the_turn_finishes() {
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        // Turn 1 in progress for an hour, then turn 2 finishes: the hour of
        // work is not idle time.
        assert_eq!(
            watch.observe(&info(State::Working, Some(1)), Todo::NotConfigured, t0),
            WatchEvent::None
        );
        let done = info(State::Done, Some(2));
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 3600)),
            WatchEvent::None
        );
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 3659)),
            WatchEvent::None
        );
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 3660)),
            WatchEvent::Nudge(NudgeReason::NoTodoFile)
        );
    }

    #[test]
    fn the_todo_file_decides_whether_work_is_left() {
        let t0 = Instant::now();
        let done = info(State::Done, Some(3));
        for (todo, want) in [
            (
                Todo::Unchecked,
                WatchEvent::Nudge(NudgeReason::TodoUnchecked),
            ),
            (Todo::Finished, WatchEvent::None),
            (Todo::Unreadable, WatchEvent::None),
        ] {
            let mut watch = pane_watch(Agent::Kiro);
            assert_eq!(watch.observe(&done, todo, t0), WatchEvent::None);
            assert_eq!(watch.observe(&done, todo, at(t0, 60)), want, "{todo:?}");
        }
    }

    #[test]
    fn cooldown_spaces_nudges_and_the_cap_stops_them() {
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let done = info(State::Done, Some(3));
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, t0),
            WatchEvent::None
        );
        // Nudge 1 at 60 s.
        assert!(matches!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 60)),
            WatchEvent::Nudge(_)
        ));
        watch.note_nudged(at(t0, 60));
        // Inside the 30 s cooldown: nothing, even though the pane is idle.
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 89)),
            WatchEvent::None
        );
        // Nudges 2 and 3 as each cooldown passes.
        for at_secs in [90, 120] {
            assert!(matches!(
                watch.observe(&done, Todo::NotConfigured, at(t0, at_secs)),
                WatchEvent::Nudge(_)
            ));
            watch.note_nudged(at(t0, at_secs));
        }
        // The cap is 3: the next due observation gives up, once.
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 150)),
            WatchEvent::GaveUp
        );
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 1000)),
            WatchEvent::None
        );
    }

    #[test]
    fn an_advancing_turn_clears_the_count_and_the_give_up() {
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let done = info(State::Done, Some(3));
        watch.observe(&done, Todo::NotConfigured, t0);
        for at_secs in [60, 90, 120] {
            assert!(matches!(
                watch.observe(&done, Todo::NotConfigured, at(t0, at_secs)),
                WatchEvent::Nudge(_)
            ));
            watch.note_nudged(at(t0, at_secs));
        }
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 150)),
            WatchEvent::GaveUp
        );
        // The agent picks the work up on its own and runs past the turns a
        // nudge could have caused: a jump from turn 3 to turn 9 is more
        // than a nudge explains, so it is progress and clears both the
        // count and the give-up.
        assert_eq!(
            watch.observe(
                &info(State::Working, Some(3)),
                Todo::NotConfigured,
                at(t0, 200)
            ),
            WatchEvent::None
        );
        let next = info(State::Done, Some(9));
        assert_eq!(
            watch.observe(&next, Todo::NotConfigured, at(t0, 300)),
            WatchEvent::None
        );
        assert_eq!(
            watch.observe(&next, Todo::NotConfigured, at(t0, 360)),
            WatchEvent::Nudge(NudgeReason::NoTodoFile)
        );
    }

    #[test]
    fn nudge_caused_turns_do_not_reset_the_cap() {
        // The loop the cap exists to stop: the agent answers every nudge
        // with a turn that finishes immediately, and each such turn is the
        // nudge's own turn + 1. The count must keep climbing to the cap.
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let mut turn = 3u64;
        let mut at_secs = 0u64;
        // First sighting starts the idle clock.
        assert_eq!(
            watch.observe(&info(State::Done, Some(turn)), Todo::NotConfigured, t0),
            WatchEvent::None
        );
        for _ in 0..3 {
            at_secs += 60;
            let done = info(State::Done, Some(turn));
            assert!(
                matches!(
                    watch.observe(&done, Todo::NotConfigured, at(t0, at_secs)),
                    WatchEvent::Nudge(_)
                ),
                "turn {turn}"
            );
            watch.note_nudged(at(t0, at_secs));
            // The agent answers the nudge: exactly one new finished turn.
            turn += 1;
            at_secs += 1;
            assert_eq!(
                watch.observe(
                    &info(State::Done, Some(turn)),
                    Todo::NotConfigured,
                    at(t0, at_secs)
                ),
                WatchEvent::None
            );
        }
        // Three nudges in, the next due observation gives up even though
        // the turn counter advanced after every nudge.
        at_secs += 60;
        assert_eq!(
            watch.observe(
                &info(State::Done, Some(turn)),
                Todo::NotConfigured,
                at(t0, at_secs)
            ),
            WatchEvent::GaveUp
        );
    }

    #[test]
    fn a_nudge_that_produces_several_turns_does_not_reset_the_cap() {
        // Each nudge makes the agent run two turns (the answer and a
        // follow-up), which the old one-nudge-one-turn rule took for
        // progress. The cap counts nudges sent, so it still holds.
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let mut turn = 3u64;
        let mut at_secs = 0u64;
        watch.observe(&info(State::Done, Some(turn)), Todo::NotConfigured, t0);
        for _ in 0..3 {
            at_secs += 60;
            assert!(matches!(
                watch.observe(
                    &info(State::Done, Some(turn)),
                    Todo::NotConfigured,
                    at(t0, at_secs)
                ),
                WatchEvent::Nudge(_)
            ));
            watch.note_nudged(at(t0, at_secs));
            // Two turns, seen in one observation.
            turn += 2;
            at_secs += 1;
            assert_eq!(
                watch.observe(
                    &info(State::Done, Some(turn)),
                    Todo::NotConfigured,
                    at(t0, at_secs)
                ),
                WatchEvent::None
            );
        }
        at_secs += 60;
        assert_eq!(
            watch.observe(
                &info(State::Done, Some(turn)),
                Todo::NotConfigured,
                at(t0, at_secs)
            ),
            WatchEvent::GaveUp
        );
    }

    #[test]
    fn a_nudge_whose_turns_are_seen_one_at_a_time_does_not_reset_the_cap() {
        // Same loop, but the two turns a nudge causes show up in separate
        // observations (working, then done, for each).
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let mut turn = 3u64;
        let mut at_secs = 0u64;
        watch.observe(&info(State::Done, Some(turn)), Todo::NotConfigured, t0);
        for _ in 0..3 {
            at_secs += 60;
            assert!(matches!(
                watch.observe(
                    &info(State::Done, Some(turn)),
                    Todo::NotConfigured,
                    at(t0, at_secs)
                ),
                WatchEvent::Nudge(_)
            ));
            watch.note_nudged(at(t0, at_secs));
            for _ in 0..2 {
                turn += 1;
                at_secs += 1;
                watch.observe(
                    &info(State::Working, Some(turn)),
                    Todo::NotConfigured,
                    at(t0, at_secs),
                );
            }
            at_secs += 1;
            watch.observe(
                &info(State::Done, Some(turn)),
                Todo::NotConfigured,
                at(t0, at_secs),
            );
        }
        at_secs += 60;
        assert_eq!(
            watch.observe(
                &info(State::Done, Some(turn)),
                Todo::NotConfigured,
                at(t0, at_secs)
            ),
            WatchEvent::GaveUp
        );
    }

    #[test]
    fn nudges_that_produce_no_turn_stop_at_the_cap_and_build_no_credit() {
        // The agent ignores every nudge: the turn never moves. The cap
        // counts nudges sent, so the third due observation gives up; and a
        // later advance past one nudge's allowance is still progress, not
        // absorbed by credit from the nudges that did nothing.
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let done = info(State::Done, Some(3));
        watch.observe(&done, Todo::NotConfigured, t0);
        for at_secs in [60, 90, 120] {
            assert!(matches!(
                watch.observe(&done, Todo::NotConfigured, at(t0, at_secs)),
                WatchEvent::Nudge(_)
            ));
            watch.note_nudged(at(t0, at_secs));
        }
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 150)),
            WatchEvent::GaveUp
        );
        let jump = 3 + NUDGE_TURN_ALLOWANCE + 1;
        let next = info(State::Done, Some(jump));
        watch.observe(&next, Todo::NotConfigured, at(t0, 200));
        assert!(matches!(
            watch.observe(&next, Todo::NotConfigured, at(t0, 260)),
            WatchEvent::Nudge(_)
        ));
    }

    #[test]
    fn read_todo_refuses_a_fifo_a_symlink_and_an_oversized_file() {
        let dir = TempDir::new("watchdog-todo-guards");
        // A FIFO would block a plain read forever; it is not a regular
        // file, so it is unreadable without reading.
        #[cfg(unix)]
        {
            let fifo = dir.path().join("fifo.md");
            let made = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo");
            assert!(made.success());
            assert_eq!(
                read_todo(dir.path(), Path::new("fifo.md")),
                Todo::Unreadable
            );
            // A symlink is refused even when its target is a normal todo
            // file inside the same directory.
            let target = dir.path().join("real.md");
            std::fs::write(&target, "- [ ] left\n").unwrap();
            let link = dir.path().join("link.md");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(
                read_todo(dir.path(), Path::new("link.md")),
                Todo::Unreadable
            );
        }
        // Over the read limit: unreadable, even with an unchecked item.
        let big = dir.path().join("big.md");
        let mut text = "- [ ] left\n".to_string();
        text.push_str(&"x".repeat(MAX_TODO_BYTES as usize));
        std::fs::write(&big, text).unwrap();
        assert_eq!(read_todo(dir.path(), Path::new("big.md")), Todo::Unreadable);
    }

    #[test]
    fn read_todo_refuses_a_symlinked_intermediate_directory() {
        // `O_NOFOLLOW` on the final component alone is not enough: with
        // `sub/TODO.md`, a `sub` that is a symlink would carry the read
        // outside the working directory. Every component is refused.
        #[cfg(unix)]
        {
            let dir = TempDir::new("watchdog-todo-symlink-dir");
            let outside = TempDir::new("watchdog-todo-outside");
            std::fs::write(outside.path().join("TODO.md"), "- [ ] left\n").unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("sub")).unwrap();
            assert_eq!(
                read_todo(dir.path(), Path::new("sub/TODO.md")),
                Todo::Unreadable
            );
            // A real subdirectory in between reads normally.
            let real = dir.path().join("real");
            std::fs::create_dir(&real).unwrap();
            std::fs::write(real.join("TODO.md"), "- [ ] left\n").unwrap();
            assert_eq!(
                read_todo(dir.path(), Path::new("real/TODO.md")),
                Todo::Unchecked
            );
        }
    }

    #[test]
    fn read_todo_reads_a_working_directory_reached_through_a_symlinked_ancestor() {
        // The boundary is the working directory, not the whole filesystem
        // path to it: on macOS /tmp and /var are symlinks into /private,
        // and a temp working directory must still read. Reaching the same
        // directory through an explicit symlinked ancestor is that case.
        #[cfg(unix)]
        {
            let dir = TempDir::new("watchdog-todo-symlink-ancestor");
            std::fs::write(dir.path().join("TODO.md"), "- [ ] left\n").unwrap();
            let alias_parent = TempDir::new("watchdog-todo-alias-parent");
            let alias = alias_parent.path().join("alias");
            std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
            assert_eq!(read_todo(&alias, Path::new("TODO.md")), Todo::Unchecked);
            // The same holds for the directory this test itself lives in
            // when its own ancestors contain a symlink (macOS temp dirs).
            assert_eq!(read_todo(dir.path(), Path::new("TODO.md")), Todo::Unchecked);
        }
    }

    #[test]
    fn a_working_directory_swapped_for_a_symlink_after_the_watch_started_is_not_followed() {
        // The directory is resolved once, when the watch opens it. If its
        // path is later replaced by a symlink to somewhere else (the real
        // directory moved aside), reads still come from the directory the
        // watch began with, never from the symlink's target.
        #[cfg(unix)]
        {
            let parent = TempDir::new("watchdog-todo-swap");
            let cwd = parent.path().join("work");
            std::fs::create_dir(&cwd).unwrap();
            std::fs::write(cwd.join("TODO.md"), "- [x] done\n").unwrap();
            let outside = parent.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("TODO.md"), "- [ ] planted\n").unwrap();
            let rel = Path::new("TODO.md");
            let work = WorkDir::open(&cwd);
            assert_eq!(read_todo_in(&work, rel), Todo::Finished);
            std::fs::rename(&cwd, parent.path().join("work.moved")).unwrap();
            std::os::unix::fs::symlink(&outside, &cwd).unwrap();
            // Control: a path-based read now follows the symlink.
            assert_eq!(read_todo(&cwd, rel), Todo::Unchecked);
            // The watch's own handle does not.
            assert_eq!(read_todo_in(&work, rel), Todo::Finished);
            assert_eq!(read_todo_in(&work.try_clone(), rel), Todo::Finished);
        }
    }

    #[test]
    fn unchecked_items_inside_a_fenced_code_block_do_not_count() {
        let dir = TempDir::new("watchdog-todo-fence");
        let file = dir.path().join("TODO.md");
        let rel = Path::new("TODO.md");
        std::fs::write(&file, "- [x] done\n```\n- [ ] example\n```\n").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Finished);
        std::fs::write(&file, "~~~md\n- [ ] example\n~~~\n- [ ] real\n").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Unchecked);
        // A fence that is never closed hides the rest, like Markdown.
        std::fs::write(&file, "```\n- [ ] example\n").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Finished);
        // A tilde line does not close a backtick fence.
        std::fs::write(&file, "```\n~~~\n- [ ] still fenced\n```\n").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Finished);
    }

    #[test]
    fn a_zero_cooldown_allows_the_next_nudge_at_the_next_observation() {
        let t0 = Instant::now();
        let mut watch = PaneWatch::new(
            WatchdogConfig {
                cooldown: Duration::ZERO,
                ..config()
            },
            PathBuf::from("/work"),
            Agent::Kiro,
            "t1".to_string(),
        );
        let done = info(State::Done, Some(3));
        watch.observe(&done, Todo::NotConfigured, t0);
        assert!(matches!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 60)),
            WatchEvent::Nudge(_)
        ));
        watch.note_nudged(at(t0, 60));
        assert!(matches!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 61)),
            WatchEvent::Nudge(_)
        ));
    }

    #[test]
    fn read_todo_counts_only_dash_unchecked_items() {
        let dir = TempDir::new("watchdog-todo");
        let file = dir.path().join("TODO.md");
        let rel = Path::new("TODO.md");
        std::fs::write(&file, "- [x] done\n- [ ] left\n").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Unchecked);
        // An indented item is still an item.
        std::fs::write(&file, "- [x] done\n  - [ ] nested left\n").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Unchecked);
        std::fs::write(&file, "- [x] done\n- [X] also done\n").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Finished);
        // Other bullets and prose are not items.
        std::fs::write(&file, "* [ ] star bullet\nsome - [ ] prose\n").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Finished);
        std::fs::write(&file, "").unwrap();
        assert_eq!(read_todo(dir.path(), rel), Todo::Finished);
        assert_eq!(
            read_todo(dir.path(), Path::new("missing.md")),
            Todo::Unreadable
        );
    }

    #[test]
    fn a_finished_turn_without_a_turn_number_still_becomes_idle() {
        // The trait allows a finished state with `turn = None`; "seen
        // finished" must not be keyed on the turn value, or such a pane
        // would never start its idle clock.
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let done = info(State::Done, None);
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, t0),
            WatchEvent::None
        );
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 60)),
            WatchEvent::Nudge(NudgeReason::NoTodoFile)
        );
    }

    #[test]
    fn a_pasted_but_unsubmitted_nudge_is_not_retried() {
        let t0 = Instant::now();
        let mut watch = pane_watch(Agent::Kiro);
        let done = info(State::Done, Some(3));
        watch.observe(&done, Todo::NotConfigured, t0);
        assert!(matches!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 60)),
            WatchEvent::Nudge(_)
        ));
        watch.note_pasted_unsubmitted(at(t0, 60));
        // Long past the cooldown, the watchdog still does not paste a
        // second copy onto the input line.
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 3600)),
            WatchEvent::None
        );
    }
}
