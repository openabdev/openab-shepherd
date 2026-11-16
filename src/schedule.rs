//! The `every` scheduler's pure half (issue #98): when a routine is due,
//! what a tick changes, and the per-pane set of routines. The tick itself
//! (reading the pane, the guards, the send) lives in [`crate::tools`],
//! beside the idle routine's.
//!
//! An `every` routine fires on a grid anchored at its creation time:
//! `created_at + k * every_secs`. The next grid time (`next_due`) is stored
//! with the routine, so a restart restores the same grid. Three rules keep
//! it from piling work on the agent:
//!
//! * A tick that finds the pane busy, blocked, replayed or held by a person
//!   is skipped and consumed. It is never queued.
//! * A tick missed while Shepherd was down or the machine slept is skipped.
//!   Nothing catches up; the routine resumes on the next grid time.
//! * The idle routine's consecutive-nudge cap does not apply. The next fire
//!   comes from the clock, not from the agent's own progress, so the cap
//!   would end an hourly routine after a few hours. Interval floor, expiry,
//!   per-pane count, `max_runs` and the give-up after failed sends bound it.
//!
//! Times here are unix seconds from an injected clock; nothing in this
//! module reads the system clock.

use crate::agent::Agent;
use crate::profiles::DynamicRoutines;
use crate::registry::{RoutineRecord, RoutineSet};
use crate::routine::WorkDir;
use crate::routine_model::{
    check_count, validate_every_secs, validate_expires_secs, validate_max_runs, validate_text,
    validate_todo_file, Action, Invalid, Routine, Status, Trigger,
};
use std::path::Path;

/// Consecutive failed sends after which a routine gives up.
pub const GIVE_UP_AFTER: u32 = 3;

/// A pane keeps at most this many times `max_per_pane` records, finished
/// ones included; a create past it evicts the oldest finished record.
pub const RECORD_FACTOR: usize = 2;

/// The first grid time after `wall`: `anchor + k * every` with the least
/// `k` that makes it later than `wall`. A clock before the anchor gives the
/// first grid time after the anchor.
pub fn next_boundary(anchor: u64, every: u64, wall: u64) -> u64 {
    let every = every.max(1);
    let elapsed = wall.saturating_sub(anchor);
    anchor.saturating_add((elapsed / every).saturating_add(1).saturating_mul(every))
}

/// Why [`assess`] changed a routine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Note {
    Expired,
    Done,
    /// More than one interval late: the missed tick is skipped.
    Missed,
    /// `next_due` was further away than one interval (the clock went back).
    Clamped,
}

/// What looking at a routine at `wall` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to do, nothing changed.
    NotYet,
    /// The routine changed (and must be kept); it does not fire now.
    Changed(Note),
    /// Fire now (if the pane allows it).
    Due,
}

/// The cheap checks, with no pane read: expiry, `max_runs`, a clock that
/// went back, a tick not yet due, a tick missed by more than one interval.
pub fn assess(routine: &mut Routine, wall: u64) -> Verdict {
    if !routine.status.is_active() {
        return Verdict::NotYet;
    }
    let Trigger::Every { every_secs } = routine.trigger else {
        return Verdict::NotYet;
    };
    if routine.expires_at.is_some_and(|at| wall >= at) {
        routine.status = Status::Expired;
        return Verdict::Changed(Note::Expired);
    }
    if routine
        .max_runs
        .is_some_and(|max| routine.runs >= u64::from(max))
    {
        routine.status = Status::Done;
        return Verdict::Changed(Note::Done);
    }
    let Some(due) = routine.next_due else {
        return Verdict::NotYet;
    };
    if due > wall.saturating_add(every_secs) {
        routine.next_due = Some(wall.saturating_add(every_secs));
        return Verdict::Changed(Note::Clamped);
    }
    if wall < due {
        return Verdict::NotYet;
    }
    if wall > due.saturating_add(every_secs) {
        routine.next_due = Some(next_boundary(routine.created_at, every_secs, wall));
        return Verdict::Changed(Note::Missed);
    }
    Verdict::Due
}

/// Consume the tick that is due: move `next_due` to the next grid time after
/// `wall`. Returns the previous `next_due`, for [`restore_due`].
pub fn advance(routine: &mut Routine, wall: u64) -> Option<u64> {
    let Trigger::Every { every_secs } = routine.trigger else {
        return None;
    };
    routine
        .next_due
        .replace(next_boundary(routine.created_at, every_secs, wall))
}

/// Undo [`advance`] in memory (`pane_busy` is not a decision, so the next
/// pass retries).
pub fn restore_due(routine: &mut Routine, previous: Option<u64>) {
    routine.next_due = previous;
}

/// The tick was skipped for `reason` (`working`, `blocked`, ...).
pub fn record_skip(routine: &mut Routine, reason: &str) {
    set_result(routine, format!("skipped:{reason}"));
}

/// The text went in at `wall`.
pub fn record_sent(routine: &mut Routine, wall: u64) {
    routine.runs += 1;
    routine.fail_streak = 0;
    routine.last_run_at = Some(wall);
    routine.last_error = None;
    set_result(routine, "sent".to_string());
    if routine
        .max_runs
        .is_some_and(|max| routine.runs >= u64::from(max))
    {
        routine.status = Status::Done;
    }
}

/// A send failed with `code`. Returns true when the routine gave up.
pub fn record_failure(routine: &mut Routine, code: &str, detail: &str) -> bool {
    routine.fail_streak += 1;
    set_result(routine, format!("failed:{code}"));
    routine.last_error = Some(short(detail));
    if routine.fail_streak >= GIVE_UP_AFTER {
        routine.status = Status::GaveUp;
        return true;
    }
    false
}

/// A send that must not be retried (the text sits pasted on the input line):
/// give up at once.
pub fn record_give_up(routine: &mut Routine, code: &str, detail: &str) {
    routine.fail_streak += 1;
    set_result(routine, format!("failed:{code}"));
    routine.last_error = Some(short(detail));
    routine.status = Status::GaveUp;
}

fn set_result(routine: &mut Routine, result: String) {
    if routine.last_result.as_deref() != Some(result.as_str()) {
        routine.last_result = Some(result);
    }
}

fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

/// What a create asks for. Validated against the profile's ceiling.
#[derive(Debug, Clone)]
pub struct NewEvery {
    pub every_secs: u64,
    pub text: String,
    pub todo_file: Option<String>,
    pub expires_secs: Option<u64>,
    pub max_runs: Option<u64>,
    pub created_by: String,
}

/// Why a create was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    /// The pane has no `dynamic_routines` ceiling, or is not a pane this
    /// process knows.
    NotAllowed,
    /// A value outside its limit.
    Invalid(Invalid),
    /// `max_per_pane` active routines already.
    Limit(Invalid),
    /// The write to the registry failed; nothing changed.
    NotSaved(String),
}

/// What an update asks for. `Some(None)` clears `todo_file` / `max_runs`.
#[derive(Debug, Clone, Default)]
pub struct EveryUpdate {
    pub every_secs: Option<u64>,
    pub text: Option<String>,
    pub todo_file: Option<Option<String>>,
    pub expires_secs: Option<u64>,
    pub max_runs: Option<Option<u64>>,
}

/// Why an update or delete was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateError {
    /// The pane has no routine with that id (or no schedule at all).
    NotFound,
    Invalid(Invalid),
    /// Renewing would exceed `max_per_pane`.
    Limit(Invalid),
    /// The write to the registry failed; nothing changed.
    NotSaved(String),
}

fn current_every(routine: &Routine) -> u64 {
    match routine.trigger {
        Trigger::Every { every_secs } => every_secs,
        Trigger::Idle { .. } => 0,
    }
}

fn no_local_dir() -> Invalid {
    Invalid {
        field: "todo_file",
        limit: "a pane with a local working directory".to_string(),
        message: "todo_file needs a pane with a local working directory".to_string(),
    }
}

/// One pane's routines of the `every` kind, and what the tick needs to act
/// on them. Kept in memory and mirrored to the registry whole.
#[derive(Debug)]
pub struct PaneSchedule {
    pane_id: String,
    agent: Agent,
    work_dir: Option<WorkDir>,
    seq: u64,
    routines: Vec<Routine>,
}

impl PaneSchedule {
    /// An empty schedule over a pane that runs in `cwd` (`None` when the
    /// pane has no local working directory: no `todo_file` then).
    pub fn new(pane_id: String, agent: Agent, cwd: Option<&Path>) -> Self {
        Self {
            pane_id,
            agent,
            work_dir: cwd.map(WorkDir::open),
            seq: 0,
            routines: Vec::new(),
        }
    }

    /// A schedule read back from the registry. A record this build cannot
    /// read (a status it does not know) is dropped, with a note.
    pub fn restored(
        pane_id: String,
        agent: Agent,
        cwd: Option<&Path>,
        set: &RoutineSet,
    ) -> (Self, Vec<String>) {
        let mut schedule = Self::new(pane_id, agent, cwd);
        schedule.seq = set.seq;
        let mut notes = Vec::new();
        for record in &set.items {
            match Routine::from_record(record) {
                Some(routine) => schedule.routines.push(routine),
                None => notes.push(format!(
                    "routine {} has the status {:?}, which this build does not know; dropped",
                    record.id, record.status
                )),
            }
        }
        (schedule, notes)
    }

    pub fn pane_id(&self) -> &str {
        &self.pane_id
    }

    pub fn agent(&self) -> Agent {
        self.agent
    }

    /// A second handle on the working directory, for a read made outside the
    /// map's lock. `None` for a pane with no local directory.
    pub fn work_dir(&self) -> Option<WorkDir> {
        self.work_dir.as_ref().map(WorkDir::try_clone)
    }

    pub fn routines(&self) -> &[Routine] {
        &self.routines
    }

    pub fn routines_mut(&mut self) -> &mut [Routine] {
        &mut self.routines
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Routine> {
        self.routines.iter_mut().find(|r| r.id == id)
    }

    pub fn active_count(&self) -> usize {
        self.routines
            .iter()
            .filter(|r| r.status.is_active())
            .count()
    }

    /// The stored form of the whole set.
    pub fn to_set(&self) -> RoutineSet {
        RoutineSet {
            seq: self.seq,
            items: self
                .routines
                .iter()
                .filter_map(Routine::to_record)
                .collect::<Vec<RoutineRecord>>(),
        }
    }

    /// The adoption rule: a routine the current ceiling no longer allows
    /// becomes `stopped / profile_changed`. `None` (the key is gone) stops
    /// every active routine. Past a lowered `max_per_pane`, the newest are
    /// stopped. Returns the ids stopped, one line each for the caller.
    pub fn apply_ceiling(&mut self, ceiling: Option<&DynamicRoutines>) -> Vec<(String, String)> {
        let mut stopped = Vec::new();
        let agent = self.agent;
        for routine in self.routines.iter_mut().filter(|r| r.status.is_active()) {
            let why = match (ceiling, routine.to_record()) {
                (None, _) => Some("the profile no longer allows dynamic routines".to_string()),
                (Some(ceiling), Some(record)) => {
                    crate::routine_model::ceiling_violation(&record, ceiling, agent)
                }
                (Some(_), None) => None,
            };
            if let Some(why) = why {
                stop_profile_changed(routine);
                stopped.push((routine.id.clone(), why));
            }
        }
        if let Some(ceiling) = ceiling {
            let max = usize::try_from(ceiling.max_per_pane).unwrap_or(usize::MAX);
            let mut active: Vec<usize> = (0..self.routines.len())
                .filter(|&i| self.routines[i].status.is_active())
                .collect();
            active.sort_by_key(|&i| (self.routines[i].created_at, id_number(&self.routines[i].id)));
            for &i in active.iter().skip(max) {
                stop_profile_changed(&mut self.routines[i]);
                stopped.push((
                    self.routines[i].id.clone(),
                    format!("the profile now allows {max} routines per pane"),
                ));
            }
        }
        stopped
    }

    /// Check a create against `ceiling` and plan it: the routines and
    /// counter as they would be, and the new routine. Nothing changes here;
    /// the caller keeps the plan after the registry accepted it.
    pub fn plan_create(
        &self,
        request: &NewEvery,
        ceiling: &DynamicRoutines,
        wall: u64,
    ) -> Result<(Vec<Routine>, u64, Routine), CreateError> {
        let text = validate_text(&request.text, self.agent).map_err(CreateError::Invalid)?;
        let every_secs =
            validate_every_secs(request.every_secs, ceiling).map_err(CreateError::Invalid)?;
        let expires_secs =
            validate_expires_secs(request.expires_secs, ceiling).map_err(CreateError::Invalid)?;
        let max_runs = request
            .max_runs
            .map(validate_max_runs)
            .transpose()
            .map_err(CreateError::Invalid)?;
        let todo_file = request
            .todo_file
            .as_deref()
            .map(validate_todo_file)
            .transpose()
            .map_err(CreateError::Invalid)?;
        if todo_file.is_some() && self.work_dir.is_none() {
            return Err(CreateError::Invalid(Invalid {
                field: "todo_file",
                limit: "a pane with a local working directory".to_string(),
                message: "todo_file needs a pane with a local working directory".to_string(),
            }));
        }
        check_count(self.active_count(), ceiling).map_err(CreateError::Limit)?;
        let mut routines = self.routines.clone();
        let mut seq = self.seq;
        seq += 1;
        let routine = Routine {
            id: format!("r{seq}"),
            trigger: Trigger::Every { every_secs },
            action: Action::Type { text },
            todo_file,
            created_by: request.created_by.clone(),
            created_at: wall,
            expires_at: Some(wall.saturating_add(expires_secs)),
            max_runs,
            status: Status::Active,
            stop_reason: None,
            runs: 0,
            fail_streak: 0,
            last_run_at: None,
            last_result: None,
            last_error: None,
            next_due: Some(wall.saturating_add(every_secs)),
        };
        routines.push(routine.clone());
        // The oldest finished record goes first when the pane would hold more
        // than its share.
        let limit = usize::try_from(ceiling.max_per_pane)
            .unwrap_or(usize::MAX)
            .saturating_mul(RECORD_FACTOR);
        while routines.len() > limit {
            let Some(oldest) = routines
                .iter()
                .enumerate()
                .filter(|(_, r)| !r.status.is_active())
                .min_by_key(|(_, r)| (r.created_at, id_number(&r.id)))
                .map(|(i, _)| i)
            else {
                break;
            };
            routines.remove(oldest);
        }
        Ok((routines, seq, routine))
    }

    /// Check an update against `ceiling` and plan it: the routines as they
    /// would be, and the changed routine. Nothing changes here.
    ///
    /// A new `every_secs` or `expires_secs` re-anchors the grid at `wall`
    /// (`created_at` and `next_due` move), so the expiry horizon the
    /// adoption rule measures from creation stays true. An `expires_secs` on
    /// a finished routine brings it back to `active` (the renew path) when
    /// the pane has room and the result still fits the ceiling.
    pub fn plan_update(
        &self,
        id: &str,
        request: &EveryUpdate,
        ceiling: &DynamicRoutines,
        wall: u64,
    ) -> Result<(Vec<Routine>, Routine), UpdateError> {
        let index = self
            .routines
            .iter()
            .position(|r| r.id == id)
            .ok_or(UpdateError::NotFound)?;
        let mut routine = self.routines[index].clone();
        let invalid = UpdateError::Invalid;
        if let Some(text) = &request.text {
            routine.action = Action::Type {
                text: validate_text(text, self.agent).map_err(invalid)?,
            };
        }
        match &request.todo_file {
            Some(Some(file)) => {
                let file = validate_todo_file(file).map_err(invalid)?;
                if self.work_dir.is_none() {
                    return Err(invalid(no_local_dir()));
                }
                routine.todo_file = Some(file);
            }
            Some(None) => routine.todo_file = None,
            None => {}
        }
        match request.max_runs {
            Some(Some(runs)) => routine.max_runs = Some(validate_max_runs(runs).map_err(invalid)?),
            Some(None) => routine.max_runs = None,
            None => {}
        }
        let mut reanchor = false;
        if let Some(secs) = request.every_secs {
            let secs = validate_every_secs(secs, ceiling).map_err(invalid)?;
            reanchor |= secs != current_every(&routine);
            routine.trigger = Trigger::Every { every_secs: secs };
        }
        if let Some(secs) = request.expires_secs {
            let secs = validate_expires_secs(Some(secs), ceiling).map_err(invalid)?;
            routine.expires_at = Some(wall.saturating_add(secs));
            reanchor = true;
            if !routine.status.is_active() {
                let others = self
                    .routines
                    .iter()
                    .filter(|r| r.id != id && r.status.is_active())
                    .count();
                check_count(others, ceiling).map_err(UpdateError::Limit)?;
                routine.status = Status::Active;
                routine.stop_reason = None;
                routine.fail_streak = 0;
            }
        }
        if reanchor {
            let every = current_every(&routine);
            routine.created_at = wall;
            routine.next_due = Some(wall.saturating_add(every));
        }
        if routine.status.is_active() {
            if let Some(record) = routine.to_record() {
                if let Some(why) =
                    crate::routine_model::ceiling_violation(&record, ceiling, self.agent)
                {
                    return Err(invalid(Invalid {
                        field: "routine",
                        limit: "the profile's dynamic_routines ceiling".to_string(),
                        message: format!("the routine would break the profile's ceiling: {why}"),
                    }));
                }
            }
            if routine
                .max_runs
                .is_some_and(|max| routine.runs >= u64::from(max))
            {
                return Err(invalid(Invalid {
                    field: "max_runs",
                    limit: format!("more than the {} runs so far", routine.runs),
                    message: format!(
                        "max_runs must exceed the {} runs this routine has made",
                        routine.runs
                    ),
                }));
            }
        }
        let mut routines = self.routines.clone();
        routines[index] = routine.clone();
        Ok((routines, routine))
    }

    /// Plan removing routine `id`: the routines as they would be.
    pub fn plan_delete(&self, id: &str) -> Result<Vec<Routine>, UpdateError> {
        if !self.routines.iter().any(|r| r.id == id) {
            return Err(UpdateError::NotFound);
        }
        Ok(self
            .routines
            .iter()
            .filter(|r| r.id != id)
            .cloned()
            .collect())
    }

    /// Keep what [`PaneSchedule::plan_create`] planned.
    pub fn commit(&mut self, routines: Vec<Routine>, seq: u64) {
        self.routines = routines;
        self.seq = seq;
    }

    /// Keep what an update or delete planned; the counter stays.
    pub fn commit_routines(&mut self, routines: Vec<Routine>) {
        self.routines = routines;
    }

    /// The set as it would be stored with `routines` in place.
    pub fn set_with(&self, routines: &[Routine]) -> RoutineSet {
        RoutineSet {
            seq: self.seq,
            items: routines.iter().filter_map(Routine::to_record).collect(),
        }
    }
}

fn stop_profile_changed(routine: &mut Routine) {
    routine.status = Status::Stopped;
    routine.stop_reason = Some(crate::routine_model::PROFILE_CHANGED.to_string());
}

fn id_number(id: &str) -> u64 {
    crate::registry::routine_seq(id).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routine(every: u64, created_at: u64) -> Routine {
        let ceiling = DynamicRoutines {
            min_every_secs: 60,
            ..DynamicRoutines::default()
        };
        let schedule = PaneSchedule::new("id-work".into(), Agent::Kiro, None);
        let request = NewEvery {
            every_secs: every,
            text: "check the todo list".into(),
            todo_file: None,
            expires_secs: Some(100_000),
            max_runs: None,
            created_by: "agent-a".into(),
        };
        schedule
            .plan_create(&request, &ceiling, created_at)
            .unwrap()
            .2
    }

    #[test]
    fn the_next_boundary_is_the_first_grid_time_after_the_clock() {
        assert_eq!(next_boundary(1_000, 600, 1_000), 1_600);
        assert_eq!(next_boundary(1_000, 600, 1_599), 1_600);
        assert_eq!(next_boundary(1_000, 600, 1_600), 2_200);
        assert_eq!(next_boundary(1_000, 600, 10_000), 10_600);
        // A clock before the anchor still gives a grid time after it.
        assert_eq!(next_boundary(1_000, 600, 10), 1_600);
    }

    #[test]
    fn a_routine_is_not_due_before_next_due_and_is_due_at_it() {
        let mut r = routine(600, 1_000);
        assert_eq!(r.next_due, Some(1_600));
        assert_eq!(assess(&mut r, 1_599), Verdict::NotYet);
        assert_eq!(assess(&mut r, 1_600), Verdict::Due);
        assert_eq!(assess(&mut r, 1_700), Verdict::Due);
        assert_eq!(
            r.next_due,
            Some(1_600),
            "assessing a due routine changes nothing"
        );
    }

    #[test]
    fn a_tick_late_by_more_than_one_interval_is_skipped_not_caught_up() {
        let mut r = routine(600, 1_000);
        // Exactly one interval late still fires.
        assert_eq!(assess(&mut r, 1_600 + 600), Verdict::Due);
        // A moment more and the missed tick is dropped; the grid goes on.
        assert_eq!(assess(&mut r, 1_600 + 601), Verdict::Changed(Note::Missed));
        assert_eq!(r.next_due, Some(2_800));
        assert_eq!(r.runs, 0);
        // Three hours of downtime: zero fires, back on the grid.
        let mut r = routine(3_600, 0);
        assert_eq!(
            assess(&mut r, 3 * 3_600 + 7),
            Verdict::Changed(Note::Missed)
        );
        assert_eq!(r.next_due, Some(4 * 3_600));
        assert_eq!(assess(&mut r, 3 * 3_600 + 8), Verdict::NotYet);
        assert_eq!(assess(&mut r, 4 * 3_600), Verdict::Due);
    }

    #[test]
    fn a_clock_that_went_back_clamps_next_due_to_one_interval() {
        let mut r = routine(600, 1_000_000);
        r.next_due = Some(1_000_000 + 600);
        assert_eq!(assess(&mut r, 100), Verdict::Changed(Note::Clamped));
        assert_eq!(r.next_due, Some(700));
    }

    #[test]
    fn expiry_and_max_runs_stop_a_routine() {
        let mut r = routine(600, 1_000);
        let at = r.expires_at.unwrap();
        r.next_due = Some(at);
        assert_eq!(assess(&mut r, at - 1), Verdict::NotYet);
        assert_eq!(assess(&mut r, at), Verdict::Changed(Note::Expired));
        assert_eq!(r.status, Status::Expired);
        assert_eq!(
            assess(&mut r, at + 1),
            Verdict::NotYet,
            "finished stays finished"
        );

        let mut r = routine(600, 1_000);
        r.max_runs = Some(2);
        record_sent(&mut r, 1_600);
        assert_eq!(r.status, Status::Active);
        record_sent(&mut r, 2_200);
        assert_eq!(r.status, Status::Done);
        assert_eq!(assess(&mut r, 2_800), Verdict::NotYet);
    }

    #[test]
    fn the_grid_does_not_drift_over_twenty_ticks_with_jitter() {
        let mut r = routine(600, 1_000);
        let jitter = [
            3u64, 0, 2, 1, 3, 0, 1, 2, 3, 0, 2, 1, 3, 1, 0, 2, 3, 1, 2, 0,
        ];
        let mut fired = Vec::new();
        for (k, j) in jitter.iter().enumerate() {
            // The tick arrives up to 3 s after the grid time.
            let wall = 1_000 + 600 * (k as u64 + 1) + j;
            assert_eq!(assess(&mut r, wall), Verdict::Due, "tick {k}");
            let previous = advance(&mut r, wall);
            assert_eq!(previous, Some(1_000 + 600 * (k as u64 + 1)));
            record_sent(&mut r, wall);
            fired.push(r.next_due.unwrap());
        }
        for (k, due) in fired.iter().enumerate() {
            assert_eq!(*due, 1_000 + 600 * (k as u64 + 2), "drift at tick {k}");
        }
        assert_eq!(r.runs, 20);
    }

    #[test]
    fn a_todo_file_needs_a_local_working_directory() {
        let schedule = PaneSchedule::new("id-work".into(), Agent::Kiro, None);
        let request = NewEvery {
            every_secs: 600,
            text: "check the todo list".into(),
            todo_file: Some("TODO.md".into()),
            expires_secs: None,
            max_runs: None,
            created_by: "agent-a".into(),
        };
        let err = schedule
            .plan_create(&request, &DynamicRoutines::default(), 1_000)
            .unwrap_err();
        assert!(
            matches!(&err, CreateError::Invalid(i) if i.field == "todo_file"),
            "{err:?}"
        );
        let dir = crate::testutil::TempDir::new("schedule-todo");
        let schedule = PaneSchedule::new("id-work".into(), Agent::Kiro, Some(dir.path()));
        assert!(schedule
            .plan_create(&request, &DynamicRoutines::default(), 1_000)
            .is_ok());
    }

    #[test]
    fn three_failed_sends_give_up_and_a_success_resets_the_streak() {
        let mut r = routine(600, 1_000);
        assert!(!record_failure(&mut r, "boom", "x"));
        assert!(!record_failure(&mut r, "boom", "x"));
        record_sent(&mut r, 2_000);
        assert_eq!(r.fail_streak, 0);
        assert!(!record_failure(&mut r, "boom", "x"));
        assert!(!record_failure(&mut r, "boom", "x"));
        assert!(record_failure(&mut r, "boom", "x"));
        assert_eq!(r.status, Status::GaveUp);
        assert_eq!(r.last_result.as_deref(), Some("failed:boom"));
        let mut r = routine(600, 1_000);
        record_give_up(&mut r, "pasted_unsubmitted", "pasted");
        assert_eq!(r.status, Status::GaveUp);
    }

    #[test]
    fn a_create_follows_the_ceiling_and_evicts_the_oldest_finished() {
        let ceiling = DynamicRoutines {
            min_every_secs: 60,
            max_per_pane: 2,
            ..DynamicRoutines::default()
        };
        let mut schedule = PaneSchedule::new("id-work".into(), Agent::Kiro, None);
        let request = |text: &str| NewEvery {
            every_secs: 60,
            text: text.into(),
            todo_file: None,
            expires_secs: None,
            max_runs: None,
            created_by: "agent-a".into(),
        };
        for (n, text) in ["first thing", "second thing"].iter().enumerate() {
            let (routines, seq, r) = schedule
                .plan_create(&request(text), &ceiling, 1_000 + n as u64)
                .unwrap();
            assert_eq!(r.id, format!("r{}", n + 1));
            schedule.commit(routines, seq);
        }
        let err = schedule
            .plan_create(&request("third thing"), &ceiling, 1_010)
            .unwrap_err();
        assert!(matches!(err, CreateError::Limit(_)), "{err:?}");
        // Finished routines stop counting, and the oldest of them goes first
        // once the pane would hold more than twice its share.
        for r in schedule.routines_mut() {
            r.status = Status::Done;
        }
        for text in ["third thing", "fourth thing", "fifth thing"] {
            let (routines, seq, _) = schedule
                .plan_create(&request(text), &ceiling, 1_020)
                .unwrap();
            schedule.commit(routines, seq);
            for r in schedule.routines_mut() {
                r.status = Status::Done;
            }
        }
        let ids: Vec<&str> = schedule.routines().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["r2", "r3", "r4", "r5"]);
    }
}
