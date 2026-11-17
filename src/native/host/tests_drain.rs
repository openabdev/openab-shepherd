//! The exit drain (issue #87): after the leader exits, `exited` waits for
//! output on its way to the sink, up to a hard cap, and no longer than the
//! idle bound when nothing is on its way.

use super::contract::{Recorder, Rig};
use super::local::{LocalHost, EXIT_DRAIN, EXIT_DRAIN_CAP};
use super::HostEvents;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A sink that takes its time: `each` per chunk while `budget` lasts, and
/// `last` for the chunk that carries the end marker, which the program prints
/// last, so the leader has exited, or is about to, while the sink still holds
/// the end of the output. The chunk reaches the recorder after the wait, as a
/// sink that does slow work before it keeps a chunk would.
struct Stalling {
    inner: Arc<Recorder>,
    last: Duration,
    each: Duration,
    budget: Mutex<Duration>,
}

impl HostEvents for Stalling {
    fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool {
        let wait = if bytes.windows(10).any(|w| w == b"end-marker") {
            self.last
        } else {
            let mut budget = self.budget.lock().unwrap();
            let wait = self.each.min(*budget);
            *budget -= wait;
            wait
        };
        std::thread::sleep(wait);
        self.inner.output(id, seq, bytes)
    }
    fn idle(&self, id: &str) {
        self.inner.idle(id);
    }
    fn exited(&self, id: &str, code: Option<i64>) {
        self.inner.exited(id, code);
    }
    fn gap(&self, id: &str, new_start: u64) {
        self.inner.gap(id, new_start);
    }
}

/// A sink that holds the first chunk until released.
struct Wedged {
    inner: Arc<Recorder>,
    open: Mutex<bool>,
    changed: Condvar,
}

impl Wedged {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

impl HostEvents for Wedged {
    fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.changed.wait(open).unwrap();
        }
        drop(open);
        self.inner.output(id, seq, bytes)
    }
    fn idle(&self, id: &str) {
        self.inner.idle(id);
    }
    fn exited(&self, id: &str, code: Option<i64>) {
        self.inner.exited(id, code);
    }
    fn gap(&self, id: &str, new_start: u64) {
        self.inner.gap(id, new_start);
    }
}

fn rig(sink: Arc<dyn HostEvents>, rec: Arc<Recorder>) -> Rig {
    let host = Arc::new(LocalHost::new());
    super::PtyHost::set_sink(&*host, sink);
    Rig {
        host,
        sink: rec,
        dir: crate::testutil::TempDir::new("drain"),
    }
}

/// A slow sink gets its output before `exited`, though it takes longer than
/// the idle bound: the last bytes of a burst are not left behind the exit.
#[test]
fn a_stalling_sink_gets_every_byte_before_exited() {
    let rec = Arc::new(Recorder::default());
    let sink = Arc::new(Stalling {
        inner: Arc::clone(&rec),
        last: EXIT_DRAIN + Duration::from_millis(300),
        each: Duration::from_millis(100),
        budget: Mutex::new(Duration::from_secs(1)),
    });
    let rig = rig(sink, rec);
    // 128 KiB, then the end marker, and the leader exits at once.
    let pane = rig.sh("head -c 131072 /dev/zero | tr '\\000' x; echo; echo end-marker");
    assert_eq!(rig.sink.wait_exit(&pane.id), Some(0));
    let ev = rig.sink.events.lock().unwrap();
    assert!(ev.violations.is_empty(), "{:?}", ev.violations);
    drop(ev);
    let text = rig.sink.text(&pane.id);
    assert!(text.contains("end-marker"), "the last output was lost");
    assert_eq!(text.matches('x').count(), 131072);
}

/// A sink that never returns costs the cap, not more: `exited` comes at
/// `EXIT_DRAIN_CAP` after the exit, with the output still held.
#[test]
fn a_wedged_sink_does_not_hold_exited_past_the_cap() {
    let rec = Arc::new(Recorder::default());
    let sink = Arc::new(Wedged {
        inner: Arc::clone(&rec),
        open: Mutex::new(false),
        changed: Condvar::new(),
    });
    let rig = rig(Arc::clone(&sink) as Arc<dyn HostEvents>, rec);
    let start = Instant::now();
    let pane = rig.sh("echo held-output");
    assert_eq!(rig.sink.wait_exit(&pane.id), Some(0));
    let took = start.elapsed();
    sink.release();
    assert!(
        took >= EXIT_DRAIN_CAP - Duration::from_millis(250),
        "exited after {took:?}, before the cap while output was pending"
    );
    assert!(
        took < EXIT_DRAIN_CAP + Duration::from_secs(5),
        "exited after {took:?}, long past the cap"
    );
    // Released, the held chunk is still delivered.
    rig.sink.wait_text(&pane.id, "held-output");
}

/// A descendant that keeps the terminal open and writes nothing does not
/// hold `exited`: with no output on its way it comes after the idle bound
/// (about 500 ms where the terminal stays open, at once where the leader's
/// exit hangs it up, as on Linux), never at the cap.
#[test]
fn a_silent_descendant_costs_the_idle_bound_not_the_cap() {
    let rec = Arc::new(Recorder::default());
    let rig = rig(Arc::clone(&rec) as Arc<dyn HostEvents>, rec);
    let start = Instant::now();
    let pane = rig.sh("echo leader-output; sleep 30 & exit 0");
    assert_eq!(rig.sink.wait_exit(&pane.id), Some(0));
    let took = start.elapsed();
    assert!(
        took < EXIT_DRAIN_CAP - Duration::from_secs(1),
        "exited after {took:?}: a silent descendant was waited for like output"
    );
    assert!(rig.sink.text(&pane.id).contains("leader-output"));
}
