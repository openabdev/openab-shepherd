//! Integration tests: NativeBackend over real programs on a real PTY. No HTTP
//! and no listener; timeouts are generous for a loaded build machine.

use super::*;
use crate::profiles::LaunchPolicy;
use crate::testutil::TempDir;
use crate::tools::{self, Args, Shepherd};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::process::Command;

const LONG: Duration = Duration::from_secs(10);

thread_local! {
    /// Set to make the next pane thread started on this thread fail.
    pub(super) static FAIL_NEXT_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The pid of the last child `insert` recorded on this thread.
    pub(super) static LAST_SPAWN_PID: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// idle; read a line; working then blocked (permission); read a line (Enter);
/// echo it; working; 1 s; done; read a line.
const OSC_SCRIPT: &str = "printf '\\033]7501;state=idle\\007'; read a; \
printf '\\033]7501;state=working\\007\\033]7501;state=blocked:kind=permission\\007'; read b; \
echo \"got:$b\"; printf '\\033]7501;state=working\\007'; sleep 1; \
printf '\\033]7501;state=done\\007'; read c; echo \"after:$c\"";

fn backend() -> NativeBackend {
    NativeBackend::new(NativeConfig::default())
}

fn spec_line(name: &str, line: &str, dir: &TempDir) -> RunSpec {
    RunSpec {
        line: Some(line.to_string()),
        name: Some(name.to_string()),
        cwd: Some(dir.path().to_string_lossy().into_owned()),
        ..RunSpec::default()
    }
}

fn spec_argv(name: &str, argv: &[&str], dir: &TempDir) -> RunSpec {
    RunSpec {
        argv: Some(argv.iter().map(|s| s.to_string()).collect()),
        name: Some(name.to_string()),
        cwd: Some(dir.path().to_string_lossy().into_owned()),
        ..RunSpec::default()
    }
}

fn screen(b: &NativeBackend, info: &TerminalInfo) -> String {
    b.read(&info.reference(), 0).unwrap().text
}

/// Poll the screen until it contains `needle`.
fn wait_for_screen(b: &NativeBackend, info: &TerminalInfo, needle: &str) -> String {
    let end = Instant::now() + LONG;
    loop {
        let text = screen(b, info);
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < end,
            "{needle:?} never appeared in:\n{text}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Poll `get` until `pred` holds.
fn wait_for(
    b: &NativeBackend,
    info: &TerminalInfo,
    pred: impl Fn(&TerminalInfo) -> bool,
) -> TerminalInfo {
    let end = Instant::now() + LONG;
    loop {
        let now = b.get(&info.reference()).unwrap();
        if pred(&now) {
            return now;
        }
        assert!(Instant::now() < end, "condition never held: {now:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn state_is(info: &TerminalInfo, state: State) -> bool {
    info.status.as_ref().is_some_and(|s| s.state == state)
}

fn pane_of(b: &NativeBackend, info: &TerminalInfo) -> Arc<Pane> {
    b.inner.resolve(&info.reference()).unwrap()
}

/// Whether any process is left in group `pgid`.
fn group_alive(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    let rc = unsafe { libc::kill(-pgid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn group_gone_within(pgid: i32, within: Duration) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if !group_alive(pgid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !group_alive(pgid)
}

fn args(value: Value) -> Args {
    value.as_object().unwrap().clone()
}

#[test]
fn capabilities_are_all_true_except_restart() {
    let caps = backend().capabilities();
    assert_eq!(
        caps,
        Capabilities {
            conditional_input: true,
            conditional_keys: true,
            client_command: true,
            client_cwd: true,
            client_env: true,
            screen_text: true,
            program_status: true,
            history: true,
            native_wait: true,
            survives_restart: false,
        }
    );
}

// (a)
#[test]
fn echo_shows_on_the_screen_and_wait_reports_exit() {
    let dir = TempDir::new("native-echo");
    let b = backend();
    let info = b.run(&spec_line("echo", "echo hello", &dir)).unwrap();
    assert!(info.id.starts_with("n:"));
    assert_eq!(info.name.as_deref(), Some("echo"));
    assert_eq!(info.cwd.as_deref(), dir.path().to_str());
    let done = b.wait(&info.reference(), LONG).unwrap();
    assert!(done.ended);
    assert_eq!(done.reason, Some(WaitReason::Exit));
    assert_eq!(done.exit_code, Some(0));
    assert_eq!(done.status, None);
    // The reader took the last output before the exit was reported.
    let read = b.read(&info.reference(), 0).unwrap();
    assert_eq!(read.text, "hello");
    assert_eq!(read.source, Some(ReadSource::Screen));
    assert_eq!(read.complete, Some(true));
    // History of a finished pane is still readable.
    let history = b.read(&info.reference(), 10).unwrap();
    assert_eq!(history.source, Some(ReadSource::History));
    assert!(history.text.contains("hello"));
    assert!(!b.get(&info.reference()).unwrap().live);
}

// (b)
#[test]
fn cat_echoes_a_send_and_bracketed_paste_wraps_it() {
    let dir = TempDir::new("native-cat");
    let b = backend();
    let cat = b.run(&spec_argv("cat", &["cat"], &dir)).unwrap();
    let sent = b
        .send(&cat.reference(), "ping-123", &InputGuard::default())
        .unwrap();
    // cat never reports a status, so nobody can tell whether it started.
    assert_eq!(sent.started, None);
    assert_eq!(sent.turn, Some(0));
    assert_eq!(sent.status, None);
    // The terminal's echo, then cat's copy.
    wait_for_screen(&b, &cat, "ping-123\nping-123");

    let paste = b
        .run(&spec_line(
            "paste",
            "printf '\\033[?2004h'; exec cat -v",
            &dir,
        ))
        .unwrap();
    let pane = pane_of(&b, &paste);
    let end = Instant::now() + LONG;
    while !pane.state.lock().unwrap().emu.bracketed_paste() {
        assert!(Instant::now() < end, "bracketed paste never turned on");
        thread::sleep(Duration::from_millis(10));
    }
    b.send(&paste.reference(), "abc", &InputGuard::default())
        .unwrap();
    wait_for_screen(&b, &paste, "^[[200~abc^[[201~");
}

// (c)
#[test]
fn osc_status_turns_and_guards_follow_the_program() {
    let dir = TempDir::new("native-osc");
    let b = backend();
    let info = b.run(&spec_line("osc", OSC_SCRIPT, &dir)).unwrap();
    let t = info.reference();

    // Not reported yet counts as working, so wait blocks until idle.
    let idle = b.wait(&t, LONG).unwrap();
    assert_eq!(idle.reason, Some(WaitReason::Status));
    assert_eq!(idle.status.as_ref().map(|s| &s.state), Some(&State::Idle));
    assert_eq!(idle.turn, Some(0));
    let got = b.get(&t).unwrap();
    assert!(got.reported);
    assert!(got.status_ms.is_some());

    let guard = InputGuard {
        refuse_when_blocked: true,
        ..InputGuard::default()
    };
    let sent = b.send(&t, "go", &guard).unwrap();
    assert_eq!(sent.turn, Some(0));
    assert_eq!(sent.status, Some(State::Idle));
    assert_eq!(sent.started, Some(true));

    let blocked = b.wait(&t, LONG).unwrap();
    assert_eq!(blocked.reason, Some(WaitReason::Status));
    assert_eq!(
        blocked.status,
        Some(crate::backend::ProgramStatus {
            state: State::Blocked,
            kind: Some("permission".to_string()),
        })
    );
    assert_eq!(blocked.turn, Some(1));

    // Typing into the blocked program is refused, with the kind.
    let err = b.send(&t, "MARKER-XYZ", &guard).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Blocked {
            kind: Some("permission".to_string())
        }
    );
    // A key with a stale turn is refused, and nothing is written.
    let stale = InputGuard {
        if_state: Some(State::Blocked),
        if_kind: Some("permission".to_string()),
        expect_turn: Some(0),
        ..InputGuard::default()
    };
    let err = b.keys(&t, &[Key::Enter], &stale).unwrap_err();
    assert_eq!(err.kind, ErrorKind::StatusMismatch);
    thread::sleep(Duration::from_millis(300));
    assert!(state_is(&b.get(&t).unwrap(), State::Blocked));
    assert!(!screen(&b, &info).contains("got:"));

    // The right turn goes through: the program reads Enter and moves on.
    let current = InputGuard {
        expect_turn: Some(1),
        ..stale
    };
    let pressed = b.keys(&t, &[Key::Enter], &current).unwrap();
    assert_eq!(pressed.turn, Some(1));
    let working = wait_for(&b, &info, |i| state_is(i, State::Working));
    assert_eq!(working.turn, Some(1));
    let done = b.wait(&t, LONG).unwrap();
    assert_eq!(done.status.as_ref().map(|s| &s.state), Some(&State::Done));
    assert_eq!(done.turn, Some(2));

    let text = screen(&b, &info);
    assert!(text.contains("got:"), "{text}");
    assert!(!text.contains("MARKER-XYZ"), "{text}");

    let log = b.status_log(&t).unwrap();
    let outcomes: Vec<&str> = log.iter().map(|e| e.outcome.as_str()).collect();
    assert_eq!(
        outcomes,
        [
            "idle",
            "working",
            "blocked kind=permission",
            "working",
            "done"
        ]
    );
    assert_eq!(log[2].body, b"state=blocked:kind=permission");
    let turns: Vec<u64> = log.iter().map(|e| e.turn_after).collect();
    assert_eq!(turns, [0, 0, 1, 1, 2]);
    assert_eq!(b.keyboard_mode(&t).unwrap(), KeyboardMode::default());
}

// (d)
#[test]
fn the_child_sees_only_the_whitelisted_environment() {
    let dir = TempDir::new("native-env");
    let parent: HashMap<&'static str, &'static str> = HashMap::from([
        ("HOME", "/tmp"),
        ("PATH", "/usr/bin:/bin"),
        ("USER", "shepherd-test"),
        ("TERM", "xterm-ghostty"),
        ("OPENAB_SHEPHERD_TOKEN", "token-value-secret-1"),
        ("OPENAB_SHEPHERD_TOKEN_FILE", "/tmp/token-file-secret-2"),
        ("OPENAB_SHEPHERD_BACKEND", "backend-secret-3"),
        ("HC_SOCKET", "/tmp/hc-socket-secret-4.sock"),
        ("AWS_SECRET_ACCESS_KEY", "aws-secret-5"),
    ]);
    let b = NativeBackend::with_env_lookup(NativeConfig::default(), move |key| {
        parent.get(key).map(OsString::from)
    });
    let info = b.run(&spec_argv("env", &["/usr/bin/env"], &dir)).unwrap();
    b.wait(&info.reference(), LONG).unwrap();
    let text = screen(&b, &info);
    let keys: BTreeSet<&str> = text
        .lines()
        .filter_map(|line| line.split_once('=').map(|(k, _)| k))
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "HOME",
            "LANG",
            "OPENAB_SHEPHERD_PANE",
            "PATH",
            "TERM",
            "USER"
        ]),
        "{text}"
    );
    assert!(text.lines().any(|l| l == "TERM=xterm-256color"), "{text}");
    assert!(text.lines().any(|l| l == "LANG=en_US.UTF-8"), "{text}");
    assert!(text.lines().any(|l| l == "PATH=/usr/bin:/bin"), "{text}");
    assert!(!text.contains("secret"), "{text}");
}

#[test]
fn child_env_defaults_when_the_parent_has_nothing() {
    let env = pty::child_env(|_| None, &[]).unwrap();
    let pairs: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    assert_eq!(
        pairs,
        [
            ("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("LANG".into(), "en_US.UTF-8".into()),
            ("TERM".into(), "xterm-256color".into()),
        ]
    );
}

/// Shepherd's own variables and KIRO_API_KEY never pass by name: the
/// whitelist is all a child gets from the parent.
#[test]
fn child_env_passes_no_credential_by_name() {
    let parent: HashMap<&'static str, &'static str> = HashMap::from([
        ("KIRO_API_KEY", "kiro-key"),
        ("KIRO_SESSION_ID", "session-secret-1"),
        ("OPENAB_SHEPHERD_TOKEN", "token-secret-2"),
        ("OPENAB_SHEPHERD_PROFILES", "/tmp/profiles-secret-3.json"),
        ("HC_SOCKET", "/tmp/hc-secret-4.sock"),
    ]);
    let env = pty::child_env(|key: &str| parent.get(key).map(OsString::from), &[]).unwrap();
    let names: Vec<String> = env
        .iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["PATH", "LANG", "TERM"]);
}

/// An executable shell script at `path` that prints `marker` and its env.
fn write_script(path: &std::path::Path, marker: &str) {
    std::fs::write(path, format!("#!/bin/sh\necho {marker}\n/usr/bin/env\n")).unwrap();
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// Owner decision SOL-B1: a credential reaches only the pane of the launch
/// profile that names it. A caller's argv or line, even with arbitrary
/// commands allowed and even for the operator's own kiro-cli, gets none: the
/// old kiro-cli name rule is gone.
#[test]
fn a_credential_reaches_only_its_profiles_pane() {
    let dir = TempDir::new("native-profile-env");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("kiro-cli"), "fake-kiro");
    let path = format!("/usr/bin:/bin:{}", bin.display());
    let parent_path = path.clone();
    let b = NativeBackend::with_env_lookup(NativeConfig::default(), move |key| match key {
        "PATH" => Some(OsString::from(parent_path.clone())),
        "KIRO_API_KEY" => Some(OsString::from("parent-kiro-key")),
        _ => None,
    });
    let profiles = json!({
        "allow_arbitrary_command": true,
        "profiles": [{"name": "kiro", "argv": ["kiro-cli"], "cwds": [dir.path()],
                      "env": {"KIRO_API_KEY": "env:PROFILE_KEY_SOURCE"}}]
    });
    let policy_path = path.clone();
    let policy = LaunchPolicy::from_json(&profiles.to_string())
        .unwrap()
        .with_env_lookup(move |key| match key {
            "PROFILE_KEY_SOURCE" => Some("profile-key-value".to_string()),
            "PATH" => Some(policy_path.clone()),
            _ => None,
        });
    let sh = Shepherd::with_policy(b.clone(), policy);
    let cwd = dir.path().to_str().unwrap();
    for (name, start, sees) in [
        ("prof", json!({"profile": "kiro", "name": "prof"}), true),
        (
            "argv",
            json!({"argv": ["kiro-cli"], "name": "argv", "cwd": cwd}),
            false,
        ),
        (
            "line",
            json!({"line": "kiro-cli", "name": "line", "cwd": cwd}),
            false,
        ),
    ] {
        tools::call(&sh, "start_pane", &args(start)).unwrap();
        tools::call(
            &sh,
            "wait_status",
            &args(json!({"terminal": name, "timeout": 10})),
        )
        .unwrap();
        let info = b.get(&TerminalRef::new(name)).unwrap();
        let text = wait_for_screen(&b, &info, "TERM=");
        assert!(text.contains("fake-kiro"), "{name}: {text}");
        assert_eq!(
            text.contains("KIRO_API_KEY=profile-key-value"),
            sees,
            "{name}: {text}"
        );
        assert!(!text.contains("parent-kiro-key"), "{name}: {text}");
        if !sees {
            assert!(!text.contains("KIRO_API_KEY"), "{name}: {text}");
        }
    }
    // The profile's pane runs in its canonical directory.
    let prof = b.get(&TerminalRef::new("prof")).unwrap();
    assert_eq!(
        prof.cwd.as_deref(),
        std::fs::canonicalize(dir.path()).unwrap().to_str()
    );
}

/// Review B2: a run's own variables reach that pane only; protected names
/// are refused before anything starts.
#[test]
fn a_per_run_environment_reaches_only_its_own_pane() {
    let dir = TempDir::new("native-run-env");
    let b = backend();
    let with = RunSpec {
        env: vec![
            ("SHEPHERD_RUN_A".to_string(), "alpha".to_string()),
            ("_EMPTY".to_string(), String::new()),
        ],
        ..spec_argv("with", &["/usr/bin/env"], &dir)
    };
    let info = b.run(&with).unwrap();
    b.wait(&info.reference(), LONG).unwrap();
    let text = screen(&b, &info);
    assert!(text.lines().any(|l| l == "SHEPHERD_RUN_A=alpha"), "{text}");
    assert!(text.lines().any(|l| l == "_EMPTY="), "{text}");
    let without = b
        .run(&spec_argv("without", &["/usr/bin/env"], &dir))
        .unwrap();
    b.wait(&without.reference(), LONG).unwrap();
    let text = screen(&b, &without);
    assert!(!text.contains("SHEPHERD_RUN_A"), "{text}");

    for key in [
        "HOME",
        "PATH",
        "USER",
        "LANG",
        "TERM",
        "OPENAB_SHEPHERD_PANE",
        "DYLD_INSERT_LIBRARIES",
        "LD_PRELOAD",
        "lower",
        "1A",
    ] {
        let spec = RunSpec {
            env: vec![(key.to_string(), "x".to_string())],
            ..spec_argv("refused", &["/usr/bin/env"], &dir)
        };
        let err = b.run(&spec).unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidInput, "{key}");
    }
    let nul = RunSpec {
        env: vec![("NUL_VALUE".to_string(), "a\0b".to_string())],
        ..spec_argv("nul", &["/usr/bin/env"], &dir)
    };
    let err = b.run(&nul).unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidInput);
    assert!(!err.detail.contains("a\0b"));
    let many = RunSpec {
        env: (0..=MAX_EXTRA_ENV)
            .map(|i| (format!("K{i}"), "v".to_string()))
            .collect(),
        ..spec_argv("many", &["/usr/bin/env"], &dir)
    };
    assert_eq!(b.run(&many).unwrap_err().kind, ErrorKind::InvalidInput);
    assert_eq!(b.list(true).unwrap().terminals.len(), 2);

    // Through the tool, the same.
    let sh = Shepherd::with_policy(backend(), LaunchPolicy::arbitrary());
    tools::call(
        &sh,
        "start_pane",
        &args(json!({"argv": ["/usr/bin/env"], "name": "tool-env",
            "cwd": dir.path().to_str(), "env": {"SHEPHERD_TOOL_B": "beta"}})),
    )
    .unwrap();
    tools::call(
        &sh,
        "wait_status",
        &args(json!({"terminal": "tool-env", "timeout": 10})),
    )
    .unwrap();
    let read = tools::call(&sh, "read_output", &args(json!({"terminal": "tool-env"}))).unwrap();
    assert!(
        read["text"]
            .as_str()
            .unwrap()
            .contains("SHEPHERD_TOOL_B=beta"),
        "{read}"
    );
}

#[test]
fn child_env_validates_extra_variables() {
    let pair = |k: &str, v: &str| (OsString::from(k), OsString::from(v));
    let env = pty::child_env(|_| None, &[pair("PROBE_X1", "yes"), pair("_Y", "")]).unwrap();
    assert!(env.contains(&pair("PROBE_X1", "yes")));
    assert!(env.contains(&pair("_Y", "")));
    for bad in [
        pair("lower", "x"),
        pair("1ABC", "x"),
        pair("A-B", "x"),
        pair("", "x"),
        pair("NUL", "a\0b"),
    ] {
        let err = pty::child_env(|_| None, std::slice::from_ref(&bad)).unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidInput, "{bad:?}");
        assert!(!err.detail.contains("a\0b"));
    }
    let many: Vec<_> = (0..=MAX_EXTRA_ENV)
        .map(|i| pair(&format!("K{i}"), "v"))
        .collect();
    assert!(pty::child_env(|_| None, &many[..MAX_EXTRA_ENV]).is_ok());
    assert_eq!(
        pty::child_env(|_| None, &many).unwrap_err().kind,
        ErrorKind::InvalidInput
    );
}

// (e)
#[test]
fn children_inherit_only_stdio_even_when_spawned_together() {
    let dir = TempDir::new("native-fds");
    let b = backend();
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let b = b.clone();
            let spec = spec_line(&format!("fds{i}"), "ls /dev/fd", &dir);
            thread::spawn(move || {
                let info = b.run(&spec).unwrap();
                b.wait(&info.reference(), LONG).unwrap();
                screen(&b, &info)
            })
        })
        .collect();
    for handle in handles {
        let text = handle.join().unwrap();
        let fds: BTreeSet<u32> = text
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect();
        // 0, 1, 2 and what ls itself opened, which take the lowest free
        // numbers: one directory on Linux, two on macOS (fts opens "." as
        // well; a plain ssh shell on remote build host lists 0-4 too). A leaked
        // descriptor would add a number or push ls's own up.
        let own = if cfg!(target_os = "macos") { 2 } else { 1 };
        let expected: BTreeSet<u32> = (0..3 + own).collect();
        assert_eq!(fds, expected, "inherited descriptors: {text}");
    }
}

// (f)
#[test]
fn interrupt_sends_ctrl_c_to_a_program_without_status() {
    let dir = TempDir::new("native-int");
    let b = backend();
    let info = b.run(&spec_line("sleeper", "sleep 30", &dir)).unwrap();
    let started = Instant::now();
    let result = b.interrupt(&info.reference(), None).unwrap();
    assert_eq!(result.key, Key::CtrlC);
    assert_eq!(result.outcome, InterruptOutcome::Exited);
    assert!(started.elapsed() < Duration::from_secs(10));
    let err = b.interrupt(&info.reference(), None).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Finished);
}

#[test]
fn interrupt_sends_a_raw_ctrl_c_despite_stale_keyboard_flags() {
    // Review O-2: flags left pushed must not turn Ctrl+C into CSI 99;5u,
    // which the line discipline would not turn into SIGINT.
    let dir = TempDir::new("native-int-kitty");
    let b = backend();
    let info = b
        .run(&spec_line("kitty", "printf '\\033[>1u'; sleep 30", &dir))
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    let result = b.interrupt(&info.reference(), None).unwrap();
    assert_eq!(result.key, Key::CtrlC);
    assert_eq!(result.outcome, InterruptOutcome::Exited);
}

#[test]
fn tcgetpgrp_on_the_master_names_the_session_group() {
    // ADR Q12: the foreground group read on the master is the leader's group.
    let dir = TempDir::new("native-pgrp");
    let b = backend();
    let info = b.run(&spec_line("pgrp", "sleep 30", &dir)).unwrap();
    let pane = pane_of(&b, &info);
    assert_eq!(pty::tcgetpgrp(pane.master.as_raw_fd()), Some(pane.pid));
}

// (g)
#[test]
fn remove_kills_the_whole_group() {
    let dir = TempDir::new("native-rm");
    let b = backend();
    let info = b
        .run(&spec_line("group", "sleep 300 & sleep 300 & wait", &dir))
        .unwrap();
    let pid = pane_of(&b, &info).pid;
    thread::sleep(Duration::from_millis(300));
    assert!(group_alive(pid));
    b.remove(&info.reference(), None).unwrap();
    assert!(group_gone_within(pid, Duration::from_secs(3)));
    let list = b.list(true).unwrap();
    assert!(list.terminals.iter().all(|p| p.id != info.id));
    assert_eq!(
        b.get(&info.reference()).unwrap_err().kind,
        ErrorKind::NotFound
    );
}

/// A Shepherd over a fresh native backend, and the backend itself.
fn native_shepherd() -> (NativeBackend, Shepherd) {
    let b = backend();
    let sh = Shepherd::with_policy(b.clone(), LaunchPolicy::arbitrary());
    (b, sh)
}

fn start_tool(sh: &Shepherd, dir: &TempDir, name: &str, line: &str) -> Value {
    tools::call(
        sh,
        "start_pane",
        &args(json!({"name": name, "line": line, "cwd": dir.path()})),
    )
    .unwrap()
}

fn listed_names(sh: &Shepherd) -> Vec<String> {
    let list = tools::call(sh, "list_panes", &args(json!({"all": true}))).unwrap();
    list["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// remove_pane on a running pane ends its whole process group, takes it out
/// of list_panes(all=true), answers later calls with not_found, frees the
/// name, and does not hang a wait_status in flight on it.
#[test]
fn remove_pane_ends_a_live_group_and_frees_the_name() {
    let dir = TempDir::new("native-rmpane");
    let (b, sh) = native_shepherd();
    start_tool(&sh, &dir, "group", "sleep 300 & sleep 300 & wait");
    start_tool(&sh, &dir, "other", "sleep 300");
    let pid = pane_of(&b, &b.get(&TerminalRef::new("group")).unwrap()).pid;
    thread::sleep(Duration::from_millis(300));
    assert!(group_alive(pid));

    let list = tools::call(&sh, "list_panes", &args(json!({}))).unwrap();
    assert_eq!(list["live_panes"], 2);
    assert_eq!(list["live_pane_limit"], MAX_LIVE_PANES);

    let waiter = {
        let sh = sh.clone();
        thread::spawn(move || {
            tools::call(
                &sh,
                "wait_status",
                &args(json!({"terminal": "group", "timeout": 30})),
            )
        })
    };
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    let out = tools::call(&sh, "remove_pane", &args(json!({"terminal": "group"}))).unwrap();
    assert_eq!(out["name"], "group");
    assert_eq!(out["was_live"], true);
    assert_eq!(out["stopped"], "exited", "{out}");
    // sh ended on the hangup or terminate signal.
    let code = out["exit_code"].as_i64().unwrap();
    assert!(code == 128 + 1 || code == 128 + 15, "{out}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(group_gone_within(pid, Duration::from_secs(3)));

    // The wait in flight ends (exited), or started after the removal.
    match waiter.join().unwrap() {
        Ok(wait) => assert_eq!(wait["outcome"], "exited", "{wait}"),
        Err(err) => assert_eq!(err.data["error"], "not_found", "{}", err.text),
    }

    assert_eq!(listed_names(&sh), ["other"]);
    let list = tools::call(&sh, "list_panes", &args(json!({}))).unwrap();
    assert_eq!(list["live_panes"], 1);
    for (tool, extra) in [
        ("send_input", json!({"text": "hi"})),
        ("answer_dialog", json!({"choice": "allow_once"})),
        ("get_status", json!({})),
        ("remove_pane", json!({})),
    ] {
        let mut a = args(extra);
        a.insert("terminal".to_string(), json!("group"));
        let err = tools::call(&sh, tool, &a).unwrap_err();
        assert_eq!(err.data["error"], "not_found", "{tool}");
    }

    // The name is free again.
    let again = start_tool(&sh, &dir, "group", "sleep 300");
    assert_eq!(again["name"], "group");
    assert_eq!(again["live"], true);
    let status = tools::call(&sh, "get_status", &args(json!({"terminal": "group"}))).unwrap();
    assert_eq!(status["live"], true);
    assert_eq!(listed_names(&sh), ["other", "group"]);
}

/// A program that ignores the hangup and terminate signals is killed once the
/// timeout passes.
#[test]
fn remove_pane_kills_a_program_that_ignores_the_signals() {
    let dir = TempDir::new("native-rmpane-kill");
    let (b, sh) = native_shepherd();
    start_tool(
        &sh,
        &dir,
        "stubborn",
        "trap '' HUP TERM; echo ready; sleep 300",
    );
    let info = b.get(&TerminalRef::new("stubborn")).unwrap();
    let pid = pane_of(&b, &info).pid;
    wait_for_screen(&b, &info, "ready");
    let out = tools::call(
        &sh,
        "remove_pane",
        &args(json!({"terminal": "stubborn", "timeout": 0.5})),
    )
    .unwrap();
    assert_eq!(
        out,
        json!({"name": "stubborn", "was_live": true, "stopped": "killed",
               "exit_code": 128 + libc::SIGKILL})
    );
    assert!(group_gone_within(pid, Duration::from_secs(3)));
    assert!(listed_names(&sh).is_empty());
}

/// A finished pane is removed without signals, with its exit code.
#[test]
fn remove_pane_removes_a_finished_pane() {
    let dir = TempDir::new("native-rmpane-done");
    let (_b, sh) = native_shepherd();
    start_tool(&sh, &dir, "done", "exit 3");
    let wait = tools::call(
        &sh,
        "wait_status",
        &args(json!({"terminal": "done", "timeout": 10})),
    )
    .unwrap();
    assert_eq!(wait["outcome"], "exited");
    let list = tools::call(&sh, "list_panes", &args(json!({}))).unwrap();
    assert_eq!(list["live_panes"], 0);
    assert_eq!(list["hidden_finished"], 1);
    assert_eq!(listed_names(&sh), ["done"]);
    let out = tools::call(&sh, "remove_pane", &args(json!({"terminal": "done"}))).unwrap();
    assert_eq!(
        out,
        json!({"name": "done", "was_live": false, "stopped": null, "exit_code": 3})
    );
    assert!(listed_names(&sh).is_empty());
    let list = tools::call(&sh, "list_panes", &args(json!({}))).unwrap();
    assert_eq!(list["hidden_finished"], 0);

    let err = tools::call(&sh, "remove_pane", &args(json!({"terminal": "nope"}))).unwrap_err();
    assert_eq!(err.data["error"], "not_found");
}

// (h)
#[test]
fn dropping_the_last_handle_kills_live_groups() {
    let dir = TempDir::new("native-drop");
    let b = backend();
    let info = b.run(&spec_line("held", "sleep 300", &dir)).unwrap();
    let pid = pane_of(&b, &info).pid;
    let second = b.clone();
    drop(b);
    assert!(group_alive(pid), "a clone still holds the backend");
    drop(second);
    assert!(group_gone_within(pid, Duration::from_secs(3)));
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn process_gone_within(pid: i32, within: Duration) -> bool {
    let end = Instant::now() + within;
    while Instant::now() < end {
        if !process_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !process_alive(pid)
}

/// Review B3, revised for issue #10: the leader exits while a member of its
/// group that ignores HUP and TERM and has closed the PTY lives on. The
/// leader stays a zombie (the group id ours) while that member gets the
/// grace, then the member is killed and the leader reaped, without any
/// removal; removing the finished pane afterwards reports its exit code.
#[test]
fn a_group_member_that_outlives_the_leader_is_ended() {
    const SURVIVOR: &str = "(trap '' HUP TERM; exec sleep 300 </dev/null >/dev/null 2>&1) & \
                            echo \"bg:$!\"; sleep 1; exit 5";
    let dir = TempDir::new("native-survivor");
    let b = backend();
    let info = b.run(&spec_line("surv", SURVIVOR, &dir)).unwrap();
    let bg = printed_pid(&b, &info, "bg:");
    let pane = pane_of(&b, &info);
    let ended = b.wait(&info.reference(), LONG).unwrap();
    assert_eq!(ended.reason, Some(WaitReason::Exit));
    assert_eq!(ended.exit_code, Some(5));
    thread::sleep(Duration::from_millis(300));
    assert!(process_alive(bg), "the survivor got no grace");
    assert!(
        pane.owns_group(),
        "the leader was reaped before its group ended"
    );
    assert!(
        process_gone_within(bg, Duration::from_secs(5)),
        "the survivor outlived its pane"
    );
    let end = Instant::now() + Duration::from_secs(5);
    while pane.owns_group() && Instant::now() < end {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!pane.owns_group());
    assert!(pane.state.lock().unwrap().reaped);
    assert!(!process_alive(pane.pid), "the leader is still a zombie");
    let out = b.remove(&info.reference(), None).unwrap();
    assert!(!out.was_live);
    assert_eq!(out.exit_code, Some(5));
}

/// Review N4: several keys are one guarded write, so a refusal writes none.
#[test]
fn a_refused_multi_key_call_writes_no_key() {
    let dir = TempDir::new("native-keys-guard");
    let b = backend();
    let cat = b.run(&spec_argv("cat", &["cat"], &dir)).unwrap();
    let guard = InputGuard {
        expect_turn: Some(99),
        ..InputGuard::default()
    };
    let err = b
        .keys(&cat.reference(), &[Key::Up, Key::Down, Key::Enter], &guard)
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::StatusMismatch);
    thread::sleep(Duration::from_millis(300));
    assert!(b.raw_output(&cat.reference()).unwrap().is_empty());
}

// (i)
#[test]
fn bounds_and_spawn_errors() {
    let dir = TempDir::new("native-bounds");
    let b = backend();

    // Name reuse after finish replaces the finished pane.
    let first = b.run(&spec_line("again", "true", &dir)).unwrap();
    b.wait(&first.reference(), LONG).unwrap();
    let second = b.run(&spec_line("again", "true", &dir)).unwrap();
    assert_ne!(first.id, second.id);
    b.wait(&second.reference(), LONG).unwrap();
    let all = b.list(true).unwrap();
    let named = all
        .terminals
        .iter()
        .filter(|p| p.name.as_deref() == Some("again"))
        .count();
    assert_eq!(named, 1);
    let live = b.list(false).unwrap();
    assert!(live.terminals.is_empty());
    assert_eq!(live.hidden_finished, Some(1));

    // A live name is taken.
    b.run(&spec_line("busy0", "sleep 30", &dir)).unwrap();
    let err = b.run(&spec_line("busy0", "sleep 30", &dir)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::NameTaken);

    // The ninth live pane is refused.
    for i in 1..MAX_LIVE_PANES {
        b.run(&spec_line(&format!("busy{i}"), "sleep 30", &dir))
            .unwrap();
    }
    let err = b.run(&spec_line("ninth", "sleep 30", &dir)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(err.detail.contains("the limit is 8"), "{}", err.detail);

    let b = backend();
    let long: Vec<String> = (0..=MAX_ARGV).map(|i| i.to_string()).collect();
    let spec = RunSpec {
        argv: Some(long),
        ..spec_argv("long", &[], &dir)
    };
    assert_eq!(b.run(&spec).unwrap_err().kind, ErrorKind::InvalidInput);
    assert_eq!(
        b.run(&spec_argv("empty", &[], &dir)).unwrap_err().kind,
        ErrorKind::InvalidInput
    );
    assert_eq!(
        b.run(&spec_argv("missing", &["shepherd-no-such-program"], &dir))
            .unwrap_err()
            .kind,
        ErrorKind::NotFound
    );
    let relative = RunSpec {
        cwd: Some("tmp".to_string()),
        ..spec_argv("rel", &["true"], &dir)
    };
    assert_eq!(b.run(&relative).unwrap_err().kind, ErrorKind::InvalidInput);
    let missing_dir = RunSpec {
        cwd: Some("/shepherd-no-such-dir".to_string()),
        ..spec_argv("nodir", &["true"], &dir)
    };
    assert_eq!(b.run(&missing_dir).unwrap_err().kind, ErrorKind::NotFound);
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let not_dir = RunSpec {
        cwd: Some(file.to_string_lossy().into_owned()),
        ..spec_argv("file", &["true"], &dir)
    };
    assert_eq!(b.run(&not_dir).unwrap_err().kind, ErrorKind::InvalidInput);
    // No failed run left a pane behind.
    assert!(b.list(true).unwrap().terminals.is_empty());

    // An unknown key refuses the whole call before any byte is written.
    let cat = b.run(&spec_argv("cat", &["cat"], &dir)).unwrap();
    let err = b
        .keys(
            &cat.reference(),
            &[Key::Up, Key::Other],
            &InputGuard::default(),
        )
        .unwrap_err();
    assert!(matches!(err.kind, ErrorKind::Unsupported(_)));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(screen(&b, &cat), "");
    assert!(b.raw_output(&cat.reference()).unwrap().is_empty());
}

// (j)
#[test]
fn send_input_through_the_tools_returns_pane_blocked() {
    let dir = TempDir::new("native-tools");
    let sh = Shepherd::with_policy(backend(), LaunchPolicy::arbitrary());
    let started = tools::call(
        &sh,
        "start_pane",
        &args(json!({"line": OSC_SCRIPT, "name": "osc", "cwd": dir.path().to_str()})),
    )
    .unwrap();
    assert_eq!(started["name"], "osc");
    let idle = tools::call(
        &sh,
        "wait_status",
        &args(json!({"terminal": "osc", "timeout": 10})),
    )
    .unwrap();
    assert_eq!(idle["status"], "idle", "{idle}");
    tools::call(
        &sh,
        "send_input",
        &args(json!({"terminal": "osc", "text": "go"})),
    )
    .unwrap();
    let blocked = tools::call(
        &sh,
        "wait_status",
        &args(json!({"terminal": "osc", "timeout": 10})),
    )
    .unwrap();
    assert_eq!(blocked["outcome"], "blocked", "{blocked}");
    assert_eq!(blocked["kind"], "permission");
    let status = tools::call(&sh, "get_status", &args(json!({"terminal": "osc"}))).unwrap();
    assert_eq!(
        status["status"],
        json!({"state": "blocked", "kind": "permission"})
    );
    let err = tools::call(
        &sh,
        "send_input",
        &args(json!({"terminal": "osc", "text": "MARKER-TOOLS"})),
    )
    .unwrap_err();
    assert_eq!(err.data["error"], "pane_blocked");
    assert_eq!(err.data["kind"], "permission");
    let read = tools::call(&sh, "read_output", &args(json!({"terminal": "osc"}))).unwrap();
    assert!(!read["text"].as_str().unwrap().contains("MARKER-TOOLS"));
}

/// Review SOL-B2: when the reaper thread cannot start, the run fails, the
/// group is killed and the leader is reaped before the error returns (no
/// zombie), and the pane is not left in the table.
#[test]
fn a_reaper_that_cannot_start_leaves_no_zombie() {
    let dir = TempDir::new("native-reaper-fail");
    let b = backend();
    FAIL_NEXT_THREAD.with(|fail| fail.set(true));
    let err = b.run(&spec_line("doomed", "sleep 300", &dir)).unwrap_err();
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(err.detail.contains("threads"), "{}", err.detail);
    let pid = LAST_SPAWN_PID.with(|last| last.get());
    assert!(pid > 1);
    // Reaped synchronously: a zombie would still answer signal 0.
    assert!(!process_alive(pid), "leader {pid} is not reaped");
    assert!(b.list(true).unwrap().terminals.is_empty());
}

/// Review N9: once the reaper has given up the group id, neither removal
/// nor shutdown signals that group again.
#[test]
fn a_lost_group_is_never_signalled() {
    let dir = TempDir::new("native-group-lost");
    let b = backend();
    let info = b.run(&spec_line("lost", "sleep 300", &dir)).unwrap();
    let pane = pane_of(&b, &info);
    pane.state.lock().unwrap().group_lost = true;
    assert!(!pane.owns_group());
    pane.release();
    thread::sleep(Duration::from_millis(200));
    assert!(group_alive(pane.pid), "release signalled a lost group");
    pty::killpg(pane.pid, libc::SIGKILL).unwrap();
    assert!(group_gone_within(pane.pid, Duration::from_secs(3)));
}

/// Review N12: a child spawned after shutdown has taken the panes is ended
/// at once instead of entering the table.
#[test]
fn a_run_after_shutdown_is_ended() {
    let dir = TempDir::new("native-exiting");
    let b = backend();
    b.shutdown();
    let err = b.run(&spec_line("late", "sleep 300", &dir)).unwrap_err();
    assert!(err.detail.contains("exiting"), "{}", err.detail);
    let pid = LAST_SPAWN_PID.with(|last| last.get());
    assert!(pid > 1);
    assert!(!process_alive(pid), "late child {pid} survived");
    assert!(b.list(true).unwrap().terminals.is_empty());
}

// Issue #10: removed panes left the killed leader unreaped (exit code lost)
// and descendants outside the pane's process group alive.

/// The pid a pane printed after `tag`, e.g. `gc:1234`.
fn printed_pid(b: &NativeBackend, info: &TerminalInfo, tag: &str) -> i32 {
    let text = wait_for_screen(b, info, tag);
    text.lines()
        .find_map(|l| l.trim().strip_prefix(tag))
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("no {tag} pid in {text}"))
}

/// A program that ignores HUP and TERM and keeps writing while nobody else
/// reads: removal kills it, reaps it at once and reports 128 + SIGKILL.
#[test]
fn a_killed_program_that_keeps_writing_is_reaped_with_its_signal_code() {
    let dir = TempDir::new("native-flood-kill");
    let b = backend();
    let info = b
        .run(&spec_line("flood", "trap '' HUP TERM; exec yes", &dir))
        .unwrap();
    let pid = pane_of(&b, &info).pid;
    wait_for_screen(&b, &info, "y\ny");
    let out = b
        .remove(&info.reference(), Some(Duration::from_millis(500)))
        .unwrap();
    assert_eq!(out.stopped, Some(StopHow::Killed), "{out:?}");
    assert_eq!(
        out.exit_code,
        Some(128 + i64::from(libc::SIGKILL)),
        "{out:?}"
    );
    assert!(!process_alive(pid), "leader {pid} was not reaped");
}

/// A program that writes a lot of output when it gets the hangup signal, as
/// a TUI does when it restores the terminal: it exits within the grace,
/// because the output is still drained while it stops.
#[test]
fn a_program_that_writes_on_hangup_exits_within_the_grace() {
    let dir = TempDir::new("native-flood-exit");
    let b = backend();
    let info = b
        .run(&spec_line(
            "tui",
            "trap 'yes | head -c 400000; exit 7' HUP TERM; echo ready; \
             while :; do sleep 1; done",
            &dir,
        ))
        .unwrap();
    let pid = pane_of(&b, &info).pid;
    wait_for_screen(&b, &info, "ready");
    let out = b.remove(&info.reference(), None).unwrap();
    assert_eq!(out.stopped, Some(StopHow::Exited), "{out:?}");
    assert_eq!(out.exit_code, Some(7), "{out:?}");
    assert!(!process_alive(pid), "leader {pid} was not reaped");
}

/// A grandchild that leaves the pane's process group (`setpgid`) or its
/// session (`setsid`, as kiro-cli's node acp-server does: `ps` shows it
/// `Ss` with no terminal), with its stdio closed so it holds no terminal.
fn escapee(how: &str) -> String {
    format!(
        "perl -MPOSIX -e 'my $r = {how}; die if !defined $r || $r == -1; \
         print \"gc:$$\\n\"; \
         close STDIN; close STDOUT; close STDERR; sleep 60'"
    )
}

const ESCAPES: [&str; 2] = ["POSIX::setpgid(0, 0)", "POSIX::setsid()"];

/// Removal ends descendants outside the process group and session.
#[test]
fn remove_ends_descendants_outside_the_group() {
    for how in ESCAPES {
        let dir = TempDir::new("native-escape-rm");
        let b = backend();
        let line = format!("{} & sleep 60", escapee(how));
        let info = b.run(&spec_line("esc", &line, &dir)).unwrap();
        let gc = printed_pid(&b, &info, "gc:");
        thread::sleep(Duration::from_millis(600));
        assert!(process_alive(gc), "{how}: the grandchild died early");
        b.remove(&info.reference(), None).unwrap();
        assert!(
            process_gone_within(gc, Duration::from_secs(5)),
            "{how}: grandchild {gc} outlived remove"
        );
    }
}

/// A grandchild whose parent already exited (reparented to pid 1) and that
/// stayed in the pane's session is ended on removal too.
#[test]
fn remove_ends_a_reparented_descendant_in_the_session() {
    let dir = TempDir::new("native-escape-orphan");
    let b = backend();
    let line = format!("({} &); sleep 60", escapee(ESCAPES[0]));
    let info = b.run(&spec_line("orphan", &line, &dir)).unwrap();
    let gc = printed_pid(&b, &info, "gc:");
    thread::sleep(Duration::from_millis(600));
    assert!(process_alive(gc), "the grandchild died early");
    b.remove(&info.reference(), None).unwrap();
    assert!(
        process_gone_within(gc, Duration::from_secs(5)),
        "grandchild {gc} outlived remove"
    );
}

/// When the pane's program exits on its own, descendants it left behind,
/// in another group or session and reparented to pid 1, are ended and the
/// leader is reaped.
#[test]
fn pane_exit_ends_descendants_outside_the_group() {
    for how in ESCAPES {
        let dir = TempDir::new("native-escape-exit");
        let b = backend();
        let line = format!("{} & sleep 1; exit 4", escapee(how));
        let info = b.run(&spec_line("esc", &line, &dir)).unwrap();
        let pid = pane_of(&b, &info).pid;
        let gc = printed_pid(&b, &info, "gc:");
        let ended = b.wait(&info.reference(), LONG).unwrap();
        assert_eq!(ended.reason, Some(WaitReason::Exit), "{how}");
        assert_eq!(ended.exit_code, Some(4), "{how}");
        assert!(
            process_gone_within(gc, Duration::from_secs(5)),
            "{how}: grandchild {gc} outlived its pane"
        );
        assert!(
            process_gone_within(pid, Duration::from_secs(5)),
            "{how}: leader {pid} was not reaped"
        );
    }
}

/// Shutdown and dropping the last handle end descendants outside the group.
#[test]
fn shutdown_and_drop_end_descendants_outside_the_group() {
    for how in ESCAPES {
        for drop_it in [false, true] {
            let dir = TempDir::new("native-escape-drop");
            let b = backend();
            let line = format!("{} & sleep 60", escapee(how));
            let info = b.run(&spec_line("esc", &line, &dir)).unwrap();
            let pid = pane_of(&b, &info).pid;
            let gc = printed_pid(&b, &info, "gc:");
            thread::sleep(Duration::from_millis(600));
            if drop_it {
                drop(b);
            } else {
                b.shutdown();
            }
            assert!(
                process_gone_within(gc, Duration::from_secs(5)),
                "{how} drop={drop_it}: grandchild {gc} survived"
            );
            assert!(
                process_gone_within(pid, Duration::from_secs(5)),
                "{how} drop={drop_it}: leader {pid} was not reaped"
            );
        }
    }
}

// Review round 3 of #10: a descendant that detaches at once (a daemon's
// double fork: it calls setsid and every ancestor between it and the pane
// exits within milliseconds) leaves no ppid, process-group, session or
// terminal link to the pane before any process-table look can see it. The
// marker in its environment (ADR §11) is then the only way to find it. No
// delay before cleanup in these tests: cleanup must find it by the marker
// alone.

/// A helper that detaches fully, then prints `gc:<pid>` and sleeps: it
/// `setsid`s into a new session, forks, the leader exits (so the printing
/// process is reparented to pid 1 in a session it does not lead and with no
/// controlling terminal), and the child closes its stdio and sleeps. It is
/// a locally built, unsigned binary, so the marker it inherits is visible on
/// macOS (`KERN_PROCARGS2` hides the environment of SIP-restricted binaries
/// such as /bin/sh or perl, not of a binary like this one). Returns its
/// path; the caller runs it through a pane so it inherits the pane marker.
fn detach_helper(dir: &TempDir) -> String {
    let src = dir.path().join("detach.c");
    let bin = dir.path().join("detach");
    std::fs::write(
        &src,
        "#include <unistd.h>\n#include <stdio.h>\n#include <sys/types.h>\n\
         int main(void){\n\
         if (setsid() == -1) return 1;\n\
         pid_t p = fork();\n\
         if (p < 0) return 1;\n\
         if (p > 0) _exit(0);\n\
         printf(\"gc:%d\\n\", (int)getpid()); fflush(stdout);\n\
         close(0); close(1); close(2);\n\
         sleep(300);\n\
         return 0;\n}\n",
    )
    .unwrap();
    let ok = Command::new("cc")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .status()
        .expect("cc to build the detach helper")
        .success();
    assert!(ok, "the detach helper did not compile");
    bin.to_string_lossy().into_owned()
}

/// Wait for `gc` to end; kill it if it did not, so a failure leaves nothing.
fn detached_ended(gc: i32) -> bool {
    let gone = process_gone_within(gc, Duration::from_secs(5));
    if !gone {
        // SAFETY: a test's own grandchild, still alive a moment ago.
        unsafe {
            libc::kill(gc, libc::SIGKILL);
        }
    }
    gone
}

#[test]
fn remove_ends_a_descendant_that_detached_at_once() {
    let dir = TempDir::new("native-fast-rm");
    let b = backend();
    let line = format!("{} & sleep 60", detach_helper(&dir));
    let info = b.run(&spec_line("fast", &line, &dir)).unwrap();
    let gc = printed_pid(&b, &info, "gc:");
    b.remove(&info.reference(), None).unwrap();
    assert!(detached_ended(gc), "grandchild {gc} outlived remove");
}

#[test]
fn pane_exit_ends_a_descendant_that_detached_at_once() {
    let dir = TempDir::new("native-fast-exit");
    let b = backend();
    let line = format!("{} & sleep 1; exit 4", detach_helper(&dir));
    let info = b.run(&spec_line("fast", &line, &dir)).unwrap();
    let gc = printed_pid(&b, &info, "gc:");
    let ended = b.wait(&info.reference(), LONG).unwrap();
    assert_eq!(ended.exit_code, Some(4));
    assert!(detached_ended(gc), "grandchild {gc} outlived its pane");
}

#[test]
fn shutdown_and_drop_end_a_descendant_that_detached_at_once() {
    for drop_it in [false, true] {
        let dir = TempDir::new("native-fast-drop");
        let b = backend();
        let line = format!("{} & sleep 60", detach_helper(&dir));
        let info = b.run(&spec_line("fast", &line, &dir)).unwrap();
        let gc = printed_pid(&b, &info, "gc:");
        if drop_it {
            drop(b);
        } else {
            b.shutdown();
        }
        assert!(
            detached_ended(gc),
            "drop={drop_it}: grandchild {gc} survived"
        );
    }
}

// Linux environment-scrubbing descendants (design review
// .agents/tasks/pr11-design-review.md, escape table). Each test pins the
// behavior observed on Linux, not a wish.

/// Like `detach_helper`, but the detached child changes its own environment
/// before it prints `gc:<pid>`: mode `clearenv` calls clearenv(3); mode
/// `fresh` execs `env -i sleep 300`, which keeps the pid but starts from an
/// empty environment.
#[cfg(target_os = "linux")]
fn scrub_helper(dir: &TempDir) -> String {
    let src = dir.path().join("scrub.c");
    let bin = dir.path().join("scrub");
    std::fs::write(
        &src,
        "#define _GNU_SOURCE\n#include <stdlib.h>\n#include <string.h>\n\
         #include <unistd.h>\n#include <stdio.h>\n#include <sys/types.h>\n\
         int main(int argc, char **argv){\n\
         if (argc < 2) return 1;\n\
         if (setsid() == -1) return 1;\n\
         pid_t p = fork();\n\
         if (p < 0) return 1;\n\
         if (p > 0) _exit(0);\n\
         printf(\"gc:%d\\n\", (int)getpid()); fflush(stdout);\n\
         close(0); close(1); close(2);\n\
         if (strcmp(argv[1], \"clearenv\") == 0) { clearenv(); sleep(300); return 0; }\n\
         execl(\"/usr/bin/env\", \"env\", \"-i\", \"sleep\", \"300\", (char *)0);\n\
         return 1;\n}\n",
    )
    .unwrap();
    let ok = Command::new("cc")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .status()
        .expect("cc to build the scrub helper")
        .success();
    assert!(ok, "the scrub helper did not compile");
    bin.to_string_lossy().into_owned()
}

// Review table row 6: clearenv() in the descendant. Observed on Linux:
// caught, because /proc/<pid>/environ still reads the original env region.
#[cfg(target_os = "linux")]
#[test]
fn a_descendant_that_calls_clearenv_is_caught() {
    let dir = TempDir::new("native-clearenv");
    let b = backend();
    let line = format!("{} clearenv & sleep 60", scrub_helper(&dir));
    let info = b.run(&spec_line("clr", &line, &dir)).unwrap();
    let gc = printed_pid(&b, &info, "gc:");
    b.remove(&info.reference(), None).unwrap();
    assert!(detached_ended(gc), "grandchild {gc} outlived remove");
}

// Review table row 7: execve with a fresh environment (`env -i`). Known
// escape: the new image carries no marker, so the sweep cannot find it.
#[cfg(target_os = "linux")]
#[test]
fn a_descendant_started_with_env_i_is_a_known_escape() {
    let dir = TempDir::new("native-env-i");
    let b = backend();
    let line = format!("{} fresh & sleep 60", scrub_helper(&dir));
    let info = b.run(&spec_line("envi", &line, &dir)).unwrap();
    let gc = printed_pid(&b, &info, "gc:");
    b.remove(&info.reference(), None).unwrap();
    let survived = !process_gone_within(gc, Duration::from_secs(3));
    // SAFETY: a test's own grandchild; clean up whatever the sweep missed.
    unsafe {
        libc::kill(gc, libc::SIGKILL);
    }
    assert!(survived, "grandchild {gc} was ended: the escape is closed");
}
