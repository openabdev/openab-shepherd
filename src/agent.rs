//! The agents a launch profile can name, and the permission dialog each one shows.
//!
//! answer_dialog finds the pane's agent (recorded when a profile started the pane)
//! and asks it for its dialect: how to read the dialog, which option a choice
//! selects, and the label of that option. A pane with no recorded agent gets no
//! dialect, so answer_dialog sends nothing to it.

use crate::devin_dialog::{self, INDEX_ALLOW, INDEX_DENY};
use crate::dialog::{self, Choice};

/// A launch profile's `agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Kiro,
    Devin,
}

impl Agent {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "kiro" => Some(Self::Kiro),
            "devin" => Some(Self::Devin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kiro => "kiro",
            Self::Devin => "devin",
        }
    }

    /// Read the dialog from screen text. Fails unless exactly one dialog parses.
    pub fn parse_dialog(self, screen: &str) -> Result<ParsedDialog, String> {
        match self {
            Self::Kiro => dialog::parse(screen).map(|found| ParsedDialog {
                title: found.title,
                command: found.command,
                options: found.options,
                cursor: found.cursor,
            }),
            Self::Devin => devin_dialog::parse(screen),
        }
    }

    /// The position of the option `choice` selects in this agent's dialog.
    pub fn target(self, choice: Choice) -> usize {
        match (self, choice) {
            (Self::Kiro, _) => choice.index(),
            (Self::Devin, Choice::AllowOnce) => INDEX_ALLOW,
            (Self::Devin, Choice::DenyOnce) => INDEX_DENY,
        }
    }

    /// The option's label as this agent's dialog shows it.
    pub fn label(self, choice: Choice) -> &'static str {
        match (self, choice) {
            (Self::Kiro, _) => choice.label(),
            (Self::Devin, Choice::AllowOnce) => devin_dialog::OPTION_ALLOW,
            (Self::Devin, Choice::DenyOnce) => devin_dialog::OPTION_DENY,
        }
    }
}

/// A permission dialog as any agent's parser reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDialog {
    pub title: String,
    pub command: String,
    pub options: Vec<String>,
    /// Index of the option the highlight is on.
    pub cursor: usize,
}

/// A launch profile's `permission_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Shell commands and writes ask for approval.
    Normal,
    /// Workspace edits are approved without asking.
    AcceptEdits,
    /// Everything is approved without asking. Never a default.
    Bypass,
}

impl PermissionMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "normal" => Some(Self::Normal),
            "accept-edits" => Some(Self::AcceptEdits),
            "bypass" => Some(Self::Bypass),
            _ => None,
        }
    }

    /// The value of Devin's `--permission-mode` flag for this mode. `normal` is
    /// `auto`, the Devin mode that approves read-only tools and asks for the
    /// rest. Passing it explicitly keeps a config file from changing the mode.
    pub fn devin_value(self) -> &'static str {
        match self {
            Self::Normal => "auto",
            Self::AcceptEdits => "accept-edits",
            Self::Bypass => "dangerous",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_devin_bypass_option_is_never_a_target() {
        for choice in [Choice::AllowOnce, Choice::DenyOnce] {
            assert_ne!(Agent::Devin.target(choice), devin_dialog::INDEX_BYPASS);
        }
    }

    #[test]
    fn targets_and_labels_match_each_agent() {
        assert_eq!(Agent::Kiro.target(Choice::AllowOnce), 0);
        assert_eq!(Agent::Kiro.target(Choice::DenyOnce), 1);
        assert_eq!(Agent::Kiro.label(Choice::DenyOnce), "Deny once");
        assert_eq!(Agent::Devin.target(Choice::AllowOnce), 0);
        assert_eq!(Agent::Devin.target(Choice::DenyOnce), 2);
        assert_eq!(Agent::Devin.label(Choice::AllowOnce), "Allow");
        assert_eq!(Agent::Devin.label(Choice::DenyOnce), "Deny");
    }

    #[test]
    fn agent_names_parse_exactly() {
        assert_eq!(Agent::parse("kiro"), Some(Agent::Kiro));
        assert_eq!(Agent::parse("devin"), Some(Agent::Devin));
        for other in ["Devin", "opencode", "", "devin "] {
            assert_eq!(Agent::parse(other), None, "{other}");
        }
    }

    #[test]
    fn devin_permission_values_are_the_cli_modes() {
        assert_eq!(PermissionMode::Normal.devin_value(), "auto");
        assert_eq!(PermissionMode::AcceptEdits.devin_value(), "accept-edits");
        assert_eq!(PermissionMode::Bypass.devin_value(), "dangerous");
        assert_eq!(PermissionMode::parse("yolo"), None);
    }
}
