//! Text parser for the OpenCode permission dialog (OpenCode v2.2.7).
//!
//! The layout was measured for a shell command and for a file edit. The three
//! options share one row, and the heading, the tool line and the option row
//! sit behind a `┃` bar:
//!
//! ```text
//!   ┃  △ Permission required
//!   ┃
//!   ┃  $ touch /tmp/oc-live-1/marker
//!   ┃
//!   ┃
//!   ┃   Allow once   Always allow   Reject  ctrl+f fullscreen  ⇆ select  enter confirm
//! ```
//!
//! The highlighted option is drawn with a background colour (selected `fab283`),
//! not with reverse video or a glyph, so it is not in the screen text. Shepherd's
//! screen read returns plain text, which means this module can establish that the
//! dialog is the expected one but never which option is highlighted. See
//! [`highlight_unverifiable`]. Anything that does not match is refused.

pub const HEADING: &str = "△ Permission required";
pub const SHELL_PREFIX: &str = "$ ";
pub const EDIT_PREFIX: &str = "→ Edit ";
pub const OPTION_ALLOW_ONCE: &str = "Allow once";
pub const OPTION_ALWAYS: &str = "Always allow";
pub const OPTION_REJECT: &str = "Reject";
/// The start of the hint after the options, the stable anchor of the row.
pub const HINT: &str = "ctrl+f fullscreen";
/// The bar OpenCode draws down the left of the dialog.
const BAR: char = '┃';

/// Which tool the dialog asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// A `$ <command>` line.
    Shell,
    /// A `→ Edit <path>` line, followed by a diff preview.
    Edit,
}

/// A dialog whose text matches the measured layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeDialog {
    pub tool: Tool,
    /// The command (shell) or the path (edit).
    pub subject: String,
    /// The three labels, in screen order.
    pub options: Vec<String>,
}

/// The refusal for `allow_once`: the text read from the pane carries no colour.
pub fn highlight_unverifiable() -> String {
    format!(
        "the highlighted option cannot be verified: OpenCode draws the highlight with a background colour, and Shepherd's screen read returns text without colours, so it cannot establish that {OPTION_ALLOW_ONCE:?} is the highlighted option"
    )
}

/// Parse the one permission dialog on the screen.
pub fn parse(screen: &str) -> Result<OpenCodeDialog, String> {
    let lines: Vec<&str> = screen.lines().map(content).collect();
    let headings: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with(HEADING))
        .map(|(index, _)| index)
        .collect();
    match headings.as_slice() {
        [] => Err(format!("no line starting {HEADING:?} on the screen")),
        [heading] => parse_block(&lines, *heading),
        many => Err(format!(
            "{} permission dialogs on the screen; expected one",
            many.len()
        )),
    }
}

/// A line without the bar and the blanks around the text.
fn content(line: &str) -> &str {
    line.trim_start_matches(|c: char| c.is_whitespace() || c == BAR)
        .trim_end()
}

fn parse_block(lines: &[&str], heading: usize) -> Result<OpenCodeDialog, String> {
    let rest = &lines[heading + 1..];
    let mut body = rest
        .iter()
        .copied()
        .enumerate()
        .filter(|(_, l)| !l.is_empty());
    let Some((tool_at, tool_line)) = body.next() else {
        return Err("nothing follows the heading".to_string());
    };
    let (tool, subject) = if let Some(command) = tool_line.strip_prefix(SHELL_PREFIX) {
        (Tool::Shell, command.trim())
    } else if let Some(path) = tool_line.strip_prefix(EDIT_PREFIX) {
        (Tool::Edit, path.trim())
    } else {
        return Err(format!(
            "the line after the heading is neither \"$ <command>\" nor \"→ Edit <path>\": {tool_line:?}"
        ));
    };
    if subject.is_empty() {
        return Err(format!(
            "the tool line has no command or path: {tool_line:?}"
        ));
    }
    let after = &rest[tool_at + 1..];
    let option_rows: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with(OPTION_ALLOW_ONCE))
        .map(|(index, _)| index)
        .collect();
    let &[row] = option_rows.as_slice() else {
        return Err(format!(
            "expected one option row starting {OPTION_ALLOW_ONCE:?} after the tool line, found {}",
            option_rows.len()
        ));
    };
    // A shell dialog has nothing but blank lines between the command and the
    // options. An edit dialog has a diff preview there.
    if tool == Tool::Shell {
        if let Some(extra) = after[..row].iter().find(|line| !line.is_empty()) {
            return Err(format!(
                "an unexpected line sits between the command and the options: {extra:?}"
            ));
        }
    }
    let options = parse_options(after[row])?;
    Ok(OpenCodeDialog {
        tool,
        subject: subject.to_string(),
        options,
    })
}

/// `Allow once   Always allow   Reject  ctrl+f fullscreen ...`, in that order.
fn parse_options(row: &str) -> Result<Vec<String>, String> {
    let mut rest = row;
    let mut options = Vec::with_capacity(3);
    for label in [OPTION_ALLOW_ONCE, OPTION_ALWAYS, OPTION_REJECT] {
        let Some(after) = rest.strip_prefix(label) else {
            return Err(format!(
                "options are not [{OPTION_ALLOW_ONCE}, {OPTION_ALWAYS}, {OPTION_REJECT}] in that order: {row:?}"
            ));
        };
        if !after.is_empty() && !after.starts_with(char::is_whitespace) {
            return Err(format!("text runs on after {label:?}: {row:?}"));
        }
        options.push(label.to_string());
        rest = after.trim_start();
    }
    if !rest.starts_with(HINT) {
        return Err(format!("the options are not followed by {HINT:?}: {row:?}"));
    }
    Ok(options)
}

/// Screens built from the measured layout above.
#[cfg(test)]
pub mod fixtures {
    const OPTIONS: &str =
        "  ┃   Allow once   Always allow   Reject  ctrl+f fullscreen  ⇆ select  enter confirm\n";

    /// The dialog for `touch /tmp/oc-live-1/marker`, as measured.
    pub fn shell_dialog() -> String {
        let mut out = String::from("  ┃  △ Permission required\n  ┃\n");
        out.push_str("  ┃  $ touch /tmp/oc-live-1/marker\n  ┃\n  ┃\n");
        out.push_str(OPTIONS);
        out
    }

    /// The dialog for a write of `/tmp/oc-live-6/note.txt`. The diff preview line
    /// is as measured; the blank lines around it are not.
    pub fn edit_dialog() -> String {
        let mut out = String::from("  ┃  △ Permission required\n  ┃\n");
        out.push_str("  ┃  → Edit /tmp/oc-live-6/note.txt\n  ┃\n  ┃  1 + hi\n  ┃\n");
        out.push_str(OPTIONS);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{edit_dialog, shell_dialog};
    use super::*;

    #[test]
    fn the_measured_shell_dialog_parses() {
        let dialog = parse(&shell_dialog()).unwrap();
        assert_eq!(dialog.tool, Tool::Shell);
        assert_eq!(dialog.subject, "touch /tmp/oc-live-1/marker");
        assert_eq!(dialog.options, ["Allow once", "Always allow", "Reject"]);
    }

    #[test]
    fn the_measured_edit_dialog_parses() {
        let dialog = parse(&edit_dialog()).unwrap();
        assert_eq!(dialog.tool, Tool::Edit);
        assert_eq!(dialog.subject, "/tmp/oc-live-6/note.txt");
    }

    #[test]
    fn text_to_the_right_of_the_dialog_is_ignored() {
        let screen = shell_dialog().replace("confirm\n", "confirm   Context 12%\n");
        assert!(parse(&screen).is_ok());
    }

    #[test]
    fn it_refuses_when_there_is_no_dialog_or_more_than_one() {
        assert!(parse("").unwrap_err().contains("no line starting"));
        assert!(parse("$ ls\n").is_err());
        let two = format!("{}{}", shell_dialog(), shell_dialog());
        assert!(parse(&two).unwrap_err().contains("2 permission dialogs"));
    }

    #[test]
    fn it_refuses_options_in_the_wrong_order() {
        let swapped =
            shell_dialog().replace("Allow once   Always allow", "Always allow   Allow once");
        assert!(parse(&swapped).is_err());
        let reject_first = shell_dialog().replace(
            "Allow once   Always allow   Reject",
            "Allow once   Reject   Always allow",
        );
        assert!(parse(&reject_first).unwrap_err().contains("in that order"));
    }

    #[test]
    fn it_refuses_a_missing_or_extra_option() {
        let missing = shell_dialog().replace("   Reject", "");
        assert!(parse(&missing).is_err());
        let extra = shell_dialog().replace("Reject  ctrl+f", "Reject   Other  ctrl+f");
        assert!(parse(&extra).is_err());
        let run_on = shell_dialog().replace("Reject  ctrl+f", "Rejected  ctrl+f");
        assert!(parse(&run_on).is_err());
        let no_hint = shell_dialog().replace("  ctrl+f fullscreen  ⇆ select  enter confirm", "");
        assert!(parse(&no_hint).unwrap_err().contains("not followed by"));
    }

    #[test]
    fn it_refuses_a_missing_or_unexpected_tool_line() {
        let no_tool = shell_dialog().replace("$ touch /tmp/oc-live-1/marker", "");
        assert!(parse(&no_tool).is_err());
        let other = shell_dialog().replace("$ touch", "% touch");
        assert!(parse(&other).unwrap_err().contains("neither"));
        let empty = shell_dialog().replace("$ touch /tmp/oc-live-1/marker", "$ ");
        assert!(parse(&empty).is_err());
    }

    #[test]
    fn a_shell_dialog_with_an_extra_line_is_refused() {
        let extra = shell_dialog().replace(
            "  ┃  $ touch /tmp/oc-live-1/marker\n",
            "  ┃  $ touch /tmp/oc-live-1/marker\n  ┃  && rm -rf /\n",
        );
        assert!(parse(&extra).unwrap_err().contains("unexpected line"));
    }

    #[test]
    fn a_dialog_with_two_option_rows_is_refused() {
        let two = format!(
            "{}{}",
            shell_dialog(),
            "  ┃   Allow once   Always allow   Reject  ctrl+f fullscreen\n"
        );
        assert!(parse(&two).unwrap_err().contains("found 2"));
    }

    #[test]
    fn a_dialog_without_its_options_is_refused() {
        let cut = "  ┃  △ Permission required\n  ┃\n  ┃  $ ls\n";
        assert!(parse(cut).unwrap_err().contains("found 0"));
        assert!(parse("  ┃  △ Permission required\n").is_err());
    }

    #[test]
    fn the_highlight_is_never_verified_from_text() {
        assert!(highlight_unverifiable().contains("cannot be verified"));
    }
}
