//! The VT emulator over a pane's output (ADR §4, §8): `vt100::Parser` with
//! hooks for OSC 7501, the terminal queries the core answers, the kitty
//! keyboard stacks and synchronized output. The parser is the only parser of
//! the output stream; its state persists across `process` calls, so a sequence
//! split across reads is still seen whole.

use super::keys::KeyboardMode;
use super::status::{self, Report};
use std::num::NonZeroU16;
use std::time::{Duration, Instant};

pub(crate) const ROWS: u16 = 120;
pub(crate) const COLS: u16 = 200;
pub(crate) const SCROLLBACK: usize = 2000;
/// Entries per kitty keyboard stack.
const KITTY_STACK_MAX: usize = 16;
const KITTY_MASK: u16 = 0x1f;
/// A synchronized update open longer than this counts as closed.
const SYNC_MAX: Duration = Duration::from_secs(1);

const DA1: &[u8] = b"\x1b[?62;22c";
const DA2: &[u8] = b"\x1b[>1;10;0c";
const DSR_OK: &[u8] = b"\x1b[0n";
const FOREGROUND: &str = "rgb:c7c7/c7c7/c7c7";
const BACKGROUND: &str = "rgb:1e1e/1e1e/1e1e";

/// One screen's kitty keyboard flags and stack.
#[derive(Debug, Clone, Default)]
struct Kitty {
    flags: u8,
    stack: Vec<u8>,
}

impl Kitty {
    fn push(&mut self, flags: u16) {
        if self.stack.len() == KITTY_STACK_MAX {
            self.stack.remove(0);
        }
        self.stack.push(self.flags);
        self.flags = mask(flags);
    }

    fn pop(&mut self, count: u16) {
        let count = usize::from(count.max(1));
        if count > self.stack.len() {
            self.stack.clear();
            self.flags = 0;
            return;
        }
        for _ in 0..count {
            self.flags = self.stack.pop().unwrap_or(0);
        }
    }

    fn set(&mut self, flags: u16, mode: u16) {
        let flags = mask(flags);
        match mode {
            0 | 1 => self.flags = flags,
            2 => self.flags |= flags,
            3 => self.flags &= !flags,
            _ => {}
        }
    }
}

fn mask(flags: u16) -> u8 {
    (flags & KITTY_MASK) as u8
}

/// What the parser's callbacks collect, in stream order.
#[derive(Debug, Default)]
pub(crate) struct Hooks {
    /// OSC 7501 reports: the body after `7501;`, and what it said.
    reports: Vec<(Vec<u8>, Report)>,
    /// Replies to terminal queries, to be written to the program.
    replies: Vec<u8>,
    /// Kitty keyboard state, indexed by `alternate_screen() as usize`.
    kitty: [Kitty; 2],
    /// When the open synchronized update (`CSI ? 2026 h`) began.
    sync_since: Option<Instant>,
    /// Scrollback rows of the main screen dropped off the top.
    evicted: u64,
    /// The byte being processed is CAN or SUB: vte dispatches an open OSC on
    /// it, but a terminal aborts the sequence, so it is not used.
    aborting: bool,
}

impl Hooks {
    fn color_query(&mut self, params: &[&[u8]]) {
        let Some(first) = params.first() else {
            return;
        };
        let base: u16 = match *first {
            b"10" => 10,
            b"11" => 11,
            b"12" => 12,
            _ => return,
        };
        let groups = &params[1..];
        if groups.is_empty() || groups.iter().any(|group| *group != b"?") {
            return;
        }
        for code in (base..).take(groups.len()) {
            let color = match code {
                10 | 12 => FOREGROUND,
                11 => BACKGROUND,
                _ => continue,
            };
            self.reply(format!("\x1b]{code};{color}\x07").as_bytes());
        }
    }

    fn reply(&mut self, bytes: &[u8]) {
        self.replies.extend_from_slice(bytes);
    }
}

impl vt100::Callbacks for Hooks {
    fn unhandled_osc(&mut self, _screen: &mut vt100::Screen, params: &[&[u8]]) {
        if let Some(report) = status::parse_osc(params) {
            let report = if self.aborting {
                Report::Discard("aborted by CAN or SUB")
            } else {
                report
            };
            if report == Report::Query {
                self.reply(status::QUERY_REPLY);
            }
            let body = params.get(1..).unwrap_or(&[]).join(&b';');
            self.reports.push((body, report));
        } else if !self.aborting {
            self.color_query(params);
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if i2.is_some() {
            return;
        }
        let num = |i: usize| params.get(i).and_then(|p| p.first()).copied().unwrap_or(0);
        let has_2026 = || params.iter().any(|p| p.first() == Some(&2026));
        let kitty = &mut self.kitty[usize::from(screen.alternate_screen())];
        match (i1, c) {
            (None, 'c') if num(0) == 0 => self.reply(DA1),
            (Some(b'>'), 'c') if num(0) == 0 => self.reply(DA2),
            (None, 'n') if num(0) == 5 => self.reply(DSR_OK),
            (None, 'n') if num(0) == 6 => {
                let (row, col) = screen.cursor_position();
                self.reply(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            (Some(b'?'), 'n') if num(0) == 6 => {
                let (row, col) = screen.cursor_position();
                self.reply(format!("\x1b[?{};{}R", row + 1, col + 1).as_bytes());
            }
            (Some(b'?'), 'u') => {
                let flags = kitty.flags;
                self.reply(format!("\x1b[?{flags}u").as_bytes());
            }
            (Some(b'>'), 'u') => kitty.push(num(0)),
            (Some(b'<'), 'u') => kitty.pop(num(0)),
            (Some(b'='), 'u') => kitty.set(num(0), num(1)),
            (Some(b'?'), 'h') if has_2026() => {
                self.sync_since.get_or_insert_with(Instant::now);
            }
            (Some(b'?'), 'l') if has_2026() => self.sync_since = None,
            _ => {}
        }
    }

    fn on_scroll(&mut self, _contents: vt100::capture::RowContents<'_>, alternate_screen: bool) {
        if !alternate_screen {
            self.evicted += 1;
        }
    }
}

/// A pane's screen, scrollback and terminal modes.
pub(crate) struct Emulator {
    parser: vt100::Parser<Hooks>,
}

impl Emulator {
    pub(crate) fn new() -> Self {
        let rows = NonZeroU16::new(ROWS).expect("ROWS is not zero");
        let cols = NonZeroU16::new(COLS).expect("COLS is not zero");
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Hooks::default()),
        }
    }

    /// Feed program output. CAN and SUB are fed on their own, so an OSC they
    /// end is known to be aborted rather than terminated.
    pub(crate) fn process(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some(at) = rest.iter().position(|&b| b == 0x18 || b == 0x1a) {
            self.parser.process(&rest[..at]);
            self.parser.callbacks_mut().aborting = true;
            self.parser.process(&rest[at..=at]);
            self.parser.callbacks_mut().aborting = false;
            rest = &rest[at + 1..];
        }
        self.parser.process(rest);
    }

    /// OSC 7501 reports since the last call, in stream order.
    pub(crate) fn take_reports(&mut self) -> Vec<(Vec<u8>, Report)> {
        std::mem::take(&mut self.parser.callbacks_mut().reports)
    }

    /// Query replies since the last call, in stream order.
    pub(crate) fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }

    /// DECCKM and the active screen's kitty flags.
    pub(crate) fn mode(&self) -> KeyboardMode {
        let screen = self.parser.screen();
        KeyboardMode {
            app_cursor: screen.application_cursor(),
            kitty_flags: self.parser.callbacks().kitty[usize::from(screen.alternate_screen())]
                .flags,
        }
    }

    pub(crate) fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    pub(crate) fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// A synchronized update is open and began less than 1 s before `now`.
    pub(crate) fn sync_open(&self, now: Instant) -> bool {
        self.parser
            .callbacks()
            .sync_since
            .is_some_and(|since| now.saturating_duration_since(since) < SYNC_MAX)
    }

    /// The visible screen: rows trimmed at the end, trailing empty rows dropped.
    pub(crate) fn screen_text(&self) -> String {
        join_rows(visible_rows(self.parser.screen()))
    }

    /// The last `lines` scrollback rows (as many as there are) followed by the
    /// screen. `complete` is false when more was asked for than is kept and
    /// rows have been dropped. The scrollback offset is back at 0 on return.
    pub(crate) fn history_text(&mut self, lines: usize) -> (String, bool) {
        let screen = self.parser.screen_mut();
        let len = screen.scrollback_len();
        let page = usize::from(ROWS);
        let mut rows = Vec::new();
        let mut offset = lines.min(len);
        while offset > 0 {
            screen.set_scrollback(offset);
            rows.extend(visible_rows(screen).into_iter().take(offset.min(page)));
            offset = offset.saturating_sub(page);
        }
        screen.set_scrollback(0);
        rows.extend(visible_rows(screen));
        let complete = !(lines > len && self.parser.callbacks().evicted > 0);
        (join_rows(rows), complete)
    }
}

fn visible_rows(screen: &vt100::Screen) -> Vec<String> {
    screen
        .rows(0, COLS)
        .map(|row| row.trim_end().to_string())
        .collect()
}

fn join_rows(mut rows: Vec<String>) -> String {
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCKED_BODY: &[u8] = b"state=blocked:kind=permission:app=kiro-cli";

    fn replies(input: &[u8]) -> Vec<u8> {
        let mut emu = Emulator::new();
        emu.process(input);
        emu.take_replies()
    }

    fn blocked() -> Report {
        Report::Status {
            state_word: "blocked".into(),
            kind: Some("permission".into()),
        }
    }

    fn statuses(emu: &mut Emulator) -> Vec<(Vec<u8>, Report)> {
        emu.take_reports()
            .into_iter()
            .filter(|(_, r)| matches!(r, Report::Status { .. }))
            .collect()
    }

    #[test]
    fn device_and_status_replies() {
        assert_eq!(replies(b"\x1b[c"), b"\x1b[?62;22c");
        assert_eq!(replies(b"\x1b[0c"), b"\x1b[?62;22c");
        assert_eq!(replies(b"\x1b[>c"), b"\x1b[>1;10;0c");
        assert_eq!(replies(b"\x1b[>0c"), b"\x1b[>1;10;0c");
        assert_eq!(replies(b"\x1b[5n"), b"\x1b[0n");
        assert_eq!(replies(b"\x1b[6n"), b"\x1b[1;1R");
        assert_eq!(replies(b"\x1b[5;7H\x1b[6n"), b"\x1b[5;7R");
        assert_eq!(replies(b"\x1b[5;7H\x1b[?6n"), b"\x1b[?5;7R");
        assert_eq!(replies(b"abc\r\nde\x1b[6n"), b"\x1b[2;3R");
        // Several queries in one chunk are answered in order.
        assert_eq!(
            replies(b"\x1b[c\x1b[3;4H\x1b[6n\x1b[5n"),
            b"\x1b[?62;22c\x1b[3;4R\x1b[0n"
        );
    }

    #[test]
    fn unanswered_queries_get_nothing() {
        for query in [
            &b"\x1b[>q"[..],
            b"\x1b[?2026$p",
            b"\x1b[=c",
            b"\x1b[?996n",
            b"\x1b[1c",
            b"\x1b]4;1;?\x07",
            b"\x1bP+q544e\x1b\\",
        ] {
            assert_eq!(replies(query), b"", "{query:?}");
        }
    }

    #[test]
    fn a_query_split_across_calls_is_answered_once() {
        let mut emu = Emulator::new();
        emu.process(b"\x1b[3;9H\x1b");
        emu.process(b"[");
        assert!(emu.take_replies().is_empty());
        emu.process(b"6n");
        assert_eq!(emu.take_replies(), b"\x1b[3;9R");
    }

    #[test]
    fn colour_queries() {
        assert_eq!(replies(b"\x1b]10;?\x07"), b"\x1b]10;rgb:c7c7/c7c7/c7c7\x07");
        assert_eq!(replies(b"\x1b]11;?\x07"), b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07");
        assert_eq!(replies(b"\x1b]12;?\x07"), b"\x1b]12;rgb:c7c7/c7c7/c7c7\x07");
        // ST-terminated queries are answered with BEL.
        assert_eq!(
            replies(b"\x1b]11;?\x1b\\"),
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07"
        );
        assert_eq!(
            replies(b"\x1b]10;?;?\x07"),
            b"\x1b]10;rgb:c7c7/c7c7/c7c7\x07\x1b]11;rgb:1e1e/1e1e/1e1e\x07"
        );
        assert_eq!(
            replies(b"\x1b]11;?;?\x07"),
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07\x1b]12;rgb:c7c7/c7c7/c7c7\x07"
        );
        // Sets, and sets mixed with queries, get no reply.
        assert_eq!(replies(b"\x1b]11;rgb:0000/0000/0000\x07"), b"");
        assert_eq!(replies(b"\x1b]10;?;#000000\x07"), b"");
        assert_eq!(replies(b"\x1b]10;#ffffff;?\x07"), b"");
        assert_eq!(replies(b"\x1b]11\x07"), b"");
    }

    #[test]
    fn status_query_gets_the_capability_reply() {
        let mut emu = Emulator::new();
        emu.process(b"\x1b]7501;?\x07");
        assert_eq!(emu.take_replies(), status::QUERY_REPLY);
        assert_eq!(emu.take_reports(), [(b"?".to_vec(), Report::Query)]);
    }

    #[test]
    fn a_report_split_at_every_byte_is_seen_once() {
        let bel = [&b"\x1b]7501;"[..], BLOCKED_BODY, b"\x07"].concat();
        let st = [&b"\x1b]7501;"[..], BLOCKED_BODY, b"\x1b\\"].concat();
        let in_frame = [
            &b"\x1b[?2026h\x1b[1;1Hredraw "[..],
            &bel,
            b" more\x1b[?2026l",
        ]
        .concat();
        let st_in_frame = [&b"text "[..], &st, b"\x1b[?2026hframe\x1b[?2026l"].concat();
        for report in [&bel, &st, &in_frame, &st_in_frame] {
            for at in 0..=report.len() {
                let mut emu = Emulator::new();
                emu.process(&report[..at]);
                emu.process(&report[at..]);
                assert_eq!(
                    emu.take_reports(),
                    [(BLOCKED_BODY.to_vec(), blocked())],
                    "{:?} split at {at}",
                    String::from_utf8_lossy(report)
                );
            }
        }
    }

    #[test]
    fn can_or_sub_mid_report_aborts_it() {
        for abort in [0x18u8, 0x1a] {
            let mut emu = Emulator::new();
            let mut input = b"\x1b]7501;state=bl".to_vec();
            input.push(abort);
            input.extend_from_slice(b"ocked\x07");
            emu.process(&input);
            let reports = emu.take_reports();
            assert_eq!(
                reports,
                [(
                    b"state=bl".to_vec(),
                    Report::Discard("aborted by CAN or SUB")
                )]
            );
            // The parser recovers: the next report is read.
            emu.process(b"\x1b]7501;state=idle\x07");
            assert_eq!(statuses(&mut emu).len(), 1);
        }
        // A colour query aborted by CAN gets no reply.
        assert_eq!(replies(b"\x1b]11;?\x18"), b"");
    }

    #[test]
    fn an_unterminated_osc_is_bounded_and_discarded() {
        let mut emu = Emulator::new();
        emu.process(b"\x1b]7501;state=idle:app=");
        for _ in 0..100 {
            emu.process(&[b'a'; 1000]);
        }
        emu.process(b"\x07");
        let reports = emu.take_reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].1, Report::Discard("truncated"));
        assert!(reports[0].0.len() <= status::OSC_BODY_MAX);
    }

    #[test]
    fn garbage_does_not_panic() {
        let mut emu = Emulator::new();
        let mut seed: u64 = 0x5eed;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        for _ in 0..2000 {
            let len = (next() % 64) as usize;
            let chunk: Vec<u8> = (0..len)
                .map(|_| match next() % 8 {
                    0 => 0x1b,
                    1 => b']',
                    2 => b'[',
                    3 => b';',
                    _ => next() as u8,
                })
                .collect();
            emu.process(&chunk);
            emu.take_reports();
            emu.take_replies();
        }
        let _ = emu.screen_text();
        let _ = emu.history_text(10_000);
        for lone in [
            &b"\x1b"[..],
            b"\x1b]",
            b"\x1b[",
            b"\x1b]7501;",
            b"\x9b",
            b"\xff",
        ] {
            let mut emu = Emulator::new();
            emu.process(lone);
            assert!(emu.take_reports().is_empty());
            let _ = emu.screen_text();
        }
    }

    #[test]
    fn kitty_push_pop_and_set() {
        let mut emu = Emulator::new();
        let flags = |emu: &mut Emulator| {
            emu.process(b"\x1b[?u");
            emu.take_replies()
        };
        assert_eq!(flags(&mut emu), b"\x1b[?0u");
        emu.process(b"\x1b[>1u");
        assert_eq!(flags(&mut emu), b"\x1b[?1u");
        emu.process(b"\x1b[>8u");
        assert_eq!(emu.mode().kitty_flags, 8);
        emu.process(b"\x1b[<u");
        assert_eq!(flags(&mut emu), b"\x1b[?1u");
        emu.process(b"\x1b[<1u");
        assert_eq!(flags(&mut emu), b"\x1b[?0u");
        // Popping past the bottom clears.
        emu.process(b"\x1b[>3u\x1b[<5u");
        assert_eq!(emu.mode().kitty_flags, 0);
        // Set modes: 1 sets (the default), 2 ORs, 3 subtracts, others ignored.
        emu.process(b"\x1b[=5u");
        assert_eq!(emu.mode().kitty_flags, 5);
        emu.process(b"\x1b[=2;2u");
        assert_eq!(emu.mode().kitty_flags, 7);
        emu.process(b"\x1b[=1;3u");
        assert_eq!(emu.mode().kitty_flags, 6);
        emu.process(b"\x1b[=31;9u");
        assert_eq!(emu.mode().kitty_flags, 6);
        emu.process(b"\x1b[=255;1u");
        assert_eq!(emu.mode().kitty_flags, 31);
        emu.process(b"\x1b[>255u");
        assert_eq!(flags(&mut emu), b"\x1b[?31u");
    }

    #[test]
    fn kitty_stack_is_bounded_at_16() {
        let mut emu = Emulator::new();
        for k in 1..=20 {
            emu.process(format!("\x1b[>{k}u").as_bytes());
        }
        assert_eq!(emu.mode().kitty_flags, 20);
        emu.process(b"\x1b[<16u");
        // The four oldest entries (0, 1, 2, 3) were dropped.
        assert_eq!(emu.mode().kitty_flags, 4);
        emu.process(b"\x1b[<u");
        assert_eq!(emu.mode().kitty_flags, 0);
    }

    #[test]
    fn kitty_state_is_per_screen() {
        let mut emu = Emulator::new();
        emu.process(b"\x1b[>1u\x1b[?1049h\x1b[?u");
        assert!(emu.alternate_screen());
        assert_eq!(emu.take_replies(), b"\x1b[?0u");
        emu.process(b"\x1b[>8u\x1b[?u");
        assert_eq!(emu.take_replies(), b"\x1b[?8u");
        assert_eq!(emu.mode().kitty_flags, 8);
        emu.process(b"\x1b[?1049l\x1b[?u");
        assert!(!emu.alternate_screen());
        assert_eq!(emu.take_replies(), b"\x1b[?1u");
    }

    #[test]
    fn modes_follow_the_screen() {
        let mut emu = Emulator::new();
        assert_eq!(emu.mode(), KeyboardMode::default());
        assert!(!emu.bracketed_paste());
        emu.process(b"\x1b[?1h\x1b[?2004h");
        assert!(emu.mode().app_cursor);
        assert!(emu.bracketed_paste());
        emu.process(b"\x1b[?1l\x1b[?2004l");
        assert!(!emu.mode().app_cursor);
        assert!(!emu.bracketed_paste());
    }

    #[test]
    fn synchronized_output_is_tracked() {
        let mut emu = Emulator::new();
        assert!(!emu.sync_open(Instant::now()));
        emu.process(b"\x1b[?2026h");
        assert!(emu.sync_open(Instant::now()));
        assert!(!emu.sync_open(Instant::now() + Duration::from_secs(2)));
        emu.process(b"\x1b[?2026l");
        assert!(!emu.sync_open(Instant::now()));
        // Inside a list of modes, too.
        emu.process(b"\x1b[?25;2026h");
        assert!(emu.sync_open(Instant::now()));
        emu.process(b"\x1b[?2026;25l");
        assert!(!emu.sync_open(Instant::now()));
    }

    #[test]
    fn screen_text_is_trimmed() {
        let mut emu = Emulator::new();
        emu.process(b"a   \r\n  b  \r\n\r\n");
        assert_eq!(emu.screen_text(), "a\n  b");
        assert_eq!(emu.history_text(0), ("a\n  b".to_string(), true));
    }

    #[test]
    fn history_pages_and_restores_the_offset() {
        let mut emu = Emulator::new();
        for i in 0..2170 {
            emu.process(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(emu.parser.screen().scrollback_len(), SCROLLBACK);

        let (text, complete) = emu.history_text(10);
        let lines: Vec<&str> = text.lines().collect();
        assert!(complete);
        assert_eq!(lines.len(), 10 + 119);
        assert_eq!(lines[0], "line 2041");
        assert_eq!(lines[9], "line 2050");
        assert_eq!(lines[10], "line 2051");
        assert_eq!(*lines.last().unwrap(), "line 2169");
        assert_eq!(emu.parser.screen().scrollback(), 0);

        // A page boundary inside the history.
        let (text, complete) = emu.history_text(250);
        assert!(complete);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 250 + 119);
        assert!(lines
            .iter()
            .enumerate()
            .all(|(n, line)| *line == format!("line {}", 1801 + n)));

        // More than is kept, after rows were dropped: incomplete.
        let (text, complete) = emu.history_text(5000);
        assert!(!complete);
        assert_eq!(text.lines().next(), Some("line 51"));
        assert_eq!(text.lines().count(), SCROLLBACK + 119);
        assert_eq!(emu.parser.screen().scrollback(), 0);
        assert!(emu.screen_text().starts_with("line 2051\n"));
    }

    #[test]
    fn history_without_eviction_is_complete() {
        let mut emu = Emulator::new();
        for i in 0..200 {
            emu.process(format!("line {i}\r\n").as_bytes());
        }
        let (text, complete) = emu.history_text(5000);
        assert!(complete);
        assert_eq!(text.lines().next(), Some("line 0"));
        assert_eq!(text.lines().count(), 200);
    }
}
