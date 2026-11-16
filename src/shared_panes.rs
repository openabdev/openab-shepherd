//! What the tools layer may ask of a backend whose panes a person can share
//! (issue #77, ADR restart-survival-and-shared-panes §4): who is at a pane
//! and whether they left a draft, and clearing that draft.
//!
//! A trait apart from `TerminalBackend`, as `PaneRegistry` is, so neither the
//! backend trait nor the pane tools change shape. A backend with no person
//! side does not implement it, and [`crate::tools::Shepherd`] then behaves as
//! it always did: nothing is refused, `draft_clear` is unsupported and the
//! status carries no `human` object.

use crate::backend::{BackendError, TerminalRef};
use std::time::Duration;

/// Who holds a pane's input lease, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseInfo {
    pub holder: String,
    pub idle: Duration,
    pub expires_in: Duration,
}

/// What a pane's person-side state is right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanState {
    /// None when no lease is held or the held one has idled out.
    pub lease: Option<LeaseInfo>,
    pub draft: bool,
}

impl HumanState {
    /// True when a person is at the pane: a live lease or a draft. A bot's
    /// write is refused then.
    pub fn is_held(&self) -> bool {
        self.lease.is_some() || self.draft
    }
}

/// The person side of a pane, as the tools see it.
pub trait SharedPanes: Send + Sync {
    /// The lease and the draft, as a status read sees them.
    fn human_state(&self, t: &TerminalRef) -> Result<HumanState, BackendError>;

    /// Clear the draft, whoever holds the lease, and leave the lease alone.
    /// True when a draft was set.
    fn draft_clear(&self, t: &TerminalRef) -> Result<bool, BackendError>;
}
