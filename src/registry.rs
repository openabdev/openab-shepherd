//! What the tools layer knows about a pane and a restarted front must get
//! back (issue #76): the agent kind and profile that decide the dialect
//! `answer_dialog` reads, the routine's counters, and whether a dialog was
//! answered.
//!
//! A backend whose panes outlive the front keeps this beside the pane
//! (the native backend, in the host's pane metadata). The tools read and
//! write it through [`PaneRegistry`], a trait apart from `TerminalBackend`
//! so neither the backend trait nor the tool schemas change. A backend with
//! nothing to keep does not implement it, and [`crate::tools::Shepherd`]
//! then behaves as it always did: memory only.

use crate::backend::BackendError;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// The routine's counters for one pane. Kept so a restart does not rearm a
/// routine that gave up or reset its nudge cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutineState {
    pub consecutive: u32,
    pub gave_up: bool,
    pub nudge_allowance: u64,
}

/// One agent-created routine, as stored beside the pane (issue #98). The
/// flat, persisted form of [`crate::routine_model::Routine`]; v1 stores
/// `every` + `type text` routines only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineRecord {
    /// `r<N>`, `N` from the set's counter.
    pub id: String,
    pub every_secs: u64,
    pub text: String,
    pub todo_file: Option<String>,
    pub created_by: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub max_runs: Option<u32>,
    /// `active`, `expired`, `done`, `gave_up` or `stopped`.
    pub status: String,
    pub stop_reason: Option<String>,
    pub runs: u64,
    pub fail_streak: u32,
    pub last_run_at: Option<u64>,
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    pub next_due: u64,
}

/// The version of the stored routines value, inside the pane meta's own
/// version.
const ROUTINES_VERSION: u64 = 1;

impl RoutineRecord {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "trigger": {"type": "every", "every_secs": self.every_secs},
            "text": self.text,
            "todo_file": self.todo_file,
            "created_by": self.created_by,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "max_runs": self.max_runs,
            "status": self.status,
            "stop_reason": self.stop_reason,
            "runs": self.runs,
            "fail_streak": self.fail_streak,
            "last_run_at": self.last_run_at,
            "last_result": self.last_result,
            "last_error": self.last_error,
            "next_due": self.next_due,
        })
    }

    /// Read one stored item; the error says which field is wrong.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let obj = value.as_object().ok_or("not an object")?;
        let req_u64 = |key: &str| {
            obj.get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("{key} is missing or not a whole number"))
        };
        let req_str = |key: &str| {
            obj.get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("{key} is missing or not a string"))
        };
        let opt_str = |key: &str| match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("{key} is not a string")),
        };
        let opt_u64 = |key: &str| match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .map(Some)
                .ok_or_else(|| format!("{key} is not a whole number")),
        };
        let trigger = obj.get("trigger").ok_or("trigger is missing")?;
        if trigger.get("type").and_then(Value::as_str) != Some("every") {
            return Err("trigger is not an every trigger".to_string());
        }
        let every_secs = trigger
            .get("every_secs")
            .and_then(Value::as_u64)
            .ok_or("every_secs is missing or not a whole number")?;
        let to_u32 =
            |key: &str, v: u64| u32::try_from(v).map_err(|_| format!("{key} is out of range"));
        let id = req_str("id")?;
        if routine_seq(&id).is_none() {
            return Err(format!("id {id:?} is not r<N>"));
        }
        Ok(Self {
            id,
            every_secs,
            text: req_str("text")?,
            todo_file: opt_str("todo_file")?,
            created_by: req_str("created_by")?,
            created_at: req_u64("created_at")?,
            expires_at: req_u64("expires_at")?,
            max_runs: opt_u64("max_runs")?
                .map(|v| to_u32("max_runs", v))
                .transpose()?,
            status: req_str("status")?,
            stop_reason: opt_str("stop_reason")?,
            runs: req_u64("runs")?,
            fail_streak: to_u32("fail_streak", req_u64("fail_streak")?)?,
            last_run_at: opt_u64("last_run_at")?,
            last_result: opt_str("last_result")?,
            last_error: opt_str("last_error")?,
            next_due: req_u64("next_due")?,
        })
    }
}

/// `N` of a routine id `r<N>`; `None` for any other spelling (the profile
/// routine's `profile` included).
pub fn routine_seq(id: &str) -> Option<u64> {
    let digits = id.strip_prefix('r')?;
    if digits.is_empty() || digits.len() > 18 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A pane's agent-created routines and the counter their ids come from. The
/// counter only grows, so an id is never reused, even after a delete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutineSet {
    pub seq: u64,
    pub items: Vec<RoutineRecord>,
}

impl RoutineSet {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The next id; advances the counter.
    pub fn next_id(&mut self) -> String {
        self.seq += 1;
        format!("r{}", self.seq)
    }

    /// Nothing to store: no routine and no counter.
    pub fn is_blank(&self) -> bool {
        self.seq == 0 && self.items.is_empty()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "v": ROUTINES_VERSION,
            "seq": self.seq,
            "items": self.items.iter().map(RoutineRecord::to_json).collect::<Vec<_>>(),
        })
    }

    /// Read a stored value without ever failing. A value of the wrong shape
    /// or version is empty; a bad item is dropped and the rest kept; each
    /// loss is one note for the caller to log. The counter is never below an
    /// id in use.
    pub fn from_json_lenient(value: &Value) -> (Self, Vec<String>) {
        let mut notes = Vec::new();
        let Some(obj) = value.as_object() else {
            return (
                Self::default(),
                vec!["routines is not an object".to_string()],
            );
        };
        match obj.get("v").and_then(Value::as_u64) {
            Some(ROUTINES_VERSION) => {}
            other => {
                return (
                    Self::default(),
                    vec![format!(
                        "routines has version {other:?}, not {ROUTINES_VERSION}"
                    )],
                );
            }
        }
        let Some(items) = obj.get("items").and_then(Value::as_array) else {
            return (
                Self::default(),
                vec!["routines has no items list".to_string()],
            );
        };
        let mut set = Self::default();
        for (index, item) in items.iter().enumerate() {
            match RoutineRecord::from_json(item) {
                Ok(record) if set.items.iter().any(|r| r.id == record.id) => {
                    notes.push(format!("routines item {index}: duplicate id {}", record.id));
                }
                Ok(record) => set.items.push(record),
                Err(why) => notes.push(format!("routines item {index} dropped: {why}")),
            }
        }
        let used = set
            .items
            .iter()
            .filter_map(|r| routine_seq(&r.id))
            .max()
            .unwrap_or(0);
        set.seq = match obj.get("seq").and_then(Value::as_u64) {
            Some(seq) => seq.max(used),
            None => {
                notes.push(
                    "routines has no usable seq; counting on from the ids in use".to_string(),
                );
                used
            }
        };
        (set, notes)
    }
}

/// One pane's record, as read back by a front that starts after the pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredPane {
    pub pane_id: String,
    pub name: Option<String>,
    pub cwd: String,
    /// The agent kind (`kiro`, `devin`, `opencode`), when a profile started
    /// the pane.
    pub agent: Option<String>,
    pub profile: Option<String>,
    pub answered: bool,
    /// Present when the routine watched the pane.
    pub routine: Option<RoutineState>,
    /// The pane's agent-created routines (issue #98).
    pub routines: RoutineSet,
    pub live: bool,
}

/// The tools layer's persisted view of its panes, keyed by backend pane id.
/// Writes are best effort for the caller (a failure is logged, the tool call
/// goes on), so an implementation reports what failed and nothing more.
pub trait PaneRegistry: Send + Sync {
    /// Record the agent kind, profile name and working directory of a pane a
    /// profile started.
    fn set_agent(
        &self,
        pane_id: &str,
        agent: Option<&str>,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(), BackendError>;

    /// Record whether a dialog was answered and its end state is pending.
    fn set_answered(&self, pane_id: &str, answered: bool) -> Result<(), BackendError>;

    /// Record the routine's counters; `None` when the pane is not watched.
    fn set_routine(
        &self,
        pane_id: &str,
        routine: Option<&RoutineState>,
    ) -> Result<(), BackendError>;

    /// Record the pane's agent-created routines. The write is all or
    /// nothing: on an error the stored routines are unchanged, so a caller
    /// that is refused can leave its own state as it was. An implementation
    /// may refuse a set it cannot hold.
    fn set_routines(&self, pane_id: &str, routines: &RoutineSet) -> Result<(), BackendError>;

    /// Clear everything recorded for the pane (it was removed). A pane the
    /// registry does not know is not an error.
    fn forget(&self, pane_id: &str) -> Result<(), BackendError>;

    /// Every pane with an agent or a routine recorded.
    fn registered(&self) -> Vec<RegisteredPane>;
}

/// A registry in memory: for a backend with no host to keep it in, and for
/// tests.
#[derive(Default)]
pub struct MemoryRegistry {
    panes: Mutex<HashMap<String, RegisteredPane>>,
}

impl MemoryRegistry {
    /// Make a pane known, as a backend does when it creates or adopts one.
    pub fn insert(&self, pane: RegisteredPane) {
        self.map().insert(pane.pane_id.clone(), pane);
    }

    /// The record of `pane_id`, if any.
    pub fn get(&self, pane_id: &str) -> Option<RegisteredPane> {
        self.map().get(pane_id).cloned()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, RegisteredPane>> {
        self.panes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn with(&self, pane_id: &str, change: impl FnOnce(&mut RegisteredPane)) {
        let mut map = self.map();
        let pane = map
            .entry(pane_id.to_string())
            .or_insert_with(|| RegisteredPane {
                pane_id: pane_id.to_string(),
                name: None,
                cwd: String::new(),
                agent: None,
                profile: None,
                answered: false,
                routine: None,
                routines: RoutineSet::default(),
                live: true,
            });
        change(pane);
    }
}

impl PaneRegistry for MemoryRegistry {
    fn set_agent(
        &self,
        pane_id: &str,
        agent: Option<&str>,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(), BackendError> {
        self.with(pane_id, |p| {
            p.agent = agent.map(str::to_string);
            p.profile = profile.map(str::to_string);
            if let Some(cwd) = cwd {
                p.cwd = cwd.to_string();
            }
        });
        Ok(())
    }

    fn set_answered(&self, pane_id: &str, answered: bool) -> Result<(), BackendError> {
        self.with(pane_id, |p| p.answered = answered);
        Ok(())
    }

    fn set_routine(
        &self,
        pane_id: &str,
        routine: Option<&RoutineState>,
    ) -> Result<(), BackendError> {
        self.with(pane_id, |p| p.routine = routine.cloned());
        Ok(())
    }

    fn set_routines(&self, pane_id: &str, routines: &RoutineSet) -> Result<(), BackendError> {
        self.with(pane_id, |p| p.routines = routines.clone());
        Ok(())
    }

    fn forget(&self, pane_id: &str) -> Result<(), BackendError> {
        self.map().remove(pane_id);
        Ok(())
    }

    fn registered(&self) -> Vec<RegisteredPane> {
        self.map()
            .values()
            .filter(|p| p.agent.is_some() || p.routine.is_some() || !p.routines.is_empty())
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(n: u64) -> RoutineRecord {
        RoutineRecord {
            id: format!("r{n}"),
            every_secs: 600,
            text: format!("check item {n}"),
            todo_file: (n & 1 == 0).then(|| "TODO.md".to_string()),
            created_by: "agent-a".to_string(),
            created_at: 1_000,
            expires_at: 100_000,
            max_runs: Some(7),
            status: "active".to_string(),
            stop_reason: None,
            runs: 1,
            fail_streak: 0,
            last_run_at: Some(1_600),
            last_result: Some("sent".to_string()),
            last_error: None,
            next_due: 2_200,
        }
    }

    #[test]
    fn a_set_round_trips_through_json() {
        let set = RoutineSet {
            seq: 9,
            items: vec![rec(1), rec(2)],
        };
        let (back, notes) = RoutineSet::from_json_lenient(&set.to_json());
        assert_eq!(back, set);
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(set.to_json()["v"], 1);
    }

    #[test]
    fn a_value_of_the_wrong_shape_reads_as_empty_with_a_note() {
        for bad in [
            json!("x"),
            json!(5),
            json!([1]),
            json!({}),
            json!({"v": 2, "seq": 1, "items": []}),
            json!({"v": 1, "seq": 1}),
            json!({"v": 1, "seq": 1, "items": "no"}),
        ] {
            let (set, notes) = RoutineSet::from_json_lenient(&bad);
            assert_eq!(set, RoutineSet::default(), "{bad}");
            assert_eq!(notes.len(), 1, "{bad}: {notes:?}");
        }
    }

    #[test]
    fn a_bad_item_is_dropped_and_the_rest_kept() {
        let mut wrong_type = rec(3).to_json();
        wrong_type["runs"] = json!("many");
        let mut other_trigger = rec(4).to_json();
        other_trigger["trigger"] = json!({"type": "idle", "idle_secs": 5});
        let mut bad_id = rec(5).to_json();
        bad_id["id"] = json!("profile");
        let value = json!({"v": 1, "seq": 6, "items": [
            rec(1).to_json(), wrong_type, other_trigger, bad_id, json!(7), rec(2).to_json(),
            rec(1).to_json(),
        ]});
        let (set, notes) = RoutineSet::from_json_lenient(&value);
        let ids: Vec<&str> = set.items.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["r1", "r2"]);
        assert_eq!(set.seq, 6);
        assert_eq!(notes.len(), 5, "{notes:?}");
    }

    #[test]
    fn the_counter_never_falls_below_an_id_in_use() {
        let (set, _) =
            RoutineSet::from_json_lenient(&json!({"v": 1, "seq": 1, "items": [rec(4).to_json()]}));
        assert_eq!(set.seq, 4);
        let (set, notes) =
            RoutineSet::from_json_lenient(&json!({"v": 1, "items": [rec(3).to_json()]}));
        assert_eq!(set.seq, 3);
        assert_eq!(notes.len(), 1);
        let mut set = set;
        assert_eq!(set.next_id(), "r4");
        assert_eq!(set.next_id(), "r5");
    }

    #[test]
    fn routine_ids_are_r_and_digits() {
        assert_eq!(routine_seq("r1"), Some(1));
        assert_eq!(routine_seq("r42"), Some(42));
        for bad in [
            "",
            "r",
            "profile",
            "R1",
            "r-1",
            "r1x",
            "1",
            "r1234567890123456789",
        ] {
            assert_eq!(routine_seq(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_memory_registry_keeps_routines_and_forgets_them() {
        let reg = MemoryRegistry::default();
        let set = RoutineSet {
            seq: 2,
            items: vec![rec(1), rec(2)],
        };
        reg.set_routines("id-1", &set).unwrap();
        // A pane with routines is registered even with no agent or routine.
        let all = reg.registered();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].routines, set);
        reg.set_routines("id-1", &RoutineSet::default()).unwrap();
        assert!(reg.registered().is_empty());
        reg.set_routines("id-1", &set).unwrap();
        reg.forget("id-1").unwrap();
        assert!(reg.registered().is_empty());
    }
}
