//! Process tests for `openab-shepherd hold` (issue #76): the built binary
//! runs as the holder and starts itself again as the hidden `front`, in its
//! `--no-listen` form (it attaches and adopts like the real one but serves no
//! MCP, so no listener is ever opened). Each test owns its holder and its
//! panes and ends them on exit.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_openab-shepherd");
const LIMIT: Duration = Duration::from_secs(30);

static NEXT: AtomicU32 = AtomicU32::new(0);

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shep-hold-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

type Lines = Arc<(Mutex<Vec<String>>, Condvar)>;

/// A running holder, with its stderr collected line by line.
struct Holder {
    child: Child,
    lines: Lines,
    dir: PathBuf,
    panes: Vec<i32>,
}

impl Holder {
    /// A holder whose fronts attach and serve nothing (`--no-listen`) and
    /// start one pane per name in `panes` (each runs `exec sleep 300` and
    /// writes its pid to `<name>.pid`), plus `front_args`.
    fn start(tag: &str, panes: &[&str], front_args: &[&str], env: &[(&str, &str)]) -> Self {
        let dir = scratch(tag);
        let mut args: Vec<String> = vec!["--no-listen".to_string()];
        for name in panes {
            args.push("--test-pane".to_string());
            args.push(format!(
                "{name}=echo $$ > {d}/{name}.tmp; mv {d}/{name}.tmp {d}/{name}.pid; exec sleep 300",
                d = dir.display()
            ));
        }
        args.extend(front_args.iter().map(|a| a.to_string()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        Self::spawn(dir, &refs, env)
    }

    /// `hold -- <front_args>` exactly as given, with a clean environment
    /// plus `env`.
    fn spawn(dir: PathBuf, front_args: &[&str], env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(EXE);
        cmd.arg("hold")
            .arg("--")
            .args(front_args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &dir)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("spawn the holder");
        let lines: Lines = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let stderr = child.stderr.take().unwrap();
        let sink = Arc::clone(&lines);
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                sink.0.lock().unwrap().push(line);
                sink.1.notify_all();
            }
        });
        Self {
            child,
            lines,
            dir,
            panes: Vec::new(),
        }
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn log(&self) -> Vec<String> {
        self.lines.0.lock().unwrap().clone()
    }

    /// The first log line at or after `from` that satisfies `pred`.
    fn wait_line_from(
        &self,
        from: usize,
        what: &str,
        pred: impl Fn(&str) -> bool,
    ) -> (usize, String) {
        let end = Instant::now() + LIMIT;
        let mut lines = self.lines.0.lock().unwrap();
        loop {
            if let Some(at) = lines.iter().skip(from).position(|l| pred(l)) {
                return (from + at, lines[from + at].clone());
            }
            let now = Instant::now();
            assert!(
                now < end,
                "no log line for {what} in {LIMIT:?}; log so far:\n{}",
                lines.join("\n")
            );
            lines = self.lines.1.wait_timeout(lines, end - now).unwrap().0;
        }
    }

    fn wait_line(&self, what: &str, pred: impl Fn(&str) -> bool) -> String {
        self.wait_line_from(0, what, pred).1
    }

    fn count(&self, pred: impl Fn(&str) -> bool) -> usize {
        self.log().iter().filter(|l| pred(l)).count()
    }

    /// The pid of the newest front the holder logged.
    fn front_pid(&self) -> i32 {
        self.log()
            .iter()
            .rev()
            .find_map(|l| front_started_pid(l))
            .expect("a front was started")
    }

    /// Wait for the front after log position `from` to attach; its position.
    fn wait_attached_after(&self, from: usize) -> usize {
        self.wait_line_from(from, "a front attached", attached).0
    }

    /// Start (or find) the pane `name` running `exec sleep 300`, and its pid.
    fn pane_pid(&mut self, name: &str) -> i32 {
        let file = self.dir.join(format!("{name}.pid"));
        let end = Instant::now() + LIMIT;
        loop {
            if let Ok(text) = std::fs::read_to_string(&file) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    if !self.panes.contains(&pid) {
                        self.panes.push(pid);
                    }
                    return pid;
                }
            }
            assert!(Instant::now() < end, "the pane {name} wrote no pid");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn signal(&self, sig: i32) {
        // SAFETY: signalling a child this test started.
        unsafe {
            libc::kill(self.pid(), sig);
        }
    }

    fn wait_exit(&mut self, limit: Duration) -> Option<std::process::ExitStatus> {
        let end = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.signal(libc::SIGTERM);
            if self.wait_exit(Duration::from_secs(8)).is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        for pid in &self.panes {
            // SAFETY: best-effort cleanup of a pane this test started.
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn front_started_pid(line: &str) -> Option<i32> {
    // "openab-shepherd: holder: front 3 started (pid 4242)"
    let rest = line.split("started (pid ").nth(1)?;
    rest.trim_end_matches(')').parse().ok()
}

/// Whether `pid` is a running process (a zombie is not).
fn running(pid: i32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

fn wait_gone(pid: i32, what: &str) {
    let end = Instant::now() + LIMIT;
    while running(pid) {
        assert!(Instant::now() < end, "{what} (pid {pid}) is still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `(pid, stat)` of every child of `parent`.
fn children(parent: i32) -> Vec<(i32, String)> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,stat="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let pid: i32 = f.next()?.parse().ok()?;
            let ppid: i32 = f.next()?.parse().ok()?;
            let stat = f.next()?.to_string();
            (ppid == parent).then_some((pid, stat))
        })
        .collect()
}

/// What each open descriptor of `pid` is, on Linux (for a failure message).
/// An entry whose link cannot be read was closed between the directory read
/// and the link read (the holder's own `/proc` reads come and go): it is no
/// descriptor now, and is left out rather than listed with an empty target.
fn fd_list(pid: i32) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    let mut out: Vec<String> = dir
        .flatten()
        .filter_map(|e| {
            let target = std::fs::read_link(e.path()).ok()?;
            Some(format!(
                "{} -> {}",
                e.file_name().to_string_lossy(),
                target.display()
            ))
        })
        .collect();
    out.sort();
    out
}

/// The number of open descriptors of `pid`, when the platform tells.
fn fd_count(pid: i32) -> Option<usize> {
    if std::fs::read_dir(format!("/proc/{pid}/fd")).is_ok() {
        // Not the holder's own reads of /proc (the pane tree refresh opens
        // /proc and /proc/<pid>/stat for a moment, every 250 ms).
        return Some(
            fd_list(pid)
                .iter()
                .filter(|l| !l.contains("-> /proc"))
                .count(),
        );
    }
    let out = Command::new("lsof")
        .args(["-nP", "-p", &pid.to_string(), "-Ffd"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Numeric descriptors only: cwd, txt and mem lines are not ours to count.
    Some(
        text.lines()
            .filter(|l| l.starts_with('f') && l[1..].chars().all(|c| c.is_ascii_digit()))
            .count(),
    )
}

/// The descriptor count once it has been the same for two samples 50 ms
/// apart (bounded 5 s), with the last listing. The holder's pane tree refresh
/// (every 250 ms) opens and closes descriptors, so one sample can be one off;
/// a real leak keeps growing and never settles on the baseline.
fn settled_fd_count(pid: i32) -> Option<(usize, Vec<String>)> {
    let mut last = fd_count(pid)?;
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let now = fd_count(pid)?;
        if now == last || Instant::now() >= end {
            return Some((now, fd_list(pid)));
        }
        last = now;
    }
}

/// A front's own line, not the holder's quote of one.
fn attached(l: &str) -> bool {
    l.starts_with("openab-shepherd: front: attached;")
}

#[test]
fn a_killed_front_is_replaced_and_the_pane_and_its_meta_survive() {
    let mut h = Holder::start("kill", &["work"], &[], &[]);
    let first = h.wait_attached_after(0);
    h.wait_line("the pane was started", |l| l.contains("started pane work"));
    let pane = h.pane_pid("work");
    let front1 = h.front_pid();

    // SAFETY: killing the front this holder started.
    unsafe {
        libc::kill(front1, libc::SIGKILL);
    }
    h.wait_line("the holder saw the exit", |l| {
        l.contains("exited:") && l.contains("signal: 9")
    });
    // An unplanned exit waits one backoff step, 1 s.
    h.wait_line("the backoff", |l| {
        l.contains("respawning the front in 1000 ms")
    });
    let second = h.wait_attached_after(first + 1);
    let line = h.wait_line_from(second, "the pane adopted", |l| {
        l.contains("pane work live=true")
    });

    assert_ne!(h.front_pid(), front1, "a new front");
    assert!(running(pane), "the pane outlived the front");
    assert_eq!(h.pane_pid("work"), pane, "the same process, not a new pane");
    // The name came back from the holder's meta; nothing started it again.
    assert!(line.1.contains("live=true"));
    assert_eq!(h.count(|l| l.contains("started pane work")), 1);
    assert!(h.log().iter().any(|l| l.contains("adopted 1 pane(s)")));
    assert!(
        h.wait_exit(Duration::from_millis(1)).is_none(),
        "the holder stays"
    );
}

#[test]
fn sigusr2_restarts_the_front_only_with_no_backoff() {
    let mut h = Holder::start("usr2", &["work"], &[], &[]);
    let first = h.wait_attached_after(0);
    let pane = h.pane_pid("work");
    let (holder, front1) = (h.pid(), h.front_pid());

    let sent = Instant::now();
    h.signal(libc::SIGUSR2);
    let second = h.wait_attached_after(first + 1);
    let latency = sent.elapsed();
    eprintln!("SIGUSR2 to the new front attached: {latency:?}");

    assert_eq!(h.pid(), holder);
    assert_ne!(h.front_pid(), front1);
    assert!(running(pane), "SIGUSR2 left the pane running");
    assert_eq!(h.pane_pid("work"), pane);
    h.wait_line_from(second, "the pane adopted", |l| {
        l.contains("pane work live=true")
    });
    assert_eq!(h.count(|l| l.contains("respawning")), 0, "no backoff");
    assert!(
        h.log()
            .iter()
            .any(|l| l.contains("SIGUSR2: restarting the front")),
        "{:?}",
        h.log()
    );
    // The old front ended on SIGTERM, not killed.
    assert!(h
        .log()
        .iter()
        .any(|l| l.contains("exited:") && l.contains("exit status: 143")));
    assert!(h.wait_exit(Duration::from_millis(1)).is_none());
}

#[test]
fn fifty_sigusr2_cycles_leave_no_defunct_front_and_a_stable_fd_count() {
    let mut h = Holder::start("cycles", &["work"], &[], &[]);
    let mut at = h.wait_attached_after(0);
    let pane = h.pane_pid("work");
    // Each front's pid comes from its own `started (pid P)` line, in order.
    // The holder logs it just after the front may print `attached`, so the
    // newest `started` line right after an `attach` can still be the
    // previous front's.
    let (mut cursor, line) = h.wait_line_from(0, "the first front started", |l| {
        front_started_pid(l).is_some()
    });
    cursor += 1;
    let mut pids = vec![front_started_pid(&line).unwrap()];
    std::thread::sleep(Duration::from_millis(300));
    let fds_before = settled_fd_count(h.pid());
    for _ in 0..50 {
        h.signal(libc::SIGUSR2);
        let (idx, line) = h.wait_line_from(cursor, "the next front started", |l| {
            front_started_pid(l).is_some()
        });
        cursor = idx + 1;
        pids.push(front_started_pid(&line).unwrap());
        at = h.wait_attached_after(at + 1);
    }
    std::thread::sleep(Duration::from_millis(300));
    let kids = children(h.pid());
    let current = *pids.last().unwrap();
    assert!(
        kids.iter().all(|(_, stat)| !stat.starts_with('Z')),
        "a defunct child: {kids:?}"
    );
    let mut sorted = pids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 51, "every cycle is a new front");
    // The holder's children are the live front and the one pane.
    let mut expected = vec![current, pane];
    expected.sort_unstable();
    let mut got: Vec<i32> = kids.iter().map(|(pid, _)| *pid).collect();
    got.sort_unstable();
    assert_eq!(got, expected, "{kids:?}");
    for old in &pids[..pids.len() - 1] {
        assert!(!running(*old), "the old front {old} is still around");
    }
    if let (Some((before, listed)), Some((after, now))) = (fds_before, settled_fd_count(h.pid())) {
        assert_eq!(
            before, after,
            "the holder's descriptors after 50 restarts; before: {listed:?}; now: {now:?}"
        );
    } else {
        eprintln!("no way to count descriptors here; that check is skipped");
    }
    assert!(running(pane));
}

#[test]
fn sigterm_to_the_holder_ends_the_panes_and_the_front_and_exits_143() {
    let mut h = Holder::start("term", &["work"], &[], &[]);
    h.wait_attached_after(0);
    let pane = h.pane_pid("work");
    let front = h.front_pid();
    h.signal(libc::SIGTERM);
    let status = h.wait_exit(LIMIT).expect("the holder exits");
    assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{:?}", h.log());
    wait_gone(pane, "the pane");
    wait_gone(front, "the front");
}

#[test]
fn killing_the_holder_ends_the_panes() {
    let mut h = Holder::start("holder-kill", &["work"], &[], &[]);
    h.wait_attached_after(0);
    let pane = h.pane_pid("work");
    let front = h.front_pid();
    assert!(running(pane));
    // SIGKILL: no handler runs; closing the PTY masters hangs the panes up.
    h.child.kill().unwrap();
    let _ = h.child.wait();
    wait_gone(pane, "the pane");
    // The front sees its link close and leaves, without ending anything.
    wait_gone(front, "the front");
}

fn write_version(path: &Path, version: &str) {
    std::fs::write(path, version).unwrap();
}

#[test]
fn a_front_of_another_protocol_exits_78_in_a_loop_while_panes_live_then_a_matching_front_adopts() {
    let dir = scratch("version-file");
    let file = dir.join("version");
    let hook = format!("@{}", file.display());
    let mut h = Holder::start(
        "version",
        &["work"],
        &[],
        &[("OPENAB_SHEPHERD_TEST_HOLDER_VERSION", &hook)],
    );
    // No file yet: the real version, a normal front.
    let first = h.wait_attached_after(0);
    h.wait_line("the pane was started", |l| l.contains("started pane work"));
    let pane = h.pane_pid("work");
    let front1 = h.front_pid();

    // The next fronts speak protocol 999.
    write_version(&file, "999\n");
    // SAFETY: killing the front this holder started.
    unsafe {
        libc::kill(front1, libc::SIGKILL);
    }
    let end = Instant::now() + LIMIT;
    while h.count(|l| l.contains("exit status: 78")) < 3 {
        assert!(Instant::now() < end, "the loop: {:#?}", h.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    let log = h.log();
    // The holder says why, with both numbers.
    assert!(log
        .iter()
        .any(|l| l.contains("exit status: 78") && l.contains("holder protocol 1, front 999")));
    // Delays double: the crash of front 1, then each refused front.
    let delays: Vec<&str> = log
        .iter()
        .filter_map(|l| l.split("respawning the front in ").nth(1))
        .collect();
    assert_eq!(&delays[..3], &["1000 ms", "2000 ms", "4000 ms"], "{log:#?}");
    // Throughout: the holder, and the pane, alive.
    assert!(h.wait_exit(Duration::from_millis(1)).is_none());
    assert!(running(pane));
    assert_eq!(h.count(|l| l.contains("started pane work")), 1);

    // A matching front adopts the pane that lived through the loop.
    write_version(&file, "");
    let count = h.count(attached);
    let end = Instant::now() + LIMIT;
    while h.count(attached) == count {
        assert!(Instant::now() < end, "no matching front: {:#?}", h.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = first;
    h.wait_line("the pane adopted by the matching front", |l| {
        l.contains("pane work live=true")
    });
    assert!(running(pane));
    assert_eq!(h.pane_pid("work"), pane);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_front_that_cannot_start_is_retried_and_the_log_says_why() {
    let dir = scratch("badflag");
    let mut h = Holder::spawn(dir, &["--bogus"], &[]);
    h.wait_line("the first failure with its stderr line", |l| {
        l.contains("exit status: 2") && l.contains("unknown argument: --bogus")
    });
    h.wait_line("a second attempt", |l| l.contains("front 2 started"));
    assert!(
        h.wait_exit(Duration::from_millis(1)).is_none(),
        "the holder stays"
    );
}

#[test]
fn a_front_with_no_settings_exits_2_and_the_holder_logs_it_and_stays() {
    // No token in the environment: the real front's own startup check.
    let dir = scratch("noconfig");
    let mut h = Holder::spawn(dir, &[], &[]);
    h.wait_line("the refusal", |l| {
        l.contains("exit status: 2") && l.contains("refusing to start")
    });
    h.wait_line("a second attempt", |l| l.contains("front 2 started"));
    assert!(h.wait_exit(Duration::from_millis(1)).is_none());
}

#[test]
fn a_front_that_does_not_leave_on_sigterm_is_killed_after_five_seconds() {
    let mut h = Holder::start("stopped", &["work"], &[], &[]);
    let first = h.wait_attached_after(0);
    let pane = h.pane_pid("work");
    let front1 = h.front_pid();
    // A stopped process cannot act on SIGTERM; only SIGKILL gets through.
    // SAFETY: stopping the front this holder started.
    unsafe {
        libc::kill(front1, libc::SIGSTOP);
    }
    let sent = Instant::now();
    h.signal(libc::SIGUSR2);
    h.wait_line("the SIGKILL", |l| l.contains("ignored SIGTERM: SIGKILL"));
    let waited = sent.elapsed();
    assert!(waited >= Duration::from_secs(4), "{waited:?}");
    h.wait_attached_after(first + 1);
    assert_ne!(h.front_pid(), front1);
    assert!(running(pane));
    wait_gone(front1, "the stopped front");
}

/// Run `EXE args` with an empty environment and wait at most `limit`; a child
/// that outlives it is killed and the test fails with what it had printed.
fn run_bounded(args: &[&str], limit: Duration) -> std::process::Output {
    let mut child = Command::new(EXE)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let end = Instant::now() + limit;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= end {
            let _ = child.kill();
            let out = child.wait_with_output().unwrap();
            panic!(
                "`{}` did not exit in {limit:?}; stderr: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// Whether descriptor `fd` is open in this process.
fn fd_open(fd: i32) -> bool {
    // SAFETY: F_GETFD only reads the descriptor's flags.
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

#[test]
fn the_front_subcommand_alone_and_the_bare_command_refuse_the_hidden_flags() {
    // A child inherits every descriptor that is not close-on-exec. Two
    // numbers that are open here would be adopted by the front as its links
    // and it would then serve forever, so use two that are closed.
    let mut closed = (40..200).filter(|fd| !fd_open(*fd));
    let (ops, events) = (closed.next().unwrap(), closed.next().unwrap());
    let run = |args: &[&str]| run_bounded(args, Duration::from_secs(20));
    let out = run(&["front"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--ops-fd"));
    // Not an open descriptor: refused, not a crash.
    let out = run(&[
        "front",
        "--ops-fd",
        &ops.to_string(),
        "--events-fd",
        &events.to_string(),
        "--no-listen",
    ]);
    assert_eq!(out.status.code(), Some(1), "{:?}", out);
    // The bare command knows none of the hidden flags.
    let out = run(&["--no-listen"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown argument: --no-listen"));
    // `hold` takes the server's flags only after `--`.
    let out = run(&["hold", "--bogus"]);
    assert_eq!(out.status.code(), Some(2));
}
