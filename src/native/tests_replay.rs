//! Issue #76, PR3: a front that starts after its panes did (adoption), silent
//! replay, the `unknown` and `replayed` states, the nudge, and the ordering of
//! the human-input flag. Everything runs in process through the seam: the
//! "front" is a `NativeBackend`, killing it drops the handle while the
//! `LocalHost` and its panes live on, and a new front adopts what is there.

use super::host::local::LocalHost;
use super::host::{Attached, HostPane, KillOutcome, Spawned};
use super::meta::{load, PaneMeta};
use super::replay::replay;
use super::status::TrackerSnapshot;
use super::*;
use crate::testutil::TempDir;
use std::sync::atomic::{AtomicBool, AtomicUsize};

const LONG: Duration = Duration::from_secs(15);

/// Shell `printf` forms of the status reports.
const WORKING: &str = "\\033]7501;state=working\\007";
const IDLE: &str = "\\033]7501;state=idle\\007";

fn osc(state: &str) -> Vec<u8> {
    format!("\x1b]7501;state={state}\x07").into_bytes()
}

// ---------------------------------------------------------------------------
// The recording host and the split fixture
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    /// Bytes that went into the PTY.
    Write(Vec<u8>),
    /// A `meta_set`, with whether its blob has the human-draft flag.
    MetaSet {
        human_draft: bool,
    },
    Resize(u16, u16),
}

/// A [`LocalHost`] (with a tail) that logs the calls that change a pane, in
/// order, and can refuse a write or a `meta_set`.
struct Rec {
    inner: LocalHost,
    calls: Mutex<Vec<Call>>,
    fail_write: AtomicBool,
    fail_meta: AtomicBool,
    /// A pane to end right after the next `list`, as if it exited between the
    /// front's `list` and its table lock.
    end_after_list: Mutex<Option<String>>,
    /// How many `exited` events reached the sink, so a test can wait for one.
    exits_seen: Arc<AtomicUsize>,
}

/// Forwards to the real sink and counts exits.
struct CountExits {
    next: Arc<dyn HostEvents>,
    seen: Arc<AtomicUsize>,
}

impl HostEvents for CountExits {
    fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool {
        self.next.output(id, seq, bytes)
    }
    fn idle(&self, id: &str) {
        self.next.idle(id);
    }
    fn exited(&self, id: &str, code: Option<i64>) {
        self.next.exited(id, code);
        self.seen.fetch_add(1, Ordering::SeqCst);
    }
    fn gap(&self, id: &str, new_start: u64) {
        self.next.gap(id, new_start);
    }
}

impl Rec {
    fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: LocalHost::with_tail(cap),
            calls: Mutex::new(Vec::new()),
            fail_write: AtomicBool::new(false),
            fail_meta: AtomicBool::new(false),
            end_after_list: Mutex::new(None),
            exits_seen: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn log(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn mark(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn writes_since(&self, mark: usize) -> Vec<Vec<u8>> {
        self.calls()[mark..]
            .iter()
            .filter_map(|c| match c {
                Call::Write(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .collect()
    }
}

impl Drop for Rec {
    fn drop(&mut self) {
        self.inner.end_all();
    }
}

impl PtyHost for Rec {
    fn set_sink(&self, sink: Arc<dyn HostEvents>) {
        self.inner.set_sink(Arc::new(CountExits {
            next: sink,
            seen: Arc::clone(&self.exits_seen),
        }));
    }
    fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError> {
        self.inner.spawn(req)
    }
    fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_write.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected write failure"));
        }
        let n = self.inner.try_write(id, bytes)?;
        if n > 0 {
            self.log(Call::Write(bytes[..n].to_vec()));
        }
        Ok(n)
    }
    fn wait_writable(&self, id: &str, timeout: Duration) -> io::Result<bool> {
        self.inner.wait_writable(id, timeout)
    }
    fn resize(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        self.log(Call::Resize(cols, rows));
        self.inner.resize(id, cols, rows)
    }
    fn signal(&self, id: &str, signal: i32) -> io::Result<()> {
        self.inner.signal(id, signal)
    }
    fn kill(&self, id: &str, grace: Duration) -> Option<KillOutcome> {
        self.inner.kill(id, grace)
    }
    fn fg_pgrp(&self, id: &str) -> Option<i32> {
        self.inner.fg_pgrp(id)
    }
    fn list(&self) -> Vec<HostPane> {
        let listed = self.inner.list();
        let ending = self.end_after_list.lock().unwrap().take();
        if let Some(id) = ending {
            let seen = self.exits_seen.load(Ordering::SeqCst);
            self.inner.signal(&id, 9).unwrap();
            // Return only once the exit reached the front's sink, which does
            // not know the pane yet: the event is dropped there.
            eventually("the exit to reach the sink", || {
                (self.exits_seen.load(Ordering::SeqCst) > seen).then_some(())
            });
        }
        listed
    }
    fn forget(&self, id: &str) {
        self.inner.forget(id);
    }
    fn end_all(&self) {
        self.inner.end_all();
    }
    fn attach(&self, id: &str) -> io::Result<Attached> {
        self.inner.attach(id)
    }
    fn meta_set(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        if self.fail_meta.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected meta failure"));
        }
        let human_draft = PaneMeta::from_bytes(bytes).is_ok_and(|m| m.human_draft);
        self.log(Call::MetaSet { human_draft });
        self.inner.meta_set(id, bytes)
    }
    fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
        self.inner.meta_get(id)
    }
    fn survives_restart(&self) -> bool {
        true
    }
}

fn front(host: &Arc<Rec>) -> NativeBackend {
    NativeBackend::with_host_meta(
        NativeConfig::default(),
        Box::new(|key| std::env::var_os(key)),
        Arc::clone(host),
        true,
    )
}

/// A host and the front over it. `kill_front` drops the front and keeps the
/// host and its panes; `new_front` starts another one and adopts.
struct Split {
    host: Arc<Rec>,
    front: Option<NativeBackend>,
}

impl Split {
    fn new(cap: usize) -> Self {
        let host = Rec::new(cap);
        let front = Some(front(&host));
        Self { host, front }
    }

    fn b(&self) -> &NativeBackend {
        self.front.as_ref().expect("a front is running")
    }

    fn kill_front(&mut self) {
        let front = self.front.take().expect("a front is running");
        front.leave_panes_on_drop();
        drop(front);
    }

    fn new_front(&mut self) -> Adoption {
        assert!(self.front.is_none(), "kill the front first");
        let front = front(&self.host);
        let adoption = front.adopt().unwrap();
        self.front = Some(front);
        adoption
    }

    fn run(&self, name: &str, script: &str) -> (TerminalInfo, TempDir) {
        let dir = TempDir::new(name);
        let info = self.b().run(&sh(name, script, &dir)).unwrap();
        (info, dir)
    }
}

fn sh(name: &str, script: &str, dir: &TempDir) -> RunSpec {
    RunSpec {
        argv: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
        name: Some(name.to_string()),
        cwd: Some(dir.path().to_string_lossy().into_owned()),
        ..RunSpec::default()
    }
}

fn eventually<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + LONG;
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn tref(info: &TerminalInfo) -> TerminalRef {
    info.reference()
}

fn screen(b: &NativeBackend, info: &TerminalInfo) -> String {
    b.read(&tref(info), 0).unwrap().text
}

fn wait_screen(b: &NativeBackend, info: &TerminalInfo, needle: &str) {
    eventually(&format!("{needle:?} on the screen"), || {
        screen(b, info).contains(needle).then_some(())
    });
}

fn with_state<R>(b: &NativeBackend, info: &TerminalInfo, f: impl FnOnce(&PaneState) -> R) -> R {
    let pane = b.inner.resolve(&tref(info)).unwrap();
    let st = pane.state.lock().unwrap();
    f(&st)
}

fn is_replayed(b: &NativeBackend, info: &TerminalInfo) -> bool {
    with_state(b, info, |st| st.replayed)
}

fn kind_of<T: std::fmt::Debug>(result: Result<T, BackendError>) -> ErrorKind {
    result.expect_err("the call should be refused").kind
}

/// Wait until the host's tail of the pane is past `n` bytes (so a small tail
/// has wrapped).
fn wait_wrapped(host: &Rec, info: &TerminalInfo) {
    eventually("the tail to wrap", || {
        (host.attach(&info.id).ok()?.tail_start > 0).then_some(())
    });
}

fn default_guard() -> InputGuard {
    InputGuard::default()
}

// ---------------------------------------------------------------------------
// Replay as a function: no processes
// ---------------------------------------------------------------------------

const QUERIES: &[u8] = b"\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[>0q\x1b[?u\x1b]7501;?\x07\x1b]10;?\x07";

fn filler(n: usize) -> Vec<u8> {
    (0..n)
        .flat_map(|i| format!("line {i}\r\n").into_bytes())
        .collect()
}

#[test]
fn replay_answers_no_query_and_makes_no_event() {
    let tail = [
        filler(5),
        QUERIES.to_vec(),
        osc("working"),
        osc("idle"),
        filler(2),
    ]
    .concat();
    // Control: the same bytes fed live do ask for replies.
    let mut live = Emulator::new();
    live.process(&tail);
    assert!(
        live.take_replies().len() > 10,
        "the fixture queries something"
    );

    let mut adopted = replay(&tail, 0, None, 1);
    assert!(adopted.emu.take_replies().is_empty());
    assert!(
        adopted.emu.take_reports().is_empty(),
        "no report is left to log"
    );
    assert_eq!(adopted.tracker.turn(), 1, "the report still counted");
    assert!(!adopted.replayed && !adopted.unknown);
}

#[test]
fn a_query_cut_by_the_wrap_is_not_answered_either() {
    // The tail starts inside `CSI 6 n`; the rest of its line is dropped, and
    // a whole query after the line start is read and not answered.
    let tail = [b"6n half a query\r\n".to_vec(), QUERIES.to_vec(), filler(1)].concat();
    let mut adopted = replay(&tail, 777, None, 1);
    assert!(adopted.emu.take_replies().is_empty());
    assert!(adopted.replayed);
    let text = adopted.emu.screen_text();
    assert!(!text.contains("half a query"), "{text}");
    assert!(text.contains("line 0"), "{text}");
}

#[test]
fn a_wrapped_tail_starts_at_the_first_line_start() {
    // Cut inside a 24-bit colour sequence and a UTF-8 character: nothing of
    // either leaks onto the screen.
    let tail = b"8;2;255;0;0mJUNK \xe2\x82\r\nclean text\r\nmore".to_vec();
    let mut adopted = replay(&tail, 10, None, 1);
    let text = adopted.emu.screen_text();
    assert!(
        text.contains("clean text") && text.contains("more"),
        "{text}"
    );
    assert!(!text.contains("JUNK") && !text.contains("255"), "{text}");
    assert!(adopted.emu.take_replies().is_empty());
}

#[test]
fn a_wrapped_tail_without_a_newline_replays_nothing() {
    let adopted = replay(b"no line end here", 5, None, 1);
    assert!(adopted.replayed && adopted.unknown);
    assert_eq!(adopted.emu.screen_text().trim(), "");
}

fn snapshot_after(reports: &[&str]) -> TrackerSnapshot {
    let mut tracker = Tracker::default();
    for word in reports {
        tracker.apply(State::parse(word), None, 1);
    }
    tracker.snapshot()
}

fn meta_with(snapshot: Option<TrackerSnapshot>, seq: u64) -> PaneMeta {
    PaneMeta {
        tracker: snapshot,
        tracker_seq: seq,
        ..PaneMeta::default()
    }
}

#[test]
fn a_complete_tail_recomputes_the_turn_and_ignores_the_metadata() {
    let tail = [osc("working"), osc("idle"), b"x".to_vec()].concat();
    let end = tail.len() as u64;
    // The metadata claims five turns exactly at the end of the tail.
    let lies = meta_with(
        Some(snapshot_after(&[
            "working", "idle", "working", "idle", "working", "idle", "working", "idle", "working",
            "idle",
        ])),
        end,
    );
    let adopted = replay(&tail, 0, Some(&lies), 1);
    assert_eq!(adopted.tracker.turn(), 1);
    assert!(adopted.notes.iter().any(|n| n.contains("differs")));
    assert!(!adopted.unknown && !adopted.replayed);
}

#[test]
fn a_wrapped_tail_without_a_usable_snapshot_is_unknown() {
    let tail = [b"cut\n".to_vec(), osc("working"), osc("idle")].concat();
    let end = 100 + tail.len() as u64;
    for meta in [
        None,
        Some(meta_with(None, 0)),
        // A snapshot from beyond the end of the tail cannot be right.
        Some(meta_with(
            Some(snapshot_after(&["working", "idle"])),
            end + 1,
        )),
    ] {
        let adopted = replay(&tail, 100, meta.as_ref(), 1);
        assert!(adopted.unknown && adopted.replayed);
        assert_eq!(adopted.tracker.turn(), 0, "reports are not guessed at");
        assert!(!adopted.tracker.reported());
    }
}

#[test]
fn a_wrapped_tail_counts_only_what_follows_the_snapshot() {
    // working idle | (snapshot) | working idle: the snapshot has one turn.
    let first = [b"top\n".to_vec(), osc("working"), osc("idle")].concat();
    let second = [osc("working"), osc("idle")].concat();
    let tail = [first.clone(), second].concat();
    let at = 50 + first.len() as u64;
    let meta = meta_with(Some(snapshot_after(&["working", "idle"])), at);
    let adopted = replay(&tail, 50, Some(&meta), 1);
    assert_eq!(adopted.tracker.turn(), 2, "the second turn counts once");
    assert!(adopted.replayed && !adopted.unknown);

    // A snapshot already at the end of the tail adds nothing.
    let end = 50 + tail.len() as u64;
    let meta = meta_with(
        Some(snapshot_after(&["working", "idle", "working", "idle"])),
        end,
    );
    assert_eq!(replay(&tail, 50, Some(&meta), 1).tracker.turn(), 2);
}

/// One chunk stream with turns, a blocked dialog, reports cut across chunks
/// and filler lines, as the old front would have seen it.
fn recorded_stream() -> Vec<Vec<u8>> {
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    for i in 0..48usize {
        chunks.push(format!("row {i} {}\r\n", "x".repeat(i % 7 * 3)).into_bytes());
        match i % 6 {
            1 => chunks.push(osc("working")),
            3 => {
                // A report cut across two chunks.
                let idle = osc("idle");
                chunks.push(idle[..6].to_vec());
                chunks.push(idle[6..].to_vec());
            }
            4 => chunks.push(osc("blocked:kind=permission")),
            5 => {
                chunks.push([osc("working"), b"after the dialog\r\n".to_vec()].concat());
            }
            _ => {}
        }
    }
    chunks.push(osc("idle"));
    chunks
}

/// What a running front would have written to meta: after every chunk that
/// changed the tracker, its snapshot and the end offset of that chunk.
fn meta_history(chunks: &[Vec<u8>]) -> Vec<(usize, TrackerSnapshot, u64)> {
    let mut emu = Emulator::new();
    let mut tracker = Tracker::default();
    let mut history = Vec::new();
    let mut end = 0u64;
    for (index, chunk) in chunks.iter().enumerate() {
        let before = tracker.snapshot();
        emu.process(chunk);
        let _ = emu.take_replies();
        for (_, report) in emu.take_reports() {
            if let Report::Status { state_word, kind } = report {
                tracker.apply(State::parse(&state_word), kind, 1);
            }
        }
        end += chunk.len() as u64;
        if tracker.snapshot() != before {
            history.push((index, tracker.snapshot(), end));
        }
    }
    history
}

#[test]
fn the_turn_is_never_counted_twice_whatever_chunk_the_front_died_at() {
    let chunks = recorded_stream();
    let stream: Vec<u8> = chunks.concat();
    let truth = replay(&stream, 0, None, 1).tracker.turn();
    assert!(truth >= 8, "the fixture has turns ({truth})");
    let history = meta_history(&chunks);

    let mut exact = 0;
    for tail_start in [0usize, 40, 160, 400] {
        let first_newline = stream[tail_start..].iter().position(|&b| b == b'\n');
        let line_start = first_newline.map_or(stream.len(), |at| tail_start + at + 1) as u64;
        let tail = &stream[tail_start..];
        // The crash chunk: meta holds the last write before it (`lag` of 0),
        // or one or two writes earlier (writes the host never got).
        for crash in 0..=chunks.len() {
            let written: Vec<_> = history
                .iter()
                .filter(|(index, _, _)| *index < crash)
                .collect();
            for lag in 0..=2usize {
                let meta = written
                    .len()
                    .checked_sub(lag + 1)
                    .map(|at| meta_with(Some(written[at].1.clone()), written[at].2));
                let adopted = replay(tail, tail_start as u64, meta.as_ref(), 1);
                let label = format!("tail {tail_start} crash {crash} lag {lag}");
                if tail_start == 0 {
                    assert_eq!(adopted.tracker.turn(), truth, "{label}");
                    continue;
                }
                match meta {
                    None => assert!(adopted.unknown, "{label}"),
                    Some(m) if m.tracker_seq >= line_start => {
                        assert_eq!(adopted.tracker.turn(), truth, "{label}");
                        exact += 1;
                    }
                    Some(m) => {
                        // The snapshot is older than the tail: turns between
                        // may be missing, never added.
                        let at = m.tracker.as_ref().unwrap().turn();
                        assert!(
                            (at..=truth).contains(&adopted.tracker.turn()),
                            "{label}: {} not in {at}..={truth}",
                            adopted.tracker.turn()
                        );
                    }
                }
            }
        }
    }
    assert!(exact > 100, "the sweep covered the wrapped cases ({exact})");
}

#[test]
fn a_snapshot_taken_inside_a_turn_finishes_it_once() {
    // The snapshot says working; the idle that ends the turn comes after it.
    let tail = [
        b"x\n".to_vec(),
        osc("working"),
        b"work\r\n".to_vec(),
        osc("idle"),
    ]
    .concat();
    let at = 20 + 2 + osc("working").len() as u64;
    let meta = meta_with(Some(snapshot_after(&["working"])), at);
    let adopted = replay(&tail, 20, Some(&meta), 1);
    assert_eq!(adopted.tracker.turn(), 1);
}

#[test]
fn a_blocked_report_counted_before_the_crash_is_not_counted_again() {
    let tail = [
        b"x\n".to_vec(),
        osc("blocked:kind=permission"),
        b"more\r\n".to_vec(),
    ]
    .concat();
    let snapshot = {
        let mut t = Tracker::default();
        t.apply(State::parse("blocked"), Some("permission".to_string()), 1);
        t.snapshot()
    };
    let at = 20 + 2 + osc("blocked:kind=permission").len() as u64;
    let meta = meta_with(Some(snapshot), at);
    assert_eq!(replay(&tail, 20, Some(&meta), 1).tracker.turn(), 1);
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

#[test]
fn replayed_refuses_in_the_guard_and_names_itself() {
    let mut tracker = Tracker::default();
    tracker.apply(State::Idle, None, 1);
    let mut st = PaneState::fresh(Emulator::new(), tracker);
    assert!(check_guard(&st, &default_guard(), "n:1").is_ok());
    st.replayed = true;
    let err = check_guard(&st, &default_guard(), "n:1").unwrap_err();
    assert_eq!(err.kind, ErrorKind::Replayed);
    assert_eq!(err.code(), "pane_replayed");
    // A finished pane is finished, not replayed.
    st.live = false;
    assert_eq!(
        check_guard(&st, &default_guard(), "n:1").unwrap_err().kind,
        ErrorKind::Finished
    );
}

#[test]
fn an_unknown_status_counts_as_working_for_the_guard() {
    let mut tracker = Tracker::default();
    tracker.apply(State::Idle, None, 1);
    let mut st = PaneState::fresh(Emulator::new(), tracker);
    st.unknown = true;
    let when = |state: State| InputGuard {
        if_state: Some(state),
        ..InputGuard::default()
    };
    assert!(check_guard(&st, &when(State::Working), "n:1").is_ok());
    assert_eq!(
        check_guard(&st, &when(State::Idle), "n:1")
            .unwrap_err()
            .kind,
        ErrorKind::StatusMismatch
    );
    let no_blocked = InputGuard {
        refuse_when_blocked: true,
        ..InputGuard::default()
    };
    assert!(check_guard(&st, &no_blocked, "n:1").is_ok());
}

// ---------------------------------------------------------------------------
// Adoption through the seam
// ---------------------------------------------------------------------------

#[test]
fn adopting_a_complete_tail_gives_the_pane_back_live_and_event_free() {
    let mut s = Split::new(1 << 20);
    let script = format!("printf '{IDLE}hello\\n'; printf '{WORKING}'; printf '{IDLE}'; cat");
    let (info, _dir) = s.run("p", &script);
    eventually("the turn", || {
        (s.b().get(&tref(&info)).unwrap().turn == Some(1)).then_some(())
    });
    s.kill_front();
    let adoption = s.new_front();
    assert_eq!(adoption.adopted, vec![info.id.clone()]);
    assert!(adoption.replayed.is_empty(), "a complete tail is trusted");

    let now = s.b().get(&tref(&info)).unwrap();
    assert!(now.live && now.reported);
    assert_eq!(now.turn, Some(1));
    assert!(state_is_idle(&now));
    wait_screen(s.b(), &now, "hello");
    // Replay made no event and no pending send.
    assert!(s.b().status_log(&tref(&info)).unwrap().is_empty());
    with_state(s.b(), &info, |st| {
        assert!(st.pending_send.is_none() && st.pending_replies.is_empty());
        assert!(!st.unknown && !st.replayed);
    });
    // And the pane works: a guarded key is accepted, a human's line arrives.
    s.b()
        .keys(&tref(&info), &[Key::Enter], &default_guard())
        .unwrap();
    s.b().human_input(&tref(&info), b"ping\n").unwrap();
    wait_screen(s.b(), &now, "ping");
    // Adopting again picks up nothing new.
    assert_eq!(s.b().adopt().unwrap(), Adoption::default());
}

#[test]
fn a_pane_that_exits_between_the_list_and_the_adoption_is_adopted_exited() {
    let mut s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", &format!("printf '{IDLE}hello\\n'; cat"));
    wait_screen(s.b(), &info, "hello");
    s.kill_front();
    *s.host.end_after_list.lock().unwrap() = Some(info.id.clone());
    let adoption = s.new_front();
    assert_eq!(adoption.adopted, vec![info.id.clone()]);
    let now = s.b().get(&tref(&info)).unwrap();
    assert!(!now.live, "the exit that came before the table was lost");
    with_state(s.b(), &info, |st| assert!(st.exit_code.is_some()));
    // And a waiter is told, instead of waiting on a dead PTY.
    assert_eq!(
        kind_of(s.b().keys(&tref(&info), &[Key::Enter], &default_guard())),
        ErrorKind::Finished
    );
}

fn state_is_idle(info: &TerminalInfo) -> bool {
    info.status.as_ref().is_some_and(|s| s.state == State::Idle)
}

#[test]
fn a_wrapped_tail_adopts_as_replayed_and_refuses_every_guarded_write() {
    let mut s = Split::new(4096);
    let script = "yes 0123456789 | head -c 20000; echo READY; cat > received.txt";
    let (info, dir) = s.run("p", script);
    wait_screen(s.b(), &info, "READY");
    wait_wrapped(&s.host, &info);
    s.kill_front();
    let adoption = s.new_front();
    assert_eq!(adoption.replayed, vec![info.id.clone()]);
    assert!(is_replayed(s.b(), &info));

    let t = tref(&info);
    assert_eq!(
        kind_of(s.b().keys(&t, &[Key::Enter], &default_guard())),
        ErrorKind::Replayed
    );
    assert_eq!(
        kind_of(s.b().send(&t, "hi", &default_guard())),
        ErrorKind::Replayed
    );
    let check = |_: &crate::backend::StyledScreen| Ok(());
    assert_eq!(
        kind_of(
            s.b()
                .keys_if_screen(&t, &[Key::Enter], &default_guard(), &check)
        ),
        ErrorKind::Replayed
    );
    // Reading is fine, and the idle program does not repaint on its own.
    assert!(screen(s.b(), &info).contains("0123456789"));
    thread::sleep(Duration::from_millis(1200));
    assert!(is_replayed(s.b(), &info), "an idle pane stays replayed");
    assert_eq!(
        kind_of(s.b().keys(&t, &[Key::Enter], &default_guard())),
        ErrorKind::Replayed
    );

    // A human line gets through (not guarded) and proves what the program
    // received: only that line, nothing from the refused calls.
    s.b().human_input(&t, b"x\n").unwrap();
    let received = dir.path().join("received.txt");
    eventually("the line", || {
        (std::fs::read(&received).ok()? == b"x\n").then_some(())
    });
    // The echo is output after the nudge: the pane is released.
    eventually("the release", || (!is_replayed(s.b(), &info)).then_some(()));
    s.b().keys(&t, &[Key::Enter], &default_guard()).unwrap();
    eventually("the second line", || {
        (std::fs::read(&received).ok()? == b"x\n\n").then_some(())
    });
}

#[test]
fn a_wrapped_pane_without_a_snapshot_is_unknown_and_clears_on_a_fresh_report() {
    let s_cap = 2048;
    let mut s = Split::new(s_cap);
    // No report before the wrap, so meta has no tracker snapshot.
    let script = format!(
        "yes 0123456789 | head -c 20000; echo READY; read go; printf '{IDLE}'; cat > /dev/null"
    );
    let (info, _dir) = s.run("p", &script);
    wait_screen(s.b(), &info, "READY");
    s.kill_front();
    s.new_front();
    with_state(s.b(), &info, |st| {
        assert!(st.unknown && st.replayed);
        assert!(!st.tracker.reported());
    });
    assert!(
        !s.b()
            .wait(&tref(&info), Duration::from_millis(300))
            .unwrap()
            .ended
    );
    // The program's own report is a fresh event.
    s.b().human_input(&tref(&info), b"go\n").unwrap();
    eventually("a fresh report", || {
        with_state(s.b(), &info, |st| !st.unknown).then_some(())
    });
    assert!(
        s.b()
            .wait(&tref(&info), Duration::from_secs(5))
            .unwrap()
            .ended
    );
}

#[test]
fn a_dropped_chunk_makes_the_pane_unknown_and_replayed_and_wait_keeps_waiting() {
    let s = Split::new(1 << 20);
    let script = format!("printf '{IDLE}'; read go; printf '{IDLE}'; cat > /dev/null");
    let (info, _dir) = s.run("p", &script);
    eventually("idle", || {
        state_is_idle(&s.b().get(&tref(&info)).unwrap()).then_some(())
    });
    assert!(
        s.b()
            .wait(&tref(&info), Duration::from_secs(5))
            .unwrap()
            .ended
    );

    let events = FrontEvents {
        table: Arc::clone(&s.b().inner.table),
        epoch: u64::MAX,
    };
    events.gap(&info.id, 12345);
    with_state(s.b(), &info, |st| assert!(st.replayed && st.unknown));
    let waited = s
        .b()
        .wait(&tref(&info), Duration::from_millis(300))
        .unwrap();
    assert!(!waited.ended, "a stale idle must not end the wait");
    assert_eq!(
        kind_of(s.b().keys(&tref(&info), &[Key::Enter], &default_guard())),
        ErrorKind::Replayed
    );
    // A fresh report ends the not-knowing; only a repaint after a nudge ends
    // the replayed state.
    s.b().human_input(&tref(&info), b"go\n").unwrap();
    eventually("a fresh report", || {
        with_state(s.b(), &info, |st| !st.unknown).then_some(())
    });
    assert!(is_replayed(s.b(), &info));
    assert!(
        s.b()
            .wait(&tref(&info), Duration::from_secs(5))
            .unwrap()
            .ended
    );
}

#[test]
fn replaying_queries_writes_nothing_and_a_live_query_is_still_answered() {
    for (name, cap, prefix) in [
        ("complete", 1usize << 20, ""),
        ("wrapped", 4096, "yes 0123456789 | head -c 20000; "),
    ] {
        let mut s = Split::new(cap);
        let script = format!(
            "{prefix}printf '\\033[c\\033[>c\\033[5n\\033[6n\\033[>0q\\033[?u\\033]10;?\\007'; \
             printf '{IDLE}READY\\n'; read go; printf '\\033[5n'; cat > /dev/null"
        );
        let (info, _dir) = s.run(name, &script);
        wait_screen(s.b(), &info, "READY");
        // Control: live, the front answered (writes happened before the kill).
        eventually("the live replies", || {
            s.host
                .writes_since(0)
                .iter()
                .any(|w| w.starts_with(b"\x1b["))
                .then_some(())
        });
        s.kill_front();
        let mark = s.host.mark();
        let adoption = s.new_front();
        assert_eq!(adoption.adopted.len(), 1, "{name}");
        // After adoption, and a moment later: no write went into the PTY.
        thread::sleep(Duration::from_millis(300));
        assert!(
            s.host.writes_since(mark).is_empty(),
            "{name}: {:?}",
            s.host.calls()
        );
        with_state(s.b(), &info, |st| {
            assert!(
                st.pending_replies.is_empty() && !st.replies_overflowed,
                "{name}"
            );
            assert!(
                st.log.entries().is_empty() && st.pending_send.is_none(),
                "{name}"
            );
        });
        // A query that arrives after adoption is answered as always.
        let after = s.host.mark();
        s.b().human_input(&tref(&info), b"go\n").unwrap();
        eventually("the live answer", || {
            s.host
                .writes_since(after)
                .iter()
                .any(|w| w == b"\x1b[0n")
                .then_some(())
        });
    }
}

#[test]
fn the_nudge_shrinks_and_restores_the_terminal_and_a_repaint_releases_the_pane() {
    let mut s = Split::new(4096);
    let script = "yes 0123456789 | head -c 20000; n=0; \
         trap 'n=$((n+1)); if [ $n -ge 2 ]; then printf REPAINT-$n; fi' WINCH; \
         echo READY; while :; do sleep 0.02; done";
    let (info, _dir) = s.run("p", script);
    wait_screen(s.b(), &info, "READY");
    wait_wrapped(&s.host, &info);
    s.kill_front();
    let mark = s.host.mark();
    s.new_front();
    let resizes: Vec<Call> = s.host.calls()[mark..]
        .iter()
        .filter(|c| matches!(c, Call::Resize(..)))
        .cloned()
        .collect();
    assert_eq!(
        resizes,
        vec![Call::Resize(COLS, ROWS - 1), Call::Resize(COLS, ROWS)]
    );
    // The program painted after the return resize: released on its own.
    eventually("the release", || (!is_replayed(s.b(), &info)).then_some(()));
    wait_screen(s.b(), &info, "REPAINT-2");
    s.b()
        .keys(&tref(&info), &[Key::Enter], &default_guard())
        .unwrap();
    let listed = s.host.list();
    assert_eq!((listed[0].cols, listed[0].rows), (COLS, ROWS));
}

#[test]
fn a_repaint_that_answers_the_smaller_size_does_not_release_the_pane() {
    let mut s = Split::new(4096);
    // Paints once, on the first resize (the one-row-smaller one).
    let script = "yes 0123456789 | head -c 20000; \
         trap 'printf PAINT-SMALL; trap - WINCH' WINCH; \
         echo READY; while :; do sleep 0.02; done";
    let (info, _dir) = s.run("p", script);
    wait_screen(s.b(), &info, "READY");
    wait_wrapped(&s.host, &info);
    s.kill_front();
    s.new_front();
    wait_screen(s.b(), &info, "PAINT-SMALL");
    thread::sleep(Duration::from_millis(800));
    assert!(
        is_replayed(s.b(), &info),
        "the 119-row paint must not release"
    );
    assert_eq!(
        kind_of(s.b().keys(&tref(&info), &[Key::Enter], &default_guard())),
        ErrorKind::Replayed
    );
}

#[test]
fn a_pane_listed_with_another_size_adopts_as_replayed() {
    let mut s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "echo READY; cat > /dev/null");
    wait_screen(s.b(), &info, "READY");
    s.host.resize(&info.id, 100, 30).unwrap();
    s.kill_front();
    let adoption = s.new_front();
    assert_eq!(
        adoption.replayed,
        vec![info.id.clone()],
        "the tail is complete"
    );
    // The nudge ends with the constant size.
    let listed = s.host.list();
    assert_eq!((listed[0].cols, listed[0].rows), (COLS, ROWS));
}

#[test]
fn a_pane_that_exits_while_no_front_runs_is_adopted_finished() {
    let mut s = Split::new(1 << 20);
    let script = format!("printf '{IDLE}done\\n'; sleep 0.4; exit 7");
    let (info, _dir) = s.run("p", &script);
    wait_screen(s.b(), &info, "done");
    s.kill_front();
    eventually("the exit", || {
        s.host.list().iter().all(|p| p.finished).then_some(())
    });
    let adoption = s.new_front();
    assert_eq!(adoption.adopted, vec![info.id.clone()]);
    assert!(
        adoption.replayed.is_empty(),
        "nothing to nudge in a finished pane"
    );
    let now = s.b().get(&tref(&info)).unwrap();
    assert!(!now.live);
    assert_eq!(now.exit_code, Some(7));
    assert!(now.status.is_none());
    assert!(
        screen(s.b(), &now).contains("done"),
        "the final output is replayed"
    );
    assert_eq!(s.b().list(true).unwrap().terminals.len(), 1);
    let ended = s.b().wait(&tref(&info), Duration::from_secs(1)).unwrap();
    assert!(ended.ended && ended.exit_code == Some(7));
    // It stays until removed; removing forgets it in the host too.
    let removed = s.b().remove(&tref(&info), None).unwrap();
    assert!(!removed.was_live);
    assert!(s.host.list().is_empty());
}

#[test]
fn output_that_races_the_adoption_is_neither_lost_nor_counted_twice() {
    for (name, cap) in [("complete", 1usize << 20), ("wrapped", 2048)] {
        let mut s = Split::new(cap);
        let script = format!(
            "i=0; while [ $i -lt 200 ]; do printf '{WORKING}{IDLE}row %s\\n' $i; \
             i=$((i+1)); sleep 0.004; done; echo END; cat > /dev/null"
        );
        let (info, _dir) = s.run(name, &script);
        eventually("some turns", || {
            (s.b().get(&tref(&info)).unwrap().turn? >= 60).then_some(())
        });
        s.kill_front();
        let adoption = s.new_front();
        wait_screen(s.b(), &info, "END");
        let now = s.b().get(&tref(&info)).unwrap();
        assert_eq!(now.turn, Some(200), "{name}: every turn once");
        assert_eq!(
            adoption.replayed.len(),
            usize::from(name == "wrapped"),
            "{name}"
        );
    }
}

#[test]
fn bytes_the_host_delivers_again_after_the_replay_are_not_fed_twice() {
    let mut s = Split::new(1 << 20);
    let script = format!("printf '{WORKING}{IDLE}once\\n'; cat > /dev/null");
    let (info, _dir) = s.run("p", &script);
    eventually("the turn", || {
        (s.b().get(&tref(&info)).unwrap().turn == Some(1)).then_some(())
    });
    s.kill_front();
    s.new_front();
    let tail = s.host.attach(&info.id).unwrap();
    let events = FrontEvents {
        table: Arc::clone(&s.b().inner.table),
        epoch: u64::MAX - 1,
    };
    // The whole replayed range again, as a chunk that raced the attach.
    assert!(events.output(&info.id, 0, &tail.bytes));
    assert_eq!(s.b().get(&tref(&info)).unwrap().turn, Some(1));
    // A chunk that overlaps the end is trimmed to what is new.
    let more = [tail.bytes.clone(), osc("working"), osc("idle")].concat();
    assert!(events.output(&info.id, 0, &more));
    assert_eq!(s.b().get(&tref(&info)).unwrap().turn, Some(2));
}

#[test]
fn a_front_whose_meta_writes_never_landed_still_replays_a_complete_tail() {
    let mut s = Split::new(1 << 20);
    s.host.fail_meta.store(true, Ordering::SeqCst);
    let script =
        format!("printf '{WORKING}{IDLE}'; sleep 1; printf '{WORKING}{IDLE}'; cat > /dev/null");
    let (info, _dir) = s.run("p", &script);
    eventually("turn 2", || {
        (s.b().get(&tref(&info)).unwrap().turn == Some(2)).then_some(())
    });
    // The crash came after the ingest and before every meta_set.
    s.kill_front();
    assert!(load(&*s.host, &info.id).is_none());
    s.host.fail_meta.store(false, Ordering::SeqCst);
    s.new_front();
    assert_eq!(
        s.b().get(&tref(&info)).unwrap().turn,
        Some(2),
        "counted once"
    );
    // Adoption left the snapshot exact at the end of the tail.
    let meta = load(&*s.host, &info.id).unwrap();
    assert_eq!(meta.tracker.unwrap().turn(), 2);
    assert_eq!(meta.tracker_seq, s.host.attach(&info.id).unwrap().tail_end);
}

#[test]
fn unusable_meta_adopts_the_pane_without_a_restore() {
    for bad in [b"not json".to_vec(), br#"{"v": 99}"#.to_vec()] {
        let mut s = Split::new(2048);
        let script = format!(
            "printf '{WORKING}{IDLE}'; yes 0123456789 | head -c 20000; echo READY; cat > /dev/null"
        );
        let (info, _dir) = s.run("p", &script);
        wait_screen(s.b(), &info, "READY");
        wait_wrapped(&s.host, &info);
        s.kill_front();
        s.host.inner.meta_set(&info.id, &bad).unwrap();
        s.new_front();
        with_state(s.b(), &info, |st| assert!(st.unknown && st.replayed));
    }
}

#[test]
fn adopting_from_a_host_without_a_tail_is_refused() {
    let host = Arc::new(LocalHost::new());
    let make = || {
        NativeBackend::with_host_meta(
            NativeConfig::default(),
            Box::new(|key| std::env::var_os(key)),
            Arc::clone(&host),
            true,
        )
    };
    let first = make();
    let dir = TempDir::new("no-tail");
    first.run(&sh("p", "sleep 30", &dir)).unwrap();
    first.leave_panes_on_drop();
    drop(first);
    let second = make();
    assert!(matches!(kind_of(second.adopt()), ErrorKind::Unsupported(_)));
}

// ---------------------------------------------------------------------------
// The human-input ordering
// ---------------------------------------------------------------------------

#[test]
fn the_draft_flag_is_in_the_host_before_the_human_byte_is_written() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let mark = s.host.mark();
    s.b().human_input(&tref(&info), b"x\n").unwrap();
    let calls = s.host.calls();
    let flag = calls[mark..]
        .iter()
        .position(|c| *c == Call::MetaSet { human_draft: true })
        .expect("the flag was set");
    let write = calls[mark..]
        .iter()
        .position(|c| *c == Call::Write(b"x\n".to_vec()))
        .expect("the byte was written");
    assert!(flag < write, "{calls:?}");
    // The flag is in the host's copy of the meta.
    assert!(load(&*s.host, &info.id).unwrap().human_draft);
}

#[test]
fn a_front_that_dies_between_the_flag_and_the_byte_leaves_the_flag_and_no_byte() {
    let mut s = Split::new(1 << 20);
    let (info, dir) = s.run("p", "echo READY; cat > received.txt");
    wait_screen(s.b(), &info, "READY");
    s.host.fail_write.store(true, Ordering::SeqCst);
    let mark = s.host.mark();
    assert!(s.b().human_input(&tref(&info), b"x\n").is_err());
    assert!(s.host.writes_since(mark).is_empty());
    s.kill_front();
    s.host.fail_write.store(false, Ordering::SeqCst);
    // After adoption the meta has the flag, and the program got nothing.
    assert!(load(&*s.host, &info.id).unwrap().human_draft);
    s.new_front();
    s.b().human_input(&tref(&info), b"y\n").unwrap();
    eventually("the later line", || {
        (std::fs::read(dir.path().join("received.txt")).ok()? == b"y\n").then_some(())
    });
}

#[test]
fn a_flag_the_host_does_not_acknowledge_means_no_byte() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    s.host.fail_meta.store(true, Ordering::SeqCst);
    let mark = s.host.mark();
    assert!(s.b().human_input(&tref(&info), b"x\n").is_err());
    assert!(s.host.writes_since(mark).is_empty());
    s.host.fail_meta.store(false, Ordering::SeqCst);
    // The flag stayed in memory for the next write, which still goes first.
    s.b().human_input(&tref(&info), b"x\n").unwrap();
    assert!(load(&*s.host, &info.id).unwrap().human_draft);
}

#[test]
fn human_input_needs_a_front_that_keeps_meta() {
    let b = NativeBackend::new(NativeConfig::default());
    let dir = TempDir::new("no-meta");
    let info = b.run(&sh("p", "cat > /dev/null", &dir)).unwrap();
    assert!(matches!(
        kind_of(b.human_input(&tref(&info), b"x")),
        ErrorKind::Unsupported(_)
    ));
    b.shutdown();
}

/// Measurement, not a gate: `cargo test replay_time -- --ignored --nocapture`.
#[test]
#[ignore = "measurement"]
fn replay_time_of_a_full_tail() {
    let mut tail = Vec::new();
    let mut i = 0;
    while tail.len() < 1024 * 1024 {
        tail.extend_from_slice(
            format!("\x1b[38;2;10;20;30mline {i}\x1b[0m some text\r\n").as_bytes(),
        );
        if i % 50 == 0 {
            tail.extend_from_slice(&osc("working"));
            tail.extend_from_slice(&osc("idle"));
        }
        i += 1;
    }
    let started = Instant::now();
    let adopted = replay(&tail, 5, None, 1);
    println!(
        "replay of {} bytes took {:?} (unknown={})",
        tail.len(),
        started.elapsed(),
        adopted.unknown
    );
}
