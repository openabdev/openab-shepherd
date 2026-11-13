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
//! not with reverse video or a glyph, so it is not in the screen text. The
//! styled screen carries it: [`verify_selected`] parses the text and then
//! requires exactly one of the three labels to be drawn with that background,
//! and that label to be `Allow once`. Anything that does not match is refused.

use crate::backend::{row_text, Color, StyledCell, StyledScreen};

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

/// The refusal for `allow_once` on a backend that cannot read cell colours.
pub fn colours_unavailable() -> String {
    format!(
        "the highlighted option cannot be verified: this terminal backend cannot read cell colours, and OpenCode draws the highlight with a background colour, so it cannot establish that {OPTION_ALLOW_ONCE:?} is the highlighted option"
    )
}

/// Parse the dialog from the styled screen's text, then require that
/// `Allow once` is the odd one out by background: its background differs from
/// both other labels' and those two share one background. The colours are
/// compared as `Color` values (Default, Idx, Rgb) without converting between
/// palette and RGB, so the rule holds for palette and truecolor alike. Any
/// reverse-video cell on a label, a label with mixed backgrounds, a default
/// background on `Allow once`, or any other pattern is refused.
pub fn verify_selected(screen: &StyledScreen) -> Result<OpenCodeDialog, String> {
    let dialog = parse(&screen.text())?;
    let rows: Vec<&Vec<StyledCell>> = screen
        .rows
        .iter()
        .filter(|row| content(&row_text(row)).starts_with(OPTION_ALLOW_ONCE))
        .collect();
    let &[row] = rows.as_slice() else {
        return Err(format!(
            "expected one option row in the styled screen, found {}",
            rows.len()
        ));
    };
    // One entry per character, with the column of the cell that holds it.
    let mut chars: Vec<(char, usize)> = Vec::new();
    for (column, cell) in row.iter().enumerate() {
        if cell.text.is_empty() {
            chars.push((' ', column));
        } else {
            chars.extend(cell.text.chars().map(|c| (c, column)));
        }
    }
    let mut at = chars
        .iter()
        .position(|(c, _)| !(c.is_whitespace() || *c == BAR))
        .unwrap_or(chars.len());
    let mut drawn = Vec::with_capacity(3);
    for label in [OPTION_ALLOW_ONCE, OPTION_ALWAYS, OPTION_REJECT] {
        let span = chars
            .get(at..at + label.chars().count())
            .filter(|span| span.iter().map(|(c, _)| *c).eq(label.chars()));
        let Some(span) = span else {
            return Err(format!("{label:?} is not where the text puts it"));
        };
        let mut bgs: Vec<Color> = Vec::new();
        for (c, column) in span.iter().filter(|(c, _)| !c.is_whitespace()) {
            let cell = &row[*column];
            if cell.reverse {
                return Err(format!(
                    "{label:?} is drawn with reverse video ({c:?}), which is not a highlight that can be verified"
                ));
            }
            bgs.push(cell.bg);
        }
        let Some(&first) = bgs.first() else {
            return Err(format!("{label:?} has no visible character"));
        };
        if bgs.iter().any(|bg| *bg != first) {
            return Err(format!(
                "{label:?} is only partly drawn with one background"
            ));
        }
        drawn.push(first);
        at += label.chars().count();
        while chars.get(at).is_some_and(|(c, _)| c.is_whitespace()) {
            at += 1;
        }
    }
    let (once, always, reject) = (drawn[0], drawn[1], drawn[2]);
    if once == always && once == reject {
        return Err("no option is highlighted: all three labels have the same background".to_string());
    }
    if always != reject {
        let who = if once == always {
            OPTION_REJECT
        } else if once == reject {
            OPTION_ALWAYS
        } else {
            "an option other than Allow once"
        };
        return Err(format!(
            "the highlighted option is {who:?}, not {OPTION_ALLOW_ONCE:?}, or the backgrounds are not one highlight over two equal options"
        ));
    }
    if once == Color::Default {
        return Err(format!(
            "{OPTION_ALLOW_ONCE:?} has the default background, which is not a readable highlight"
        ));
    }
    Ok(dialog)
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
    use crate::backend::{Color, StyledCell, StyledScreen};

    /// Measured colours: selected fg `0a0a0a` on bg `fab283`; unselected fg
    /// `eeeeee` on bg `1e1e1e`.
    pub const SELECTED: (Color, Color) =
        (Color::Rgb(0x0a, 0x0a, 0x0a), Color::Rgb(0xfa, 0xb2, 0x83));
    pub const UNSELECTED: (Color, Color) =
        (Color::Rgb(0xee, 0xee, 0xee), Color::Rgb(0x1e, 0x1e, 0x1e));

    /// Measured without COLORTERM (palette): selected fg `Idx(232)` on bg
    /// `Idx(216)`; unselected fg `Idx(255)` on bg `Idx(234)`.
    pub const PALETTE_SELECTED: (Color, Color) = (Color::Idx(232), Color::Idx(216));
    pub const PALETTE_UNSELECTED: (Color, Color) = (Color::Idx(255), Color::Idx(234));

    /// The styled form of `text`: every cell default-coloured, except that the
    /// characters of each label on the option row take the colours `paint`
    /// gives that label (None leaves them default). A character that is
    /// two columns wide is not used in the fixtures.
    pub fn styled(
        text: &str,
        paint: impl Fn(&str) -> Option<((Color, Color), bool)>,
    ) -> StyledScreen {
        let rows = text
            .lines()
            .map(|line| {
                let chars: Vec<char> = line.chars().collect();
                let mut cells: Vec<StyledCell> = chars
                    .iter()
                    .map(|c| StyledCell {
                        text: c.to_string(),
                        fg: Color::Default,
                        bg: Color::Default,
                        reverse: false,
                    })
                    .collect();
                if line.contains("Allow once") && line.contains("Reject") {
                    for label in ["Allow once", "Always allow", "Reject"] {
                        let start = find(&chars, label);
                        if let Some(((fg, bg), reverse)) = paint(label) {
                            for cell in &mut cells[start..start + label.chars().count()] {
                                cell.fg = fg;
                                cell.bg = bg;
                                cell.reverse = reverse;
                            }
                        }
                    }
                }
                cells.resize(
                    200,
                    StyledCell {
                        text: String::new(),
                        fg: Color::Default,
                        bg: Color::Default,
                        reverse: false,
                    },
                );
                cells
            })
            .collect();
        StyledScreen { rows }
    }

    fn find(hay: &[char], label: &str) -> usize {
        let needle: Vec<char> = label.chars().collect();
        (0..hay.len())
            .find(|i| hay[*i..].starts_with(&needle))
            .expect("label on the row")
    }

    /// The styled dialog with the given colours for `Allow once`,
    /// `Always allow` and `Reject`, in that order.
    pub fn styled_with(
        text: &str,
        once: (Color, Color),
        always: (Color, Color),
        reject: (Color, Color),
    ) -> StyledScreen {
        styled(text, |label| {
            Some((
                match label {
                    "Allow once" => once,
                    "Always allow" => always,
                    _ => reject,
                },
                false,
            ))
        })
    }

    /// The measured styled dialog with `selected` drawn as the highlight and
    /// the other labels as unselected.
    pub fn styled_with_selected(text: &str, selected: &str) -> StyledScreen {
        styled(text, |label| {
            Some((
                if label == selected {
                    SELECTED
                } else {
                    UNSELECTED
                },
                false,
            ))
        })
    }

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
    use super::fixtures::{
        edit_dialog, shell_dialog, styled, styled_with, styled_with_selected, PALETTE_SELECTED,
        PALETTE_UNSELECTED, SELECTED, UNSELECTED,
    };
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

    fn selected(text: &str, label: &str) -> StyledScreen {
        styled_with_selected(text, label)
    }

    #[test]
    fn the_measured_highlight_on_allow_once_is_verified() {
        for text in [shell_dialog(), edit_dialog()] {
            // Truecolor (COLORTERM=truecolor).
            let dialog = verify_selected(&selected(&text, "Allow once")).unwrap();
            assert_eq!(dialog, parse(&text).unwrap());
            // 256-colour palette (no COLORTERM), as Shepherd's panes see it.
            let palette = styled_with(
                &text,
                PALETTE_SELECTED,
                PALETTE_UNSELECTED,
                PALETTE_UNSELECTED,
            );
            assert_eq!(verify_selected(&palette).unwrap(), parse(&text).unwrap());
        }
    }

    #[test]
    fn a_highlight_on_another_option_is_refused() {
        for label in ["Always allow", "Reject"] {
            let err = verify_selected(&selected(&shell_dialog(), label)).unwrap_err();
            assert!(err.contains("not \"Allow once\""), "{err}");
            let (a, b) = if label == "Always allow" {
                (PALETTE_UNSELECTED, PALETTE_SELECTED)
            } else {
                (PALETTE_SELECTED, PALETTE_UNSELECTED)
            };
            let palette = if label == "Always allow" {
                styled_with(&shell_dialog(), a, b, a)
            } else {
                styled_with(&shell_dialog(), a, a, b)
            };
            assert!(verify_selected(&palette).is_err());
        }
    }

    #[test]
    fn all_three_the_same_is_refused() {
        for (sel, un) in [(SELECTED, UNSELECTED), (PALETTE_SELECTED, PALETTE_UNSELECTED)] {
            for c in [sel, un] {
                let screen = styled_with(&shell_dialog(), c, c, c);
                assert!(verify_selected(&screen)
                    .unwrap_err()
                    .contains("same background"));
            }
        }
        // Default colours everywhere.
        let plain = styled(&shell_dialog(), |_| None);
        assert!(verify_selected(&plain).is_err());
    }

    #[test]
    fn allow_once_equal_to_one_other_with_the_third_differing_is_refused() {
        for (sel, un) in [(SELECTED, UNSELECTED), (PALETTE_SELECTED, PALETTE_UNSELECTED)] {
            for (a, b, c) in [(sel, sel, un), (sel, un, sel), (un, un, sel), (un, sel, un)] {
                let screen = styled_with(&shell_dialog(), a, b, c);
                assert!(verify_selected(&screen).is_err(), "{a:?} {b:?} {c:?}");
            }
        }
    }

    #[test]
    fn three_different_backgrounds_are_refused() {
        let screen = styled_with(&shell_dialog(), SELECTED, UNSELECTED, PALETTE_SELECTED);
        assert!(verify_selected(&screen).is_err());
    }

    #[test]
    fn reverse_video_alone_is_not_a_highlight() {
        let screen = styled(&shell_dialog(), |label| {
            Some((UNSELECTED, label == "Allow once"))
        });
        assert!(verify_selected(&screen).is_err());
        // The highlight background with reverse set shows inverted: refused.
        let inverted = styled(&shell_dialog(), |label| {
            Some((
                if label == "Allow once" {
                    SELECTED
                } else {
                    UNSELECTED
                },
                label == "Allow once",
            ))
        });
        assert!(verify_selected(&inverted).unwrap_err().contains("reverse"));
        // Reverse on another label is refused too.
        let other = styled(&shell_dialog(), |label| {
            Some((
                if label == "Allow once" {
                    SELECTED
                } else {
                    UNSELECTED
                },
                label == "Reject",
            ))
        });
        assert!(verify_selected(&other).is_err());
    }

    #[test]
    fn a_default_background_on_allow_once_is_refused() {
        let screen = styled_with(
            &shell_dialog(),
            (SELECTED.0, Color::Default),
            UNSELECTED,
            UNSELECTED,
        );
        assert!(verify_selected(&screen)
            .unwrap_err()
            .contains("default background"));
    }

    #[test]
    fn a_partly_drawn_label_is_refused() {
        let mut screen = selected(&shell_dialog(), "Allow once");
        let row = screen
            .rows
            .iter_mut()
            .find(|row| crate::backend::row_text(row).contains("Allow once"))
            .unwrap();
        let at = row.iter().position(|c| c.text == "A").unwrap();
        row[at + 9].bg = UNSELECTED.1; // the final "e"
        assert!(verify_selected(&screen).unwrap_err().contains("partly"));
    }

    #[test]
    fn the_text_checks_still_apply_to_the_styled_screen() {
        let wrong =
            shell_dialog().replace("Allow once   Always allow", "Always allow   Allow once");
        assert!(verify_selected(&selected_any(&wrong)).is_err());
    }

    fn selected_any(text: &str) -> StyledScreen {
        styled(text, |_| None)
    }
}
