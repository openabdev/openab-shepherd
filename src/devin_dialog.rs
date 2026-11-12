//! Text parser for the Cognition Devin CLI permission dialog (Devin CLI 3000.12.43).
//!
//! The layout was measured for a shell command. The three options are one per
//! row, in this order, and the highlighted row starts with `❭`:
//!
//! ```text
//!  Running command
//!  $ touch shepherd_marker.txt
//!
//!  ❭ Allow ← ◼ ◼◼◼ → Once Exec(touch shepherd_marker.txt)
//!    Bypass mode
//!    Deny
//!  ↑↓ select · ←→ 1-4 scope · e edit · ↵ confirm · esc cancel
//! ```
//!
//! The Allow row may carry the scope selector after its label. Anything else is
//! refused rather than guessed.

use crate::agent::ParsedDialog;

pub const TITLE: &str = "Running command";
pub const COMMAND_PREFIX: &str = "$ ";
pub const CURSOR: char = '❭';
pub const OPTION_ALLOW: &str = "Allow";
pub const OPTION_BYPASS: &str = "Bypass mode";
pub const OPTION_DENY: &str = "Deny";
/// The scope that `allow_once` approves, and the arrow before the scope label.
pub const SCOPE_ONCE: &str = "Once";
const SCOPE_ARROW: char = '→';
/// Positions of the three options, top to bottom.
pub const INDEX_ALLOW: usize = 0;
pub const INDEX_BYPASS: usize = 1;
pub const INDEX_DENY: usize = 2;

/// Parse the one permission dialog on the screen.
pub fn parse(screen: &str) -> Result<ParsedDialog, String> {
    let lines: Vec<&str> = screen.lines().collect();
    let titles: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == TITLE)
        .map(|(index, _)| index)
        .collect();
    match titles.as_slice() {
        [] => Err(format!("no line reading {TITLE:?} on the screen")),
        [title] => parse_block(&lines, *title),
        many => Err(format!(
            "{} permission dialogs parsed on the screen; expected one",
            many.len()
        )),
    }
}

fn parse_block(lines: &[&str], title_index: usize) -> Result<ParsedDialog, String> {
    let next = skip_blank(lines, title_index + 1);
    let command_line = lines.get(next).map_or("", |line| line.trim());
    let Some(command) = command_line.strip_prefix(COMMAND_PREFIX) else {
        return Err(format!(
            "the line after {TITLE:?} is not \"$ <command>\": {command_line:?}"
        ));
    };
    let next = skip_blank(lines, next + 1);

    let rows: Vec<&str> = lines
        .iter()
        .skip(next)
        .take(3)
        .map(|line| line.trim())
        .collect();
    if rows.len() < 3 {
        return Err(format!(
            "expected three option rows after the command, found {}",
            rows.len()
        ));
    }
    let mut options = Vec::with_capacity(3);
    let mut cursors = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let label = match row.strip_prefix(CURSOR) {
            Some(rest) => {
                cursors.push(index);
                rest.trim()
            }
            None => row,
        };
        options.push(label.to_string());
    }
    let in_order = is_allow_row(&options[INDEX_ALLOW])
        && options[INDEX_BYPASS] == OPTION_BYPASS
        && options[INDEX_DENY] == OPTION_DENY;
    if !in_order {
        return Err(format!(
            "options are not [{OPTION_ALLOW}, {OPTION_BYPASS}, {OPTION_DENY}] in that order: {options:?}"
        ));
    }
    let &[cursor] = cursors.as_slice() else {
        return Err(format!(
            "expected one {CURSOR} cursor among the options, found {}",
            cursors.len()
        ));
    };
    Ok(ParsedDialog {
        title: TITLE.to_string(),
        command: command.trim().to_string(),
        options,
        cursor,
    })
}

fn is_allow_row(text: &str) -> bool {
    text == OPTION_ALLOW || text.starts_with("Allow ")
}

/// Whether the Allow row's scope selector reads `Once`, the scope `allow_once`
/// approves. The scope label is the first word after the `→`, as in
/// `Allow ← ◼ ◼◼◼ → Once Exec(...)`. A row with no scope, or another scope, is not Once.
pub fn allow_scope_is_once(allow_row: &str) -> bool {
    allow_row
        .split_once(SCOPE_ARROW)
        .and_then(|(_, scope)| scope.split_whitespace().next())
        == Some(SCOPE_ONCE)
}

fn skip_blank(lines: &[&str], mut index: usize) -> usize {
    while lines.get(index).is_some_and(|line| line.trim().is_empty()) {
        index += 1;
    }
    index
}

/// Screens built from the measured layout above.
#[cfg(test)]
pub mod fixtures {
    /// The dialog for `touch shepherd_marker.txt` with the highlight on
    /// `cursor` (0 Allow, 1 Bypass mode, 2 Deny).
    pub fn dialog_with_cursor_on(cursor: usize) -> String {
        let rows = [
            "Allow ← ◼ ◼◼◼ → Once Exec(touch shepherd_marker.txt)",
            "Bypass mode",
            "Deny",
        ];
        let mut out = String::from(" Running command\n $ touch shepherd_marker.txt\n\n");
        for (index, row) in rows.iter().enumerate() {
            let mark = if index == cursor { super::CURSOR } else { ' ' };
            out.push_str(&format!(" {mark} {row}\n"));
        }
        out.push_str(" ↑↓ select · ←→ 1-4 scope · e edit · ↵ confirm · esc cancel\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::dialog_with_cursor_on;
    use super::*;
    use crate::dialog::fixtures::dialog_on_allow;

    #[test]
    fn parses_the_highlight_on_each_option() {
        for (cursor, want) in [(0, INDEX_ALLOW), (1, INDEX_BYPASS), (2, INDEX_DENY)] {
            let dialog = parse(&dialog_with_cursor_on(cursor)).unwrap();
            assert_eq!(dialog.title, TITLE);
            assert_eq!(dialog.command, "touch shepherd_marker.txt");
            assert_eq!(dialog.cursor, want);
            assert_eq!(dialog.options.len(), 3);
            assert_eq!(dialog.options[1], OPTION_BYPASS);
            assert_eq!(dialog.options[2], OPTION_DENY);
            assert!(dialog.options[0].starts_with("Allow "));
        }
    }

    #[test]
    fn allow_row_keeps_its_scope_text_and_cursor_is_removed() {
        let dialog = parse(&dialog_with_cursor_on(2)).unwrap();
        assert_eq!(
            dialog.options[0],
            "Allow ← ◼ ◼◼◼ → Once Exec(touch shepherd_marker.txt)"
        );
        assert_eq!(dialog.options[2], "Deny");
    }

    #[test]
    fn allow_scope_is_read_from_the_allow_row() {
        assert!(allow_scope_is_once(
            "Allow ← ◼ ◼◼◼ → Once Exec(touch shepherd_marker.txt)"
        ));
        assert!(allow_scope_is_once("Allow ← ◼ ◼◼◼ → Once"));
        assert!(!allow_scope_is_once("Allow"));
        assert!(!allow_scope_is_once(
            "Allow ← ◼ ◼◼◼ → Session Exec(touch shepherd_marker.txt)"
        ));
        assert!(!allow_scope_is_once("Allow ← ◼ → Onceler"));
    }

    #[test]
    fn does_not_read_the_kiro_dialog() {
        assert!(parse(&dialog_on_allow()).is_err());
    }

    #[test]
    fn refuses_a_screen_without_the_title() {
        let err = parse(" $ touch x\n ❭ Allow\n   Bypass mode\n   Deny\n").unwrap_err();
        assert!(err.contains("no line reading"), "{err}");
    }

    #[test]
    fn refuses_two_dialogs() {
        let screen = format!("{}{}", dialog_with_cursor_on(0), dialog_with_cursor_on(0));
        let err = parse(&screen).unwrap_err();
        assert!(err.contains("2 permission dialogs"), "{err}");
    }

    #[test]
    fn refuses_a_missing_command_line() {
        let screen = dialog_with_cursor_on(0).replace(" $ touch shepherd_marker.txt\n", "");
        let err = parse(&screen).unwrap_err();
        assert!(err.contains("$ <command>"), "{err}");
    }

    #[test]
    fn refuses_options_out_of_order_or_with_extra_text() {
        let deny_first = dialog_with_cursor_on(0)
            .replace("   Bypass mode\n   Deny\n", "   Deny\n   Bypass mode\n");
        let err = parse(&deny_first).unwrap_err();
        assert!(err.contains("in that order"), "{err}");
        let extra = dialog_with_cursor_on(0).replace("Bypass mode", "Bypass mode now");
        assert!(parse(&extra).is_err());
    }

    #[test]
    fn refuses_a_missing_or_doubled_cursor() {
        let none = dialog_with_cursor_on(0).replace('❭', " ");
        let err = parse(&none).unwrap_err();
        assert!(err.contains("found 0"), "{err}");
        let two = dialog_with_cursor_on(0).replace(" Bypass mode", "❭ Bypass mode");
        let err = parse(&two).unwrap_err();
        assert!(err.contains("found 2"), "{err}");
    }

    #[test]
    fn refuses_fewer_than_three_rows() {
        // Five lines: the title, the command, a blank, and two option rows.
        let screen: String = dialog_with_cursor_on(0)
            .lines()
            .take(5)
            .map(|line| format!("{line}\n"))
            .collect();
        let err = parse(&screen).unwrap_err();
        assert!(err.contains("three option rows"), "{err}");
    }
}
