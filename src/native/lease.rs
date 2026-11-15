//! The input lease and the draft flag of one pane (issue #77, shared panes).
//!
//! A person who types into a pane through a human client holds the pane's
//! input lease while they do, and the pane carries a draft flag from their
//! first byte until they press Enter (or an operator clears it). A bot's write
//! is refused while either exists; it is never queued. This module is the pure
//! part: the lease record, the idle rule, the injectable clock and the scanner
//! that finds the Enter of a human's byte stream. The state lives in
//! `PaneState` and the checks in `Pane::write_guarded`.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A lease that has had no accepted human input (and no explicit acquire) for
/// this long ends. Pings and the pane's own output do not renew it.
pub(crate) const LEASE_IDLE: Duration = Duration::from_secs(300);

/// The monotonic clock the lease idle rule reads. A closure so a test moves
/// time without sleeping; the default is `Instant::now`.
#[derive(Clone)]
pub(crate) struct LeaseClock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl LeaseClock {
    pub(crate) fn new(now: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    pub(crate) fn now(&self) -> Instant {
        (self.0)()
    }
}

impl Default for LeaseClock {
    fn default() -> Self {
        Self::new(Instant::now)
    }
}

impl fmt::Debug for LeaseClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LeaseClock")
    }
}

static NEXT_LEASE: AtomicU64 = AtomicU64::new(1);

/// The one input lease of a pane. Never persisted: a front that restarts has
/// none.
#[derive(Debug, Clone)]
pub(crate) struct Lease {
    /// Who holds it (the client and its connection), for display and for the
    /// "already held" refusal.
    pub holder: String,
    pub id: u64,
    pub last_input: Instant,
    pub scan: EnterScanner,
}

impl Lease {
    pub(crate) fn new(holder: String, now: Instant) -> Self {
        Self {
            holder,
            id: NEXT_LEASE.fetch_add(1, Ordering::Relaxed),
            last_input: now,
            scan: EnterScanner::default(),
        }
    }

    pub(crate) fn idle(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_input)
    }

    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.idle(now) >= LEASE_IDLE
    }

    pub(crate) fn expires_in(&self, now: Instant) -> Duration {
        LEASE_IDLE.saturating_sub(self.idle(now))
    }
}

/// What a scan of one human write found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scan {
    /// An Enter outside a bracketed paste was among the bytes.
    pub enter: bool,
    /// Something other than an Enter came after the last Enter (or, with no
    /// Enter, anywhere): the line is not empty again.
    pub trailing: bool,
}

/// Finds the Enter in a human's byte stream.
///
/// Enter is a CR, or an unmodified `CSI 13 u` (`ESC [ 13 u`, `ESC [ 13 ; 1 u`),
/// and only outside a bracketed paste (`ESC[200~` to `ESC[201~`). LF is not
/// Enter, and neither is a modified one such as Shift+Enter (`CSI 13 ; 2 u`):
/// several agent TUIs read those as a newline inside the prompt, so counting
/// them would let a bot submit over a fragment, which is the unsafe direction.
///
/// The state carries across calls because a client's frames split anywhere:
/// whether a paste is open, and an escape sequence cut in the middle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EnterScanner {
    in_paste: bool,
    /// The escape sequence being read, starting at ESC. Empty between them.
    pending: Vec<u8>,
}

/// A CSI longer than this is not one the scanner means to read: it is let go
/// as ordinary bytes.
const MAX_SEQUENCE: usize = 24;

const ESC: u8 = 0x1b;

impl EnterScanner {
    pub(crate) fn scan(&mut self, bytes: &[u8]) -> Scan {
        let mut out = Scan {
            enter: false,
            trailing: false,
        };
        for &byte in bytes {
            match self.step(byte) {
                Step::Enter => {
                    out.enter = true;
                    out.trailing = false;
                }
                Step::Text => out.trailing = true,
                Step::Pending => {}
            }
        }
        // A sequence left open at the end is something typed after the Enter.
        if !self.pending.is_empty() {
            out.trailing = true;
        }
        out
    }

    fn step(&mut self, byte: u8) -> Step {
        if self.pending.is_empty() {
            return match byte {
                ESC => {
                    self.pending.push(ESC);
                    Step::Pending
                }
                b'\r' if !self.in_paste => Step::Enter,
                _ => Step::Text,
            };
        }
        if self.pending.len() == 1 {
            // After the lone ESC.
            return match byte {
                b'[' => {
                    self.pending.push(byte);
                    Step::Pending
                }
                // ESC ESC: the first was a key of its own.
                ESC => Step::Text,
                _ => {
                    self.pending.clear();
                    Step::Text
                }
            };
        }
        self.pending.push(byte);
        match byte {
            // Parameter and intermediate bytes: the sequence goes on.
            0x20..=0x3f if self.pending.len() <= MAX_SEQUENCE => Step::Pending,
            0x40..=0x7e => {
                let sequence = std::mem::take(&mut self.pending);
                self.finish(&sequence)
            }
            _ => {
                self.pending.clear();
                // An ESC inside the sequence starts the next one.
                if byte == ESC {
                    self.pending.push(ESC);
                }
                Step::Text
            }
        }
    }

    fn finish(&mut self, sequence: &[u8]) -> Step {
        match sequence {
            b"\x1b[200~" => {
                self.in_paste = true;
                Step::Text
            }
            b"\x1b[201~" => {
                self.in_paste = false;
                Step::Text
            }
            b"\x1b[13u" | b"\x1b[13;1u" if !self.in_paste => Step::Enter,
            _ => Step::Text,
        }
    }
}

enum Step {
    Enter,
    Text,
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Scan {
        let mut scanner = EnterScanner::default();
        let mut last = Scan {
            enter: false,
            trailing: false,
        };
        let mut enter = false;
        for chunk in chunks {
            last = scanner.scan(chunk);
            enter |= last.enter;
        }
        Scan {
            enter,
            trailing: last.trailing,
        }
    }

    fn enters(bytes: &[u8]) -> bool {
        scan(&[bytes]).enter
    }

    #[test]
    fn cr_and_unmodified_csi_u_are_enter() {
        assert!(enters(b"\r"));
        assert!(enters(b"hello\r"));
        assert!(enters(b"\x1b[13u"));
        assert!(enters(b"\x1b[13;1u"));
        assert!(enters(b"text\x1b[13u"));
    }

    #[test]
    fn lf_and_modified_enter_are_not() {
        assert!(!enters(b"\n"));
        assert!(!enters(b"x\n"));
        assert!(!enters(b"\x1b[13;2u"), "Shift+Enter");
        assert!(!enters(b"\x1b[13;5u"), "Ctrl+Enter");
        assert!(!enters(b"\x1b[13;3u"), "Alt+Enter");
        assert!(!enters(b"\x1b\r"), "Alt+CR is not a bare Enter");
    }

    #[test]
    fn an_escape_inside_a_sequence_starts_the_next_one() {
        assert!(enters(b"\x1b[1\x1b[13u"));
    }

    #[test]
    fn other_keys_are_not() {
        for bytes in [
            &b"\x15"[..],
            b"\x03",
            b"\x1b[A",
            b"\x1b[1;2R",
            b"abc",
            b"\x1b[130u",
            b"\x1b[13~",
            b"\x1b[113u",
        ] {
            assert!(!enters(bytes), "{bytes:?}");
        }
    }

    #[test]
    fn a_cr_inside_a_bracketed_paste_is_not() {
        assert!(!enters(b"\x1b[200~line one\rline two\x1b[201~"));
        assert!(!enters(b"\x1b[200~a\x1b[13ub\x1b[201~"));
        // And the Enter after the paste is.
        assert!(enters(b"\x1b[200~a\rb\x1b[201~\r"));
    }

    #[test]
    fn a_paste_split_across_calls_stays_open() {
        assert!(!scan(&[b"\x1b[200~first\r", b"second\r", b"\x1b[201~"]).enter);
        assert!(scan(&[b"\x1b[200~first\r", b"\x1b[201~", b"\r"]).enter);
    }

    #[test]
    fn a_sequence_split_at_every_boundary_is_read_the_same() {
        for seq in [&b"\x1b[13u"[..], b"\x1b[13;1u"] {
            for cut in 0..=seq.len() {
                let got = scan(&[&seq[..cut], &seq[cut..]]);
                assert!(got.enter, "{seq:?} cut at {cut}");
            }
        }
        for seq in [
            &b"\x1b[13;2u"[..],
            b"\x1b[200~x\ry\x1b[201~",
            b"\x1b[1\x1b[13;2u",
        ] {
            for cut in 0..=seq.len() {
                let got = scan(&[&seq[..cut], &seq[cut..]]);
                assert!(!got.enter, "{seq:?} cut at {cut}");
            }
        }
    }

    #[test]
    fn text_after_the_last_enter_is_trailing() {
        assert_eq!(
            scan(&[b"a\r"]),
            Scan {
                enter: true,
                trailing: false
            }
        );
        assert_eq!(
            scan(&[b"a\rb"]),
            Scan {
                enter: true,
                trailing: true
            }
        );
        assert_eq!(
            scan(&[b"a\rb\r"]),
            Scan {
                enter: true,
                trailing: false
            }
        );
        assert_eq!(
            scan(&[b"\r\x1b"]),
            Scan {
                enter: true,
                trailing: true
            },
            "a sequence left open counts as typed"
        );
        assert_eq!(
            scan(&[b"x\n"]),
            Scan {
                enter: false,
                trailing: true
            }
        );
    }

    #[test]
    fn a_runaway_sequence_cannot_hide_an_enter() {
        let mut scanner = EnterScanner::default();
        let mut long = vec![ESC, b'['];
        long.extend(vec![b'1'; 100]);
        scanner.scan(&long);
        assert!(scanner.scan(b"\r").enter);
    }

    #[test]
    fn the_lease_idles_out_at_300_seconds() {
        let t0 = Instant::now();
        let lease = Lease::new("h".into(), t0);
        assert!(!lease.expired(t0 + Duration::from_secs(299)));
        assert!(lease.expired(t0 + Duration::from_secs(300)));
        assert_eq!(
            lease.expires_in(t0 + Duration::from_secs(100)),
            Duration::from_secs(200)
        );
        assert_eq!(
            lease.expires_in(t0 + Duration::from_secs(400)),
            Duration::ZERO
        );
    }
}
