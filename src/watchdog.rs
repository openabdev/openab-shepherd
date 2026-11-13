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

/// Read the todo file at `path`. An unchecked item is a line whose first
/// non-blank characters are `- [ ]`; `- [x]`, other bullets and prose do
/// not count.
pub fn read_todo(path: &Path) -> Todo {
    match fs::read_to_string(path) {
        Ok(text) => {
            if text
                .lines()
                .any(|line| line.trim_start().starts_with("- [ ]"))
            {
                Todo::Unchecked
            } else {
                Todo::Finished
            }
        }
        Err(_) => Todo::Unreadable,
    }
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
    /// The turn counter at the last observation. A turn that advances clears
    /// the consecutive count and a give-up: progress is the reset.
    last_turn: Option<u64>,
    /// The turn the pane was first seen finished at, and when. The idle
    /// clock runs from here, so time spent working on that turn never
    /// counts as idle.
    finished_turn: Option<u64>,
    finished_since: Option<Instant>,
    consecutive: u32,
    last_nudge: Option<Instant>,
    gave_up: bool,
}

impl PaneWatch {
    pub fn new(config: WatchdogConfig, cwd: PathBuf, agent: Agent) -> Self {
        Self {
            config,
            cwd,
            agent,
            last_turn: None,
            finished_turn: None,
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

    pub fn agent(&self) -> Agent {
        self.agent
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
        // pane, because an advancing turn is what clears the stop.
        if info.turn != self.last_turn {
            if let (Some(before), Some(after)) = (self.last_turn, info.turn) {
                if after > before {
                    self.consecutive = 0;
                    self.gave_up = false;
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
            if self.finished_turn != info.turn {
                self.finished_turn = info.turn;
                self.finished_since = Some(now);
            }
        } else {
            self.finished_turn = None;
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
        PaneWatch::new(config(), PathBuf::from("/work"), agent)
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
        // The agent picks the work up: working, then done at the next turn.
        assert_eq!(
            watch.observe(
                &info(State::Working, Some(3)),
                Todo::NotConfigured,
                at(t0, 200)
            ),
            WatchEvent::None
        );
        let next = info(State::Done, Some(4));
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
    fn read_todo_counts_only_dash_unchecked_items() {
        let dir = TempDir::new("watchdog-todo");
        let file = dir.path().join("TODO.md");
        std::fs::write(&file, "- [x] done\n- [ ] left\n").unwrap();
        assert_eq!(read_todo(&file), Todo::Unchecked);
        // An indented item is still an item.
        std::fs::write(&file, "- [x] done\n  - [ ] nested left\n").unwrap();
        assert_eq!(read_todo(&file), Todo::Unchecked);
        std::fs::write(&file, "- [x] done\n- [X] also done\n").unwrap();
        assert_eq!(read_todo(&file), Todo::Finished);
        // Other bullets and prose are not items.
        std::fs::write(&file, "* [ ] star bullet\nsome - [ ] prose\n").unwrap();
        assert_eq!(read_todo(&file), Todo::Finished);
        std::fs::write(&file, "").unwrap();
        assert_eq!(read_todo(&file), Todo::Finished);
        assert_eq!(read_todo(&dir.path().join("missing.md")), Todo::Unreadable);
    }
}
