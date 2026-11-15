//! The host contract: scenarios any [`PtyHost`] must pass, written against
//! the trait and a recording sink, never against a host's internals. They run
//! on [`LocalHost`] here; a host reached over a link must pass the same list.

use super::local::LocalHost;
use super::{pty, HostEvents, HostPane, PtyHost, SpawnReq};
use crate::backend::{BackendError, Capabilities, TerminalBackend};
use crate::native::{NativeBackend, NativeConfig};
use crate::testutil::TempDir;
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const LONG: Duration = Duration::from_secs(15);

/// Everything the sink was told, in order.
#[derive(Default)]
struct Events {
    /// Per pane: `(seq, bytes)` of each `output`.
    output: HashMap<String, Vec<(u64, Vec<u8>)>>,
    /// Per pane: the exit code, once `exited` came.
    exited: HashMap<String, Option<i64>>,
    /// Calls that broke the ordering promise (output after exit, twice exited).
    violations: Vec<String>,
    gaps: usize,
}

#[derive(Default)]
struct Recorder {
    events: Mutex<Events>,
    changed: Condvar,
}

impl HostEvents for Recorder {
    fn output(&self, id: &str, seq: u64, bytes: &[u8]) -> bool {
        let mut ev = self.events.lock().unwrap();
        if ev.exited.contains_key(id) {
            ev.violations.push(format!("{id}: output after exited"));
        }
        ev.output
            .entry(id.to_string())
            .or_default()
            .push((seq, bytes.to_vec()));
        self.changed.notify_all();
        true
    }

    fn idle(&self, _id: &str) {}

    fn exited(&self, id: &str, code: Option<i64>) {
        let mut ev = self.events.lock().unwrap();
        if ev.exited.insert(id.to_string(), code).is_some() {
            ev.violations.push(format!("{id}: exited twice"));
        }
        self.changed.notify_all();
    }

    fn gap(&self, _id: &str, _new_start: u64) {
        self.events.lock().unwrap().gaps += 1;
        self.changed.notify_all();
    }
}

impl Recorder {
    /// Block until `ready` holds for the events, or fail after `LONG`.
    fn wait_until(&self, what: &str, ready: impl Fn(&Events) -> bool) {
        let end = Instant::now() + LONG;
        let mut ev = self.events.lock().unwrap();
        while !ready(&ev) {
            let left = end.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            ev = self.changed.wait_timeout(ev, left).unwrap().0;
        }
    }

    fn text(&self, id: &str) -> String {
        let ev = self.events.lock().unwrap();
        let bytes: Vec<u8> = ev
            .output
            .get(id)
            .into_iter()
            .flatten()
            .flat_map(|(_, chunk)| chunk.iter().copied())
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn wait_text(&self, id: &str, needle: &str) {
        self.wait_until(&format!("{needle:?} from {id}"), |ev| {
            let all: Vec<u8> = ev
                .output
                .get(id)
                .into_iter()
                .flatten()
                .flat_map(|(_, chunk)| chunk.iter().copied())
                .collect();
            String::from_utf8_lossy(&all).contains(needle)
        });
    }

    fn wait_exit(&self, id: &str) -> Option<i64> {
        self.wait_until(&format!("{id} to exit"), |ev| ev.exited.contains_key(id));
        self.events.lock().unwrap().exited[id]
    }
}

struct Rig {
    host: Arc<dyn PtyHost>,
    sink: Arc<Recorder>,
    dir: TempDir,
}

impl Rig {
    fn new(host: Arc<dyn PtyHost>, name: &str) -> Self {
        let sink = Arc::new(Recorder::default());
        host.set_sink(Arc::clone(&sink) as Arc<dyn HostEvents>);
        Self {
            host,
            sink,
            dir: TempDir::new(name),
        }
    }

    fn spawn(&self, argv: &[&str]) -> super::Spawned {
        let env = pty::child_env(|key| std::env::var_os(key), &[]).unwrap();
        self.host
            .spawn(SpawnReq {
                argv: argv.iter().map(|s| (*s).to_string()).collect(),
                cwd: self.dir.path().to_string_lossy().into_owned(),
                env,
            })
            .unwrap()
    }

    fn sh(&self, line: &str) -> super::Spawned {
        self.spawn(&["/bin/sh", "-c", line])
    }
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn gone_within(pid: i32, within: Duration) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_alive(pid)
}

/// Output arrives with `seq` counting the offset of each chunk's first byte,
/// from 0 and without a gap, and `exited` comes after the last `output`.
fn output_is_numbered_and_ordered(rig: &Rig) {
    let pane = rig.sh("yes 0123456789 | head -c 200000; echo done-marker");
    rig.sink.wait_text(&pane.id, "done-marker");
    assert_eq!(rig.sink.wait_exit(&pane.id), Some(0));
    let ev = rig.sink.events.lock().unwrap();
    let chunks = &ev.output[&pane.id];
    let mut next = 0u64;
    for (seq, bytes) in chunks {
        assert_eq!(*seq, next, "chunk offsets must be contiguous from 0");
        next += bytes.len() as u64;
    }
    assert!(next >= 200_000, "only {next} bytes arrived");
    assert!(chunks.len() > 1, "the stream should arrive in chunks");
    assert!(ev.violations.is_empty(), "{:?}", ev.violations);
    assert_eq!(
        ev.gaps, 0,
        "a host with no replay buffer never reports a gap"
    );
}

/// Each pane counts its own offsets.
fn seq_is_per_pane(rig: &Rig) {
    let a = rig.sh("printf aaaa");
    let b = rig.sh("printf bbbbbb");
    rig.sink.wait_exit(&a.id);
    rig.sink.wait_exit(&b.id);
    let ev = rig.sink.events.lock().unwrap();
    assert_eq!(ev.output[&a.id][0].0, 0);
    assert_eq!(ev.output[&b.id][0].0, 0);
}

fn exit_status_is_reported(rig: &Rig) {
    let pane = rig.sh("exit 7");
    assert_eq!(rig.sink.wait_exit(&pane.id), Some(7));
}

fn a_signal_ends_the_pane(rig: &Rig) {
    let pane = rig.spawn(&["sleep", "30"]);
    rig.host
        .signal(&pane.id, libc::SIGTERM)
        .expect("signal reaches the group");
    // 128 + SIGTERM for a signal death.
    assert_eq!(
        rig.sink.wait_exit(&pane.id),
        Some(128 + i64::from(libc::SIGTERM))
    );
}

/// A pane that exits on SIGHUP is not "killed"; one that ignores HUP and TERM
/// is, after the grace, and the pane is forgotten either way.
fn kill_honours_the_grace(rig: &Rig) {
    let polite = rig.spawn(&["sleep", "30"]);
    let outcome = rig.host.kill(&polite.id, Duration::from_secs(5)).unwrap();
    assert!(!outcome.killed, "{outcome:?}");

    let stubborn = rig.sh("trap '' HUP TERM; echo ready; while :; do sleep 1; done");
    rig.sink.wait_text(&stubborn.id, "ready");
    let started = Instant::now();
    let outcome = rig
        .host
        .kill(&stubborn.id, Duration::from_millis(300))
        .unwrap();
    assert!(outcome.killed, "{outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        rig.sink.wait_exit(&stubborn.id),
        Some(128 + i64::from(libc::SIGKILL))
    );
    assert!(!process_alive(stubborn.pid), "the leader was not reaped");
    assert!(rig.host.list().iter().all(|p| p.id != stubborn.id));
    assert!(rig.host.kill(&stubborn.id, Duration::ZERO).is_none());
}

/// A descendant that left the pane's session is still ended by `kill`.
fn kill_ends_a_descendant_in_another_session(rig: &Rig) {
    let pane = rig.sh(
        "perl -MPOSIX -e 'POSIX::setsid() != -1 or die; print \"gc:$$\\n\"; \
         close STDIN; close STDOUT; close STDERR; sleep 60' & sleep 60",
    );
    rig.sink.wait_text(&pane.id, "gc:");
    let text = rig.sink.text(&pane.id);
    let gc: i32 = text
        .split("gc:")
        .nth(1)
        .and_then(|rest| rest.trim().lines().next())
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("no grandchild pid in {text:?}"));
    std::thread::sleep(Duration::from_millis(600));
    assert!(process_alive(gc), "the grandchild died early");
    rig.host.kill(&pane.id, Duration::from_secs(2)).unwrap();
    assert!(
        gone_within(gc, Duration::from_secs(5)),
        "{gc} outlived kill"
    );
}

fn resize_is_seen_by_the_program(rig: &Rig) {
    let pane = rig.sh("read x; stty size");
    rig.host.resize(&pane.id, 100, 30).unwrap();
    let listed: Vec<HostPane> = rig.host.list();
    let mine = listed.iter().find(|p| p.id == pane.id).unwrap();
    assert_eq!((mine.cols, mine.rows), (100, 30));
    wait_write(rig, &pane.id, b"\n");
    rig.sink.wait_text(&pane.id, "30 100");
}

fn wait_write(rig: &Rig, id: &str, bytes: &[u8]) {
    let end = Instant::now() + LONG;
    loop {
        match rig.host.try_write(id, bytes) {
            Ok(n) => {
                assert_eq!(n, bytes.len());
                return;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < end, "the pane never took input");
                let _ = rig.host.wait_writable(id, Duration::from_millis(100));
            }
            Err(err) => panic!("write failed: {err}"),
        }
    }
}

fn writes_reach_the_program(rig: &Rig) {
    let pane = rig.spawn(&["cat"]);
    assert!(rig
        .host
        .wait_writable(&pane.id, Duration::from_secs(5))
        .unwrap());
    wait_write(rig, &pane.id, b"ping\n");
    rig.sink.wait_text(&pane.id, "ping");
    let err = rig.host.try_write("n:999999", b"x").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

fn the_foreground_group_is_the_leaders(rig: &Rig) {
    let pane = rig.spawn(&["sleep", "30"]);
    let end = Instant::now() + LONG;
    while rig.host.fg_pgrp(&pane.id) != Some(pane.pid) {
        assert!(Instant::now() < end, "fg_pgrp never named the leader");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(rig.host.fg_pgrp("n:999999"), None);
}

/// Ids are minted by the host, differ per pane, and mean the same thing until
/// the pane is killed or forgotten; an exited pane stays listed.
fn ids_are_stable_and_panes_stay_listed_until_ended(rig: &Rig) {
    let first = rig.sh("exit 0");
    let second = rig.spawn(&["sleep", "30"]);
    assert_ne!(first.id, second.id);
    assert!(first.id.starts_with("n:") && second.id.starts_with("n:"));
    rig.sink.wait_exit(&first.id);
    let listed = rig.host.list();
    let ids: Vec<&str> = listed.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, [first.id.as_str(), second.id.as_str()]);
    assert!(listed[0].exited && !listed[1].exited);
    assert_eq!(listed[1].pid, second.pid);
    rig.host.forget(&first.id);
    assert!(rig.host.list().iter().all(|p| p.id != first.id));
    rig.host.end_all();
    assert!(rig.host.list().is_empty());
    assert!(gone_within(second.pid, Duration::from_secs(5)));
}

type Scenario = (&'static str, fn(&Rig));

const SCENARIOS: &[Scenario] = &[
    (
        "output is numbered and ordered",
        output_is_numbered_and_ordered,
    ),
    ("seq is per pane", seq_is_per_pane),
    ("exit status is reported", exit_status_is_reported),
    ("a signal ends the pane", a_signal_ends_the_pane),
    ("kill honours the grace", kill_honours_the_grace),
    (
        "kill ends a descendant in another session",
        kill_ends_a_descendant_in_another_session,
    ),
    (
        "resize is seen by the program",
        resize_is_seen_by_the_program,
    ),
    ("writes reach the program", writes_reach_the_program),
    (
        "the foreground group is the leader's",
        the_foreground_group_is_the_leaders,
    ),
    (
        "ids are stable and panes stay listed until ended",
        ids_are_stable_and_panes_stay_listed_until_ended,
    ),
];

/// Run every scenario on a fresh host from `make`.
fn run_contract(make: impl Fn() -> Arc<dyn PtyHost>) {
    for (name, scenario) in SCENARIOS {
        eprintln!("host contract: {name}");
        let rig = Rig::new(make(), "host-contract");
        scenario(&rig);
        rig.host.end_all();
    }
}

#[test]
fn local_host_meets_the_host_contract() {
    run_contract(|| Arc::new(LocalHost::new()));
}

#[test]
fn a_local_host_does_not_survive_a_restart() {
    assert!(!LocalHost::new().survives_restart());
}

/// A host that is not [`LocalHost`]: everything but the capability panics.
struct SurvivingHost;

impl PtyHost for SurvivingHost {
    fn set_sink(&self, _sink: Arc<dyn HostEvents>) {}
    fn spawn(&self, _req: SpawnReq) -> Result<super::Spawned, BackendError> {
        unimplemented!()
    }
    fn try_write(&self, _id: &str, _bytes: &[u8]) -> io::Result<usize> {
        unimplemented!()
    }
    fn wait_writable(&self, _id: &str, _timeout: Duration) -> io::Result<bool> {
        unimplemented!()
    }
    fn resize(&self, _id: &str, _cols: u16, _rows: u16) -> io::Result<()> {
        unimplemented!()
    }
    fn signal(&self, _id: &str, _signal: i32) -> io::Result<()> {
        unimplemented!()
    }
    fn kill(&self, _id: &str, _grace: Duration) -> Option<super::KillOutcome> {
        unimplemented!()
    }
    fn fg_pgrp(&self, _id: &str) -> Option<i32> {
        unimplemented!()
    }
    fn list(&self) -> Vec<HostPane> {
        Vec::new()
    }
    fn forget(&self, _id: &str) {}
    fn end_all(&self) {}
    fn survives_restart(&self) -> bool {
        true
    }
}

/// `survives_restart` is a property of the host, not of the native core.
#[test]
fn the_backend_reports_the_hosts_survives_restart() {
    let local = NativeBackend::new(NativeConfig::default());
    assert!(!local.capabilities().survives_restart);
    let held = NativeBackend::with_host(
        NativeConfig::default(),
        Box::new(|key| std::env::var_os(key)),
        Arc::new(SurvivingHost),
    );
    let caps: Capabilities = held.capabilities();
    assert!(caps.survives_restart);
    // Nothing else about the capabilities depends on the host.
    assert_eq!(
        Capabilities {
            survives_restart: false,
            ..caps
        },
        Capabilities {
            survives_restart: false,
            ..local.capabilities()
        }
    );
}
