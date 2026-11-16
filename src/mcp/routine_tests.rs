//! The four routine tools (issue #98, PR 4), called through `call_tool` the
//! way the server calls them: authorize, audit, run, audit. A fake backend,
//! an in-memory registry that can be told to refuse, injected clocks, no
//! sleeps and no server.

use super::*;
use crate::agent::Agent;
use crate::audit::testing::{failing_audit, MemorySink};
use crate::audit::{Audit, AuditSink};
use crate::backend::{BackendError, ErrorKind, ProgramStatus, State};
use crate::clients::{Caller, ProfileAccess, Scope};
use crate::profiles::LaunchPolicy;
use crate::registry::{MemoryRegistry, PaneRegistry, RegisteredPane, RoutineSet, RoutineState};
use crate::schedule::PaneSchedule;
use crate::testutil::{FakeBackend, TempDir};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const W0: u64 = 2_000_000;
/// A text that must never reach the audit file.
const CANARY: &str = "CANARY-routine-text-7f3a";

fn fake_kiro(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let program = bin.join("kiro-cli");
    std::fs::write(&program, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::canonicalize(bin).unwrap()
}

/// `kiro` allows routines (`dynamic`, when given) and may carry an idle
/// `routine`; `plain` allows none.
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

/// A registry that keeps what it is given and can be told to refuse the
/// routines write.
struct Registry {
    inner: MemoryRegistry,
    refuse: AtomicBool,
    writes: AtomicU64,
}

impl PaneRegistry for Registry {
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
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.set_routines(pane_id, routines)
    }
    fn forget(&self, pane_id: &str) -> Result<(), BackendError> {
        self.inner.forget(pane_id)
    }
    fn registered(&self) -> Vec<RegisteredPane> {
        self.inner.registered()
    }
}

/// An audit sink that fails while `fail` is set.
struct Switch {
    inner: MemorySink,
    fail: Arc<AtomicBool>,
}

impl AuditSink for Switch {
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(io::Error::other("disk full"));
        }
        self.inner.write_line(line)
    }
}

struct Rig {
    sh: Shepherd,
    fake: FakeBackend,
    reg: Arc<Registry>,
    clock: Arc<AtomicU64>,
    sink: MemorySink,
    audit_fails: Arc<AtomicBool>,
    _dir: TempDir,
}

fn caller(scopes: &[Scope]) -> Caller {
    Caller::new("agent-a", scopes.iter().copied(), ProfileAccess::Any)
}

fn agent_a() -> Caller {
    caller(&[Scope::Routine])
}

fn agent_b() -> Caller {
    Caller::new("agent-b", [Scope::Routine], ProfileAccess::Any)
}

fn every(text: &str, secs: u64) -> Value {
    json!({"terminal": "work", "text": text,
           "trigger": {"type": "every", "every_secs": secs}})
}

impl Rig {
    fn build(dynamic: Option<Value>, idle: Option<Value>, audit: Option<Arc<Audit>>) -> Self {
        let dir = TempDir::new("routine-tools");
        let fake = FakeBackend::new(true);
        fake.state.lock().unwrap().runnable = true;
        let reg = Arc::new(Registry {
            inner: MemoryRegistry::default(),
            refuse: AtomicBool::new(false),
            writes: AtomicU64::new(0),
        });
        let clock = Arc::new(AtomicU64::new(W0));
        let cell = Arc::clone(&clock);
        let audit_fails = Arc::new(AtomicBool::new(false));
        let sink = MemorySink::default();
        let audit = audit.unwrap_or_else(|| {
            Arc::new(Audit::new(
                Box::new(Switch {
                    inner: sink.clone(),
                    fail: Arc::clone(&audit_fails),
                }),
                crate::audit::testing::fixed_clock(1_700_000_000),
            ))
        });
        let sh = Shepherd::with_policy(fake.clone(), policy(dir.path(), dynamic, idle))
            .with_registry(reg.clone())
            .with_wall_clock(Arc::new(move || {
                Duration::from_secs(cell.load(Ordering::SeqCst))
            }))
            .with_audit(audit);
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        let rig = Self {
            sh,
            fake,
            reg,
            clock,
            sink,
            audit_fails,
            _dir: dir,
        };
        rig.start("kiro", "work");
        rig.pane(State::Done, 3);
        rig
    }

    /// Pane `work` from `kiro` with a ceiling that allows 60 s routines.
    fn easy() -> Self {
        Self::build(Some(json!({"min_every_secs": 60})), None, None)
    }

    fn start(&self, profile: &str, name: &str) {
        let args = json!({"profile": profile, "name": name});
        tools::call(&self.sh, "start_pane", args.as_object().unwrap()).unwrap();
    }

    fn pane(&self, state: State, turn: u64) {
        let mut fake = self.fake.state.lock().unwrap();
        fake.panes[0].status = Some(ProgramStatus {
            state: state.clone(),
            kind: None,
        });
        fake.panes[0].turn = Some(turn);
    }

    fn run(&self, who: &Caller, tool: &str, arguments: Value) -> Value {
        let params = json!({"name": tool, "arguments": arguments});
        call_tool(&self.sh, who, Some(&params)).expect("a tool result")
    }

    fn create(&self, text: &str, secs: u64) -> Value {
        self.run(&agent_a(), "routine_create", every(text, secs))
    }

    fn list(&self) -> Vec<Value> {
        let out = self.run(&agent_a(), "routine_list", json!({}));
        assert_eq!(out["isError"], false, "{out}");
        out["structuredContent"]["routines"]
            .as_array()
            .unwrap()
            .clone()
    }

    fn sent(&self) -> Vec<String> {
        self.fake
            .state
            .lock()
            .unwrap()
            .sent
            .iter()
            .map(|(text, _)| text.clone())
            .collect()
    }

    /// One pass of both engines at wall time `wall`.
    fn tick(&self, wall: u64) {
        self.clock.store(wall, Ordering::SeqCst);
        self.sh.routine_tick_at_wall(Instant::now(), wall);
    }
}

fn code(out: &Value) -> String {
    out["structuredContent"]["error"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn data(out: &Value) -> &Value {
    &out["structuredContent"]
}

#[test]
fn the_legacy_token_gets_forbidden_and_does_not_see_the_tools() {
    let rig = Rig::easy();
    let legacy = Caller::default_client();
    assert!(!legacy.has(Scope::Routine));
    for tool in [
        "routine_create",
        "routine_list",
        "routine_update",
        "routine_delete",
    ] {
        let arguments = match tool {
            "routine_create" => every("check the todo list", 600),
            "routine_list" => json!({}),
            _ => json!({"terminal": "work", "id": "r1", "text": "check again"}),
        };
        let out = rig.run(&legacy, tool, arguments);
        assert_eq!(out["isError"], true, "{tool}");
        assert_eq!(code(&out), "forbidden", "{tool}: {out}");
        assert_eq!(data(&out)["scope"], "routine", "{tool}");
    }
    let listed = visible_tools(&legacy);
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.iter().all(|name| !name.starts_with("routine_")));
    // Nothing was created and nothing was written for a restart.
    assert!(rig.sh.routines_of("work").is_empty());
    assert_eq!(rig.reg.writes.load(Ordering::SeqCst), 0);
    // The refusal is on file as a denied call, not a call.
    let records = rig.sink.records();
    assert!(
        records.iter().all(|r| r["phase"] == "denied"),
        "{records:?}"
    );
}

#[test]
fn a_routine_client_sees_exactly_the_four_tools_and_a_bare_one_sees_none() {
    let names = |who: &Caller| -> Vec<String> {
        visible_tools(who)
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(
        names(&agent_a()),
        [
            "routine_create",
            "routine_list",
            "routine_update",
            "routine_delete"
        ]
    );
    assert!(names(&caller(&[Scope::Observe, Scope::Control]))
        .iter()
        .all(|n| !n.starts_with("routine_")));
    // The scope does not imply `control`: it opens no other tool.
    let rig = Rig::easy();
    let out = rig.run(
        &agent_a(),
        "send_input",
        json!({"terminal": "work", "text": "hello"}),
    );
    assert_eq!(code(&out), "forbidden", "{out}");
    assert!(rig.sent().is_empty());
}

#[test]
fn a_routine_client_creates_lists_updates_and_deletes() {
    let rig = Rig::easy();
    let out = rig.create("check the todo list", 600);
    assert_eq!(out["isError"], false, "{out}");
    let created = &data(&out)["routine"];
    assert_eq!(created["id"], "r1");
    assert_eq!(created["terminal"], "work");
    assert_eq!(created["source"], "agent");
    assert_eq!(created["created_by"], "agent-a");
    assert_eq!(created["status"], "active");
    assert_eq!(
        created["trigger"],
        json!({"type": "every", "every_secs": 600})
    );
    assert_eq!(created["next_due_in_secs"], 600);
    assert_eq!(created["expires_in_secs"], 604_800);
    assert_eq!(
        data(&out)["ceiling"],
        json!({"min_every_secs": 60, "max_per_pane": 3, "max_expires_secs": 604_800,
               "active": 1, "free": 2})
    );

    rig.clock.store(W0 + 100, Ordering::SeqCst);
    let listed = rig.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], "r1");
    assert_eq!(listed[0]["next_due_in_secs"], 500);
    assert_eq!(listed[0]["last_result"], Value::Null);

    let out = rig.run(
        &agent_a(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "text": "look at the list again",
               "every_secs": 120, "max_runs": 5}),
    );
    assert_eq!(out["isError"], false, "{out}");
    let updated = &data(&out)["routine"];
    assert_eq!(updated["action"]["text"], "look at the list again");
    assert_eq!(updated["trigger"]["every_secs"], 120);
    assert_eq!(updated["max_runs"], 5);
    // A new interval restarts the grid from now.
    assert_eq!(updated["next_due_in_secs"], 120);
    assert_eq!(updated["created_by"], "agent-a");

    let out = rig.run(
        &agent_a(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "max_runs": null}),
    );
    assert_eq!(data(&out)["routine"]["max_runs"], Value::Null, "{out}");

    let out = rig.run(
        &agent_a(),
        "routine_delete",
        json!({"terminal": "work", "id": "r1"}),
    );
    assert_eq!(
        data(&out),
        &json!({"terminal": "work", "id": "r1", "deleted": true}),
        "{out}"
    );
    assert!(rig.list().is_empty());
    let again = rig.run(
        &agent_a(),
        "routine_delete",
        json!({"terminal": "work", "id": "r1"}),
    );
    assert_eq!(code(&again), "routine_not_found", "{again}");
}

#[test]
fn a_created_routine_types_its_text_unchanged_on_the_grid() {
    let rig = Rig::easy();
    rig.create("check the todo list", 60);
    rig.tick(W0 + 59);
    assert!(rig.sent().is_empty());
    rig.tick(W0 + 60);
    assert_eq!(rig.sent(), ["check the todo list"]);
    let listed = rig.list();
    assert_eq!(listed[0]["runs"], 1);
    assert_eq!(listed[0]["last_result"], "sent");
    assert_eq!(listed[0]["last_run_at"], W0 + 60);
}

#[test]
fn each_ceiling_refusal_has_its_code_with_the_field_and_the_limit() {
    let rig = Rig::build(
        Some(json!({"min_every_secs": 300, "max_per_pane": 2, "max_expires_secs": 7200})),
        None,
        None,
    );
    let refused = |out: &Value, error: &str, field: &str| {
        assert_eq!(out["isError"], true, "{out}");
        assert_eq!(code(out), error, "{out}");
        assert_eq!(data(out)["field"], field, "{out}");
        assert!(data(out)["limit"].as_str().is_some_and(|l| !l.is_empty()));
    };

    // The interval floor, and the hard ceiling of a week.
    let low = rig.create("check the todo list", 299);
    refused(&low, "routine_invalid", "every_secs");
    assert_eq!(data(&low)["limit"], "300..=604800");
    refused(
        &rig.create("check the todo list", 604_801),
        "routine_invalid",
        "every_secs",
    );
    // Expiry above the profile's maximum, and below the minimum.
    let mut long = every("check the todo list", 300);
    long["expires_secs"] = json!(7201);
    let out = rig.run(&agent_a(), "routine_create", long);
    refused(&out, "routine_invalid", "expires_secs");
    assert_eq!(data(&out)["limit"], "60..=7200");
    let mut short = every("check the todo list", 300);
    short["expires_secs"] = json!(59);
    refused(
        &rig.run(&agent_a(), "routine_create", short),
        "routine_invalid",
        "expires_secs",
    );
    // max_runs.
    let mut runs = every("check the todo list", 300);
    runs["max_runs"] = json!(0);
    refused(
        &rig.run(&agent_a(), "routine_create", runs),
        "routine_invalid",
        "max_runs",
    );
    // Text: a bare confirmation, too long, a control character, empty.
    for bad in ["yes", "1", "ok"] {
        refused(&rig.create(bad, 300), "routine_invalid", "text");
    }
    let too_long = rig.create(&"x".repeat(1025), 300);
    refused(&too_long, "routine_invalid", "text");
    assert_eq!(data(&too_long)["limit"], "1024 bytes");
    refused(
        &rig.create("line one\nline two", 300),
        "routine_invalid",
        "text",
    );
    refused(&rig.create("   ", 300), "routine_invalid", "text");
    // The text at exactly the limit is accepted.
    let edge = rig.create(&"x".repeat(1024), 300);
    assert_eq!(edge["isError"], false, "{edge}");
    // A todo file must stay inside the working directory.
    let mut todo = every("check the todo list", 300);
    todo["todo_file"] = json!("../outside.md");
    refused(
        &rig.run(&agent_a(), "routine_create", todo),
        "routine_invalid",
        "todo_file",
    );
    // The count: two are allowed, the third is refused with the maximum.
    let second = rig.create("second routine text", 300);
    assert_eq!(second["isError"], false, "{second}");
    let third = rig.create("third routine text", 300);
    assert_eq!(code(&third), "routine_limit", "{third}");
    assert_eq!(data(&third)["max"], 2);
    assert_eq!(data(&third)["field"], "routines");
    // Nothing but the two valid routines exists.
    assert_eq!(rig.list().len(), 2);
}

#[test]
fn a_trigger_other_than_every_is_refused() {
    let rig = Rig::easy();
    let idle = json!({"terminal": "work", "text": "check the todo list",
                      "trigger": {"type": "idle", "idle_secs": 600}});
    let out = rig.run(&agent_a(), "routine_create", idle);
    assert_eq!(code(&out), "routine_invalid", "{out}");
    assert_eq!(data(&out)["field"], "trigger.type");
    assert_eq!(data(&out)["limit"], "every");
    // Malformed arguments are plain invalid_input.
    for bad in [
        json!({"terminal": "work", "text": "x y z"}),
        json!({"terminal": "work", "text": "x y z", "trigger": "every"}),
        json!({"terminal": "work", "text": "x y z", "trigger": {"type": "every"}}),
        json!({"terminal": "work", "text": "x y z",
               "trigger": {"type": "every", "every_secs": "600"}}),
        json!({"terminal": "work", "text": "x y z",
               "trigger": {"type": "every", "every_secs": 600.5}}),
        json!({"terminal": "work", "trigger": {"type": "every", "every_secs": 600}}),
    ] {
        let out = rig.run(&agent_a(), "routine_create", bad.clone());
        assert_eq!(code(&out), "invalid_input", "{bad}: {out}");
    }
    assert!(rig.list().is_empty());
}

#[test]
fn a_todo_file_needs_a_pane_with_a_local_working_directory() {
    let rig = Rig::easy();
    let id = rig.fake.state.lock().unwrap().panes[0].id.clone();
    rig.sh
        .register_schedule("work", PaneSchedule::new(id, Agent::Kiro, None));
    let mut todo = every("check the todo list", 600);
    todo["todo_file"] = json!("TODO.md");
    let out = rig.run(&agent_a(), "routine_create", todo);
    assert_eq!(code(&out), "routine_invalid", "{out}");
    assert_eq!(data(&out)["field"], "todo_file");
    assert_eq!(data(&out)["limit"], "a pane with a local working directory");
    // Without a todo file the same pane takes a routine.
    assert_eq!(rig.create("check the todo list", 600)["isError"], false);
}

#[test]
fn a_pane_whose_profile_does_not_allow_routines_is_refused_and_nothing_changes() {
    // The profile has no dynamic_routines at all.
    let rig = Rig::build(None, None, None);
    let out = rig.create("check the todo list", 600);
    assert_eq!(code(&out), "routines_not_allowed", "{out}");
    assert!(out["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("dynamic_routines"));
    assert!(rig.list().is_empty());
    assert_eq!(rig.reg.writes.load(Ordering::SeqCst), 0);
    let none = rig.run(
        &agent_a(),
        "routine_delete",
        json!({"terminal": "work", "id": "r1"}),
    );
    assert_eq!(code(&none), "routine_not_found", "{none}");

    // One profile allows them, another does not: the pane decides.
    let rig = Rig::easy();
    rig.start("plain", "other");
    let mut args = every("check the todo list", 600);
    args["terminal"] = json!("other");
    let out = rig.run(&agent_a(), "routine_create", args);
    assert_eq!(code(&out), "routines_not_allowed", "{out}");
}

#[test]
fn a_pane_that_is_missing_or_finished_is_reported_as_such() {
    let rig = Rig::easy();
    let mut missing = every("check the todo list", 600);
    missing["terminal"] = json!("nope");
    assert_eq!(
        code(&rig.run(&agent_a(), "routine_create", missing)),
        "not_found"
    );
    rig.fake.state.lock().unwrap().panes[0].live = false;
    let out = rig.create("check the todo list", 600);
    assert_eq!(code(&out), "pane_not_live", "{out}");
    let out = rig.run(&agent_a(), "routine_list", json!({"terminal": "work"}));
    assert_eq!(code(&out), "pane_not_live", "{out}");
}

#[test]
fn a_failed_write_refuses_the_change_and_changes_nothing() {
    let rig = Rig::easy();
    rig.reg.refuse.store(true, Ordering::SeqCst);
    let out = rig.create("check the todo list", 600);
    assert_eq!(code(&out), "routine_not_saved", "{out}");
    assert!(rig.sh.routines_of("work").is_empty(), "memory changed");
    assert!(rig.list().is_empty());

    // The refused create did not use up an id.
    rig.reg.refuse.store(false, Ordering::SeqCst);
    let out = rig.create("check the todo list", 600);
    assert_eq!(data(&out)["routine"]["id"], "r1", "{out}");
    let before = rig.sh.routines_of("work");

    rig.reg.refuse.store(true, Ordering::SeqCst);
    let out = rig.run(
        &agent_a(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "text": "something else entirely"}),
    );
    assert_eq!(code(&out), "routine_not_saved", "{out}");
    assert_eq!(rig.sh.routines_of("work"), before, "update changed memory");
    let out = rig.run(
        &agent_a(),
        "routine_delete",
        json!({"terminal": "work", "id": "r1"}),
    );
    assert_eq!(code(&out), "routine_not_saved", "{out}");
    assert_eq!(rig.sh.routines_of("work"), before, "delete changed memory");
}

#[test]
fn what_was_acknowledged_is_what_the_registry_holds() {
    let rig = Rig::easy();
    rig.create("check the todo list", 600);
    let id = rig.fake.state.lock().unwrap().panes[0].id.clone();
    let stored = |rig: &Rig| -> RoutineSet {
        rig.reg
            .registered()
            .into_iter()
            .find(|p| p.pane_id == id)
            .map(|p| p.routines)
            .unwrap()
    };
    assert_eq!(stored(&rig).items.len(), 1);
    assert_eq!(stored(&rig).items[0].created_by, "agent-a");
    rig.run(
        &agent_a(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "text": "a new instruction"}),
    );
    assert_eq!(stored(&rig).items[0].text, "a new instruction");
    rig.run(
        &agent_a(),
        "routine_delete",
        json!({"terminal": "work", "id": "r1"}),
    );
    assert!(stored(&rig).items.is_empty());
    // The id counter outlives the delete: an id is never reused.
    assert_eq!(stored(&rig).seq, 1);
    let out = rig.create("check the todo list", 600);
    assert_eq!(data(&out)["routine"]["id"], "r2");
}

#[test]
fn a_failing_audit_refuses_create_update_and_delete_and_the_list_still_works() {
    // Create: nothing is made.
    let rig = Rig::easy();
    rig.audit_fails.store(true, Ordering::SeqCst);
    let out = rig.create("check the todo list", 600);
    assert_eq!(code(&out), "audit_unavailable", "{out}");
    assert!(rig.sh.routines_of("work").is_empty());
    assert_eq!(rig.reg.writes.load(Ordering::SeqCst), 0);
    // The read does not need the audit.
    assert!(rig.list().is_empty());

    // Update and delete: the routine stays as it was.
    rig.audit_fails.store(false, Ordering::SeqCst);
    rig.create("check the todo list", 600);
    let before = rig.sh.routines_of("work");
    rig.audit_fails.store(true, Ordering::SeqCst);
    let out = rig.run(
        &agent_a(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "text": "a different line"}),
    );
    assert_eq!(code(&out), "audit_unavailable", "{out}");
    let out = rig.run(
        &agent_a(),
        "routine_delete",
        json!({"terminal": "work", "id": "r1"}),
    );
    assert_eq!(code(&out), "audit_unavailable", "{out}");
    assert_eq!(rig.sh.routines_of("work"), before);
    assert_eq!(rig.list().len(), 1);

    // A sink that never works, as the server's own failing audit.
    let rig = Rig::build(
        Some(json!({"min_every_secs": 60})),
        None,
        Some(Arc::new(failing_audit())),
    );
    let out = rig.create("check the todo list", 600);
    assert_eq!(code(&out), "audit_unavailable", "{out}");
    assert!(rig.sh.routines_of("work").is_empty());
}

#[test]
fn the_audit_has_a_call_and_an_outcome_and_never_the_text() {
    let rig = Rig::easy();
    let mut create = every(CANARY, 600);
    create["todo_file"] = json!("TODO.md");
    create["max_runs"] = json!(7);
    let out = rig.run(&agent_a(), "routine_create", create);
    assert_eq!(out["isError"], false, "{out}");
    rig.run(
        &agent_a(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "text": format!("{CANARY} again"),
               "every_secs": 900}),
    );
    rig.run(
        &agent_a(),
        "routine_delete",
        json!({"terminal": "work", "id": "r1"}),
    );
    // A refused create (the text is a bare answer) is on file too.
    rig.run(&agent_a(), "routine_create", every("yes", 600));
    rig.run(&agent_a(), "routine_list", json!({}));

    let bytes = rig.sink.bytes();
    assert!(
        !bytes.contains(CANARY),
        "the text reached the audit:\n{bytes}"
    );
    let records = rig.sink.records();
    let of = |tool: &str, phase: &str| -> Vec<&Value> {
        records
            .iter()
            .filter(|r| r["tool"] == tool && r["phase"] == phase)
            .collect()
    };
    let call = of("routine_create", "call")[0];
    assert_eq!(call["client"], "agent-a");
    assert_eq!(call["pane"], "work");
    assert_eq!(call["trigger"], "every");
    assert_eq!(call["every_secs"], 600);
    assert_eq!(call["text_len"], CANARY.len());
    assert_eq!(call["max_runs"], 7);
    assert_eq!(call["todo"], true);
    assert!(call.get("text").is_none() && call.get("todo_file").is_none());
    let outcome = of("routine_create", "outcome")[0];
    assert_eq!(outcome["outcome"], "ok");
    assert_eq!(outcome["routine"], "r1");
    assert_eq!(
        of("routine_create", "outcome")[1]["outcome"],
        "error:routine_invalid"
    );
    let update = of("routine_update", "call")[0];
    assert_eq!(update["routine"], "r1");
    assert_eq!(update["changed"], json!(["every_secs", "text"]));
    assert_eq!(update["text_len"], CANARY.len() + " again".len());
    assert_eq!(of("routine_update", "outcome")[0]["outcome"], "ok");
    let delete = of("routine_delete", "call")[0];
    assert_eq!(
        (delete["pane"].as_str(), delete["routine"].as_str()),
        (Some("work"), Some("r1"))
    );
    assert_eq!(of("routine_delete", "outcome")[0]["outcome"], "ok");
    // The list writes nothing.
    assert!(records.iter().all(|r| r["tool"] != "routine_list"));
}

#[test]
fn a_denied_call_records_no_text_either() {
    let rig = Rig::easy();
    rig.run(
        &Caller::default_client(),
        "routine_create",
        every(CANARY, 600),
    );
    let bytes = rig.sink.bytes();
    assert!(!bytes.contains(CANARY), "{bytes}");
    assert_eq!(rig.sink.records()[0]["phase"], "denied");
}

#[test]
fn any_routine_client_may_manage_a_routine_on_a_pane_another_one_made() {
    let rig = Rig::easy();
    rig.create("check the todo list", 600);
    let out = rig.run(
        &agent_b(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "text": "b rewrote this one"}),
    );
    assert_eq!(out["isError"], false, "{out}");
    assert_eq!(data(&out)["routine"]["created_by"], "agent-a");
    let listed = rig.run(&agent_b(), "routine_list", json!({}));
    assert_eq!(data(&listed)["routines"][0]["created_by"], "agent-a");
    let out = rig.run(
        &agent_b(),
        "routine_delete",
        json!({"terminal": "work", "id": "r1"}),
    );
    assert_eq!(out["isError"], false, "{out}");
    // The audit names who acted.
    let records = rig.sink.records();
    let by: Vec<&str> = records
        .iter()
        .filter(|r| r["phase"] == "call")
        .map(|r| r["client"].as_str().unwrap())
        .collect();
    assert_eq!(by, ["agent-a", "agent-b", "agent-b"]);
    // A routine made by b is stamped b.
    let out = rig.run(&agent_b(), "routine_create", every("b's own routine", 600));
    assert_eq!(data(&out)["routine"]["created_by"], "agent-b");
}

#[test]
fn the_list_covers_every_pane_that_has_routines_and_only_those() {
    let rig = Rig::easy();
    rig.start("plain", "other");
    rig.start("kiro", "second");
    rig.create("check the todo list", 600);
    assert_eq!(rig.list().len(), 1, "a pane with none is not listed");
    let mut args = every("second pane routine", 600);
    args["terminal"] = json!("second");
    rig.run(&agent_a(), "routine_create", args);
    let listed = rig.list();
    let panes: Vec<&str> = listed
        .iter()
        .map(|r| r["terminal"].as_str().unwrap())
        .collect();
    assert_eq!(panes, ["second", "work"]);
    let one = rig.run(&agent_a(), "routine_list", json!({"terminal": "work"}));
    assert_eq!(data(&one)["routines"].as_array().unwrap().len(), 1);
}

#[test]
fn the_profile_idle_routine_is_listed_read_only_and_cannot_be_changed() {
    let idle = json!({"idle_timeout_secs": 3600, "cooldown_secs": 600,
                      "max_consecutive_nudges": 3, "nudge_text": "continue the work"});
    let rig = Rig::build(Some(json!({"min_every_secs": 60})), Some(idle), None);
    rig.create("check the todo list", 600);
    let listed = rig.list();
    assert_eq!(listed.len(), 2);
    let profile = &listed[0];
    assert_eq!(profile["id"], "profile");
    assert_eq!(profile["source"], "profile");
    assert_eq!(profile["terminal"], "work");
    assert_eq!(profile["trigger"]["type"], "idle");
    assert_eq!(profile["trigger"]["idle_secs"], 3600);
    assert_eq!(profile["status"], "active");
    assert_eq!(profile["last_result"], Value::Null);
    assert_eq!(profile["next_due_in_secs"], Value::Null, "not timing yet");
    assert_eq!(listed[1]["id"], "r1");

    // One pass starts the idle clock on the finished turn.
    rig.sh.routine_tick_at_wall(Instant::now(), W0);
    let timing = rig.list()[0]["next_due_in_secs"].as_u64().unwrap();
    assert!((3590..=3600).contains(&timing), "{timing}");

    for (tool, extra) in [
        ("routine_update", json!({"text": "change the profile line"})),
        ("routine_delete", json!({})),
    ] {
        let mut args = json!({"terminal": "work", "id": "profile"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let out = rig.run(&agent_a(), tool, args);
        assert_eq!(code(&out), "routine_read_only", "{tool}: {out}");
    }
    assert_eq!(rig.list().len(), 2);
}

#[test]
fn an_update_checks_the_same_limits_and_reports_what_it_cannot_do() {
    let rig = Rig::easy();
    rig.create("check the todo list", 600);
    let update = |extra: Value| {
        let mut args = json!({"terminal": "work", "id": "r1"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        rig.run(&agent_a(), "routine_update", args)
    };
    let out = update(json!({"every_secs": 59}));
    assert_eq!(
        (code(&out).as_str(), data(&out)["field"].as_str()),
        ("routine_invalid", Some("every_secs"))
    );
    let out = update(json!({"text": "yes"}));
    assert_eq!(data(&out)["field"], "text", "{out}");
    let out = update(json!({"expires_secs": 604_801}));
    assert_eq!(data(&out)["field"], "expires_secs", "{out}");
    let out = update(json!({"todo_file": "/etc/passwd"}));
    assert_eq!(data(&out)["field"], "todo_file", "{out}");
    let out = update(json!({}));
    assert_eq!(code(&out), "invalid_input", "{out}");
    let out = update(json!({"id": "r9", "text": "no such routine"}));
    assert_eq!(code(&out), "routine_not_found", "{out}");
    // Nothing above changed the routine.
    let r = &rig.list()[0];
    assert_eq!(r["trigger"]["every_secs"], 600);
    assert_eq!(r["action"]["text"], "check the todo list");
    // A todo file can be set and cleared.
    let out = update(json!({"todo_file": "TODO.md"}));
    assert_eq!(data(&out)["routine"]["todo_file"], "TODO.md", "{out}");
    let out = update(json!({"todo_file": null}));
    assert_eq!(data(&out)["routine"]["todo_file"], Value::Null, "{out}");
}

#[test]
fn an_expired_routine_comes_back_with_an_expiry_and_respects_the_count() {
    let rig = Rig::build(
        Some(json!({"min_every_secs": 60, "max_per_pane": 1, "max_expires_secs": 3600})),
        None,
        None,
    );
    let mut args = every("check the todo list", 60);
    args["expires_secs"] = json!(120);
    rig.run(&agent_a(), "routine_create", args);
    rig.tick(W0 + 200);
    let r = rig.list()[0].clone();
    assert_eq!(r["status"], "expired", "{r}");
    assert_eq!(r["next_due_in_secs"], Value::Null);
    // It does not count toward the limit: a new one fits.
    let second = rig.create("second routine text", 60);
    assert_eq!(second["isError"], false, "{second}");
    // Renewing the first would be a second active one.
    let out = rig.run(
        &agent_a(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "expires_secs": 600}),
    );
    assert_eq!(code(&out), "routine_limit", "{out}");
    rig.run(
        &agent_a(),
        "routine_delete",
        json!({"terminal": "work", "id": "r2"}),
    );
    let out = rig.run(
        &agent_a(),
        "routine_update",
        json!({"terminal": "work", "id": "r1", "expires_secs": 600}),
    );
    assert_eq!(out["isError"], false, "{out}");
    let r = &data(&out)["routine"];
    assert_eq!(r["status"], "active");
    assert_eq!(r["expires_in_secs"], 600);
    assert_eq!(r["next_due_in_secs"], 60);
}

#[test]
fn a_client_that_does_not_know_the_tools_sees_the_old_surface_unchanged() {
    let rig = Rig::easy();
    // The old tools answer a legacy caller exactly as before.
    let legacy = Caller::default_client();
    let out = rig.run(&legacy, "get_status", json!({"terminal": "work"}));
    assert_eq!(out["isError"], false, "{out}");
    assert_eq!(data(&out)["name"], "work", "{out}");
    // A pane with routines looks the same to it as one without.
    let before = out.clone();
    rig.run(
        &agent_a(),
        "routine_create",
        every("check the todo list", 600),
    );
    let after = rig.run(&legacy, "get_status", json!({"terminal": "work"}));
    assert_eq!(after, before);
    let all = tools::definitions();
    let old: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .filter(|n| !n.starts_with("routine_"))
        .collect();
    assert_eq!(old.len(), 10);
}
