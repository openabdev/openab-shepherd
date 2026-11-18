//! The shared process-table snapshot (`procs::Shared`) and the stop path:
//! the readers' periodic look shares one scan, a stop takes its own.

use super::contract::{gone_within, process_alive, Rig, LONG};
use super::local::LocalHost;
use super::PtyHost;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let end = Instant::now() + LONG;
    while !ready() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// With a snapshot that never ages out, the readers cannot see a process
/// forked after it, yet a kill ends that process: the stop scans afresh.
/// (Mutation check: make `soft_stop`, `await_tree_gone` and `release` call
/// `refresh_shared` instead of `refresh`, and this test fails.)
#[test]
fn a_stop_scans_afresh_and_ends_a_child_the_shared_snapshot_missed() {
    let host = Arc::new(LocalHost::new().with_scan_max_age(Duration::from_secs(3600)));
    let rig = Rig::new(Arc::clone(&host) as Arc<dyn PtyHost>, "scan-stop");
    let pane = rig.sh("echo ready; read x; \
         perl -MPOSIX -e 'POSIX::setsid() != -1 or die; print \"gc:$$\\n\"; \
         close STDIN; close STDOUT; close STDERR; sleep 60' & sleep 60");
    rig.sink.wait_text(&pane.id, "ready");
    wait_for("the first scan", || host.scans() >= 1);
    let scans = host.scans();
    super::contract::wait_write(&rig, &pane.id, b"\n");
    rig.sink.wait_text(&pane.id, "gc:");
    let text = rig.sink.text(&pane.id);
    let gc: i32 = text
        .split("gc:")
        .nth(1)
        .and_then(|rest| rest.trim().lines().next())
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("no grandchild pid in {text:?}"));
    // Several polls pass; every one reuses the old snapshot.
    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(host.scans(), scans, "the readers took a scan");
    assert!(process_alive(gc), "the grandchild died early");
    host.kill(&pane.id, Duration::from_secs(2)).unwrap();
    let ended = gone_within(gc, Duration::from_secs(5));
    if !ended {
        // SAFETY: kill on the pid the pane reported, to clean up.
        unsafe {
            libc::kill(gc, libc::SIGKILL);
        }
    }
    assert!(ended, "{gc} outlived kill: the stop used a stale snapshot");
    assert_eq!(
        host.scans(),
        scans,
        "a stop must not use the shared snapshot"
    );
}

/// Eight panes polling for descendants cost about one scan per tick, not
/// eight. The exact count (one scan per 250 ms window) is pinned by
/// `eight_pollers_take_exactly_one_scan_per_window` in `procs.rs`, where the
/// clock is supplied. Real pane readers wake every 200 ms, so their looks
/// land where the scheduler and the wake grid put them: panes started
/// together cluster and gave 3 scans in 1.5 s. The hard limit is one scan per
/// window, at most 7 in 1.5 s. Zero scans (no periodic look) and about 48 (no
/// sharing) both fail.
#[test]
fn eight_panes_share_one_scan_per_tick() {
    let host = Arc::new(LocalHost::new());
    let rig = Rig::new(Arc::clone(&host) as Arc<dyn PtyHost>, "scan-share");
    let panes: Vec<_> = (0..8).map(|_| rig.spawn(&["sleep", "30"])).collect();
    // Past the first look of every pane, and a few ticks in.
    std::thread::sleep(Duration::from_millis(700));
    let before = host.scans();
    std::thread::sleep(Duration::from_millis(1500));
    let taken = host.scans() - before;
    assert!(
        (2..=7).contains(&taken),
        "{taken} scans in 1.5 s for 8 panes, expected 2 to 7 (at most one per 250 ms window)"
    );
    for pane in &panes {
        host.kill(&pane.id, Duration::from_secs(2)).unwrap();
    }
}
