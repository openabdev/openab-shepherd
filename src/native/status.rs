//! OSC 7501 program status: the body parser and the turn tracker (ADR §5, §6).
//!
//! Pure: no I/O and no clock. The emulator (`term.rs`) is the only parser of
//! the output stream; it hands each OSC's params here.

use crate::backend::{dialog_kind, ProgramStatus, State};
use std::collections::VecDeque;

/// vte (without std) keeps at most this many bytes of an OSC.
pub(crate) const OSC_BODY_MAX: usize = 1024;
/// vte splits an OSC into at most this many params.
const OSC_PARAMS_MAX: usize = 16;
/// The longest state or kind word.
const WORD_MAX: usize = 32;
/// Entries kept in a pane's status log.
pub(crate) const STATUS_LOG_LEN: usize = 64;

/// The reply to `OSC 7501 ; ?`: hc's capability string, ended with BEL.
pub(crate) const QUERY_REPLY: &[u8] =
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x07";

/// What one OSC 7501 said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Report {
    /// A status report. `kind` is set only for `blocked`, and is the raw word.
    Status {
        state_word: String,
        kind: Option<String>,
    },
    /// The capability query `?`.
    Query,
    /// A report that could not be used, and why.
    Discard(&'static str),
}

/// Parse one OSC, given as vte's `;`-split params. None when it is not 7501.
pub(crate) fn parse_osc(params: &[&[u8]]) -> Option<Report> {
    if params.first() != Some(&&b"7501"[..]) {
        return None;
    }
    let total: usize = params.iter().map(|p| p.len()).sum::<usize>() + params.len() - 1;
    if params.len() >= OSC_PARAMS_MAX || total >= OSC_BODY_MAX - 1 {
        return Some(Report::Discard("truncated"));
    }
    let body = params[1..].join(&b';');
    if body == b"?" {
        return Some(Report::Query);
    }
    let mut state: Option<&[u8]> = None;
    let mut kind: Option<&[u8]> = None;
    for field in body.split(|&b| b == b':') {
        if !field.is_ascii() {
            continue;
        }
        let Some(eq) = field.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, value) = (&field[..eq], &field[eq + 1..]);
        match key {
            b"state" if state.is_none() => state = Some(value),
            b"kind" if kind.is_none() => kind = Some(value),
            _ => {}
        }
    }
    let Some(state) = state else {
        return Some(Report::Discard("missing state"));
    };
    if !is_word(state) {
        return Some(Report::Discard("bad state word"));
    }
    if kind.is_some_and(|k| !is_word(k)) {
        return Some(Report::Discard("bad kind word"));
    }
    let state_word = String::from_utf8_lossy(state).into_owned();
    let kind = kind
        .filter(|_| state_word == "blocked")
        .map(|k| String::from_utf8_lossy(k).into_owned());
    Some(Report::Status { state_word, kind })
}

fn is_word(word: &[u8]) -> bool {
    (1..=WORD_MAX).contains(&word.len())
        && word
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// What [`Tracker::apply`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Applied {
    pub turn_changed: bool,
    pub status_changed: bool,
}

/// A pane's status, `reported`, `turn` and `status_ms`. The kind is kept raw
/// (for turn comparison and the log) and exported through `dialog_kind`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Tracker {
    status: Option<(State, Option<String>)>,
    reported: bool,
    turn: u64,
    status_ms: Option<u64>,
}

impl Tracker {
    /// Apply a report (ADR §6). The turn goes up when the status leaves
    /// working, or on entering blocked with a (state, kind) different from the
    /// current one. `status_ms` moves only when (state, kind) changes.
    pub(crate) fn apply(&mut self, state: State, raw_kind: Option<String>, now_ms: u64) -> Applied {
        let prev = self.status.as_ref();
        let leaves_working =
            prev.is_some_and(|(s, _)| *s == State::Working) && state != State::Working;
        let new_block = state == State::Blocked
            && prev.is_none_or(|(s, k)| *s != State::Blocked || *k != raw_kind);
        let status_changed = prev.is_none_or(|(s, k)| *s != state || *k != raw_kind);
        let turn_changed = leaves_working || new_block;
        if turn_changed {
            self.turn += 1;
        }
        if status_changed {
            self.status_ms = Some(now_ms);
        }
        self.status = Some((state, raw_kind));
        self.reported = true;
        Applied {
            turn_changed,
            status_changed,
        }
    }

    /// The program ended: no status any more; `reported` and `turn` stay.
    pub(crate) fn on_exit(&mut self) {
        self.status = None;
    }

    /// The status as Shepherd sees it, with the kind through `dialog_kind`.
    pub(crate) fn program_status(&self) -> Option<ProgramStatus> {
        self.status.as_ref().map(|(state, kind)| ProgramStatus {
            state: state.clone(),
            kind: kind.as_deref().map(dialog_kind),
        })
    }

    pub(crate) fn state(&self) -> Option<&State> {
        self.status.as_ref().map(|(state, _)| state)
    }

    pub(crate) fn raw_kind(&self) -> Option<&str> {
        self.status.as_ref().and_then(|(_, kind)| kind.as_deref())
    }

    pub(crate) fn reported(&self) -> bool {
        self.reported
    }

    pub(crate) fn turn(&self) -> u64 {
        self.turn
    }

    pub(crate) fn status_ms(&self) -> Option<u64> {
        self.status_ms
    }
}

/// One status log entry: a report as it arrived and what came of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub at_ms: u64,
    /// The OSC body after `7501;`, at most [`OSC_BODY_MAX`] bytes.
    pub body: Vec<u8>,
    /// The parsed status, or why it was discarded.
    pub outcome: String,
    pub turn_after: u64,
}

/// The last [`STATUS_LOG_LEN`] reports of a pane, oldest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StatusLog {
    entries: VecDeque<LogEntry>,
}

impl StatusLog {
    pub(crate) fn push(&mut self, mut entry: LogEntry) {
        entry.body.truncate(OSC_BODY_MAX);
        if self.entries.len() == STATUS_LOG_LEN {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub(crate) fn entries(&self) -> Vec<LogEntry> {
        self.entries.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Option<Report> {
        let params: Vec<&[u8]> = body.as_bytes().split(|&b| b == b';').collect();
        parse_osc(&params)
    }

    fn status(state: &str, kind: Option<&str>) -> Option<Report> {
        Some(Report::Status {
            state_word: state.to_string(),
            kind: kind.map(str::to_string),
        })
    }

    #[test]
    fn a_whole_report_parses() {
        assert_eq!(
            parse("7501;state=blocked:kind=permission:app=kiro-cli"),
            status("blocked", Some("permission"))
        );
        assert_eq!(
            parse("7501;state=working:app=kiro-cli"),
            status("working", None)
        );
    }

    #[test]
    fn only_7501_is_ours() {
        assert_eq!(parse("7502;state=idle"), None);
        assert_eq!(parse("11;?"), None);
        assert_eq!(parse_osc(&[]), None);
    }

    #[test]
    fn query_is_recognized() {
        assert_eq!(parse("7501;?"), Some(Report::Query));
    }

    #[test]
    fn opencode_startup_report_is_idle() {
        // Measured: OpenCode's startup report is `state=idle:app=opencode`.
        assert_eq!(parse("7501;state=idle:app=opencode"), status("idle", None));
    }

    #[test]
    fn fields_follow_the_rules() {
        // Unknown keys ignored; the first key wins.
        assert_eq!(parse("7501;app=kiro-cli:state=done"), status("done", None));
        assert_eq!(parse("7501;state=done:state=working"), status("done", None));
        assert_eq!(
            parse("7501;state=blocked:kind=question:kind=auth"),
            status("blocked", Some("question"))
        );
        // A field without '=' is ignored; the value splits at the first '='.
        assert_eq!(parse("7501;junk:state=idle"), status("idle", None));
        assert_eq!(
            parse("7501;state=idle=x"),
            Some(Report::Discard("bad state word"))
        );
        // A ';' inside the body is part of it.
        assert_eq!(parse("7501;app=a;b:state=idle"), status("idle", None));
    }

    #[test]
    fn bad_reports_are_discarded() {
        assert_eq!(
            parse("7501;app=kiro-cli"),
            Some(Report::Discard("missing state"))
        );
        assert_eq!(parse("7501;"), Some(Report::Discard("missing state")));
        assert_eq!(parse("7501"), Some(Report::Discard("missing state")));
        assert_eq!(
            parse("7501;state="),
            Some(Report::Discard("bad state word"))
        );
        assert_eq!(
            parse("7501;state=Idle"),
            Some(Report::Discard("bad state word"))
        );
        let long = format!("7501;state={}", "a".repeat(33));
        assert_eq!(parse(&long), Some(Report::Discard("bad state word")));
        let max = format!("7501;state={}", "a".repeat(32));
        assert_eq!(parse(&max), status(&"a".repeat(32), None));
        assert_eq!(
            parse("7501;state=blocked:kind=Perm"),
            Some(Report::Discard("bad kind word"))
        );
        // A non-ASCII field is ignored, so its state does not count.
        assert_eq!(
            parse("7501;state=wörk"),
            Some(Report::Discard("missing state"))
        );
        assert_eq!(parse("7501;x=é:state=idle"), status("idle", None));
    }

    #[test]
    fn truncated_osc_is_discarded() {
        let sixteen = format!("7501;state=idle:{}", ";x".repeat(14));
        assert_eq!(sixteen.split(';').count(), 16);
        assert_eq!(parse(&sixteen), Some(Report::Discard("truncated")));
        let fifteen = format!("7501;state=idle:{}", ";x".repeat(13));
        assert_eq!(parse(&fifteen), status("idle", None));

        let pad = |n: usize| format!("7501;state=idle:app={}", "a".repeat(n - 20));
        assert_eq!(pad(1023).len(), 1023);
        assert_eq!(parse(&pad(1023)), Some(Report::Discard("truncated")));
        assert_eq!(parse(&pad(1022)), status("idle", None));
    }

    #[test]
    fn kind_is_kept_only_for_blocked() {
        assert_eq!(
            parse("7501;state=done:kind=permission"),
            status("done", None)
        );
        assert_eq!(
            parse("7501;state=blocked:kind=elicitation"),
            status("blocked", Some("elicitation"))
        );
    }

    fn feed(tracker: &mut Tracker, words: &[&str]) -> Vec<u64> {
        words
            .iter()
            .enumerate()
            .map(|(i, word)| {
                let (state, kind) = match word.split_once('/') {
                    Some((s, k)) => (s, Some(k.to_string())),
                    None => (*word, None),
                };
                tracker.apply(State::parse(state), kind, i as u64);
                tracker.turn()
            })
            .collect()
    }

    #[test]
    fn measured_streams_give_the_measured_turns() {
        let mut t = Tracker::default();
        assert!(!t.reported());
        assert_eq!(feed(&mut t, &["idle", "working", "done"]), [0, 0, 1]);
        assert!(t.reported());

        let mut t = Tracker::default();
        let turns = feed(
            &mut t,
            &[
                "idle",
                "working",
                "blocked/permission",
                "working",
                "done",
                "working",
                "blocked/permission",
                "working",
                "done",
            ],
        );
        assert_eq!(turns, [0, 0, 1, 1, 2, 2, 3, 3, 4]);
    }

    #[test]
    fn a_repeated_report_changes_nothing() {
        let mut t = Tracker::default();
        t.apply(State::Done, None, 10);
        let applied = t.apply(State::Done, None, 20);
        assert_eq!(
            applied,
            Applied {
                turn_changed: false,
                status_changed: false
            }
        );
        assert_eq!(t.status_ms(), Some(10));
        assert_eq!(t.turn(), 0);

        t.apply(State::Blocked, Some("permission".into()), 30);
        let applied = t.apply(State::Blocked, Some("permission".into()), 40);
        assert!(!applied.turn_changed && !applied.status_changed);
        assert_eq!((t.turn(), t.status_ms()), (1, Some(30)));
    }

    #[test]
    fn blocked_without_working_goes_up() {
        let mut t = Tracker::default();
        feed(&mut t, &["done"]);
        assert!(
            t.apply(State::Blocked, Some("permission".into()), 1)
                .turn_changed
        );
        assert_eq!(t.turn(), 1);

        let mut t = Tracker::default();
        feed(&mut t, &["idle"]);
        assert!(t.apply(State::Blocked, None, 1).turn_changed);
        assert_eq!(t.turn(), 1);

        // From no status at all, too.
        let mut t = Tracker::default();
        assert!(t.apply(State::Blocked, Some("auth".into()), 1).turn_changed);
    }

    #[test]
    fn a_kind_change_while_blocked_goes_up() {
        let mut t = Tracker::default();
        assert_eq!(
            feed(
                &mut t,
                &["working", "blocked/permission", "blocked/question"]
            ),
            [0, 1, 2]
        );
    }

    #[test]
    fn exit_clears_status_and_keeps_turn() {
        let mut t = Tracker::default();
        feed(&mut t, &["working", "done"]);
        t.on_exit();
        assert_eq!(t.program_status(), None);
        assert!(t.reported());
        assert_eq!(t.turn(), 1);
        // A report after exit is not working, so leaving nothing bumps nothing.
        assert!(!t.apply(State::Done, None, 5).turn_changed);
    }

    #[test]
    fn exported_kind_goes_through_dialog_kind() {
        let mut t = Tracker::default();
        t.apply(State::Blocked, Some("elicitation".into()), 1);
        assert_eq!(t.raw_kind(), Some("elicitation"));
        assert_eq!(
            t.program_status(),
            Some(ProgramStatus {
                state: State::Blocked,
                kind: Some("other".into())
            })
        );
        t.apply(State::Blocked, Some("permission".into()), 2);
        assert!(t.program_status().unwrap().is_blocked_with("permission"));
        t.apply(State::parse("thinking"), None, 3);
        assert_eq!(t.state(), Some(&State::Other));
    }

    #[test]
    fn status_log_keeps_the_last_64() {
        let mut log = StatusLog::default();
        for i in 0..70u64 {
            log.push(LogEntry {
                at_ms: i,
                body: vec![b'x'; 2000],
                outcome: "idle".into(),
                turn_after: 0,
            });
        }
        let entries = log.entries();
        assert_eq!(entries.len(), STATUS_LOG_LEN);
        assert_eq!(entries[0].at_ms, 6);
        assert_eq!(entries[63].at_ms, 69);
        assert!(entries.iter().all(|e| e.body.len() == OSC_BODY_MAX));
    }
}
