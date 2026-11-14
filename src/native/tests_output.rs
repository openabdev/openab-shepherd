//! Issue #54: the output of a program that exits at once, on a real PTY.
//! The parent keeps its slave copy open until the leader has exited, so the
//! output is not lost on macOS.

use super::*;
use crate::profiles::LaunchPolicy;
use crate::testutil::TempDir;
use crate::tools::{self, Args, Shepherd};
use serde_json::{json, Value};
use std::sync::atomic::AtomicBool;

const LONG: Duration = Duration::from_secs(20);

fn args(value: Value) -> Args {
    value.as_object().unwrap().clone()
}

fn argv_spec(name: &str, argv: &[&str], dir: &TempDir) -> RunSpec {
    RunSpec {
        argv: Some(argv.iter().map(|s| s.to_string()).collect()),
        name: Some(name.to_string()),
        cwd: Some(dir.path().to_string_lossy().into_owned()),
        ..RunSpec::default()
    }
}

/// Busy threads on every core until dropped.
struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Load {
    fn start() -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let n = thread::available_parallelism().map_or(4, |n| n.get());
        let threads = (0..n)
            .map(|_| {
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let mut x = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(7));
                    }
                })
            })
            .collect();
        Load { stop, threads }
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn read_after_exit(b: &NativeBackend, i: usize, dir: &TempDir) {
    let word = format!("out-{i}");
    let info = b
        .run(&argv_spec("quick", &["/bin/echo", &word], dir))
        .unwrap();
    let done = b.wait(&info.reference(), LONG).unwrap();
    assert!(done.ended, "run {i}: not ended");
    let read = b.read(&info.reference(), 0).unwrap();
    assert_eq!(read.text, word, "run {i}: output lost");
    b.remove(&info.reference(), None).unwrap();
}

fn tool_after_exit(sh: &Shepherd, i: usize, dir: &TempDir) {
    let word = format!("tool-{i}");
    tools::call(
        sh,
        "start_pane",
        &args(json!({"name": "quick", "argv": ["/bin/echo", word], "cwd": dir.path()})),
    )
    .unwrap();
    tools::call(
        sh,
        "wait_status",
        &args(json!({"terminal": "quick", "timeout": 20})),
    )
    .unwrap();
    let out = tools::call(sh, "read_output", &args(json!({"terminal": "quick"}))).unwrap();
    assert_eq!(out["text"], word.as_str(), "run {i}: output lost: {out}");
    tools::call(sh, "remove_pane", &args(json!({"terminal": "quick"}))).unwrap();
}

#[test]
fn quick_exit_output_is_readable_200_times() {
    let dir = TempDir::new("native-quick");
    let b = NativeBackend::new(NativeConfig::default());
    for i in 0..200 {
        read_after_exit(&b, i, &dir);
    }
}

#[test]
fn quick_exit_output_is_readable_200_times_under_load() {
    let dir = TempDir::new("native-quick-load");
    let b = NativeBackend::new(NativeConfig::default());
    let _load = Load::start();
    for i in 0..200 {
        read_after_exit(&b, i, &dir);
    }
}

#[test]
fn quick_exit_output_reaches_read_output_200_times_under_load() {
    let dir = TempDir::new("native-quick-tool");
    let b = NativeBackend::new(NativeConfig::default());
    let sh = Shepherd::with_policy(b, LaunchPolicy::arbitrary());
    let _load = Load::start();
    for i in 0..200 {
        tool_after_exit(&sh, i, &dir);
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn gone_within(pid: i32, within: Duration) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if !alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !alive(pid)
}

/// A leader that exits while a background child is still printing: what the
/// child wrote while the leader lived is received, and the pane ends within
/// a bound with no descendant left. (Output after the leader's exit is not
/// delivered on either OS: the exit ends the descendants, ADR §11.)
#[test]
fn leader_exit_with_a_printing_child_ends_the_pane() {
    let dir = TempDir::new("native-late");
    let b = NativeBackend::new(NativeConfig::default());
    let _load = Load::start();
    for i in 0..20 {
        let line = "(echo early-child; echo $$ > pid.txt; sleep 30; echo late-child) & sleep 0.5; echo leader-done";
        let info = b
            .run(&RunSpec {
                line: Some(line.to_string()),
                name: Some("late".to_string()),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                ..RunSpec::default()
            })
            .unwrap();
        let done = b.wait(&info.reference(), LONG).unwrap();
        assert!(done.ended, "run {i}: pane did not end");
        let text = b.read(&info.reference(), 0).unwrap().text;
        assert!(text.contains("early-child"), "run {i}: {text}");
        assert!(text.contains("leader-done"), "run {i}: {text}");
        let pid: i32 = std::fs::read_to_string(dir.path().join("pid.txt"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            gone_within(pid, Duration::from_secs(5)),
            "run {i}: child {pid} outlived the pane"
        );
        b.remove(&info.reference(), None).unwrap();
    }
}

/// More output than the PTY buffer, written by a program that exits at once:
/// the reader keeps the last 256 KiB raw, and the screen shows the end.
#[test]
fn large_output_of_a_quick_exit_is_received_up_to_the_cap() {
    let dir = TempDir::new("native-large");
    let b = NativeBackend::new(NativeConfig::default());
    let _load = Load::start();
    for i in 0..5 {
        let line = "i=1; while [ $i -le 20000 ]; do echo line-$i-0123456789; i=$((i+1)); done; echo END-MARK";
        let info = b
            .run(&RunSpec {
                line: Some(line.to_string()),
                name: Some("large".to_string()),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                ..RunSpec::default()
            })
            .unwrap();
        let done = b.wait(&info.reference(), LONG).unwrap();
        assert!(done.ended, "run {i}: pane did not end");
        assert_eq!(done.exit_code, Some(0));
        let raw = b.raw_output(&info.reference()).unwrap();
        let raw = String::from_utf8_lossy(&raw).into_owned();
        assert!(raw.len() <= 256 * 1024, "run {i}: cap exceeded");
        assert!(raw.len() > 200 * 1024, "run {i}: only {} bytes", raw.len());
        assert!(raw.trim_end().ends_with("END-MARK"), "run {i}: tail lost");
        // Complete up to the cap: consecutive line numbers up to the end.
        let nums: Vec<u32> = raw
            .lines()
            .filter_map(|l| l.strip_prefix("line-")?.split('-').next()?.parse().ok())
            .collect();
        assert_eq!(nums.last(), Some(&20000), "run {i}");
        assert!(nums.windows(2).all(|w| w[1] == w[0] + 1), "run {i}: gap");
        b.remove(&info.reference(), None).unwrap();
    }
}

/// `remove` and shutdown still end a pane and its descendants.
#[test]
fn remove_and_shutdown_leave_no_descendant() {
    let dir = TempDir::new("native-end");
    for how in ["remove", "shutdown"] {
        let b = NativeBackend::new(NativeConfig::default());
        let file = format!("{how}.pid");
        let line = format!("sleep 300 & echo $! > {file}; wait");
        let info = b
            .run(&RunSpec {
                line: Some(line),
                name: Some("end".to_string()),
                cwd: Some(dir.path().to_string_lossy().into_owned()),
                ..RunSpec::default()
            })
            .unwrap();
        let end = Instant::now() + LONG;
        let pid: i32 = loop {
            if let Some(pid) = std::fs::read_to_string(dir.path().join(&file))
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break pid;
            }
            assert!(Instant::now() < end, "{how}: no pid");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(alive(pid), "{how}: child died early");
        if how == "remove" {
            b.remove(&info.reference(), None).unwrap();
        } else {
            b.shutdown();
        }
        assert!(
            gone_within(pid, Duration::from_secs(5)),
            "{how}: child {pid} outlived it"
        );
        let now = b.get(&info.reference());
        if how == "shutdown" {
            assert!(now.map_or(true, |i| !i.live), "pane still live");
        }
    }
}
