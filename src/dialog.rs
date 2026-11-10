//! Text parser for the kiro-cli permission dialog.
//!
//! herdr-core reports only `blocked`/`permission`; the dialog itself exists only
//! as screen text. The layout below was measured on kiro-cli 3.1.0 (V3 engine)
//! for a shell tool call. Anything else is refused rather than guessed.
//!
//! ```text
//!  Create an empty marker file requires approval
//!  shell → touch shepherd_marker.txt
//!
//!  ❯ Allow once
//!    Deny once
//!    Create allow rule to approve future similar actions
//!    Create deny rule to block future similar actions
//! ────────────────────────────────────────────────────
//! ```

pub const TITLE_SUFFIX: &str = "requires approval";
pub const COMMAND_ARROW: &str = " → ";
pub const CURSOR: char = '❯';

/// The four options, in screen order. Only the first two can be chosen.
pub const OPTION_ALLOW_ONCE: &str = "Allow once";
pub const OPTION_DENY_ONCE: &str = "Deny once";
const OPTION_ALLOW_RULE_PREFIX: &str = "Create allow rule";
const OPTION_DENY_RULE_PREFIX: &str = "Create deny rule";

/// The choices `answer_dialog` accepts. Rule options are not representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    AllowOnce,
    DenyOnce,
}

impl Choice {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "allow_once" => Some(Self::AllowOnce),
            "deny_once" => Some(Self::DenyOnce),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::DenyOnce => "deny_once",
        }
    }

    /// The option's position in the dialog.
    pub fn index(self) -> usize {
        match self {
            Self::AllowOnce => 0,
            Self::DenyOnce => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => OPTION_ALLOW_ONCE,
            Self::DenyOnce => OPTION_DENY_ONCE,
        }
    }
}

/// One parsed dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    /// The `... requires approval` line, trimmed.
    pub title: String,
    /// The `<tool> → <command>` line, trimmed.
    pub command: String,
    /// The four option labels, cursor removed.
    pub options: Vec<String>,
    /// Index of the option under `❯`.
    pub cursor: usize,
}

/// Parse the one permission dialog on the screen. Fails when there is none,
/// when more than one parses, or when the layout differs from the measured one.
pub fn parse(screen: &str) -> Result<Dialog, String> {
    let lines: Vec<&str> = screen.lines().collect();
    let mut found = Vec::new();
    let mut last_error = None;
    for (index, line) in lines.iter().enumerate() {
        if !line.trim_end().ends_with(TITLE_SUFFIX) {
            continue;
        }
        match parse_block(&lines, index) {
            Ok(dialog) => found.push(dialog),
            Err(err) => last_error = Some(err),
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(last_error
            .unwrap_or_else(|| format!("no line ending in {TITLE_SUFFIX:?} on the screen"))),
        n => Err(format!(
            "{n} permission dialogs parsed on the screen; expected one"
        )),
    }
}

fn parse_block(lines: &[&str], title_index: usize) -> Result<Dialog, String> {
    let title = lines[title_index].trim().to_string();
    let command = lines
        .get(title_index + 1)
        .map(|line| line.trim())
        .unwrap_or_default();
    if !command.contains(COMMAND_ARROW) {
        return Err(format!(
            "the line after {title:?} is not \"<tool> → <command>\": {command:?}"
        ));
    }

    let mut next = title_index + 2;
    while lines.get(next).is_some_and(|line| line.trim().is_empty()) {
        next += 1;
    }
    let block: Vec<&str> = lines.iter().skip(next).take(4).copied().collect();
    if block.len() < 4 {
        return Err(format!(
            "expected four option lines after {title:?}, found {}: {block:?}",
            block.len()
        ));
    }

    let mut options = Vec::with_capacity(4);
    let mut cursors = Vec::new();
    for (index, line) in block.iter().enumerate() {
        let trimmed = line.trim();
        let label = match trimmed.strip_prefix(CURSOR) {
            Some(rest) => {
                cursors.push(index);
                rest.trim()
            }
            None => trimmed,
        };
        options.push(label.to_string());
    }
    let expected = options.len() == 4
        && options[0] == OPTION_ALLOW_ONCE
        && options[1] == OPTION_DENY_ONCE
        && options[2].starts_with(OPTION_ALLOW_RULE_PREFIX)
        && options[3].starts_with(OPTION_DENY_RULE_PREFIX);
    if !expected {
        return Err(format!(
            "options after {title:?} are not [{OPTION_ALLOW_ONCE:?}, {OPTION_DENY_ONCE:?}, \
             \"{OPTION_ALLOW_RULE_PREFIX}...\", \"{OPTION_DENY_RULE_PREFIX}...\"]: {options:?}"
        ));
    }
    if let Some(after) = lines.get(next + 4) {
        let after = after.trim();
        if !(after.is_empty() || after.starts_with('─')) {
            return Err(format!("unexpected line after the four options: {after:?}"));
        }
    }
    let &[cursor] = cursors.as_slice() else {
        return Err(format!(
            "expected one {CURSOR} cursor among the options, found {}",
            cursors.len()
        ));
    };
    Ok(Dialog {
        title,
        command: command.to_string(),
        options,
        cursor,
    })
}

/// Fixtures from the kiro-cli 3.1.0 probe (`hc read` of a blocked pane).
#[cfg(test)]
pub mod fixtures {
    pub const PROBE_HEAD: &str = "  Use the shell tool to run exactly this command in the current directory and nothing else: touch shepherd_marker.txt

↓ Shell touch shepherd_marker.txt
    ╰ description=Create an empty marker file, cwd=/private<probe-dir>, run_in_background=false

────────────────────────────────────────────────────────────────────────
 Create an empty marker file requires approval
 shell → touch shepherd_marker.txt

";
    pub const OPTIONS_ON_ALLOW: &str = " ❯ Allow once
   Deny once
   Create allow rule to approve future similar actions
   Create deny rule to block future similar actions
";
    pub const OPTIONS_ON_DENY: &str = "   Allow once
 ❯ Deny once
   Create allow rule to approve future similar actions
   Create deny rule to block future similar actions
";
    pub const PROBE_TAIL: &str =
        "────────────────────────────────────────────────────────────────────────
 esc to close · ↑↓ to navigate · ↵ to select · Tab to edit
[hc] incomplete: older output may be missing
";

    /// The dialog with the cursor on Allow once (as first shown).
    pub fn dialog_on_allow() -> String {
        format!("{PROBE_HEAD}{OPTIONS_ON_ALLOW}{PROBE_TAIL}")
    }

    /// The dialog after one `down`.
    pub fn dialog_on_deny() -> String {
        format!("{PROBE_HEAD}{OPTIONS_ON_DENY}{PROBE_TAIL}")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn parses_probe_dialog_with_cursor_on_allow_once() {
        let dialog = parse(&dialog_on_allow()).unwrap();
        assert_eq!(
            dialog.title,
            "Create an empty marker file requires approval"
        );
        assert_eq!(dialog.command, "shell → touch shepherd_marker.txt");
        assert_eq!(dialog.options[0], "Allow once");
        assert_eq!(dialog.options[1], "Deny once");
        assert_eq!(dialog.cursor, 0);
    }

    #[test]
    fn parses_cursor_on_deny_once() {
        assert_eq!(parse(&dialog_on_deny()).unwrap().cursor, 1);
    }

    #[test]
    fn refuses_screen_without_dialog() {
        let err = parse("• Rejected touch shepherd_marker.txt\n").unwrap_err();
        assert!(err.contains("requires approval"), "{err}");
    }

    #[test]
    fn refuses_missing_option() {
        let screen = format!(
            "{PROBE_HEAD} ❯ Allow once\n   Create allow rule to approve future similar actions\n   Create deny rule to block future similar actions\n   Something else\n{PROBE_TAIL}"
        );
        let err = parse(&screen).unwrap_err();
        assert!(err.contains("options after"), "{err}");
    }

    #[test]
    fn refuses_extra_option_line() {
        let screen =
            format!("{PROBE_HEAD}{OPTIONS_ON_ALLOW}   Always allow everything\n{PROBE_TAIL}");
        let err = parse(&screen).unwrap_err();
        assert!(err.contains("unexpected line"), "{err}");
    }

    #[test]
    fn refuses_no_cursor_or_two_cursors() {
        let none = OPTIONS_ON_ALLOW.replace('❯', " ");
        let err = parse(&format!("{PROBE_HEAD}{none}{PROBE_TAIL}")).unwrap_err();
        assert!(err.contains("found 0"), "{err}");
        let two = OPTIONS_ON_ALLOW.replace("   Deny once", " ❯ Deny once");
        let err = parse(&format!("{PROBE_HEAD}{two}{PROBE_TAIL}")).unwrap_err();
        assert!(err.contains("found 2"), "{err}");
    }

    #[test]
    fn refuses_missing_command_line() {
        let screen = dialog_on_allow().replace(" shell → touch shepherd_marker.txt\n", "");
        assert!(parse(&screen).is_err());
    }

    #[test]
    fn refuses_two_dialogs() {
        let screen = format!("{}{}", dialog_on_allow(), dialog_on_allow());
        let err = parse(&screen).unwrap_err();
        assert!(err.contains("2 permission dialogs"), "{err}");
    }

    #[test]
    fn choice_accepts_only_the_two_once_options() {
        assert_eq!(Choice::parse("allow_once"), Some(Choice::AllowOnce));
        assert_eq!(Choice::parse("deny_once"), Some(Choice::DenyOnce));
        for other in [
            "allow_rule",
            "deny_rule",
            "create_allow_rule",
            "Allow once",
            "2",
            "",
        ] {
            assert_eq!(Choice::parse(other), None, "{other}");
        }
    }
}
