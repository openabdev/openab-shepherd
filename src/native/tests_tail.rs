//! Issue #76, PR2: what the host keeps beside a pane (tail, metadata) and the
//! front's one writer of that metadata. The host-side scenarios every host
//! must pass are in `host/contract.rs`; these are the ones that need the
//! native backend or reach into `LocalHost`.

use super::host::local::LocalHost;
use super::host::{Attached, HostPane, KillOutcome, Spawned, META_CAP};
use super::meta::{load, RoutineMeta, META_VERSION, ROUTINES_BLOB_CAP};
use super::*;
use crate::registry::{RoutineRecord, RoutineSet};
use crate::testutil::TempDir;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const LONG: Duration = Duration::from_secs(15);

/// A [`LocalHost`] that counts the `meta_set` calls it gets.
struct Counting {
    inner: LocalHost,
    meta_sets: AtomicUsize,
}

impl Counting {
    fn new(inner: LocalHost) -> Arc<Self> {
        Arc::new(Self {
            inner,
            meta_sets: AtomicUsize::new(0),
        })
    }

    fn sets(&self) -> usize {
        self.meta_sets.load(Ordering::SeqCst)
    }
}

impl PtyHost for Counting {
    fn set_sink(&self, sink: Arc<dyn HostEvents>) {
        self.inner.set_sink(sink);
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
        self.meta_sets.fetch_add(1, Ordering::SeqCst);
        self.inner.meta_set(id, bytes)
    }
    fn meta_get(&self, id: &str) -> io::Result<Vec<u8>> {
        self.inner.meta_get(id)
    }
    fn survives_restart(&self) -> bool {
        self.inner.survives_restart()
    }
}

fn front(host: &Arc<Counting>, keep_meta: bool) -> NativeBackend {
    NativeBackend::with_host_meta(
        NativeConfig::default(),
        Box::new(|key| std::env::var_os(key)),
        Arc::clone(host),
        keep_meta,
    )
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

#[test]
fn the_single_process_host_keeps_no_tail() {
    let host = LocalHost::new();
    let rig = host_rig(&host);
    let err = host.attach(&rig).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
    // Its metadata still works: it is a map write, not a per-chunk cost.
    host.meta_set(&rig, b"x").unwrap();
    host.end_all();
}

/// A pane on `host` with a sink that discards everything.
fn host_rig(host: &LocalHost) -> String {
    struct Discard;
    impl HostEvents for Discard {
        fn output(&self, _: &str, _: u64, _: &[u8]) -> bool {
            true
        }
        fn idle(&self, _: &str) {}
        fn exited(&self, _: &str, _: Option<i64>) {}
        fn gap(&self, _: &str, _: u64) {}
    }
    host.set_sink(Arc::new(Discard));
    let dir = TempDir::new("tail-rig");
    let env = pty::child_env(|key| std::env::var_os(key), &[]).unwrap();
    host.spawn(SpawnReq {
        argv: vec!["sleep".into(), "30".into()],
        cwd: dir.path().to_string_lossy().into_owned(),
        env,
    })
    .unwrap()
    .id
}

#[test]
fn a_host_with_a_tail_forgets_the_oldest_finished_pane_past_the_backstop() {
    let host = LocalHost::with_tail(4096).with_finished_backstop(2);
    struct Exits(AtomicUsize);
    impl HostEvents for Exits {
        fn output(&self, _: &str, _: u64, _: &[u8]) -> bool {
            true
        }
        fn idle(&self, _: &str) {}
        fn exited(&self, _: &str, _: Option<i64>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn gap(&self, _: &str, _: u64) {}
    }
    let exits = Arc::new(Exits(AtomicUsize::new(0)));
    host.set_sink(Arc::clone(&exits) as Arc<dyn HostEvents>);
    let dir = TempDir::new("tail-backstop");
    let spawn = |argv: &[&str]| {
        let env = pty::child_env(|key| std::env::var_os(key), &[]).unwrap();
        host.spawn(SpawnReq {
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            cwd: dir.path().to_string_lossy().into_owned(),
            env,
        })
        .unwrap()
    };
    let finished: Vec<String> = (0..3)
        .map(|n| {
            let pane = spawn(&["true"]);
            eventually("a pane to exit", || {
                (exits.0.load(Ordering::SeqCst) == n + 1).then_some(())
            });
            pane.id
        })
        .collect();
    // Nothing is forgotten until a spawn needs the room.
    assert_eq!(host.list().len(), 3);
    let live = spawn(&["sleep", "30"]);
    let ids: Vec<String> = host.list().into_iter().map(|p| p.id).collect();
    assert_eq!(ids, [finished[1].clone(), finished[2].clone(), live.id]);
    host.end_all();
}

#[test]
fn a_host_without_a_tail_keeps_every_finished_pane() {
    let host = LocalHost::new().with_finished_backstop(1);
    let first = host_rig(&host);
    host.kill(&first, Duration::ZERO).unwrap();
    // No backstop applies without a tail: the single-process host leaves the
    // finished-pane policy to the front, as before.
    struct Exits(AtomicUsize);
    impl HostEvents for Exits {
        fn output(&self, _: &str, _: u64, _: &[u8]) -> bool {
            true
        }
        fn idle(&self, _: &str) {}
        fn exited(&self, _: &str, _: Option<i64>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn gap(&self, _: &str, _: u64) {}
    }
    let exits = Arc::new(Exits(AtomicUsize::new(0)));
    host.set_sink(Arc::clone(&exits) as Arc<dyn HostEvents>);
    let dir = TempDir::new("no-backstop");
    for n in 0..3 {
        let env = pty::child_env(|key| std::env::var_os(key), &[]).unwrap();
        host.spawn(SpawnReq {
            argv: vec!["true".into()],
            cwd: dir.path().to_string_lossy().into_owned(),
            env,
        })
        .unwrap();
        eventually("a pane to exit", || {
            (exits.0.load(Ordering::SeqCst) == n + 1).then_some(())
        });
    }
    let env = pty::child_env(|key| std::env::var_os(key), &[]).unwrap();
    host.spawn(SpawnReq {
        argv: vec!["sleep".into(), "30".into()],
        cwd: dir.path().to_string_lossy().into_owned(),
        env,
    })
    .unwrap();
    assert_eq!(host.list().len(), 4);
    host.end_all();
}

#[test]
fn the_front_writes_no_meta_unless_asked() {
    let host = Counting::new(LocalHost::with_tail(4096));
    let b = front(&host, false);
    let dir = TempDir::new("no-meta");
    let info = b
        .run(&sh(
            "p",
            "printf '\\033]7501;state=working\\007\\033]7501;state=idle\\007'; sleep 30",
            &dir,
        ))
        .unwrap();
    eventually("the turn", || {
        (b.get(&info.reference()).unwrap().turn == Some(1)).then_some(())
    });
    assert_eq!(host.sets(), 0);
    b.shutdown();
}

#[test]
fn the_front_writes_meta_at_spawn_and_when_the_tracker_changes() {
    let host = Counting::new(LocalHost::with_tail(4096));
    let b = front(&host, true);
    let dir = TempDir::new("meta-tracker");
    let info = b
        .run(&sh(
            "p",
            "printf '\\033]7501;state=working\\007'; sleep 1; printf '\\033]7501;state=idle\\007'; sleep 30",
            &dir,
        ))
        .unwrap();
    // At spawn: name, argv, cwd; no tracker yet.
    let first = load(host.as_ref(), &info.id).expect("meta at spawn");
    assert_eq!(first.name.as_deref(), Some("p"));
    assert_eq!(first.argv.len(), 3);
    assert_eq!(first.cwd, info.cwd.clone().unwrap());

    let meta = eventually("the turn in meta", || {
        let meta = load(host.as_ref(), &info.id)?;
        let snapshot = meta.tracker.as_ref()?;
        (snapshot.to_json()["turn"] == 1).then_some(meta)
    });
    let at = host.attach(&info.id).unwrap();
    assert_eq!(at.tail_start, 0);
    // The program printed nothing after the idle report, so the snapshot is
    // exact at the end of the tail, and both reports are before it.
    assert_eq!(meta.tracker_seq, at.tail_end);
    let before = &at.bytes[..meta.tracker_seq as usize];
    let text = String::from_utf8_lossy(before);
    assert!(text.contains("state=working") && text.contains("state=idle"));
    assert_eq!(meta.tracker.unwrap().to_json()["state"], "idle");
    b.shutdown();
}

#[test]
fn a_chunk_that_changes_nothing_writes_nothing() {
    let host = Counting::new(LocalHost::with_tail(4096));
    let b = front(&host, true);
    let dir = TempDir::new("meta-quiet");
    let info = b
        .run(&sh("p", "yes 0123456789 | head -c 300000; sleep 30", &dir))
        .unwrap();
    // Wait for the stream to land in the tail, then let the ingest finish.
    eventually("the output", || {
        (host.attach(&info.id).ok()?.tail_end > 300_000).then_some(())
    });
    thread::sleep(Duration::from_millis(300));
    assert_eq!(host.sets(), 1, "only the write at spawn");
    b.shutdown();
}

#[test]
fn meta_that_cannot_be_read_is_adopted_without_it() {
    let host = LocalHost::new();
    let id = host_rig(&host);
    assert!(load(&host, &id).is_none(), "nothing set yet");
    let good = PaneMeta {
        cwd: "/x".into(),
        ..PaneMeta::default()
    };
    host.meta_set(&id, &good.to_bytes()).unwrap();
    assert_eq!(load(&host, &id), Some(good));
    for bad in [
        b"not json".to_vec(),
        br#"{"v": 2}"#.to_vec(),
        br#"{"name": null}"#.to_vec(),
        br#"{"v": 1, "name": 5}"#.to_vec(),
        br#"[1, 2]"#.to_vec(),
    ] {
        host.meta_set(&id, &bad).unwrap();
        assert!(
            load(&host, &id).is_none(),
            "{}",
            String::from_utf8_lossy(&bad)
        );
    }
    assert!(PaneMeta::from_bytes(br#"{"v": 2}"#)
        .unwrap_err()
        .contains("version 2"));
    host.end_all();
}

#[test]
fn every_meta_field_round_trips() {
    let mut tracker = Tracker::default();
    tracker.apply(State::Blocked, Some("permission".into()), 42);
    let meta = PaneMeta {
        name: Some("worker".into()),
        labels: vec![("team".into(), "a".into())],
        argv: vec!["kiro-cli".into(), "chat".into()],
        cwd: "/work".into(),
        agent: Some("kiro".into()),
        profile: Some("kiro-default".into()),
        tracker: Some(tracker.snapshot()),
        tracker_seq: 123_456_789_012,
        human_draft: true,
        routine: Some(RoutineMeta {
            consecutive: 2,
            gave_up: true,
            nudge_allowance: 3,
        }),
        routines: Default::default(),
        answered: true,
    };
    let back = PaneMeta::from_bytes(&meta.to_bytes()).unwrap();
    assert_eq!(back, meta);
    let empty = PaneMeta::default();
    assert_eq!(PaneMeta::from_bytes(&empty.to_bytes()).unwrap(), empty);
    let v: serde_json::Value = serde_json::from_slice(&meta.to_bytes()).unwrap();
    assert_eq!(v["v"], META_VERSION);
}

#[test]
fn concurrent_meta_writers_never_lose_a_field() {
    const ROUNDS: u32 = 400;
    let host = Counting::new(LocalHost::with_tail(4096));
    let id = host_rig(&host.inner);
    let store = Arc::new(MetaStore::new(
        Arc::clone(&host) as Arc<dyn PtyHost>,
        id.clone(),
        PaneMeta::default(),
    ));
    let draft_acked = Arc::new(AtomicBool::new(false));
    let human = {
        let (store, flag) = (Arc::clone(&store), Arc::clone(&draft_acked));
        thread::spawn(move || {
            // Let the other writer get going first.
            thread::sleep(Duration::from_millis(2));
            store.set_human_draft().unwrap();
            flag.store(true, Ordering::SeqCst);
        })
    };
    let routine = {
        let (store, flag, host) = (
            Arc::clone(&store),
            Arc::clone(&draft_acked),
            Arc::clone(&host),
        );
        let id = id.clone();
        thread::spawn(move || {
            for n in 1..=ROUNDS {
                let acked_before = flag.load(Ordering::SeqCst);
                store
                    .update(|m| {
                        m.routine = Some(RoutineMeta {
                            consecutive: n,
                            ..RoutineMeta::default()
                        });
                        m.tracker_seq = u64::from(n);
                    })
                    .unwrap();
                if acked_before {
                    // Once the draft was acknowledged, no later write may
                    // carry the flag cleared.
                    let blob = PaneMeta::from_bytes(&host.meta_get(&id).unwrap()).unwrap();
                    assert!(blob.human_draft, "round {n} lost the draft flag");
                }
                thread::yield_now();
            }
        })
    };
    human.join().unwrap();
    routine.join().unwrap();
    let last = PaneMeta::from_bytes(&host.meta_get(&id).unwrap()).unwrap();
    assert!(last.human_draft);
    assert_eq!(last.routine.as_ref().unwrap().consecutive, ROUNDS);
    assert_eq!(last.tracker_seq, u64::from(ROUNDS));
    assert_eq!(last, store.get());
    host.end_all();
}

#[test]
fn a_meta_write_the_host_refuses_stays_in_memory_for_the_next() {
    let host = Counting::new(LocalHost::with_tail(4096));
    let id = host_rig(&host.inner);
    let store = MetaStore::new(
        Arc::clone(&host) as Arc<dyn PtyHost>,
        id.clone(),
        PaneMeta::default(),
    );
    store.update(|m| m.cwd = "/ok".into()).unwrap();
    let err = store
        .update(|m| m.argv = vec!["x".repeat(META_CAP)])
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    // The host still has the last good blob.
    let kept = PaneMeta::from_bytes(&host.meta_get(&id).unwrap()).unwrap();
    assert_eq!(kept.cwd, "/ok");
    assert!(kept.argv.is_empty());
    // The refused change stays in memory; the next write replaces it whole.
    assert_eq!(store.get().argv[0].len(), META_CAP);
    store.update(|m| m.argv = vec!["y".into()]).unwrap();
    assert_eq!(store.get().argv, ["y"]);
    host.end_all();
}

#[test]
fn the_default_tail_is_one_mebibyte() {
    use super::host::tail::TAIL_CAP;
    assert_eq!(TAIL_CAP, 1024 * 1024);
    let host = Arc::new(Counting::new(LocalHost::with_tail(TAIL_CAP)));
    let b = front(&host, true);
    let dir = TempDir::new("tail-1mib");
    // About 1.1 MiB, in pieces, ending in a marker.
    let info = b
        .run(&sh(
            "big",
            "yes 0123456789abcdef | head -c 1153434; printf END-MARK; sleep 30",
            &dir,
        ))
        .unwrap();
    let at = eventually("the output", || {
        thread::sleep(Duration::from_millis(40));
        let at = host.attach(&info.id).ok()?;
        String::from_utf8_lossy(&at.bytes)
            .ends_with("END-MARK")
            .then_some(at)
    });
    assert_eq!(at.bytes.len(), TAIL_CAP);
    assert_eq!(at.tail_end - at.tail_start, TAIL_CAP as u64);
    assert!(at.tail_start > 0, "1.1 MiB of output wrapped a 1 MiB tail");
    b.shutdown();
}

// ---- issue #98, PR2: the routines key in the meta ----

fn routine_rec(n: u64, text: &str) -> RoutineRecord {
    RoutineRecord {
        id: format!("r{n}"),
        every_secs: 600,
        text: text.to_string(),
        todo_file: Some("a/relative/path/to/TODO.md".to_string()),
        created_by: "agent-a".to_string(),
        created_at: 1_000,
        expires_at: 100_000,
        max_runs: Some(7),
        status: "active".to_string(),
        stop_reason: None,
        runs: 1,
        fail_streak: 0,
        last_run_at: Some(1_600),
        last_result: Some("sent".to_string()),
        last_error: None,
        next_due: 2_200,
    }
}

fn meta_with_tracker() -> PaneMeta {
    let mut tracker = Tracker::default();
    tracker.apply(State::Blocked, Some("permission".into()), 42);
    PaneMeta {
        name: Some("worker".into()),
        cwd: "/work".into(),
        agent: Some("kiro".into()),
        profile: Some("kiro-default".into()),
        tracker: Some(tracker.snapshot()),
        tracker_seq: 99,
        human_draft: true,
        ..PaneMeta::default()
    }
}

#[test]
fn routines_round_trip_in_the_meta_and_the_version_stays_1() {
    let mut meta = meta_with_tracker();
    meta.routines = RoutineSet {
        seq: 3,
        items: vec![routine_rec(1, "check"), routine_rec(3, "again")],
    };
    let back = PaneMeta::from_bytes(&meta.to_bytes()).unwrap();
    assert_eq!(back, meta);
    let v: serde_json::Value = serde_json::from_slice(&meta.to_bytes()).unwrap();
    assert_eq!(v["v"], 1);
    assert_eq!(v["routines"]["v"], 1);
    assert_eq!(v["routines"]["seq"], 3);
    // No routines, no key: an older build reads the same bytes as before.
    let none: serde_json::Value = serde_json::from_slice(&meta_with_tracker().to_bytes()).unwrap();
    assert!(none.get("routines").is_none());
}

#[test]
fn a_blob_written_before_routines_existed_still_loads() {
    let old = br#"{"v":1,"name":"w","labels":[],"argv":["kiro-cli"],"cwd":"/w",
        "agent":"kiro","profile":"kiro","tracker":null,"tracker_seq":0,
        "human_draft":false,"routine":null,"answered":false}"#;
    let meta = PaneMeta::from_bytes(old).unwrap();
    assert_eq!(meta.agent.as_deref(), Some("kiro"));
    assert!(meta.routines.is_empty());
    assert_eq!(meta.routines.seq, 0);
}

#[test]
fn a_malformed_routines_value_is_empty_and_keeps_the_rest_of_the_meta() {
    let base = meta_with_tracker();
    for bad in [
        serde_json::json!("garbage"),
        serde_json::json!(12),
        serde_json::json!([1, 2]),
        serde_json::json!({"v": 1}),
        serde_json::json!({"v": 7, "seq": 1, "items": []}),
        serde_json::json!({"v": 1, "seq": "x", "items": {"a": 1}}),
    ] {
        let mut blob: serde_json::Value = serde_json::from_slice(&base.to_bytes()).unwrap();
        blob["routines"] = bad.clone();
        let (meta, notes) = PaneMeta::from_bytes_with_notes(blob.to_string().as_bytes())
            .unwrap_or_else(|e| {
                panic!("{bad}: the whole meta was refused: {e}");
            });
        assert!(meta.routines.is_empty(), "{bad}");
        assert!(!notes.is_empty(), "{bad}: nothing was logged");
        // The tracker and the human-draft flag survive.
        assert_eq!(meta.tracker, base.tracker, "{bad}");
        assert!(meta.human_draft, "{bad}");
        assert_eq!(meta.agent, base.agent, "{bad}");
    }
}

#[test]
fn a_bad_routine_item_is_dropped_and_the_others_and_the_tracker_are_kept() {
    let base = meta_with_tracker();
    let mut blob: serde_json::Value = serde_json::from_slice(&base.to_bytes()).unwrap();
    let mut broken = routine_rec(2, "second").to_json();
    broken["every_secs"] = serde_json::json!("often");
    broken["trigger"] = serde_json::json!({"type": "every"});
    blob["routines"] = serde_json::json!({"v": 1, "seq": 3, "items": [
        routine_rec(1, "first").to_json(), broken, routine_rec(3, "third").to_json()]});
    let (meta, notes) = PaneMeta::from_bytes_with_notes(blob.to_string().as_bytes()).unwrap();
    let ids: Vec<&str> = meta.routines.items.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["r1", "r3"]);
    assert_eq!(meta.routines.seq, 3);
    assert_eq!(notes.len(), 1);
    assert_eq!(meta.tracker, base.tracker);
}

#[test]
fn an_unknown_future_key_is_ignored() {
    let mut blob: serde_json::Value =
        serde_json::from_slice(&meta_with_tracker().to_bytes()).unwrap();
    blob["something_later"] = serde_json::json!({"x": [1, 2, 3]});
    blob["routines"] = serde_json::json!({"v": 1, "seq": 1, "items": [],
        "later_field": true});
    let meta = PaneMeta::from_bytes(blob.to_string().as_bytes()).unwrap();
    assert_eq!(meta.agent.as_deref(), Some("kiro"));
    assert_eq!(meta.routines.seq, 1);
}

#[test]
fn load_reads_a_malformed_routines_key_without_losing_the_pane() {
    let host = LocalHost::new();
    let id = host_rig(&host);
    let mut blob: serde_json::Value =
        serde_json::from_slice(&meta_with_tracker().to_bytes()).unwrap();
    blob["routines"] = serde_json::json!("garbage");
    host.meta_set(&id, blob.to_string().as_bytes()).unwrap();
    let meta = load(&host, &id).expect("the meta is adopted");
    assert!(meta.tracker.is_some());
    assert!(meta.routines.is_empty());
    host.end_all();
}

#[test]
fn set_routines_refuses_a_blob_over_the_cap_and_changes_nothing() {
    let host = Counting::new(LocalHost::with_tail(4096));
    let id = host_rig(&host.inner);
    let store = MetaStore::new(
        Arc::clone(&host) as Arc<dyn PtyHost>,
        id.clone(),
        PaneMeta::default(),
    );
    // Ten routines of the largest text fit with room to spare.
    let max_text = "x".repeat(1024);
    let worst = RoutineSet {
        seq: 10,
        items: (1..=10).map(|n| routine_rec(n, &max_text)).collect(),
    };
    store.set_routines(&worst).unwrap();
    let stored = host.meta_get(&id).unwrap();
    assert!(stored.len() < ROUTINES_BLOB_CAP / 2, "{}", stored.len());
    assert_eq!(store.get().routines, worst);

    // A pane whose argv already fills the budget cannot take them.
    store
        .update(|m| m.argv = vec!["a".repeat(40 * 1024)])
        .unwrap();
    let before_bytes = host.meta_get(&id).unwrap();
    let sets_before = host.sets();
    let mut bigger = worst.clone();
    bigger.items.push(routine_rec(11, &max_text));
    let err = store.set_routines(&bigger).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(host.sets(), sets_before, "nothing was written");
    assert_eq!(host.meta_get(&id).unwrap(), before_bytes);
    assert_eq!(store.get().routines, worst, "memory is unchanged");
    host.end_all();
}
