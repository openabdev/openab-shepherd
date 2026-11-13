//! Helpers shared by unit tests: scratch directories, a git repository
//! initializer, and a fake terminal backend. Compiled only for `cargo test`.
//! The fake herdr-core socket is in `hc/fake.rs`.

pub mod scripted;

use crate::backend::{
    BackendError, Capabilities, ErrorKind, InputGuard, InterruptResult, Key, KeysResult,
    ProgramStatus, RemoveResult, RunSpec, ScreenText, SendResult, State, StopHow, TerminalBackend,
    TerminalInfo, TerminalList, TerminalRef, WaitResult,
};
use crate::panelock::PaneLocks;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh directory under /tmp, removed when dropped. /tmp keeps Unix socket
/// paths below the platform length limit.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!("/tmp/shep-{}-{tag}-{n}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Create a git repository at `dir`. Fails the test if git is not installed.
pub fn git_init(dir: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .arg(dir)
        .status()
        .expect("git must be installed to run this test");
    assert!(status.success(), "git init failed");
}

/// What a [`FakeBackend`] knows and saw.
#[derive(Debug, Default)]
pub struct FakeState {
    pub panes: Vec<TerminalInfo>,
    /// Each trait call, by method name, in order.
    pub calls: Vec<&'static str>,
    /// Text passed to `send`, and whether the pane lock was held for the pane
    /// at that moment.
    pub sent: Vec<(String, bool)>,
    /// The tools' pane locks, so `send` can check it runs under one.
    pub locks: Option<Arc<PaneLocks>>,
    /// When set, the Nth `get` (1-based, counting every get) first turns the
    /// pane it finds blocked with kind permission.
    pub block_on_get: Option<usize>,
    /// What `read` returns. `None` makes `read` unsupported.
    pub screen: Option<String>,
    /// Keys passed to `keys`, one entry per call.
    pub keys: Vec<Vec<Key>>,
    /// Each `keys` call replaces `screen` with the next of these, when any
    /// are left: the screen the program draws after the key.
    pub next_screens: std::collections::VecDeque<String>,
    /// Specs passed to `run`. Refused unless `runnable` is set.
    pub runs: Vec<RunSpec>,
    /// When set, `run` starts a live pane named after the spec and returns it.
    pub runnable: bool,
    /// Results `wait` returns, one per call, in order. `wait` is unsupported
    /// when this is empty.
    pub waits: std::collections::VecDeque<WaitResult>,
    /// When set, the next `remove` signals the sender once it has started,
    /// then waits for the receiver before it removes the pane.
    pub remove_gate: Option<(Sender<()>, Receiver<()>)>,
}

/// An in-memory backend. `get`, `send`, `remove`, and (with
/// [`FakeState::screen`] set) `read` and `keys` do real work; every other call is recorded and refused as
/// unsupported. With `conditional_input`, `send` refuses a blocked pane itself,
/// as a backend with an atomic guard would; with `conditional_keys`, `keys`
/// enforces its guard the same way.
#[derive(Clone)]
pub struct FakeBackend {
    pub caps: Capabilities,
    pub state: Arc<Mutex<FakeState>>,
}

impl FakeBackend {
    pub fn new(conditional_input: bool) -> Self {
        Self {
            caps: Capabilities {
                conditional_input,
                conditional_keys: false,
                client_command: true,
                client_cwd: true,
                client_env: true,
                screen_text: true,
                program_status: true,
                history: true,
                native_wait: true,
                survives_restart: false,
            },
            state: Arc::new(Mutex::new(FakeState::default())),
        }
    }

    /// Add a live pane `name` (id `id-<name>`) with this status.
    pub fn add_pane(&self, name: &str, state: State, kind: Option<&str>) {
        self.state.lock().unwrap().panes.push(TerminalInfo {
            id: format!("id-{name}"),
            name: Some(name.to_string()),
            live: true,
            exit_code: None,
            status: Some(ProgramStatus {
                state,
                kind: kind.map(str::to_string),
            }),
            reported: true,
            turn: Some(1),
            status_ms: None,
            argv: None,
            cwd: None,
        });
    }

    pub fn calls(&self) -> Vec<&'static str> {
        self.state.lock().unwrap().calls.clone()
    }

    fn record(&self, call: &'static str) {
        self.state.lock().unwrap().calls.push(call);
    }

    fn find(&self, t: &TerminalRef) -> Result<TerminalInfo, BackendError> {
        let state = self.state.lock().unwrap();
        state
            .panes
            .iter()
            .find(|p| p.id == t.as_str() || p.name.as_deref() == Some(t.as_str()))
            .cloned()
            .ok_or_else(|| BackendError::new(ErrorKind::NotFound, "fake: no such pane"))
    }

    fn unsupported<T>(&self, call: &'static str) -> Result<T, BackendError> {
        self.record(call);
        Err(BackendError::new(
            ErrorKind::Unsupported(call.to_string()),
            "fake: not implemented",
        ))
    }
}

impl TerminalBackend for FakeBackend {
    fn capabilities(&self) -> Capabilities {
        self.caps
    }

    fn run(&self, spec: &RunSpec) -> Result<TerminalInfo, BackendError> {
        let runnable = {
            let mut state = self.state.lock().unwrap();
            state.runs.push(spec.clone());
            state.runnable
        };
        if !runnable {
            return self.unsupported("run");
        }
        self.record("run");
        let info = TerminalInfo {
            id: format!("id-{}", spec.name.as_deref().unwrap_or("anon")),
            name: spec.name.clone(),
            live: true,
            exit_code: None,
            status: Some(ProgramStatus {
                state: State::Idle,
                kind: None,
            }),
            reported: true,
            turn: Some(1),
            status_ms: None,
            argv: spec.argv.clone(),
            cwd: spec.cwd.clone(),
        };
        self.state.lock().unwrap().panes.push(info.clone());
        Ok(info)
    }

    fn get(&self, t: &TerminalRef) -> Result<TerminalInfo, BackendError> {
        self.record("get");
        let mut state = self.state.lock().unwrap();
        let gets = state.calls.iter().filter(|call| **call == "get").count();
        if state.block_on_get == Some(gets) {
            if let Some(pane) = state
                .panes
                .iter_mut()
                .find(|p| p.id == t.as_str() || p.name.as_deref() == Some(t.as_str()))
            {
                pane.status = Some(ProgramStatus {
                    state: State::Blocked,
                    kind: Some("permission".to_string()),
                });
            }
        }
        drop(state);
        self.find(t)
    }

    fn list(&self, _all: bool) -> Result<TerminalList, BackendError> {
        self.unsupported("list")
    }

    fn read(&self, t: &TerminalRef, _lines: u32) -> Result<ScreenText, BackendError> {
        let Some(text) = self.state.lock().unwrap().screen.clone() else {
            return self.unsupported("read");
        };
        self.record("read");
        self.find(t)?;
        Ok(ScreenText {
            text,
            complete: Some(true),
            source: None,
            limit: None,
        })
    }

    fn send(
        &self,
        t: &TerminalRef,
        text: &str,
        guard: &InputGuard,
    ) -> Result<SendResult, BackendError> {
        self.record("send");
        let pane = self.find(t)?;
        let blocked = pane.status.as_ref().filter(|s| s.state == State::Blocked);
        if self.caps.conditional_input && guard.refuse_when_blocked {
            if let Some(status) = blocked {
                return Err(BackendError::new(
                    ErrorKind::Blocked {
                        kind: status.kind.clone(),
                    },
                    "fake: refused while blocked",
                ));
            }
        }
        let mut state = self.state.lock().unwrap();
        let held = state
            .locks
            .as_ref()
            .is_some_and(|locks| locks.is_held(&pane.id));
        state.sent.push((text.to_string(), held));
        Ok(SendResult {
            turn: pane.turn,
            started: Some(true),
            status: pane.status.map(|s| s.state),
        })
    }

    fn keys(
        &self,
        t: &TerminalRef,
        keys: &[Key],
        guard: &InputGuard,
    ) -> Result<KeysResult, BackendError> {
        if self.state.lock().unwrap().screen.is_none() {
            return self.unsupported("keys");
        }
        self.record("keys");
        let pane = self.find(t)?;
        if self.caps.conditional_keys {
            let state = pane.status.as_ref().map(|s| &s.state);
            let kind = pane.status.as_ref().and_then(|s| s.kind.as_deref());
            let holds = guard
                .if_state
                .as_ref()
                .is_none_or(|want| state == Some(want))
                && guard
                    .if_kind
                    .as_deref()
                    .is_none_or(|want| kind == Some(want))
                && guard.expect_turn.is_none_or(|want| pane.turn == Some(want));
            if !holds {
                return Err(BackendError::new(
                    ErrorKind::StatusMismatch,
                    "fake: key guard did not hold",
                ));
            }
        }
        let mut state = self.state.lock().unwrap();
        state.keys.push(keys.to_vec());
        if let Some(next) = state.next_screens.pop_front() {
            state.screen = Some(next);
        }
        Ok(KeysResult {
            status: pane.status.map(|s| s.state),
            turn: pane.turn,
        })
    }

    fn wait(&self, _t: &TerminalRef, _timeout: Duration) -> Result<WaitResult, BackendError> {
        let next = self.state.lock().unwrap().waits.pop_front();
        match next {
            Some(result) => {
                self.record("wait");
                Ok(result)
            }
            None => self.unsupported("wait"),
        }
    }

    fn interrupt(
        &self,
        _t: &TerminalRef,
        _timeout: Option<Duration>,
    ) -> Result<InterruptResult, BackendError> {
        self.unsupported("interrupt")
    }

    /// Forget the pane; a live one counts as having exited.
    fn remove(
        &self,
        t: &TerminalRef,
        _grace: Option<Duration>,
    ) -> Result<RemoveResult, BackendError> {
        self.record("remove");
        let gate = self.state.lock().unwrap().remove_gate.take();
        if let Some((started, proceed)) = gate {
            started.send(()).unwrap();
            proceed.recv().unwrap();
        }
        let pane = self.find(t)?;
        self.state.lock().unwrap().panes.retain(|p| p.id != pane.id);
        Ok(RemoveResult {
            was_live: pane.live,
            stopped: pane.live.then_some(StopHow::Exited),
            exit_code: pane.exit_code,
        })
    }
}
