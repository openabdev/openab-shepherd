//! The link itself (issue #76, PR5): the handshake, malformed input, the
//! ordering and gap promises of the events, and the ops that must not wait
//! for each other. Everything runs in process over real socketpairs with the
//! real holder and client; nothing here starts the MCP server.

use super::client::{ConnectError, HolderClient};
use super::contract::{
    self, an_exited_pane_keeps_its_tail_until_kill, attach_below_the_cap_is_complete,
    meta_is_kept_as_set, process_alive, wait_write, Recorder, Rig, LONG, TAIL_TEST_CAP,
};
use super::held::{GateHost, Held};
use super::local::LocalHost;
use super::server::{Holder, ServeError};
use super::wire::{
    greeting, hello, read_frame, write_frame, Frame, Greeting, WireError, HOLDER_VERSION, MAX_FRAME,
};
use super::{HolderHost, HostEvents, PtyHost, SpawnReq};
use crate::testutil::TempDir;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

fn as_host(client: &Arc<HolderClient>) -> Arc<dyn PtyHost> {
    Arc::clone(client) as Arc<dyn PtyHost>
}

fn eventually(what: &str, mut probe: impl FnMut() -> bool) {
    let end = Instant::now() + LONG;
    while !probe() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

// ---------------------------------------------------------------------------
// The host contract, over the link
// ---------------------------------------------------------------------------

#[test]
fn a_holder_client_meets_the_host_contract() {
    contract::run_contract(|| as_host(&Held::new().connect()));
}

/// The tail scenarios that do not depend on the sink hearing every byte (a
/// link with a small ring may skip bytes by design). The others are covered
/// below against an oracle that does not use the sink.
#[test]
fn a_holder_client_meets_the_meta_and_retention_scenarios() {
    for scenario in [
        attach_below_the_cap_is_complete as fn(&Rig),
        an_exited_pane_keeps_its_tail_until_kill,
        meta_is_kept_as_set,
    ] {
        let rig = Rig::new(
            as_host(&Held::with_tail(TAIL_TEST_CAP).connect()),
            "link-tail",
        );
        scenario(&rig);
        rig.host.end_all();
    }
}

#[test]
fn only_the_client_survives_a_restart() {
    let client = Held::new().connect();
    assert!(client.survives_restart());
    assert!(!LocalHost::new().survives_restart());
}

// ---------------------------------------------------------------------------
// Attach over the wire, against an oracle that does not use the sink
// ---------------------------------------------------------------------------

/// The stream the panes below print: `a` to `z` over and over, with no
/// newline (the tty line discipline rewrites newlines, and on a loaded macOS
/// host it was seen to emit `\r\r\n`, which says nothing about the ring).
fn stream_byte(offset: u64) -> u8 {
    b'a' + (offset % 26) as u8
}

const ALPHABET_STREAM: &str = "yes abcdefghijklmnopqrstuvwxyz | tr -d '\\n' | head -c";

fn assert_is_stream(start: u64, bytes: &[u8]) {
    for (i, b) in bytes.iter().enumerate() {
        assert_eq!(
            *b,
            stream_byte(start + i as u64),
            "offset {} (index {i} of {} from start {start}); around: {:?}",
            start + i as u64,
            bytes.len(),
            String::from_utf8_lossy(&bytes[i.saturating_sub(12)..(i + 12).min(bytes.len())])
        );
    }
}

#[test]
fn attach_after_a_wrap_returns_the_last_cap_bytes_with_exact_offsets() {
    let held = Held::with_tail(TAIL_TEST_CAP);
    let client = held.connect();
    let rig = Rig::new(as_host(&client), "link-wrap");
    let pane = rig.sh(&format!("{ALPHABET_STREAM} 100000"));
    rig.sink.wait_exit(&pane.id);
    let at = client.attach(&pane.id).unwrap();
    assert_eq!(at.tail_end - at.tail_start, TAIL_TEST_CAP as u64);
    assert_eq!(at.bytes.len(), TAIL_TEST_CAP);
    assert!(at.tail_start > 0);
    assert_is_stream(at.tail_start, &at.bytes);
    // The holder's own view agrees.
    let local = held.local.attach(&pane.id).unwrap();
    assert_eq!(local, at);
}

#[test]
fn attach_under_output_is_a_consistent_cut_over_the_wire() {
    let held = Held::with_tail(TAIL_TEST_CAP);
    let client = held.connect();
    let rig = Rig::new(as_host(&client), "link-atomic");
    let pane = rig.sh(&format!("{ALPHABET_STREAM} 3000000"));
    let mut cuts = Vec::new();
    while !rig
        .sink
        .events
        .lock()
        .unwrap()
        .exited
        .contains_key(&pane.id)
    {
        if let Ok(at) = client.attach(&pane.id) {
            cuts.push(at);
        }
    }
    assert!(cuts.len() > 5, "only {} attaches overlapped", cuts.len());
    for at in cuts {
        assert_eq!(at.tail_end - at.tail_start, at.bytes.len() as u64);
        assert!(at.bytes.len() <= TAIL_TEST_CAP);
        assert_is_stream(at.tail_start, &at.bytes);
    }
}

// ---------------------------------------------------------------------------
// The handshake
// ---------------------------------------------------------------------------

fn pair() -> (UnixStream, UnixStream) {
    UnixStream::pair().expect("socketpair")
}

#[test]
fn a_front_with_another_version_is_refused_and_both_ends_say_so() {
    let local = Arc::new(LocalHost::with_tail(4096));
    let (holder, log) = Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>).capture_log();
    let held = Held::with_holder(local, Arc::new(holder));
    let (ops, events) = held.serve_pair();
    let err = HolderClient::connect_as(ops, events, HOLDER_VERSION + 1).err();
    assert!(
        matches!(
            err,
            Some(ConnectError::VersionMismatch {
                holder: 1,
                front: 2
            })
        ),
        "{err:?}"
    );
    let ended = held.finish_servers();
    assert!(
        matches!(
            ended.as_slice(),
            [Err(ServeError::VersionMismatch {
                holder: 1,
                front: 2
            })]
        ),
        "{ended:?}"
    );
    let log = log.lock().unwrap().join("\n");
    assert!(log.contains("holder protocol 1, front 2"), "{log}");
}

#[test]
fn a_holder_with_another_version_is_refused_by_the_front() {
    let local = Arc::new(LocalHost::with_tail(4096));
    let holder = Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>).with_version(2);
    let held = Held::with_holder(local, Arc::new(holder));
    let (ops, events) = held.serve_pair();
    let err = HolderClient::connect(ops, events).err();
    assert!(
        matches!(
            err,
            Some(ConnectError::VersionMismatch {
                holder: 2,
                front: 1
            })
        ),
        "{err:?}"
    );
    let ended = held.finish_servers();
    assert!(
        matches!(
            ended.as_slice(),
            [Err(ServeError::VersionMismatch {
                holder: 2,
                front: 1
            })]
        ),
        "{ended:?}"
    );
}

#[test]
fn equal_versions_other_than_the_default_connect() {
    let local = Arc::new(LocalHost::with_tail(4096));
    let holder = Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>).with_version(77);
    let held = Held::with_holder(local, Arc::new(holder));
    let (ops, events) = held.serve_pair();
    let client = HolderClient::connect_as(ops, events, 77).expect("equal versions");
    assert!(client.list().is_empty());
    assert!(!client.is_closed());
}

// ---------------------------------------------------------------------------
// Malformed input, from a front that is a raw socket
// ---------------------------------------------------------------------------

struct Raw {
    ops: UnixStream,
    events: UnixStream,
    server: JoinHandle<Result<(), ServeError>>,
    log: Arc<Mutex<Vec<String>>>,
}

fn raw_link() -> (Raw, Arc<Held>) {
    let local = Arc::new(LocalHost::with_tail(4096));
    let (holder, log) = Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>).capture_log();
    let holder = Arc::new(holder);
    let held = Held::with_holder(local, Arc::clone(&holder));
    let (mut ops, holder_ops) = pair();
    let (mut events, holder_events) = pair();
    let server = thread::spawn(move || holder.serve(holder_ops, holder_events));
    handshake(&mut ops, &mut events);
    (
        Raw {
            ops,
            events,
            server,
            log,
        },
        held,
    )
}

/// A front's side of the greeting, as a raw socket does it.
fn handshake(ops: &mut UnixStream, events: &mut UnixStream) {
    for stream in [&mut *ops, &mut *events] {
        let first = read_frame(stream).expect("the holder's hello");
        assert!(matches!(greeting(&first), Ok(Greeting::Hello(v)) if v == HOLDER_VERSION));
    }
    write_frame(ops, &hello(HOLDER_VERSION, "ops"), &[]).unwrap();
}

impl Raw {
    fn request(&mut self, header: Value, payload: &[u8]) -> Frame {
        write_frame(&mut self.ops, &header, payload).unwrap();
        read_frame(&mut self.ops).expect("a reply")
    }

    fn log(&self) -> String {
        self.log.lock().unwrap().join("\n")
    }
}

#[test]
fn an_unknown_op_is_refused_and_the_link_goes_on() {
    let (mut raw, _held) = raw_link();
    let reply = raw.request(json!({"op": "frobnicate", "id": 1}), b"");
    assert_eq!(reply.u64("id"), Some(1));
    assert_eq!(reply.header["ok"], json!(false));
    assert_eq!(reply.header["err"]["k"], json!("unsupported"));
    // The next request is served.
    let reply = raw.request(json!({"op": "list", "id": 2}), b"");
    assert_eq!(reply.header["ok"], json!(true));
    assert_eq!(reply.header["panes"], json!([]));
    assert!(
        raw.log().contains("unknown op \"frobnicate\""),
        "{}",
        raw.log()
    );
    drop(raw.ops);
    drop(raw.events);
    assert!(matches!(raw.server.join().unwrap(), Ok(())));
}

#[test]
fn a_request_with_missing_fields_is_refused_and_the_link_goes_on() {
    let (mut raw, _held) = raw_link();
    for header in [
        json!({"op": "write", "id": 1}),
        json!({"op": "resize", "id": 2, "pane": "n:1"}),
        json!({"op": "spawn", "id": 3, "cwd": "/"}),
        json!({"op": "kill", "id": 4}),
    ] {
        let reply = raw.request(header.clone(), b"");
        assert_eq!(reply.header["ok"], json!(false), "{header}");
        assert_eq!(reply.header["err"]["k"], json!("invalid_input"), "{header}");
    }
    let reply = raw.request(json!({"op": "list", "id": 9}), b"");
    assert_eq!(reply.header["ok"], json!(true));
}

#[test]
fn a_request_with_no_id_ends_the_link() {
    let (mut raw, _held) = raw_link();
    write_frame(&mut raw.ops, &json!({"op": "list"}), b"").unwrap();
    let ended = raw.server.join().unwrap();
    assert!(
        matches!(ended, Err(ServeError::Wire(WireError::Malformed(_)))),
        "{ended:?}"
    );
    // The holder closed the link: the front sees the end.
    assert!(read_frame(&mut raw.ops).is_err());
}

#[test]
fn an_oversize_frame_ends_the_link_before_it_is_read() {
    let (mut raw, _held) = raw_link();
    use std::io::Write;
    let mut prefix = Vec::new();
    prefix.extend_from_slice(&2u32.to_be_bytes());
    prefix.extend_from_slice(&(MAX_FRAME as u32).to_be_bytes());
    raw.ops.write_all(&prefix).unwrap();
    let ended = raw.server.join().unwrap();
    assert!(
        matches!(ended, Err(ServeError::Wire(WireError::Oversize(_)))),
        "{ended:?}"
    );
    assert!(read_frame(&mut raw.ops).is_err());
}

#[test]
fn a_frame_cut_short_ends_the_link_as_truncated() {
    let (mut raw, _held) = raw_link();
    use std::io::Write;
    let mut partial = Vec::new();
    partial.extend_from_slice(&40u32.to_be_bytes());
    partial.extend_from_slice(&0u32.to_be_bytes());
    partial.extend_from_slice(b"{\"op\":");
    raw.ops.write_all(&partial).unwrap();
    raw.ops.shutdown(std::net::Shutdown::Write).unwrap();
    let ended = raw.server.join().unwrap();
    assert!(
        matches!(ended, Err(ServeError::Wire(WireError::Truncated))),
        "{ended:?}"
    );
}

#[test]
fn a_front_that_closes_between_frames_is_a_clean_end() {
    let (raw, held) = raw_link();
    let Raw {
        ops,
        events,
        server,
        ..
    } = raw;
    drop(ops);
    drop(events);
    let ended = server.join().unwrap();
    assert!(matches!(ended, Ok(())), "{ended:?}");
    drop(held);
}

#[test]
fn the_holder_log_never_repeats_a_request_body() {
    let secret = "sk-TOPSECRET-4242";
    let local = Arc::new(LocalHost::with_tail(4096));
    let (holder, log) = Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>).capture_log();
    let held = Held::with_holder(local, Arc::new(holder));
    let client = held.connect();
    let dir = TempDir::new("link-log");
    let spawned = client
        .spawn(SpawnReq {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo argv-marker-9917; sleep 5".into(),
            ],
            cwd: dir.path().to_string_lossy().into_owned(),
            env: vec![(OsString::from("API_TOKEN"), OsString::from(secret))],
        })
        .unwrap();
    // Provoke every kind of complaint the holder logs.
    let (mut bad_ops, mut bad_events) = held.serve_pair();
    handshake(&mut bad_ops, &mut bad_events);
    write_frame(
        &mut bad_ops,
        &json!({"op": "write", "id": 1, "argv": [secret], "env": secret}),
        secret.as_bytes(),
    )
    .unwrap();
    let _ = read_frame(&mut bad_ops);
    use std::io::Write;
    let garbage = format!("{{\"token\": \"{secret}\" nonsense");
    let mut frame = Vec::new();
    frame.extend_from_slice(&(garbage.len() as u32).to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame.extend_from_slice(garbage.as_bytes());
    bad_ops.write_all(&frame).unwrap();
    let _ = held.finish_servers();
    client.kill(&spawned.id, Duration::ZERO);
    let log = log.lock().unwrap().join("\n");
    assert!(!log.is_empty(), "the holder logged nothing at all");
    assert!(!log.contains(secret), "{log}");
    assert!(!log.contains("argv-marker-9917"), "{log}");
}

// ---------------------------------------------------------------------------
// What crosses the wire intact
// ---------------------------------------------------------------------------

#[test]
fn an_environment_with_equals_signs_and_non_utf8_bytes_arrives_exactly() {
    let client = Held::new().connect();
    let rig = Rig::new(as_host(&client), "link-env");
    let pane = client
        .spawn(SpawnReq {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf %s \"$ODD\" | /usr/bin/od -An -tx1; printf '[%s]' \"${EMPTY+set}\"".into(),
            ],
            cwd: rig.dir.path().to_string_lossy().into_owned(),
            env: vec![
                (
                    OsString::from("ODD"),
                    OsString::from_vec(b"a=b\xff\x01".to_vec()),
                ),
                (OsString::from("EMPTY"), OsString::new()),
            ],
        })
        .unwrap();
    rig.sink.wait_exit(&pane.id);
    let text = rig.sink.text(&pane.id);
    // `od` pads its columns differently per platform.
    let words: Vec<&str> = text.split_whitespace().collect();
    assert!(
        words
            .windows(5)
            .any(|w| w == ["61", "3d", "62", "ff", "01"]),
        "{text:?}"
    );
    // Set to the empty string, which is not the same as unset.
    assert!(text.contains("[set]"), "{text:?}");
}

#[test]
fn a_spawn_failure_keeps_its_kind_and_text() {
    let client = Held::new().connect();
    let err = client
        .spawn(SpawnReq {
            argv: vec!["/definitely/not/here".into()],
            cwd: "/".into(),
            env: Vec::new(),
        })
        .expect_err("a missing program fails");
    // The same error the in-process host gives.
    let local = LocalHost::with_tail(4096);
    local.set_sink(Arc::new(NullSink));
    let want = local
        .spawn(SpawnReq {
            argv: vec!["/definitely/not/here".into()],
            cwd: "/".into(),
            env: Vec::new(),
        })
        .expect_err("a missing program fails");
    assert_eq!(err, want);
}

struct NullSink;

impl HostEvents for NullSink {
    fn output(&self, _: &str, _: u64, _: &[u8]) -> bool {
        true
    }
    fn idle(&self, _: &str) {}
    fn exited(&self, _: &str, _: Option<i64>) {}
    fn gap(&self, _: &str, _: u64) {}
}

#[test]
fn replies_find_their_callers_under_concurrent_calls() {
    let client = Held::new().connect();
    let rig = Rig::new(as_host(&client), "link-concurrent");
    let panes: Vec<_> = (0..4).map(|_| rig.spawn(&["sleep", "30"])).collect();
    let failures = Arc::new(AtomicUsize::new(0));
    let threads: Vec<_> = panes
        .iter()
        .enumerate()
        .map(|(n, pane)| {
            let client = Arc::clone(&client);
            let failures = Arc::clone(&failures);
            let id = pane.id.clone();
            thread::spawn(move || {
                for i in 0..150 {
                    let blob = format!("pane {n} round {i} {}", "x".repeat(i)).into_bytes();
                    client.meta_set(&id, &blob).unwrap();
                    if client.meta_get(&id).unwrap() != blob {
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                    if client.list().len() != 4 {
                        failures.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(failures.load(Ordering::SeqCst), 0);
}

/// A pane that never reads fills its input buffer; the write then answers
/// `WouldBlock` with the OS error intact, `wait_writable` says no, and the
/// front's lock-release loop keeps working.
#[test]
fn a_full_input_buffer_answers_would_block_over_the_link() {
    let client = Held::new().connect();
    let rig = Rig::new(as_host(&client), "link-full");
    // Raw mode, so the line discipline keeps the bytes (in canonical mode it
    // drops what a line cannot hold and never pushes back).
    let pane = rig.sh("stty raw -echo; echo ready; sleep 30");
    rig.sink.wait_text(&pane.id, "ready");
    let chunk = vec![b'x'; 4096];
    let mut accepted = 0usize;
    let err = loop {
        match client.try_write(&pane.id, &chunk) {
            Ok(n) => accepted += n,
            Err(err) => break err,
        }
        assert!(accepted < 16 * 1024 * 1024, "the buffer never filled");
    };
    assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock, "{err}");
    assert_eq!(err.raw_os_error(), Some(libc::EAGAIN));
    // The op itself works on a full pane (whether it says "writable" for the
    // few bytes of room a platform may have left is the PTY's business).
    client
        .wait_writable(&pane.id, Duration::from_millis(100))
        .expect("wait_writable answers");
    assert!(accepted > 0);
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Many short outputs then an exit: every output precedes the exit and the
/// last byte is in the sink when `exited` arrives.
#[test]
fn every_output_precedes_the_exit_and_the_last_byte_is_there() {
    for _ in 0..5 {
        let client = Held::new().connect();
        let rig = Rig::new(as_host(&client), "link-order");
        let pane =
            rig.sh("i=0; while [ $i -lt 300 ]; do printf 'x%d;' $i; i=$((i+1)); done; printf LAST");
        rig.sink.wait_exit(&pane.id);
        let ev = rig.sink.events.lock().unwrap();
        assert!(ev.violations.is_empty(), "{:?}", ev.violations);
        drop(ev);
        let text = rig.sink.text(&pane.id);
        assert!(
            text.ends_with("LAST"),
            "{:?}",
            &text[text.len().saturating_sub(40)..]
        );
        assert!(text.contains("x299;"));
    }
}

/// Events for a pane before the front has a sink wait in the socket and are
/// delivered once the sink is set.
#[test]
fn output_before_the_sink_is_delivered_when_it_is_set() {
    let client = Held::new().connect();
    let dir = TempDir::new("link-presink");
    let pane = client
        .spawn(SpawnReq {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf early-bytes; sleep 30".into(),
            ],
            cwd: dir.path().to_string_lossy().into_owned(),
            env: Vec::new(),
        })
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let sink = Arc::new(Recorder::default());
    client.set_sink(Arc::clone(&sink) as Arc<dyn HostEvents>);
    sink.wait_text(&pane.id, "early-bytes");
}

#[test]
fn a_quiet_pane_produces_idle_ticks() {
    struct Ticks(AtomicUsize);
    impl HostEvents for Ticks {
        fn output(&self, _: &str, _: u64, _: &[u8]) -> bool {
            true
        }
        fn idle(&self, _: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn exited(&self, _: &str, _: Option<i64>) {}
        fn gap(&self, _: &str, _: u64) {}
    }
    let client = Held::new().connect();
    let ticks = Arc::new(Ticks(AtomicUsize::new(0)));
    client.set_sink(Arc::clone(&ticks) as Arc<dyn HostEvents>);
    let dir = TempDir::new("link-idle");
    client
        .spawn(SpawnReq {
            argv: vec!["sleep".into(), "30".into()],
            cwd: dir.path().to_string_lossy().into_owned(),
            env: contract_env(),
        })
        .unwrap();
    eventually("two idle ticks", || ticks.0.load(Ordering::SeqCst) >= 2);
}

fn contract_env() -> Vec<(OsString, OsString)> {
    super::pty::child_env(|key| std::env::var_os(key), &[]).unwrap()
}

/// A front that stops reading its events does not hold the pane back: the
/// program runs to its end, the ring wraps, and when the front reads again it
/// is told of the gap, then gets the rest and the exit, in order.
#[test]
fn a_stalled_events_reader_gets_a_gap_and_the_pump_is_never_held_back() {
    let held = Held::with_tail(TAIL_TEST_CAP);
    let client = held.connect();
    let rig = Rig::new(as_host(&client), "link-gap");
    client.pause_events(true);
    let pane = rig.sh("head -c 3000000 /dev/zero | tr '\\0' 'a'; printf END-MARK");
    // The program finishes on the holder while nothing reads the events.
    eventually("the pane to finish on the holder", || {
        held.local
            .list()
            .iter()
            .any(|p| p.id == pane.id && p.finished)
    });
    assert!(
        rig.sink.events.lock().unwrap().output.is_empty(),
        "nothing was read while paused"
    );
    client.pause_events(false);
    rig.sink.wait_exit(&pane.id);
    let ev = rig.sink.events.lock().unwrap();
    assert!(ev.gaps >= 1, "no gap was reported");
    assert!(ev.violations.is_empty(), "{:?}", ev.violations);
    let chunks = &ev.output[&pane.id];
    let heard: usize = chunks.iter().map(|(_, b)| b.len()).sum();
    assert!(
        heard < 3_000_000,
        "everything arrived ({heard}), so nothing gapped"
    );
    // What did arrive is contiguous from the resume point to the end.
    let (last_seq, last) = chunks.last().unwrap();
    let end = last_seq + last.len() as u64;
    drop(ev);
    let tail = held.local.attach(&pane.id).unwrap();
    assert_eq!(
        end, tail.tail_end,
        "the stream ends where the holder's does"
    );
    assert!(rig.sink.text(&pane.id).ends_with("END-MARK"));
}

/// After a gap a new front adopts the pane from its tail like any other.
#[test]
fn after_a_gap_a_new_front_reads_the_tail_cleanly() {
    let held = Held::with_tail(TAIL_TEST_CAP);
    let client = held.connect();
    let rig = Rig::new(as_host(&client), "link-gap2");
    client.pause_events(true);
    let pane = rig.sh(&format!("{ALPHABET_STREAM} 2000000; sleep 30"));
    eventually("the ring to wrap", || {
        held.local
            .tail_bounds(&pane.id)
            .is_ok_and(|(start, _)| start > 100_000)
    });
    drop(rig);
    drop(client);
    let second = held.connect();
    let at = second.attach(&pane.id).unwrap();
    assert_is_stream(at.tail_start, &at.bytes);
    assert!(at.tail_start > 0);
}

// ---------------------------------------------------------------------------
// Ops that must not wait for each other
// ---------------------------------------------------------------------------

#[test]
fn a_slow_kill_of_one_pane_does_not_delay_a_write_to_another() {
    let client = Held::new().connect();
    let rig = Rig::new(as_host(&client), "link-slowkill");
    let stubborn = rig.sh("trap '' HUP TERM; echo ready; while :; do sleep 1; done");
    rig.sink.wait_text(&stubborn.id, "ready");
    let other = rig.spawn(&["cat"]);
    let killer = {
        let client = Arc::clone(&client);
        let id = stubborn.id.clone();
        thread::spawn(move || client.kill(&id, Duration::from_secs(4)))
    };
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    for _ in 0..20 {
        wait_write(&rig, &other.id, b"x");
    }
    let took = started.elapsed();
    assert!(
        !killer.is_finished(),
        "the kill must still be running (it has a 4 s grace)"
    );
    assert!(took < Duration::from_secs(2), "20 writes took {took:?}");
    let outcome = killer.join().unwrap().expect("the pane was known");
    assert!(outcome.killed);
    assert!(!process_alive(stubborn.pid));
}

/// The exit of a killed pane is the sink's by the time `kill` returns, which
/// is what `remove` reads its exit code from, even when the events are held
/// back behind output the front has not read.
#[test]
fn kill_returns_with_the_exit_applied_even_when_the_events_are_behind() {
    let client = Held::new().connect();
    let rig = Rig::new(as_host(&client), "link-killexit");
    let pane = rig.spawn(&["sleep", "30"]);
    client.pause_events(true);
    let started = Instant::now();
    let outcome = client
        .kill(&pane.id, Duration::from_secs(2))
        .expect("known pane");
    assert!(!outcome.killed);
    assert!(started.elapsed() < Duration::from_secs(5));
    let exit = rig
        .sink
        .events
        .lock()
        .unwrap()
        .exited
        .get(&pane.id)
        .copied();
    assert!(
        matches!(exit, Some(Some(129 | 143))),
        "the exit (128 + HUP or TERM) had not reached the sink when kill returned: {exit:?}"
    );
    // The event that follows is the same exit, not a second one.
    client.pause_events(false);
    thread::sleep(Duration::from_millis(300));
    assert!(rig.sink.events.lock().unwrap().violations.is_empty());
}

#[test]
fn an_unanswered_op_times_out_and_the_link_stays_usable() {
    let gate_host = GateHost::new(4096);
    let held = gate_host.rig();
    let client = held.connect();
    let rig = Rig::new(as_host(&client), "link-timeout");
    let pane = rig.spawn(&["cat"]);
    client.set_reply_timeout(Duration::from_millis(300));
    gate_host.gate.arm();
    let started = Instant::now();
    let err = client.try_write(&pane.id, b"x").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(gate_host.gate.wait_entered(1, LONG));
    // The late reply finds no caller and is dropped; the link works.
    gate_host.gate.open();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(client.list().len(), 1);
    assert!(!client.is_closed());
    client.set_reply_timeout(Duration::from_secs(10));
    assert_eq!(client.try_write(&pane.id, b"y").unwrap(), 1);
}

// ---------------------------------------------------------------------------
// The life of a link
// ---------------------------------------------------------------------------

#[test]
fn a_new_front_replaces_the_old_one_and_finds_its_panes() {
    let held = Held::new();
    let first = held.connect();
    let rig = Rig::new(as_host(&first), "link-takeover");
    let pane = rig.spawn(&["sleep", "30"]);
    let second = held.connect();
    eventually("the old link to close", || first.is_closed());
    let err = first.try_write(&pane.id, b"x").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    assert!(first.list().is_empty());
    let listed = second.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, pane.id);
    assert!(process_alive(pane.pid), "the pane outlived its front");
}

#[test]
fn a_front_that_goes_away_leaves_its_panes_running() {
    let held = Held::new();
    let first = held.connect();
    let rig = Rig::new(as_host(&first), "link-leave");
    let pane = rig.spawn(&["sleep", "30"]);
    drop(rig);
    drop(first);
    thread::sleep(Duration::from_millis(300));
    assert!(process_alive(pane.pid));
    let second = held.connect();
    assert_eq!(second.list().len(), 1);
}

/// A holder that dies: callers waiting for a reply fail at once, and every
/// later call fails.
#[test]
fn a_dead_holder_fails_waiting_and_later_calls() {
    let (ops, mut holder_ops) = pair();
    let (events, mut holder_events) = pair();
    write_frame(&mut holder_ops, &hello(HOLDER_VERSION, "ops"), &[]).unwrap();
    write_frame(&mut holder_events, &hello(HOLDER_VERSION, "events"), &[]).unwrap();
    let client = Arc::new(HolderClient::connect(ops, events).unwrap());
    let _ = read_frame(&mut holder_ops).unwrap(); // the front's hello
    let waiting = {
        let client = Arc::clone(&client);
        thread::spawn(move || client.try_write("n:1", b"x"))
    };
    // The request arrives and is never answered; then the holder dies.
    let request = read_frame(&mut holder_ops).unwrap();
    assert_eq!(request.str("op"), Some("write"));
    let started = Instant::now();
    drop(holder_ops);
    drop(holder_events);
    let err = waiting.join().unwrap().unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe, "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(client.is_closed());
    assert_eq!(
        client.try_write("n:1", b"x").unwrap_err().kind(),
        std::io::ErrorKind::BrokenPipe
    );
    assert!(client.list().is_empty());
    assert!(client.kill("n:1", Duration::ZERO).is_none());
}

// ---------------------------------------------------------------------------
// A front that vanishes mid-write
// ---------------------------------------------------------------------------

/// The holder's events writer is blocked in a write to a front that does not
/// read; the front then dies. The write fails with EPIPE (the process ignores
/// SIGPIPE, as every Rust binary does), the serve call ends, the pane runs
/// on, and a new front is served.
#[test]
fn a_front_that_dies_while_the_holder_is_writing_to_it_costs_the_holder_nothing() {
    let local = Arc::new(LocalHost::with_tail(4096));
    let (holder, log2) = Holder::new(Arc::clone(&local) as Arc<dyn HolderHost>).capture_log();
    let held = Held::with_holder(local, Arc::new(holder));
    let first = held.connect();
    let rig = Rig::new(as_host(&first), "link-vanish");
    first.pause_events(true);
    let pane = rig.sh("head -c 5000000 /dev/zero | tr '\\0' 'a'; sleep 30");
    eventually("the ring to pass its size", || {
        held.local
            .tail_bounds(&pane.id)
            .is_ok_and(|(_, end)| end > 2_000_000)
    });
    // The writer has filled the socket and is blocked. Now the front dies.
    drop(rig);
    drop(first);
    let ended = held.finish_servers();
    assert!(
        matches!(ended.as_slice(), [Ok(())] | [Err(_)]),
        "the serve call must end, got {ended:?}"
    );
    let log = log2.lock().unwrap().join("\n");
    assert!(log.contains("the front is gone"), "{log}");
    // The holder and its pane are fine, and a new front is served.
    assert!(process_alive(pane.pid));
    let second = held.connect();
    assert_eq!(second.list().len(), 1);
    assert!(second.attach(&pane.id).is_ok());
}

// ---------------------------------------------------------------------------
// Measurements (run by hand: `cargo test --release --lib -- --ignored
// --nocapture measure_`)
// ---------------------------------------------------------------------------

pub(crate) fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize]
}

pub(crate) fn report(what: &str, mut samples: Vec<Duration>) {
    if samples.is_empty() {
        eprintln!("MEASURE {what}: no samples (nothing arrived)");
        return;
    }
    samples.sort();
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    eprintln!(
        "MEASURE {what}: n={} p50={:.0}us p90={:.0}us p99={:.0}us p99.9={:.0}us max={:.0}us",
        samples.len(),
        us(percentile(&samples, 0.5)),
        us(percentile(&samples, 0.9)),
        us(percentile(&samples, 0.99)),
        us(percentile(&samples, 0.999)),
        us(*samples.last().unwrap()),
    );
}

/// One write to a pane that echoes it, over the link and straight to the
/// host. The pane's echo flows back as events while the writes go on.
#[test]
#[ignore = "measurement"]
fn measure_write_round_trip() {
    let n = 20_000;
    let client = Held::new().connect();
    let rig = Rig::new(as_host(&client), "measure-write");
    let pane = rig.spawn(&["cat"]);
    let mut over_link = Vec::with_capacity(n);
    for i in 0..n + 200 {
        let started = Instant::now();
        loop {
            match client.try_write(&pane.id, b"x") {
                Ok(_) => break,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    let _ = client.wait_writable(&pane.id, Duration::from_millis(50));
                }
                Err(err) => panic!("{err}"),
            }
        }
        if i >= 200 {
            over_link.push(started.elapsed());
        }
    }
    report("write over the link", over_link);

    let local = LocalHost::with_tail(1 << 20);
    local.set_sink(Arc::new(NullSink));
    let dir = TempDir::new("measure-local");
    let pane = local
        .spawn(SpawnReq {
            argv: vec!["cat".into()],
            cwd: dir.path().to_string_lossy().into_owned(),
            env: contract_env(),
        })
        .unwrap();
    let mut direct = Vec::with_capacity(n);
    for i in 0..n + 200 {
        let started = Instant::now();
        loop {
            match local.try_write(&pane.id, b"x") {
                Ok(_) => break,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    let _ = local.wait_writable(&pane.id, Duration::from_millis(50));
                }
                Err(err) => panic!("{err}"),
            }
        }
        if i >= 200 {
            direct.push(started.elapsed());
        }
    }
    report("write in process", direct);
    local.end_all();
}

fn cpu_time() -> Duration {
    // SAFETY: getrusage fills the struct.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    tv(usage.ru_utime) + tv(usage.ru_stime)
}

/// A holder with a front and eight quiet panes should cost next to nothing.
#[test]
#[ignore = "measurement"]
fn measure_idle_cpu() {
    let client = Held::new().connect();
    let rig = Rig::new(as_host(&client), "measure-idle");
    for _ in 0..8 {
        rig.spawn(&["sleep", "60"]);
    }
    thread::sleep(Duration::from_secs(2));
    let before = cpu_time();
    let started = Instant::now();
    thread::sleep(Duration::from_secs(10));
    let cpu = cpu_time() - before;
    eprintln!(
        "MEASURE idle: 8 quiet panes over a link, {:.1} ms of CPU in {:.1} s of wall time \
         (this process: holder, client and the test harness)",
        cpu.as_secs_f64() * 1e3,
        started.elapsed().as_secs_f64()
    );
    rig.host.end_all();

    // The same eight panes with no link at all: what the pump and reaper
    // threads cost by themselves.
    let local = LocalHost::with_tail(1 << 20);
    local.set_sink(Arc::new(NullSink));
    let dir = TempDir::new("measure-idle-local");
    for _ in 0..8 {
        local
            .spawn(SpawnReq {
                argv: vec!["sleep".into(), "60".into()],
                cwd: dir.path().to_string_lossy().into_owned(),
                env: contract_env(),
            })
            .unwrap();
    }
    thread::sleep(Duration::from_secs(2));
    let before = cpu_time();
    thread::sleep(Duration::from_secs(10));
    eprintln!(
        "MEASURE idle: the same 8 panes in process, {:.1} ms of CPU in 10 s",
        (cpu_time() - before).as_secs_f64() * 1e3
    );
    local.end_all();
}
