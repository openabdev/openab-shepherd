//! Input rules for text that reaches the terminal backend.
//!
//! Every string field that is sent to a pane passes through `validate_input`:
//! a control character is refused, the text is trimmed, and empty or oversized
//! results are refused. The text that passes is what gets sent.

use std::fmt;

/// Largest accepted input, in UTF-8 bytes, measured after trimming. This is the
/// exact text sent to the backend.
pub const MAX_INPUT_BYTES: usize = 4096;

/// Longest accepted pane name (1-32 of a-z 0-9 - _ .).
pub const MAX_NAME_CHARS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputError(pub String);

impl fmt::Display for InputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

/// Control characters are refused, except TAB, which a shell line may need.
/// This covers newline, CR, ESC, DEL, and the C1 range U+0080..=U+009F. A newline
/// would break one-message-per-line framing, so it must never pass through.
fn is_refused(c: char) -> bool {
    c.is_control() && c != '\t'
}

/// Return the text to send, or an error naming the rule that failed.
pub fn validate_input(text: &str) -> Result<String, InputError> {
    if let Some(c) = text.chars().find(|&c| is_refused(c)) {
        return Err(InputError(format!(
            "input contains control character U+{:04X}; remove it and retry",
            c as u32
        )));
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(InputError("input is empty after trimming".to_string()));
    }
    let size = trimmed.len();
    if size > MAX_INPUT_BYTES {
        return Err(InputError(format!(
            "input is {size} bytes; the limit is {MAX_INPUT_BYTES} bytes of UTF-8"
        )));
    }
    Ok(trimmed.to_string())
}

/// Terminal names: the input rules, then 1-32 characters of a-z, 0-9, '-', '_', '.'.
pub fn validate_name(text: &str) -> Result<String, InputError> {
    let cleaned = validate_input(text)?;
    let allowed = cleaned
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'));
    if !allowed || cleaned.chars().count() > MAX_NAME_CHARS {
        return Err(InputError(format!(
            "name must be 1-{MAX_NAME_CHARS} characters of a-z, 0-9, '-', '_', '.'"
        )));
    }
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err_text(result: Result<String, InputError>) -> String {
        result.expect_err("expected rejection").0
    }

    #[test]
    fn empty_input_rejected() {
        assert!(err_text(validate_input("")).contains("empty"));
    }

    #[test]
    fn whitespace_only_rejected() {
        assert!(err_text(validate_input("   \t  ")).contains("empty"));
    }

    #[test]
    fn only_control_characters_rejected() {
        assert!(
            err_text(validate_input("\u{1}\u{2}\u{7f}\u{9b}")).contains("control character U+0001")
        );
    }

    #[test]
    fn input_is_trimmed() {
        assert_eq!(validate_input("  echo hi  ").unwrap(), "echo hi");
    }

    #[test]
    fn oversized_ascii_rejected() {
        let text = "a".repeat(MAX_INPUT_BYTES + 1);
        assert!(err_text(validate_input(&text)).contains("limit is 4096"));
    }

    #[test]
    fn exact_limit_accepted() {
        let text = "a".repeat(MAX_INPUT_BYTES);
        assert_eq!(validate_input(&text).unwrap(), text);
    }

    #[test]
    fn limit_counts_utf8_bytes_not_characters() {
        // "é" is 2 bytes in UTF-8: 2049 of them is 4098 bytes.
        assert!(validate_input(&"é".repeat(2049)).is_err());
        assert_eq!(validate_input(&"é".repeat(2048)).unwrap(), "é".repeat(2048));
    }

    #[test]
    fn control_characters_refused_not_stripped() {
        let err = err_text(validate_input("ls\u{7} -la"));
        assert!(err.contains("control character U+0007"), "{err}");
        assert!(err_text(validate_input("ls\u{7f}")).contains("U+007F"));
        assert!(err_text(validate_input("ls\u{85}")).contains("U+0085"));
    }

    #[test]
    fn newline_and_cr_refused() {
        // Framing depends on one message per line, so a newline must never pass.
        assert!(err_text(validate_input("echo a\necho b")).contains("U+000A"));
        assert!(err_text(validate_input("echo a\r")).contains("U+000D"));
    }

    #[test]
    fn tab_kept_inside_input() {
        assert_eq!(validate_input("a\tb").unwrap(), "a\tb");
    }

    #[test]
    fn escape_byte_refused() {
        assert!(err_text(validate_input("\u{1b}[31mred")).contains("U+001B"));
    }

    #[test]
    fn ordinary_text_including_non_ascii_intact() {
        let text = "echo 你好 café — ok ✓ $HOME";
        assert_eq!(validate_input(text).unwrap(), text);
    }

    #[test]
    fn name_rules() {
        assert_eq!(validate_name("build-1.2_x").unwrap(), "build-1.2_x");
        assert!(validate_name("Upper").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name(&"a".repeat(33)).is_err());
        assert!(validate_name(&"a".repeat(32)).is_ok());
        assert!(validate_name("   ").is_err());
    }
}
