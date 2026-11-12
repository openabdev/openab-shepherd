//! Escape-sequence filter for text read from panes.
//!
//! Removes CSI, OSC (including OSC 7501 status reports), DCS/SOS/PM/APC strings,
//! other two-byte and nF escapes, and lone ESC bytes. Newlines, tabs, and printable
//! text are kept. TUI redraw noise is not removed; it is a known limit.

const ESC: char = '\u{1b}';
const CSI_8BIT: char = '\u{9b}';
const BEL: char = '\u{7}';

pub fn strip_terminal_escapes(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            ESC => i = skip_escape(&chars, i + 1),
            CSI_8BIT => i = skip_csi(&chars, i + 1),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// `start` is the index just after an ESC. Returns the index to resume at.
fn skip_escape(chars: &[char], start: usize) -> usize {
    let Some(&next) = chars.get(start) else {
        return chars.len();
    };
    match next {
        '[' => skip_csi(chars, start + 1),
        ']' => skip_string(chars, start + 1, true),
        'P' | 'X' | '^' | '_' => skip_string(chars, start + 1, false),
        '\u{20}'..='\u{2f}' => {
            // nF escape: intermediate bytes, then one final byte.
            let mut j = start;
            while j < chars.len() && ('\u{20}'..='\u{2f}').contains(&chars[j]) {
                j += 1;
            }
            match chars.get(j) {
                Some(&c) if ('\u{30}'..='\u{7e}').contains(&c) => j + 1,
                Some(_) => j,
                None => chars.len(),
            }
        }
        // Two-byte escape such as ESC 7, ESC 8, or ESC c.
        '\u{30}'..='\u{7e}' => start + 1,
        // Lone ESC: drop it and let the next character be processed normally.
        _ => start,
    }
}

/// CSI body: parameter and intermediate bytes, then one final byte 0x40-0x7E.
fn skip_csi(chars: &[char], start: usize) -> usize {
    let mut j = start;
    while j < chars.len() && ('\u{20}'..='\u{3f}').contains(&chars[j]) {
        j += 1;
    }
    match chars.get(j) {
        Some(&c) if ('\u{40}'..='\u{7e}').contains(&c) => j + 1,
        Some(_) => j,
        None => chars.len(),
    }
}

/// String body (OSC, DCS, ...): ends at ST (ESC \), or at BEL when `bel_terminates`.
/// An unterminated string swallows the rest of the buffer. An ESC that does not
/// start ST ends the string and is left for the caller to process.
fn skip_string(chars: &[char], start: usize, bel_terminates: bool) -> usize {
    let mut j = start;
    while j < chars.len() {
        let c = chars[j];
        if bel_terminates && c == BEL {
            return j + 1;
        }
        if c == ESC {
            return if chars.get(j + 1) == Some(&'\\') {
                j + 2
            } else {
                j
            };
        }
        j += 1;
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi_stripped() {
        assert_eq!(strip_terminal_escapes("\u{1b}[31mred\u{1b}[0m"), "red");
    }

    #[test]
    fn csi_with_private_params_stripped() {
        assert_eq!(strip_terminal_escapes("a\u{1b}[?25lb\u{1b}[?25h"), "ab");
    }

    #[test]
    fn eight_bit_csi_stripped() {
        assert_eq!(strip_terminal_escapes("a\u{9b}31mb"), "ab");
    }

    #[test]
    fn osc_7501_bel_terminated_stripped() {
        assert_eq!(
            strip_terminal_escapes("\u{1b}]7501;state=done\u{7}hello"),
            "hello"
        );
    }

    #[test]
    fn osc_st_terminated_stripped() {
        assert_eq!(strip_terminal_escapes("\u{1b}]0;title\u{1b}\\x"), "x");
    }

    #[test]
    fn osc_unterminated_at_end_stripped() {
        assert_eq!(
            strip_terminal_escapes("keep\u{1b}]7501;state=working"),
            "keep"
        );
    }

    #[test]
    fn dcs_string_stripped() {
        assert_eq!(strip_terminal_escapes("a\u{1b}Pq#0;2;0;0;0\u{1b}\\b"), "ab");
    }

    #[test]
    fn two_byte_escapes_stripped() {
        assert_eq!(strip_terminal_escapes("a\u{1b}7b\u{1b}8c"), "abc");
        assert_eq!(strip_terminal_escapes("a\u{1b}cb"), "ab");
    }

    #[test]
    fn lone_escape_removed() {
        assert_eq!(strip_terminal_escapes("a\u{1b}"), "a");
    }

    #[test]
    fn newlines_tabs_and_text_kept() {
        let text = "line one\r\n\tline two 你好";
        assert_eq!(strip_terminal_escapes(text), text);
    }

    // The ST form is measured from OpenCode 2.2.7 TUI startup (four captured runs).
    // The BEL form is a generic OSC terminator case, not observed from OpenCode.
    #[test]
    fn opencode_startup_status_st_measured_bel_generic_stripped() {
        let bel = "\u{1b}]7501;state=idle:app=opencode\u{7}Build · Big Pickle";
        assert_eq!(strip_terminal_escapes(bel), "Build · Big Pickle");
        let st = "\u{1b}]7501;state=idle:app=opencode\u{1b}\\Build · Big Pickle";
        assert_eq!(strip_terminal_escapes(st), "Build · Big Pickle");
    }

    #[test]
    fn opencode_startup_queries_stripped() {
        let text = "\u{1b}]7501;?\u{1b}\\\u{1b}[>0q\u{1b}[>4;1mAsk anything";
        assert_eq!(strip_terminal_escapes(text), "Ask anything");
        let bel = "\u{1b}]7501;?\u{7}Ask anything";
        assert_eq!(strip_terminal_escapes(bel), "Ask anything");
    }
}
