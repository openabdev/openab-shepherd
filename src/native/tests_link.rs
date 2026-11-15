//! The front over a holder link (issue #76, PR5): what only shows with a real
//! front in the picture. The host-level link tests are in `host/tests_link.rs`;
//! the whole native suite over a link is `tests_held` and its siblings.

use super::host::client::HolderClient;
use super::host::held::GateHost;
use super::host::tests_link::report;
use super::host::{Attached, HostPane, KillOutcome, SpawnReq, Spawned};
use super::*;
use crate::testutil::TempDir;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc;

const LONG: Duration = Duration::from_secs(15);

fn eventually(what: &str, mut probe: impl FnMut() -> bool) {
    let end = Instant::now() + LONG;
    while !probe() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn spec(name: &str, script: &str, dir: &TempDir) -> RunSpec {
    RunSpec {
        argv: Some(vec!["/bin/sh".into(), "-c".into(), script.into()]),
        name: Some(name.to_string()),
        cwd: Some(dir.path().to_string_lossy().into_owned()),
        ..RunSpec::default()
    }
}

/// How many `output` events are inside the sink (entered) and how many have
/// come out of it (done): a difference means the events thread is stuck in
/// the front, which is how it waits for a pane's `state`.
#[derive(Default)]
struct Depth {
    entered: AtomicUsize,
    done: AtomicUsize,
}

struct Counting {
    next: Arc<dyn HostEvents>,
    depth: Arc<Depth>,
}

impl HostEvents for Counting {
    fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool {
        self.depth.entered.fetch_add(1, Ordering::SeqCst);
        let more = self.next.output(id, seq, bytes);
        self.depth.done.fetch_add(1, Ordering::SeqCst);
        more
    }
    fn idle(&self, id: &str) {
        self.next.idle(id);
    }
    fn exited(&self, id: &str, code: Option<i64>) {
        self.next.exited(id, code);
    }
    fn gap(&self, id: &str, new_start: u64) {
        self.next.gap(id, new_start);
    }
}

/// The client, with the sink the front installs wrapped to count.
struct Probe {
    inner: Arc<HolderClient>,
    depth: Arc<Depth>,
}

impl PtyHost for Probe {
    fn set_sink(&self, sink: Arc<dyn HostEvents>) {
        self.inner.set_sink(Arc::new(Counting {
            next: sink,
            depth: Arc::clone(&self.depth),
        }));
    }
    fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError> {
        self.inner.spawn(req)
    }
    fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize> {
        self.inner.try_write(id, bytes)
    }
    fn wait_writable(&self, id: &str, timeout: Duration) -> io::Result<bool> {
        self.inner.wait_writable(id, timeout)
    }
    fn resize(&self, id: &str, cols: u16, rows: u16) -> io::Result<()> {
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
        self.inner.list()
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
        self.inner.meta_set(id, bytes)
    }
    fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
        self.inner.meta_get(id)
    }
    fn survives_restart(&self) -> bool {
        true
    }
}

fn front_over(client: Arc<HolderClient>) -> (NativeBackend, Arc<Depth>) {
    let depth = Arc::new(Depth::default());
    let probe = Arc::new(Probe {
        inner: client,
        depth: Arc::clone(&depth),
    });
    let b = NativeBackend::with_host_meta(
        NativeConfig::default(),
        Box::new(|key| std::env::var_os(key)),
        probe,
        true,
    );
    (b, depth)
}

/// `write_guarded` holds the pane's `state` while it waits for the holder's
/// reply to the `write`; the events thread, meanwhile, is inside `ingest`
/// waiting for the same `state`. The reply must reach its caller anyway: the
/// reply router takes no front lock. With one stream, or a router that waits
/// for the events thread, this is a deadlock.
#[test]
fn the_reply_router_does_not_deadlock_with_a_write_that_holds_the_panes_state() {
    let gate_host = GateHost::new(1 << 20);
    let rig = gate_host.rig();
    let (b, depth) = front_over(rig.connect());
    let dir = TempDir::new("link-deadlock");
    let info = b
        .run(&spec(
            "p",
            "while :; do printf tick; sleep 0.02; done",
            &dir,
        ))
        .unwrap();
    eventually("output to flow", || depth.done.load(Ordering::SeqCst) >= 3);

    // Hold the holder's answer to the front's next write.
    gate_host.gate.arm();
    let (tx, rx) = mpsc::channel();
    let sender = {
        let b = b.clone();
        let t = info.reference();
        thread::spawn(move || {
            let result = b.send(&t, "x", &InputGuard::default());
            tx.send(result.is_ok()).unwrap();
        })
    };
    assert!(
        gate_host.gate.wait_entered(1, LONG),
        "the write never reached the holder"
    );
    // The events thread is now inside the sink, on the lock the writer holds.
    eventually("the events thread to be stuck in ingest", || {
        depth.entered.load(Ordering::SeqCst) > depth.done.load(Ordering::SeqCst)
    });
    thread::sleep(Duration::from_millis(200));
    assert!(
        depth.entered.load(Ordering::SeqCst) > depth.done.load(Ordering::SeqCst),
        "the events thread should still be waiting for the pane's state"
    );
    assert!(
        rx.try_recv().is_err(),
        "the send cannot finish before its reply"
    );

    // The reply goes out; its caller must hear it while the events thread
    // is still stuck behind that caller.
    gate_host.gate.open();
    let sent = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the reply never reached the writer: the router is waiting for the events thread");
    assert!(sent);
    sender.join().unwrap();
    let before = depth.done.load(Ordering::SeqCst);
    eventually("the events thread to go on", || {
        depth.done.load(Ordering::SeqCst) > before
    });
}

/// A front that falls behind on a real link is told of the gap, and the pane
/// then refuses guarded writes like any replayed pane.
#[test]
fn a_front_that_falls_behind_has_a_replayed_unknown_pane_and_a_new_front_adopts_it_so() {
    let rig = super::host::held::Held::with_tail(4096);
    let client = rig.connect();
    let (b, _depth) = front_over(Arc::clone(&client));
    let dir = TempDir::new("link-gap-front");
    client.pause_events(true);
    // A newline before END: after a gap the column the emulator is at is
    // anyone's guess, and a marker that straddles a wrapped line is not found.
    let script =
        "printf 'BEGIN '; head -c 3000000 /dev/zero | tr '\\0' 'a'; printf '\\nEND'; cat > /dev/null";
    let info = b.run(&spec("p", script, &dir)).unwrap();
    eventually("the program's output to pass the ring", || {
        rig.local
            .attach(&info.id)
            .is_ok_and(|at| at.tail_end > 3_000_000)
    });
    client.pause_events(false);
    let pane = b.inner.resolve(&info.reference()).unwrap();
    let end = Instant::now() + LONG;
    while !b
        .read(&info.reference(), 0)
        .is_ok_and(|s| s.text.contains("END"))
    {
        if Instant::now() >= end {
            let st = pane.state.lock().unwrap();
            let holder = rig
                .local
                .attach(&info.id)
                .map(|at| (at.tail_start, at.tail_end));
            panic!(
                "END never reached the screen: fed_end {} replayed {} unknown {} live {} \
                 closed {}; holder tail {holder:?}; screen tail {:?}",
                st.fed_end,
                st.replayed,
                st.unknown,
                st.live,
                client.is_closed(),
                st.emu
                    .screen_text()
                    .chars()
                    .rev()
                    .take(60)
                    .collect::<String>(),
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    {
        let st = pane.state.lock().unwrap();
        assert!(st.replayed && st.unknown, "the gap left the pane trusted");
    }
    let refused = b
        .keys(&info.reference(), &[Key::Enter], &InputGuard::default())
        .unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Replayed);

    // A new front gets the same answer from the wrapped tail.
    b.leave_panes_on_drop();
    drop(pane);
    drop(b);
    drop(client);
    let (b2, _) = front_over(rig.connect());
    let adoption = b2.adopt().unwrap();
    assert_eq!(adoption.adopted, vec![info.id.clone()]);
    assert_eq!(adoption.replayed, vec![info.id.clone()]);
}

// ---------------------------------------------------------------------------
// Measurements (run by hand: `cargo test --release --lib -- --ignored
// --nocapture measure_`)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Tally {
    gaps: AtomicUsize,
    bytes: AtomicUsize,
}

struct Tallying {
    next: Arc<dyn HostEvents>,
    tally: Arc<Tally>,
}

impl HostEvents for Tallying {
    fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool {
        self.tally.bytes.fetch_add(bytes.len(), Ordering::Relaxed);
        self.next.output(id, seq, bytes)
    }
    fn idle(&self, id: &str) {
        self.next.idle(id);
    }
    fn exited(&self, id: &str, code: Option<i64>) {
        self.next.exited(id, code);
    }
    fn gap(&self, id: &str, new_start: u64) {
        self.tally.gaps.fetch_add(1, Ordering::Relaxed);
        self.next.gap(id, new_start);
    }
}

/// `yes | head -c 200M` through a pane, three ways: the program's own pace
/// is how long it takes to finish with nobody reading its output (events
/// held), with a front in process, and with a front over the link. The link
/// front may gap; the program must not be slowed.
#[test]
#[ignore = "measurement"]
fn measure_flood_through_a_pane() {
    const TOTAL: usize = 200 * 1024 * 1024;
    let script =
        format!("yes 0123456789abcdef | head -c {TOTAL}; printf '\\nFLOOD-DONE'; cat > /dev/null");
    let dir = TempDir::new("measure-flood");
    let wait_done = |b: &NativeBackend, info: &TerminalInfo| {
        let end = Instant::now() + Duration::from_secs(300);
        while !b
            .read(&info.reference(), 0)
            .is_ok_and(|s| s.text.contains("FLOOD-DONE"))
        {
            assert!(Instant::now() < end, "the flood never finished");
            thread::sleep(Duration::from_millis(20));
        }
    };

    // In process: the front's ingest is the pump's own sink.
    let b = NativeBackend::new(NativeConfig::default());
    let started = Instant::now();
    let info = b.run(&spec("f", &script, &dir)).unwrap();
    wait_done(&b, &info);
    eprintln!(
        "MEASURE flood in process: front saw FLOOD-DONE after {:.2} s",
        started.elapsed().as_secs_f64()
    );
    b.shutdown();

    // Over the link, events held: how fast the program runs when nothing
    // reads (the pump is not held back).
    let rig = super::host::held::Held::new();
    let client = rig.connect();
    client.pause_events(true);
    let (b, _) = front_over(Arc::clone(&client));
    let started = Instant::now();
    let info = b.run(&spec("f", &script, &dir)).unwrap();
    let end = Instant::now() + Duration::from_secs(300);
    while !rig.local.list().iter().any(|p| p.id == info.id && false) {
        // The program parks in `cat` after FLOOD-DONE: done when the ring ends with it.
        let at = rig.local.attach(&info.id).unwrap();
        if String::from_utf8_lossy(&at.bytes).contains("FLOOD-DONE") {
            break;
        }
        assert!(Instant::now() < end, "the held flood never finished");
        thread::sleep(Duration::from_millis(20));
    }
    eprintln!(
        "MEASURE flood over the link, events held: program finished after {:.2} s",
        started.elapsed().as_secs_f64()
    );
    client.pause_events(false);
    wait_done(&b, &info);
    eprintln!(
        "MEASURE flood over the link, events held then released: front saw FLOOD-DONE after {:.2} s",
        started.elapsed().as_secs_f64()
    );
    drop(b);
    drop(client);

    // Over the link, front reading as it goes.
    let client = rig.connect();
    let tally = Arc::new(Tally::default());
    struct Tallied(Arc<HolderClient>, Arc<Tally>);
    impl PtyHost for Tallied {
        fn set_sink(&self, sink: Arc<dyn HostEvents>) {
            self.0.set_sink(Arc::new(Tallying {
                next: sink,
                tally: Arc::clone(&self.1),
            }));
        }
        fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError> {
            self.0.spawn(req)
        }
        fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize> {
            self.0.try_write(id, bytes)
        }
        fn wait_writable(&self, id: &str, t: Duration) -> io::Result<bool> {
            self.0.wait_writable(id, t)
        }
        fn resize(&self, id: &str, c: u16, r: u16) -> io::Result<()> {
            self.0.resize(id, c, r)
        }
        fn signal(&self, id: &str, s: i32) -> io::Result<()> {
            self.0.signal(id, s)
        }
        fn kill(&self, id: &str, g: Duration) -> Option<KillOutcome> {
            self.0.kill(id, g)
        }
        fn fg_pgrp(&self, id: &str) -> Option<i32> {
            self.0.fg_pgrp(id)
        }
        fn list(&self) -> Vec<HostPane> {
            self.0.list()
        }
        fn forget(&self, id: &str) {
            self.0.forget(id);
        }
        fn end_all(&self) {
            self.0.end_all();
        }
        fn attach(&self, id: &str) -> io::Result<Attached> {
            self.0.attach(id)
        }
        fn meta_set(&self, id: &str, b: &[u8]) -> io::Result<()> {
            self.0.meta_set(id, b)
        }
        fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
            self.0.meta_get(id)
        }
        fn survives_restart(&self) -> bool {
            true
        }
    }
    let b = NativeBackend::with_host_meta(
        NativeConfig::default(),
        Box::new(|key| std::env::var_os(key)),
        Arc::new(Tallied(Arc::clone(&client), Arc::clone(&tally))),
        true,
    );
    let started = Instant::now();
    let info = b.run(&spec("f2", &script, &dir)).unwrap();
    wait_done(&b, &info);
    eprintln!(
        "MEASURE flood over the link, front reading: front saw FLOOD-DONE after {:.2} s; \
         it was handed {:.1} MiB of {} MiB and told of {} gaps",
        started.elapsed().as_secs_f64(),
        tally.bytes.load(Ordering::Relaxed) as f64 / 1048576.0,
        TOTAL / 1048576,
        tally.gaps.load(Ordering::Relaxed)
    );
    b.shutdown();
}

/// While one pane is written to as fast as possible (each write holds that
/// pane's `state` across a round trip), how late are the events of a pane
/// that ticks every 5 ms? One events thread serves both panes.
#[test]
#[ignore = "measurement"]
fn measure_head_of_line_behind_a_writer() {
    use std::sync::Mutex as StdMutex;
    struct Arrivals {
        next: Arc<dyn HostEvents>,
        ticker: StdMutex<Option<String>>,
        at: Arc<StdMutex<Vec<Instant>>>,
    }
    impl HostEvents for Arrivals {
        fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool {
            if self.ticker.lock().unwrap().as_deref() == Some(id) {
                self.at.lock().unwrap().push(Instant::now());
            }
            self.next.output(id, seq, bytes)
        }
        fn idle(&self, id: &str) {
            self.next.idle(id);
        }
        fn exited(&self, id: &str, code: Option<i64>) {
            self.next.exited(id, code);
        }
        fn gap(&self, id: &str, s: u64) {
            self.next.gap(id, s);
        }
    }
    let client = super::host::held::Held::new().connect();
    let at = Arc::new(StdMutex::new(Vec::new()));
    let arrivals_slot: Arc<StdMutex<Option<Arc<Arrivals>>>> = Arc::new(StdMutex::new(None));
    struct Wrap(
        Arc<HolderClient>,
        Arc<StdMutex<Vec<Instant>>>,
        Arc<StdMutex<Option<Arc<Arrivals>>>>,
    );
    impl PtyHost for Wrap {
        fn set_sink(&self, sink: Arc<dyn HostEvents>) {
            let a = Arc::new(Arrivals {
                next: sink,
                ticker: StdMutex::new(None),
                at: Arc::clone(&self.1),
            });
            *self.2.lock().unwrap() = Some(Arc::clone(&a));
            self.0.set_sink(a);
        }
        fn spawn(&self, req: SpawnReq) -> Result<Spawned, BackendError> {
            self.0.spawn(req)
        }
        fn try_write(&self, id: &str, bytes: &[u8]) -> io::Result<usize> {
            self.0.try_write(id, bytes)
        }
        fn wait_writable(&self, id: &str, t: Duration) -> io::Result<bool> {
            self.0.wait_writable(id, t)
        }
        fn resize(&self, id: &str, c: u16, r: u16) -> io::Result<()> {
            self.0.resize(id, c, r)
        }
        fn signal(&self, id: &str, s: i32) -> io::Result<()> {
            self.0.signal(id, s)
        }
        fn kill(&self, id: &str, g: Duration) -> Option<KillOutcome> {
            self.0.kill(id, g)
        }
        fn fg_pgrp(&self, id: &str) -> Option<i32> {
            self.0.fg_pgrp(id)
        }
        fn list(&self) -> Vec<HostPane> {
            self.0.list()
        }
        fn forget(&self, id: &str) {
            self.0.forget(id);
        }
        fn end_all(&self) {
            self.0.end_all();
        }
        fn attach(&self, id: &str) -> io::Result<Attached> {
            self.0.attach(id)
        }
        fn meta_set(&self, id: &str, b: &[u8]) -> io::Result<()> {
            self.0.meta_set(id, b)
        }
        fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
            self.0.meta_get(id)
        }
        fn survives_restart(&self) -> bool {
            true
        }
    }
    let b = NativeBackend::with_host_meta(
        NativeConfig::default(),
        Box::new(|key| std::env::var_os(key)),
        Arc::new(Wrap(
            Arc::clone(&client),
            Arc::clone(&at),
            Arc::clone(&arrivals_slot),
        )),
        true,
    );
    let dir = TempDir::new("measure-hol");
    let writer_pane = b.run(&spec("a", "cat > /dev/null", &dir)).unwrap();
    let ticker = b
        .run(&spec("b", "while :; do printf t; sleep 0.005; done", &dir))
        .unwrap();
    *arrivals_slot
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .ticker
        .lock()
        .unwrap() = Some(ticker.id.clone());
    let gaps = |at: &StdMutex<Vec<Instant>>| -> Vec<Duration> {
        let v = at.lock().unwrap();
        v.windows(2).map(|w| w[1] - w[0]).collect()
    };
    thread::sleep(Duration::from_secs(1));
    at.lock().unwrap().clear();
    thread::sleep(Duration::from_secs(4));
    report("ticker event spacing, nobody writing", gaps(&at));

    // A writer on the other pane at three rates; 0 means as fast as it can.
    for every in [
        Duration::from_millis(10),
        Duration::from_millis(1),
        Duration::ZERO,
    ] {
        at.lock().unwrap().clear();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let b = b.clone();
            let t = writer_pane.reference();
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let _ = b.keys(&t, &[Key::Enter], &InputGuard::default());
                    n += 1;
                    if !every.is_zero() {
                        thread::sleep(every);
                    }
                }
                n
            })
        };
        thread::sleep(Duration::from_secs(4));
        stop.store(true, Ordering::Relaxed);
        let sends = writer.join().unwrap();
        report(
            &format!("ticker event spacing, a writer on the other pane every {every:?} ({sends} sends in 4 s)"),
            gaps(&at),
        );
    }
    b.shutdown();
}
