//! Zombie handling under an init process (issue #76, PR7), Linux only.
//!
//! This is its own test binary on purpose. The test process makes itself a child
//! subreaper (what tini does in the image), so every orphan in the holder's tree is
//! re-parented to it. In a shared binary that would take the grandchildren of other
//! tests and make their `/proc` and `kill(pid, 0)` checks unreliable. Keep it to the
//! one test function below, and do not add any other test to this file.
//!
//! A pane runs `sh -c '(sleep N &); exit'`. The test then asserts that:
//! - the holder reaped the pane's leader (no defunct child of the holder, and the
//!   leader's pid is gone), so the holder reaps what it starts;
//! - the leftover `sleep` was re-parented to the test process, not to the holder, so
//!   the holder is not a subreaper (N10) and never waits on a pid it did not start;
//! - the test process, as init, reaps that `sleep` and nothing defunct remains.
//!
//! Real PID 1 under tini in the image is checked by
//! `docs/operator-e2e/zombie-under-tini.sh`.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_openab-shepherd");
const LIMIT: Duration = Duration::from_secs(30);

/// `(ppid, state)` of `pid` from `/proc`, or `None` once it is gone.
fn stat_of(pid: i32) -> Option<(i32, char)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) S ppid ...": comm may hold spaces and parentheses.
    let rest = &text[text.rfind(')')? + 2..];
    let mut f = rest.split_whitespace();
    let state = f.next()?.chars().next()?;
    let ppid = f.next()?.parse().ok()?;
    Some((ppid, state))
}

/// Every process whose parent is `parent`: `(pid, state)`.
fn children_of(parent: i32) -> Vec<(i32, char)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if let Some((ppid, state)) = stat_of(pid) {
            if ppid == parent {
                out.push((pid, state));
            }
        }
    }
    out
}

fn read_pid(file: &Path) -> Option<i32> {
    std::fs::read_to_string(file).ok()?.trim().parse().ok()
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + LIMIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_orphan_goes_to_the_init_process_and_nothing_stays_defunct() {
    let me = std::process::id() as i32;
    // Stand in for tini: orphans in the holder's tree come here.
    // SAFETY: plain prctl on this process; no pointers.
    let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    assert_eq!(rc, 0, "PR_SET_CHILD_SUBREAPER");

    let dir = std::env::temp_dir().join(format!("shep-zombie-{me}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.display();
    // The leader writes its own pid and the orphan's pid, leaves the orphan running
    // in the background and exits. 60 s is far beyond the test; the orphan is ended
    // either by the holder's tree kill or by this test.
    let pane = format!(
        "z=echo $$ > {d}/leader.tmp; mv {d}/leader.tmp {d}/leader.pid; \
         (sleep 60 & echo $! > {d}/orphan.tmp; mv {d}/orphan.tmp {d}/orphan.pid); exit 0"
    );
    let mut holder = Command::new(EXE)
        .args(["hold", "--", "--no-listen", "--test-pane", &pane])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &dir)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the holder");
    let holder_pid = holder.id() as i32;

    let leader = until("the pane leader's pid", || {
        read_pid(&dir.join("leader.pid"))
    });
    let orphan = until("the orphan's pid", || read_pid(&dir.join("orphan.pid")));

    // The orphan's parent is the pane's shell or its subshell (a child of the leader)
    // until they exit. Then it must be this process: not the holder (no subreaper
    // there), not the system init.
    let reparented = until("the orphan re-parented", || match stat_of(orphan) {
        Some((ppid, _)) => {
            let transitional = ppid == leader || stat_of(ppid).map(|(up, _)| up) == Some(leader);
            (!transitional).then_some(ppid)
        }
        None => panic!("the orphan is gone before it was re-parented to a subreaper"),
    });
    assert_eq!(
        reparented, me,
        "the orphan went to pid {reparented}, not to the init stand-in {me} \
         (the holder is {holder_pid})"
    );

    // The holder reaped the leader: its pid is gone, and no child of the holder is
    // defunct.
    until("the leader reaped", || {
        stat_of(leader).is_none().then_some(())
    });
    let zombies: Vec<_> = children_of(holder_pid)
        .into_iter()
        .filter(|(_, s)| *s == 'Z')
        .collect();
    assert!(
        zombies.is_empty(),
        "defunct children of the holder: {zombies:?}"
    );

    // Init (this process) reaps the orphan, which the holder may already have ended.
    // SAFETY: signalling and waiting for a process that is our own child now.
    unsafe {
        libc::kill(orphan, libc::SIGKILL);
        let mut status = 0;
        let got = libc::waitpid(orphan, &mut status, 0);
        assert_eq!(got, orphan, "the init stand-in reaps the orphan");
    }
    assert!(
        stat_of(orphan).is_none(),
        "the orphan is gone after the wait"
    );

    // End the holder; every process it started is gone and none is defunct.
    // SAFETY: SIGTERM to the child this test started.
    unsafe {
        libc::kill(holder_pid, libc::SIGTERM);
    }
    let status = holder.wait().expect("wait for the holder");
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    // The front and any pane were children of the holder, so they were reaped by
    // it; anything of ours left over would show here as our own defunct child.
    let mine: Vec<_> = children_of(me)
        .into_iter()
        .filter(|(_, s)| *s == 'Z')
        .collect();
    assert!(
        mine.is_empty(),
        "defunct children of the init stand-in: {mine:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
