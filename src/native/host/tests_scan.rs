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

/// The window one scan serves. A literal, so a change to `SNAPSHOT_MAX_AGE`
/// fails the test rather than moving its expectation with it.
const WINDOW: Duration = Duration::from_millis(250);

/// Eight panes take exactly one scan per 250 ms window. Every look the pane
/// readers made is replayed against that rule: a look reuses the last scan
/// when the scan began within the window and not before the look's pane was
/// spawned, and otherwise a scan must have been taken. The expected count is
/// the number of scans the rule gives for the looks that really happened, so
/// it does not depend on the readers' wake timing. A doubled window, or a
/// look that scans when it could reuse, fails.
#[test]
fn eight_panes_share_one_scan_per_tick() {
    let host = Arc::new(LocalHost::new());
    let rig = Rig::new(Arc::clone(&host) as Arc<dyn PtyHost>, "scan-share");
    let panes: Vec<_> = (0..8).map(|_| rig.spawn(&["sleep", "30"])).collect();
    std::thread::sleep(Duration::from_millis(1500));
    let looks = host.scan_requests();
    assert!(
        looks.len() >= 8,
        "{} looks for 8 panes: not every pane looked",
        looks.len()
    );
    let mut began: Option<Instant> = None;
    let mut expected = 0;
    for look in &looks {
        let reusable = began
            .is_some_and(|b| b >= look.not_before && look.at.saturating_duration_since(b) < WINDOW);
        assert_eq!(
            look.scanned,
            !reusable,
            "look at {:?} (pane spawned at {:?}): the shared snapshot rule says {}",
            look.at,
            look.not_before,
            if reusable { "reuse" } else { "scan" }
        );
        if !reusable {
            began = Some(look.at);
            expected += 1;
        }
    }
    assert_eq!(
        host.scans(),
        expected,
        "scans taken for {} looks",
        looks.len()
    );
    assert!(expected >= 1);
    for pane in &panes {
        host.kill(&pane.id, Duration::from_secs(2)).unwrap();
    }
}
