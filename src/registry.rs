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

    fn forget(&self, pane_id: &str) -> Result<(), BackendError> {
        self.map().remove(pane_id);
        Ok(())
    }

    fn registered(&self) -> Vec<RegisteredPane> {
        self.map()
            .values()
            .filter(|p| p.agent.is_some() || p.routine.is_some())
            .cloned()
            .collect()
    }
}
