//! The model of a routine (issue #98): a trigger and an action, with the
//! rules every routine obeys wherever it came from.
//!
//! Two kinds exist. The profile's `routine` section is the idle trigger and
//! stays where it is parsed ([`crate::profiles`]); [`Routine::from_profile`]
//! shows it in this shape. A routine an agent creates is an `every` trigger
//! with a `type text` action, kept beside the pane ([`crate::registry`]) and
//! limited by the profile's `dynamic_routines` ceiling.
//!
//! Everything here is pure: validation takes the ceiling and returns what
//! failed, with the limit, so a caller can refuse and say how to retry. A
//! request outside a limit is refused, never clamped.

use crate::agent::Agent;
use crate::input::{is_bare_confirmation, validate_input};
use crate::profiles::{DynamicRoutines, RoutineConfig};
use crate::registry::RoutineRecord;
use std::path::{Component, Path};

/// Hard bounds on `every_secs`, whatever a profile says.
pub const EVERY_MIN_SECS: u64 = 60;
pub const EVERY_MAX_SECS: u64 = 604_800;
/// Hard bounds on a routine's own `expires_secs`.
pub const EXPIRES_MIN_SECS: u64 = 60;
pub const EXPIRES_MAX_SECS: u64 = 2_592_000;
/// Bounds on a profile's `dynamic_routines.max_expires_secs`.
pub const CEILING_EXPIRES_MIN_SECS: u64 = 3_600;
/// Hard bounds on `max_per_pane`.
pub const PER_PANE_MIN: u64 = 1;
pub const PER_PANE_MAX: u64 = 10;
/// Defaults of the three ceiling keys.
pub const DEFAULT_MIN_EVERY_SECS: u64 = 600;
pub const DEFAULT_MAX_PER_PANE: u32 = 3;
pub const DEFAULT_MAX_EXPIRES_SECS: u64 = 604_800;
/// Largest text of an agent routine, in bytes (fixed, not configurable).
pub const TEXT_MAX_BYTES: usize = 1024;
/// Bounds on `max_runs`.
pub const MAX_RUNS_MIN: u64 = 1;
pub const MAX_RUNS_MAX: u64 = 10_000;

/// What starts a routine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// The pane sat finished and untouched. Only the profile spells it.
    Idle {
        idle_secs: u64,
        cooldown_secs: u64,
        max_consecutive: u32,
    },
    /// A fixed interval on a grid anchored at creation.
    Every { every_secs: u64 },
}

/// What a routine does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Type `text` into the pane through the guarded send path.
    Type { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    Expired,
    Done,
    GaveUp,
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Expired => "expired",
            Status::Done => "done",
            Status::GaveUp => "gave_up",
            Status::Stopped => "stopped",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "active" => Status::Active,
            "expired" => Status::Expired,
            "done" => Status::Done,
            "gave_up" => Status::GaveUp,
            "stopped" => Status::Stopped,
            _ => return None,
        })
    }

    /// Whether the routine still counts toward `max_per_pane`.
    pub fn is_active(self) -> bool {
        self == Status::Active
    }
}

/// The `stop_reason` of a routine a changed profile no longer allows.
pub const PROFILE_CHANGED: &str = "profile_changed";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routine {
    /// `profile` or `r<N>`.
    pub id: String,
    pub trigger: Trigger,
    pub action: Action,
    pub todo_file: Option<String>,
    /// The client id that created it; `profile` for the profile's routine.
    pub created_by: String,
    pub created_at: u64,
    /// Mandatory for an agent routine; the profile's routine has none.
    pub expires_at: Option<u64>,
    pub max_runs: Option<u32>,
    pub status: Status,
    pub stop_reason: Option<String>,
    pub runs: u64,
    pub fail_streak: u32,
    pub last_run_at: Option<u64>,
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    /// The next grid time of an `every` trigger (unix seconds).
    pub next_due: Option<u64>,
}

/// Why a request was refused: the field, the limit and a sentence the model
/// can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid {
    pub field: &'static str,
    pub limit: String,
    pub message: String,
}

impl Invalid {
    fn new(field: &'static str, limit: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field,
            limit: limit.into(),
            message: message.into(),
        }
    }
}

impl Routine {
    /// The profile's idle routine in the model's shape. Read-only: the
    /// counters of the live watch are not part of it.
    pub fn from_profile(config: &RoutineConfig) -> Self {
        Self {
            id: "profile".to_string(),
            trigger: Trigger::Idle {
                idle_secs: config.idle_timeout.as_secs(),
                cooldown_secs: config.cooldown.as_secs(),
                max_consecutive: config.max_consecutive_nudges,
            },
            action: Action::Type {
                text: config.nudge_text.clone(),
            },
            todo_file: config
                .todo_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            created_by: "profile".to_string(),
            created_at: 0,
            expires_at: None,
            max_runs: None,
            status: Status::Active,
            stop_reason: None,
            runs: 0,
            fail_streak: 0,
            last_run_at: None,
            last_result: None,
            last_error: None,
            next_due: None,
        }
    }

    /// The stored form. `None` for a routine that is not `every`: v1 stores
    /// no other.
    pub fn to_record(&self) -> Option<RoutineRecord> {
        let Trigger::Every { every_secs } = self.trigger else {
            return None;
        };
        let Action::Type { text } = &self.action;
        Some(RoutineRecord {
            id: self.id.clone(),
            every_secs,
            text: text.clone(),
            todo_file: self.todo_file.clone(),
            created_by: self.created_by.clone(),
            created_at: self.created_at,
            expires_at: self.expires_at?,
            max_runs: self.max_runs,
            status: self.status.as_str().to_string(),
            stop_reason: self.stop_reason.clone(),
            runs: self.runs,
            fail_streak: self.fail_streak,
            last_run_at: self.last_run_at,
            last_result: self.last_result.clone(),
            last_error: self.last_error.clone(),
            next_due: self.next_due?,
        })
    }

    /// Read a stored item; `None` for a status this build does not know.
    pub fn from_record(record: &RoutineRecord) -> Option<Self> {
        Some(Self {
            id: record.id.clone(),
            trigger: Trigger::Every {
                every_secs: record.every_secs,
            },
            action: Action::Type {
                text: record.text.clone(),
            },
            todo_file: record.todo_file.clone(),
            created_by: record.created_by.clone(),
            created_at: record.created_at,
            expires_at: Some(record.expires_at),
            max_runs: record.max_runs,
            status: Status::parse(&record.status)?,
            stop_reason: record.stop_reason.clone(),
            runs: record.runs,
            fail_streak: record.fail_streak,
            last_run_at: record.last_run_at,
            last_result: record.last_result.clone(),
            last_error: record.last_error.clone(),
            next_due: Some(record.next_due),
        })
    }
}

/// `every_secs` against the hard bounds and the profile's floor.
pub fn validate_every_secs(secs: u64, ceiling: &DynamicRoutines) -> Result<u64, Invalid> {
    let floor = ceiling.min_every_secs.max(EVERY_MIN_SECS);
    if secs < floor || secs > EVERY_MAX_SECS {
        return Err(Invalid::new(
            "every_secs",
            format!("{floor}..={EVERY_MAX_SECS}"),
            format!("every_secs must be between {floor} and {EVERY_MAX_SECS} seconds"),
        ));
    }
    Ok(secs)
}

/// The text of an agent routine: the rules of `send_input`, at most
/// [`TEXT_MAX_BYTES`], not a bare confirmation, and not `/yolo` for Devin.
pub fn validate_text(text: &str, agent: Agent) -> Result<String, Invalid> {
    let text = validate_input(text)
        .map_err(|e| Invalid::new("text", "non-empty, no control characters", e.to_string()))?;
    if text.len() > TEXT_MAX_BYTES {
        return Err(Invalid::new(
            "text",
            format!("{TEXT_MAX_BYTES} bytes"),
            format!(
                "text is {} bytes; the limit is {TEXT_MAX_BYTES} bytes of UTF-8",
                text.len()
            ),
        ));
    }
    if is_bare_confirmation(&text) {
        return Err(Invalid::new(
            "text",
            "an instruction, not a bare answer",
            "text may not be a bare confirmation or menu answer such as y, yes, 1 or ok; write a sentence that tells the agent what to do",
        ));
    }
    if agent == Agent::Devin && text.eq_ignore_ascii_case("/yolo") {
        return Err(Invalid::new(
            "text",
            "not /yolo for devin",
            "text may not be /yolo for a devin pane",
        ));
    }
    Ok(text)
}

/// `todo_file`: a relative path inside the working directory, the rule the
/// profile's `routine.todo_file` follows.
pub fn validate_todo_file(text: &str) -> Result<String, Invalid> {
    let text = validate_input(text)
        .map_err(|e| Invalid::new("todo_file", "a relative path", e.to_string()))?;
    let path = Path::new(&text);
    let escapes = path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if escapes {
        return Err(Invalid::new(
            "todo_file",
            "a relative path inside the working directory",
            "todo_file must be a relative path inside the working directory",
        ));
    }
    Ok(text)
}

/// `expires_secs`; `None` takes the profile's maximum.
pub fn validate_expires_secs(secs: Option<u64>, ceiling: &DynamicRoutines) -> Result<u64, Invalid> {
    let max = ceiling.max_expires_secs.min(EXPIRES_MAX_SECS);
    let secs = secs.unwrap_or(max);
    if secs < EXPIRES_MIN_SECS || secs > max {
        return Err(Invalid::new(
            "expires_secs",
            format!("{EXPIRES_MIN_SECS}..={max}"),
            format!("expires_secs must be between {EXPIRES_MIN_SECS} and {max} seconds"),
        ));
    }
    Ok(secs)
}

pub fn validate_max_runs(runs: u64) -> Result<u32, Invalid> {
    match u32::try_from(runs) {
        Ok(n) if (MAX_RUNS_MIN..=MAX_RUNS_MAX).contains(&runs) => Ok(n),
        _ => Err(Invalid::new(
            "max_runs",
            format!("{MAX_RUNS_MIN}..={MAX_RUNS_MAX}"),
            format!("max_runs must be between {MAX_RUNS_MIN} and {MAX_RUNS_MAX}"),
        )),
    }
}

/// Whether one more active routine fits on the pane. `active` is the number
/// of active routines now. Finished ones do not count.
pub fn check_count(active: usize, ceiling: &DynamicRoutines) -> Result<(), Invalid> {
    let max = usize::try_from(ceiling.max_per_pane.min(PER_PANE_MAX as u32)).unwrap_or(0);
    if active >= max {
        return Err(Invalid::new(
            "routines",
            max.to_string(),
            format!("this pane already has {active} active routines; the limit is {max}"),
        ));
    }
    Ok(())
}

/// Why `record` is not allowed under the current ceiling, or `None`. The
/// adoption rule: a stored routine that breaks it becomes
/// `stopped / profile_changed`. Checks the interval floor, the expiry
/// horizon (from creation) and the text.
pub fn ceiling_violation(
    record: &RoutineRecord,
    ceiling: &DynamicRoutines,
    agent: Agent,
) -> Option<String> {
    if let Err(why) = validate_every_secs(record.every_secs, ceiling) {
        return Some(why.message);
    }
    let horizon = ceiling.max_expires_secs.min(EXPIRES_MAX_SECS);
    if record.expires_at.saturating_sub(record.created_at) > horizon {
        return Some(format!(
            "it expires more than {horizon} seconds after creation"
        ));
    }
    if let Err(why) = validate_text(&record.text, agent) {
        return Some(why.message);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn ceiling() -> DynamicRoutines {
        DynamicRoutines::default()
    }

    fn record() -> RoutineRecord {
        RoutineRecord {
            id: "r1".into(),
            every_secs: 600,
            text: "check the todo list".into(),
            todo_file: Some("TODO.md".into()),
            created_by: "agent-a".into(),
            created_at: 1_000,
            expires_at: 1_000 + 3_600,
            max_runs: Some(5),
            status: "active".into(),
            stop_reason: None,
            runs: 2,
            fail_streak: 1,
            last_run_at: Some(1_700),
            last_result: Some("sent".into()),
            last_error: None,
            next_due: 2_200,
        }
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let c = ceiling();
        assert_eq!(c.min_every_secs, 600);
        assert_eq!(c.max_per_pane, 3);
        assert_eq!(c.max_expires_secs, 604_800);
    }

    #[test]
    fn every_secs_is_bounded_by_the_floor_and_the_hard_limits() {
        let c = ceiling();
        assert!(validate_every_secs(599, &c).is_err());
        assert_eq!(validate_every_secs(600, &c).unwrap(), 600);
        assert_eq!(validate_every_secs(EVERY_MAX_SECS, &c).unwrap(), 604_800);
        assert!(validate_every_secs(EVERY_MAX_SECS + 1, &c).is_err());
        let low = DynamicRoutines {
            min_every_secs: 60,
            ..c
        };
        assert!(validate_every_secs(59, &low).is_err());
        assert_eq!(validate_every_secs(60, &low).unwrap(), 60);
        let err = validate_every_secs(5, &ceiling()).unwrap_err();
        assert_eq!(err.field, "every_secs");
        assert!(err.limit.starts_with("600.."), "{}", err.limit);
    }

    #[test]
    fn text_follows_the_send_rules_and_is_capped_at_1024_bytes() {
        let a = Agent::Kiro;
        assert_eq!(validate_text("  keep going  ", a).unwrap(), "keep going");
        assert!(validate_text("", a).is_err());
        assert!(validate_text("two\nlines", a).is_err());
        assert!(validate_text(&"x".repeat(1024), a).is_ok());
        let err = validate_text(&"x".repeat(1025), a).unwrap_err();
        assert_eq!((err.field, err.limit.as_str()), ("text", "1024 bytes"));
    }

    #[test]
    fn a_bare_confirmation_is_refused() {
        for bad in ["yes", "Y", "ok.", "1", "Allow once"] {
            let err = validate_text(bad, Agent::Kiro).unwrap_err();
            assert_eq!(err.field, "text", "{bad}");
        }
        assert!(validate_text("yes, continue with the list", Agent::Kiro).is_ok());
    }

    #[test]
    fn yolo_is_refused_for_devin_only() {
        assert!(validate_text("/yolo", Agent::Devin).is_err());
        assert!(validate_text("/YOLO", Agent::Devin).is_err());
        assert!(validate_text("/yolo", Agent::Kiro).is_ok());
    }

    #[test]
    fn todo_file_must_stay_inside_the_working_directory() {
        assert_eq!(
            validate_todo_file("notes/TODO.md").unwrap(),
            "notes/TODO.md"
        );
        for bad in ["/abs/TODO.md", "../TODO.md", "a/../../TODO.md", ""] {
            assert!(validate_todo_file(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn expiry_defaults_to_the_maximum_and_is_bounded() {
        let c = ceiling();
        assert_eq!(validate_expires_secs(None, &c).unwrap(), 604_800);
        assert_eq!(validate_expires_secs(Some(60), &c).unwrap(), 60);
        assert!(validate_expires_secs(Some(59), &c).is_err());
        assert!(validate_expires_secs(Some(604_801), &c).is_err());
        let wide = DynamicRoutines {
            max_expires_secs: EXPIRES_MAX_SECS,
            ..c
        };
        assert!(validate_expires_secs(Some(EXPIRES_MAX_SECS), &wide).is_ok());
        assert!(validate_expires_secs(Some(EXPIRES_MAX_SECS + 1), &wide).is_err());
    }

    #[test]
    fn max_runs_is_one_to_ten_thousand() {
        assert!(validate_max_runs(0).is_err());
        assert_eq!(validate_max_runs(1).unwrap(), 1);
        assert_eq!(validate_max_runs(10_000).unwrap(), 10_000);
        assert!(validate_max_runs(10_001).is_err());
        assert!(validate_max_runs(u64::MAX).is_err());
    }

    #[test]
    fn the_per_pane_count_counts_active_routines_only() {
        let c = ceiling();
        assert!(check_count(2, &c).is_ok());
        let err = check_count(3, &c).unwrap_err();
        assert_eq!((err.field, err.limit.as_str()), ("routines", "3"));
    }

    #[test]
    fn a_profile_routine_has_the_model_shape() {
        let config = RoutineConfig {
            idle_timeout: Duration::from_secs(120),
            cooldown: Duration::from_secs(45),
            max_consecutive_nudges: 5,
            todo_file: Some(PathBuf::from("notes/TODO.md")),
            nudge_text: "keep going".into(),
        };
        let r = Routine::from_profile(&config);
        assert_eq!(
            (r.id.as_str(), r.created_by.as_str()),
            ("profile", "profile")
        );
        assert_eq!(
            r.trigger,
            Trigger::Idle {
                idle_secs: 120,
                cooldown_secs: 45,
                max_consecutive: 5
            }
        );
        assert_eq!(
            r.action,
            Action::Type {
                text: "keep going".into()
            }
        );
        assert_eq!(r.todo_file.as_deref(), Some("notes/TODO.md"));
        assert_eq!(r.expires_at, None);
        assert!(r.to_record().is_none(), "the idle routine is never stored");
    }

    #[test]
    fn a_record_round_trips_through_the_model() {
        let rec = record();
        let routine = Routine::from_record(&rec).unwrap();
        assert_eq!(routine.trigger, Trigger::Every { every_secs: 600 });
        assert_eq!(routine.to_record().unwrap(), rec);
    }

    #[test]
    fn an_unknown_status_is_not_read() {
        let mut rec = record();
        rec.status = "paused".into();
        assert!(Routine::from_record(&rec).is_none());
        for s in [
            Status::Active,
            Status::Expired,
            Status::Done,
            Status::GaveUp,
            Status::Stopped,
        ] {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn the_adoption_rule_names_what_the_ceiling_no_longer_allows() {
        let c = ceiling();
        assert_eq!(ceiling_violation(&record(), &c, Agent::Kiro), None);
        let raised = DynamicRoutines {
            min_every_secs: 900,
            ..c.clone()
        };
        assert!(ceiling_violation(&record(), &raised, Agent::Kiro).is_some());
        let short = DynamicRoutines {
            max_expires_secs: 3_600,
            ..c.clone()
        };
        assert_eq!(ceiling_violation(&record(), &short, Agent::Kiro), None);
        let mut long = record();
        long.expires_at = long.created_at + 604_801;
        assert!(ceiling_violation(&long, &c, Agent::Kiro).is_some());
        let mut yes = record();
        yes.text = "yes".into();
        assert!(ceiling_violation(&yes, &c, Agent::Kiro).is_some());
    }
}
