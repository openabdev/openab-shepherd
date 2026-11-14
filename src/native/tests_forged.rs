//! OSC 7501 parser robustness. A program in a pane controls the bytes it
//! prints, so the emulator must cope with a status that is split across
//! reads, oversized, or printed on the alternate screen: no panic, no early
//! report, and bounded memory. A pane printing its own status is not a
//! security boundary (see the trust model); these tests only cover the parser.

use super::status::{self, Report};
use super::term::Emulator;

fn status_of(word: &str) -> Report {
    Report::Status {
        state_word: word.to_string(),
        kind: None,
    }
}

/// Feed `bytes` to a fresh emulator: its reports (body, outcome).
fn feed(bytes: &[u8]) -> Vec<(Vec<u8>, Report)> {
    let mut emu = Emulator::new();
    emu.process(bytes);
    emu.take_reports()
}

#[test]
fn a_status_split_at_every_byte_is_seen_once_and_never_early() {
    for terminator in [&b"\x07"[..], b"\x1b\\"] {
        let whole = [&b"text \x1b]7501;state=done"[..], terminator, b" more"].concat();
        for at in 0..=whole.len() {
            let mut emu = Emulator::new();
            emu.process(&whole[..at]);
            let early = emu.take_reports();
            // The parser ends an OSC at the first byte of its terminator, so
            // nothing is reported before that byte has arrived.
            let ended = at > whole.len() - b" more".len() - terminator.len();
            assert_eq!(early.len(), usize::from(ended), "split at {at}");
            emu.process(&whole[at..]);
            assert_eq!(early.len() + emu.take_reports().len(), 1, "split at {at}");
        }
    }
}

#[test]
fn a_huge_field_is_discarded_and_memory_stays_bounded() {
    // A field far past the OSC body limit is discarded as truncated, and the
    // body kept for the log is at most the limit.
    let huge = [&b"\x1b]7501;state=idle:x="[..], &vec![b'a'; 4000], b"\x07"].concat();
    let reports = feed(&huge);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].1, Report::Discard("truncated"));
    assert!(reports[0].0.len() <= status::OSC_BODY_MAX);

    // Ten megabytes of an unterminated body in 64 KiB reads: no report while
    // it is open; the terminator then yields one bounded discard and the next
    // real status parses normally.
    let mut emu = Emulator::new();
    emu.process(b"\x1b]7501;state=idle:x=");
    let chunk = vec![b'a'; 64 * 1024];
    for _ in 0..160 {
        emu.process(&chunk);
        assert!(emu.take_reports().is_empty());
    }
    emu.process(b"\x07\x1b]7501;state=working\x07");
    let reports = emu.take_reports();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_eq!(reports[0].1, Report::Discard("truncated"));
    assert!(reports[0].0.len() <= status::OSC_BODY_MAX);
    assert_eq!(reports[1].1, status_of("working"));
}

#[test]
fn a_status_on_the_alternate_screen_parses_like_any_other() {
    let reports = feed(b"\x1b[?1049h\x1b]7501;state=idle\x07\x1b[?1049l");
    assert_eq!(reports, [(b"state=idle".to_vec(), status_of("idle"))]);
    // An unterminated body on the alternate screen reports nothing and does
    // not panic.
    assert!(feed(b"\x1b[?1049h\x1b]7501;state=idle").is_empty());
}
