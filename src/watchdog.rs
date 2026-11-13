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

/// Read the todo file named by the relative path `rel` inside the pane's
/// working directory `cwd`. An unchecked item is a line whose first
/// non-blank characters are `- [ ]`; `- [x]`, other bullets and prose do
/// not count. (`- [ ]` inside a fenced code block counts too; the check is
/// a plain line scan, not a Markdown parse.)
///
/// The file is read defensively: the supervised agent writes in the same
/// directory, so the path could be a FIFO (which would block a plain read
/// forever and stall the single watchdog thread), a symlink out of the
/// directory, or an unbounded file. The working directory is the boundary
/// the read enforces: on Unix, `cwd` itself is opened as a directory fd
/// (its ancestors are resolved by that open, so a working directory
/// reached through a symlinked ancestor — `/tmp` on macOS, say — reads
/// normally), and only the components of `rel` are then walked with
/// `openat` and `O_NOFOLLOW`, so a symlink in any component of `rel` —
/// an intermediate directory, not just the file itself — is refused, and
/// no component can be swapped for a symlink between a check and the
/// open. The handle must be a regular file, and at most
/// [`MAX_TODO_BYTES`] are read. Any failure, or a file over the limit,
/// maps to [`Todo::Unreadable`]: fail closed, no nudge. (On non-Unix the
/// read is a plain open of `cwd.join(rel)` and follows symlinks; the
/// symlink refusal above is Unix-only.)
pub fn read_todo(cwd: &Path, rel: &Path) -> Todo {
    use std::io::Read as _;
    #[cfg(unix)]
    let opened = open_todo_refusing_symlinks(cwd, rel);
    #[cfg(not(unix))]
    let opened = fs::File::open(cwd.join(rel)).ok();
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
    if text
        .lines()
        .any(|line| line.trim_start().starts_with("- [ ]"))
    {
        Todo::Unchecked
    } else {
        Todo::Finished
    }
}

/// Open the todo file `rel` inside `cwd` for reading, refusing a symlink
/// in every component of `rel`. The working directory is the boundary:
/// `cwd` is opened as a directory fd with a plain open (which resolves
/// any symlinked ancestors of the working directory itself — the walk
/// does not police directories outside the boundary it enforces), and
/// each component of `rel` is then opened relative to the previous one
/// with `O_NOFOLLOW`, so the walk never follows a link inside the
/// boundary and no component can be swapped for a symlink between a
/// check and the open. `..` and absolute components are refused
/// outright; profiles already reject them, and a reader that resolves
/// them component by component has no safe meaning for them here.
#[cfg(unix)]
fn open_todo_refusing_symlinks(cwd: &Path, rel: &Path) -> Option<fs::File> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::io::FromRawFd as _;
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
    let cwd_c = CString::new(cwd.as_os_str().as_bytes()).ok()?;
    // SAFETY: every fd returned below is checked before use and closed
    // exactly once; the final fd's ownership passes to the returned File.
    let mut fd = unsafe {
        libc::open(
            cwd_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    for dir in dirs {
        let next = unsafe {
            libc::openat(
                fd,
                dir.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        unsafe { libc::close(fd) };
        if next < 0 {
            return None;
        }
        fd = next;
    }
    let file_fd = unsafe {
        libc::openat(
            fd,
            last.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    unsafe { libc::close(fd) };
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
    cwd: PathBuf,
    agent: Agent,
    /// The backend id of the pane this watch was registered for. A name
    /// reused by a later start is a different pane and never this watch's.
    pane_id: String,
    /// The turn counter at the last observation. A turn that advances on
    /// its own clears the consecutive count and a give-up: progress is the
    /// reset. The turn a nudge itself causes (nudged turn + 1) does not
    /// clear anything — otherwise every answered nudge would reset the
    /// cap and the watchdog would nudge forever.
    last_turn: Option<u64>,
    /// The turn the last successful nudge was sent on.
    nudged_turn: Option<u64>,
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
    pub fn new(config: WatchdogConfig, cwd: PathBuf, agent: Agent, pane_id: String) -> Self {
        Self {
            config,
            cwd,
            agent,
            pane_id,
            last_turn: None,
            nudged_turn: None,
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

    pub fn cwd(&self) -> &Path {
        &self.cwd
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
        // exception is the turn a nudge itself causes: a nudge sent on turn
        // N makes the agent run turn N+1, and that advance is the watchdog's
        // own doing, not progress — it must not reset the cap, or an agent
        // that answers every nudge would be nudged forever.
        if info.turn != self.last_turn {
            if let (Some(before), Some(after)) = (self.last_turn, info.turn) {
                let nudge_caused = self.nudged_turn == Some(before) && after == before + 1;
                if after > before && !nudge_caused {
                    self.consecutive = 0;
                    self.gave_up = false;
                    self.nudged_turn = None;
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

    /// Record that the nudge text went in at `now`, on turn `turn`.
    pub fn note_nudged(&mut self, now: Instant, turn: Option<u64>) {
        self.consecutive += 1;
        self.last_nudge = Some(now);
        self.nudged_turn = turn;
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
        watch.note_nudged(at(t0, 60), Some(3));
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
            watch.note_nudged(at(t0, at_secs), Some(3));
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
            watch.note_nudged(at(t0, at_secs), Some(3));
        }
        assert_eq!(
            watch.observe(&done, Todo::NotConfigured, at(t0, 150)),
            WatchEvent::GaveUp
        );
        // The agent picks the work up on its own and runs past the turn a
        // nudge could have caused: a jump from turn 3 to turn 5 is not the
        // single step a nudge on turn 3 would produce, so it is progress
        // and clears both the count and the give-up.
        assert_eq!(
            watch.observe(
                &info(State::Working, Some(3)),
                Todo::NotConfigured,
                at(t0, 200)
            ),
            WatchEvent::None
        );
        let next = info(State::Done, Some(5));
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
            watch.note_nudged(at(t0, at_secs), Some(turn));
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
        watch.note_nudged(at(t0, 60), Some(3));
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
