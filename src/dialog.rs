//! Text parser for the kiro-cli permission dialog.
//!
//! The backend reports only `blocked`/`permission`; the dialog itself exists only
//! as screen text. The layout below was measured on kiro-cli 3.1.0 and 3.3.0 (V3
//! engine) for a shell tool call; 3.3.0 also for `fs_write`, `mcp` and `web_fetch`,
//! which share it. Anything else is refused rather than guessed. The OSC 7501
//! `kind` is `permission` for every tool, so it never selects a rule here.
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

/// Tool names (the text before the arrow) whose dialog layout was measured.
/// Any other name is refused.
pub const TOOLS: [&str; 4] = ["shell", "fs_write", "mcp", "web_fetch"];

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
    /// The tool name before the arrow, one of [`TOOLS`].
    pub tool: String,
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

    let tool = command
        .split_once(COMMAND_ARROW)
        .map(|(tool, _)| tool.trim())
        .unwrap_or_default();
    if !TOOLS.contains(&tool) {
        return Err(format!(
            "unsupported tool {tool:?} in {command:?}; measured tools are {TOOLS:?}"
        ));
    }

    // Exactly one blank line separates the tool line from the options. Any other
    // line there is a continuation of a wrapped command, which was not measured.
    let gap = lines.get(title_index + 2).map(|line| line.trim());
    match gap {
        Some("") => {}
        Some(line) if line.strip_prefix(CURSOR).unwrap_or(line).trim() == OPTION_ALLOW_ONCE => {
            return Err(format!(
                "no blank line between {command:?} and the options; refusing an unmeasured layout"
            ));
        }
        other => {
            return Err(format!(
                "wrapped command: the line after {command:?} is {other:?}, not a single blank line; \
                 a command that wraps is refused"
            ));
        }
    }
    let next = title_index + 3;
    if lines.get(next).is_some_and(|line| line.trim().is_empty()) {
        return Err(format!(
            "expected a single blank line after {command:?}, found more than one"
        ));
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
        tool: tool.to_string(),
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

#[cfg(test)]
mod measured_tests {
    use super::*;

    const FS_WRITE_1: &str = include_str!("../tests/fixtures/kiro-3.3.0/fs_write-1.screen.txt");
    const FS_WRITE_2: &str = include_str!("../tests/fixtures/kiro-3.3.0/fs_write-2.screen.txt");
    const MCP_1: &str = include_str!("../tests/fixtures/kiro-3.3.0/mcp-1.screen.txt");
    const WEB_FETCH_1: &str = include_str!("../tests/fixtures/kiro-3.3.0/web_fetch-1.screen.txt");
    const WEB_SEARCH_1: &str = include_str!("../tests/fixtures/kiro-3.3.0/web_search-1.screen.txt");
    const WRAPPED_1: &str =
        include_str!("../tests/fixtures/kiro-3.3.0/shell_wrapped80-1.screen.txt");

    /// (fixture, tool, cursor) for the measured accepted dialogs.
    fn accepted() -> [(&'static str, &'static str, usize); 4] {
        [
            (FS_WRITE_1, "fs_write", 0),
            (FS_WRITE_2, "fs_write", 1),
            (MCP_1, "mcp", 0),
            (WEB_FETCH_1, "web_fetch", 0),
        ]
    }

    #[test]
    fn accepts_measured_3_3_0_dialogs_and_records_the_tool() {
        for (screen, tool, cursor) in accepted() {
            let dialog = parse(screen).unwrap();
            assert_eq!(dialog.tool, tool);
            assert_eq!(dialog.cursor, cursor);
            assert!(dialog.command.starts_with(&format!("{tool} → ")));
        }
    }

    #[test]
    fn records_shell_tool() {
        assert_eq!(parse(&fixtures::dialog_on_allow()).unwrap().tool, "shell");
    }

    #[test]
    fn refuses_web_search_without_arrow() {
        let err = parse(WEB_SEARCH_1).unwrap_err();
        assert!(err.contains("web_search requires approval"), "{err}");
        assert!(err.contains("not \"<tool> → <command>\""), "{err}");
    }

    #[test]
    fn refuses_wrapped_shell_command_explicitly() {
        let err = parse(WRAPPED_1).unwrap_err();
        assert!(err.contains("wrapped command"), "{err}");
    }

    #[test]
    fn refuses_unknown_tool_naming_it() {
        let screen = FS_WRITE_1.replace("fs_write → note.txt", "use_aws → s3 ls");
        let err = parse(&screen).unwrap_err();
        assert!(err.contains("unsupported tool \"use_aws\""), "{err}");
    }

    #[test]
    fn refuses_missing_or_extra_blank_line_before_options() {
        let none = FS_WRITE_1.replace("fs_write → note.txt\n\n", "fs_write → note.txt\n");
        let err = parse(&none).unwrap_err();
        assert!(err.contains("no blank line"), "{err}");
        let two = FS_WRITE_1.replace("fs_write → note.txt\n\n", "fs_write → note.txt\n\n\n");
        let err = parse(&two).unwrap_err();
        assert!(err.contains("single blank line"), "{err}");
    }
}
