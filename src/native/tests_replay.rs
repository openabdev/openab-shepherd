//! Issue #76, PR3: a front that starts after its panes did (adoption), silent
//! replay, the `unknown` and `replayed` states, the nudge, and the ordering of
//! the human-input flag. Everything runs in process through the seam: the
//! "front" is a `NativeBackend`, killing it drops the handle while the
//! `LocalHost` and its panes live on, and a new front adopts what is there.
//!
//! The file is compiled twice (see `mod.rs`): once with the front calling the
//! `LocalHost` directly, and once as `tests_replay_held`, where each front
//! reaches the same host through a holder over a socketpair link.

use super::host::held::Held;
use super::host::local::LocalHost;
use super::host::{Attached, HostPane, KillOutcome, Spawned};
use super::human::LeaseStatus;
use super::meta::{load, PaneMeta};
use super::replay::replay;
use super::status::TrackerSnapshot;
use super::*;
use crate::testutil::TempDir;
use std::sync::atomic::{AtomicBool, AtomicUsize};

const LONG: Duration = Duration::from_secs(15);

/// Whether this copy of the file runs the fronts over a holder link.
fn held() -> bool {
    super::is_held(module_path!())
}

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
    /// The pane was killed (remove).
    Kill,
}

/// A [`LocalHost`] (with a tail) that logs the calls that change a pane, in
/// order, and can refuse a write or a `meta_set`.
struct Rec {
    /// What the calls reach: the front's client while one is connected over a
    /// link, otherwise the host itself.
    inner: Mutex<Arc<dyn PtyHost>>,
    /// The holder, when the fronts reach the host over a link.
    rig: Option<Arc<Held>>,
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
        let (inner, rig) = if held() {
            let rig = Held::with_tail(cap);
            (rig.connect() as Arc<dyn PtyHost>, Some(rig))
        } else {
            (
                Arc::new(LocalHost::with_tail(cap)) as Arc<dyn PtyHost>,
                None,
            )
        };
        Arc::new(Self {
            inner: Mutex::new(inner),
            rig,
            calls: Mutex::new(Vec::new()),
            fail_write: AtomicBool::new(false),
            fail_meta: AtomicBool::new(false),
            end_after_list: Mutex::new(None),
            exits_seen: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn inner(&self) -> Arc<dyn PtyHost> {
        Arc::clone(&self.inner.lock().unwrap())
    }

    /// A new front's link (a no-op without a holder).
    fn connect(&self) {
        if let Some(rig) = &self.rig {
            *self.inner.lock().unwrap() = rig.connect();
        }
    }

    /// The front is gone: its link closes, the host stays and is reached
    /// directly until the next front.
    fn disconnect(&self) {
        if let Some(rig) = &self.rig {
            *self.inner.lock().unwrap() = Arc::clone(&rig.local) as Arc<dyn PtyHost>;
            // Whatever the dead front had sent is handled before a test acts
            // on the host directly.
            assert!(rig.links_ended(LONG), "the old front's link never ended");
        }
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
        self.inner().end_all();
    }
}

impl PtyHost for Rec {
    fn set_sink(&self, sink: Arc<dyn HostEvents>) {
        self.inner().set_sink(Arc::new(CountExits {
            next: sink,
            seen: Arc::clone(&self.exits_seen),
        }));
    }
    fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError> {
        self.inner().spawn(req)
    }
    fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_write.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected write failure"));
        }
        let n = self.inner().try_write(id, bytes)?;
        if n > 0 {
            self.log(Call::Write(bytes[..n].to_vec()));
        }
        Ok(n)
    }
    fn wait_writable(&self, id: &str, timeout: Duration) -> io::Result<bool> {
        self.inner().wait_writable(id, timeout)
    }
    fn resize(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
        self.log(Call::Resize(cols, rows));
        self.inner().resize(id, cols, rows)
    }
    fn signal(&self, id: &str, signal: i32) -> io::Result<()> {
        self.inner().signal(id, signal)
    }
    fn kill(&self, id: &str, grace: Duration) -> Option<KillOutcome> {
        self.log(Call::Kill);
        self.inner().kill(id, grace)
    }
    fn fg_pgrp(&self, id: &str) -> Option<i32> {
        self.inner().fg_pgrp(id)
    }
    fn list(&self) -> Vec<HostPane> {
        let listed = self.inner().list();
        let ending = self.end_after_list.lock().unwrap().take();
        if let Some(id) = ending {
            let seen = self.exits_seen.load(Ordering::SeqCst);
            self.inner().signal(&id, 9).unwrap();
            // Return only once the exit reached the front's sink, which does
            // not know the pane yet: the event is dropped there.
            eventually("the exit to reach the sink", || {
                (self.exits_seen.load(Ordering::SeqCst) > seen).then_some(())
            });
        }
        listed
    }
    fn forget(&self, id: &str) {
        self.inner().forget(id);
    }
    fn end_all(&self) {
        self.inner().end_all();
    }
    fn attach(&self, id: &str) -> io::Result<Attached> {
        self.inner().attach(id)
    }
    fn meta_set(&self, id: &str, bytes: &[u8]) -> io::Result<()> {
        if self.fail_meta.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected meta failure"));
        }
        let human_draft = PaneMeta::from_bytes(bytes).is_ok_and(|m| m.human_draft);
        self.log(Call::MetaSet { human_draft });
        self.inner().meta_set(id, bytes)
    }
    fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
        self.inner().meta_get(id)
    }
    fn survives_restart(&self) -> bool {
        true
    }
}

fn front_with(host: &Arc<Rec>, config: NativeConfig) -> NativeBackend {
    NativeBackend::with_host_meta(
        config,
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
    /// What every front of this split is built with.
    config: NativeConfig,
}

impl Split {
    fn new(cap: usize) -> Self {
        Self::with_config(cap, NativeConfig::default())
    }

    fn with_config(cap: usize, config: NativeConfig) -> Self {
        let host = Rec::new(cap);
        let front = Some(front_with(&host, config.clone()));
        Self {
            host,
            front,
            config,
        }
    }

    fn b(&self) -> &NativeBackend {
        self.front.as_ref().expect("a front is running")
    }

    fn kill_front(&mut self) {
        let front = self.front.take().expect("a front is running");
        front.leave_panes_on_drop();
        drop(front);
        self.host.disconnect();
    }

    fn new_front(&mut self) -> Adoption {
        assert!(self.front.is_none(), "kill the front first");
        self.host.connect();
        let front = front_with(&self.host, self.config.clone());
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

/// A person's line into a pane, for the tests that need the pane to receive
/// one: take the lease, write, let the lease go. The draft stays, as it does
/// for a person who walks away (a line ending in LF is not an Enter). Issue
/// #77 replaced the lease-less `human_input` with `human_write`, which needs a
/// lease; every call site below goes through this one helper.
trait HumanInput {
    fn human_input(&self, t: &TerminalRef, bytes: &[u8]) -> Result<(), BackendError>;
}

impl HumanInput for NativeBackend {
    fn human_input(&self, t: &TerminalRef, bytes: &[u8]) -> Result<(), BackendError> {
        let grant = self.lease_acquire(t, "test-human", false)?;
        let written = self.human_write(t, grant.id, bytes);
        self.lease_release(t, grant.id)?;
        written
    }
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
    // The line is a draft (LF is not an Enter), and a bot's write waits for
    // the person; this test is about the replayed guard, so the draft goes.
    s.b().draft_clear(&t).unwrap();
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
        s.host.inner().meta_set(&info.id, &bad).unwrap();
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
fn the_draft_works_in_memory_on_a_front_that_keeps_no_meta() {
    // Issue #77: the bare command keeps no pane metadata. `human_input` used
    // to be refused there; the draft is now held in memory and a bot is
    // refused all the same.
    let b = NativeBackend::new(NativeConfig::default());
    let dir = TempDir::new("no-meta");
    let info = b.run(&sh("p", "cat > /dev/null", &dir)).unwrap();
    b.human_input(&tref(&info), b"x").unwrap();
    assert!(b.human_state(&tref(&info)).unwrap().draft);
    assert_eq!(
        kind_of(b.send(&tref(&info), "hi", &default_guard())),
        ErrorKind::DraftPending
    );
    b.draft_clear(&tref(&info)).unwrap();
    b.send(&tref(&info), "hi", &default_guard()).unwrap();
    b.shutdown();
}

// ---------------------------------------------------------------------------
// Issue #77: the input lease and the draft
// ---------------------------------------------------------------------------

/// A lease clock a test moves by hand.
#[derive(Clone)]
struct TestClock {
    base: Instant,
    offset: Arc<Mutex<Duration>>,
}

impl TestClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    fn config(&self) -> NativeConfig {
        let clock = self.clone();
        NativeConfig {
            lease_clock: LeaseClock::new(move || clock.base + *clock.offset.lock().unwrap()),
            ..NativeConfig::default()
        }
    }

    fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap() += by;
    }
}

fn is_human_controlled(kind: &ErrorKind) -> bool {
    matches!(kind, ErrorKind::HumanControlled { .. })
}

fn draft_of(b: &NativeBackend, t: &TerminalRef) -> bool {
    b.human_state(t).unwrap().draft
}

fn lease_of(b: &NativeBackend, t: &TerminalRef) -> bool {
    b.human_state(t).unwrap().lease.is_some()
}

/// Acquire, write, and keep the lease; returns its id.
fn typing(b: &NativeBackend, t: &TerminalRef, bytes: &[u8]) -> u64 {
    let grant = b.lease_acquire(t, "person", false).unwrap();
    b.human_write(t, grant.id, bytes).unwrap();
    grant.id
}

#[test]
fn a_lease_acquire_never_lands_between_a_bot_paste_and_its_enter() {
    // Three panes at once, 67 rounds each: 201 acquires that race a bot's
    // paste, its settle and its Enter. Few panes, because a macOS shell's
    // default limit is 256 open files and every pane and link takes some.
    let workers: Vec<_> = (0..3)
        .map(|n| {
            thread::spawn(move || {
                let s = Split::new(1 << 20);
                let (info, _dir) = s.run(&format!("p{n}"), "cat > /dev/null");
                let t = tref(&info);
                for round in 0..67 {
                    let mark = s.host.mark();
                    let (b, t2) = (s.b().clone(), t.clone());
                    let sender = thread::spawn(move || {
                        b.send(&t2, "hello", &default_guard()).unwrap();
                    });
                    eventually("the paste", || {
                        s.host
                            .writes_since(mark)
                            .iter()
                            .any(|w| w == b"hello")
                            .then_some(())
                    });
                    let grant = s.b().lease_acquire(&t, "person", false).unwrap();
                    // The acquire returned only after the bot's Enter.
                    assert_eq!(
                        s.host.writes_since(mark),
                        [b"hello".to_vec(), b"\r".to_vec()],
                        "pane {n} round {round}: the acquire landed inside the send"
                    );
                    s.b().human_write(&t, grant.id, b"X").unwrap();
                    sender.join().unwrap();
                    assert_eq!(
                        s.host.writes_since(mark),
                        [b"hello".to_vec(), b"\r".to_vec(), b"X".to_vec()],
                        "pane {n} round {round}"
                    );
                    s.b().lease_release(&t, grant.id).unwrap();
                    s.b().draft_clear(&t).unwrap();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn what_the_program_receives_is_the_paste_the_enter_and_then_the_person() {
    let s = Split::new(1 << 20);
    let (info, dir) = s.run("p", "cat > received.txt");
    let t = tref(&info);
    let (b, t2) = (s.b().clone(), t.clone());
    let mark = s.host.mark();
    let sender = thread::spawn(move || {
        b.send(&t2, "hello", &default_guard()).unwrap();
    });
    eventually("the paste", || {
        s.host
            .writes_since(mark)
            .iter()
            .any(|w| w == b"hello")
            .then_some(())
    });
    let grant = s.b().lease_acquire(&t, "person", false).unwrap();
    s.b().human_write(&t, grant.id, b"X\r").unwrap();
    sender.join().unwrap();
    eventually("both lines, in order", || {
        (std::fs::read(dir.path().join("received.txt")).ok()? == b"hello\nX\n").then_some(())
    });
    assert!(!draft_of(s.b(), &t), "the person pressed Enter");
}

#[test]
fn a_bot_is_refused_exactly_while_a_person_is_at_the_pane() {
    // A model of the lease and the draft, against random operations from a
    // fixed seed. Bot writes only, so no settle: 300 rounds are quick.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rand = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    let b = s.b();
    // The bytes a person types, and whether the draft is left set.
    let typed: [(&[u8], bool); 7] = [
        (b"x", true),
        (b"\r", false),
        (b"x\r", false),
        (b"\r x", true),
        (b"\n", true),
        (b"\x1b[13u", false),
        (b"\x15", true),
    ];
    let mut lease: Option<(&str, u64)> = None;
    let mut draft = false;
    let mut expected_writes: Vec<Vec<u8>> = Vec::new();
    for round in 0..300 {
        match rand(8) {
            0 | 1 => {
                let got = if rand(2) == 0 {
                    b.keys(&t, &[Key::Enter], &default_guard()).map(drop)
                } else {
                    b.keys_if_screen(&t, &[Key::Enter], &default_guard(), &|_| Ok(()))
                        .map(drop)
                };
                if lease.is_some() {
                    assert!(is_human_controlled(&kind_of(got)), "round {round}");
                } else if draft {
                    assert_eq!(kind_of(got), ErrorKind::DraftPending, "round {round}");
                } else {
                    got.unwrap();
                    expected_writes.push(b"\r".to_vec());
                }
            }
            2 => {
                let who = ["a", "b"][rand(2) as usize];
                let got = b.lease_acquire(&t, who, false);
                match lease {
                    Some((holder, _)) if holder != who => {
                        assert!(is_human_controlled(&kind_of(got)), "round {round}");
                    }
                    _ => {
                        let grant = got.unwrap();
                        if let Some((_, id)) = lease {
                            assert_eq!(grant.id, id, "round {round}: a renewal keeps the lease");
                        }
                        lease = Some((who, grant.id));
                    }
                }
            }
            3 => {
                let who = ["a", "b"][rand(2) as usize];
                let grant = b.lease_acquire(&t, who, true).unwrap();
                lease = Some((who, grant.id));
            }
            4 => {
                let (bytes, left_set) = typed[rand(typed.len() as u64) as usize];
                let id = lease.map_or(u64::MAX, |(_, id)| id);
                let got = b.human_write(&t, id, bytes);
                if lease.is_some() {
                    got.unwrap();
                    expected_writes.push(bytes.to_vec());
                    draft = left_set;
                } else {
                    got.unwrap_err();
                }
            }
            5 => {
                if let Some((_, id)) = lease.take() {
                    assert!(b.lease_release(&t, id).unwrap());
                }
            }
            _ => {
                assert_eq!(b.draft_clear(&t).unwrap(), draft, "round {round}");
                draft = false;
            }
        }
        let now = b.human_state(&t).unwrap();
        assert_eq!(now.lease.is_some(), lease.is_some(), "round {round}");
        assert_eq!(now.draft, draft, "round {round}");
    }
    assert_eq!(s.host.writes_since(0), expected_writes);
}

#[test]
fn a_second_holder_is_refused_unless_it_preempts_and_the_old_one_is_then_lost() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    let first = s.b().lease_acquire(&t, "one", false).unwrap();
    assert!(is_human_controlled(&kind_of(
        s.b().lease_acquire(&t, "two", false)
    )));
    let again = s.b().lease_acquire(&t, "one", false).unwrap();
    assert_eq!(again.id, first.id);
    let second = s.b().lease_acquire(&t, "two", true).unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(s.b().lease_status(&t, first.id).unwrap(), LeaseStatus::Lost);
    assert!(matches!(
        s.b().lease_status(&t, second.id).unwrap(),
        LeaseStatus::Held { .. }
    ));
    let mark = s.host.mark();
    s.b().human_write(&t, first.id, b"x").unwrap_err();
    assert!(s.host.writes_since(mark).is_empty());
    assert!(!draft_of(s.b(), &t), "a refused write sets no draft");
    // Release ends the lease, never the draft.
    s.b().human_write(&t, second.id, b"x").unwrap();
    assert!(s.b().lease_release(&t, second.id).unwrap());
    assert!(!s.b().lease_release(&t, second.id).unwrap());
    assert!(!lease_of(s.b(), &t) && draft_of(s.b(), &t));
}

#[test]
fn a_human_byte_sets_the_draft_and_a_bot_waits_for_an_enter_or_a_clear() {
    let clock = TestClock::new();
    let s = Split::with_config(1 << 20, clock.config());
    let script = format!("printf '{IDLE}'; read go; printf '{WORKING}{IDLE}'; cat > /dev/null");
    let (info, _dir) = s.run("p", &script);
    let t = tref(&info);
    eventually("idle", || {
        state_is_idle(&s.b().get(&t).unwrap()).then_some(())
    });
    let turn0 = s.b().get(&t).unwrap().turn;
    let id = typing(s.b(), &t, b"go\n");
    s.b().lease_release(&t, id).unwrap();
    // The program's turn advances: that is not the person finishing.
    eventually("a new turn", || {
        (s.b().get(&t).unwrap().turn > turn0).then_some(())
    });
    // Nor is time.
    clock.advance(Duration::from_secs(3600));
    let mark = s.host.mark();
    assert_eq!(
        kind_of(s.b().keys(&t, &[Key::Enter], &default_guard())),
        ErrorKind::DraftPending
    );
    assert_eq!(
        kind_of(s.b().send(&t, "hi", &default_guard())),
        ErrorKind::DraftPending
    );
    assert!(s.host.writes_since(mark).is_empty());
    assert!(draft_of(s.b(), &t));
    // A person's Enter does.
    let id = typing(s.b(), &t, b"\r");
    s.b().lease_release(&t, id).unwrap();
    assert!(!draft_of(s.b(), &t));
    assert!(!load(&*s.host, &info.id).unwrap().human_draft);
    s.b().keys(&t, &[Key::Enter], &default_guard()).unwrap();
}

#[test]
fn only_an_enter_outside_a_paste_clears_the_draft() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    let id = s.b().lease_acquire(&t, "person", false).unwrap().id;
    let b = s.b();
    for (name, chunks, clears) in [
        ("LF", &[&b"x\n"[..]][..], false),
        ("Shift+Enter", &[b"x\x1b[13;2u"], false),
        ("Ctrl-U", &[b"x\x15"], false),
        ("CR inside a paste", &[b"\x1b[200~a\rb\x1b[201~"], false),
        (
            "CR in a paste split over calls",
            &[b"\x1b[200~a\r", b"b\r", b"c"],
            false,
        ),
        ("CR", &[b"x\r"], true),
        ("CSI 13 u", &[b"x\x1b[13u"], true),
        ("CSI 13;1 u", &[b"x\x1b[13;1u"], true),
        ("CR after a paste", &[b"\x1b[200~a\rb\x1b[201~\r"], true),
        ("CR, then text", &[b"x\ry"], false),
        ("CR, then CR", &[b"x\ry\r"], true),
    ] {
        for (n, chunk) in chunks.iter().enumerate() {
            b.human_write(&t, id, chunk).unwrap();
            // Only the last call may clear it: an earlier Enter that counted
            // would clear the draft here, and the next call set it again.
            if !(clears && n + 1 == chunks.len()) {
                assert!(draft_of(b, &t), "{name}: call {n}");
            }
        }
        assert_eq!(!draft_of(b, &t), clears, "{name}");
        // Back to a known state: the paste closed, the draft cleared.
        b.human_write(&t, id, b"\x1b[201~\r").unwrap();
        b.draft_clear(&t).unwrap();
    }
    // A paste that is still open when the lease ends does not leak into the
    // next one.
    b.human_write(&t, id, b"\x1b[200~").unwrap();
    b.lease_release(&t, id).unwrap();
    let id = b.lease_acquire(&t, "person", false).unwrap().id;
    b.human_write(&t, id, b"x\r").unwrap();
    assert!(!draft_of(b, &t), "a new lease starts outside a paste");
}

#[test]
fn a_clear_the_host_does_not_acknowledge_keeps_the_draft_and_the_next_enter_retries() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    let id = typing(s.b(), &t, b"x");
    s.host.fail_meta.store(true, Ordering::SeqCst);
    s.b().human_write(&t, id, b"\r").unwrap();
    assert!(draft_of(s.b(), &t), "fail closed: the flag stays");
    s.b().lease_release(&t, id).unwrap();
    assert_eq!(
        kind_of(s.b().keys(&t, &[Key::Enter], &default_guard())),
        ErrorKind::DraftPending
    );
    // The host is back: the next Enter writes the flag again, then clears it.
    s.host.fail_meta.store(false, Ordering::SeqCst);
    let id = typing(s.b(), &t, b"\r");
    assert!(!draft_of(s.b(), &t));
    assert!(!load(&*s.host, &info.id).unwrap().human_draft);
    s.b().lease_release(&t, id).unwrap();
    s.b().keys(&t, &[Key::Enter], &default_guard()).unwrap();
}

#[test]
fn draft_clear_clears_the_draft_and_leaves_the_lease() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    let id = typing(s.b(), &t, b"x");
    assert!(draft_of(s.b(), &t) && lease_of(s.b(), &t));
    assert!(s.b().draft_clear(&t).unwrap());
    assert!(!draft_of(s.b(), &t) && lease_of(s.b(), &t));
    assert!(!load(&*s.host, &info.id).unwrap().human_draft);
    assert!(
        !s.b().draft_clear(&t).unwrap(),
        "clearing nothing is not an error"
    );
    // The holder is not disturbed.
    s.b().human_write(&t, id, b"y").unwrap();
}

#[test]
fn a_flag_the_host_holds_for_a_byte_that_never_went_is_cleared_too() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    let id = s.b().lease_acquire(&t, "person", false).unwrap().id;
    s.host.fail_write.store(true, Ordering::SeqCst);
    s.b().human_write(&t, id, b"x").unwrap_err();
    s.host.fail_write.store(false, Ordering::SeqCst);
    assert!(load(&*s.host, &info.id).unwrap().human_draft);
    assert!(s.b().draft_clear(&t).unwrap());
    assert!(!load(&*s.host, &info.id).unwrap().human_draft);
}

#[test]
fn a_front_restart_drops_the_lease_and_keeps_the_draft() {
    let mut s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    typing(s.b(), &t, b"half a sent");
    s.kill_front();
    s.new_front();
    let state = s.b().human_state(&t).unwrap();
    assert!(state.lease.is_none(), "a lease is never carried over");
    assert!(state.draft, "the draft is restored from the meta");
    let mark = s.host.mark();
    assert_eq!(
        kind_of(s.b().keys(&t, &[Key::Enter], &default_guard())),
        ErrorKind::DraftPending
    );
    assert!(s.host.writes_since(mark).is_empty());
    // The person comes back, presses Enter, and the bot may write.
    let id = s.b().lease_acquire(&t, "person", false).unwrap().id;
    s.b().human_write(&t, id, b"\r").unwrap();
    s.b().lease_release(&t, id).unwrap();
    s.b().keys(&t, &[Key::Enter], &default_guard()).unwrap();
}

#[test]
fn a_front_that_dies_after_the_flag_and_before_the_byte_adopts_with_the_draft() {
    let mut s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    let id = s.b().lease_acquire(&t, "person", false).unwrap().id;
    s.host.fail_write.store(true, Ordering::SeqCst);
    s.b().human_write(&t, id, b"x").unwrap_err();
    s.kill_front();
    s.host.fail_write.store(false, Ordering::SeqCst);
    s.new_front();
    assert!(draft_of(s.b(), &t));
    assert_eq!(
        kind_of(s.b().keys(&t, &[Key::Enter], &default_guard())),
        ErrorKind::DraftPending
    );
}

#[test]
fn a_lease_idles_out_after_300_seconds_without_input() {
    let clock = TestClock::new();
    let s = Split::with_config(1 << 20, clock.config());
    let (info, _dir) = s.run("p", "sleep 0.3; echo tick; cat > /dev/null");
    let t = tref(&info);
    let id = s.b().lease_acquire(&t, "person", false).unwrap().id;
    // Output does not renew it.
    wait_screen(s.b(), &info, "tick");
    clock.advance(Duration::from_secs(299));
    let kind = kind_of(s.b().keys(&t, &[Key::Enter], &default_guard()));
    assert_eq!(
        kind,
        ErrorKind::HumanControlled {
            idle_secs: 299,
            expires_in_secs: 1
        }
    );
    // Accepted input renews it.
    s.b().human_write(&t, id, b"\x15").unwrap();
    s.b().draft_clear(&t).unwrap();
    clock.advance(Duration::from_secs(299));
    assert!(is_human_controlled(&kind_of(s.b().keys(
        &t,
        &[Key::Enter],
        &default_guard()
    ))));
    // So does an explicit acquire by the holder.
    s.b().lease_acquire(&t, "person", false).unwrap();
    clock.advance(Duration::from_secs(299));
    assert!(lease_of(s.b(), &t));
    // 300 s of silence ends it.
    clock.advance(Duration::from_secs(1));
    assert!(!lease_of(s.b(), &t));
    assert_eq!(s.b().lease_status(&t, id).unwrap(), LeaseStatus::Idle);
    let mark = s.host.mark();
    s.b().human_write(&t, id, b"y").unwrap_err();
    assert!(s.host.writes_since(mark).is_empty());
    assert!(!draft_of(s.b(), &t), "a refused byte sets no draft");
    s.b().keys(&t, &[Key::Enter], &default_guard()).unwrap();
    // Another person may take it over without a preempt.
    s.b().lease_acquire(&t, "other", false).unwrap();
}

#[test]
fn an_idled_out_lease_leaves_the_draft_in_charge() {
    let clock = TestClock::new();
    let s = Split::with_config(1 << 20, clock.config());
    let (info, _dir) = s.run("p", "cat > /dev/null");
    let t = tref(&info);
    typing(s.b(), &t, b"x");
    clock.advance(Duration::from_secs(299));
    assert!(is_human_controlled(&kind_of(s.b().keys(
        &t,
        &[Key::Enter],
        &default_guard()
    ))));
    clock.advance(Duration::from_secs(2));
    assert_eq!(
        kind_of(s.b().keys(&t, &[Key::Enter], &default_guard())),
        ErrorKind::DraftPending
    );
}

#[test]
fn refused_operations_are_never_queued() {
    for scenario in ["lease", "lease and draft", "draft"] {
        let s = Split::new(1 << 20);
        let (info, _dir) = s.run("p", "cat > /dev/null");
        let t = tref(&info);
        let id = match scenario {
            "lease" => s.b().lease_acquire(&t, "person", false).unwrap().id,
            _ => typing(s.b(), &t, b"x"),
        };
        if scenario == "draft" {
            s.b().lease_release(&t, id).unwrap();
        }
        let want_lease = scenario != "draft";
        let g = default_guard();
        let b = s.b();
        let mark = s.host.mark();
        let refused = [
            ("send", kind_of(b.send(&t, "hi", &g))),
            ("keys", kind_of(b.keys(&t, &[Key::Enter], &g))),
            (
                "keys_if_screen (the check allows)",
                kind_of(b.keys_if_screen(&t, &[Key::Enter], &g, &|_| Ok(()))),
            ),
            (
                "keys_if_screen (the check denies)",
                kind_of(b.keys_if_screen(&t, &[Key::Esc], &g, &|_| Err("no".to_string()))),
            ),
            (
                "interrupt",
                kind_of(b.interrupt(&t, Some(Duration::from_millis(50)))),
            ),
            ("remove", kind_of(b.remove(&t, None))),
        ];
        for (op, kind) in refused {
            if want_lease {
                assert!(is_human_controlled(&kind), "{scenario}: {op}: {kind:?}");
            } else {
                assert_eq!(kind, ErrorKind::DraftPending, "{scenario}: {op}");
            }
        }
        assert!(
            s.host.writes_since(mark).is_empty() && !s.host.calls()[mark..].contains(&Call::Kill),
            "{scenario}: {:?}",
            &s.host.calls()[mark..]
        );
        assert!(b.get(&t).unwrap().live, "{scenario}: remove left it alone");
        // Nothing waits to go out when the person leaves.
        if want_lease {
            b.lease_release(&t, id).unwrap();
        }
        thread::sleep(Duration::from_secs(1));
        assert!(
            s.host.writes_since(mark).is_empty(),
            "{scenario}: something was queued"
        );
    }
}

#[test]
fn a_refused_send_leaves_no_pasted_text() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat");
    let t = tref(&info);
    typing(s.b(), &t, b"mine");
    s.b().send(&t, "BOT-TEXT", &default_guard()).unwrap_err();
    thread::sleep(Duration::from_millis(300));
    let text = screen(s.b(), &info);
    assert!(
        text.contains("mine") && !text.contains("BOT-TEXT"),
        "{text}"
    );
}

#[test]
fn a_finished_pane_is_removable_whatever_a_person_left_behind() {
    let s = Split::new(1 << 20);
    let (info, _dir) = s.run("p", "cat");
    let t = tref(&info);
    typing(s.b(), &t, b"x");
    assert!(is_human_controlled(&kind_of(s.b().remove(&t, None))));
    s.host.inner().signal(&info.id, 9).unwrap();
    eventually("the exit", || (!s.b().get(&t).unwrap().live).then_some(()));
    assert_eq!(
        kind_of(s.b().keys(&t, &[Key::Enter], &default_guard())),
        ErrorKind::Finished
    );
    s.b().remove(&t, None).unwrap();
}

#[test]
fn the_programs_own_query_replies_are_never_refused() {
    let s = Split::new(1 << 20);
    let script = format!("printf '{IDLE}READY\\n'; read go; printf '\\033[5n'; cat > /dev/null");
    let (info, _dir) = s.run("p", &script);
    wait_screen(s.b(), &info, "READY");
    let t = tref(&info);
    let id = s.b().lease_acquire(&t, "person", false).unwrap().id;
    s.b().send(&t, "hi", &default_guard()).unwrap_err();
    let mark = s.host.mark();
    s.b().human_write(&t, id, b"go\r").unwrap();
    eventually("the reply to the program's query", || {
        s.host
            .writes_since(mark)
            .iter()
            .any(|w| w == b"\x1b[0n")
            .then_some(())
    });
    assert!(lease_of(s.b(), &t), "the lease was held all along");
}

#[test]
fn a_person_may_type_into_a_replayed_pane() {
    let mut s = Split::new(2048);
    let script = "yes 0123456789 | head -c 20000; echo READY; cat > /dev/null";
    let (info, _dir) = s.run("p", script);
    wait_screen(s.b(), &info, "READY");
    wait_wrapped(&s.host, &info);
    s.kill_front();
    s.new_front();
    let t = tref(&info);
    assert!(is_replayed(s.b(), &info));
    let mark = s.host.mark();
    typing(s.b(), &t, b"x");
    assert_eq!(s.host.writes_since(mark), [b"x".to_vec()]);
    // A bot's refusal names the person, not the replay.
    assert!(is_human_controlled(&kind_of(s.b().keys(
        &t,
        &[Key::Enter],
        &default_guard()
    ))));
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

// ---------------------------------------------------------------------------
// PR4: the tools layer's record of a pane survives the front
// ---------------------------------------------------------------------------

mod tools_layer {
    use super::*;
    use crate::profiles::LaunchPolicy;
    use crate::registry::{PaneRegistry, RoutineState};
    use crate::tools::{call, Args, Shepherd};
    use serde_json::{json, Value};

    fn args(value: Value) -> Args {
        value.as_object().unwrap().clone()
    }

    /// A policy with one `kiro` profile that runs `script` in `dir`.
    fn policy(dir: &TempDir, script: &str, routine: Option<Value>) -> LaunchPolicy {
        let mut profile = json!({
            "name": "kiro",
            "argv": ["/bin/sh", "-c", script],
            "cwds": [dir.path()],
        });
        if let Some(routine) = routine {
            profile["routine"] = routine;
        }
        LaunchPolicy::from_json(&json!({"profiles": [profile]}).to_string()).unwrap()
    }

    /// The tools over the split's current front, with the front as registry.
    fn tools(s: &Split, policy: LaunchPolicy) -> Shepherd {
        let b = s.b().clone();
        Shepherd::with_policy(b.clone(), policy).with_registry(Arc::new(b))
    }

    /// Kill the front the way a restart does: the tools and the backend go,
    /// the host and its panes stay. Then start a new front over the same
    /// host, adopt, and rebuild the tools from the record.
    fn restart(s: &mut Split, sh: Shepherd, policy: LaunchPolicy) -> Shepherd {
        drop(sh);
        s.kill_front();
        s.new_front();
        let sh = tools(s, policy);
        sh.adopt_registry();
        sh
    }

    const BLOCKED: &str = "\\033]7501;state=blocked:kind=permission\\007";
    const DONE: &str = "\\033]7501;state=done\\007";

    /// A pane that shows a Kiro permission dialog, then reads one line from
    /// its terminal and notes that it did.
    fn dialog_script(filler: usize) -> String {
        format!(
            "i=0; while [ $i -lt {filler} ]; do echo \"filler line $i\"; i=$((i+1)); done; \
             printf '{WORKING}'; printf '{BLOCKED}'; cat dialog.txt; \
             IFS= read -r l; echo got > received.txt; sleep 30"
        )
    }

    fn start(sh: &Shepherd, name: &str) {
        call(
            sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": name})),
        )
        .unwrap();
    }

    fn answer(sh: &Shepherd, name: &str) -> Result<Value, crate::tools::ToolError> {
        call(
            sh,
            "answer_dialog",
            &args(json!({"terminal": name, "choice": "allow_once"})),
        )
    }

    #[test]
    fn answer_dialog_works_on_a_pane_a_front_before_started_when_the_tail_is_complete() {
        let mut s = Split::new(1 << 20);
        let dir = TempDir::new("pr4-dialog-live");
        std::fs::write(
            dir.path().join("dialog.txt"),
            crate::dialog::fixtures::dialog_on_allow(),
        )
        .unwrap();
        let pol = policy(&dir, &dialog_script(0), None);
        let sh = tools(&s, pol);
        start(&sh, "dlg");
        let info = s.b().get(&TerminalRef::new("dlg")).unwrap();
        wait_screen(s.b(), &info, "requires approval");
        eventually("the blocked status", || {
            let got = s.b().get(&TerminalRef::new("dlg")).ok()?;
            got.status
                .as_ref()
                .is_some_and(|st| st.is_blocked_with("permission"))
                .then_some(())
        });
        let sh = restart(&mut s, sh, policy(&dir, &dialog_script(0), None));
        assert!(!is_replayed(s.b(), &info), "a complete tail is live");
        // Without the record there would be no dialect and no keys.
        answer(&sh, "dlg").expect("the dialog is answered after the restart");
        eventually("the program to get the key", || {
            dir.path().join("received.txt").exists().then_some(())
        });
    }

    #[test]
    fn answer_dialog_is_refused_and_sends_nothing_while_the_pane_is_replayed() {
        let mut s = Split::new(2048);
        let dir = TempDir::new("pr4-dialog-replayed");
        std::fs::write(
            dir.path().join("dialog.txt"),
            crate::dialog::fixtures::dialog_on_allow(),
        )
        .unwrap();
        // Enough output ahead of the dialog that the small tail has wrapped.
        let pol = policy(&dir, &dialog_script(400), None);
        let sh = tools(&s, pol);
        start(&sh, "dlg");
        let info = s.b().get(&TerminalRef::new("dlg")).unwrap();
        wait_screen(s.b(), &info, "requires approval");
        eventually("the blocked status", || {
            let got = s.b().get(&TerminalRef::new("dlg")).ok()?;
            got.status
                .as_ref()
                .is_some_and(|st| st.is_blocked_with("permission"))
                .then_some(())
        });
        wait_wrapped(&s.host, &info);
        // The status is in memory before the host has it: wait for the host,
        // or the front dies before its snapshot lands and the pane adopts
        // as unknown, which is a different test.
        eventually("the snapshot in the host", || {
            let kept = load(&*s.host, &info.id)?;
            (kept.tracker?.to_json()["state"] == "blocked").then_some(())
        });
        let sh = restart(&mut s, sh, policy(&dir, &dialog_script(400), None));
        assert!(is_replayed(s.b(), &info), "a wrapped tail is replayed");
        let err = answer(&sh, "dlg").expect_err("a replayed pane is not answered");
        assert_eq!(err.data["error"], "pane_replayed", "{err:?}");
        thread::sleep(Duration::from_millis(500));
        assert!(
            !dir.path().join("received.txt").exists(),
            "the program received a key through a replayed pane"
        );
    }

    #[test]
    fn the_routine_counters_and_the_turn_survive_a_restart_and_the_turn_is_not_counted_twice() {
        let mut s = Split::new(1 << 20);
        let dir = TempDir::new("pr4-routine");
        let script = format!(
            "printf '{WORKING}'; printf '{DONE}'; printf '{WORKING}'; printf '{DONE}'; \
             cat > typed.txt"
        );
        let routine = || {
            json!({"idle_timeout_secs": 10, "cooldown_secs": 1,
                   "max_consecutive_nudges": 3, "nudge_text": "continue the work"})
        };
        let sh = tools(&s, policy(&dir, &script, Some(routine())));
        start(&sh, "w");
        let info = s.b().get(&TerminalRef::new("w")).unwrap();
        let turn_before = eventually("two finished turns", || {
            let got = s.b().get(&TerminalRef::new("w")).ok()?;
            (got.turn == Some(2)
                && got
                    .status
                    .as_ref()
                    .is_some_and(|st| st.state == State::Done))
            .then_some(2)
        });
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        eventually("the nudge in the pane", || {
            std::fs::read_to_string(dir.path().join("typed.txt"))
                .ok()?
                .contains("continue the work")
                .then_some(())
        });
        // The count was in the host before the bytes went in.
        let calls = s.host.calls();
        let write = calls
            .iter()
            .position(|c| matches!(c, Call::Write(b) if b.windows(8).any(|w| w == b"continue")))
            .expect("the nudge was written");
        assert!(
            calls[..write]
                .iter()
                .any(|c| matches!(c, Call::MetaSet { .. })),
            "{calls:?}"
        );
        let kept = load(&*s.host, &info.id).unwrap();
        assert_eq!(
            kept.routine.as_ref().map(|r| r.consecutive),
            Some(1),
            "{kept:?}"
        );
        assert_eq!(kept.agent.as_deref(), Some("kiro"));
        assert_eq!(kept.profile.as_deref(), Some("kiro"));

        let sh = restart(&mut s, sh, policy(&dir, &script, Some(routine())));
        // The agent kind is back, and the watch carries the count.
        assert_eq!(sh.agent_of("w"), Some(crate::agent::Agent::Kiro));
        let registered = PaneRegistry::registered(s.b());
        assert_eq!(registered.len(), 1);
        assert_eq!(
            registered[0].routine,
            Some(RoutineState {
                consecutive: 1,
                gave_up: false,
                nudge_allowance: crate::routine::NUDGE_TURN_ALLOWANCE,
            })
        );
        // The turn is what it was: the replay did not count a finished turn
        // again (a repeat would read as progress and clear the cap).
        let got = s.b().get(&TerminalRef::new("w")).unwrap();
        assert_eq!(got.turn, Some(turn_before));
    }

    #[test]
    fn a_count_the_host_cannot_keep_means_the_nudge_is_not_typed() {
        let s = Split::new(1 << 20);
        let dir = TempDir::new("pr4-routine-refused");
        let script = format!("printf '{WORKING}'; printf '{DONE}'; cat > typed.txt");
        let routine = json!({"idle_timeout_secs": 10, "cooldown_secs": 1,
                             "nudge_text": "continue the work"});
        let sh = tools(&s, policy(&dir, &script, Some(routine)));
        start(&sh, "w");
        eventually("a finished turn", || {
            let got = s.b().get(&TerminalRef::new("w")).ok()?;
            got.status
                .as_ref()
                .is_some_and(|st| st.state == State::Done)
                .then_some(())
        });
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        let mark = s.host.mark();
        s.host.fail_meta.store(true, Ordering::SeqCst);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert!(
            s.host.writes_since(mark).is_empty(),
            "bytes went in uncounted"
        );
        s.host.fail_meta.store(false, Ordering::SeqCst);
        sh.routine_tick_at(t0 + Duration::from_secs(12));
        eventually("the nudge once the count is kept", || {
            std::fs::read_to_string(dir.path().join("typed.txt"))
                .ok()?
                .contains("continue the work")
                .then_some(())
        });
    }

    #[test]
    fn meta_a_later_version_wrote_resurrects_no_agent_and_no_watch() {
        let mut s = Split::new(1 << 20);
        let dir = TempDir::new("pr4-stale");
        let script = format!("printf '{WORKING}'; printf '{DONE}'; sleep 30");
        let routine = json!({"idle_timeout_secs": 10, "nudge_text": "continue the work"});
        let sh = tools(&s, policy(&dir, &script, Some(routine.clone())));
        start(&sh, "w");
        let info = s.b().get(&TerminalRef::new("w")).unwrap();
        // The front writes the turn to the meta after it has counted it, on
        // the thread that reads the pane. Let that write land before the
        // front dies, or it can overwrite the blob this test plants next.
        eventually("the turn in the meta", || {
            load(&*s.host, &info.id)?
                .tracker
                .filter(|t| t.turn() == 1)
                .map(|_| ())
        });
        drop(sh);
        s.kill_front();
        s.host
            .meta_set(&info.id, br#"{"v":99,"agent":"kiro"}"#)
            .unwrap();
        s.new_front();
        let sh = tools(&s, policy(&dir, &script, Some(routine)));
        sh.adopt_registry();
        assert!(PaneRegistry::registered(s.b()).is_empty());
        assert_eq!(sh.agent_of("w"), None);
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert!(s.host.writes_since(0).is_empty());
    }

    #[test]
    fn removing_a_pane_leaves_no_record_behind() {
        let s = Split::new(1 << 20);
        let dir = TempDir::new("pr4-remove");
        let sh = tools(&s, policy(&dir, "sleep 30", None));
        start(&sh, "w");
        assert_eq!(PaneRegistry::registered(s.b()).len(), 1);
        call(&sh, "remove_pane", &args(json!({"terminal": "w"}))).unwrap();
        assert!(PaneRegistry::registered(s.b()).is_empty());
    }
    // ---- issue #77, PR2: the tools over a real native front ----

    /// The tools with the front as the person side, over a pane that reports
    /// `done` and then reads its terminal.
    fn shared_tools(s: &Split, dir: &TempDir) -> Shepherd {
        let script = format!("printf '{DONE}'; cat > /dev/null");
        let b = s.b().clone();
        Shepherd::with_policy(b.clone(), policy(dir, &script, None)).with_shared_panes(Arc::new(b))
    }

    fn code_of(result: Result<Value, crate::tools::ToolError>) -> String {
        result.expect_err("the call should be refused").data["error"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn a_draft_is_cleared_by_draft_clear_and_by_nothing_a_bot_does() {
        let s = Split::new(1 << 20);
        let dir = TempDir::new("pr2-draft-clear");
        let sh = shared_tools(&s, &dir);
        start(&sh, "w");
        let t = TerminalRef::new("w");
        eventually("the done status", || {
            let got = s.b().get(&t).ok()?;
            got.status
                .as_ref()
                .is_some_and(|st| st.state == State::Done)
                .then_some(())
        });
        let get = |tool: &str| call(&sh, tool, &args(json!({"terminal": "w"}))).unwrap();
        assert!(get("get_status").get("human").is_none(), "nobody is here");
        let id = typing(s.b(), &t, b"half a line");
        let mark = s.host.mark();
        let status = get("get_status");
        assert_eq!(status["human"]["draft"], true, "{status}");
        assert_eq!(status["human"]["lease"]["holder"], "person", "{status}");
        assert!(
            status["human"]["lease"]["expires_in_secs"]
                .as_u64()
                .unwrap()
                <= 300
        );
        let send = || {
            call(
                &sh,
                "send_input",
                &args(json!({"terminal": "w", "text": "bot"})),
            )
        };
        let interrupt = || call(&sh, "interrupt_pane", &args(json!({"terminal": "w"})));
        let remove = || call(&sh, "remove_pane", &args(json!({"terminal": "w"})));
        // The lease first, with the draft under it.
        for refused in [code_of(send()), code_of(interrupt()), code_of(remove())] {
            assert_eq!(refused, "pane_human_controlled");
        }
        // The person lets go; the draft is what is left.
        s.b().lease_release(&t, id).unwrap();
        let listed = call(&sh, "list_panes", &args(json!({}))).unwrap();
        assert_eq!(
            listed["panes"][0]["human"],
            json!({"lease": null, "draft": true})
        );
        for refused in [code_of(send()), code_of(interrupt()), code_of(remove())] {
            assert_eq!(refused, "pane_draft_pending");
        }
        // Waiting, and the refusals themselves, clear nothing and queue nothing.
        thread::sleep(Duration::from_secs(1));
        assert!(draft_of(s.b(), &t));
        assert!(s.host.writes_since(mark).is_empty());
        assert!(!s.host.calls()[mark..].contains(&Call::Kill));
        // A person with lease:preempt clears it; the next bot write goes.
        let cleared = get("draft_clear");
        assert_eq!(cleared["cleared"], true, "{cleared}");
        assert!(!draft_of(s.b(), &t));
        assert!(
            !load(&*s.host, &s.b().get(&t).unwrap().id)
                .unwrap()
                .human_draft
        );
        assert!(get("get_status").get("human").is_none());
        call(
            &sh,
            "send_input",
            &args(json!({"terminal": "w", "text": "bot"})),
        )
        .unwrap();
        assert!(
            !s.host.writes_since(mark).is_empty(),
            "the bot's write went after the clear"
        );
    }

    #[test]
    fn draft_clear_leaves_a_held_lease_and_a_bot_stays_refused() {
        let s = Split::new(1 << 20);
        let dir = TempDir::new("pr2-draft-clear-lease");
        let sh = shared_tools(&s, &dir);
        start(&sh, "w");
        let t = TerminalRef::new("w");
        let id = typing(s.b(), &t, b"x");
        let out = call(&sh, "draft_clear", &args(json!({"terminal": "w"}))).unwrap();
        assert_eq!(out["cleared"], true);
        assert!(lease_of(s.b(), &t) && !draft_of(s.b(), &t));
        let mark = s.host.mark();
        let sent = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "w", "text": "bot"})),
        );
        assert_eq!(code_of(sent), "pane_human_controlled");
        assert!(s.host.writes_since(mark).is_empty());
        s.b().human_write(&t, id, b"y").unwrap();
    }

    #[test]
    fn a_bare_front_with_no_person_side_behaves_as_before() {
        let s = Split::new(1 << 20);
        let dir = TempDir::new("pr2-bare");
        let script = format!("printf '{DONE}'; cat > /dev/null");
        let sh = tools(&s, policy(&dir, &script, None));
        start(&sh, "w");
        let t = TerminalRef::new("w");
        eventually("the done status", || {
            let got = s.b().get(&t).ok()?;
            got.status
                .as_ref()
                .is_some_and(|st| st.state == State::Done)
                .then_some(())
        });
        let status = call(&sh, "get_status", &args(json!({"terminal": "w"}))).unwrap();
        assert!(status.get("human").is_none());
        let cleared = call(&sh, "draft_clear", &args(json!({"terminal": "w"})));
        assert_eq!(code_of(cleared), "unsupported");
        call(
            &sh,
            "send_input",
            &args(json!({"terminal": "w", "text": "bot"})),
        )
        .unwrap();
    }
}
