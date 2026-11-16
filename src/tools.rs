//! The ten MCP tools. They talk only to [`TerminalBackend`]; the safety rules
//! (input validation, the pane lock, refusing to type into a blocked pane, and
//! answer_dialog's checks) are enforced here, whatever the backend.
//!
//! Tool -> backend calls. Every tool that takes `terminal` resolves it first:
//! a `get` by name that must return a pane with exactly that name (see
//! `resolve`). Later calls use the resolved pane's id.
//!   start_pane      -> run
//!   send_input      -> get (x2, see lock_pane), send
//!   read_output     -> get, read
//!   get_status      -> get
//!   wait_status     -> get, wait, then get when it ends blocked without a kind
//!   list_panes      -> list
//!   interrupt_pane  -> get (x2, see lock_pane), interrupt
//!   answer_dialog   -> get, read, keys
//!   remove_pane     -> get (x2, see lock_pane), remove
//!   draft_clear     -> get, then SharedPanes::draft_clear (no pane lock: it types nothing)

use crate::agent::{unmeasured, Agent, ParsedDialog};
use crate::ansi::strip_terminal_escapes;
use crate::audit::{self, Audit};
use crate::backend::{
    dialog_kind, status_json, BackendError, ErrorKind, InputGuard, Key, KeysResult, RunSpec, State,
    StopHow, StyledScreen, TerminalBackend, TerminalInfo, TerminalRef, WaitReason, WaitResult,
};
use crate::devin_dialog;
use crate::dialog::Choice;
use crate::input::{validate_input, validate_name};
use crate::opencode_dialog;
use crate::panelock::{PaneGuard, PaneLocks};
use crate::profiles::{CwdError, DynamicRoutines, LaunchPolicy, RoutineConfig};
use crate::registry::{PaneRegistry, RegisteredPane, RoutineSet, RoutineState};
use crate::routine::{self, PaneWatch, Todo, WatchEvent};
use crate::routine_model::{Invalid, Routine, Trigger};
use crate::schedule::{
    self, CreateError, EveryUpdate, NewEvery, Note, PaneSchedule, UpdateError, Verdict,
};
use crate::shared_panes::SharedPanes;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

pub const TOOL_NAMES: [&str; 14] = [
    "start_pane",
    "send_input",
    "read_output",
    "get_status",
    "wait_status",
    "list_panes",
    "interrupt_pane",
    "answer_dialog",
    "remove_pane",
    "draft_clear",
    "routine_create",
    "routine_list",
    "routine_update",
    "routine_delete",
];
pub const READ_MAX_LINES: u64 = 10_000;
pub const WAIT_MIN_SECONDS: f64 = 1.0;
pub const WAIT_MAX_SECONDS: f64 = 1800.0;
pub const WAIT_DEFAULT_SECONDS: f64 = 30.0;
pub const INTERRUPT_MAX_SECONDS: f64 = 60.0;
/// remove_pane's grace period: how long a running program gets to end after
/// the hangup and terminate signals before it is killed.
pub const REMOVE_MIN_SECONDS: f64 = 0.0;
pub const REMOVE_MAX_SECONDS: f64 = 60.0;
pub const REMOVE_DEFAULT_SECONDS: f64 = 2.0;
/// answer_dialog re-reads the screen up to this many times, waiting for the
/// cursor to land on the target after the moves (the backend may deliver keys
/// after a quiet period, and the program redraws after that).
pub const CONFIRM_READS: usize = 5;
pub const CONFIRM_INTERVAL: Duration = Duration::from_millis(200);
/// kiro-cli 3.1.0 (V3) reports OSC 7501 blocked/permission before it has drawn
/// the dialog. answer_dialog waits up to this long for the dialog to parse,
/// re-reading the screen every [`RENDER_POLL_INTERVAL`].
pub const RENDER_WAIT: Duration = Duration::from_secs(3);
pub const RENDER_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How long wait_status pauses before it waits again on an OpenCode pane that
/// reported `clear`.
const SETTLE_INTERVAL: Duration = Duration::from_millis(100);
/// Screen reads in the render wait: one at once, then one per interval.
pub const RENDER_READS: usize =
    (RENDER_WAIT.as_millis() / RENDER_POLL_INTERVAL.as_millis()) as usize + 1;
/// Lines of screen text returned with an answer_dialog refusal.
const SEEN_TAIL_LINES: usize = 40;
/// What a caller does after typing was refused because the pane is blocked.
const BLOCKED_NEXT: &str = "read_output, then answer_dialog; nothing was typed";
/// The coding CLI is not logged in. answer_dialog cannot fix that.
const AUTH_NEXT: &str = "tell the operator; nothing was typed";
const AUTH_BLOCKED: &str = "the CLI is not logged in; the operator must log in to it or provide its API key, then start a new pane. Shepherd does not log in or handle credentials";

pub type Args = Map<String, Value>;

/// The tools' shared state: one backend, the per-pane locks, and the agent of
/// each pane a profile started. Clones share all three.
#[derive(Clone)]
pub struct Shepherd {
    backend: Arc<dyn TerminalBackend>,
    locks: Arc<PaneLocks>,
    policy: Arc<LaunchPolicy>,
    panes: Arc<Mutex<HashMap<String, PaneAgent>>>,
    routine: Arc<Mutex<HashMap<String, PaneWatch>>>,
    /// The `every` routines an agent created (issue #98), by pane name.
    schedules: Arc<Mutex<HashMap<String, PaneSchedule>>>,
    /// Unix time for the `every` routines: due times and expiry.
    wall: audit::Clock,
    /// How long the routine waits for a pane's lock before it skips the
    /// pane for this tick (see [`routine::LOCK_WAIT`]); tests shorten it.
    routine_lock_wait: Duration,
    /// Where calls are recorded, when the operator set an audit file. `None`
    /// records nothing.
    audit: Option<Arc<Audit>>,
    /// Where the agent kind, routine counters and answered flag are kept so
    /// a front that starts after its panes gets them back. `None` keeps them
    /// in memory only, as before.
    registry: Option<Arc<dyn PaneRegistry>>,
    /// Who is at a pane and whether they left a draft (issue #77). `None`
    /// for a backend with no person side: nothing is refused, `draft_clear`
    /// is unsupported and the status carries no `human` object.
    shared: Option<Arc<dyn SharedPanes>>,
}

/// What this process knows about a pane a profile started, keyed by pane name.
#[derive(Debug, Clone)]
struct PaneAgent {
    agent: Agent,
    /// The backend id of the pane this record belongs to, when the start
    /// that created it resolved one. Cleanup that runs after an await
    /// (`remove_pane`) must only forget the record for its own pane id —
    /// a new start may have reused the name in between (see
    /// [`Shepherd::forget_agent_if`]). `None` only for records made by
    /// test helpers that never resolved a pane.
    pane_id: Option<String>,
    /// A dialog was answered with Enter or Esc, so an idle state that follows
    /// is a cancel (see [`wait_outcome`]). Cleared once wait_status reaches a
    /// final outcome, so a timeout or other intermediate result keeps it.
    answered: bool,
    /// The launch profile that started the pane, when known. Routines an
    /// agent creates are allowed by this profile's `dynamic_routines`.
    profile: Option<String>,
}

impl Shepherd {
    /// A Shepherd with no launch profiles and no arbitrary commands:
    /// `start_pane` starts nothing.
    pub fn new(backend: impl TerminalBackend + 'static) -> Self {
        Self::with_policy(backend, LaunchPolicy::default())
    }

    /// A Shepherd that starts what `policy` allows.
    pub fn with_policy(backend: impl TerminalBackend + 'static, policy: LaunchPolicy) -> Self {
        Self {
            backend: Arc::new(backend),
            locks: Arc::new(PaneLocks::default()),
            policy: Arc::new(policy),
            panes: Arc::new(Mutex::new(HashMap::new())),
            routine: Arc::new(Mutex::new(HashMap::new())),
            schedules: Arc::new(Mutex::new(HashMap::new())),
            wall: audit::system_clock(),
            routine_lock_wait: routine::LOCK_WAIT,
            audit: None,
            registry: None,
            shared: None,
        }
    }

    /// Ask `shared` who is at a pane: for the `human` object of the status,
    /// for `draft_clear`, and for the routine to skip a pane a person is at
    /// before it records a call. The refusal of a bot's write itself stays in
    /// the backend.
    pub fn with_shared_panes(mut self, shared: Arc<dyn SharedPanes>) -> Self {
        self.shared = Some(shared);
        self
    }

    /// Keep each pane's agent kind, profile, routine counters and answered
    /// flag in `registry`, and write them there as they change. After the
    /// backend has adopted its panes, [`Shepherd::adopt_registry`] reads them
    /// back.
    pub fn with_registry(mut self, registry: Arc<dyn PaneRegistry>) -> Self {
        self.registry = Some(registry);
        self
    }

    /// Read the wall clock of the `every` routines from `clock` (tests).
    pub fn with_wall_clock(mut self, clock: audit::Clock) -> Self {
        self.wall = clock;
        self
    }

    fn wall_secs(&self) -> u64 {
        (self.wall)().as_secs()
    }

    /// Run a registry write. A failure is logged and the call goes on: the
    /// pane works either way, only a later restart would miss the record.
    /// True when the write landed or there is no registry to write to.
    fn registry_write(
        &self,
        pane_id: &str,
        what: &str,
        write: impl FnOnce(&dyn PaneRegistry) -> Result<(), BackendError>,
    ) -> bool {
        let Some(registry) = &self.registry else {
            return true;
        };
        match write(&**registry) {
            Ok(()) => true,
            Err(err) => {
                eprintln!(
                    "openab-shepherd: pane {pane_id}: cannot keep the {what} for a restart: {}",
                    err.detail
                );
                false
            }
        }
    }

    fn persist_routine(&self, pane_id: &str, state: &RoutineState) -> bool {
        self.registry_write(pane_id, "routine state", |r| {
            r.set_routine(pane_id, Some(state))
        })
    }

    fn persist_routines(&self, pane_id: &str, set: &RoutineSet) -> bool {
        self.registry_write(pane_id, "routines", |r| r.set_routines(pane_id, set))
    }

    /// Rebuild what this process knew about its panes from the registry:
    /// each pane's agent (so `answer_dialog` has its dialect), the pending
    /// answer, and the routine's watch with its counters. Call it once the
    /// backend has adopted its panes and before [`Shepherd::spawn_routine`].
    ///
    /// The routine's configuration is read from the current profile, so a
    /// profile edited before the restart takes effect. A pane whose profile
    /// is gone, or no longer has a routine, is not watched (one log line).
    /// A record that names no usable agent resurrects nothing.
    pub fn adopt_registry(&self) {
        let Some(registry) = &self.registry else {
            return;
        };
        let mut found = registry.registered();
        // A live pane wins a name a finished one also carries.
        found.sort_by_key(|pane| pane.live);
        for pane in found {
            // A registry that does not know names (one with no backend of
            // its own) is asked of the backend, by pane id.
            let looked_up;
            let name = match pane.name.as_deref() {
                Some(name) => name,
                None => match self.backend.get(&TerminalRef::new(pane.pane_id.as_str())) {
                    Ok(info) if info.id == pane.pane_id && info.name.is_some() => {
                        looked_up = info.name.unwrap_or_default();
                        looked_up.as_str()
                    }
                    _ => continue,
                },
            };
            let agent = match pane.agent.as_deref().map(|text| (text, Agent::parse(text))) {
                Some((_, Some(agent))) => agent,
                Some((text, None)) => {
                    eprintln!(
                        "openab-shepherd: pane {name}: the record names the agent {text:?}, \
                         which this build does not know; not restoring it"
                    );
                    continue;
                }
                None => continue,
            };
            self.panes().insert(
                name.to_string(),
                PaneAgent {
                    agent,
                    pane_id: Some(pane.pane_id.clone()),
                    answered: pane.answered,
                    profile: pane.profile.clone(),
                },
            );
            self.adopt_schedule(name, &pane, agent);
            let Some(state) = pane.routine.as_ref().filter(|_| pane.live) else {
                self.forget_routine(name);
                continue;
            };
            let config = pane
                .profile
                .as_deref()
                .and_then(|p| self.policy.profile(p))
                .and_then(|p| p.routine.clone());
            let Some(config) = config else {
                eprintln!(
                    "openab-shepherd: routine: pane {name}: its profile is gone or has no \
                     routine now, so the routine does not watch this pane"
                );
                self.forget_routine(name);
                continue;
            };
            if pane.cwd.is_empty() {
                eprintln!(
                    "openab-shepherd: routine: pane {name}: the record has no working \
                     directory, so the routine does not watch this pane"
                );
                self.forget_routine(name);
                continue;
            }
            self.routine_map().insert(
                name.to_string(),
                PaneWatch::restored(
                    config,
                    PathBuf::from(&pane.cwd),
                    agent,
                    pane.pane_id.clone(),
                    state,
                ),
            );
        }
    }

    /// Rebuild the `every` routines of a live pane from its record. The
    /// ceiling is the current profile's: a routine it no longer allows is
    /// stopped with the reason `profile_changed` (kept, so the agent sees
    /// it), never typed again. A profile that is gone restores nothing.
    fn adopt_schedule(&self, name: &str, pane: &RegisteredPane, agent: Agent) {
        self.forget_schedule(name);
        if !pane.live {
            return;
        }
        let Some(profile) = pane.profile.as_deref().and_then(|p| self.policy.profile(p)) else {
            if !pane.routines.is_empty() {
                eprintln!(
                    "openab-shepherd: routine: pane {name}: its profile is gone, so its \
                     agent-created routines are not restored"
                );
            }
            return;
        };
        let ceiling = profile.dynamic_routines.as_ref();
        if ceiling.is_none() && pane.routines.is_empty() {
            return;
        }
        let cwd = (!pane.cwd.is_empty()).then(|| Path::new(&pane.cwd));
        let (mut schedule, notes) =
            PaneSchedule::restored(pane.pane_id.clone(), agent, cwd, &pane.routines);
        for note in &notes {
            eprintln!("openab-shepherd: routine: pane {name}: {note}");
        }
        let stopped = schedule.apply_ceiling(ceiling);
        for (id, why) in &stopped {
            eprintln!(
                "openab-shepherd: routine: pane {name}: routine {id} stopped (profile_changed): \
                 {why}; it is kept but never typed again"
            );
        }
        if !stopped.is_empty() || !notes.is_empty() {
            self.persist_routines(&pane.pane_id, &schedule.to_set());
        }
        self.register_schedule(name, schedule);
    }

    /// Record calls to `audit`. Set before the routine starts, so its
    /// nudges are recorded too.
    pub fn with_audit(mut self, audit: Arc<Audit>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// The audit recorder, when one is configured.
    pub fn audit(&self) -> Option<&Arc<Audit>> {
        self.audit.as_ref()
    }

    #[cfg(test)]
    pub fn pane_locks(&self) -> &Arc<PaneLocks> {
        &self.locks
    }

    /// Record that the pane `name` runs `agent`, as if a profile had started it.
    #[cfg(test)]
    pub fn assume_agent(&self, name: &str, agent: Agent) {
        self.remember_agent(name, agent, None, None);
    }

    fn panes(&self) -> MutexGuard<'_, HashMap<String, PaneAgent>> {
        self.panes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn remember_agent(
        &self,
        name: &str,
        agent: Agent,
        pane_id: Option<String>,
        profile: Option<String>,
    ) {
        self.panes().insert(
            name.to_string(),
            PaneAgent {
                agent,
                pane_id,
                answered: false,
                profile,
            },
        );
    }

    /// The launch profile that started the pane `name`. None for a pane
    /// this process did not start from a profile.
    pub fn profile_of(&self, name: &str) -> Option<String> {
        self.panes().get(name).and_then(|pane| pane.profile.clone())
    }

    /// The agent of a pane this process started from a profile. None for a
    /// pane started any other way, or before this process started.
    pub(crate) fn agent_of(&self, name: &str) -> Option<Agent> {
        self.panes().get(name).map(|pane| pane.agent)
    }

    fn mark_answered(&self, name: &str) {
        self.set_answered(name, true);
    }

    /// Set the pending-answer flag and, when it changed, keep it for a restart.
    fn set_answered(&self, name: &str, answered: bool) {
        let changed = {
            let mut panes = self.panes();
            panes.get_mut(name).and_then(|pane| {
                let before = std::mem::replace(&mut pane.answered, answered);
                (before != answered).then(|| pane.pane_id.clone())
            })
        };
        if let Some(Some(pane_id)) = changed {
            self.registry_write(&pane_id, "answered flag", |r| {
                r.set_answered(&pane_id, answered)
            });
        }
    }

    /// Whether a dialog answered on this pane is still waiting for its end state.
    fn answer_pending(&self, name: &str) -> bool {
        self.panes().get(name).is_some_and(|pane| pane.answered)
    }

    /// Forget the pending answer. Called once the pane reached a final outcome.
    fn clear_answer(&self, name: &str) {
        self.set_answered(name, false);
    }

    fn forget_agent(&self, name: &str) {
        self.panes().remove(name);
    }

    /// Forget the agent record for `name` only when it belongs to pane
    /// `pane_id` (or carries no id, as test-helper records do): a new
    /// start may have reused the name while the caller was still working
    /// on the old pane, and dropping the new pane's record would silently
    /// disable the checks keyed on it — notably the Devin mode-switch
    /// refusal in `send_text_guarded`. Mirrors `forget_routine_if`.
    fn forget_agent_if(&self, name: &str, pane_id: &str) {
        let mut panes = self.panes();
        let belongs = panes
            .get(name)
            .is_some_and(|pane| pane.pane_id.as_deref().is_none_or(|id| id == pane_id));
        if belongs {
            panes.remove(name);
        }
    }

    fn routine_map(&self) -> MutexGuard<'_, HashMap<String, PaneWatch>> {
        self.routine.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn schedules(&self) -> MutexGuard<'_, HashMap<String, PaneSchedule>> {
        self.schedules
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn register_schedule(&self, name: &str, schedule: PaneSchedule) {
        self.schedules().insert(name.to_string(), schedule);
    }

    fn forget_schedule(&self, name: &str) {
        self.schedules().remove(name);
    }

    /// Forget the schedule for `name` only when it is still pane
    /// `pane_id`'s (see [`Shepherd::forget_routine_if`]).
    fn forget_schedule_if(&self, name: &str, pane_id: &str) {
        let mut map = self.schedules();
        if map.get(name).is_some_and(|s| s.pane_id() == pane_id) {
            map.remove(name);
        }
    }

    /// Run `change` on the schedule of `name` when it is still pane
    /// `pane_id`'s. The registry write belongs inside `change`, under the
    /// lock, so a later write never carries an older snapshot.
    fn with_schedule<R>(
        &self,
        name: &str,
        pane_id: &str,
        change: impl FnOnce(&mut PaneSchedule) -> R,
    ) -> Option<R> {
        let mut map = self.schedules();
        map.get_mut(name)
            .filter(|s| s.pane_id() == pane_id)
            .map(change)
    }

    fn keep_schedule(&self, schedule: &PaneSchedule) -> bool {
        self.persist_routines(schedule.pane_id(), &schedule.to_set())
    }

    /// The `every` routines on pane `name`, newest state. Empty for a pane
    /// that has none or does not allow them.
    pub fn routines_of(&self, name: &str) -> Vec<Routine> {
        self.schedules()
            .get(name)
            .map(|s| s.routines().to_vec())
            .unwrap_or_default()
    }

    /// The `dynamic_routines` ceiling of pane `name`'s profile, when it has
    /// one.
    pub fn ceiling_of(&self, name: &str) -> Option<DynamicRoutines> {
        self.profile_of(name).and_then(|p| {
            self.policy
                .profile(&p)
                .and_then(|p| p.dynamic_routines.clone())
        })
    }

    /// Create an `every` routine on pane `name` under its profile's
    /// `dynamic_routines` ceiling. The routine is kept in the registry first;
    /// when that write fails nothing changes (an acknowledged routine that
    /// would vanish at the next restart is the worst failure here).
    pub fn create_every(&self, name: &str, request: NewEvery) -> Result<Routine, CreateError> {
        self.create_every_on(name, None, request)
    }

    /// [`Shepherd::create_every`], refusing a schedule that belongs to a
    /// pane other than `pane_id` (a name reused since the caller looked).
    pub fn create_every_on(
        &self,
        name: &str,
        pane_id: Option<&str>,
        request: NewEvery,
    ) -> Result<Routine, CreateError> {
        let ceiling = self.ceiling_of(name).ok_or(CreateError::NotAllowed)?;
        let wall = self.wall_secs();
        let mut map = self.schedules();
        let schedule = map
            .get_mut(name)
            .filter(|s| pane_id.is_none_or(|id| s.pane_id() == id))
            .ok_or(CreateError::NotAllowed)?;
        let (routines, seq, routine) = schedule.plan_create(&request, &ceiling, wall)?;
        if let Some(registry) = &self.registry {
            let planned = RoutineSet {
                seq,
                items: routines.iter().filter_map(Routine::to_record).collect(),
            };
            registry
                .set_routines(schedule.pane_id(), &planned)
                .map_err(|err| CreateError::NotSaved(err.detail))?;
        }
        schedule.commit(routines, seq);
        eprintln!(
            "openab-shepherd: routine: pane {name}: routine {} created by {}",
            routine.id, routine.created_by
        );
        Ok(routine)
    }

    /// Change routine `id` on pane `name` (see [`PaneSchedule::plan_update`]).
    /// Kept in the registry before memory changes, like a create.
    pub fn update_every(
        &self,
        name: &str,
        pane_id: &str,
        id: &str,
        request: &EveryUpdate,
    ) -> Result<Routine, UpdateError> {
        let ceiling = self.ceiling_of(name);
        let wall = self.wall_secs();
        let mut map = self.schedules();
        let schedule = map
            .get_mut(name)
            .filter(|s| s.pane_id() == pane_id)
            .ok_or(UpdateError::NotFound)?;
        let Some(ceiling) = ceiling else {
            // The profile no longer allows routines: the ones left can be
            // listed and deleted, not changed.
            return Err(if schedule.routines().iter().any(|r| r.id == id) {
                UpdateError::Invalid(Invalid {
                    field: "routine",
                    limit: "a profile with dynamic_routines".to_string(),
                    message: "the pane's profile no longer allows routines; delete this one"
                        .to_string(),
                })
            } else {
                UpdateError::NotFound
            });
        };
        let (routines, routine) = schedule.plan_update(id, request, &ceiling, wall)?;
        self.save_planned(schedule, &routines)?;
        schedule.commit_routines(routines);
        eprintln!("openab-shepherd: routine: pane {name}: routine {id} updated");
        Ok(routine)
    }

    /// Remove routine `id` from pane `name`. Works whatever its status, and
    /// whether or not the profile still allows routines.
    pub fn delete_every(&self, name: &str, pane_id: &str, id: &str) -> Result<(), UpdateError> {
        let mut map = self.schedules();
        let schedule = map
            .get_mut(name)
            .filter(|s| s.pane_id() == pane_id)
            .ok_or(UpdateError::NotFound)?;
        let routines = schedule.plan_delete(id)?;
        self.save_planned(schedule, &routines)?;
        schedule.commit_routines(routines);
        eprintln!("openab-shepherd: routine: pane {name}: routine {id} deleted");
        Ok(())
    }

    /// Write the planned set to the registry; memory is untouched either way.
    fn save_planned(
        &self,
        schedule: &PaneSchedule,
        routines: &[Routine],
    ) -> Result<(), UpdateError> {
        if let Some(registry) = &self.registry {
            registry
                .set_routines(schedule.pane_id(), &schedule.set_with(routines))
                .map_err(|err| UpdateError::NotSaved(err.detail))?;
        }
        Ok(())
    }

    /// The names of the panes that have any routine to show: an `every`
    /// routine of any status, or the profile's idle routine.
    fn panes_with_routines(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .schedules()
            .iter()
            .filter(|(_, s)| !s.routines().is_empty())
            .map(|(name, _)| name.clone())
            .collect();
        names.extend(self.routine_map().keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    /// The profile's idle routine on pane `name`, as `routine_list` shows it.
    fn profile_routine_view(&self, name: &str, now: Instant) -> Option<Value> {
        self.routine_map().get(name).map(|watch| watch.view(now))
    }

    /// Whether pane `name` has the profile's idle routine.
    fn has_profile_routine(&self, name: &str) -> bool {
        self.routine_map().contains_key(name)
    }

    fn register_routine(
        &self,
        name: &str,
        config: RoutineConfig,
        cwd: PathBuf,
        agent: Agent,
        pane_id: String,
    ) {
        self.routine_map().insert(
            name.to_string(),
            PaneWatch::new(config, cwd, agent, pane_id),
        );
    }

    fn forget_routine(&self, name: &str) {
        self.routine_map().remove(name);
    }

    /// Forget the watch for `name` only when it is still the watch that was
    /// registered for pane `pane_id`: a new `start_pane` may have reused
    /// the name and registered its own watch in between.
    fn forget_routine_if(&self, name: &str, pane_id: &str) {
        let mut map = self.routine_map();
        if map
            .get(name)
            .is_some_and(|watch| watch.pane_id() == pane_id)
        {
            map.remove(name);
        }
    }

    /// Start the routine thread (see `routine`), when any launch profile
    /// sets a routine or allows agent-created ones. With none there is no
    /// thread and nothing to watch.
    /// The binary calls this once at startup; tests drive
    /// [`Shepherd::routine_tick_at`] with a fixed clock instead.
    pub fn spawn_routine(&self) {
        if !self.policy.has_routine() && !self.policy.has_dynamic_routines() {
            return;
        }
        let sh = self.clone();
        let spawned = thread::Builder::new()
            .name("routine".to_string())
            .spawn(move || loop {
                thread::sleep(routine::CHECK_INTERVAL);
                sh.routine_tick_with(&Instant::now);
            });
        if let Err(err) = spawned {
            eprintln!("openab-shepherd: routine: cannot start the routine thread: {err}");
        }
    }

    /// One routine pass at a fixed time (tests). Production passes a
    /// live clock through [`Shepherd::routine_tick_with`] instead.
    #[cfg(test)]
    pub(crate) fn routine_tick_at(&self, now: Instant) {
        self.routine_tick_with(&|| now);
    }

    /// [`Shepherd::routine_tick_at`] with the `every` routines' clock fixed
    /// at `wall` (unix seconds) too.
    #[cfg(test)]
    pub(crate) fn routine_tick_at_wall(&self, now: Instant, wall: u64) {
        self.routine_tick_full(&|| now, &|| wall);
    }

    /// One routine pass: read every watched pane once and act on what its
    /// observation decided. A pane that no longer resolves is forgotten.
    ///
    /// The clock is consulted fresh for every pane, and again after a
    /// nudge send returns: checks run serially and a send can take
    /// seconds (pane lock, settle, turn-start wait), so a single
    /// tick-start instant would backdate later panes' idle clocks and
    /// record nudges earlier than they happened, cutting the cooldown
    /// short.
    pub(crate) fn routine_tick_with(&self, clock: &dyn Fn() -> Instant) {
        self.routine_tick_full(clock, &|| self.wall_secs());
    }

    /// One pass of both engines: the idle routine on `clock` (monotonic),
    /// then the `every` routines on `wall` (unix seconds).
    pub(crate) fn routine_tick_full(&self, clock: &dyn Fn() -> Instant, wall: &dyn Fn() -> u64) {
        self.idle_tick(clock);
        let names: Vec<String> = self.schedules().keys().cloned().collect();
        for name in names {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.schedule_check(&name, clock, wall);
            }));
            if result.is_err() {
                eprintln!(
                    "openab-shepherd: routine: pane {name}: the schedule check panicked; \
                     skipping this pane for this tick"
                );
            }
        }
    }

    fn idle_tick(&self, clock: &dyn Fn() -> Instant) {
        let names: Vec<String> = self.routine_map().keys().cloned().collect();
        for name in names {
            // One pane's panic must not end the routine thread (or the
            // checks of the panes after it) silently.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.routine_check(&name, clock);
            }));
            if result.is_err() {
                eprintln!(
                    "openab-shepherd: routine: pane {name}: the routine check panicked; \
                     skipping this pane for this tick"
                );
            }
        }
    }

    /// Apply `change` to the watch of `name` when it is still pane
    /// `pane_id`'s, then keep its counters for a restart.
    fn settle_routine(&self, name: &str, pane_id: &str, change: impl FnOnce(&mut PaneWatch)) {
        let state = {
            let mut map = self.routine_map();
            match map.get_mut(name).filter(|w| w.pane_id() == pane_id) {
                Some(watch) => {
                    change(watch);
                    watch.state()
                }
                None => return,
            }
        };
        self.persist_routine(pane_id, &state);
    }

    /// A nudge was not sent because a person is at the pane. Counts toward
    /// neither the cooldown nor the cap; logs once per episode.
    fn note_person_at_pane(&self, name: &str, pane_id: &str) {
        let mut first = false;
        self.settle_routine(name, pane_id, |watch| first = watch.note_held());
        if first {
            eprintln!(
                "openab-shepherd: routine: pane {name}: not nudging while a person is at the pane"
            );
        }
    }

    fn routine_check(&self, name: &str, clock: &dyn Fn() -> Instant) {
        let now = clock();
        let (config, work_dir, todo_rel, pane_id, finished_state) = {
            let map = self.routine_map();
            let Some(watch) = map.get(name) else {
                return;
            };
            let config = watch.config().clone();
            (
                config.clone(),
                watch.work_dir(),
                config.todo_file.clone(),
                watch.pane_id().to_string(),
                watch.finished_state(),
            )
        };
        let info = match resolve(self, name) {
            Ok(info) => info,
            Err(err) => {
                if err.data["error"] == "not_found" {
                    self.forget_routine_if(name, &pane_id);
                }
                return;
            }
        };
        // The name may have been reused by a new start while this check
        // was resolving: that pane is not this watch's pane.
        if info.id != pane_id {
            return;
        }
        // A pane whose program has ended is never nudged and never will
        // be again: forget the watch (id-qualified) instead of resolving
        // it and re-reading its todo file on every tick until removal.
        if !info.live {
            self.forget_routine_if(name, &pane_id);
            return;
        }
        let todo = match &todo_rel {
            Some(rel) => routine::read_todo_in(&work_dir, rel),
            None => Todo::NotConfigured,
        };
        let (event, idle_secs, observed) = {
            let mut map = self.routine_map();
            let Some(watch) = map.get_mut(name) else {
                return;
            };
            // The name may have been reused since this check resolved the
            // pane: that watch belongs to a different pane, and this
            // check's observation must not change its state.
            if watch.pane_id() != pane_id {
                return;
            }
            if watch.note_todo(todo) {
                eprintln!(
                    "openab-shepherd: routine: pane {name}: the configured todo file \
                     became unreadable; not nudging while it is"
                );
            }
            let idle = watch.idle_for(now).map(|d| d.as_secs());
            let before = watch.state();
            let event = watch.observe(&info, todo, now);
            // The observation moved a counter (progress, or a give-up): keep it.
            let moved = (watch.state() != before).then(|| watch.state());
            (event, idle, moved)
        };
        if let Some(state) = observed {
            self.persist_routine(&pane_id, &state);
        }
        match event {
            WatchEvent::None => {}
            WatchEvent::GaveUp => {
                eprintln!(
                    "openab-shepherd: routine gave up: pane {name}: {} nudges without progress \
                     the routine did not cause; not nudging again until there is some",
                    config.max_consecutive_nudges
                );
            }
            WatchEvent::Nudge(reason) => {
                let turn = info
                    .turn
                    .map_or_else(|| "?".to_string(), |turn| turn.to_string());
                // A person at the pane (a lease or a draft): skip before the
                // pending count is kept and before the send writes an audit
                // `call` line, or every tick would record a call and an
                // error while they type. Only an early exit: the backend's
                // refusal inside the write is what keeps the bytes out, and
                // the arm below still handles a person who arrives between
                // this check and the send.
                if let Some(shared) = &self.shared {
                    if shared
                        .human_state(&info.reference())
                        .is_ok_and(|state| state.is_held())
                    {
                        self.note_person_at_pane(name, &pane_id);
                        return;
                    }
                }
                // Condition the send on the state this check observed: if
                // the pane changed (working, blocked, a new turn) between
                // the observation and the send, the send is refused as a
                // status mismatch instead of typed into a live turn.
                let guard = InputGuard {
                    refuse_when_blocked: true,
                    if_state: Some(finished_state),
                    expect_turn: info.turn,
                    ..InputGuard::default()
                };
                // The counter goes to the registry before the bytes go to the
                // pane: a crash in between over-counts one nudge, never loses
                // one. The outcome below writes the real counters again.
                let pending = {
                    let map = self.routine_map();
                    map.get(name)
                        .filter(|watch| watch.pane_id() == pane_id)
                        .map(PaneWatch::state_after_nudge)
                };
                // When the counter cannot be kept the nudge is not sent: the
                // next tick tries again, and nothing is typed uncounted.
                if let Some(pending) = &pending {
                    if !self.persist_routine(&pane_id, pending) {
                        return;
                    }
                }
                // The pane lock is waited for only briefly: a pane whose lock
                // is held for long (a slow send or wait) is skipped for this
                // tick instead of stalling every other watched pane.
                match send_text_within(
                    self,
                    name,
                    &config.nudge_text,
                    Some(guard),
                    Some(&pane_id),
                    Some(self.routine_lock_wait),
                    Some(RoutineAudit::PROFILE),
                ) {
                    Ok(_) => {
                        // The send just returned: stamp it with a fresh
                        // time, not the observation time, or a slow send
                        // would eat into (or erase) the cooldown.
                        let sent_at = clock();
                        self.settle_routine(name, &pane_id, |watch| watch.note_nudged(sent_at));
                        eprintln!(
                            "openab-shepherd: routine nudge: pane {name} (turn {turn}, \
                             idle {}s): {}",
                            idle_secs.unwrap_or(0),
                            reason.as_str()
                        );
                    }
                    Err(err) if err.data["error"] == "pane_busy" => {
                        // Nothing was sent and nothing is counted: the next
                        // tick looks again.
                        self.settle_routine(name, &pane_id, |_| ());
                    }
                    Err(err) if err.data["error"] == "pane_replayed" => {
                        // The pane was picked up after a restart and its
                        // screen is not trusted yet. Nothing was sent, so
                        // this is a skip, not a failed nudge: it counts
                        // toward neither the cooldown nor the cap.
                        let mut first = false;
                        self.settle_routine(name, &pane_id, |watch| first = watch.note_replayed());
                        if first {
                            eprintln!(
                                "openab-shepherd: routine: pane {name}: not nudging a replayed \
                                 pane until it has repainted"
                            );
                        }
                    }
                    Err(err)
                        if err.data["error"] == "pane_human_controlled"
                            || err.data["error"] == "pane_draft_pending" =>
                    {
                        // A person holds the pane's lease or has typed and
                        // not pressed Enter. Nothing was sent, and the nudge
                        // is not queued: a skip like a replayed pane, counted
                        // toward neither the cooldown nor the cap, so a
                        // person at the pane cannot make the routine give up.
                        self.note_person_at_pane(name, &pane_id);
                    }
                    Err(err) => {
                        // Any other refusal or failure (a `terminal_changed`
                        // or `status_mismatch` among them) counts toward the
                        // cooldown and the cap through `note_failed`.
                        let failed_at = clock();
                        // The native backend pastes, then presses Enter
                        // under a second guard; when that guard refuses,
                        // the nudge text is left sitting on the input
                        // line. That failure must not be retried — a
                        // retry would paste a second copy.
                        let pasted_unsubmitted = err.data["backend_detail"]
                            .as_str()
                            .is_some_and(crate::backend::is_pasted_unsubmitted);
                        self.settle_routine(name, &pane_id, |watch| {
                            if pasted_unsubmitted {
                                watch.note_pasted_unsubmitted(failed_at);
                            } else {
                                watch.note_failed(failed_at);
                            }
                        });
                        if pasted_unsubmitted {
                            eprintln!(
                                "openab-shepherd: routine: pane {name}: the nudge text was \
                                 pasted but Enter was not pressed; not retrying, and the \
                                 routine stops for this pane until a turn advance it did \
                                 not cause: {}",
                                err.text
                            );
                        } else {
                            eprintln!(
                                "openab-shepherd: routine: pane {name}: the nudge was not sent: {}",
                                err.text
                            );
                        }
                    }
                }
            }
        }
    }

    /// One pass over the `every` routines of pane `name` (issue #98). The
    /// cheap checks run first and read no pane: expiry, `max_runs`, a tick
    /// not yet due, a tick missed by more than an interval. Only a due
    /// routine reads the pane.
    fn schedule_check(&self, name: &str, clock: &dyn Fn() -> Instant, wall: &dyn Fn() -> u64) {
        let (pane_id, agent, due) = {
            let mut map = self.schedules();
            let Some(schedule) = map.get_mut(name) else {
                return;
            };
            let now = wall();
            let mut changed = false;
            let mut due = Vec::new();
            for routine in schedule.routines_mut() {
                match schedule::assess(routine, now) {
                    Verdict::NotYet => {}
                    Verdict::Changed(note) => {
                        changed = true;
                        log_schedule_note(name, &routine.id, note);
                    }
                    Verdict::Due => due.push(routine.id.clone()),
                }
            }
            if changed {
                self.keep_schedule(schedule);
            }
            (schedule.pane_id().to_string(), schedule.agent(), due)
        };
        if due.is_empty() {
            return;
        }
        let info = match resolve(self, name) {
            Ok(info) => info,
            Err(err) => {
                if err.data["error"] == "not_found" {
                    self.forget_schedule_if(name, &pane_id);
                }
                // Anything else is a read that failed: the routine stays due
                // and the next pass tries again.
                return;
            }
        };
        if info.id != pane_id {
            return;
        }
        if !info.live {
            self.forget_schedule_if(name, &pane_id);
            return;
        }
        let mut sent = false;
        for id in due {
            sent |= self.fire_every(name, &pane_id, agent, &id, &info, sent, clock, wall);
        }
    }

    /// Decide and act on one due routine. True when its text went in.
    ///
    /// A skip (working, blocked, a person, nothing left on the todo list, a
    /// replayed pane) consumes the tick: `next_due` moves to the next grid
    /// time and nothing is queued. The next grid time is kept before the
    /// text is typed, and a send is never tried when it cannot be kept: a
    /// crash then loses one tick and can never fire it twice. `pane_busy`
    /// is not a decision and consumes nothing: the next pass retries.
    #[allow(clippy::too_many_arguments)]
    fn fire_every(
        &self,
        name: &str,
        pane_id: &str,
        agent: Agent,
        id: &str,
        info: &TerminalInfo,
        sent_this_pass: bool,
        clock: &dyn Fn() -> Instant,
        wall: &dyn Fn() -> u64,
    ) -> bool {
        let snapshot = self.with_schedule(name, pane_id, |schedule| {
            schedule
                .routines()
                .iter()
                .find(|r| r.id == id && r.status.is_active())
                .map(|r| {
                    let crate::routine_model::Action::Type { text } = &r.action;
                    (
                        text.clone(),
                        r.todo_file.clone(),
                        r.created_by.clone(),
                        schedule.work_dir(),
                    )
                })
        });
        let Some(Some((text, todo_file, created_by, work_dir))) = snapshot else {
            return false;
        };
        let blocked = info
            .status
            .as_ref()
            .is_some_and(|status| status.state == State::Blocked);
        let held = || {
            self.shared.as_ref().is_some_and(|shared| {
                shared
                    .human_state(&info.reference())
                    .is_ok_and(|state| state.is_held())
            })
        };
        let skip = if blocked {
            Some("blocked")
        } else if sent_this_pass || !routine::turn_finished(agent, info) {
            Some("working")
        } else if held() {
            Some("person")
        } else if todo_file.as_deref().is_some_and(|rel| {
            let todo = work_dir.as_ref().map_or(Todo::Unreadable, |dir| {
                routine::read_todo_in(dir, Path::new(rel))
            });
            matches!(todo, Todo::Finished | Todo::Unreadable)
        }) {
            Some("todo_done")
        } else {
            None
        };
        if let Some(reason) = skip {
            self.consume_tick(name, pane_id, id, wall, reason);
            return false;
        }
        // Keep the next grid time first; a failed write means no send.
        let begun = self.with_schedule(name, pane_id, |schedule| {
            let Some(routine) = schedule.get_mut(id) else {
                return Err(());
            };
            let previous = schedule::advance(routine, wall());
            if self.keep_schedule(schedule) {
                Ok(previous)
            } else {
                if let Some(routine) = schedule.get_mut(id) {
                    schedule::restore_due(routine, previous);
                }
                Err(())
            }
        });
        let Some(Ok(previous)) = begun else {
            eprintln!(
                "openab-shepherd: routine: pane {name}: routine {id} not sent: its next \
                 time could not be kept"
            );
            return false;
        };
        let guard = InputGuard {
            refuse_when_blocked: true,
            if_state: Some(routine::finished_state_of(agent)),
            expect_turn: info.turn,
            ..InputGuard::default()
        };
        let result = send_text_within(
            self,
            name,
            &text,
            Some(guard),
            Some(pane_id),
            Some(self.routine_lock_wait),
            Some(RoutineAudit {
                routine: id,
                created_by: &created_by,
            }),
        );
        let code = |err: &ToolError| err.data["error"].as_str().unwrap_or("error").to_string();
        match result {
            Ok(_) => {
                let at = wall();
                let runs = self.with_schedule(name, pane_id, |schedule| {
                    let runs = schedule.get_mut(id).map(|routine| {
                        schedule::record_sent(routine, at);
                        routine.runs
                    });
                    self.keep_schedule(schedule);
                    runs
                });
                // The idle routine's watch must not take the turns this text
                // causes for the agent's own progress.
                let sent_at = clock();
                self.settle_routine(name, pane_id, |watch| watch.note_foreign_nudge(sent_at));
                eprintln!(
                    "openab-shepherd: routine nudge: pane {name} (routine {id}, run {})",
                    runs.flatten().unwrap_or(0)
                );
                true
            }
            Err(err) if code(&err) == "pane_busy" => {
                self.with_schedule(name, pane_id, |schedule| {
                    if let Some(routine) = schedule.get_mut(id) {
                        schedule::restore_due(routine, previous);
                    }
                });
                false
            }
            Err(err) if code(&err) == "pane_replayed" => {
                self.record_skip_after_advance(name, pane_id, id, "replayed");
                false
            }
            Err(err)
                if code(&err) == "pane_human_controlled" || code(&err) == "pane_draft_pending" =>
            {
                self.record_skip_after_advance(name, pane_id, id, "person");
                false
            }
            Err(err) => {
                let pasted = err.data["backend_detail"]
                    .as_str()
                    .is_some_and(crate::backend::is_pasted_unsubmitted);
                let failed = code(&err);
                let gave_up = self
                    .with_schedule(name, pane_id, |schedule| {
                        let gave_up = schedule.get_mut(id).is_some_and(|routine| {
                            if pasted {
                                schedule::record_give_up(routine, &failed, &err.text);
                                true
                            } else {
                                schedule::record_failure(routine, &failed, &err.text)
                            }
                        });
                        self.keep_schedule(schedule);
                        gave_up
                    })
                    .unwrap_or(false);
                if gave_up {
                    eprintln!(
                        "openab-shepherd: routine gave up: pane {name}: routine {id}: {}",
                        err.text
                    );
                } else {
                    eprintln!(
                        "openab-shepherd: routine: pane {name}: routine {id} was not sent: {}",
                        err.text
                    );
                }
                false
            }
        }
    }

    /// A skipped tick: move to the next grid time, note why, keep both.
    fn consume_tick(&self, name: &str, pane_id: &str, id: &str, wall: &dyn Fn() -> u64, why: &str) {
        self.with_schedule(name, pane_id, |schedule| {
            if let Some(routine) = schedule.get_mut(id) {
                schedule::advance(routine, wall());
                schedule::record_skip(routine, why);
            }
            self.keep_schedule(schedule);
        });
    }

    /// The tick was already consumed (and kept) before the send; note why
    /// the send did not go and keep that.
    fn record_skip_after_advance(&self, name: &str, pane_id: &str, id: &str, why: &str) {
        self.with_schedule(name, pane_id, |schedule| {
            if let Some(routine) = schedule.get_mut(id) {
                schedule::record_skip(routine, why);
            }
            self.keep_schedule(schedule);
        });
    }
}

fn log_schedule_note(name: &str, id: &str, note: Note) {
    let what = match note {
        Note::Expired => "expired",
        Note::Done => "is done (max_runs reached)",
        Note::Missed => "missed a tick; skipped it and kept the grid",
        Note::Clamped => "had a next time too far ahead (the clock went back); clamped it",
    };
    eprintln!("openab-shepherd: routine: pane {name}: routine {id} {what}");
}

/// A tool failure: the text shown to the MCP client, and structured data
/// (returned as `structuredContent` alongside `isError: true`). `data.error` is
/// Shepherd's error code and `data.message` repeats the text without it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    pub text: String,
    pub data: Value,
}

impl ToolError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            text: format!("{code}: {message}"),
            data: json!({"error": code, "message": message}),
        }
    }

    fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.data[key] = value.into();
        self
    }

    /// Put `prefix` in front of the message.
    fn context(self, prefix: &str) -> Self {
        let code = self.data["error"].as_str().unwrap_or_default().to_string();
        let message = format!(
            "{prefix}: {}",
            self.data["message"].as_str().unwrap_or_default()
        );
        Self {
            text: format!("{code}: {message}"),
            data: {
                let mut data = self.data;
                data["message"] = json!(message);
                data
            },
        }
    }
}

/// An argument that fails validation.
impl From<String> for ToolError {
    fn from(text: String) -> Self {
        Self::new("invalid_input", text)
    }
}

pub fn is_tool(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// The `tools/list` payload.
pub fn definitions() -> Value {
    json!([
        {
            "name": "start_pane",
            "description": "Start a program in a new pane. Give profile: the name of one of the operator's launch profiles (list_panes lists them with the directories each allows); the profile fixes the program, its arguments and its environment. cwd, when given, must be one of the profile's directories or below one (after resolving symlinks); without it the profile's first directory is used. argv (a program and its arguments) or line (a shell line run by sh -c) instead of profile work only when the operator allows arbitrary commands (list_panes: allow_arbitrary_command), and such a pane never gets a profile's environment. With argv or line, agent (kiro, devin or opencode) says which agent the pane runs, so answer_dialog and the agent's other guards apply to it; it is the caller's claim, recorded in the audit and not checked against the program. A pane without a profile has no routine, no dynamic routines and no permission_mode. Later calls address the pane by its name: give one, or Shepherd picks one (pane-<hex>) and returns it. Returns the pane (name, live, status, reported, turn, status_ms, exit_code, argv, cwd). Input is trimmed and limited to 4096 bytes per field. Control characters (except TAB) are refused, not removed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "profile": {"type": "string", "description": "Launch profile name, as list_panes shows it."},
                    "argv": {"type": "array", "items": {"type": "string"}, "description": "Program and arguments; non-empty. Only when arbitrary commands are allowed."},
                    "line": {"type": "string", "description": "Shell line run by sh -c. Only when arbitrary commands are allowed."},
                    "agent": {"type": "string", "enum": ["kiro", "devin", "opencode"], "description": "With argv or line only (refused with profile, which fixes its agent): the agent the program is. Optional; without it the pane has no agent and answer_dialog refuses it."},
                    "cwd": {"type": "string", "description": "Working directory (absolute)."},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Extra environment variables for an argv or line pane only (refused with profile): at most 32, names [A-Z_][A-Z0-9_]*, values under the same input rules (empty allowed; leading and trailing whitespace is trimmed). A backend may refuse protected names (the native backend refuses HOME, PATH, USER, LANG, TERM and DYLD_*/LD_*); a backend without per-run environments returns unsupported."},
                    "name": {"type": "string", "description": "Pane name: 1-32 of a-z, 0-9, '-', '_', '.'. Optional; Shepherd picks one when it is missing."}
                }
            }
        },
        {
            "name": "send_input",
            "description": "Type text into a pane and press Enter. Sendable when the pane is idle or done (kiro-cli stays in done after a turn). Input is trimmed and limited to 4096 bytes. Control characters (except TAB) are refused, not removed. While the program is blocked on a dialog, nothing is typed and the error code is pane_blocked, with kind saying which dialog (for example permission). Read the screen, then use answer_dialog. This tool never forces input. A pane reporting idle or done while its permission dialog is still on the screen is refused (refused).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name, as start_pane or list_panes returned it."},
                    "text": {"type": "string"}
                },
                "required": ["terminal", "text"]
            }
        },
        {
            "name": "read_output",
            "description": "Read a pane's screen (lines=0) or its last N lines of history. ANSI CSI and OSC escape sequences are removed from text. TUI redraw noise is not removed. Returns text, complete, source (screen, history, stitched, or other), and reason (why only the screen was read: blocked, other, or null).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "lines": {"type": "integer", "minimum": 0, "maximum": READ_MAX_LINES, "default": 0}
                },
                "required": ["terminal"]
            }
        },
        {
            "name": "get_status",
            "description": "Return a pane: its status ({state, kind} with state idle, working, done, blocked, error, or other; null when it never reported one), its reported flag, live, turn, status_ms, and exit_code. reported is false when the program never sent an OSC 7501 status.",
            "inputSchema": {
                "type": "object",
                "properties": {"terminal": {"type": "string"}},
                "required": ["terminal"]
            }
        },
        {
            "name": "wait_status",
            "description": "Wait until a pane stops working, or check in after timeout seconds. Branch on outcome: done (the turn finished; for an opencode pane, idle after a turn, since OpenCode reports no done), blocked (waiting on a dialog; kind says which, for example permission), timeout (a check-in: still running), exited (the program ended), cancelled (the pane went idle after answer_dialog denied or cancelled its dialog; a Devin cancel returns to idle, not done), or other (any other status or reason; read status). turn_complete is true only for done and exited; a blocked pane has not completed its turn even though its turn number advanced. A program that never reports status counts as working until it exits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "timeout": {"type": "number", "minimum": WAIT_MIN_SECONDS, "maximum": WAIT_MAX_SECONDS, "default": WAIT_DEFAULT_SECONDS}
                },
                "required": ["terminal"]
            }
        },
        {
            "name": "list_panes",
            "description": "List panes, each in the same shape start_pane returns. all=true includes finished panes; hidden_finished counts the ones left out. live_panes counts the running panes and live_pane_limit is how many may run at once (null when the backend sets no limit); start_pane is refused at the limit, so remove panes you no longer need with remove_pane. Also returns profiles (each launch profile's name and the directories it allows) and allow_arbitrary_command (whether start_pane takes argv or line).",
            "inputSchema": {
                "type": "object",
                "properties": {"all": {"type": "boolean", "default": false}}
            }
        },
        {
            "name": "interrupt_pane",
            "description": "Send the stop key to a pane and report which key the backend chose (key: esc, ctrl+c, or other) and the outcome: stopped, exited, still_working, unknown, or other. It does not escalate.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "timeout": {"type": "number", "minimum": WAIT_MIN_SECONDS, "maximum": INTERRUPT_MAX_SECONDS}
                },
                "required": ["terminal"]
            }
        },
        {
            "name": "answer_dialog",
            "description": "Answer a permission dialog in a pane that is blocked with kind permission: allow_once or deny_once. Rule-creating options are not offered. The tool reads the screen, waiting up to 3 s (re-reading every 200 ms) for the dialog to render, because kiro-cli reports blocked before it draws the dialog; between reads it checks the pane is still blocked with kind permission in the same turn, and if the dialog never renders it refuses without sending anything. It then moves the cursor with up/down keys, reads the screen again until the cursor is confirmed on the chosen option, checks again that the pane is still blocked with kind permission in the same turn, and only then presses Enter. On any mismatch it sends nothing more and returns what it saw. Other Shepherd calls that type into the same pane wait until it finishes. A person at the terminal or another client of the terminal backend can still act between the last check and Enter when the terminal backend has no conditional key press; the native backend checks the status and presses Enter in one step. The dialog is parsed from screen text. The dialect is the pane's agent, set by the launch profile that started it: kiro (measured on kiro-cli 3.3.0 V3 for shell, fs_write, mcp and web_fetch permission dialogs; shell also on 3.1.0) or devin (Devin CLI 3000.12.43 and 3000.12.57 shell command dialogs; Devin never selects Bypass mode, and a highlight that does not land on the choice sends Esc, which cancels, then refuses); opencode (OpenCode v2.2.7): deny_once sends Esc, which cancels like Reject, after the pane is blocked with kind permission and the screen shows the whole expected dialog; allow_once presses Enter only when the styled screen, read in the same step as the key, shows Allow once with a background that differs from both other options while those two share one background, compared as palette-or-truecolor values so it works with either (otherwise it is refused with no keys sent), and Always allow is never selected. A pane not started from a profile by this server is refused with no keys sent. Follow with wait_status.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string"},
                    "choice": {"type": "string", "enum": ["allow_once", "deny_once"]}
                },
                "required": ["terminal", "choice"]
            }
        },
        {
            "name": "remove_pane",
            "description": "Close a pane and forget it. It waits for any other Shepherd call that types into the same pane to finish first. If the program is running, Shepherd sends its process group and the processes it started the hangup and terminate signals, waits up to timeout seconds (default 2) for them to end, then kills whatever is left. No key is pressed first; use interrupt_pane before it if the program should stop its turn cleanly. Afterwards list_panes (even with all=true) no longer shows the pane, later calls that name it get not_found, and its name can be used again. Returns name, was_live (whether the program was running), stopped (exited: it ended within the timeout; killed: it had to be killed; other: the backend could not say; null when it had already finished) and exit_code (128 + the signal number when a signal ended it; null when unknown). A backend that times its own stop ignores timeout.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name, as start_pane or list_panes returned it."},
                    "timeout": {"type": "number", "minimum": REMOVE_MIN_SECONDS, "maximum": REMOVE_MAX_SECONDS, "default": REMOVE_DEFAULT_SECONDS}
                },
                "required": ["terminal"]
            }
        },
        {
            "name": "draft_clear",
            "description": "Clear the draft flag of a pane: the mark a person's typing leaves until they press Enter. While it is set, every bot write to the pane is refused (pane_draft_pending). Use it only when the person is gone and their half-typed line no longer matters; the line itself stays on the pane's input, so read_output first if unsure. It clears the flag whoever holds the pane's input lease and does not touch the lease: while a lease is held a bot is still refused (pane_human_controlled). Needs the lease:preempt scope, which a bot normally does not hold. Returns terminal and cleared (whether a draft was set). A backend with no person side returns unsupported.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name, as start_pane or list_panes returned it."}
                },
                "required": ["terminal"]
            }
        },
        {
            "name": "routine_create",
            "description": "Create a routine on a pane: Shepherd types text into it every every_secs seconds, like a reminder you set for yourself. Only for a pane started from a launch profile that allows it (dynamic_routines); otherwise it is refused with routines_not_allowed. The text is typed unchanged, as if send_input sent it, and only when the pane's turn is finished: a pane that is working, blocked, held by a person or still repainting after a restart is skipped, and a skipped tick is not queued or made up later, so a routine never interrupts a running turn. Ticks missed while Shepherd was down are not made up either. The routine runs on a fixed grid from its creation and ends at expiry (expires_secs, at most the profile's limit) or after max_runs sends; three failed sends in a row stop it. Values outside the profile's limits are refused with routine_invalid (field and limit say which), never adjusted; the result shows the limits (ceiling). Text must be an instruction, not a bare answer such as yes or 1, and at most 1024 bytes. todo_file (a relative path in the pane's working directory) makes a tick type only while that file has an unchecked item (- [ ]). A routine does not survive its pane. Call routine_list to see status, last_result, next_due_in_secs and why one stopped; nothing is pushed to you when a routine ends. Needs the routine scope. Your own pane name is in the SHEPHERD_PANE environment variable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name, as start_pane or list_panes returned it."},
                    "trigger": {
                        "type": "object",
                        "description": "When to type. Only an every trigger exists.",
                        "properties": {
                            "type": {"type": "string", "enum": ["every"]},
                            "every_secs": {"type": "integer", "minimum": 60, "maximum": 604800, "description": "Seconds between lines; the profile may set a higher minimum (default 600)."}
                        },
                        "required": ["type", "every_secs"]
                    },
                    "text": {"type": "string", "description": "The line to type; at most 1024 bytes; not a bare yes, no, ok or number."},
                    "todo_file": {"type": "string", "description": "Relative path of a todo file in the pane's working directory; the line is typed only while it has an unchecked item."},
                    "expires_secs": {"type": "integer", "minimum": 60, "maximum": 2592000, "description": "Seconds until the routine expires; default and maximum come from the profile (default 7 days)."},
                    "max_runs": {"type": "integer", "minimum": 1, "maximum": 10000, "description": "Stop after this many sends."}
                },
                "required": ["terminal", "trigger", "text"]
            }
        },
        {
            "name": "routine_list",
            "description": "List the routines of one pane, or of every pane that has any when terminal is left out. Includes the pane profile's own idle routine (id profile, source profile), which is read-only: its runs are the nudges since the agent last made progress, and last_result is sent or gave_up. Each routine shows id, source, trigger, action, status (active, expired, done, gave_up, stopped), stop_reason (profile_changed: the profile no longer allows it), runs, last_run_at, last_result (sent, skipped:working, skipped:blocked, skipped:replayed, skipped:person, skipped:todo_done, failed:<code>), next_due_at, next_due_in_secs and expires_in_secs. Needs the routine scope.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name. Omit to list every pane that has routines."}
                }
            }
        },
        {
            "name": "routine_update",
            "description": "Change a routine you or another client created, by id. Give at least one of every_secs, text, todo_file (null removes it), expires_secs and max_runs (null removes it). A new every_secs or expires_secs restarts the grid from now. expires_secs on an expired, done, gave_up or stopped routine makes it active again. The same limits as routine_create apply and an out-of-range value is refused, not adjusted. The profile's own routine (id profile) cannot be changed. Needs the routine scope.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name."},
                    "id": {"type": "string", "description": "Routine id, as routine_create or routine_list showed it."},
                    "every_secs": {"type": "integer", "minimum": 60, "maximum": 604800},
                    "text": {"type": "string", "description": "At most 1024 bytes; not a bare yes, no, ok or number."},
                    "todo_file": {"type": ["string", "null"]},
                    "expires_secs": {"type": "integer", "minimum": 60, "maximum": 2592000},
                    "max_runs": {"type": ["integer", "null"], "minimum": 1, "maximum": 10000}
                },
                "required": ["terminal", "id"]
            }
        },
        {
            "name": "routine_delete",
            "description": "Delete a routine by id. It works on a routine of any status, also when the profile no longer allows routines. The profile's own routine (id profile) cannot be deleted. Needs the routine scope.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "terminal": {"type": "string", "description": "Pane name."},
                    "id": {"type": "string", "description": "Routine id, as routine_create or routine_list showed it."}
                },
                "required": ["terminal", "id"]
            }
        }
    ])
}

/// Run one tool. `Err` carries what is shown to the MCP client with `isError`.
pub fn call(sh: &Shepherd, name: &str, args: &Args) -> Result<Value, ToolError> {
    call_as(sh, crate::clients::DEFAULT_ID, name, args)
}

/// Run one tool for `client`, the id of the matched caller. Most tools do
/// not care who asked; the routine tools record who created a routine.
pub fn call_as(sh: &Shepherd, client: &str, name: &str, args: &Args) -> Result<Value, ToolError> {
    match name {
        "start_pane" => start_pane(sh, args),
        "send_input" => send_input(sh, args),
        "read_output" => read_output(sh, args),
        "get_status" => get_status(sh, args),
        "wait_status" => wait_status(sh, args),
        "list_panes" => list_panes(sh, args),
        "interrupt_pane" => interrupt_pane(sh, args),
        "answer_dialog" => answer_dialog(sh, args),
        "remove_pane" => remove_pane(sh, args),
        "draft_clear" => draft_clear(sh, args),
        "routine_create" => routine_create(sh, client, args),
        "routine_list" => routine_list(sh, args),
        "routine_update" => routine_update(sh, args),
        "routine_delete" => routine_delete(sh, args),
        other => Err(format!("unknown tool: {other}").into()),
    }
}

fn start_pane(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let declared_agent = declared_agent(args)?;
    let mut spec = match optional_str(args, "profile")? {
        Some(_) if declared_agent.is_some() => {
            return Err(
                "agent is not accepted with a profile; a profile fixes its agent"
                    .to_string()
                    .into(),
            )
        }
        Some(profile) => profile_spec(sh, args, profile)?,
        None => command_spec(sh, args)?,
    };
    // Every pane Shepherd starts has a name: callers address panes by name.
    let chosen_name = optional_str(args, "name")?;
    spec.name = Some(match chosen_name {
        Some(name) => validate_name(name).map_err(|err| format!("name: {err}"))?,
        None => generated_name(),
    });
    // Credentials are read last, when everything else has passed, and only
    // into the spec of this profile's pane.
    if let Some(profile) = spec.profile.as_deref().and_then(|p| sh.policy.profile(p)) {
        spec.profile_env = sh.policy.resolve_env(profile).map_err(|message| {
            ToolError::new("profile_unavailable", message).with("profile", profile.name.as_str())
        })?;
    }
    let mut info = sh.backend.run(&spec).map_err(|err| match err.kind {
        ErrorKind::NotFound => ToolError::new(
            "not_found",
            "the program or working directory was not found",
        )
        .with("backend_detail", err.detail),
        ErrorKind::NameTaken if chosen_name.is_none() => ToolError::new(
            "name_taken",
            "the name Shepherd generated for the pane is already in use; try again",
        )
        .with("backend_detail", err.detail),
        _ => backend_failure(spec.name.as_deref(), err),
    })?;
    // The profile's agent, or the one an argv or line start declared, decides
    // the dialect answer_dialog reads for this pane.
    let profile_agent = spec
        .profile
        .as_deref()
        .and_then(|p| sh.policy.profile(p))
        .map(|p| p.agent)
        .or(declared_agent);
    if info.name.is_none() {
        info.name = spec.name.clone();
    }
    if profile_agent == Some(Agent::Devin) {
        let pane = info.reference();
        if trust_prompt_shown(sh, &pane) {
            // Nothing was typed into the pane, so it is closed, not left waiting.
            let _ = sh.backend.remove(&pane, Some(Duration::ZERO));
            return Err(ToolError::new(
                "trust_prompt",
                format!(
                    "Devin asked \"{TRUST_PROMPT}\" for this directory, so the pane was closed before it ran anything. Shepherd never answers this prompt. Trust the directory once in Devin (run devin there and choose 1), then start the pane again"
                ),
            )
            .with("terminal", spec.name.clone()));
        }
    }
    if let Some(name) = info.name.as_deref() {
        match profile_agent {
            Some(agent) => {
                sh.remember_agent(name, agent, Some(info.id.clone()), spec.profile.clone());
                sh.registry_write(&info.id, "agent", |r| {
                    r.set_agent(
                        &info.id,
                        Some(agent.as_str()),
                        spec.profile.as_deref(),
                        spec.cwd.as_deref(),
                    )
                });
            }
            // A pane this profile did not start must not inherit an agent that an
            // earlier pane with the same name had.
            None => sh.forget_agent(name),
        }
        // The routine watches only a pane a routine profile started here;
        // any other start under this name drops an earlier registration.
        let watched = spec
            .profile
            .as_deref()
            .and_then(|p| sh.policy.profile(p))
            .and_then(|p| p.routine.clone().map(|config| (config, p.agent)));
        match (watched, spec.cwd.as_deref()) {
            (Some((config, agent)), Some(cwd)) => {
                sh.register_routine(name, config, PathBuf::from(cwd), agent, info.id.clone());
                sh.persist_routine(&info.id, &RoutineState::default());
            }
            (Some(_), None) => {
                eprintln!(
                    "openab-shepherd: routine: pane {name}: the start named no working \
                     directory, so the routine does not watch this pane"
                );
                sh.forget_routine(name);
            }
            _ => sh.forget_routine(name),
        }
        // The `every` scheduler watches only a pane whose profile allows
        // agent-created routines; any other start under this name drops an
        // earlier schedule.
        sh.forget_schedule(name);
        let allows_dynamic = spec
            .profile
            .as_deref()
            .and_then(|p| sh.policy.profile(p))
            .is_some_and(|p| p.dynamic_routines.is_some());
        if let (true, Some(agent)) = (allows_dynamic, profile_agent) {
            sh.register_schedule(
                name,
                PaneSchedule::new(info.id.clone(), agent, spec.cwd.as_deref().map(Path::new)),
            );
        }
    }
    Ok(info.to_json())
}

/// `agent`: one of the three agent names, or absent (`null` counts as absent).
/// An unknown value is refused, naming the three.
fn declared_agent(args: &Args) -> Result<Option<Agent>, ToolError> {
    match args.get("agent") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(name)) => Agent::parse(name).map(Some).ok_or_else(|| {
            "agent must be one of kiro, devin or opencode"
                .to_string()
                .into()
        }),
        Some(_) => Err("agent must be one of kiro, devin or opencode"
            .to_string()
            .into()),
    }
}

/// The Devin prompt that asks whether to trust a directory before Devin runs
/// anything there.
const TRUST_PROMPT: &str = "Do you trust the authors of this directory?";

/// Read the pane for the trust prompt for the same window answer_dialog uses to
/// wait for a dialog to render. True as soon as the prompt shows. False when it
/// never shows in that window, or when the backend has no screen text.
fn trust_prompt_shown(sh: &Shepherd, pane: &TerminalRef) -> bool {
    if !sh.backend.capabilities().screen_text {
        return false;
    }
    for attempt in 0..RENDER_READS {
        if attempt > 0 {
            thread::sleep(RENDER_POLL_INTERVAL);
        }
        if let Ok(screen) = sh.backend.read(pane, 0) {
            if strip_terminal_escapes(&screen.text).contains(TRUST_PROMPT) {
                return true;
            }
        }
    }
    false
}

/// What Kiro prints at every start with `-a` / `--trust-all-tools` (measured on
/// kiro-cli 3.3.0). A short fragment so a wrapped line still matches.
const KIRO_TRUST_WARNING: &str = "trust all tools mode";

const TRUST_ACCEPTANCE_NEXT: &str =
    "a person attaches and accepts the warning, or start the pane without -a; nothing was typed";

/// True when the visible screen shows Kiro's trust-all-tools warning. One read,
/// no waiting: a blank screen, a backend without screen text and a failed read
/// are all "not shown", so the guard refuses only on a positive match.
fn kiro_trust_warning_shown(sh: &Shepherd, pane: &TerminalRef) -> bool {
    if !sh.backend.capabilities().screen_text {
        return false;
    }
    sh.backend
        .read(pane, 0)
        .is_ok_and(|screen| strip_terminal_escapes(&screen.text).contains(KIRO_TRUST_WARNING))
}

/// A launch profile's run: its fixed argv, a cwd it allows, no caller env.
fn profile_spec(sh: &Shepherd, args: &Args, profile: &str) -> Result<RunSpec, ToolError> {
    let given = |key: &str| !matches!(args.get(key), None | Some(Value::Null));
    if given("argv") || given("line") {
        return Err("give profile, or argv or line, not both".to_string().into());
    }
    if given("env") {
        return Err(
            "env is not accepted with a profile; the profile sets the environment"
                .to_string()
                .into(),
        );
    }
    let name = validate_name(profile).map_err(|err| format!("profile: {err}"))?;
    let profile = sh.policy.profile(&name).ok_or_else(|| {
        ToolError::new(
            "invalid_input",
            format!("no launch profile is named {name}; list_panes shows the profiles"),
        )
        .with("profile", name.as_str())
    })?;
    let requested = match optional_str(args, "cwd")? {
        Some(cwd) => Some(validate_input(cwd).map_err(|err| format!("cwd: {err}"))?),
        None => None,
    };
    let cwd = sh
        .policy
        .cwd(profile, requested.as_deref())
        .map_err(|err| match err {
            CwdError::NotFound => {
                ToolError::new("not_found", "the working directory was not found")
            }
            CwdError::Invalid(why) => ToolError::from(format!("cwd: {why}")),
        })?;
    let caps = sh.backend.capabilities();
    if !caps.client_cwd {
        return Err(unsupported("a working directory the caller chooses"));
    }
    if !profile.env.is_empty() && !caps.client_env {
        return Err(unsupported(
            "a launch profile with an environment (use the native backend)",
        ));
    }
    // Pin the executable before any credential is read, independent of cwd.
    let program = sh.policy.program(profile).map_err(|why| {
        ToolError::new("not_found", "the program was not found")
            .with("profile", name.as_str())
            .with("backend_detail", why)
    })?;
    // The program, then the agent's permission flag, then the profile's own arguments.
    let mut argv = vec![program.to_string_lossy().into_owned()];
    argv.extend(profile.permission_args());
    argv.extend(profile.argv[1..].iter().cloned());
    Ok(RunSpec {
        argv: Some(argv),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        profile: Some(name),
        ..RunSpec::default()
    })
}

/// A caller-chosen `argv` or `line`, when the operator allows those. It never
/// gets a profile's environment.
fn command_spec(sh: &Shepherd, args: &Args) -> Result<RunSpec, ToolError> {
    if !sh.policy.allow_arbitrary_command {
        return Err(ToolError::new(
            "profile_required",
            "give profile: this server starts only its launch profiles (list_panes shows them)",
        ));
    }
    let argv_items = match args.get("argv") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(items),
        Some(_) => return Err("argv must be a list of strings".to_string().into()),
    };
    let line = optional_str(args, "line")?;
    let has_argv = argv_items.is_some_and(|items| !items.is_empty());
    if has_argv == line.is_some() {
        return Err("give exactly one of argv (non-empty list) or line"
            .to_string()
            .into());
    }

    // Same input rules as send_input, applied to every string that reaches the pane.
    let mut spec = RunSpec::default();
    if let Some(items) = argv_items.filter(|items| !items.is_empty()) {
        let mut argv = Vec::with_capacity(items.len());
        for item in items {
            let text = item
                .as_str()
                .ok_or_else(|| "argv must be a list of strings".to_string())?;
            argv.push(validate_input(text).map_err(|err| format!("argv: {err}"))?);
        }
        spec.argv = Some(argv);
    }
    if let Some(line) = line {
        spec.line = Some(validate_input(line).map_err(|err| format!("line: {err}"))?);
    }
    if let Some(cwd) = optional_str(args, "cwd")? {
        spec.cwd = Some(validate_input(cwd).map_err(|err| format!("cwd: {err}"))?);
    }
    spec.env = env_arg(args)?;
    if let Some(name) = optional_str(args, "name")? {
        validate_name(name).map_err(|err| format!("name: {err}"))?;
    }

    let caps = sh.backend.capabilities();
    if !caps.client_command {
        return Err(unsupported("starting a program the caller chooses"));
    }
    if spec.cwd.is_some() && !caps.client_cwd {
        return Err(unsupported("a working directory the caller chooses"));
    }
    if !spec.env.is_empty() && !caps.client_env {
        return Err(unsupported("a per-run environment"));
    }
    Ok(spec)
}

/// Most variables one `start_pane` may add.
const MAX_RUN_ENV: usize = 32;

/// `env`: an object of names (`[A-Z_][A-Z0-9_]*`) to string values. Values
/// follow the input rules, except that an empty value is allowed. Which
/// names a backend accepts (it may protect some) is the backend's rule.
fn env_arg(args: &Args) -> Result<Vec<(String, String)>, String> {
    let map = match args.get("env") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(map)) => map,
        Some(_) => return Err("env must be an object of names to strings".to_string()),
    };
    if map.len() > MAX_RUN_ENV {
        return Err(format!("env: at most {MAX_RUN_ENV} variables are allowed"));
    }
    let mut env = Vec::with_capacity(map.len());
    for (key, value) in map {
        let valid_key = key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
            && key
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        if !valid_key {
            return Err("env: a name must match [A-Z_][A-Z0-9_]*".to_string());
        }
        let value = value
            .as_str()
            .ok_or_else(|| format!("env: {key} must be a string"))?;
        let value = if value.is_empty() {
            String::new()
        } else {
            validate_input(value).map_err(|err| format!("env: {key}: {err}"))?
        };
        env.push((key.clone(), value));
    }
    Ok(env)
}

/// A name for a pane started without one: `pane-` and 12 hex digits.
fn generated_name() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(COUNT.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("pane-{:012x}", hasher.finish() & 0xffff_ffff_ffff)
}

fn send_input(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let text = validate_input(required_str(args, "text")?).map_err(|err| format!("text: {err}"))?;
    send_text(sh, &terminal, &text)
}

/// Type `text` into the pane `terminal` and press Enter: send_input's whole
/// path, under the pane lock and with its refusals (a blocked pane, the Devin
/// mode-switch rule). The routine's nudge goes through here too, so a nudge
/// is refused exactly where a caller's text would be.
///
/// This function must not grow caller-specific behaviour: the only
/// difference a caller may add is an extra [`InputGuard`] condition through
/// `send_text_guarded`, which narrows when a send is refused, never what is
/// sent.
fn send_text(sh: &Shepherd, terminal: &str, text: &str) -> Result<Value, ToolError> {
    send_text_guarded(sh, terminal, text, None, None)
}

fn send_text_guarded(
    sh: &Shepherd,
    terminal: &str,
    text: &str,
    extra_guard: Option<InputGuard>,
    expected_pane_id: Option<&str>,
) -> Result<Value, ToolError> {
    send_text_within(
        sh,
        terminal,
        text,
        extra_guard,
        expected_pane_id,
        None,
        None,
    )
}

/// Who a routine's send is recorded as: the fixed actor, the routine's id
/// and the client that created it (`profile` for the profile routine).
#[derive(Clone, Copy)]
struct RoutineAudit<'a> {
    routine: &'a str,
    created_by: &'a str,
}

impl RoutineAudit<'_> {
    const PROFILE: RoutineAudit<'static> = RoutineAudit {
        routine: "profile",
        created_by: "profile",
    };
}

/// `send_text_guarded` with an optional bound on the wait for the pane lock:
/// a lock still held when `lock_wait` runs out is a `pane_busy` refusal and
/// nothing is sent. `None` waits as every other tool does.
///
/// `routine` is set by a caller that does not come through `mcp::call_tool`
/// (the routine), which records MCP calls itself: the send is then recorded
/// as a `send_input` by `system:routine` with the routine's id and creator, a
/// `call` before it (the send is refused when that cannot be written) and an
/// `outcome` after it, `ok` or `error:<code>`. Never the text. This adds no
/// behaviour to the send itself.
fn send_text_within(
    sh: &Shepherd,
    terminal: &str,
    text: &str,
    extra_guard: Option<InputGuard>,
    expected_pane_id: Option<&str>,
    lock_wait: Option<Duration>,
    routine: Option<RoutineAudit<'_>>,
) -> Result<Value, ToolError> {
    let (Some(who), Some(audit)) = (routine, sh.audit()) else {
        return send_text_inner(sh, terminal, text, extra_guard, expected_pane_id, lock_wait);
    };
    let mut args = Args::new();
    args.insert("terminal".into(), json!(terminal));
    args.insert("text".into(), json!(text));
    args.insert("routine".into(), json!(who.routine));
    args.insert("created_by".into(), json!(who.created_by));
    audit
        .before(audit::ROUTINE_ACTOR, "send_input", &args)
        .map_err(|unavailable| unavailable.to_tool_error())?;
    let result = send_text_inner(sh, terminal, text, extra_guard, expected_pane_id, lock_wait);
    audit.after(audit::ROUTINE_ACTOR, "send_input", &args, &result);
    result
}

fn send_text_inner(
    sh: &Shepherd,
    terminal: &str,
    text: &str,
    extra_guard: Option<InputGuard>,
    expected_pane_id: Option<&str>,
    lock_wait: Option<Duration>,
) -> Result<Value, ToolError> {
    let (_guard, pane, now) = lock_pane_within(sh, terminal, lock_wait)?;
    // A caller that observed a specific pane (the routine) can pin the
    // send to that pane's id: if the name was reused while waiting for
    // the pane lock, `lock_pane`'s own check sees two reads of the NEW
    // pane and passes, so compare against the observed id here.
    if let Some(expected) = expected_pane_id {
        if now.id != expected {
            return Err(ToolError::new(
                "terminal_changed",
                format!(
                    "{terminal} named a different pane than the one this send was conditioned on; nothing was sent"
                ),
            )
            .with("terminal", terminal));
        }
    }
    if sh.agent_of(terminal) == Some(Agent::Devin) && is_devin_mode_switch(text) {
        return Err(ToolError::new(
            "refused",
            "refused by the Devin mode-switch rule: /yolo can switch Devin into Bypass mode, which Shepherd never allows; nothing was sent",
        )
        .with("terminal", terminal));
    }
    if sh.agent_of(terminal) == Some(Agent::Kiro)
        && matches!(now.turn, None | Some(0))
        && kiro_trust_warning_shown(sh, &pane)
    {
        return Err(ToolError::new(
            "pane_needs_trust_acceptance",
            format!(
                "{terminal} shows Kiro's trust-all-tools warning, so nothing was typed: a key here would choose `No, exit` and end the session. Shepherd never answers this dialog. A person must accept it by attaching to the pane, or start the pane without -a"
            ),
        )
        .with("terminal", terminal)
        .with("next", TRUST_ACCEPTANCE_NEXT));
    }
    // Idle and done are both sendable; a blocked program is never typed into.
    let guard = match extra_guard {
        Some(extra) => InputGuard {
            refuse_when_blocked: true,
            if_state: extra.if_state,
            if_kind: extra.if_kind,
            expect_turn: extra.expect_turn,
        },
        None => InputGuard {
            refuse_when_blocked: true,
            ..InputGuard::default()
        },
    };
    if !sh.backend.capabilities().conditional_input {
        // The backend cannot refuse atomically: check the status got inside the
        // lock, then send.
        if let Some(status) = now.status.as_ref().filter(|s| s.state == State::Blocked) {
            return Err(pane_blocked(
                terminal,
                status.kind.clone(),
                "refused by Shepherd: the status read inside the pane lock was blocked",
            ));
        }
        if let Some(want) = &guard.if_state {
            if now.status.as_ref().map(|s| &s.state) != Some(want) {
                return Err(ToolError::new(
                    "status_mismatch",
                    "refused by Shepherd: the status read inside the pane lock no longer matches the state the send was conditioned on; nothing was sent"
                        .to_string(),
                )
                .with("terminal", terminal));
            }
        }
        if let Some(want) = guard.expect_turn {
            if now.turn != Some(want) {
                return Err(ToolError::new(
                    "status_mismatch",
                    "refused by Shepherd: the turn read inside the pane lock no longer matches the turn the send was conditioned on; nothing was sent"
                        .to_string(),
                )
                .with("terminal", terminal));
            }
        }
    }
    match sh.backend.send(&pane, text, &guard) {
        Ok(sent) => {
            // A new OpenCode turn must not inherit a deny from an earlier one.
            if sent.started != Some(false) && sh.agent_of(terminal) == Some(Agent::OpenCode) {
                sh.clear_answer(terminal);
            }
            Ok(json!({
                "terminal": terminal,
                "turn": sent.turn,
                "started": sent.started,
                "status": sent.status.as_ref().map(State::as_str),
            }))
        }
        Err(BackendError {
            kind: ErrorKind::Blocked { kind: None },
            detail,
        }) => {
            // The refusal did not say which dialog; read the pane now, best
            // effort, and fall back to the read taken before the send.
            let blocked_kind = sh
                .backend
                .get(&pane)
                .ok()
                .and_then(|info| blocked_kind_of(&info))
                .or_else(|| blocked_kind_of(&now));
            Err(pane_blocked(terminal, blocked_kind, detail))
        }
        Err(err) => Err(backend_failure(Some(terminal), err)),
    }
}

/// Devin slash commands that may change the permission mode. A name-based check
/// against accidental use, not a defense against a different spelling or a wrapper.
fn is_devin_mode_switch(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case("/yolo")
}

fn blocked_kind_of(info: &TerminalInfo) -> Option<String> {
    info.status
        .as_ref()
        .filter(|status| status.state == State::Blocked)
        .and_then(|status| status.kind.as_deref().map(dialog_kind))
}

/// Resolve `terminal`, the caller's pane name, with `get`. A backend may also
/// accept its own references (for example its own id), so the pane found must
/// carry exactly this name; otherwise no pane has it. Every later backend call
/// uses the resolved pane's id ([`TerminalInfo::reference`]), never the
/// caller's string.
fn resolve(sh: &Shepherd, terminal: &str) -> Result<TerminalInfo, ToolError> {
    let info = get_pane(sh, &TerminalRef::new(terminal), terminal)?;
    if info.name.as_deref() != Some(terminal) {
        return Err(ToolError::new(
            "not_found",
            format!(
                "no pane is named {terminal}; panes are addressed by the name list_panes shows"
            ),
        )
        .with("terminal", terminal));
    }
    Ok(info)
}

/// Resolve `terminal` (a pane name), hold the lock for its id, then resolve
/// it again inside the lock and check it is still the same pane. Returns the
/// guard, a reference by id, and the pane as read inside the lock. Every tool
/// that types or presses keys into an existing pane calls this first, so
/// Shepherd's own calls on one pane run one at a time. start_pane does not:
/// `run` refuses a name that is taken (`name_taken`), so it never types into
/// an existing pane.
fn lock_pane(
    sh: &Shepherd,
    terminal: &str,
) -> Result<(PaneGuard, TerminalRef, TerminalInfo), ToolError> {
    lock_pane_within(sh, terminal, None)
}

/// [`lock_pane`] with an optional bound on the wait for the lock: `None`
/// waits as long as it takes, `Some(wait)` gives up with `pane_busy`.
fn lock_pane_within(
    sh: &Shepherd,
    terminal: &str,
    lock_wait: Option<Duration>,
) -> Result<(PaneGuard, TerminalRef, TerminalInfo), ToolError> {
    let before = resolve(sh, terminal)?;
    let guard = match lock_wait {
        None => sh.locks.lock(&before.id),
        Some(wait) => sh.locks.lock_within(&before.id, wait).ok_or_else(|| {
            ToolError::new(
                "pane_busy",
                format!("{terminal} stayed locked by another call; nothing was sent"),
            )
            .with("terminal", terminal)
        })?,
    };
    let now = resolve(sh, terminal)?;
    if now.id != before.id {
        return Err(ToolError::new(
            "terminal_changed",
            format!(
                "{terminal} named a different pane while waiting for the pane lock; nothing was sent"
            ),
        )
        .with("terminal", terminal));
    }
    Ok((guard, before.reference(), now))
}

fn get_pane(sh: &Shepherd, pane: &TerminalRef, terminal: &str) -> Result<TerminalInfo, ToolError> {
    sh.backend
        .get(pane)
        .map_err(|err| backend_failure(Some(terminal), err))
}

fn read_output(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let lines = match args.get("lines") {
        None | Some(Value::Null) => 0,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| "lines must be a non-negative integer".to_string())?,
    };
    if lines > READ_MAX_LINES {
        return Err(format!("lines must be between 0 and {READ_MAX_LINES}").into());
    }
    if !sh.backend.capabilities().screen_text {
        return Err(unsupported("reading screen text"));
    }
    if lines > 0 && !sh.backend.capabilities().history {
        return Err(unsupported("reading history"));
    }
    let pane = resolve(sh, &terminal)?.reference();
    let screen = sh
        .backend
        .read(&pane, lines as u32)
        .map_err(|err| backend_failure(Some(&terminal), err))?;
    Ok(json!({
        "text": strip_terminal_escapes(&screen.text),
        "complete": screen.complete,
        "source": screen.source.as_ref().map(|source| source.as_str()),
        "reason": screen.limit.as_ref().map(|limit| limit.as_str()),
    }))
}

fn get_status(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let info = resolve(sh, &terminal)?;
    let mut out = pane_json(sh, &info);
    out["terminal"] = json!(terminal);
    Ok(out)
}

/// [`TerminalInfo::to_json`] plus, when a person is at the pane, `human`:
/// `lease` (null, or holder, idle_secs and expires_in_secs) and `draft`. The
/// object is left out when there is neither, and when the backend has no
/// person side, so the common output does not change.
fn pane_json(sh: &Shepherd, info: &TerminalInfo) -> Value {
    let mut out = info.to_json();
    let Some(shared) = &sh.shared else {
        return out;
    };
    // A pane that went away between the read and this one has nothing to show.
    let Ok(state) = shared.human_state(&info.reference()) else {
        return out;
    };
    if !state.is_held() {
        return out;
    }
    out["human"] = json!({
        "lease": state.lease.as_ref().map(|lease| json!({
            "holder": lease.holder,
            "idle_secs": lease.idle.as_secs(),
            "expires_in_secs": lease.expires_in.as_secs() + u64::from(lease.expires_in.subsec_nanos() > 0),
        })),
        "draft": state.draft,
    });
    out
}

fn wait_status(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let timeout = optional_number(args, "timeout", WAIT_MIN_SECONDS, WAIT_MAX_SECONDS)?
        .unwrap_or(WAIT_DEFAULT_SECONDS);
    let pane = resolve(sh, &terminal)?.reference();
    let agent = sh.agent_of(&terminal);
    let result =
        wait_settled(sh, &pane, agent, Duration::from_secs_f64(timeout)).map_err(|err| {
            let protocol = err.kind == ErrorKind::Protocol;
            let failure = backend_failure(Some(&terminal), err);
            if protocol {
                failure.context("the wait result is unknown and is not treated as idle")
            } else {
                failure
            }
        })?;
    let state = result.status.as_ref().map(|status| &status.state);
    let answered = sh.answer_pending(&terminal);
    let mut outcome = wait_outcome(result.ended, result.reason.as_ref(), state, answered);
    // OpenCode reports no done state: a turn ends clear, then idle. An idle
    // after at least one turn (the turn number is 0 until the first leaves
    // working) is its done.
    if outcome == "other"
        && agent == Some(Agent::OpenCode)
        && result.ended
        && result.reason == Some(WaitReason::Status)
        && state == Some(&State::Idle)
        && result.turn.is_some_and(|turn| turn > 0)
    {
        outcome = "done";
    }
    if matches!(outcome, "done" | "cancelled" | "blocked" | "exited") {
        sh.clear_answer(&terminal);
    }
    let mut out = json!({
        "terminal": terminal,
        "outcome": outcome,
        "turn_complete": matches!(outcome, "done" | "exited"),
        "ended": result.ended,
        "reason": result.reason.as_ref().map(WaitReason::as_str),
        "status": state.map(State::as_str),
        "kind": Value::Null,
        "exit_code": result.exit_code,
        "turn": result.turn,
    });
    if outcome == "blocked" {
        match result
            .status
            .as_ref()
            .and_then(|status| status.kind.clone())
        {
            Some(kind) => out["kind"] = json!(dialog_kind(&kind)),
            None => {
                // The wait did not say which dialog; read the pane for its kind.
                let now = get_pane(sh, &pane, &terminal).map_err(|err| {
                    err.context("the wait ended blocked, but reading the dialog kind failed")
                })?;
                out["kind"] = json!(blocked_kind_of(&now));
                out["status_now"] = status_json(now.status.as_ref());
            }
        }
    }
    Ok(out)
}

/// The backend's wait. OpenCode reports `clear` between `working` and `idle`
/// at the end of a turn; it maps to the unknown state, which would end the wait
/// for a moment before the idle that closes the turn. For an OpenCode pane such
/// a result is waited out again, within the caller's timeout.
fn wait_settled(
    sh: &Shepherd,
    pane: &TerminalRef,
    agent: Option<Agent>,
    timeout: Duration,
) -> Result<WaitResult, BackendError> {
    let end = Instant::now() + timeout;
    let mut remaining = timeout;
    loop {
        let result = sh.backend.wait(pane, remaining)?;
        let settling = agent == Some(Agent::OpenCode)
            && result.ended
            && result.reason == Some(WaitReason::Status)
            && result.status.as_ref().map(|status| &status.state) == Some(&State::Other);
        let left = end.saturating_duration_since(Instant::now());
        if !settling || left <= SETTLE_INTERVAL {
            return Ok(result);
        }
        thread::sleep(SETTLE_INTERVAL);
        remaining = end.saturating_duration_since(Instant::now());
    }
}

/// What a wait result means. Only `done` and `exited` complete a turn. `idle`
/// after a dialog this process answered is `cancelled`: a Devin deny or cancel
/// returns the pane to idle, not done.
fn wait_outcome(
    ended: bool,
    reason: Option<&WaitReason>,
    state: Option<&State>,
    answered: bool,
) -> &'static str {
    match (ended, reason, state) {
        (false, Some(WaitReason::Timeout), _) => "timeout",
        (true, Some(WaitReason::Exit), _) => "exited",
        (true, Some(WaitReason::Status), Some(State::Done)) => "done",
        (true, Some(WaitReason::Status), Some(State::Blocked)) => "blocked",
        (true, Some(WaitReason::Status), Some(State::Idle)) if answered => "cancelled",
        _ => "other",
    }
}

fn list_panes(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let all = match args.get("all") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(all)) => *all,
        Some(_) => return Err("all must be a boolean".to_string().into()),
    };
    let list = sh
        .backend
        .list(all)
        .map_err(|err| backend_failure(None, err))?;
    let panes: Vec<Value> = list
        .terminals
        .iter()
        .map(|pane| pane_json(sh, pane))
        .collect();
    // Running panes are listed whether or not `all` is set.
    let live_panes = list.terminals.iter().filter(|pane| pane.live).count();
    let launch = sh.policy.to_json();
    Ok(json!({
        "panes": panes,
        "hidden_finished": list.hidden_finished,
        "live_panes": live_panes,
        "live_pane_limit": list.live_limit,
        "profiles": launch["profiles"],
        "allow_arbitrary_command": launch["allow_arbitrary_command"],
    }))
}

fn interrupt_pane(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let timeout = optional_number(args, "timeout", WAIT_MIN_SECONDS, INTERRUPT_MAX_SECONDS)?;
    let (_guard, pane, _) = lock_pane(sh, &terminal)?;
    let result = sh
        .backend
        .interrupt(&pane, timeout.map(Duration::from_secs_f64))
        .map_err(|err| backend_failure(Some(&terminal), err))?;
    Ok(json!({
        "terminal": terminal,
        "key": result.key.as_str(),
        "outcome": result.outcome.as_str(),
    }))
}

/// Stop the pane's program if it runs, then remove the pane, holding the pane
/// lock throughout. A call that waited for the lock and finds the name gone
/// gets not_found from `lock_pane`.
fn remove_pane(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let grace = optional_number(args, "timeout", REMOVE_MIN_SECONDS, REMOVE_MAX_SECONDS)?
        .unwrap_or(REMOVE_DEFAULT_SECONDS);
    let (_guard, pane, _) = lock_pane(sh, &terminal)?;
    let removed = sh
        .backend
        .remove(&pane, Some(Duration::from_secs_f64(grace)))
        .map_err(|err| backend_failure(Some(&terminal), err))?;
    // Forget only this pane's agent record and watch, keyed by the id
    // `lock_pane` resolved: a new start may have reused the name and
    // registered its own records while this removal was still running,
    // and dropping the new pane's agent record would disable the Devin
    // mode-switch refusal for it.
    sh.forget_agent_if(&terminal, pane.as_str());
    sh.forget_routine_if(&terminal, pane.as_str());
    sh.forget_schedule_if(&terminal, pane.as_str());
    sh.registry_write(pane.as_str(), "removal", |r| r.forget(pane.as_str()));
    Ok(json!({
        "name": terminal,
        "was_live": removed.was_live,
        "stopped": removed.stopped.as_ref().map(StopHow::as_str),
        "exit_code": removed.exit_code,
    }))
}

/// Clear a pane's draft flag (issue #77). Does not take the pane lock: it types
/// nothing, and a draft left behind must be clearable while a call is stuck on
/// that lock. The backend waits (bounded) for a write in flight, so it never
/// lands inside a bot's paste and its Enter.
fn draft_clear(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let Some(shared) = &sh.shared else {
        return Err(unsupported("clearing a draft"));
    };
    let pane = resolve(sh, &terminal)?.reference();
    let cleared = shared
        .draft_clear(&pane)
        .map_err(|err| backend_failure(Some(&terminal), err))?;
    Ok(json!({"terminal": terminal, "cleared": cleared}))
}

/// What `routine_create` and friends return for one routine.
fn routine_json(terminal: &str, routine: &Routine, wall: u64) -> Value {
    let Trigger::Every { every_secs } = routine.trigger else {
        return json!({"terminal": terminal, "id": routine.id});
    };
    let crate::routine_model::Action::Type { text } = &routine.action;
    let active = routine.status.is_active();
    json!({
        "terminal": terminal,
        "id": routine.id,
        "source": "agent",
        "trigger": {"type": "every", "every_secs": every_secs},
        "action": {"type": "text", "text": text},
        "todo_file": routine.todo_file,
        "created_by": routine.created_by,
        "created_at": routine.created_at,
        "expires_at": routine.expires_at,
        "max_runs": routine.max_runs,
        "runs": routine.runs,
        "status": routine.status.as_str(),
        "stop_reason": routine.stop_reason,
        "last_run_at": routine.last_run_at,
        "last_result": routine.last_result,
        "next_due_at": routine.next_due.filter(|_| active),
        "next_due_in_secs": routine.next_due.filter(|_| active).map(|due| due.saturating_sub(wall)),
        "expires_in_secs": routine.expires_at.filter(|_| active).map(|at| at.saturating_sub(wall)),
    })
}

/// An integer argument: absent or null is `None`.
fn optional_u64(args: &Args, key: &str) -> Result<Option<u64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{key} must be a non-negative integer").into()),
    }
}

/// The pane a routine tool names: it must exist and still run.
fn routine_pane(sh: &Shepherd, terminal: &str) -> Result<TerminalInfo, ToolError> {
    let info = resolve(sh, terminal)?;
    if !info.live {
        return Err(ToolError::new(
            "pane_not_live",
            format!("{terminal} has finished; routines end with their pane"),
        )
        .with("terminal", terminal));
    }
    Ok(info)
}

fn invalid_routine(why: Invalid) -> ToolError {
    ToolError::new("routine_invalid", why.message)
        .with("field", why.field)
        .with("limit", why.limit)
}

fn limit_routine(why: Invalid) -> ToolError {
    let max = why.limit.parse::<u64>().ok();
    let err = ToolError::new("routine_limit", why.message)
        .with("field", why.field)
        .with("limit", why.limit);
    match max {
        Some(max) => err.with("max", max),
        None => err,
    }
}

fn not_saved(detail: String) -> ToolError {
    ToolError::new(
        "routine_not_saved",
        "the routine could not be kept for a restart, so nothing was changed; try again",
    )
    .with("backend_detail", detail)
}

fn routine_not_found(terminal: &str, id: &str) -> ToolError {
    ToolError::new(
        "routine_not_found",
        format!("{terminal} has no routine {id}; routine_list shows them"),
    )
    .with("terminal", terminal)
    .with("id", id)
}

fn update_failure(terminal: &str, id: &str, err: UpdateError) -> ToolError {
    match err {
        UpdateError::NotFound => routine_not_found(terminal, id),
        UpdateError::Invalid(why) => invalid_routine(why),
        UpdateError::Limit(why) => limit_routine(why),
        UpdateError::NotSaved(detail) => not_saved(detail),
    }
}

/// `profile` names the pane's read-only idle routine, never an agent routine.
fn refuse_profile_routine(sh: &Shepherd, terminal: &str, id: &str) -> Result<(), ToolError> {
    if id == "profile" && sh.has_profile_routine(terminal) {
        return Err(ToolError::new(
            "routine_read_only",
            "the profile's own routine is set in the launch profile and cannot be changed here",
        )
        .with("terminal", terminal)
        .with("id", id));
    }
    Ok(())
}

fn routine_create(sh: &Shepherd, client: &str, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let trigger = match args.get("trigger") {
        Some(Value::Object(trigger)) => trigger,
        Some(_) => return Err("trigger must be an object".to_string().into()),
        None => return Err("trigger is required".to_string().into()),
    };
    match optional_str(trigger, "type")? {
        Some("every") => {}
        Some(other) => {
            return Err(ToolError::new(
                "routine_invalid",
                format!("trigger.type {other:?} is not supported; only every is"),
            )
            .with("field", "trigger.type")
            .with("limit", "every"))
        }
        None => return Err("trigger.type is required".to_string().into()),
    }
    let every_secs = optional_u64(trigger, "every_secs")?
        .ok_or_else(|| "trigger.every_secs is required".to_string())?;
    let text = required_str(args, "text")?.to_string();
    let todo_file = optional_str(args, "todo_file")?.map(str::to_string);
    let expires_secs = optional_u64(args, "expires_secs")?;
    let max_runs = optional_u64(args, "max_runs")?;
    let info = routine_pane(sh, &terminal)?;
    let request = NewEvery {
        every_secs,
        text,
        todo_file,
        expires_secs,
        max_runs,
        created_by: client.to_string(),
    };
    let routine = sh
        .create_every_on(&terminal, Some(&info.id), request)
        .map_err(|err| match err {
            CreateError::NotAllowed => ToolError::new(
                "routines_not_allowed",
                format!(
                    "{terminal} was not started from a launch profile that allows routines \
                     (dynamic_routines); ask the operator to enable it"
                ),
            )
            .with("terminal", terminal.as_str()),
            CreateError::Invalid(why) => invalid_routine(why),
            CreateError::Limit(why) => limit_routine(why),
            CreateError::NotSaved(detail) => not_saved(detail),
        })?;
    let wall = sh.wall_secs();
    let ceiling = sh.ceiling_of(&terminal).unwrap_or_default();
    let active = sh
        .routines_of(&terminal)
        .iter()
        .filter(|r| r.status.is_active())
        .count();
    let max = usize::try_from(ceiling.max_per_pane).unwrap_or(usize::MAX);
    Ok(json!({
        "routine": routine_json(&terminal, &routine, wall),
        "ceiling": {
            "min_every_secs": ceiling.min_every_secs,
            "max_per_pane": ceiling.max_per_pane,
            "max_expires_secs": ceiling.max_expires_secs,
            "active": active,
            "free": max.saturating_sub(active),
        },
    }))
}

fn routine_list(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let names = match optional_str(args, "terminal")? {
        Some(raw) => {
            let terminal = validate_input(raw).map_err(|err| format!("terminal: {err}"))?;
            routine_pane(sh, &terminal)?;
            vec![terminal]
        }
        None => sh.panes_with_routines(),
    };
    let wall = sh.wall_secs();
    let now = Instant::now();
    let mut routines = Vec::new();
    for name in names {
        if let Some(mut profile) = sh.profile_routine_view(&name, now) {
            profile["terminal"] = json!(name);
            routines.push(profile);
        }
        for routine in sh.routines_of(&name) {
            routines.push(routine_json(&name, &routine, wall));
        }
    }
    Ok(json!({"routines": routines}))
}

fn routine_update(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let id = required_str(args, "id")?.to_string();
    let present = |key: &str| args.contains_key(key);
    let text = optional_str(args, "text")?.map(str::to_string);
    let request = EveryUpdate {
        every_secs: optional_u64(args, "every_secs")?,
        text,
        todo_file: if present("todo_file") {
            Some(optional_str(args, "todo_file")?.map(str::to_string))
        } else {
            None
        },
        expires_secs: optional_u64(args, "expires_secs")?,
        max_runs: if present("max_runs") {
            Some(optional_u64(args, "max_runs")?)
        } else {
            None
        },
    };
    // A null `every_secs`, `text` or `expires_secs` means "not given".
    let given = request.every_secs.is_some()
        || request.text.is_some()
        || request.todo_file.is_some()
        || request.expires_secs.is_some()
        || request.max_runs.is_some();
    if !given {
        return Err(
            "give at least one of every_secs, text, todo_file, expires_secs and max_runs"
                .to_string()
                .into(),
        );
    }
    let info = routine_pane(sh, &terminal)?;
    refuse_profile_routine(sh, &terminal, &id)?;
    let routine = sh
        .update_every(&terminal, &info.id, &id, &request)
        .map_err(|err| update_failure(&terminal, &id, err))?;
    Ok(json!({"routine": routine_json(&terminal, &routine, sh.wall_secs())}))
}

fn routine_delete(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let id = required_str(args, "id")?.to_string();
    let info = routine_pane(sh, &terminal)?;
    refuse_profile_routine(sh, &terminal, &id)?;
    sh.delete_every(&terminal, &info.id, &id)
        .map_err(|err| update_failure(&terminal, &id, err))?;
    Ok(json!({"terminal": terminal, "id": id, "deleted": true}))
}

/// A backend failure in Shepherd's words. The backend's own text is kept only
/// under `backend_detail`.
fn backend_failure(terminal: Option<&str>, err: BackendError) -> ToolError {
    let pane = terminal.unwrap_or("the pane");
    let message = match &err.kind {
        ErrorKind::Blocked { kind } => return pane_blocked(pane, kind.clone(), err.detail),
        ErrorKind::Unavailable => "the terminal backend is not reachable".to_string(),
        ErrorKind::Timeout => "the terminal backend did not answer in time".to_string(),
        ErrorKind::NotFound => match terminal {
            Some(terminal) => format!("no pane matches {terminal}"),
            None => "the terminal backend could not find what the request named".to_string(),
        },
        ErrorKind::NameTaken => format!("a pane named {pane} already exists; choose another name"),
        ErrorKind::StatusMismatch => {
            format!("{pane} is not in the state the request required; nothing was sent")
        }
        ErrorKind::ScreenRefused => {
            format!("the screen check on {pane} refused the key; nothing was sent")
        }
        ErrorKind::Finished => format!("{pane} has finished; its program is no longer running"),
        ErrorKind::Replayed => format!(
            "{pane} was picked up after a restart and its screen is not yet trusted; nothing was sent"
        ),
        ErrorKind::HumanControlled {
            expires_in_secs, ..
        } => format!(
            "a person is typing into {pane}; nothing was sent. Their lease ends in \
             {expires_in_secs} s unless they keep typing; look again later"
        ),
        ErrorKind::DraftPending => format!(
            "a person has typed into {pane} and not pressed Enter; nothing was sent. \
             Look again later, or clear the mark with draft_clear (it needs the \
             lease:preempt scope) when the person is gone"
        ),
        ErrorKind::Unsupported(op) => format!("the terminal backend does not support {op}"),
        ErrorKind::InvalidInput => "the terminal backend refused the request".to_string(),
        ErrorKind::Protocol => "the terminal backend sent a reply Shepherd cannot use".to_string(),
        ErrorKind::Other => "the terminal backend reported a failure".to_string(),
    };
    let mut failure = ToolError::new(err.code(), message);
    if let Some(terminal) = terminal {
        failure = failure.with("terminal", terminal);
    }
    if let ErrorKind::HumanControlled {
        idle_secs,
        expires_in_secs,
    } = &err.kind
    {
        failure = failure
            .with("idle_secs", *idle_secs)
            .with("expires_in_secs", *expires_in_secs);
    }
    failure.with("backend_detail", err.detail)
}

/// Typing was refused because the program is blocked on a dialog.
fn pane_blocked(terminal: &str, kind: Option<String>, detail: impl Into<String>) -> ToolError {
    let kind = kind.as_deref().map(dialog_kind);
    let dialog = match &kind {
        Some(kind) if kind == "other" => "a dialog of another kind".to_string(),
        Some(kind) => format!("a {kind} dialog"),
        None => "a dialog".to_string(),
    };
    let auth = kind.as_deref() == Some("auth");
    let (message, next) = if auth {
        (
            format!("{terminal} is blocked on an auth prompt: {AUTH_BLOCKED}. Nothing was typed"),
            AUTH_NEXT,
        )
    } else {
        (
            format!("{terminal} is blocked on {dialog}; nothing was typed. Read the screen with read_output, then answer with answer_dialog"),
            BLOCKED_NEXT,
        )
    };
    ToolError::new("pane_blocked", message)
        .with("terminal", terminal)
        .with("kind", kind)
        .with("next", next)
        .with("backend_detail", detail.into())
}

fn unsupported(op: &str) -> ToolError {
    ToolError::new(
        "unsupported",
        format!("the terminal backend does not support {op}"),
    )
}

fn answer_dialog(sh: &Shepherd, args: &Args) -> Result<Value, ToolError> {
    let terminal = terminal_arg(args)?;
    let choice_text = required_str(args, "choice")?;
    let choice = Choice::parse(choice_text)
        .ok_or_else(|| format!("choice must be allow_once or deny_once, not {choice_text:?}"))?;
    let caps = sh.backend.capabilities();
    if !caps.screen_text {
        return Err(unsupported("reading screen text"));
    }
    let mut sent: Vec<Vec<String>> = Vec::new();

    // Hold the pane lock from the first get through Enter.
    let (_guard, pane, got) = lock_pane(sh, &terminal)?;

    // 1. Only a pane blocked on a permission dialog.
    let status = status_json(got.status.as_ref());
    if !got
        .status
        .as_ref()
        .is_some_and(|s| s.is_blocked_with("permission"))
    {
        let reason = if got
            .status
            .as_ref()
            .is_some_and(|s| s.is_blocked_with("auth"))
        {
            format!("this is an auth prompt, not a permission dialog, and answer_dialog cannot answer it: {AUTH_BLOCKED}; status is {status}")
        } else {
            format!("the pane is not blocked on a permission dialog: status is {status}")
        };
        return Err(refusal(
            &terminal,
            choice,
            reason,
            json!({"status": status}),
            &sent,
        ));
    }
    let Some(turn) = got.turn else {
        return Err(refusal(
            &terminal,
            choice,
            "the pane reported no turn number, so a change before Enter could not be detected"
                .to_string(),
            json!({"status": status, "turn": Value::Null}),
            &sent,
        ));
    };
    // The dialect comes from the profile that started the pane. A pane with
    // none, or one this process did not start, gets no keys.
    let Some(agent) = sh.agent_of(&terminal) else {
        return Err(refusal(
            &terminal,
            choice,
            "the pane was not started from a launch profile by this Shepherd process, so no dialect applies"
                .to_string(),
            json!({"status": status}),
            &sent,
        ));
    };
    // Every key goes to this dialog in this turn. A backend with conditional
    // keys enforces it; for one without, the checks below do (non-atomically).
    // OpenCode's dialog has its own path: the highlight is a colour that the
    // screen text does not carry, so allow_once reads styled cells (see
    // opencode_dialog).
    if agent == Agent::OpenCode {
        return answer_opencode(sh, &terminal, &pane, choice, turn, caps.conditional_keys);
    }
    // An agent the generic path does not serve has no option to press: refuse
    // before reading the screen, and send nothing.
    let Some((target, label)) = agent.option(choice) else {
        return Err(refusal(
            &terminal,
            choice,
            unmeasured(agent),
            json!({"status": status}),
            &sent,
        ));
    };
    let key_guard = InputGuard {
        refuse_when_blocked: false,
        if_state: Some(State::Blocked),
        if_kind: Some("permission".to_string()),
        expect_turn: Some(turn),
    };
    let ask = Ask {
        sh,
        pane: &pane,
        terminal: &terminal,
        choice,
        agent,
        turn,
        key_guard: &key_guard,
    };

    // 2. Parse the dialog from the screen, waiting for it to render. Between
    // reads, check the pane is still blocked on a permission dialog in the same
    // turn. Nothing is sent until the dialog parses.
    let mut screen = String::new();
    let mut parse_error = String::new();
    let mut parsed = None;
    for attempt in 0..RENDER_READS {
        if attempt > 0 {
            thread::sleep(RENDER_POLL_INTERVAL);
            let now = get_pane(sh, &pane, &terminal)?;
            let changes = pane_changes(&now, turn);
            if !changes.is_empty() {
                return Err(refusal(
                    &terminal,
                    choice,
                    format!(
                        "the pane changed while waiting for the dialog to render: {}",
                        changes.join("; ")
                    ),
                    json!({
                        "status": status_json(now.status.as_ref()),
                        "turn_before": turn,
                        "turn_now": now.turn,
                        "screen": tail(&screen),
                    }),
                    &sent,
                ));
            }
        }
        screen = read_screen(sh, &pane, &terminal)?;
        match agent.parse_dialog(&screen) {
            Ok(found) => {
                parsed = Some(found);
                break;
            }
            Err(err) => parse_error = err,
        }
    }
    let Some(first) = parsed else {
        return Err(refusal(
            &terminal,
            choice,
            format!(
                "cannot parse the permission dialog: timed out waiting for the dialog to render ({RENDER_READS} reads over {} ms): {parse_error}",
                RENDER_WAIT.as_millis()
            ),
            json!({"status": status, "screen": tail(&screen)}),
            &sent,
        ));
    };

    // 3. Move the cursor. Kiro: all the moves at once, then one read. Devin:
    // one key at a time, each checked against the screen, never past the target.
    let moves = if agent == Agent::Devin {
        step_devin(&ask, &mut sent, &first, &screen, target)?
    } else {
        let moves = target as isize - first.cursor as isize;
        if moves != 0 {
            let key = if moves > 0 { Key::Down } else { Key::Up };
            let keys = vec![key; moves.unsigned_abs()];
            let result = press(sh, &pane, &terminal, &keys, &key_guard)?;
            sent.push(keys.iter().map(|key| key.as_str().to_string()).collect());
            if result.status != Some(State::Blocked) {
                let after = result.status.as_ref().map(State::as_str);
                return Err(refusal(
                    &terminal,
                    choice,
                    format!("after the cursor moves the status is {after:?}, not blocked"),
                    json!({"keys_result": result.to_json()}),
                    &sent,
                ));
            }
        }
        moves
    };

    // 4. Confirm the cursor is on the target in the same dialog.
    let mut last_seen = String::new();
    let mut last_problem = String::new();
    let mut confirmed = false;
    for attempt in 0..CONFIRM_READS {
        if attempt > 0 {
            thread::sleep(CONFIRM_INTERVAL);
        }
        last_seen = read_screen(sh, &pane, &terminal)?;
        match agent.parse_dialog(&last_seen) {
            Ok(now) if now.title != first.title || now.command != first.command => {
                return Err(refusal(
                    &terminal,
                    choice,
                    format!(
                        "a different dialog is on the screen now: {:?} / {:?}",
                        now.title, now.command
                    ),
                    json!({"screen": tail(&last_seen)}),
                    &sent,
                ));
            }
            Ok(now) if !devin_scope_ok(agent, choice, &now) => {
                last_problem = format!(
                    "the Allow row does not show the Once scope: {:?}",
                    now.options[devin_dialog::INDEX_ALLOW]
                );
            }
            Ok(now) if now.cursor == target => {
                confirmed = true;
                break;
            }
            Ok(now) => {
                last_problem = format!("the cursor is on {:?}", now.options[now.cursor]);
            }
            Err(err) => last_problem = format!("cannot parse the dialog: {err}"),
        }
    }
    if !confirmed {
        return Err(ask.mismatch(
            &mut sent,
            format!(
                "the cursor is not confirmed on {:?} after {CONFIRM_READS} reads ({last_problem})",
                label
            ),
            json!({"screen": tail(&last_seen)}),
        )?);
    }

    // 5. Re-check, still inside the lock, that the pane is blocked on a
    // permission dialog in the same turn. A backend without conditional keys
    // cannot do this with the Enter, so the check and the Enter are two calls:
    // a person or another client of the backend can still act between them.
    if !caps.conditional_keys {
        let again = get_pane(sh, &pane, &terminal)?;
        let changes = pane_changes(&again, turn);
        if !changes.is_empty() {
            return Err(refusal(
                &terminal,
                choice,
                format!(
                    "the pane changed after the cursor was confirmed: {}",
                    changes.join("; ")
                ),
                json!({
                    "status": status_json(again.status.as_ref()),
                    "turn_before": turn,
                    "turn_now": again.turn,
                    "screen": tail(&last_seen),
                }),
                &sent,
            ));
        }
    }

    // 6. Select it.
    let result = press(sh, &pane, &terminal, &[Key::Enter], &key_guard)?;
    if agent == Agent::Devin {
        sh.mark_answered(&terminal);
    }
    let mut answer = json!({
        "terminal": terminal,
        "agent": agent.as_str(),
        "choice": choice.as_str(),
        "selected": label,
        "title": first.title,
        "command": first.command,
        "options": first.options,
        "moves": moves,
        "keys_result": result.to_json(),
    });
    if let Some(tool) = &first.tool {
        answer["tool"] = json!(tool);
    }
    Ok(answer)
}

/// OpenCode (v2.2.7). `deny_once` sends Esc, which cancels exactly like Reject
/// (measured), once the pane is blocked on a permission dialog in this turn and
/// the screen shows the whole expected dialog. `allow_once` presses Enter only
/// when the styled cells, read in the same step as the key, show `Allow once`
/// as the one option whose background differs from the other two, which
/// match each other; otherwise it refuses
/// with no key sent. Right is never sent, so `Always allow` is never reachable.
fn answer_opencode(
    sh: &Shepherd,
    terminal: &str,
    pane: &TerminalRef,
    choice: Choice,
    turn: u64,
    conditional_keys: bool,
) -> Result<Value, ToolError> {
    let sent: Vec<Vec<String>> = Vec::new();
    // Wait for the dialog text to render, checking between reads that the pane
    // is still blocked on a permission dialog in this turn.
    let mut screen = String::new();
    let mut parse_error = String::new();
    let mut parsed = None;
    for attempt in 0..RENDER_READS {
        if attempt > 0 {
            thread::sleep(RENDER_POLL_INTERVAL);
            let now = get_pane(sh, pane, terminal)?;
            let changes = pane_changes(&now, turn);
            if !changes.is_empty() {
                return Err(refusal(
                    terminal,
                    choice,
                    format!(
                        "the pane changed while waiting for the dialog to render: {}",
                        changes.join("; ")
                    ),
                    json!({
                        "status": status_json(now.status.as_ref()),
                        "turn_before": turn,
                        "turn_now": now.turn,
                        "screen": tail(&screen),
                    }),
                    &sent,
                ));
            }
        }
        screen = read_screen(sh, pane, terminal)?;
        match opencode_dialog::parse(&screen) {
            Ok(found) => {
                parsed = Some(found);
                break;
            }
            Err(err) => parse_error = err,
        }
    }
    let Some(dialog) = parsed else {
        return Err(refusal(
            terminal,
            choice,
            format!(
                "cannot parse the permission dialog: timed out waiting for the dialog to render ({RENDER_READS} reads over {} ms): {parse_error}",
                RENDER_WAIT.as_millis()
            ),
            json!({"screen": tail(&screen)}),
            &sent,
        ));
    };
    // Re-check, immediately before Esc, that the pane is still blocked on a
    // permission dialog in this turn. A backend with conditional keys also
    // enforces it with the key itself.
    if !conditional_keys {
        let again = get_pane(sh, pane, terminal)?;
        let changes = pane_changes(&again, turn);
        if !changes.is_empty() {
            return Err(refusal(
                terminal,
                choice,
                format!(
                    "the pane changed after the dialog was read: {}",
                    changes.join("; ")
                ),
                json!({
                    "status": status_json(again.status.as_ref()),
                    "turn_before": turn,
                    "turn_now": again.turn,
                    "screen": tail(&screen),
                }),
                &sent,
            ));
        }
    }
    let guard = InputGuard {
        refuse_when_blocked: false,
        if_state: Some(State::Blocked),
        if_kind: Some("permission".to_string()),
        expect_turn: Some(turn),
    };
    if choice == Choice::AllowOnce {
        // The styled cells are read and Enter is pressed in one step inside
        // the backend, after its status guard: the check and the key see the
        // same cells. Any failure to verify sends nothing.
        let check = |styled: &StyledScreen| -> Result<(), String> {
            let now = opencode_dialog::verify_selected(styled)?;
            if now != dialog {
                return Err(format!(
                    "the dialog changed since it was read: {:?} is now {:?}",
                    dialog.subject, now.subject
                ));
            }
            Ok(())
        };
        let result = match sh
            .backend
            .keys_if_screen(pane, &[Key::Enter], &guard, &check)
        {
            Ok(result) => result,
            Err(err) => {
                let reason = match &err.kind {
                    ErrorKind::ScreenRefused => format!(
                        "the highlighted option is not verified as {:?}: {}",
                        opencode_dialog::OPTION_ALLOW_ONCE,
                        err.detail
                    ),
                    ErrorKind::Unsupported(_) => opencode_dialog::colours_unavailable(),
                    _ => return Err(backend_failure(Some(terminal), err)),
                };
                return Err(refusal(
                    terminal,
                    choice,
                    reason,
                    json!({"screen": tail(&screen)}),
                    &sent,
                ));
            }
        };
        // An allowed command runs, so the idle that follows is the turn's
        // done, not a cancel. Drop a marker left by an earlier denied dialog
        // in the same turn.
        sh.clear_answer(terminal);
        return Ok(json!({
            "terminal": terminal,
            "agent": Agent::OpenCode.as_str(),
            "choice": choice.as_str(),
            "selected": opencode_dialog::OPTION_ALLOW_ONCE,
            "title": opencode_dialog::HEADING,
            "command": dialog.subject,
            "options": dialog.options,
            "keys": [Key::Enter.as_str()],
            "keys_result": result.to_json(),
        }));
    }
    let result = press(sh, pane, terminal, &[Key::Esc], &guard)?;
    sh.mark_answered(terminal);
    Ok(json!({
        "terminal": terminal,
        "agent": Agent::OpenCode.as_str(),
        "choice": choice.as_str(),
        "selected": opencode_dialog::OPTION_REJECT,
        "title": opencode_dialog::HEADING,
        "command": dialog.subject,
        "options": dialog.options,
        "keys": [Key::Esc.as_str()],
        "keys_result": result.to_json(),
    }))
}

/// What one answer_dialog call checks and sends keys with, after its pane was
/// found, its agent known, and its key guard built.
struct Ask<'a> {
    sh: &'a Shepherd,
    pane: &'a TerminalRef,
    terminal: &'a str,
    choice: Choice,
    agent: Agent,
    turn: u64,
    key_guard: &'a InputGuard,
}

impl Ask<'_> {
    /// A dialog that did not land as expected. Kiro: refuse and send nothing
    /// more. Devin: when the pane is still blocked on a permission dialog in
    /// this turn, send Esc to cancel the request, then refuse. Enter is never
    /// sent here.
    fn mismatch(
        &self,
        sent: &mut Vec<Vec<String>>,
        reason: String,
        seen: Value,
    ) -> Result<ToolError, ToolError> {
        if self.agent != Agent::Devin {
            return Ok(refusal(
                self.terminal,
                self.choice,
                reason,
                seen,
                sent.as_slice(),
            ));
        }
        let again = get_pane(self.sh, self.pane, self.terminal)?;
        let changes = pane_changes(&again, self.turn);
        if !changes.is_empty() {
            let reason = format!(
                "{reason}; the pane then changed, so nothing more was sent: {}",
                changes.join("; ")
            );
            return Ok(refusal(
                self.terminal,
                self.choice,
                reason,
                seen,
                sent.as_slice(),
            ));
        }
        press(
            self.sh,
            self.pane,
            self.terminal,
            &[Key::Esc],
            self.key_guard,
        )?;
        sent.push(vec![Key::Esc.as_str().to_string()]);
        self.sh.mark_answered(self.terminal);
        let reason = format!("{reason}; Esc cancelled the dialog");
        Ok(refusal(
            self.terminal,
            self.choice,
            reason,
            seen,
            sent.as_slice(),
        ))
    }
}

/// Devin: the highlight must start on Allow. Each Down moves it one row, and the
/// screen is read after each key. The highlight must be on the next row before
/// another key is sent, so it never passes the target. Up is never sent: its
/// effect on this dialog was not measured. Any other reading is a mismatch, so
/// Enter is never sent from here. Returns the moves made.
fn step_devin(
    ask: &Ask<'_>,
    sent: &mut Vec<Vec<String>>,
    first: &ParsedDialog,
    first_screen: &str,
    target: usize,
) -> Result<isize, ToolError> {
    if first.cursor != devin_dialog::INDEX_ALLOW {
        let reason = format!(
            "the highlight starts on {:?}, not on Allow; Devin dialogs are answered only from Allow",
            first.options[first.cursor]
        );
        return Err(ask.mismatch(sent, reason, json!({"screen": tail(first_screen)}))?);
    }
    if !devin_scope_ok(ask.agent, ask.choice, first) {
        let reason = format!(
            "the Allow row does not show the Once scope: {:?}",
            first.options[devin_dialog::INDEX_ALLOW]
        );
        return Err(ask.mismatch(sent, reason, json!({"screen": tail(first_screen)}))?);
    }
    let mut cursor = first.cursor;
    let mut moves: isize = 0;
    while cursor < target {
        let next = cursor + 1;
        let result = press(
            ask.sh,
            ask.pane,
            ask.terminal,
            std::slice::from_ref(&Key::Down),
            ask.key_guard,
        )?;
        sent.push(vec![Key::Down.as_str().to_string()]);
        moves += 1;
        if result.status != Some(State::Blocked) {
            let after = result.status.as_ref().map(State::as_str);
            let reason = format!("after a move the status is {after:?}, not blocked");
            return Err(ask.mismatch(sent, reason, json!({"keys_result": result.to_json()}))?);
        }
        // Devin redraws the highlight about 60 ms after the key, so the first
        // read can still show the old row. Re-read a bounded number of times;
        // only the old row (or an unparseable screen) is waited out. Any other
        // row, or a different dialog, is a mismatch at once.
        let mut landed = None;
        let mut last_screen = String::new();
        let mut last_problem = String::new();
        for attempt in 0..CONFIRM_READS {
            if attempt > 0 {
                thread::sleep(CONFIRM_INTERVAL);
            }
            last_screen = read_screen(ask.sh, ask.pane, ask.terminal)?;
            match ask.agent.parse_dialog(&last_screen) {
                Ok(now) if now.title != first.title || now.command != first.command => {
                    let reason = "a different dialog is on the screen after a move".to_string();
                    return Err(ask.mismatch(
                        sent,
                        reason,
                        json!({"screen": tail(&last_screen)}),
                    )?);
                }
                Ok(now) if now.cursor == next => {
                    landed = Some(now);
                    break;
                }
                Ok(now) if now.cursor == cursor => {
                    last_problem = format!(
                        "after a move the highlight is on {:?}, not on {:?}",
                        now.options[now.cursor], first.options[next]
                    );
                }
                Ok(now) => {
                    let reason = format!(
                        "after a move the highlight is on {:?}, not on {:?}",
                        now.options[now.cursor], first.options[next]
                    );
                    return Err(ask.mismatch(
                        sent,
                        reason,
                        json!({"screen": tail(&last_screen)}),
                    )?);
                }
                Err(err) => last_problem = format!("cannot parse the dialog after a move: {err}"),
            }
        }
        let Some(now) = landed else {
            let reason = format!("{last_problem} (after {CONFIRM_READS} reads)");
            return Err(ask.mismatch(sent, reason, json!({"screen": tail(&last_screen)}))?);
        };
        cursor = now.cursor;
    }
    Ok(moves)
}

/// Devin's allow_once approves the Once scope only. Any other scope on the Allow
/// row is refused. Every other choice passes.
fn devin_scope_ok(agent: Agent, choice: Choice, dialog: &ParsedDialog) -> bool {
    agent != Agent::Devin
        || choice != Choice::AllowOnce
        || devin_dialog::allow_scope_is_once(&dialog.options[devin_dialog::INDEX_ALLOW])
}

/// What differs between a pane and "blocked with kind permission in turn
/// `turn`". Empty when nothing changed.
fn pane_changes(info: &TerminalInfo, turn: u64) -> Vec<String> {
    let mut changes = Vec::new();
    if !info
        .status
        .as_ref()
        .is_some_and(|s| s.is_blocked_with("permission"))
    {
        changes.push(format!(
            "the status is now {}, not blocked with kind permission",
            status_json(info.status.as_ref())
        ));
    }
    if info.turn != Some(turn) {
        changes.push(format!("the turn is now {}, not {turn}", json!(info.turn)));
    }
    changes
}

fn read_screen(sh: &Shepherd, pane: &TerminalRef, terminal: &str) -> Result<String, ToolError> {
    let screen = sh
        .backend
        .read(pane, 0)
        .map_err(|err| backend_failure(Some(terminal), err))?;
    Ok(strip_terminal_escapes(&screen.text))
}

fn press(
    sh: &Shepherd,
    pane: &TerminalRef,
    terminal: &str,
    keys: &[Key],
    guard: &InputGuard,
) -> Result<KeysResult, ToolError> {
    sh.backend
        .keys(pane, keys, guard)
        .map_err(|err| backend_failure(Some(terminal), err))
}

/// An answer_dialog refusal: what was seen, and which keys (if any) were sent.
/// Enter is never among them.
fn refusal(
    terminal: &str,
    choice: Choice,
    reason: String,
    seen: Value,
    sent: &[Vec<String>],
) -> ToolError {
    let message = format!(
        "answer_dialog ({}) refused: {reason}. Enter was not sent; keys sent: {sent:?}.\nSeen: {seen}",
        choice.as_str()
    );
    ToolError::new("refused", message)
        .with("terminal", terminal)
        .with("choice", choice.as_str())
        .with("reason", reason)
        .with("seen", seen)
        .with("keys_sent", json!(sent))
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(SEEN_TAIL_LINES);
    lines[start..].join("\n")
}

fn required_str<'a>(args: &'a Args, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        Some(Value::String(text)) => Ok(text.as_str()),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("{key} is required")),
    }
}

fn optional_str<'a>(args: &'a Args, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.as_str())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// The pane name: required and cleaned with the input rules. `resolve` then
/// checks a pane carries it.
fn terminal_arg(args: &Args) -> Result<String, String> {
    let raw = required_str(args, "terminal")?;
    validate_input(raw).map_err(|err| format!("terminal: {err}"))
}

fn optional_number(args: &Args, key: &str, min: f64, max: f64) -> Result<Option<f64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_f64()
                .ok_or_else(|| format!("{key} must be a number"))?;
            if !(min..=max).contains(&number) {
                return Err(format!("{key} must be between {min} and {max}"));
            }
            Ok(Some(number))
        }
    }
}

#[cfg(test)]
mod every_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ProgramStatus, Secret, WaitResult};
    use crate::testutil::{FakeBackend, TempDir};

    fn args(value: Value) -> Args {
        value.as_object().unwrap().clone()
    }

    /// A Shepherd over `fake`, with the fake able to see the pane locks.
    fn shepherd(fake: &FakeBackend) -> Shepherd {
        let sh = Shepherd::with_policy(fake.clone(), LaunchPolicy::arbitrary());
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        sh
    }

    #[test]
    fn send_input_without_conditional_input_checks_status_under_the_lock() {
        let fake = FakeBackend::new(false);
        fake.add_pane("stuck", State::Blocked, Some("permission"));
        fake.add_pane("ready", State::Done, None);
        let sh = shepherd(&fake);

        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "stuck", "text": "yes"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "pane_blocked");
        assert_eq!(err.data["kind"], "permission");
        assert_eq!(err.data["terminal"], "stuck");
        // get, then get inside the lock; the status check refused, so no send.
        assert_eq!(fake.calls(), ["get", "get"]);
        assert!(fake.state.lock().unwrap().sent.is_empty());

        let sent = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "ready", "text": "next"})),
        )
        .unwrap();
        assert_eq!(sent["status"], "done");
        assert_eq!(fake.calls(), ["get", "get", "get", "get", "send"]);
        // The send ran while Shepherd held the pane lock for the pane's id.
        assert_eq!(
            fake.state.lock().unwrap().sent,
            [("next".to_string(), true)]
        );
        assert!(!sh.pane_locks().is_held("id-ready"));
    }

    #[test]
    fn auth_blocked_pane_says_not_logged_in_and_does_not_point_at_answer_dialog() {
        let fake = FakeBackend::new(false);
        fake.add_pane("stuck", State::Blocked, Some("auth"));
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "stuck", "text": "yes"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "pane_blocked");
        assert_eq!(err.data["kind"], "auth");
        let message = err.text.clone();
        assert!(message.contains("not logged in"), "{message}");
        assert!(message.contains("operator"), "{message}");
        assert!(!message.contains("answer_dialog"), "{message}");
        let next = err.data["next"].as_str().unwrap();
        assert!(!next.contains("answer_dialog"), "{next}");
        assert!(fake.state.lock().unwrap().sent.is_empty());

        let err = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "stuck", "choice": "allow_once"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "refused");
        let reason = err.data["reason"].as_str().unwrap();
        assert!(reason.contains("not logged in"), "{reason}");
        assert!(reason.contains("operator"), "{reason}");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn permission_blocked_message_and_next_are_unchanged() {
        let fake = FakeBackend::new(false);
        fake.add_pane("stuck", State::Blocked, Some("permission"));
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "stuck", "text": "yes"})),
        )
        .unwrap_err();
        assert_eq!(err.data["next"], BLOCKED_NEXT);
        let text = err.text.clone();
        assert!(
            text.contains("stuck is blocked on a permission dialog; nothing was typed. Read the screen with read_output, then answer with answer_dialog"),
            "{text}"
        );
    }

    #[test]
    fn send_input_refuses_yolo_on_a_devin_pane_and_sends_nothing() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Done, None);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Devin);
        for text in ["/yolo", "  /YOLO "] {
            let err = call(
                &sh,
                "send_input",
                &args(json!({"terminal": "work", "text": text})),
            )
            .unwrap_err();
            assert_eq!(err.data["error"], "refused");
            assert!(err.data["message"]
                .as_str()
                .unwrap()
                .contains("mode-switch"));
        }
        assert!(fake.state.lock().unwrap().sent.is_empty());
        call(
            &sh,
            "send_input",
            &args(json!({"terminal": "work", "text": "fix the tests"})),
        )
        .unwrap();
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
    }

    #[test]
    fn send_input_yolo_on_a_non_devin_pane_is_unaffected() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Done, None);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Kiro);
        call(
            &sh,
            "send_input",
            &args(json!({"terminal": "work", "text": "/yolo"})),
        )
        .unwrap();
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
    }

    const KIRO_TRUST_WARNING_SCREEN: &str =
        include_str!("../tests/fixtures/kiro-3.3.0/trust-all-warning-synthetic.screen.txt");

    /// A Kiro pane named `work` at `turn`, showing `screen`.
    fn kiro_pane(turn: Option<u64>, screen: Option<&str>) -> (Shepherd, FakeBackend) {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Idle, None);
        {
            let mut state = fake.state.lock().unwrap();
            state.panes[0].turn = turn;
            state.screen = screen.map(str::to_string);
        }
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Kiro);
        (sh, fake)
    }

    fn send_hello(sh: &Shepherd) -> Result<Value, ToolError> {
        call(
            sh,
            "send_input",
            &args(json!({"terminal": "work", "text": "hello"})),
        )
    }

    #[test]
    fn send_input_refuses_a_kiro_pane_showing_the_trust_all_warning_at_turn_zero() {
        for turn in [None, Some(0)] {
            let (sh, fake) = kiro_pane(turn, Some(KIRO_TRUST_WARNING_SCREEN));
            let err = send_hello(&sh).unwrap_err();
            assert_eq!(err.data["error"], "pane_needs_trust_acceptance", "{turn:?}");
            assert_eq!(err.data["terminal"], "work");
            assert_eq!(err.data["next"], TRUST_ACCEPTANCE_NEXT);
            assert!(fake.state.lock().unwrap().sent.is_empty(), "{turn:?}");
            assert!(fake.state.lock().unwrap().keys.is_empty(), "{turn:?}");
            // The pane is left open for a person to accept the warning.
            assert!(fake.state.lock().unwrap().panes[0].live);
        }
    }

    #[test]
    fn the_kiro_trust_guard_sees_through_escapes_in_the_phrase() {
        let screen = "\x1b[1mKiro is running in trust all\x1b[0m tools mode";
        let (sh, fake) = kiro_pane(Some(0), Some(screen));
        let err = send_hello(&sh).unwrap_err();
        assert_eq!(err.data["error"], "pane_needs_trust_acceptance");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn the_kiro_trust_guard_is_off_after_the_first_turn() {
        let (sh, fake) = kiro_pane(Some(1), Some(KIRO_TRUST_WARNING_SCREEN));
        send_hello(&sh).unwrap();
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
        // No per-send read once a turn has happened.
        assert!(!fake.calls().contains(&"read"));
    }

    #[test]
    fn the_kiro_trust_guard_sends_on_a_blank_or_other_screen() {
        for screen in ["", "   \n\n", "kiro-cli 3.3.0\n> "] {
            let (sh, fake) = kiro_pane(Some(0), Some(screen));
            send_hello(&sh).unwrap();
            assert_eq!(fake.state.lock().unwrap().sent.len(), 1, "{screen:?}");
        }
    }

    #[test]
    fn the_kiro_trust_guard_sends_when_the_read_is_unsupported_or_the_backend_has_no_screen() {
        // `read` unsupported: no screen set.
        let (sh, fake) = kiro_pane(Some(0), None);
        send_hello(&sh).unwrap();
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
        // The backend says it has no screen text: no read is even tried.
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Idle, None);
        {
            let mut state = fake.state.lock().unwrap();
            state.panes[0].turn = Some(0);
            state.screen = Some(KIRO_TRUST_WARNING_SCREEN.to_string());
        }
        let mut quiet = fake.clone();
        quiet.caps.screen_text = false;
        let sh = Shepherd::with_policy(quiet, LaunchPolicy::arbitrary());
        sh.assume_agent("work", Agent::Kiro);
        send_hello(&sh).unwrap();
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
        assert!(!fake.calls().contains(&"read"));
    }

    #[test]
    fn the_kiro_trust_guard_ignores_other_agents_and_unknown_agents() {
        for agent in [Some(Agent::Devin), Some(Agent::OpenCode), None] {
            let (sh, fake) = kiro_pane(Some(0), Some(KIRO_TRUST_WARNING_SCREEN));
            sh.forget_agent("work");
            if let Some(agent) = agent {
                sh.assume_agent("work", agent);
            }
            send_hello(&sh).unwrap();
            assert_eq!(fake.state.lock().unwrap().sent.len(), 1, "{agent:?}");
        }
    }

    #[test]
    fn answer_dialog_on_a_pane_at_the_trust_warning_is_the_not_blocked_refusal() {
        let (sh, fake) = kiro_pane(Some(0), Some(KIRO_TRUST_WARNING_SCREEN));
        let err = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "allow_once"})),
        )
        .unwrap_err();
        assert!(
            err.text.contains("not blocked on a permission dialog"),
            "{}",
            err.text
        );
        assert!(fake.state.lock().unwrap().sent.is_empty());
        assert!(fake.state.lock().unwrap().keys.is_empty());
    }

    #[test]
    fn a_routine_send_hits_the_same_kiro_trust_refusal() {
        // The routine sends through `send_text_guarded`, as `send_input` does.
        let (sh, fake) = kiro_pane(Some(0), Some(KIRO_TRUST_WARNING_SCREEN));
        let err = send_text_guarded(&sh, "work", "continue", None, None).unwrap_err();
        assert_eq!(err.data["error"], "pane_needs_trust_acceptance");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn send_input_with_conditional_input_leaves_the_refusal_to_the_backend() {
        let fake = FakeBackend::new(true);
        fake.add_pane("stuck", State::Blocked, Some("permission"));
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "stuck", "text": "yes"})),
        )
        .unwrap_err();
        assert_eq!(fake.calls(), ["get", "get", "send"]);
        assert_eq!(err.data["error"], "pane_blocked");
        assert_eq!(err.data["kind"], "permission");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn send_input_checks_the_status_read_inside_the_lock_not_before_it() {
        let fake = FakeBackend::new(false);
        fake.add_pane("ready", State::Done, None);
        // Done at the lookup before the lock, blocked at the get inside it.
        fake.state.lock().unwrap().block_on_get = Some(2);
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "ready", "text": "next"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "pane_blocked");
        assert_eq!(err.data["kind"], "permission");
        assert_eq!(fake.calls(), ["get", "get"]);
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn panes_are_reached_only_by_their_name() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Done, None);
        fake.add_pane("anon", State::Done, None);
        fake.state.lock().unwrap().panes[1].name = None;
        fake.state.lock().unwrap().screen = Some("screen".to_string());
        let sh = shepherd(&fake);
        // The fake's get also accepts ids, as a backend may accept its own ids.
        for terminal in ["id-work", "id-anon"] {
            for (tool, extra) in [
                ("send_input", json!({"text": "hi"})),
                ("read_output", json!({})),
                ("get_status", json!({})),
                ("wait_status", json!({"timeout": 1})),
                ("interrupt_pane", json!({})),
                ("answer_dialog", json!({"choice": "allow_once"})),
                ("remove_pane", json!({})),
            ] {
                let mut a = args(extra);
                a.insert("terminal".to_string(), json!(terminal));
                let err = call(&sh, tool, &a).unwrap_err();
                assert_eq!(err.data["error"], "not_found", "{tool} {terminal}");
                assert!(err.text.contains("no pane is named"), "{}", err.text);
            }
        }
        // Only the lookups ran; nothing reached either pane.
        assert!(fake.calls().iter().all(|call| *call == "get"));
        assert_eq!(fake.calls().len(), 14);
        assert!(fake.state.lock().unwrap().sent.is_empty());

        // The name works, and the result names the pane by it.
        let out = call(&sh, "read_output", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out["text"], "screen");
        let out = call(&sh, "get_status", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out["terminal"], "work");
        assert_eq!(out["name"], "work");
    }

    #[test]
    fn remove_pane_checks_its_arguments_before_any_call() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        let sh = shepherd(&fake);
        for bad in [json!(-1), json!(60.5), json!("2")] {
            let err = call(
                &sh,
                "remove_pane",
                &args(json!({"terminal": "work", "timeout": bad})),
            )
            .unwrap_err();
            assert_eq!(err.data["error"], "invalid_input", "{bad}");
        }
        let err = call(&sh, "remove_pane", &args(json!({}))).unwrap_err();
        assert_eq!(err.data["error"], "invalid_input");
        assert!(fake.calls().is_empty());

        // An unknown name: only the lookup runs.
        let err = call(&sh, "remove_pane", &args(json!({"terminal": "nope"}))).unwrap_err();
        assert_eq!(err.data["error"], "not_found");
        assert_eq!(err.data["terminal"], "nope");
        assert_eq!(fake.calls(), ["get"]);
    }

    /// Another call holds the pane lock; the pane's program ends meanwhile.
    /// remove_pane waits for the lock, then reports the state it finds.
    #[test]
    fn remove_pane_waits_for_the_lock_and_acts_on_the_state_then() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Working, None);
        let sh = shepherd(&fake);
        let held = sh.pane_locks().lock("id-work");
        let remover = {
            let sh = sh.clone();
            thread::spawn(move || call(&sh, "remove_pane", &args(json!({"terminal": "work"}))))
        };
        while sh.pane_locks().waiting("id-work") == 0 {
            thread::yield_now();
        }
        assert_eq!(fake.calls(), ["get"]);
        {
            let mut state = fake.state.lock().unwrap();
            state.panes[0].live = false;
            state.panes[0].status = None;
            state.panes[0].exit_code = Some(0);
        }
        drop(held);
        let out = remover.join().unwrap().unwrap();
        assert_eq!(
            out,
            json!({"name": "work", "was_live": false, "stopped": null, "exit_code": 0})
        );
        assert_eq!(fake.calls(), ["get", "get", "remove"]);
        assert!(!sh.pane_locks().is_held("id-work"));
        let err = call(&sh, "get_status", &args(json!({"terminal": "work"}))).unwrap_err();
        assert_eq!(err.data["error"], "not_found");
    }

    /// A send_input that arrives while remove_pane holds the pane lock waits
    /// for it, then finds the pane gone: nothing is typed and nothing hangs.
    #[test]
    fn send_input_waiting_behind_remove_pane_gets_not_found() {
        use std::sync::mpsc;
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        let sh = shepherd(&fake);
        let (started_tx, started) = mpsc::channel();
        let (proceed, proceed_rx) = mpsc::channel();
        fake.state.lock().unwrap().remove_gate = Some((started_tx, proceed_rx));
        let remover = {
            let sh = sh.clone();
            thread::spawn(move || call(&sh, "remove_pane", &args(json!({"terminal": "work"}))))
        };
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("remove never started");
        assert!(sh.pane_locks().is_held("id-work"));
        let sender = {
            let sh = sh.clone();
            thread::spawn(move || {
                call(
                    &sh,
                    "send_input",
                    &args(json!({"terminal": "work", "text": "hi"})),
                )
            })
        };
        while sh.pane_locks().waiting("id-work") == 0 {
            thread::yield_now();
        }
        proceed.send(()).unwrap();
        let out = remover.join().unwrap().unwrap();
        assert_eq!(
            out,
            json!({"name": "work", "was_live": true, "stopped": "exited", "exit_code": null})
        );
        let err = sender.join().unwrap().unwrap_err();
        assert_eq!(err.data["error"], "not_found");
        assert_eq!(err.data["terminal"], "work");
        // The send's first lookup ran while the remove was in progress; its
        // lookup inside the lock found the pane gone, so no send call.
        assert_eq!(fake.calls(), ["get", "get", "remove", "get", "get"]);
        assert!(fake.state.lock().unwrap().sent.is_empty());
        assert!(!sh.pane_locks().is_held("id-work"));
        assert_eq!(sh.pane_locks().waiting("id-work"), 0);

        // Later calls on the removed name get not_found too.
        fake.state.lock().unwrap().screen = Some("screen".to_string());
        for (tool, extra) in [
            ("answer_dialog", json!({"choice": "allow_once"})),
            ("remove_pane", json!({})),
        ] {
            let mut a = args(extra);
            a.insert("terminal".to_string(), json!("work"));
            assert_eq!(call(&sh, tool, &a).unwrap_err().data["error"], "not_found");
        }
    }

    #[test]
    fn read_output_refuses_history_on_a_backend_without_it() {
        let mut fake = FakeBackend::new(false);
        fake.caps.history = false;
        fake.add_pane("work", State::Done, None);
        fake.state.lock().unwrap().screen = Some("screen".to_string());
        let sh = shepherd(&fake);

        let err = call(
            &sh,
            "read_output",
            &args(json!({"terminal": "work", "lines": 5})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "unsupported");
        assert!(!fake.calls().contains(&"read"));

        let out = call(
            &sh,
            "read_output",
            &args(json!({"terminal": "work", "lines": 0})),
        )
        .unwrap();
        assert_eq!(out["text"], "screen");
        assert!(fake.calls().contains(&"read"));
    }

    #[test]
    fn start_pane_checks_env_before_any_call() {
        let mut fake = FakeBackend::new(false);
        fake.caps.client_env = false;
        let sh = shepherd(&fake);
        let refused = |env: Value| {
            call(
                &sh,
                "start_pane",
                &args(json!({"argv": ["true"], "env": env})),
            )
            .unwrap_err()
        };
        assert_eq!(refused(json!({"A": "b"})).data["error"], "unsupported");
        for bad in [
            json!(["A=b"]),
            json!({"lower": "x"}),
            json!({"1A": "x"}),
            json!({"A-B": "x"}),
            json!({"A": 1}),
            json!({"A": "x\u{1b}y"}),
        ] {
            let err = refused(bad.clone());
            assert_ne!(err.data["error"], "unsupported", "{bad}");
        }
        let many: serde_json::Map<String, Value> = (0..=MAX_RUN_ENV)
            .map(|i| (format!("K{i}"), json!("v")))
            .collect();
        let err = refused(Value::Object(many));
        assert!(err.text.contains("at most 32"), "{}", err.text);
        assert!(fake.calls().is_empty());
        let mut env = env_arg(&args(json!({"env": {"B": " two ", "A": ""}}))).unwrap();
        env.sort();
        assert_eq!(
            env,
            [
                ("A".to_string(), String::new()),
                ("B".to_string(), "two".to_string())
            ]
        );
    }

    #[test]
    fn answer_dialog_leaves_the_pre_enter_check_to_a_backend_with_conditional_keys() {
        let mut fake = FakeBackend::new(false);
        fake.caps.conditional_keys = true;
        fake.add_pane("work", State::Blocked, Some("permission"));
        fake.state.lock().unwrap().screen = Some(crate::dialog::fixtures::dialog_on_allow());
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Kiro);
        let out = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "allow_once"})),
        )
        .unwrap();
        assert_eq!(out["selected"], "Allow once");
        // No get between the confirming read and Enter: the guard goes with it.
        assert_eq!(fake.calls(), ["get", "get", "read", "read", "keys"]);
        assert_eq!(fake.state.lock().unwrap().keys, [vec![Key::Enter]]);
    }

    /// The Devin dialog with the highlight on `cursor` (0 Allow, 1 Bypass mode, 2 Deny).
    fn devin_screen(cursor: usize) -> String {
        crate::devin_dialog::fixtures::dialog_with_cursor_on(cursor)
    }

    /// The keys each `keys` call sent, by name.
    fn key_names(fake: &FakeBackend) -> Vec<Vec<&'static str>> {
        fake.state
            .lock()
            .unwrap()
            .keys
            .iter()
            .map(|keys| keys.iter().map(Key::as_str).collect())
            .collect()
    }

    /// answer_dialog on pane `work`, which is a Devin pane.
    fn devin_answer(fake: &FakeBackend, choice: &str) -> Result<Value, ToolError> {
        let sh = shepherd(fake);
        sh.assume_agent("work", Agent::Devin);
        call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": choice})),
        )
    }

    fn blocked_devin_pane(screens: Vec<String>) -> FakeBackend {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Blocked, Some("permission"));
        let mut state = fake.state.lock().unwrap();
        let mut screens = screens.into_iter();
        state.screen = screens.next();
        state.next_screens = screens.collect();
        drop(state);
        fake
    }

    #[test]
    fn devin_allow_once_presses_enter_only_when_allow_is_highlighted() {
        let fake = blocked_devin_pane(vec![devin_screen(0)]);
        let out = devin_answer(&fake, "allow_once").unwrap();
        assert_eq!(out["agent"], "devin");
        assert_eq!(out["selected"], "Allow");
        assert_eq!(out["moves"], 0);
        assert_eq!(key_names(&fake), [["enter"]]);
    }

    #[test]
    fn devin_deny_once_moves_down_one_key_at_a_time_then_enter() {
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(1), devin_screen(2)]);
        let out = devin_answer(&fake, "deny_once").unwrap();
        assert_eq!(out["selected"], "Deny");
        assert_eq!(out["moves"], 2);
        assert_eq!(key_names(&fake), [["down"], ["down"], ["enter"]]);
    }

    #[test]
    fn devin_refuses_when_the_highlight_lands_on_bypass_mode_and_never_enters() {
        // The second Down leaves the highlight on Bypass mode.
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(1), devin_screen(1)]);
        let err = devin_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("not on"), "{}", err.text);
        assert_eq!(key_names(&fake), [["down"], ["down"], ["esc"]]);
    }

    #[test]
    fn devin_refuses_when_a_move_does_not_land_on_the_target() {
        // The Down key is ignored: the highlight stays on Allow.
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(0)]);
        let err = devin_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert_eq!(key_names(&fake), [["down"], ["esc"]]);
    }

    fn read_count(fake: &FakeBackend) -> usize {
        fake.calls().iter().filter(|call| **call == "read").count()
    }

    /// Deny-once on a Devin pane whose every Down is first read `stale` times
    /// on the old row. Returns the answer and the number of screen reads.
    fn devin_deny_with_stale(stale: usize) -> (Result<Value, ToolError>, usize, FakeBackend) {
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(1), devin_screen(2)]);
        fake.state.lock().unwrap().stale_reads = stale;
        let out = devin_answer(&fake, "deny_once");
        let reads = read_count(&fake);
        (out, reads, fake)
    }

    #[test]
    fn devin_deny_once_waits_for_a_highlight_that_updates_on_the_second_read() {
        // Each Down is first read on the old row (Devin redraws ~60 ms later).
        let (_, base_reads, _) = devin_deny_with_stale(0);
        let (out, reads, fake) = devin_deny_with_stale(1);
        let out = out.unwrap();
        assert_eq!(out["selected"], "Deny");
        assert_eq!(out["moves"], 2);
        assert_eq!(key_names(&fake), [["down"], ["down"], ["enter"]]);
        // One extra read per Down proves the stale screen was really served.
        assert_eq!(reads, base_reads + 2);
    }

    #[test]
    fn devin_deny_once_accepts_a_highlight_that_lands_on_the_fifth_read() {
        let (out, _, fake) = devin_deny_with_stale(CONFIRM_READS - 1);
        let out = out.unwrap();
        assert_eq!(out["selected"], "Deny");
        assert_eq!(key_names(&fake), [["down"], ["down"], ["enter"]]);
    }

    #[test]
    fn devin_deny_once_refuses_when_the_highlight_lands_on_the_sixth_read() {
        let (out, _, fake) = devin_deny_with_stale(CONFIRM_READS);
        let err = out.unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("after 5 reads"), "{}", err.text);
        assert_eq!(key_names(&fake), [["down"], ["esc"]]);
    }

    #[test]
    fn devin_refuses_after_bounded_rereads_when_the_highlight_never_moves() {
        let fake = blocked_devin_pane(vec![devin_screen(0)]);
        let err = devin_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("after 5 reads"), "{}", err.text);
        assert_eq!(key_names(&fake), [["down"], ["esc"]]);
    }

    #[test]
    fn devin_refuses_at_once_when_the_highlight_lands_on_the_wrong_row() {
        // Allow to Deny in one Down skips the verified Bypass row.
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(2)]);
        let err = devin_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert_eq!(key_names(&fake), [["down"], ["esc"]]);
    }

    #[test]
    fn devin_refuses_when_bypass_is_still_highlighted_at_the_end() {
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(1), devin_screen(1)]);
        let err = devin_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert_eq!(key_names(&fake), [["down"], ["down"], ["esc"]]);
    }

    #[test]
    fn devin_never_sends_enter_after_a_failed_check() {
        let cases: Vec<Vec<String>> = vec![
            // Cursor stops short of the target.
            vec![devin_screen(0), devin_screen(0)],
            // The dialog stops parsing after a move.
            vec![devin_screen(0), "no dialog here\n".to_string()],
        ];
        for screens in cases {
            let fake = blocked_devin_pane(screens);
            let err = devin_answer(&fake, "deny_once").unwrap_err();
            assert_eq!(err.data["error"], "refused");
            assert!(
                key_names(&fake).iter().flatten().all(|key| *key != "enter"),
                "{:?}",
                key_names(&fake)
            );
        }
    }

    #[test]
    fn devin_refuses_without_keys_when_the_pane_has_no_recorded_agent() {
        let fake = blocked_devin_pane(vec![devin_screen(0)]);
        let sh = shepherd(&fake);
        let err = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "allow_once"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("no dialect applies"), "{}", err.text);
        assert!(key_names(&fake).is_empty());
    }

    /// answer_dialog on pane `work`, which is an OpenCode pane.
    fn opencode_answer(fake: &FakeBackend, choice: &str) -> Result<Value, ToolError> {
        let sh = shepherd(fake);
        sh.assume_agent("work", Agent::OpenCode);
        call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": choice})),
        )
    }

    fn opencode_shell() -> String {
        crate::opencode_dialog::fixtures::shell_dialog()
    }

    #[test]
    fn opencode_deny_once_sends_only_esc_after_the_dialog_checks() {
        for screen in [
            opencode_shell(),
            crate::opencode_dialog::fixtures::edit_dialog(),
        ] {
            let fake = blocked_devin_pane(vec![screen]);
            let out = opencode_answer(&fake, "deny_once").unwrap();
            assert_eq!(out["agent"], "opencode");
            assert_eq!(out["selected"], "Reject");
            assert_eq!(out["keys"], json!(["esc"]));
            assert_eq!(key_names(&fake), [["esc"]]);
            assert_eq!(fake.calls(), ["get", "get", "read", "get", "keys"]);
        }
    }

    /// An OpenCode pane whose text screen is the shell dialog and whose
    /// styled screen is `styled`.
    fn opencode_styled_pane(styled: Option<StyledScreen>) -> FakeBackend {
        let fake = blocked_devin_pane(vec![opencode_shell()]);
        fake.state.lock().unwrap().styled = styled;
        fake
    }

    fn styled_selected(label: &str) -> StyledScreen {
        crate::opencode_dialog::fixtures::styled_with_selected(&opencode_shell(), label)
    }

    #[test]
    fn opencode_allow_once_presses_enter_when_the_styled_cells_show_allow_once_selected() {
        for text in [
            opencode_shell(),
            crate::opencode_dialog::fixtures::edit_dialog(),
        ] {
            let fake = blocked_devin_pane(vec![text.clone()]);
            fake.state.lock().unwrap().styled = Some(
                crate::opencode_dialog::fixtures::styled_with_selected(&text, "Allow once"),
            );
            let out = opencode_answer(&fake, "allow_once").unwrap();
            assert_eq!(out["agent"], "opencode");
            assert_eq!(out["selected"], "Allow once");
            assert_eq!(out["keys"], json!(["enter"]));
            assert_eq!(key_names(&fake), [["enter"]]);
            assert_eq!(
                fake.calls(),
                ["get", "get", "read", "get", "keys_if_screen"]
            );
        }
    }

    #[test]
    fn opencode_allow_once_never_puts_colours_in_the_result() {
        let fake = opencode_styled_pane(Some(styled_selected("Allow once")));
        let out = opencode_answer(&fake, "allow_once").unwrap();
        let text = out.to_string().to_lowercase();
        assert!(
            !text.contains("fab283") && !text.contains("rgb") && !text.contains("idx"),
            "{text}"
        );
    }

    #[test]
    fn opencode_allow_once_refuses_without_a_key_unless_allow_once_is_the_only_highlight() {
        use crate::backend::Color;
        use crate::opencode_dialog::fixtures::{
            styled, styled_with_selected, SELECTED, UNSELECTED,
        };
        let shell = opencode_shell();
        let cases: Vec<(&str, StyledScreen, &str)> = vec![
            (
                "wrong option",
                styled_selected("Always allow"),
                "highlighted option",
            ),
            ("reject", styled_selected("Reject"), "highlighted option"),
            (
                "two",
                styled(&shell, |l| {
                    Some((if l == "Reject" { UNSELECTED } else { SELECTED }, false))
                }),
                "highlighted option",
            ),
            (
                "all the same",
                styled(&shell, |_| Some((UNSELECTED, false))),
                "same background",
            ),
            (
                "reverse only",
                styled(&shell, |l| Some((UNSELECTED, l == "Allow once"))),
                "reverse",
            ),
            (
                "default background",
                styled(&shell, |l| {
                    Some((
                        if l == "Allow once" {
                            (SELECTED.0, Color::Default)
                        } else {
                            UNSELECTED
                        },
                        false,
                    ))
                }),
                "default background",
            ),
            (
                "text changed between the read and the key",
                styled_with_selected(&shell.replace("marker", "marker2"), "Allow once"),
                "changed since it was read",
            ),
        ];
        for (name, styled, expect) in cases {
            let fake = opencode_styled_pane(Some(styled));
            let err = opencode_answer(&fake, "allow_once").unwrap_err();
            assert_eq!(err.data["error"], "refused", "{name}");
            assert!(err.text.contains(expect), "{name}: {}", err.text);
            assert!(key_names(&fake).is_empty(), "{name}");
        }
    }

    #[test]
    fn opencode_allow_once_refuses_when_colours_are_unavailable() {
        let fake = opencode_styled_pane(None);
        let err = opencode_answer(&fake, "allow_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(
            err.text.contains("cannot read cell colours"),
            "{}",
            err.text
        );
        assert!(key_names(&fake).is_empty());
    }

    #[test]
    fn opencode_sends_nothing_unless_the_pane_is_blocked_on_a_permission_dialog() {
        let cases = [
            (State::Working, None),
            (State::Idle, None),
            (State::Blocked, Some("auth")),
            (State::Blocked, Some("question")),
        ];
        for (state, kind) in cases {
            for choice in ["allow_once", "deny_once"] {
                let fake = FakeBackend::new(false);
                fake.add_pane("work", state.clone(), kind);
                fake.state.lock().unwrap().screen = Some(opencode_shell());
                let err = opencode_answer(&fake, choice).unwrap_err();
                assert_eq!(err.data["error"], "refused", "{state:?} {kind:?}");
                assert!(key_names(&fake).is_empty(), "{state:?} {kind:?}");
            }
        }
    }

    #[test]
    fn opencode_deny_once_sends_nothing_when_the_dialog_is_not_the_expected_one() {
        // The options are in the wrong order, so the dialog never parses.
        let wrong = opencode_shell().replace(
            "Allow once   Always allow   Reject",
            "Allow once   Reject   Always allow",
        );
        let fake = blocked_devin_pane(vec![wrong]);
        let err = opencode_answer(&fake, "deny_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("in that order"), "{}", err.text);
        assert!(key_names(&fake).is_empty());
    }

    #[test]
    fn opencode_sends_enter_only_for_a_verified_allow_once_and_never_right() {
        let fake = blocked_devin_pane(vec![opencode_shell()]);
        opencode_answer(&fake, "deny_once").unwrap();
        assert!(key_names(&fake)
            .iter()
            .flatten()
            .all(|key| !matches!(*key, "enter" | "right" | "left" | "tab")));
        // allow_once sends Enter and nothing else: never Right.
        let fake2 = opencode_styled_pane(Some(styled_selected("Allow once")));
        opencode_answer(&fake2, "allow_once").unwrap();
        assert_eq!(key_names(&fake2), [["enter"]]);
    }

    #[test]
    fn opencode_idle_after_a_denied_dialog_is_cancelled() {
        let fake = blocked_devin_pane(vec![opencode_shell()]);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::OpenCode);
        call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "deny_once"})),
        )
        .unwrap();
        fake.state
            .lock()
            .unwrap()
            .waits
            .push_back(wait_idle(Some(3)));
        let out = call(&sh, "wait_status", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out["outcome"], "cancelled");
    }

    fn wait_result(state: State, turn: Option<u64>) -> crate::backend::WaitResult {
        crate::backend::WaitResult {
            ended: true,
            reason: Some(crate::backend::WaitReason::Status),
            status: Some(crate::backend::ProgramStatus { state, kind: None }),
            exit_code: None,
            turn,
        }
    }

    fn wait_idle(turn: Option<u64>) -> crate::backend::WaitResult {
        wait_result(State::Idle, turn)
    }

    fn push_wait(fake: &FakeBackend, wait: crate::backend::WaitResult) {
        fake.state.lock().unwrap().waits.push_back(wait);
    }

    fn wait_work(sh: &Shepherd) -> Value {
        call(sh, "wait_status", &args(json!({"terminal": "work"}))).unwrap()
    }

    fn opencode_pane_with_allowable_dialog() -> (FakeBackend, Shepherd) {
        let fake = blocked_devin_pane(vec![opencode_shell()]);
        fake.state.lock().unwrap().styled = Some(styled_selected("Allow once"));
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::OpenCode);
        (fake, sh)
    }

    fn answer_work(sh: &Shepherd, choice: &str) {
        call(
            sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": choice})),
        )
        .unwrap();
    }

    #[test]
    fn opencode_idle_after_allow_once_is_done() {
        let (fake, sh) = opencode_pane_with_allowable_dialog();
        answer_work(&sh, "allow_once");
        push_wait(&fake, wait_result(State::Other, Some(1)));
        push_wait(&fake, wait_idle(Some(1)));
        let out = wait_work(&sh);
        assert_eq!(out["outcome"], "done");
        assert_eq!(out["turn_complete"], true);
    }

    #[test]
    fn opencode_allow_then_a_second_dialog_then_deny_is_cancelled() {
        let (fake, sh) = opencode_pane_with_allowable_dialog();
        answer_work(&sh, "allow_once");
        answer_work(&sh, "deny_once");
        push_wait(&fake, wait_idle(Some(1)));
        let out = wait_work(&sh);
        assert_eq!(out["outcome"], "cancelled");
        assert_eq!(out["turn_complete"], false);
    }

    #[test]
    fn opencode_allow_then_a_second_dialog_then_allow_is_done() {
        let (fake, sh) = opencode_pane_with_allowable_dialog();
        answer_work(&sh, "allow_once");
        answer_work(&sh, "allow_once");
        push_wait(&fake, wait_idle(Some(1)));
        let out = wait_work(&sh);
        assert_eq!(out["outcome"], "done");
        assert_eq!(out["turn_complete"], true);
    }

    #[test]
    fn opencode_deny_then_a_second_dialog_then_allow_is_done() {
        let (fake, sh) = opencode_pane_with_allowable_dialog();
        answer_work(&sh, "deny_once");
        answer_work(&sh, "allow_once");
        push_wait(&fake, wait_idle(Some(1)));
        assert_eq!(wait_work(&sh)["outcome"], "done");
    }

    #[test]
    fn an_opencode_deny_does_not_leak_into_the_next_turn() {
        let (fake, sh) = opencode_pane_with_allowable_dialog();
        answer_work(&sh, "deny_once");
        // The deny's wait never ran. The pane goes idle; a new turn starts.
        fake.state.lock().unwrap().panes[0].status = Some(crate::backend::ProgramStatus {
            state: State::Idle,
            kind: None,
        });
        // The denied dialog is gone from the screen.
        fake.state.lock().unwrap().screen = Some("> ready\n".to_string());
        let sent = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "work", "text": "next"})),
        )
        .unwrap();
        assert_eq!(sent["started"], true);
        push_wait(&fake, wait_result(State::Other, Some(2)));
        push_wait(&fake, wait_idle(Some(2)));
        assert_eq!(wait_work(&sh)["outcome"], "done");
    }

    fn wait_outcome_of(agent: Agent, waits: Vec<crate::backend::WaitResult>) -> Value {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Working, None);
        fake.state.lock().unwrap().waits = waits.into();
        let sh = shepherd(&fake);
        sh.assume_agent("work", agent);
        call(
            &sh,
            "wait_status",
            &args(json!({"terminal": "work", "timeout": 5})),
        )
        .unwrap()
    }

    #[test]
    fn an_opencode_turn_with_no_dialog_ends_done_at_idle_after_clear() {
        // working, then clear (the unknown state), then idle: no blocked at all.
        let out = wait_outcome_of(
            Agent::OpenCode,
            vec![wait_result(State::Other, Some(1)), wait_idle(Some(1))],
        );
        assert_eq!(out["outcome"], "done");
        assert_eq!(out["turn_complete"], true);
        assert_eq!(out["status"], "idle");
    }

    #[test]
    fn an_opencode_idle_before_any_turn_is_not_done() {
        let out = wait_outcome_of(Agent::OpenCode, vec![wait_idle(Some(0))]);
        assert_eq!(out["outcome"], "other");
        assert_eq!(out["turn_complete"], false);
    }

    #[test]
    fn idle_is_not_done_for_kiro_or_devin() {
        for agent in [Agent::Kiro, Agent::Devin] {
            let out = wait_outcome_of(agent, vec![wait_idle(Some(3))]);
            assert_eq!(out["outcome"], "other");
            // Only OpenCode waits out the unknown state.
            let out = wait_outcome_of(agent, vec![wait_result(State::Other, Some(3))]);
            assert_eq!(out["outcome"], "other");
            assert_eq!(out["status"], "other");
        }
    }

    #[test]
    fn devin_refuses_when_the_highlight_does_not_start_on_allow() {
        // Bypass mode or Deny is highlighted before any key. Only Esc is sent.
        for start in [1, 2] {
            for choice in ["allow_once", "deny_once"] {
                let fake = blocked_devin_pane(vec![devin_screen(start)]);
                let err = devin_answer(&fake, choice).unwrap_err();
                assert_eq!(err.data["error"], "refused", "{start} {choice}");
                assert!(err.text.contains("not on Allow"), "{}", err.text);
                assert_eq!(key_names(&fake), [["esc"]], "{start} {choice}");
            }
        }
    }

    #[test]
    fn devin_allow_once_refuses_a_scope_other_than_once() {
        let screen = devin_screen(0).replace("→ Once", "→ Session");
        let fake = blocked_devin_pane(vec![screen]);
        let err = devin_answer(&fake, "allow_once").unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(err.text.contains("Once scope"), "{}", err.text);
        assert_eq!(key_names(&fake), [["esc"]]);
    }

    #[test]
    fn devin_deny_once_does_not_look_at_the_allow_scope() {
        let session = |cursor: usize| devin_screen(cursor).replace("→ Once", "→ Session");
        let fake = blocked_devin_pane(vec![session(0), session(1), session(2)]);
        let out = devin_answer(&fake, "deny_once").unwrap();
        assert_eq!(out["selected"], "Deny");
        assert_eq!(key_names(&fake), [["down"], ["down"], ["enter"]]);
    }

    fn devin_screen_12_57(cursor: usize) -> String {
        crate::devin_dialog::fixtures_12_57::dialog_with_cursor_on(cursor)
    }

    #[test]
    fn devin_3000_12_57_allow_once_presses_enter_on_allow_with_scope_once() {
        let fake = blocked_devin_pane(vec![devin_screen_12_57(0)]);
        let out = devin_answer(&fake, "allow_once").unwrap();
        assert_eq!(out["selected"], "Allow");
        assert_eq!(out["moves"], 0);
        assert_eq!(key_names(&fake), [["enter"]]);
    }

    #[test]
    fn devin_3000_12_57_deny_once_moves_to_deny_then_enter() {
        let fake = blocked_devin_pane(vec![
            devin_screen_12_57(0),
            devin_screen_12_57(1),
            devin_screen_12_57(2),
        ]);
        let out = devin_answer(&fake, "deny_once").unwrap();
        assert_eq!(out["selected"], "Deny");
        assert_eq!(out["moves"], 2);
        assert_eq!(key_names(&fake), [["down"], ["down"], ["enter"]]);
    }

    #[test]
    fn devin_3000_12_57_refuses_a_wrong_scope_and_a_bypass_highlight() {
        let screen = devin_screen_12_57(0).replace("→ Once", "→ Session");
        let fake = blocked_devin_pane(vec![screen]);
        let err = devin_answer(&fake, "allow_once").unwrap_err();
        assert!(err.text.contains("Once scope"), "{}", err.text);
        assert_eq!(key_names(&fake), [["esc"]]);

        let fake = blocked_devin_pane(vec![devin_screen_12_57(1)]);
        let err = devin_answer(&fake, "allow_once").unwrap_err();
        assert!(err.text.contains("not on Allow"), "{}", err.text);
        assert_eq!(key_names(&fake), [["esc"]]);
    }

    #[test]
    fn a_devin_cancel_stays_cancelled_through_a_timeout_and_intermediate_results() {
        let fake = blocked_devin_pane(vec![devin_screen(0), devin_screen(1), devin_screen(2)]);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Devin);
        call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "deny_once"})),
        )
        .unwrap();
        let result = |ended, reason, state: Option<State>| WaitResult {
            ended,
            reason,
            status: state.map(|state| ProgramStatus { state, kind: None }),
            exit_code: None,
            turn: Some(2),
        };
        fake.state.lock().unwrap().waits.extend([
            result(false, Some(WaitReason::Timeout), None),
            result(true, Some(WaitReason::Status), Some(State::Other)),
            result(true, Some(WaitReason::Status), Some(State::Idle)),
            // The cancel was reported, so a later idle is not one this answer caused.
            result(true, Some(WaitReason::Status), Some(State::Idle)),
        ]);
        let outcomes: Vec<String> = (0..4)
            .map(|_| {
                let out = call(
                    &sh,
                    "wait_status",
                    &args(json!({"terminal": "work", "timeout": 1})),
                )
                .unwrap();
                out["outcome"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(outcomes, ["timeout", "other", "cancelled", "other"]);
    }

    #[test]
    fn a_pane_started_without_a_profile_forgets_the_agent_of_an_earlier_pane_with_its_name() {
        let fake = FakeBackend::new(true);
        fake.state.lock().unwrap().runnable = true;
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Devin);
        call(
            &sh,
            "start_pane",
            &args(json!({"argv": ["true"], "name": "work"})),
        )
        .unwrap();
        assert_eq!(sh.agent_of("work"), None);
    }

    /// A start with `argv` and, when given, a declared `agent`, as pane `work`.
    fn start_declared(sh: &Shepherd, agent: Option<&str>) -> Result<Value, ToolError> {
        let mut body = json!({"argv": ["kiro-cli", "chat"], "name": "work"});
        if let Some(agent) = agent {
            body["agent"] = json!(agent);
        }
        call(sh, "start_pane", &args(body))
    }

    /// Make pane `work` of the fake blocked on a Kiro permission dialog.
    fn block_work_on_a_dialog(fake: &FakeBackend) {
        let mut state = fake.state.lock().unwrap();
        state.panes[0].status = Some(ProgramStatus {
            state: State::Blocked,
            kind: Some("permission".to_string()),
        });
        state.screen = Some(crate::dialog::fixtures::dialog_on_allow());
    }

    #[test]
    fn a_declared_agent_gives_an_argv_pane_the_dialect_and_without_it_nothing_is_sent() {
        let answer = args(json!({"terminal": "work", "choice": "allow_once"}));
        let fake = FakeBackend::new(false);
        fake.state.lock().unwrap().runnable = true;
        let sh = shepherd(&fake);
        start_declared(&sh, Some("kiro")).unwrap();
        assert_eq!(sh.agent_of("work"), Some(Agent::Kiro));
        assert_eq!(sh.profile_of("work"), None);
        block_work_on_a_dialog(&fake);
        let out = call(&sh, "answer_dialog", &answer).unwrap();
        assert_eq!(out["selected"], "Allow once");
        assert_eq!(key_names(&fake), [["enter"]]);

        // The same start without `agent`: answer_dialog refuses and sends no key.
        let fake = FakeBackend::new(false);
        fake.state.lock().unwrap().runnable = true;
        let sh = shepherd(&fake);
        start_declared(&sh, None).unwrap();
        assert_eq!(sh.agent_of("work"), None);
        block_work_on_a_dialog(&fake);
        let err = call(&sh, "answer_dialog", &answer).unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(key_names(&fake).is_empty());
    }

    #[test]
    fn a_null_agent_is_the_same_as_none() {
        let fake = FakeBackend::new(false);
        fake.state.lock().unwrap().runnable = true;
        let sh = shepherd(&fake);
        call(
            &sh,
            "start_pane",
            &args(json!({"argv": ["true"], "name": "work", "agent": null})),
        )
        .unwrap();
        assert_eq!(sh.agent_of("work"), None);
    }

    #[test]
    fn agent_with_a_profile_or_an_unknown_value_is_invalid_input_and_runs_nothing() {
        let dir = crate::testutil::TempDir::new("tools-declared-agent-refuse");
        let fake = FakeBackend::new(true);
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), true));
        for (start, needle) in [
            (
                json!({"profile": "kiro", "agent": "kiro"}),
                "a profile fixes its agent",
            ),
            (
                json!({"argv": ["x"], "agent": "claude"}),
                "kiro, devin or opencode",
            ),
            (
                json!({"argv": ["x"], "agent": "Kiro"}),
                "kiro, devin or opencode",
            ),
            (json!({"line": "x", "agent": 7}), "kiro, devin or opencode"),
            (
                json!({"line": "x", "agent": ["kiro"]}),
                "kiro, devin or opencode",
            ),
        ] {
            let err = call(&sh, "start_pane", &args(start.clone())).unwrap_err();
            assert_eq!(err.data["error"], "invalid_input", "{start}");
            assert!(
                err.data["message"].as_str().unwrap().contains(needle),
                "{start}: {err:?}"
            );
        }
        // An agent with neither argv nor line is the existing missing-input error.
        let err = call(&sh, "start_pane", &args(json!({"agent": "kiro"}))).unwrap_err();
        assert_eq!(err.data["error"], "invalid_input");
        assert!(err.data["message"].as_str().unwrap().contains("argv"));
        assert!(fake.state.lock().unwrap().runs.is_empty());
        // With the server flag off, a declared agent does not open argv or line.
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), false));
        let err = call(
            &sh,
            "start_pane",
            &args(json!({"argv": ["kiro-cli"], "agent": "kiro"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "profile_required");
        assert!(fake.state.lock().unwrap().runs.is_empty());
    }

    #[test]
    fn a_declared_devin_pane_refuses_yolo() {
        let fake = FakeBackend::new(false);
        fake.state.lock().unwrap().runnable = true;
        let sh = shepherd(&fake);
        call(
            &sh,
            "start_pane",
            &args(json!({"line": "devin", "name": "work", "agent": "devin"})),
        )
        .unwrap();
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "work", "text": "/yolo"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn routine_create_on_a_declared_agent_pane_is_routines_not_allowed() {
        let fake = FakeBackend::new(false);
        fake.state.lock().unwrap().runnable = true;
        let sh = shepherd(&fake);
        start_declared(&sh, Some("kiro")).unwrap();
        fake.state.lock().unwrap().panes[0].status = Some(ProgramStatus {
            state: State::Done,
            kind: None,
        });
        let err = call(
            &sh,
            "routine_create",
            &args(json!({
                "terminal": "work",
                "trigger": {"type": "every", "every_secs": 60},
                "text": "check whether any todo work is left"
            })),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "routines_not_allowed");
    }

    #[test]
    fn a_name_reused_without_agent_forgets_the_declared_agent_and_remove_forgets_it() {
        let fake = FakeBackend::new(false);
        fake.state.lock().unwrap().runnable = true;
        let sh = shepherd(&fake);
        start_declared(&sh, Some("devin")).unwrap();
        assert_eq!(sh.agent_of("work"), Some(Agent::Devin));
        fake.state.lock().unwrap().panes.clear();
        start_declared(&sh, None).unwrap();
        assert_eq!(sh.agent_of("work"), None);
        fake.state.lock().unwrap().panes.clear();
        start_declared(&sh, Some("opencode")).unwrap();
        assert_eq!(sh.agent_of("work"), Some(Agent::OpenCode));
        call(&sh, "remove_pane", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(sh.agent_of("work"), None);
    }

    #[test]
    fn kiro_mismatch_refuses_without_an_escape() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Blocked, Some("permission"));
        fake.state.lock().unwrap().screen = Some(crate::dialog::fixtures::dialog_on_allow());
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Kiro);
        let err = call(
            &sh,
            "answer_dialog",
            &args(json!({"terminal": "work", "choice": "deny_once"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "refused");
        assert_eq!(key_names(&fake), [["down"]]);
    }

    #[test]
    fn remove_pane_forgets_the_pane_agent() {
        let fake = FakeBackend::new(false);
        fake.add_pane("work", State::Done, None);
        let sh = shepherd(&fake);
        sh.assume_agent("work", Agent::Devin);
        call(&sh, "remove_pane", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(sh.agent_of("work"), None);
    }

    /// A policy with a devin profile (`mode`) and a kiro profile, both
    /// programs in `<dir>/bin`, which the policy's PATH names.
    fn agent_policy(dir: &std::path::Path, mode: &str) -> LaunchPolicy {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for program in ["devin", "kiro-cli"] {
            let path = bin.join(program);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let bin = bin.to_string_lossy().into_owned();
        let text = json!({"profiles": [
            {"name": "devin", "argv": ["devin"], "cwds": [dir], "agent": "devin", "permission_mode": mode},
            {"name": "kiro", "argv": ["kiro-cli", "chat"], "extra_args": ["--trust-workspace"],
             "cwds": [dir]},
        ]});
        LaunchPolicy::from_json(&text.to_string())
            .unwrap()
            .with_env_lookup(move |key| (key == "PATH").then(|| bin.clone()))
    }

    /// The argv `start_pane` asked the backend to run (the fake refuses the run).
    fn started_argv(profile: &str, mode: &str) -> Vec<String> {
        let dir = TempDir::new("tools-agent-argv");
        let fake = FakeBackend::new(true);
        let sh = Shepherd::with_policy(fake.clone(), agent_policy(dir.path(), mode));
        let _ = call(
            &sh,
            "start_pane",
            &args(json!({"profile": profile, "name": "p"})),
        );
        let runs = fake.state.lock().unwrap().runs.clone();
        runs[0].argv.clone().unwrap()
    }

    #[test]
    fn devin_profile_passes_its_permission_flag_and_no_bypass_by_default() {
        let argv = started_argv("devin", "normal");
        assert_eq!(
            argv[1..].iter().map(String::as_str).collect::<Vec<_>>(),
            ["--permission-mode", "auto"]
        );
        assert!(!argv.iter().any(|a| a == "dangerous"), "{argv:?}");
        assert_eq!(
            started_argv("devin", "accept-edits")[1..3],
            ["--permission-mode", "accept-edits"]
        );
        assert_eq!(
            started_argv("devin", "bypass")[1..3],
            ["--permission-mode", "dangerous"]
        );
    }

    #[test]
    fn kiro_profile_argv_is_unchanged_by_the_permission_field() {
        let argv = started_argv("kiro", "normal");
        assert_eq!(
            argv[1..].iter().map(String::as_str).collect::<Vec<_>>(),
            ["chat", "--trust-workspace"]
        );
    }

    #[test]
    fn a_replayed_pane_reaches_callers_as_pane_replayed() {
        let failure = backend_failure(
            Some("work"),
            BackendError::new(ErrorKind::Replayed, "raw backend text --force"),
        );
        assert_eq!(failure.data["error"], "pane_replayed");
        assert_eq!(failure.data["terminal"], "work");
        assert_eq!(failure.data["backend_detail"], "raw backend text --force");
        assert!(failure.text.starts_with("pane_replayed: "));
        assert!(
            failure.text.contains("nothing was sent"),
            "{}",
            failure.text
        );
        assert!(
            !failure.text.contains("raw backend text"),
            "{}",
            failure.text
        );
    }

    #[test]
    fn backend_failures_use_shepherd_codes_and_keep_detail_aside() {
        for (kind, code) in [
            (ErrorKind::Unavailable, "unavailable"),
            (ErrorKind::Timeout, "timeout"),
            (ErrorKind::NotFound, "not_found"),
            (ErrorKind::NameTaken, "name_taken"),
            (ErrorKind::Blocked { kind: None }, "pane_blocked"),
            (ErrorKind::StatusMismatch, "status_mismatch"),
            (ErrorKind::ScreenRefused, "screen_refused"),
            (ErrorKind::Finished, "pane_finished"),
            (ErrorKind::Unsupported("x".to_string()), "unsupported"),
            (ErrorKind::InvalidInput, "invalid_input"),
            (ErrorKind::Protocol, "backend_error"),
            (ErrorKind::Other, "backend_error"),
        ] {
            let failure = backend_failure(
                Some("work"),
                BackendError::new(kind, "raw backend text --force"),
            );
            assert_eq!(failure.data["error"], code);
            assert_eq!(failure.data["backend_detail"], "raw backend text --force");
            assert!(failure.text.starts_with(&format!("{code}: ")));
            assert!(
                !failure.text.contains("raw backend text"),
                "{}",
                failure.text
            );
        }
    }

    #[test]
    fn generated_names_are_valid_and_distinct() {
        let names: Vec<String> = (0..50).map(|_| generated_name()).collect();
        for name in &names {
            assert_eq!(validate_name(name).as_deref(), Ok(name.as_str()));
            assert_eq!(name.len(), "pane-".len() + 12, "{name}");
        }
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
    }

    #[test]
    fn wait_outcome_only_completes_on_done_and_exit() {
        let status = Some(&WaitReason::Status);
        assert_eq!(
            wait_outcome(true, status, Some(&State::Done), false),
            "done"
        );
        assert_eq!(
            wait_outcome(true, status, Some(&State::Blocked), false),
            "blocked"
        );
        assert_eq!(
            wait_outcome(true, status, Some(&State::Idle), false),
            "other"
        );
        assert_eq!(
            wait_outcome(true, status, Some(&State::Other), false),
            "other"
        );
        assert_eq!(
            wait_outcome(true, Some(&WaitReason::Exit), None, false),
            "exited"
        );
        assert_eq!(
            wait_outcome(false, Some(&WaitReason::Timeout), None, false),
            "timeout"
        );
        assert_eq!(
            wait_outcome(false, Some(&WaitReason::Other), None, false),
            "other"
        );
        assert_eq!(wait_outcome(true, None, Some(&State::Done), false), "other");
    }

    #[test]
    fn idle_after_an_answered_dialog_is_cancelled_not_done() {
        let status = Some(&WaitReason::Status);
        assert_eq!(
            wait_outcome(true, status, Some(&State::Idle), true),
            "cancelled"
        );
        // Without an answer, idle stays what it was.
        assert_eq!(
            wait_outcome(true, status, Some(&State::Idle), false),
            "other"
        );
        // Done after an answer is still done.
        assert_eq!(wait_outcome(true, status, Some(&State::Done), true), "done");
    }

    /// An executable `kiro-cli` in `<dir>/bin`, which the policies' PATH names.
    fn fake_kiro(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join("kiro-cli");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::canonicalize(bin).unwrap()
    }

    fn kiro_policy(dir: &std::path::Path, arbitrary: bool) -> LaunchPolicy {
        let bin = fake_kiro(dir).to_string_lossy().into_owned();
        let text = json!({
            "allow_arbitrary_command": arbitrary,
            "profiles": [
                {"name": "kiro", "argv": ["kiro-cli", "chat"], "extra_args": ["--trust-workspace"],
                 "cwds": [dir], "env": {"KIRO_API_KEY": "env:KEY_SOURCE"}},
                {"name": "plain", "argv": ["/bin/sh"], "cwds": [dir]}
            ]
        });
        LaunchPolicy::from_json(&text.to_string())
            .unwrap()
            .with_env_lookup(move |key| match key {
                "KEY_SOURCE" => Some("profile-secret".to_string()),
                "PATH" => Some(bin.clone()),
                _ => None,
            })
    }

    #[test]
    fn start_pane_refuses_argv_and_line_by_default() {
        let fake = FakeBackend::new(true);
        let sh = Shepherd::new(fake.clone());
        for start in [
            json!({"argv": ["kiro-cli", "chat"]}),
            json!({"line": "echo hi"}),
            json!({}),
        ] {
            let err = call(&sh, "start_pane", &args(start.clone())).unwrap_err();
            assert_eq!(err.data["error"], "profile_required", "{start}");
        }
        assert!(fake.calls().is_empty());
        let dir = crate::testutil::TempDir::new("tools-profile-default");
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), false));
        let err = call(&sh, "start_pane", &args(json!({"argv": ["true"]}))).unwrap_err();
        assert_eq!(err.data["error"], "profile_required");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn start_pane_runs_a_profile_with_its_fixed_argv_and_env() {
        let dir = crate::testutil::TempDir::new("tools-profile-run");
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let fake = FakeBackend::new(true);
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), true));
        let sub = dir.path().join("sub");
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": "k", "cwd": sub})),
        )
        .unwrap_err();
        call(
            &sh,
            "start_pane",
            &args(json!({"argv": ["kiro-cli"], "name": "a"})),
        )
        .unwrap_err();
        let runs = fake.state.lock().unwrap().runs.clone();
        assert_eq!(runs.len(), 2);
        let canonical = std::fs::canonicalize(&sub).unwrap();
        let pinned = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("bin/kiro-cli");
        assert_eq!(
            runs[0].argv.as_deref(),
            Some(
                &[
                    pinned.to_string_lossy().into_owned(),
                    "chat".into(),
                    "--trust-workspace".into()
                ][..]
            )
        );
        assert_eq!(runs[0].cwd.as_deref(), canonical.to_str());
        assert_eq!(runs[0].profile.as_deref(), Some("kiro"));
        assert_eq!(
            runs[0].profile_env,
            [("KIRO_API_KEY".to_string(), Secret::new("profile-secret"))]
        );
        assert!(runs[0].env.is_empty());
        // An arbitrary command, allowed here, gets no profile environment.
        assert_eq!(runs[1].profile, None);
        assert!(runs[1].profile_env.is_empty());
    }

    #[test]
    fn start_pane_checks_a_profile_before_any_call() {
        let dir = crate::testutil::TempDir::new("tools-profile-refuse");
        let outside = crate::testutil::TempDir::new("tools-profile-outside");
        let fake = FakeBackend::new(true);
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), true));
        for (start, code) in [
            (json!({"profile": "nope"}), "invalid_input"),
            (json!({"profile": "kiro", "argv": ["x"]}), "invalid_input"),
            (json!({"profile": "kiro", "line": "x"}), "invalid_input"),
            (
                json!({"profile": "kiro", "env": {"A": "b"}}),
                "invalid_input",
            ),
            (
                json!({"profile": "kiro", "cwd": outside.path()}),
                "invalid_input",
            ),
            (
                json!({"profile": "kiro", "cwd": "relative"}),
                "invalid_input",
            ),
            (
                json!({"profile": "kiro", "cwd": dir.path().join("missing")}),
                "not_found",
            ),
            (
                json!({"profile": "kiro", "name": "Bad Name"}),
                "invalid_input",
            ),
        ] {
            let err = call(&sh, "start_pane", &args(start.clone())).unwrap_err();
            assert_eq!(err.data["error"], code, "{start}: {err:?}");
            assert!(!err.text.contains("profile-secret"), "{err:?}");
        }
        assert!(fake.calls().is_empty());

        // A source that cannot be read: refused, no run, no value.
        let bin = fake_kiro(dir.path()).to_string_lossy().into_owned();
        let unset = kiro_policy(dir.path(), false)
            .with_env_lookup(move |key| (key == "PATH").then(|| bin.clone()));
        let sh = Shepherd::with_policy(fake.clone(), unset);
        let err = call(&sh, "start_pane", &args(json!({"profile": "kiro"}))).unwrap_err();
        assert_eq!(err.data["error"], "profile_unavailable");
        assert!(err.text.contains("env:KEY_SOURCE"), "{err:?}");
        assert!(fake.calls().is_empty());

        // A backend without per-run environments refuses a credential
        // profile before its source is read, and runs one without env.
        let mut hc_like = FakeBackend::new(true);
        hc_like.caps.client_env = false;
        let sh = Shepherd::with_policy(hc_like.clone(), kiro_policy(dir.path(), false));
        let err = call(&sh, "start_pane", &args(json!({"profile": "kiro"}))).unwrap_err();
        assert_eq!(err.data["error"], "unsupported");
        assert!(hc_like.calls().is_empty());
        call(&sh, "start_pane", &args(json!({"profile": "plain"}))).unwrap_err();
        assert_eq!(hc_like.calls(), ["run"]);
    }

    #[test]
    fn a_profile_program_is_never_found_through_the_callers_cwd() {
        // Review SOL2-B1: PATH=.:<empty>:<bin>, an impostor kiro-cli in an
        // allowed subdirectory, and a profile secret.
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testutil::TempDir::new("tools-profile-impostor");
        let sub = dir.path().join("work");
        std::fs::create_dir_all(&sub).unwrap();
        let impostor = sub.join("kiro-cli");
        std::fs::write(&impostor, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&impostor, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bin = fake_kiro(dir.path()).to_string_lossy().into_owned();
        let path = format!(".::work:{bin}");
        let fake = FakeBackend::new(true);
        let policy = kiro_policy(dir.path(), false).with_env_lookup(move |key| match key {
            "KEY_SOURCE" => Some("profile-secret".to_string()),
            "PATH" => Some(path.clone()),
            _ => None,
        });
        let sh = Shepherd::with_policy(fake.clone(), policy);
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": "k", "cwd": sub})),
        )
        .unwrap_err();
        let runs = fake.state.lock().unwrap().runs.clone();
        assert_eq!(runs.len(), 1);
        let argv0 = &runs[0].argv.as_ref().unwrap()[0];
        assert_eq!(argv0, &format!("{bin}/kiro-cli"));
        assert!(std::path::Path::new(argv0).is_absolute());

        // With only relative or empty entries, nothing is found and no
        // credential is read.
        let fake = FakeBackend::new(true);
        let policy = kiro_policy(dir.path(), false).with_env_lookup(|key| match key {
            "KEY_SOURCE" => panic!("a credential was read"),
            "PATH" => Some(".::work".to_string()),
            _ => None,
        });
        let sh = Shepherd::with_policy(fake.clone(), policy);
        let err = call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "cwd": sub})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "not_found", "{err:?}");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn list_panes_shows_profiles_without_their_environment() {
        let dir = crate::testutil::TempDir::new("tools-profile-list");
        let policy = kiro_policy(dir.path(), false);
        let shown = policy.to_json();
        assert_eq!(shown["allow_arbitrary_command"], false);
        assert_eq!(shown["profiles"][0]["name"], "kiro");
        assert_eq!(
            shown["profiles"][0]["cwds"][0],
            std::fs::canonicalize(dir.path()).unwrap().to_str().unwrap()
        );
        let text = shown.to_string();
        assert!(
            !text.contains("KIRO_API_KEY") && !text.contains("KEY_SOURCE"),
            "{text}"
        );
        assert!(!text.contains("kiro-cli"), "{text}");
    }

    fn routine_policy(dir: &std::path::Path, routine: Value) -> LaunchPolicy {
        let bin = fake_kiro(dir).to_string_lossy().into_owned();
        let text = json!({
            "profiles": [
                {"name": "kiro", "argv": ["kiro-cli", "chat"], "cwds": [dir],
                 "routine": routine}
            ]
        });
        LaunchPolicy::from_json(&text.to_string())
            .unwrap()
            .with_env_lookup(move |key| (key == "PATH").then(|| bin.clone()))
    }

    /// A Shepherd over a runnable `fake`, with the fake able to see the pane
    /// locks, and the `kiro` routine profile of `routine_policy` started as
    /// pane `work`, finished at turn 3.
    fn watched_pane(
        routine: Value,
        tag: &str,
    ) -> (Shepherd, FakeBackend, crate::testutil::TempDir) {
        watched_pane_with_caps(routine, tag, true)
    }

    fn watched_pane_with_caps(
        routine: Value,
        tag: &str,
        conditional_input: bool,
    ) -> (Shepherd, FakeBackend, crate::testutil::TempDir) {
        let dir = crate::testutil::TempDir::new(tag);
        let fake = FakeBackend::new(conditional_input);
        fake.state.lock().unwrap().runnable = true;
        let sh = Shepherd::with_policy(fake.clone(), routine_policy(dir.path(), routine));
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": "work"})),
        )
        .unwrap();
        let mut state = fake.state.lock().unwrap();
        state.panes[0].status = Some(ProgramStatus {
            state: State::Done,
            kind: None,
        });
        state.panes[0].turn = Some(3);
        drop(state);
        (sh, fake, dir)
    }

    #[test]
    fn the_routine_nudges_a_stalled_profile_pane() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "cooldown_secs": 10,
                   "nudge_text": "check your todo and continue"}),
            "tools-routine-nudge",
        );
        let t0 = Instant::now();
        // The first pass only starts the idle clock.
        sh.routine_tick_at(t0);
        assert!(fake.state.lock().unwrap().sent.is_empty());
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(
            fake.state.lock().unwrap().sent,
            [("check your todo and continue".to_string(), true)]
        );
    }

    #[test]
    fn a_routine_tick_at_the_kiro_trust_warning_types_nothing() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "cooldown_secs": 10, "nudge_text": "continue"}),
            "tools-routine-trust-warning",
        );
        {
            let mut state = fake.state.lock().unwrap();
            state.panes[0].turn = Some(0);
            state.screen = Some(
                include_str!("../tests/fixtures/kiro-3.3.0/trust-all-warning-synthetic.screen.txt")
                    .to_string(),
            );
        }
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert!(fake.state.lock().unwrap().sent.is_empty());
        assert!(fake.calls().contains(&"read"));
    }

    #[test]
    fn the_routine_never_nudges_a_blocked_pane() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-blocked",
        );
        {
            let mut state = fake.state.lock().unwrap();
            state.panes[0].status = Some(ProgramStatus {
                state: State::Blocked,
                kind: Some("permission".to_string()),
            });
        }
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn a_guarded_nudge_is_refused_when_the_pane_changed_after_the_observation() {
        // The routine conditions its send on the finished state and turn
        // it observed. If the pane flipped to blocked, went back to
        // working, or advanced to a new turn before the send, nothing is
        // typed.
        for (state, turn) in [
            (State::Blocked, Some(3)),
            (State::Working, Some(3)),
            (State::Done, Some(4)),
        ] {
            // A backend without conditional input: Shepherd's own re-check
            // under the pane lock is what refuses these sends. (With a
            // conditional backend the same guard is enforced atomically by
            // the backend, as the `keys` guard tests show.)
            let (sh, fake, _dir) = watched_pane_with_caps(
                json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
                "tools-routine-flip",
                false,
            );
            {
                let mut fake_state = fake.state.lock().unwrap();
                fake_state.panes[0].status = Some(ProgramStatus {
                    state: state.clone(),
                    kind: (state == State::Blocked).then(|| "permission".to_string()),
                });
                fake_state.panes[0].turn = turn;
            }
            let guard = InputGuard {
                refuse_when_blocked: true,
                if_state: Some(State::Done),
                expect_turn: Some(3),
                ..InputGuard::default()
            };
            let err = send_text_guarded(&sh, "work", "continue", Some(guard), None).unwrap_err();
            assert!(
                ["pane_blocked", "status_mismatch"].contains(&err.data["error"].as_str().unwrap()),
                "{state:?} turn {turn:?}: {err:?}"
            );
            assert!(fake.state.lock().unwrap().sent.is_empty(), "{state:?}");
        }
    }

    /// Tick once to start the idle clock, then arrange for the pane to
    /// change at the read the nudge's send makes inside the pane lock (the
    /// third get of the next tick: the check's resolve, then the send's two),
    /// and tick again past the idle timeout. The backend is conditional, so
    /// only the guard the routine built can refuse the send.
    fn tick_with_pane_flipping_before_the_send(
        sh: &Shepherd,
        fake: &FakeBackend,
        state: State,
        turn: Option<u64>,
    ) {
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        {
            let mut fake_state = fake.state.lock().unwrap();
            let gets = fake_state.calls.iter().filter(|c| **c == "get").count();
            fake_state.flip_on_get = Some((gets + 3, state, turn));
        }
        sh.routine_tick_at(t0 + Duration::from_secs(11));
    }

    #[test]
    fn the_routine_does_not_nudge_a_pane_that_went_blocked_before_the_send() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-tick-blocked",
        );
        tick_with_pane_flipping_before_the_send(&sh, &fake, State::Blocked, None);
        assert!(fake.calls().contains(&"send"), "the send was never tried");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn the_routine_does_not_nudge_a_pane_that_went_back_to_working_before_the_send() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-tick-working",
        );
        tick_with_pane_flipping_before_the_send(&sh, &fake, State::Working, None);
        assert!(fake.calls().contains(&"send"), "the send was never tried");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn the_routine_does_not_nudge_a_pane_whose_turn_advanced_before_the_send() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-tick-turn",
        );
        tick_with_pane_flipping_before_the_send(&sh, &fake, State::Done, Some(4));
        assert!(fake.calls().contains(&"send"), "the send was never tried");
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn the_routine_does_not_nudge_a_working_pane() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-tick-idle-working",
        );
        fake.state.lock().unwrap().panes[0].status = Some(ProgramStatus {
            state: State::Working,
            kind: None,
        });
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert!(!fake.calls().contains(&"send"));
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn the_routine_nudges_when_the_pane_is_unchanged_at_the_send() {
        // Positive control for the flip tests: the same conditional backend
        // lets the send through when nothing changed.
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-tick-control",
        );
        tick_with_pane_flipping_before_the_send(&sh, &fake, State::Done, Some(3));
        assert_eq!(
            fake.state.lock().unwrap().sent,
            [("continue".to_string(), true)]
        );
    }

    #[test]
    fn removing_a_watched_pane_unregisters_it() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-remove",
        );
        call(&sh, "remove_pane", &args(json!({"terminal": "work"}))).unwrap();
        assert!(sh.routine_map().is_empty());
        let t0 = Instant::now();
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn a_watched_pane_whose_program_ended_is_forgotten() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-ended",
        );
        fake.state.lock().unwrap().panes[0].live = false;
        let t0 = Instant::now();
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert!(
            sh.routine_map().is_empty(),
            "the ended pane's watch was not forgotten"
        );
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn a_slow_nudge_send_does_not_cut_the_cooldown_short() {
        // The clock is consulted again when the send returns: a send that
        // takes far longer than the cooldown must still space the next
        // nudge from when it finished, not from when it was observed.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "cooldown_secs": 10,
                   "nudge_text": "continue"}),
            "tools-routine-slow-send",
        );
        let t0 = Instant::now();
        // First pass only starts the idle clock.
        sh.routine_tick_at(t0);
        assert!(fake.state.lock().unwrap().sent.is_empty());
        // Observation at t0+11 (nudge due); the send "returns" at t0+111.
        let calls = AtomicUsize::new(0);
        let slow_clock = || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                t0 + Duration::from_secs(11)
            } else {
                t0 + Duration::from_secs(111)
            }
        };
        sh.routine_tick_with(&slow_clock);
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
        // 5 s after the send actually finished (but 105 s after the
        // observation): still inside the cooldown from the real send.
        sh.routine_tick_at(t0 + Duration::from_secs(116));
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
        // Past the cooldown from the real send time: nudged again.
        sh.routine_tick_at(t0 + Duration::from_secs(122));
        assert_eq!(fake.state.lock().unwrap().sent.len(), 2);
    }

    #[test]
    fn removing_a_pane_does_not_drop_a_reused_names_watch() {
        // While `remove_pane` is still inside the backend remove, a new
        // start reuses the freed name and registers its own watch. The
        // removal's cleanup is keyed by the OLD pane's id, so the new
        // pane's watch must survive it.
        use std::sync::mpsc;
        let (sh, fake, dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-remove-race",
        );
        let (started_tx, started) = mpsc::channel();
        let (proceed_tx, proceed_rx) = mpsc::channel();
        fake.state.lock().unwrap().remove_gate = Some((started_tx, proceed_rx));
        let remover = {
            let sh = sh.clone();
            thread::spawn(move || call(&sh, "remove_pane", &args(json!({"terminal": "work"}))))
        };
        started
            .recv_timeout(Duration::from_secs(10))
            .expect("remove never started");
        // The new start's registration under the reused name.
        let config = sh.routine_map().get("work").unwrap().config().clone();
        sh.register_routine(
            "work",
            config,
            dir.path().to_path_buf(),
            Agent::Kiro,
            "id-work-2".to_string(),
        );
        // And its agent record: dropping it would disable the Devin
        // mode-switch refusal for the new pane.
        sh.remember_agent("work", Agent::Devin, Some("id-work-2".to_string()), None);
        proceed_tx.send(()).unwrap();
        remover.join().unwrap().unwrap();
        let map = sh.routine_map();
        assert_eq!(
            map.get("work").map(|watch| watch.pane_id()),
            Some("id-work-2"),
            "the removal dropped the new pane's watch"
        );
        assert_eq!(
            sh.agent_of("work"),
            Some(Agent::Devin),
            "the removal dropped the new pane's agent record"
        );
    }

    #[test]
    fn a_guarded_send_pinned_to_another_pane_id_is_refused() {
        // The routine passes the id of the pane it observed; if the name
        // now resolves to a different pane, nothing is typed into it.
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-routine-wrong-pane",
        );
        let err = send_text_guarded(&sh, "work", "continue", None, Some("id-other")).unwrap_err();
        assert_eq!(err.data["error"], "terminal_changed");
        assert!(fake.state.lock().unwrap().sent.is_empty());
        // The observed pane itself accepts the pinned send.
        send_text_guarded(&sh, "work", "continue", None, Some("id-work")).unwrap();
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
    }

    #[test]
    fn the_routine_nudges_only_while_the_todo_file_has_work_left() {
        let (sh, fake, dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue",
                   "todo_file": "TODO.md"}),
            "tools-routine-todo",
        );
        // The pane runs in the canonical directory the profile resolved to.
        let todo = std::fs::canonicalize(dir.path()).unwrap().join("TODO.md");
        std::fs::write(&todo, "- [x] everything is done\n").unwrap();
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(60));
        assert!(fake.state.lock().unwrap().sent.is_empty());
        // Work appears in the file: the same stalled pane is nudged.
        std::fs::write(&todo, "- [x] done\n- [ ] one more thing\n").unwrap();
        sh.routine_tick_at(t0 + Duration::from_secs(61));
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
    }

    #[test]
    fn a_pane_from_a_profile_without_a_routine_is_not_watched() {
        let dir = crate::testutil::TempDir::new("tools-routine-unwatched");
        let fake = FakeBackend::new(true);
        fake.state.lock().unwrap().runnable = true;
        let sh = Shepherd::with_policy(fake.clone(), kiro_policy(dir.path(), false));
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "plain", "name": "work"})),
        )
        .unwrap();
        {
            let mut state = fake.state.lock().unwrap();
            state.panes[0].status = Some(ProgramStatus {
                state: State::Done,
                kind: None,
            });
            state.panes[0].turn = Some(3);
        }
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn restarting_a_name_from_a_profile_without_a_routine_forgets_the_old_watch() {
        // Start `work` from a routine profile, then start the same name
        // from a profile without one (here with arbitrary argv, which the
        // policy allows): the old registration must be dropped, or the
        // new pane would be nudged under the old profile's rules.
        let dir = crate::testutil::TempDir::new("tools-routine-restart-plain");
        let fake = FakeBackend::new(true);
        fake.state.lock().unwrap().runnable = true;
        let bin = fake_kiro(dir.path()).to_string_lossy().into_owned();
        let text = json!({
            "allow_arbitrary_command": true,
            "profiles": [
                {"name": "kiro", "argv": ["kiro-cli", "chat"], "cwds": [dir.path()],
                 "routine": {"idle_timeout_secs": 10, "nudge_text": "continue"}}
            ]
        });
        let policy = LaunchPolicy::from_json(&text.to_string())
            .unwrap()
            .with_env_lookup(move |key| (key == "PATH").then(|| bin.clone()));
        let sh = Shepherd::with_policy(fake.clone(), policy);
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": "work"})),
        )
        .unwrap();
        assert_eq!(sh.routine_map().len(), 1);
        call(
            &sh,
            "start_pane",
            &args(json!({"argv": ["true"], "name": "work"})),
        )
        .unwrap();
        assert!(
            sh.routine_map().is_empty(),
            "the restart kept the old pane's watch"
        );
    }

    #[test]
    fn the_routine_skips_a_pane_whose_lock_stays_held_and_nudges_it_later() {
        // Another call holds the pane's lock (a long send or wait). The
        // routine thread must not block on it: that would stall every
        // other watched pane. It skips the pane for the tick, counts
        // nothing, and nudges once the lock is free.
        let (mut sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "cooldown_secs": 10,
                   "nudge_text": "continue"}),
            "tools-routine-lock-held",
        );
        sh.routine_lock_wait = Duration::from_millis(50);
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        let held = sh.pane_locks().lock("id-work");
        let (done_tx, done) = std::sync::mpsc::channel();
        let ticker = {
            let sh = sh.clone();
            thread::spawn(move || {
                sh.routine_tick_at(t0 + Duration::from_secs(11));
                done_tx.send(()).unwrap();
            })
        };
        let finished = done.recv_timeout(Duration::from_secs(10)).is_ok();
        // Release the lock first, so a blocked tick (a failing run) ends too.
        drop(held);
        ticker.join().unwrap();
        assert!(finished, "the routine tick blocked on the held pane lock");
        assert!(fake.state.lock().unwrap().sent.is_empty());
        // Nothing was counted against the pane: the next tick, 1 s later and
        // well inside what a counted attempt's cooldown would be, nudges.
        sh.routine_tick_at(t0 + Duration::from_secs(12));
        assert_eq!(fake.state.lock().unwrap().sent.len(), 1);
    }

    fn sends_tried(fake: &FakeBackend) -> usize {
        fake.calls().iter().filter(|call| **call == "send").count()
    }

    #[test]
    fn a_pasted_but_unsubmitted_nudge_stops_the_watch_and_a_plain_failure_only_cools_down() {
        let json_config = json!({"idle_timeout_secs": 10, "cooldown_secs": 10,
                                 "nudge_text": "continue"});
        // The backend's own paste-without-Enter failure: no second paste.
        let (sh, fake, _dir) = watched_pane(json_config.clone(), "tools-routine-pasted");
        fake.state.lock().unwrap().send_error = Some(crate::backend::pasted_unsubmitted(
            "id-work",
            "the program became blocked",
        ));
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sends_tried(&fake), 1);
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert_eq!(sends_tried(&fake), 1, "the routine pasted a second copy");
        // Control: any other failure is counted (cooldown) and retried after.
        let (sh, fake, _dir) = watched_pane(json_config, "tools-routine-failed");
        fake.state.lock().unwrap().send_error = Some(BackendError::new(ErrorKind::Other, "boom"));
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sends_tried(&fake), 1);
        sh.routine_tick_at(t0 + Duration::from_secs(12));
        assert_eq!(sends_tried(&fake), 1, "a failed nudge skipped the cooldown");
        sh.routine_tick_at(t0 + Duration::from_secs(22));
        assert_eq!(sends_tried(&fake), 2);
    }

    #[test]
    fn a_working_directory_swapped_for_a_symlink_after_the_start_is_not_followed() {
        // The watch resolved the pane's directory when it started. Swapping
        // that path for a symlink afterwards (the real directory moved
        // aside) must not carry the todo read into the symlink's target.
        let (sh, fake, dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue",
                   "todo_file": "TODO.md"}),
            "tools-routine-cwd-swap",
        );
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(cwd.join("TODO.md"), "- [x] everything is done\n").unwrap();
        let outside = crate::testutil::TempDir::new("tools-routine-cwd-swap-outside");
        std::fs::write(outside.path().join("TODO.md"), "- [ ] planted\n").unwrap();
        let moved = cwd.with_extension("moved");
        std::fs::rename(&cwd, &moved).unwrap();
        std::os::unix::fs::symlink(outside.path(), &cwd).unwrap();
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(60));
        let sent = fake.state.lock().unwrap().sent.len();
        // Put the directory back so the temp dir cleans up.
        std::fs::remove_file(&cwd).unwrap();
        std::fs::rename(&moved, &cwd).unwrap();
        assert_eq!(sent, 0, "the routine followed the swapped symlink");
    }

    // -----------------------------------------------------------------
    // Issue #76: the agent kind, the routine's counters and the answered
    // flag survive a front restart through the registry.
    // -----------------------------------------------------------------

    use crate::registry::{MemoryRegistry, RegisteredPane};

    const RESTART_ROUTINE: fn() -> Value = || {
        json!({"idle_timeout_secs": 10, "cooldown_secs": 1,
               "max_consecutive_nudges": 2, "nudge_text": "continue"})
    };

    /// A registry that notes how many sends the fake had seen when each
    /// routine write arrived, and can refuse the writes.
    struct Spy {
        inner: MemoryRegistry,
        fake: FakeBackend,
        routine_writes: Mutex<Vec<(u32, usize)>>,
        refuse_routine: std::sync::atomic::AtomicBool,
    }

    impl Spy {
        fn new(fake: &FakeBackend) -> Arc<Self> {
            Arc::new(Self {
                inner: MemoryRegistry::default(),
                fake: fake.clone(),
                routine_writes: Mutex::new(Vec::new()),
                refuse_routine: std::sync::atomic::AtomicBool::new(false),
            })
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
            if self
                .refuse_routine
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(BackendError::new(ErrorKind::Other, "injected"));
            }
            if let Some(r) = routine {
                let sent = self.fake.state.lock().unwrap().sent.len();
                self.routine_writes
                    .lock()
                    .unwrap()
                    .push((r.consecutive, sent));
            }
            self.inner.set_routine(pane_id, routine)
        }
        fn set_routines(
            &self,
            pane_id: &str,
            routines: &crate::registry::RoutineSet,
        ) -> Result<(), BackendError> {
            self.inner.set_routines(pane_id, routines)
        }
        fn forget(&self, pane_id: &str) -> Result<(), BackendError> {
            self.inner.forget(pane_id)
        }
        fn registered(&self) -> Vec<RegisteredPane> {
            self.inner.registered()
        }
    }

    /// A front over a runnable fake with `registry`, and pane `work` started
    /// from the routine profile, finished at turn 3.
    fn registered_pane(
        registry: Arc<dyn PaneRegistry>,
        fake: &FakeBackend,
        dir: &crate::testutil::TempDir,
    ) -> Shepherd {
        fake.state.lock().unwrap().runnable = true;
        let sh = Shepherd::with_policy(fake.clone(), routine_policy(dir.path(), RESTART_ROUTINE()))
            .with_registry(registry);
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        call(
            &sh,
            "start_pane",
            &args(json!({"profile": "kiro", "name": "work"})),
        )
        .unwrap();
        let mut state = fake.state.lock().unwrap();
        state.panes[0].status = Some(ProgramStatus {
            state: State::Done,
            kind: None,
        });
        state.panes[0].turn = Some(3);
        drop(state);
        sh
    }

    /// A new front over the same backend and registry: nothing in memory,
    /// then `adopt_registry`.
    fn restarted(
        registry: Arc<dyn PaneRegistry>,
        fake: &FakeBackend,
        policy: LaunchPolicy,
    ) -> Shepherd {
        let sh = Shepherd::with_policy(fake.clone(), policy).with_registry(registry);
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        assert_eq!(sh.agent_of("work"), None, "a new front starts empty");
        sh.adopt_registry();
        sh
    }

    fn sent_count(fake: &FakeBackend) -> usize {
        fake.state.lock().unwrap().sent.len()
    }

    #[test]
    fn a_profile_start_records_agent_profile_and_an_unspent_routine() {
        let dir = crate::testutil::TempDir::new("tools-registry-start");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        let _sh = registered_pane(reg.clone(), &fake, &dir);
        let rec = reg.get("id-work").expect("the start was recorded");
        assert_eq!(rec.agent.as_deref(), Some("kiro"));
        assert_eq!(rec.profile.as_deref(), Some("kiro"));
        assert_eq!(rec.routine, Some(RoutineState::default()));
        assert!(!rec.answered);
    }

    #[test]
    fn a_declared_agent_start_is_recorded_without_a_profile_and_restored() {
        let fake = FakeBackend::new(true);
        fake.state.lock().unwrap().runnable = true;
        let reg = Arc::new(MemoryRegistry::default());
        let sh = Shepherd::with_policy(fake.clone(), LaunchPolicy::arbitrary())
            .with_registry(reg.clone());
        fake.state.lock().unwrap().locks = Some(Arc::clone(sh.pane_locks()));
        call(
            &sh,
            "start_pane",
            &args(json!({"argv": ["kiro-cli", "chat"], "name": "work", "agent": "kiro"})),
        )
        .unwrap();
        let rec = reg.get("id-work").expect("the start was recorded");
        assert_eq!(rec.agent.as_deref(), Some("kiro"));
        assert_eq!(rec.profile, None);
        assert_eq!(rec.routine, None);
        drop(sh);
        let sh = restarted(reg, &fake, LaunchPolicy::arbitrary());
        assert_eq!(sh.agent_of("work"), Some(Agent::Kiro));
        assert_eq!(sh.profile_of("work"), None);
        assert!(sh.routine_map().get("work").is_none());
    }

    #[test]
    fn a_restarted_front_gets_the_agent_and_the_watch_back() {
        let dir = crate::testutil::TempDir::new("tools-registry-restore");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        let old = registered_pane(reg.clone(), &fake, &dir);
        drop(old);
        let policy = routine_policy(dir.path(), RESTART_ROUTINE());
        let sh = restarted(reg, &fake, policy);
        assert_eq!(sh.agent_of("work"), Some(Agent::Kiro));
        assert_eq!(
            sh.routine_map()
                .get("work")
                .map(|w| w.pane_id().to_string()),
            Some("id-work".to_string())
        );
    }

    #[test]
    fn a_restored_kiro_pane_at_turn_zero_is_still_guarded() {
        let dir = crate::testutil::TempDir::new("tools-registry-trust-guard");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        let old = registered_pane(reg.clone(), &fake, &dir);
        drop(old);
        {
            let mut state = fake.state.lock().unwrap();
            state.panes[0].status = Some(ProgramStatus {
                state: State::Idle,
                kind: None,
            });
            state.panes[0].turn = Some(0);
            state.screen = Some(
                include_str!("../tests/fixtures/kiro-3.3.0/trust-all-warning-synthetic.screen.txt")
                    .to_string(),
            );
        }
        let sh = restarted(reg, &fake, routine_policy(dir.path(), RESTART_ROUTINE()));
        assert_eq!(sh.agent_of("work"), Some(Agent::Kiro));
        let err = call(
            &sh,
            "send_input",
            &args(json!({"terminal": "work", "text": "hello"})),
        )
        .unwrap_err();
        assert_eq!(err.data["error"], "pane_needs_trust_acceptance");
        assert_eq!(sent_count(&fake), 0);
    }

    #[test]
    fn the_nudge_cap_and_a_give_up_hold_across_a_restart() {
        let dir = crate::testutil::TempDir::new("tools-registry-cap");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        let sh = registered_pane(reg.clone(), &fake, &dir);
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        sh.routine_tick_at(t0 + Duration::from_secs(13));
        assert_eq!(sent_count(&fake), 2);
        sh.routine_tick_at(t0 + Duration::from_secs(15));
        assert_eq!(
            reg.get("id-work").unwrap().routine,
            Some(RoutineState {
                consecutive: 2,
                gave_up: true,
                nudge_allowance: routine::NUDGE_TURN_ALLOWANCE,
            })
        );
        drop(sh);
        // The same stalled pane, a front that remembers nothing but the record.
        let sh = restarted(reg, &fake, routine_policy(dir.path(), RESTART_ROUTINE()));
        let t1 = Instant::now();
        for secs in [0, 11, 100, 10_000] {
            sh.routine_tick_at(t1 + Duration::from_secs(secs));
        }
        assert_eq!(sent_count(&fake), 2, "a routine that gave up was rearmed");
    }

    #[test]
    fn the_idle_clock_restarts_but_the_counter_carries_on() {
        let dir = crate::testutil::TempDir::new("tools-registry-clock");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        let sh = registered_pane(reg.clone(), &fake, &dir);
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sent_count(&fake), 1);
        drop(sh);
        let sh = restarted(
            reg.clone(),
            &fake,
            routine_policy(dir.path(), RESTART_ROUTINE()),
        );
        let t1 = Instant::now();
        // The pane has been finished for ages, but the clock starts now.
        sh.routine_tick_at(t1);
        sh.routine_tick_at(t1 + Duration::from_secs(5));
        assert_eq!(sent_count(&fake), 1, "an adopted pane was nudged at once");
        sh.routine_tick_at(t1 + Duration::from_secs(11));
        assert_eq!(sent_count(&fake), 2);
        assert_eq!(reg.get("id-work").unwrap().routine.unwrap().consecutive, 2);
    }

    #[test]
    fn a_profile_that_lost_its_routine_means_no_watch() {
        let dir = crate::testutil::TempDir::new("tools-registry-noroutine");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        drop(registered_pane(reg.clone(), &fake, &dir));
        // Same profile name, no routine block any more.
        let sh = restarted(reg, &fake, kiro_policy(dir.path(), false));
        assert_eq!(sh.agent_of("work"), Some(Agent::Kiro));
        assert!(sh.routine_map().is_empty());
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert_eq!(sent_count(&fake), 0);
    }

    #[test]
    fn a_profile_that_is_gone_means_no_watch() {
        let dir = crate::testutil::TempDir::new("tools-registry-noprofile");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        drop(registered_pane(reg.clone(), &fake, &dir));
        let sh = restarted(reg, &fake, LaunchPolicy::default());
        assert!(sh.routine_map().is_empty());
        assert_eq!(sh.agent_of("work"), Some(Agent::Kiro));
    }

    #[test]
    fn a_record_for_another_pane_id_never_nudges_the_pane_that_has_the_name() {
        let dir = crate::testutil::TempDir::new("tools-registry-mismatch");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        drop(registered_pane(reg.clone(), &fake, &dir));
        // The record is for a pane that no longer exists under that id.
        reg.forget("id-work").unwrap();
        reg.insert(RegisteredPane {
            pane_id: "id-ghost".to_string(),
            name: Some("work".to_string()),
            cwd: dir.path().to_string_lossy().into_owned(),
            agent: Some("kiro".to_string()),
            profile: Some("kiro".to_string()),
            answered: false,
            routine: Some(RoutineState::default()),
            routines: Default::default(),
            live: true,
        });
        let sh = restarted(reg, &fake, routine_policy(dir.path(), RESTART_ROUTINE()));
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(3600));
        assert_eq!(sent_count(&fake), 0);
    }

    #[test]
    fn a_record_that_names_no_usable_agent_resurrects_nothing() {
        let dir = crate::testutil::TempDir::new("tools-registry-stale");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        for (agent, routine) in [
            (Some("someday-agent"), Some(RoutineState::default())),
            (None, Some(RoutineState::default())),
        ] {
            reg.insert(RegisteredPane {
                pane_id: "id-work".to_string(),
                name: Some("work".to_string()),
                cwd: dir.path().to_string_lossy().into_owned(),
                agent: agent.map(str::to_string),
                profile: Some("kiro".to_string()),
                answered: true,
                routine,
                routines: Default::default(),
                live: true,
            });
            let sh = restarted(
                reg.clone(),
                &fake,
                routine_policy(dir.path(), RESTART_ROUTINE()),
            );
            assert_eq!(sh.agent_of("work"), None);
            assert!(!sh.answer_pending("work"));
            assert!(sh.routine_map().is_empty());
        }
    }

    #[test]
    fn a_finished_pane_gets_its_agent_but_no_watch() {
        let dir = crate::testutil::TempDir::new("tools-registry-finished");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        drop(registered_pane(reg.clone(), &fake, &dir));
        let mut rec = reg.get("id-work").unwrap();
        rec.live = false;
        reg.insert(rec);
        let sh = restarted(reg, &fake, routine_policy(dir.path(), RESTART_ROUTINE()));
        assert_eq!(sh.agent_of("work"), Some(Agent::Kiro));
        assert!(sh.routine_map().is_empty());
    }

    #[test]
    fn a_replayed_pane_is_skipped_not_counted_as_a_failed_nudge() {
        let dir = crate::testutil::TempDir::new("tools-registry-replayed");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        let sh = registered_pane(reg.clone(), &fake, &dir);
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        fake.state.lock().unwrap().send_error =
            Some(BackendError::new(ErrorKind::Replayed, "replayed"));
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sent_count(&fake), 0);
        assert_eq!(
            sh.routine_map().get("work").unwrap().state(),
            RoutineState::default(),
            "a replayed refusal counted toward the cap"
        );
        assert_eq!(
            reg.get("id-work").unwrap().routine,
            Some(RoutineState::default()),
            "the pending count was not taken back"
        );
        // No cooldown either: the next tick nudges as soon as the pane is trusted.
        sh.routine_tick_at(t0 + Duration::from_secs(12));
        assert_eq!(sent_count(&fake), 1);
    }

    #[test]
    fn a_person_at_the_pane_is_a_skip_not_a_failed_nudge() {
        for refusal in [
            ErrorKind::HumanControlled {
                idle_secs: 3,
                expires_in_secs: 297,
            },
            ErrorKind::DraftPending,
        ] {
            let dir = crate::testutil::TempDir::new("tools-registry-human");
            let fake = FakeBackend::new(true);
            let reg = Arc::new(MemoryRegistry::default());
            let sh = registered_pane(reg.clone(), &fake, &dir);
            let t0 = Instant::now();
            sh.routine_tick_at(t0);
            // Far more ticks than the cap allows failed nudges.
            for tick in 11..31 {
                fake.state.lock().unwrap().send_error =
                    Some(BackendError::new(refusal.clone(), "a person is here"));
                sh.routine_tick_at(t0 + Duration::from_secs(tick));
            }
            assert_eq!(sent_count(&fake), 0, "{refusal:?}");
            let watch = sh.routine_map().get("work").unwrap().state();
            assert_eq!(watch, RoutineState::default(), "{refusal:?}: counted");
            assert_eq!(
                reg.get("id-work").unwrap().routine,
                Some(RoutineState::default()),
                "{refusal:?}: the pending count was not taken back"
            );
            // The person leaves: no cooldown, no give-up, the next tick nudges.
            sh.routine_tick_at(t0 + Duration::from_secs(31));
            assert_eq!(sent_count(&fake), 1, "{refusal:?}");
        }
    }

    #[test]
    fn a_person_at_the_pane_reaches_callers_with_their_codes_and_timings() {
        let held = backend_failure(
            Some("work"),
            BackendError::new(
                ErrorKind::HumanControlled {
                    idle_secs: 12,
                    expires_in_secs: 288,
                },
                "raw backend text",
            ),
        );
        assert_eq!(held.data["error"], "pane_human_controlled");
        assert_eq!(held.data["idle_secs"], 12);
        assert_eq!(held.data["expires_in_secs"], 288);
        assert_eq!(held.data["terminal"], "work");
        assert_eq!(held.data["backend_detail"], "raw backend text");
        assert!(held.text.starts_with("pane_human_controlled: "));
        assert!(held.text.contains("nothing was sent"), "{}", held.text);
        let draft = backend_failure(
            Some("work"),
            BackendError::new(ErrorKind::DraftPending, "raw"),
        );
        assert_eq!(draft.data["error"], "pane_draft_pending");
        assert!(draft.data.get("idle_secs").is_none());
        assert!(draft.text.contains("nothing was sent"), "{}", draft.text);
    }

    // -----------------------------------------------------------------
    // Issue #77: the `human` object, `draft_clear` and the routine pre-check.
    // -----------------------------------------------------------------

    use crate::testutil::FakeShared;

    fn shared_over(fake: &FakeBackend) -> (Shepherd, Arc<FakeShared>) {
        let shared = Arc::new(FakeShared::default());
        let sh = Shepherd::new(fake.clone()).with_shared_panes(shared.clone());
        (sh, shared)
    }

    fn status_of(sh: &Shepherd) -> Value {
        call(sh, "get_status", &args(json!({"terminal": "work"}))).unwrap()
    }

    fn listed(sh: &Shepherd) -> Value {
        call(sh, "list_panes", &args(json!({}))).unwrap()["panes"][0].clone()
    }

    #[test]
    fn the_human_object_is_left_out_when_nobody_is_at_the_pane() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        fake.state.lock().unwrap().listable = true;
        let plain = Shepherd::new(fake.clone());
        let (shared_sh, shared) = shared_over(&fake);
        // Neither a lease nor a draft: the same bytes as without the handle.
        assert_eq!(status_of(&shared_sh), status_of(&plain));
        assert_eq!(listed(&shared_sh), listed(&plain));
        assert!(status_of(&shared_sh).get("human").is_none());
        assert!(listed(&shared_sh).get("human").is_none());
        // The backend was asked, so the absence is the answer and not a skip.
        assert!(shared.asked.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    }

    #[test]
    fn the_human_object_shows_a_lease_a_draft_or_both() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        fake.state.lock().unwrap().listable = true;
        let (sh, shared) = shared_over(&fake);
        shared.person_at_the_pane(true, false);
        for pane in [status_of(&sh), listed(&sh)] {
            assert_eq!(
                pane["human"],
                json!({
                    "lease": {"holder": "person", "idle_secs": 3, "expires_in_secs": 297},
                    "draft": false
                })
            );
        }
        shared.person_at_the_pane(true, true);
        assert_eq!(status_of(&sh)["human"]["draft"], true);
        assert_eq!(status_of(&sh)["human"]["lease"]["holder"], "person");
        // A draft the person left behind: no lease, the flag alone.
        shared.person_at_the_pane(false, true);
        for pane in [status_of(&sh), listed(&sh)] {
            assert_eq!(pane["human"], json!({"lease": null, "draft": true}));
        }
        // The rest of the pane object is the one it always was.
        let mut without = status_of(&sh);
        without.as_object_mut().unwrap().remove("human");
        shared.person_at_the_pane(false, false);
        assert_eq!(without, status_of(&sh));
    }

    #[test]
    fn draft_clear_is_unsupported_on_a_backend_with_no_person_side() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        let sh = Shepherd::new(fake.clone());
        let err = call(&sh, "draft_clear", &args(json!({"terminal": "work"}))).unwrap_err();
        assert_eq!(err.data["error"], "unsupported");
    }

    #[test]
    fn draft_clear_clears_the_draft_and_leaves_the_lease() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        let (sh, shared) = shared_over(&fake);
        shared.person_at_the_pane(true, true);
        let out = call(&sh, "draft_clear", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(out, json!({"terminal": "work", "cleared": true}));
        assert!(shared.lease.lock().unwrap().is_some());
        assert!(!*shared.draft.lock().unwrap());
        let again = call(&sh, "draft_clear", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(again["cleared"], false);
        // It types nothing and takes no pane lock.
        assert!(fake.state.lock().unwrap().sent.is_empty());
    }

    #[test]
    fn draft_clear_checks_its_argument_and_names_only_a_real_pane() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        let (sh, shared) = shared_over(&fake);
        shared.person_at_the_pane(false, true);
        for bad in [json!({}), json!({"terminal": 5}), json!({"terminal": ""})] {
            let err = call(&sh, "draft_clear", &args(bad)).unwrap_err();
            assert_eq!(err.data["error"], "invalid_input");
        }
        let err = call(&sh, "draft_clear", &args(json!({"terminal": "nope"}))).unwrap_err();
        assert_eq!(err.data["error"], "not_found");
        assert_eq!(shared.clears(), 0);
        assert!(*shared.draft.lock().unwrap());
    }

    #[test]
    fn a_draft_that_cannot_be_cleared_is_reported_and_stays() {
        let fake = FakeBackend::new(true);
        fake.add_pane("work", State::Idle, None);
        let (sh, shared) = shared_over(&fake);
        shared.person_at_the_pane(false, true);
        *shared.clear_error.lock().unwrap() = Some(ErrorKind::Timeout);
        let err = call(&sh, "draft_clear", &args(json!({"terminal": "work"}))).unwrap_err();
        assert_eq!(err.data["error"], "timeout");
        assert!(*shared.draft.lock().unwrap());
    }

    #[test]
    fn the_draft_refusal_names_draft_clear() {
        let draft = backend_failure(
            Some("work"),
            BackendError::new(ErrorKind::DraftPending, "raw"),
        );
        assert!(draft.text.contains("draft_clear"), "{}", draft.text);
    }

    /// The routine's pre-check: a pane a person is at is skipped before the
    /// pending count is kept and before a `call` line is written.
    #[test]
    fn the_routine_writes_no_audit_line_while_a_lease_or_a_draft_exists() {
        use crate::audit::testing::memory_audit;
        for (lease, draft) in [(true, false), (false, true), (true, true)] {
            let dir = crate::testutil::TempDir::new("tools-routine-precheck");
            let fake = FakeBackend::new(true);
            let reg = Arc::new(MemoryRegistry::default());
            let (audit, sink) = memory_audit();
            let shared = Arc::new(FakeShared::default());
            let sh = registered_pane(reg.clone(), &fake, &dir)
                .with_audit(Arc::new(audit))
                .with_shared_panes(shared.clone());
            // The audit and the person side were added after the start.
            let t0 = Instant::now();
            sh.routine_tick_at(t0);
            shared.person_at_the_pane(lease, draft);
            for tick in 11..31 {
                sh.routine_tick_at(t0 + Duration::from_secs(tick));
            }
            let case = format!("lease {lease}, draft {draft}");
            assert!(sink.records().is_empty(), "{case}: {:?}", sink.records());
            assert_eq!(sent_count(&fake), 0, "{case}");
            assert!(
                !fake.calls().contains(&"send"),
                "{case}: the backend was asked to type: {:?}",
                fake.calls()
            );
            assert_eq!(
                sh.routine_map().get("work").unwrap().state(),
                RoutineState::default(),
                "{case}: counted"
            );
            assert_eq!(
                reg.get("id-work").unwrap().routine,
                Some(RoutineState::default()),
                "{case}: a pending count was kept for a nudge never sent"
            );
            // After `draft_clear` (and the lease ending) the next tick nudges,
            // and only now does the audit see the routine.
            shared.person_at_the_pane(lease, draft);
            *shared.lease.lock().unwrap() = None;
            if draft {
                let cleared = call(&sh, "draft_clear", &args(json!({"terminal": "work"}))).unwrap();
                assert_eq!(cleared["cleared"], true, "{case}");
            }
            sh.routine_tick_at(t0 + Duration::from_secs(31));
            assert_eq!(sent_count(&fake), 1, "{case}");
            let records = sink.records();
            assert!(
                records.iter().any(|r| r["client"] == audit::ROUTINE_ACTOR
                    && r["tool"] == "send_input"
                    && r["phase"] == "call"),
                "{case}: {records:?}"
            );
        }
    }

    /// A routine nudge is a `call` line and then an `outcome` line by the
    /// routine actor, naming the routine and its creator, never the text.
    #[test]
    fn a_nudge_writes_a_call_then_an_outcome_naming_the_routine() {
        use crate::audit::testing::memory_audit;
        let dir = crate::testutil::TempDir::new("tools-nudge-audit");
        let fake = FakeBackend::new(true);
        let (audit, sink) = memory_audit();
        let sh = registered_pane(Arc::new(MemoryRegistry::default()), &fake, &dir)
            .with_audit(Arc::new(audit));
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sent_count(&fake), 1);
        let records = sink.records();
        let nudges: Vec<&Value> = records
            .iter()
            .filter(|r| r["client"] == audit::ROUTINE_ACTOR)
            .collect();
        assert_eq!(nudges.len(), 2, "{records:?}");
        assert_eq!(nudges[0]["phase"], "call");
        assert_eq!(nudges[1]["phase"], "outcome");
        assert_eq!(nudges[1]["outcome"], "ok");
        for record in &nudges {
            assert_eq!(record["tool"], "send_input");
            assert_eq!(record["pane"], "work");
            assert_eq!(record["routine"], "profile");
            assert_eq!(record["created_by"], "profile");
            assert_eq!(record["text_len"], "continue".len());
        }
        assert!(
            !sink.bytes().contains("continue\""),
            "the text was recorded"
        );
    }

    /// A refused nudge is on file as an error, not as a sent one.
    #[test]
    fn a_refused_nudge_shows_its_error_code_in_the_outcome() {
        use crate::audit::testing::memory_audit;
        let dir = crate::testutil::TempDir::new("tools-nudge-audit-refused");
        let fake = FakeBackend::new(true);
        let (audit, sink) = memory_audit();
        let sh = registered_pane(Arc::new(MemoryRegistry::default()), &fake, &dir)
            .with_audit(Arc::new(audit));
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        fake.state.lock().unwrap().send_error =
            Some(BackendError::new(ErrorKind::Replayed, "replayed"));
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sent_count(&fake), 0);
        let records = sink.records();
        let outcome = records
            .iter()
            .find(|r| r["client"] == audit::ROUTINE_ACTOR && r["phase"] == "outcome")
            .unwrap_or_else(|| panic!("no outcome line: {records:?}"));
        assert_eq!(outcome["outcome"], "error:pane_replayed");
        assert_eq!(outcome["routine"], "profile");
    }

    #[test]
    fn the_count_is_kept_before_the_nudge_is_typed() {
        let dir = crate::testutil::TempDir::new("tools-registry-order");
        let fake = FakeBackend::new(true);
        let spy = Spy::new(&fake);
        let sh = registered_pane(spy.clone(), &fake, &dir);
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sent_count(&fake), 1);
        let writes = spy.routine_writes.lock().unwrap().clone();
        // (consecutive, sends seen at the time of the write): the register
        // write, the count before the bytes, the count after.
        assert_eq!(writes, [(0, 0), (1, 0), (1, 1)], "{writes:?}");
    }

    #[test]
    fn a_count_that_cannot_be_kept_means_the_nudge_is_not_typed() {
        let dir = crate::testutil::TempDir::new("tools-registry-refused");
        let fake = FakeBackend::new(true);
        let spy = Spy::new(&fake);
        let sh = registered_pane(spy.clone(), &fake, &dir);
        spy.refuse_routine
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sent_count(&fake), 0);
        assert_eq!(
            sh.routine_map().get("work").unwrap().state(),
            RoutineState::default()
        );
        // The registry comes back: the next tick nudges.
        spy.refuse_routine
            .store(false, std::sync::atomic::Ordering::SeqCst);
        sh.routine_tick_at(t0 + Duration::from_secs(12));
        assert_eq!(sent_count(&fake), 1);
    }

    #[test]
    fn the_answered_flag_is_kept_and_restored() {
        let dir = crate::testutil::TempDir::new("tools-registry-answered");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        let sh = registered_pane(reg.clone(), &fake, &dir);
        sh.mark_answered("work");
        assert!(reg.get("id-work").unwrap().answered);
        drop(sh);
        let sh = restarted(
            reg.clone(),
            &fake,
            routine_policy(dir.path(), RESTART_ROUTINE()),
        );
        assert!(sh.answer_pending("work"));
        sh.clear_answer("work");
        assert!(!reg.get("id-work").unwrap().answered);
    }

    #[test]
    fn removing_a_pane_clears_its_record() {
        let dir = crate::testutil::TempDir::new("tools-registry-remove");
        let fake = FakeBackend::new(true);
        let reg = Arc::new(MemoryRegistry::default());
        let sh = registered_pane(reg.clone(), &fake, &dir);
        call(&sh, "remove_pane", &args(json!({"terminal": "work"}))).unwrap();
        assert_eq!(reg.get("id-work"), None);
    }

    #[test]
    fn a_shepherd_without_a_registry_behaves_as_before() {
        let (sh, fake, _dir) = watched_pane(
            json!({"idle_timeout_secs": 10, "nudge_text": "continue"}),
            "tools-registry-none",
        );
        sh.adopt_registry();
        let t0 = Instant::now();
        sh.routine_tick_at(t0);
        sh.routine_tick_at(t0 + Duration::from_secs(11));
        assert_eq!(sent_count(&fake), 1);
    }
}
