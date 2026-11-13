//! Backends for the tool-flow tests in `tool_flows.rs`. Compiled only for
//! `cargo test`.
//!
//! [`ScriptedBackend`] answers the Nth trait call with the Nth scripted step
//! and records every call. [`PaneBackend`] is a stateful one-pane backend that
//! can hold a `read` so a test fixes the order of two concurrent calls.

use crate::backend::{
    BackendError, Capabilities, ErrorKind, InputGuard, InterruptResult, Key, KeysResult,
    ProgramStatus, ReadLimit, ReadSource, RemoveResult, RunSpec, ScreenText, SendResult, State,
    TerminalBackend, TerminalInfo, TerminalList, TerminalRef, WaitResult,
};
use crate::dialog::fixtures::{dialog_on_allow, dialog_on_deny};
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One call a backend received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Run(RunSpec),
    Get(String),
    List(bool),
    Read(String, u32),
    Send {
        id: String,
        text: String,
        guard: InputGuard,
    },
    Keys {
        id: String,
        keys: Vec<Key>,
        guard: InputGuard,
    },
    Wait(String, Duration),
    Interrupt(String, Option<Duration>),
    Remove(String, Option<Duration>),
}

impl Call {
    pub fn method(&self) -> &'static str {
        match self {
            Self::Run(_) => "run",
            Self::Get(_) => "get",
            Self::List(_) => "list",
            Self::Read(..) => "read",
            Self::Send { .. } => "send",
            Self::Keys { .. } => "keys",
            Self::Wait(..) => "wait",
            Self::Interrupt(..) => "interrupt",
            Self::Remove(..) => "remove",
        }
    }
}

/// The method of every call, in order.
pub fn methods(log: &[Call]) -> Vec<&'static str> {
    log.iter().map(Call::method).collect()
}

/// The key names of every `keys` call, in order.
pub fn keys_sent(log: &[Call]) -> Vec<Vec<&'static str>> {
    log.iter()
        .filter_map(|call| match call {
            Call::Keys { keys, .. } => Some(keys.iter().map(Key::as_str).collect()),
            _ => None,
        })
        .collect()
}

/// What a scripted step returns on success.
#[derive(Debug, Clone)]
pub enum Payload {
    Info(TerminalInfo),
    List(TerminalList),
    Screen(ScreenText),
    Send(SendResult),
    Keys(KeysResult),
    Wait(WaitResult),
    Interrupt(InterruptResult),
    Remove(RemoveResult),
}

/// One scripted reply, for the call named `method`.
#[derive(Debug, Clone)]
pub struct Step {
    method: &'static str,
    result: Result<Payload, BackendError>,
}

pub fn run_ok(info: TerminalInfo) -> Step {
    step("run", Payload::Info(info))
}

pub fn get(info: TerminalInfo) -> Step {
    step("get", Payload::Info(info))
}

pub fn list_ok(list: TerminalList) -> Step {
    step("list", Payload::List(list))
}

/// A read that returns `text` as the blocked screen.
pub fn screen(text: &str) -> Step {
    step(
        "read",
        Payload::Screen(ScreenText {
            text: text.to_string(),
            complete: Some(false),
            source: Some(ReadSource::Screen),
            limit: Some(ReadLimit::Blocked),
        }),
    )
}

pub fn read_ok(screen: ScreenText) -> Step {
    step("read", Payload::Screen(screen))
}

pub fn send_ok(result: SendResult) -> Step {
    step("send", Payload::Send(result))
}

pub fn keys_ok(status: State) -> Step {
    step(
        "keys",
        Payload::Keys(KeysResult {
            status: Some(status),
            turn: Some(1),
        }),
    )
}

pub fn wait_ok(result: WaitResult) -> Step {
    step("wait", Payload::Wait(result))
}

pub fn interrupt_ok(result: InterruptResult) -> Step {
    step("interrupt", Payload::Interrupt(result))
}

pub fn remove_ok(result: RemoveResult) -> Step {
    step("remove", Payload::Remove(result))
}

/// The call named `method` fails.
pub fn fail(method: &'static str, kind: ErrorKind, detail: &str) -> Step {
    Step {
        method,
        result: Err(BackendError::new(kind, detail)),
    }
}

fn step(method: &'static str, payload: Payload) -> Step {
    Step {
        method,
        result: Ok(payload),
    }
}

/// The pane `work` with this id, live, turn 1, and this status.
pub fn pane(id: &str, status: Option<ProgramStatus>) -> TerminalInfo {
    TerminalInfo {
        id: id.to_string(),
        name: Some("work".to_string()),
        live: true,
        exit_code: None,
        status,
        reported: true,
        turn: Some(1),
        status_ms: None,
        argv: None,
        cwd: None,
    }
}

pub fn status(state: State, kind: Option<&str>) -> ProgramStatus {
    ProgramStatus {
        state,
        kind: kind.map(str::to_string),
    }
}

/// `work` at `id`, done.
pub fn pane_at(id: &str) -> TerminalInfo {
    pane(id, Some(status(State::Done, None)))
}

/// `work` at `t8` with this status.
pub fn pane_with_status(state: State, kind: Option<&str>) -> TerminalInfo {
    pane("t8", Some(status(state, kind)))
}

/// `work` at `t8`, blocked with this dialog kind.
pub fn blocked_pane(kind: &str) -> TerminalInfo {
    pane_with_status(State::Blocked, Some(kind))
}

/// `work` at `t8` after its program ended with code 2.
pub fn finished_pane() -> TerminalInfo {
    TerminalInfo {
        live: false,
        exit_code: Some(2),
        status: None,
        ..pane("t8", None)
    }
}

fn caps(conditional_input: bool, conditional_keys: bool) -> Capabilities {
    Capabilities {
        conditional_input,
        conditional_keys,
        client_command: true,
        client_cwd: true,
        client_env: true,
        screen_text: true,
        program_status: true,
        history: true,
        native_wait: true,
        survives_restart: false,
    }
}

struct Script {
    steps: VecDeque<Step>,
    log: Vec<Call>,
    unexpected: Vec<String>,
}

/// A backend that answers each call with the next scripted step. A call that
/// does not match the next step, or comes after the last one, fails with
/// [`ErrorKind::Other`] and is reported by [`ScriptedBackend::finish`].
#[derive(Clone)]
pub struct ScriptedBackend {
    caps: Capabilities,
    script: Arc<Mutex<Script>>,
}

impl ScriptedBackend {
    pub fn new(conditional_input: bool, conditional_keys: bool, steps: Vec<Step>) -> Self {
        Self {
            caps: caps(conditional_input, conditional_keys),
            script: Arc::new(Mutex::new(Script {
                steps: steps.into(),
                log: Vec::new(),
                unexpected: Vec::new(),
            })),
        }
    }

    /// Return every call made. Fails the test when the script was not used up
    /// or a call did not match it. Call it after the tool calls returned.
    pub fn finish(&self) -> Vec<Call> {
        let script = self.script.lock().unwrap();
        assert!(
            script.unexpected.is_empty(),
            "unexpected calls: {:?}",
            script.unexpected
        );
        assert!(
            script.steps.is_empty(),
            "script not used up, left: {:?}",
            script.steps
        );
        script.log.clone()
    }

    fn next(&self, call: Call) -> Result<Payload, BackendError> {
        let mut script = self.script.lock().unwrap();
        let method = call.method();
        script.log.push(call);
        match script.steps.front() {
            Some(step) if step.method == method => {
                let step = script.steps.pop_front().expect("front step");
                step.result
            }
            other => {
                let expected = other.map_or("nothing", |step| step.method);
                let message = format!("scripted: unexpected {method}, expected {expected}");
                script.unexpected.push(message.clone());
                Err(BackendError::new(ErrorKind::Other, message))
            }
        }
    }
}

fn wrong(method: &str) -> BackendError {
    BackendError::new(
        ErrorKind::Other,
        format!("scripted: wrong payload for {method}"),
    )
}

impl TerminalBackend for ScriptedBackend {
    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn run(&self, spec: &RunSpec) -> Result<TerminalInfo, BackendError> {
        match self.next(Call::Run(spec.clone()))? {
            Payload::Info(info) => Ok(info),
            _ => Err(wrong("run")),
        }
    }

    fn get(&self, t: &TerminalRef) -> Result<TerminalInfo, BackendError> {
        match self.next(Call::Get(t.as_str().to_string()))? {
            Payload::Info(info) => Ok(info),
            _ => Err(wrong("get")),
        }
    }

    fn list(&self, all: bool) -> Result<TerminalList, BackendError> {
        match self.next(Call::List(all))? {
            Payload::List(list) => Ok(list),
            _ => Err(wrong("list")),
        }
    }

    fn read(&self, t: &TerminalRef, history_lines: u32) -> Result<ScreenText, BackendError> {
        match self.next(Call::Read(t.as_str().to_string(), history_lines))? {
            Payload::Screen(screen) => Ok(screen),
            _ => Err(wrong("read")),
        }
    }

    fn send(
        &self,
        t: &TerminalRef,
        text: &str,
        guard: &InputGuard,
    ) -> Result<SendResult, BackendError> {
        let call = Call::Send {
            id: t.as_str().to_string(),
            text: text.to_string(),
            guard: guard.clone(),
        };
        match self.next(call)? {
            Payload::Send(sent) => Ok(sent),
            _ => Err(wrong("send")),
        }
    }

    fn keys(
        &self,
        t: &TerminalRef,
        keys: &[Key],
        guard: &InputGuard,
    ) -> Result<KeysResult, BackendError> {
        let call = Call::Keys {
            id: t.as_str().to_string(),
            keys: keys.to_vec(),
            guard: guard.clone(),
        };
        match self.next(call)? {
            Payload::Keys(result) => Ok(result),
            _ => Err(wrong("keys")),
        }
    }

    fn wait(&self, t: &TerminalRef, timeout: Duration) -> Result<WaitResult, BackendError> {
        match self.next(Call::Wait(t.as_str().to_string(), timeout))? {
            Payload::Wait(result) => Ok(result),
            _ => Err(wrong("wait")),
        }
    }

    fn interrupt(
        &self,
        t: &TerminalRef,
        timeout: Option<Duration>,
    ) -> Result<InterruptResult, BackendError> {
        match self.next(Call::Interrupt(t.as_str().to_string(), timeout))? {
            Payload::Interrupt(result) => Ok(result),
            _ => Err(wrong("interrupt")),
        }
    }

    fn remove(
        &self,
        t: &TerminalRef,
        grace: Option<Duration>,
    ) -> Result<RemoveResult, BackendError> {
        match self.next(Call::Remove(t.as_str().to_string(), grace))? {
            Payload::Remove(result) => Ok(result),
            _ => Err(wrong("remove")),
        }
    }
}

/// The one pane a [`PaneBackend`] serves: id `p8`, name `work`.
pub const PANE_ID: &str = "p8";
pub const PANE_NAME: &str = "work";

/// State of the pane. It starts blocked on the probe's permission dialog with
/// the cursor on Allow once.
#[derive(Debug)]
pub struct PaneModel {
    pub blocked: bool,
    pub turn: u64,
    pub cursor: usize,
    /// Every call, in the order the backend received it.
    pub log: Vec<Call>,
    /// The cursor position at each Enter pressed while blocked.
    pub selected: Vec<usize>,
    reads: usize,
}

/// A backend with one pane. Concurrent calls really overlap, and it can hold
/// the reply to the Nth `read` so a test fixes the order of two calls without
/// sleeping. Like the script backend it has conditional input but not
/// conditional keys.
#[derive(Clone)]
pub struct PaneBackend {
    model: Arc<Mutex<PaneModel>>,
    pause_at_read: Option<usize>,
    paused_tx: Arc<Mutex<Sender<()>>>,
    paused_rx: Arc<Mutex<Receiver<()>>>,
    release_tx: Arc<Mutex<Sender<()>>>,
    release_rx: Arc<Mutex<Receiver<()>>>,
}

impl PaneBackend {
    /// `pause_at_read` is the 1-based index of the `read` to hold until
    /// [`PaneBackend::release`].
    pub fn new(pause_at_read: Option<usize>) -> Self {
        let (paused_tx, paused_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        Self {
            model: Arc::new(Mutex::new(PaneModel {
                blocked: true,
                turn: 1,
                cursor: 0,
                log: Vec::new(),
                selected: Vec::new(),
                reads: 0,
            })),
            pause_at_read,
            paused_tx: Arc::new(Mutex::new(paused_tx)),
            paused_rx: Arc::new(Mutex::new(paused_rx)),
            release_tx: Arc::new(Mutex::new(release_tx)),
            release_rx: Arc::new(Mutex::new(release_rx)),
        }
    }

    /// Wait until the held read has arrived (its reply is held back).
    pub fn wait_paused(&self) {
        self.paused_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("the paused read never arrived");
    }

    /// Let the held read reply.
    pub fn release(&self) {
        self.release_tx
            .lock()
            .unwrap()
            .send(())
            .expect("release the paused read");
    }

    /// The pane as it is now. Call it after every tool call returned.
    pub fn finish(&self) -> (Vec<Call>, Vec<usize>) {
        let model = self.model.lock().unwrap();
        (model.log.clone(), model.selected.clone())
    }

    fn info(model: &PaneModel) -> TerminalInfo {
        let now = if model.blocked {
            status(State::Blocked, Some("permission"))
        } else {
            status(State::Working, None)
        };
        TerminalInfo {
            turn: Some(model.turn),
            ..pane(PANE_ID, Some(now))
        }
    }

    fn check(t: &TerminalRef) -> Result<(), BackendError> {
        if t.as_str() == PANE_ID || t.as_str() == PANE_NAME {
            Ok(())
        } else {
            Err(BackendError::new(ErrorKind::NotFound, "no such pane"))
        }
    }
}

impl TerminalBackend for PaneBackend {
    fn capabilities(&self) -> Capabilities {
        caps(true, false)
    }

    fn run(&self, _spec: &RunSpec) -> Result<TerminalInfo, BackendError> {
        Err(BackendError::new(ErrorKind::Other, "pane backend: no run"))
    }

    fn get(&self, t: &TerminalRef) -> Result<TerminalInfo, BackendError> {
        let mut model = self.model.lock().unwrap();
        model.log.push(Call::Get(t.as_str().to_string()));
        Self::check(t)?;
        Ok(Self::info(&model))
    }

    fn list(&self, _all: bool) -> Result<TerminalList, BackendError> {
        Err(BackendError::new(ErrorKind::Other, "pane backend: no list"))
    }

    fn read(&self, t: &TerminalRef, history_lines: u32) -> Result<ScreenText, BackendError> {
        let (reply, pause) = {
            let mut model = self.model.lock().unwrap();
            model
                .log
                .push(Call::Read(t.as_str().to_string(), history_lines));
            Self::check(t)?;
            model.reads += 1;
            let pause = Some(model.reads) == self.pause_at_read;
            let text = match (model.blocked, model.cursor) {
                (false, _) => "working".to_string(),
                (true, 1) => dialog_on_deny(),
                (true, _) => dialog_on_allow(),
            };
            (
                ScreenText {
                    text,
                    complete: Some(false),
                    source: Some(ReadSource::Screen),
                    limit: None,
                },
                pause,
            )
        };
        if pause {
            self.paused_tx
                .lock()
                .unwrap()
                .send(())
                .expect("signal pause");
            self.release_rx
                .lock()
                .unwrap()
                .recv()
                .expect("wait for release");
        }
        Ok(reply)
    }

    fn send(
        &self,
        t: &TerminalRef,
        text: &str,
        guard: &InputGuard,
    ) -> Result<SendResult, BackendError> {
        let mut model = self.model.lock().unwrap();
        model.log.push(Call::Send {
            id: t.as_str().to_string(),
            text: text.to_string(),
            guard: guard.clone(),
        });
        Self::check(t)?;
        if model.blocked {
            return Err(BackendError::new(
                ErrorKind::Blocked { kind: None },
                "the pane is blocked",
            ));
        }
        Ok(SendResult {
            turn: Some(model.turn),
            started: Some(true),
            status: Some(State::Working),
        })
    }

    fn keys(
        &self,
        t: &TerminalRef,
        keys: &[Key],
        guard: &InputGuard,
    ) -> Result<KeysResult, BackendError> {
        let mut model = self.model.lock().unwrap();
        model.log.push(Call::Keys {
            id: t.as_str().to_string(),
            keys: keys.to_vec(),
            guard: guard.clone(),
        });
        Self::check(t)?;
        for key in keys {
            match key {
                Key::Down => model.cursor = (model.cursor + 1).min(3),
                Key::Up => model.cursor = model.cursor.saturating_sub(1),
                Key::Enter if model.blocked => {
                    let cursor = model.cursor;
                    model.selected.push(cursor);
                    model.blocked = false;
                }
                _ => {}
            }
        }
        let state = if model.blocked {
            State::Blocked
        } else {
            State::Working
        };
        Ok(KeysResult {
            status: Some(state),
            turn: Some(model.turn),
        })
    }

    fn wait(&self, _t: &TerminalRef, _timeout: Duration) -> Result<WaitResult, BackendError> {
        Err(BackendError::new(ErrorKind::Other, "pane backend: no wait"))
    }

    fn interrupt(
        &self,
        _t: &TerminalRef,
        _timeout: Option<Duration>,
    ) -> Result<InterruptResult, BackendError> {
        Err(BackendError::new(
            ErrorKind::Other,
            "pane backend: no interrupt",
        ))
    }

    fn remove(
        &self,
        _t: &TerminalRef,
        _grace: Option<Duration>,
    ) -> Result<RemoveResult, BackendError> {
        Err(BackendError::new(
            ErrorKind::Other,
            "pane backend: no remove",
        ))
    }
}
