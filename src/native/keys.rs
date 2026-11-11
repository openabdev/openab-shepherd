//! Key encoding for the program's keyboard modes (ADR §7). Pure.

use crate::backend::{BackendError, ErrorKind, Key};

/// Kitty flag 1: disambiguate escape codes.
const KITTY_DISAMBIGUATE: u8 = 1;
/// Kitty flag 8: report all keys as escape codes.
const KITTY_ALL_KEYS: u8 = 8;

/// The keyboard modes a key's bytes depend on, read from the emulator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardMode {
    /// DECCKM (application cursor keys).
    pub app_cursor: bool,
    /// The kitty keyboard flags of the active screen.
    pub kitty_flags: u8,
}

/// The bytes for `key` under `mode`. `Key::Other` is refused.
pub(crate) fn encode(key: &Key, mode: KeyboardMode) -> Result<&'static [u8], BackendError> {
    let kitty = mode.kitty_flags & (KITTY_DISAMBIGUATE | KITTY_ALL_KEYS) != 0;
    let all_keys = mode.kitty_flags & KITTY_ALL_KEYS != 0;
    let app_cursor = mode.app_cursor && !kitty;
    let out: &'static [u8] = match key {
        Key::Enter if all_keys => b"\x1b[13u",
        Key::Enter => b"\r",
        Key::Up if app_cursor => b"\x1bOA",
        Key::Up => b"\x1b[A",
        Key::Down if app_cursor => b"\x1bOB",
        Key::Down => b"\x1b[B",
        Key::Esc if kitty => b"\x1b[27u",
        Key::Esc => b"\x1b",
        Key::CtrlC if kitty => b"\x1b[99;5u",
        Key::CtrlC => b"\x03",
        Key::Other => {
            let op = "sending an unknown key";
            return Err(BackendError::new(
                ErrorKind::Unsupported(op.to_string()),
                format!("native: not supported: {op}"),
            ));
        }
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(key: Key, kitty_flags: u8, app_cursor: bool) -> &'static [u8] {
        encode(
            &key,
            KeyboardMode {
                app_cursor,
                kitty_flags,
            },
        )
        .unwrap()
    }

    #[test]
    fn the_full_table() {
        // (flags, DECCKM) -> [Enter, Up, Down, Esc, CtrlC]
        let legacy: [&[u8]; 5] = [b"\r", b"\x1b[A", b"\x1b[B", b"\x1b", b"\x03"];
        let legacy_ckm: [&[u8]; 5] = [b"\r", b"\x1bOA", b"\x1bOB", b"\x1b", b"\x03"];
        let disambiguate: [&[u8]; 5] = [b"\r", b"\x1b[A", b"\x1b[B", b"\x1b[27u", b"\x1b[99;5u"];
        let all_keys: [&[u8]; 5] = [
            b"\x1b[13u",
            b"\x1b[A",
            b"\x1b[B",
            b"\x1b[27u",
            b"\x1b[99;5u",
        ];
        let cases: [(u8, bool, [&[u8]; 5]); 10] = [
            (0, false, legacy),
            (0, true, legacy_ckm),
            (1, false, disambiguate),
            (1, true, disambiguate),
            (1 | 2, false, disambiguate),
            (1 | 2, true, disambiguate),
            (8, false, all_keys),
            (8, true, all_keys),
            (31, false, all_keys),
            (31, true, all_keys),
        ];
        let keys = [Key::Enter, Key::Up, Key::Down, Key::Esc, Key::CtrlC];
        for (flags, ckm, expected) in cases {
            for (key, want) in keys.iter().zip(expected) {
                assert_eq!(
                    bytes(key.clone(), flags, ckm),
                    want,
                    "{key:?} flags={flags} decckm={ckm}"
                );
            }
        }
    }

    #[test]
    fn flags_without_1_or_8_stay_legacy() {
        assert_eq!(bytes(Key::Esc, 2, false), b"\x1b");
        assert_eq!(bytes(Key::Up, 4 | 16, true), b"\x1bOA");
    }

    #[test]
    fn other_is_refused() {
        let err = encode(&Key::Other, KeyboardMode::default()).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::Unsupported("sending an unknown key".into())
        );
        assert_eq!(err.detail, "native: not supported: sending an unknown key");
    }
}
