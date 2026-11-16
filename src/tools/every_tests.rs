//! The `every` engine (issue #98), driven through the library with a fake
//! backend, an injected wall clock and injected tick instants. Nothing here
//! sleeps or reads the system clock.

use super::*;
use crate::audit::testing::{memory_audit, MemorySink};
use crate::backend::{BackendError, ErrorKind, ProgramStatus};
use crate::registry::MemoryRegistry;
use crate::routine_model::Status;
use crate::testutil::{FakeBackend, FakeShared, TempDir};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const W0: u64 = 1_000_000;

fn args(value: Value) -> Args {
    value.as_object().unwrap().clone()
}

/// An executable `kiro-cli` in `<dir>/bin`, which the policies' PATH names.
fn fake_kiro(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("kiro-cli");
    std::fs::write(&program, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::canonicalize(bin).unwrap()
}

/// Two kiro profiles: `kiro` with the given `dynamic_routines` (and idle
/// `routine`), and `plain` with neither.
fn policy(dir: &Path, dynamic: Option<Value>, routine: Option<Value>) -> LaunchPolicy {
    let bin = fake_kiro(dir).to_string_lossy().into_owned();
    let mut kiro = json!({"name": "kiro", "argv": ["kiro-cli", "chat"], "cwds": [dir]});
    if let Some(dynamic) = dynamic {
        kiro["dynamic_routines"] = dynamic;
    }
    if let Some(routine) = routine {
        kiro["routine"] = routine;
    }
    let text = json!({"profiles": [
        kiro,
        {"name": "plain", "argv": ["kiro-cli", "chat"], "cwds": [dir]}
    ]});
    LaunchPolicy::from_json(&text.to_string())
        .unwrap()
        .with_env_lookup(move |key| (key == "PATH").then(|| bin.clone()))
}

/// What the registry saw of each `set_routines` write, and the number of
/// sends the fake had seen when it arrived.
#[derive(Debug, Clone)]
struct Write {
    sent: usize,
    next_due: Vec<u64>,
    runs: Vec<u64>,
}

struct Spy {
    inner: MemoryRegistry,
    fake: FakeBackend,
    writes: Mutex<Vec<Write>>,
    refuse: AtomicBool,
}

impl Spy {
    fn new(fake: &FakeBackend) -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryRegistry::default(),
            fake: fake.clone(),
            writes: Mutex::new(Vec::new()),
            refuse: AtomicBool::new(false),
        })
    }

    fn writes(&self) -> Vec<Write> {
        self.writes.lock().unwrap().clone()
    }
}

impl PaneRegistry for Spy {
    fn set_agent(
        &self,
        pane_id: &str,
        agent: Option<&str>,
        profile: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<(), BackendError> {
        self.inner.set_agent(pane_id, agent, profile, cwd)
    }
    fn set_answered(&self, pane_id: &str, answered: bool) -> Result<(), BackendError> {
        self.inner.set_answered(pane_id, answered)
    }
    fn set_routine(
        &self,
        pane_id: &str,
        routine: Option<&RoutineState>,
    ) -> Result<(), BackendError> {
        self.inner.set_routine(pane_id, routine)
    }
    fn set_routines(&self, pane_id: &str, routines: &RoutineSet) -> Result<(), BackendError> {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(BackendError::new(ErrorKind::Other, "injected"));
        }
        self.writes.lock().unwrap().push(Write {
            sent: self.fake.state.lock().unwrap().sent.len(),
            next_due: routines.items.iter().map(|r| r.next_due).collect(),
            runs: routines.items.iter().map(|r| r.runs).collect(),
        });
        self.inner.set_routines(pane_id, routines)
    }
    fn forget(&self, pane_id: &str) -> Result<(), BackendError> {
        self.inner.forget(pane_id)
    }
    fn registered(&self) -> Vec<RegisteredPane> {
        self.inner.registered()
    }
}

struct Rig {
    sh: Shepherd,
    fake: FakeBackend,
    reg: Arc<Spy>,
    clock: Arc<AtomicU64>,
    sink: MemorySink,
    shared: Arc<FakeShared>,
    t0: Instant,
    dir: TempDir,
}

fn wall_clock(cell: &Arc<AtomicU64>) -> audit::Clock {
    let cell = Arc::clone(cell);
    Arc::new(move || Duration::from_secs(cell.load(Ordering::SeqCst)))
}

impl Rig {
    /// A front over a runnable fake with pane `work` started from the
    /// `kiro` profile, finished at turn 3, the wall clock at [`W0`].
    fn new(dynamic: Option<Value>, routine: Option<Value>, tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let fake = FakeBackend::new(true);
        fake.state.lock().unwrap().runnable = true;
        let reg = Spy::new(&fake);
        let clock = Arc::new(AtomicU64::new(W0));
        let (audit, sink) = memory_audit();
        let shared = Arc::new(FakeShared::default());
        let mut sh = Shepherd::with_policy(fake.clone(), policy(dir.path(), dynamic, routine))
            .with_registry(reg.clone())
            .with_wall_clock(wall_clock(&clock))
            .with_audit(Arc::new(audit))
            .with_shared_panes(shared.clone());
        sh.routine_lock_wait = Duration::from_millis(50);
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": "work"})),
        )
        .unwrap();
        let rig = Self {
            sh,
            fake,
            reg,
            clock,
            sink,
            shared,
            t0: Instant::now(),
            dir,
        };
        rig.pane(State::Done, 3);
        rig
    }

    fn easy(tag: &str) -> Self {
        Self::new(Some(json!({"min_every_secs": 60})), None, tag)
    }

    fn pane(&self, state: State, turn: u64) {
        let mut fake = self.fake.state.lock().unwrap();
        fake.panes[0].status = Some(ProgramStatus {
            state: state.clone(),
            kind: (state == State::Blocked).then(|| "permission".to_string()),
        });
        fake.panes[0].turn = Some(turn);
    }

    fn request(every_secs: u64) -> NewEvery {
        NewEvery {
            every_secs,
            text: "check the todo list and continue".to_string(),
            todo_file: None,
            expires_secs: None,
            max_runs: None,
            created_by: "agent-a".to_string(),
        }
    }

    fn create(&self, every_secs: u64) -> Routine {
        self.sh
            .create_every("work", Self::request(every_secs))
            .unwrap()
    }

    /// One pass at wall time `wall` (and the matching monotonic instant).
    fn tick(&self, wall: u64) {
        self.clock.store(wall, Ordering::SeqCst);
        self.sh
            .routine_tick_at_wall(self.t0 + Duration::from_secs(wall - W0), wall);
    }

    fn sent(&self) -> usize {
        self.fake.state.lock().unwrap().sent.len()
    }

    fn sends_tried(&self) -> usize {
        self.fake.calls().iter().filter(|c| **c == "send").count()
    }

    fn routine(&self, id: &str) -> Routine {
        self.sh
            .routines_of("work")
            .into_iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("no routine {id}"))
    }

    /// A new front over the same backend and registry, with `dynamic` as the
    /// profile's ceiling, after `adopt_registry`.
    fn restart(&self, dynamic: Option<Value>, routine: Option<Value>) -> Shepherd {
        let sh =
            Shepherd::with_policy(self.fake.clone(), policy(self.dir.path(), dynamic, routine))
                .with_registry(self.reg.clone())
                .with_wall_clock(wall_clock(&self.clock))
                .with_shared_panes(self.shared.clone());
        self.fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        assert!(
            sh.routines_of("work").is_empty(),
            "a new front starts empty"
        );
        sh.adopt_registry();
        sh
    }
}

#[test]
fn a_routine_fires_only_at_or_after_it_is_due() {
    let rig = Rig::easy("every-due");
    let r = rig.create(60);
    assert_eq!(r.id, "r1");
    assert_eq!(r.next_due, Some(W0 + 60));
    rig.tick(W0 + 59);
    assert_eq!(rig.sent(), 0, "fired before it was due");
    rig.tick(W0 + 60);
    assert_eq!(rig.sent(), 1);
    let r = rig.routine("r1");
    assert_eq!((r.runs, r.last_result.as_deref()), (1, Some("sent")));
    assert_eq!(r.next_due, Some(W0 + 120));
    rig.tick(W0 + 61);
    assert_eq!(rig.sent(), 1, "fired twice for one grid time");
}

#[test]
fn the_grid_does_not_drift_over_twenty_ticks_with_jitter() {
    let rig = Rig::easy("every-drift");
    rig.create(60);
    let jitter = [
        3u64, 0, 2, 1, 3, 0, 1, 2, 3, 0, 2, 1, 3, 1, 0, 2, 3, 1, 2, 0,
    ];
    for (k, j) in jitter.iter().enumerate() {
        let grid = W0 + 60 * (k as u64 + 1);
        rig.tick(grid - 1);
        assert_eq!(rig.sent(), k, "early tick {k}");
        rig.tick(grid + j);
        assert_eq!(rig.sent(), k + 1, "tick {k}");
        assert_eq!(rig.routine("r1").next_due, Some(grid + 60), "drift at {k}");
    }
    assert_eq!(rig.routine("r1").runs, 20);
}

#[test]
fn a_working_pane_is_skipped_and_nothing_is_queued() {
    let rig = Rig::easy("every-working");
    rig.create(60);
    rig.pane(State::Working, 4);
    rig.tick(W0 + 60);
    assert_eq!(rig.sends_tried(), 0);
    let r = rig.routine("r1");
    assert_eq!(r.last_result.as_deref(), Some("skipped:working"));
    assert_eq!((r.runs, r.next_due), (0, Some(W0 + 120)));
    // The pane finishes right after: the missed tick is not made up.
    rig.pane(State::Done, 5);
    rig.tick(W0 + 65);
    rig.tick(W0 + 119);
    assert_eq!(rig.sends_tried(), 0, "a skipped tick was queued");
    rig.tick(W0 + 120);
    assert_eq!(rig.sent(), 1);
    // Only the one send wrote audit lines: a call and its outcome.
    assert_eq!(rig.sink.records().len(), 2);
}

#[test]
fn a_blocked_pane_a_person_and_a_replayed_pane_are_skipped() {
    // Blocked: nothing is tried, and no audit line is written.
    let rig = Rig::easy("every-blocked");
    rig.create(60);
    rig.pane(State::Blocked, 3);
    rig.tick(W0 + 60);
    assert_eq!(rig.sends_tried(), 0);
    assert_eq!(
        rig.routine("r1").last_result.as_deref(),
        Some("skipped:blocked")
    );
    assert!(
        rig.sink.records().is_empty(),
        "a skipped tick wrote an audit line"
    );

    // A lease or a draft: the same, and the pre-check keeps the audit clean.
    for (lease, draft) in [(true, false), (false, true)] {
        let rig = Rig::easy("every-person");
        rig.create(60);
        rig.shared.person_at_the_pane(lease, draft);
        rig.tick(W0 + 60);
        assert_eq!(rig.sends_tried(), 0, "lease {lease} draft {draft}");
        assert_eq!(
            rig.routine("r1").last_result.as_deref(),
            Some("skipped:person")
        );
        assert!(rig.sink.records().is_empty());
        // The person leaves: the missed tick is gone, the next one fires.
        rig.shared.person_at_the_pane(false, false);
        rig.tick(W0 + 90);
        assert_eq!(rig.sends_tried(), 0, "a refused tick was queued");
        rig.tick(W0 + 120);
        assert_eq!(rig.sent(), 1);
    }

    // A replayed pane refuses the send itself: the tick is consumed.
    let rig = Rig::easy("every-replayed");
    rig.create(60);
    rig.fake.state.lock().unwrap().send_error =
        Some(BackendError::new(ErrorKind::Replayed, "not repainted"));
    rig.tick(W0 + 60);
    assert_eq!(rig.sent(), 0);
    let r = rig.routine("r1");
    assert_eq!(r.last_result.as_deref(), Some("skipped:replayed"));
    assert_eq!((r.runs, r.fail_streak, r.next_due), (0, 0, Some(W0 + 120)));
    rig.tick(W0 + 70);
    assert_eq!(rig.sends_tried(), 1, "a replayed tick was retried");
}

#[test]
fn a_pane_that_is_gone_ended_or_replaced_is_not_typed_into() {
    let rig = Rig::easy("every-gone");
    rig.create(60);
    rig.fake.state.lock().unwrap().panes.clear();
    rig.tick(W0 + 60);
    assert!(
        rig.sh.routines_of("work").is_empty(),
        "an unknown pane keeps its schedule"
    );

    let rig = Rig::easy("every-ended");
    rig.create(60);
    rig.fake.state.lock().unwrap().panes[0].live = false;
    rig.tick(W0 + 60);
    assert_eq!(rig.sends_tried(), 0);
    assert!(rig.sh.routines_of("work").is_empty());

    // The name now belongs to another pane: nothing is typed, nothing moves.
    let rig = Rig::easy("every-reused");
    rig.create(60);
    rig.fake.state.lock().unwrap().panes[0].id = "id-other".to_string();
    rig.tick(W0 + 60);
    assert_eq!(rig.sends_tried(), 0);
    let r = rig.routine("r1");
    assert_eq!((r.runs, r.next_due), (0, Some(W0 + 60)));
}

#[test]
fn a_tick_missed_during_downtime_is_skipped_not_caught_up() {
    let rig = Rig::easy("every-downtime");
    rig.create(3_600);
    // Three hours with no front: the first tick after finds three grid times
    // behind it and fires none of them.
    rig.tick(W0 + 3 * 3_600 + 7);
    assert_eq!(rig.sends_tried(), 0, "a missed tick was caught up");
    assert_eq!(rig.routine("r1").next_due, Some(W0 + 4 * 3_600));
    rig.tick(W0 + 4 * 3_600 - 1);
    assert_eq!(rig.sends_tried(), 0);
    rig.tick(W0 + 4 * 3_600);
    assert_eq!(rig.sent(), 1);
    assert_eq!(rig.routine("r1").next_due, Some(W0 + 5 * 3_600));
}

#[test]
fn a_clock_that_went_back_is_clamped_to_one_interval() {
    let rig = Rig::easy("every-clamp");
    rig.create(60);
    rig.reg
        .inner
        .set_routines("id-work", &{
            let mut set = rig.reg.inner.get("id-work").unwrap().routines;
            set.items[0].next_due = W0 + 100_000;
            set
        })
        .unwrap();
    let sh = rig.restart(Some(json!({"min_every_secs": 60})), None);
    sh.routine_tick_at_wall(rig.t0, W0 + 10);
    assert_eq!(sh.routines_of("work")[0].next_due, Some(W0 + 70));
    assert_eq!(sent_of(&rig.fake), 0);
    sh.routine_tick_at_wall(rig.t0, W0 + 70);
    assert_eq!(sent_of(&rig.fake), 1);
}

fn sent_of(fake: &FakeBackend) -> usize {
    fake.state.lock().unwrap().sent.len()
}

#[test]
fn expiry_and_max_runs_stop_a_routine() {
    let rig = Rig::easy("every-expiry");
    let mut request = Rig::request(60);
    request.expires_secs = Some(150);
    request.max_runs = Some(10);
    rig.sh.create_every("work", request).unwrap();
    rig.tick(W0 + 60);
    rig.tick(W0 + 120);
    assert_eq!(rig.sent(), 2);
    rig.tick(W0 + 150);
    assert_eq!(rig.routine("r1").status, Status::Expired);
    rig.tick(W0 + 180);
    assert_eq!(rig.sent(), 2, "an expired routine fired");

    let rig = Rig::easy("every-maxruns");
    let mut request = Rig::request(60);
    request.max_runs = Some(2);
    rig.sh.create_every("work", request).unwrap();
    for k in 1..=5 {
        rig.tick(W0 + 60 * k);
    }
    assert_eq!(rig.sent(), 2);
    assert_eq!(rig.routine("r1").status, Status::Done);
}

#[test]
fn three_failed_sends_give_up_and_a_success_resets_the_streak() {
    let fail = |rig: &Rig| {
        rig.fake.state.lock().unwrap().send_error =
            Some(BackendError::new(ErrorKind::Other, "boom"));
    };
    let rig = Rig::easy("every-giveup");
    rig.create(60);
    for k in 1..=3 {
        fail(&rig);
        rig.tick(W0 + 60 * k);
    }
    let r = rig.routine("r1");
    assert_eq!(r.status, Status::GaveUp);
    assert_eq!(r.fail_streak, 3);
    rig.tick(W0 + 240);
    assert_eq!(rig.sends_tried(), 3, "a routine that gave up tried again");

    // Two failures, a success, two failures: still going.
    let rig = Rig::easy("every-streak");
    rig.create(60);
    for k in 1..=2 {
        fail(&rig);
        rig.tick(W0 + 60 * k);
    }
    rig.tick(W0 + 180);
    assert_eq!(rig.routine("r1").fail_streak, 0);
    for k in 4..=5 {
        fail(&rig);
        rig.tick(W0 + 60 * k);
    }
    let r = rig.routine("r1");
    assert_eq!((r.status, r.fail_streak), (Status::Active, 2));
}

#[test]
fn a_pasted_but_unsubmitted_send_gives_up_at_once() {
    let rig = Rig::easy("every-pasted");
    rig.create(60);
    rig.fake.state.lock().unwrap().send_error = Some(crate::backend::pasted_unsubmitted(
        "id-work",
        "the program became blocked",
    ));
    rig.tick(W0 + 60);
    assert_eq!(rig.routine("r1").status, Status::GaveUp);
    rig.tick(W0 + 120);
    assert_eq!(rig.sends_tried(), 1, "a second copy was pasted");
}

#[test]
fn a_busy_pane_retries_instead_of_consuming_the_tick() {
    let rig = Rig::easy("every-busy");
    rig.create(60);
    {
        let _held = rig.sh.pane_locks().lock("id-work");
        rig.tick(W0 + 60);
    }
    assert_eq!(rig.sent(), 0);
    let r = rig.routine("r1");
    assert_eq!(r.next_due, Some(W0 + 60), "pane_busy consumed the tick");
    assert_eq!((r.runs, r.fail_streak), (0, 0));
    rig.tick(W0 + 65);
    assert_eq!(rig.sent(), 1);
    assert_eq!(rig.routine("r1").next_due, Some(W0 + 120));
}

#[test]
fn the_next_time_is_kept_before_the_text_is_typed() {
    let rig = Rig::easy("every-order");
    rig.create(60);
    rig.tick(W0 + 60);
    let writes = rig.reg.writes();
    // Create, then the advance before the send, then the outcome.
    assert_eq!(writes.len(), 3, "{writes:?}");
    assert_eq!(
        (writes[1].sent, writes[1].next_due.clone()),
        (0, vec![W0 + 120])
    );
    assert_eq!((writes[2].sent, writes[2].runs.clone()), (1, vec![1]));
}

#[test]
fn a_write_that_fails_means_no_send() {
    let rig = Rig::easy("every-nowrite");
    rig.create(60);
    rig.reg.refuse.store(true, Ordering::SeqCst);
    rig.tick(W0 + 60);
    assert_eq!(rig.sends_tried(), 0, "typed with the next time unkept");
    let r = rig.routine("r1");
    assert_eq!((r.runs, r.next_due), (0, Some(W0 + 60)));
    rig.reg.refuse.store(false, Ordering::SeqCst);
    rig.tick(W0 + 65);
    assert_eq!(rig.sent(), 1);
}

#[test]
fn a_front_restart_restores_the_same_grid() {
    let rig = Rig::easy("every-restart");
    rig.create(60);
    rig.tick(W0 + 62);
    assert_eq!(rig.sent(), 1);
    let sh = rig.restart(Some(json!({"min_every_secs": 60})), None);
    let r = sh.routines_of("work").remove(0);
    assert_eq!(
        (r.id.as_str(), r.runs, r.next_due, r.status),
        ("r1", 1, Some(W0 + 120), Status::Active)
    );
    sh.routine_tick_at_wall(rig.t0, W0 + 119);
    assert_eq!(sent_of(&rig.fake), 1);
    sh.routine_tick_at_wall(rig.t0, W0 + 121);
    assert_eq!(sent_of(&rig.fake), 2);
    assert_eq!(sh.routines_of("work")[0].next_due, Some(W0 + 180));
    // The counter survives too: the next id is r2.
    let again = sh.create_every("work", Rig::request(60)).unwrap();
    assert_eq!(again.id, "r2");
}

#[test]
fn a_routine_the_profile_no_longer_allows_is_stopped_not_typed() {
    // The floor was raised, the key was removed, the count was lowered.
    let rig = Rig::easy("every-ceiling");
    rig.create(60);
    for dynamic in [Some(json!({"min_every_secs": 120})), None] {
        let sh = rig.restart(dynamic, None);
        let r = sh.routines_of("work").remove(0);
        assert_eq!(r.status, Status::Stopped);
        assert_eq!(r.stop_reason.as_deref(), Some("profile_changed"));
        sh.routine_tick_at_wall(rig.t0, W0 + 60);
        sh.routine_tick_at_wall(rig.t0, W0 + 600);
        assert_eq!(sent_of(&rig.fake), 0, "a stopped routine was typed");
        let kept = rig.reg.inner.get("id-work").unwrap().routines;
        assert_eq!(kept.items[0].status, "stopped", "the stop was not kept");
    }

    let rig = Rig::easy("every-lowered");
    for _ in 0..3 {
        rig.create(60);
    }
    let sh = rig.restart(Some(json!({"min_every_secs": 60, "max_per_pane": 2})), None);
    let status: Vec<Status> = sh.routines_of("work").iter().map(|r| r.status).collect();
    assert_eq!(status, [Status::Active, Status::Active, Status::Stopped]);
}

#[test]
fn a_profile_that_is_gone_restores_nothing() {
    let rig = Rig::easy("every-noprofile");
    rig.create(60);
    let sh = Shepherd::with_policy(rig.fake.clone(), LaunchPolicy::default())
        .with_registry(rig.reg.clone());
    sh.adopt_registry();
    assert!(sh.routines_of("work").is_empty());
}

#[test]
fn two_routines_due_together_send_one_nudge_per_pass() {
    let rig = Rig::easy("every-one");
    rig.create(60);
    rig.create(60);
    rig.tick(W0 + 60);
    assert_eq!(rig.sent(), 1);
    assert_eq!(rig.routine("r1").last_result.as_deref(), Some("sent"));
    let second = rig.routine("r2");
    assert_eq!(second.last_result.as_deref(), Some("skipped:working"));
    assert_eq!(
        second.next_due,
        Some(W0 + 120),
        "the second tick was queued"
    );
}

#[test]
fn a_todo_file_with_nothing_left_skips_the_tick() {
    let rig = Rig::easy("every-todo");
    let mut request = Rig::request(60);
    request.todo_file = Some("TODO.md".to_string());
    rig.sh.create_every("work", request).unwrap();
    let todo = rig.dir.path().join("TODO.md");
    std::fs::write(&todo, "- [x] all done\n").unwrap();
    rig.tick(W0 + 60);
    assert_eq!(rig.sends_tried(), 0);
    assert_eq!(
        rig.routine("r1").last_result.as_deref(),
        Some("skipped:todo_done")
    );
    std::fs::write(&todo, "- [ ] one more\n").unwrap();
    rig.tick(W0 + 120);
    assert_eq!(rig.sent(), 1);
}

#[test]
fn a_nudge_is_recorded_as_the_routine_with_its_id_and_creator_and_never_its_text() {
    let rig = Rig::easy("every-audit");
    rig.create(60);
    rig.tick(W0 + 60);
    let records = rig.sink.records();
    assert_eq!(records.len(), 2, "{records:?}");
    assert_eq!(records[0]["phase"], "call");
    assert_eq!(records[1]["phase"], "outcome");
    for record in &records {
        assert_eq!(record["client"], audit::ROUTINE_ACTOR);
        assert_eq!(record["tool"], "send_input");
        assert_eq!(record["routine"], "r1");
        assert_eq!(record["created_by"], "agent-a");
    }
    assert_eq!(records[1]["outcome"], "ok");
    assert!(!rig.sink.bytes().contains("check the todo list"));
}

#[test]
fn the_consecutive_nudge_cap_does_not_apply_to_every() {
    // Decision D1. The idle routine of this profile gives up after 2 nudges;
    // an `every` routine on the same pane goes on far past that, with the
    // agent making no progress at all.
    let rig = Rig::new(
        Some(json!({"min_every_secs": 60})),
        Some(
            json!({"idle_timeout_secs": 3600, "max_consecutive_nudges": 2,
                    "nudge_text": "continue"}),
        ),
        "every-d1",
    );
    rig.create(60);
    for k in 1..=8 {
        rig.tick(W0 + 60 * k);
    }
    assert_eq!(rig.sent(), 8);
    let r = rig.routine("r1");
    assert_eq!((r.status, r.runs), (Status::Active, 8));
    let watch = rig.sh.routine_map();
    let state = watch.get("work").unwrap().state();
    assert_eq!((state.consecutive, state.gave_up), (0, false));
}

#[test]
fn an_every_nudge_is_not_progress_and_cannot_rearm_a_given_up_idle_routine() {
    let rig = Rig::new(
        Some(json!({"min_every_secs": 60})),
        Some(json!({"idle_timeout_secs": 10, "cooldown_secs": 1,
                    "max_consecutive_nudges": 1, "nudge_text": "continue"})),
        "every-foreign",
    );
    rig.create(60);
    rig.tick(W0);
    rig.tick(W0 + 11);
    assert_eq!(rig.sent(), 1, "the idle routine's nudge");
    rig.tick(W0 + 13);
    assert!(rig.sh.routine_map().get("work").unwrap().state().gave_up);
    // The turns the idle nudge caused use up its allowance.
    rig.pane(State::Done, 6);
    rig.tick(W0 + 15);
    assert_eq!(
        rig.sh
            .routine_map()
            .get("work")
            .unwrap()
            .state()
            .nudge_allowance,
        0
    );
    // The every nudge goes in; the turn it causes is not progress.
    rig.tick(W0 + 60);
    assert_eq!(rig.sent(), 2, "the every nudge");
    rig.pane(State::Done, 7);
    rig.tick(W0 + 65);
    assert!(
        rig.sh.routine_map().get("work").unwrap().state().gave_up,
        "an every nudge re-armed a given-up idle routine"
    );
}

#[test]
fn the_idle_cooldown_spaces_after_an_every_nudge() {
    let rig = Rig::new(
        Some(json!({"min_every_secs": 60})),
        Some(json!({"idle_timeout_secs": 70, "cooldown_secs": 100,
                    "max_consecutive_nudges": 5, "nudge_text": "continue"})),
        "every-cooldown",
    );
    rig.create(60);
    rig.tick(W0);
    rig.tick(W0 + 60);
    assert_eq!(rig.sent(), 1, "the every nudge");
    // The pane has now been idle past 70 s, but the cooldown runs from the
    // every nudge.
    rig.tick(W0 + 75);
    rig.tick(W0 + 100);
    assert_eq!(rig.sent(), 1, "the idle routine nudged inside the cooldown");
}

#[test]
fn a_create_is_refused_where_the_profile_does_not_allow_it() {
    let rig = Rig::easy("every-refused");
    // A pane from a profile with no ceiling.
    call(
        &rig.sh,
        "start_pane",
        &args(json!({"profile": "plain", "name": "other"})),
    )
    .unwrap();
    assert_eq!(
        rig.sh.create_every("other", Rig::request(60)).unwrap_err(),
        CreateError::NotAllowed
    );
    assert_eq!(
        rig.sh.create_every("nobody", Rig::request(60)).unwrap_err(),
        CreateError::NotAllowed
    );
    // The floor, the count and the text.
    let err = rig.sh.create_every("work", Rig::request(59)).unwrap_err();
    assert!(
        matches!(&err, CreateError::Invalid(i) if i.field == "every_secs"),
        "{err:?}"
    );
    let mut yes = Rig::request(60);
    yes.text = "yes".to_string();
    assert!(matches!(
        rig.sh.create_every("work", yes).unwrap_err(),
        CreateError::Invalid(i) if i.field == "text"
    ));
    for _ in 0..3 {
        rig.create(60);
    }
    assert!(matches!(
        rig.sh.create_every("work", Rig::request(60)).unwrap_err(),
        CreateError::Limit(_)
    ));
}

#[test]
fn a_create_whose_write_fails_changes_nothing() {
    let rig = Rig::easy("every-notsaved");
    rig.reg.refuse.store(true, Ordering::SeqCst);
    let err = rig.sh.create_every("work", Rig::request(60)).unwrap_err();
    assert!(matches!(err, CreateError::NotSaved(_)), "{err:?}");
    assert!(rig.sh.routines_of("work").is_empty());
    rig.reg.refuse.store(false, Ordering::SeqCst);
    assert_eq!(rig.create(60).id, "r1", "the failed create used up an id");
}
