//! Tool-level tests over the backend trait: each drives an MCP message through
//! Shepherd against a scripted or stateful in-repo backend (`testutil::scripted`),
//! then checks the backend calls made and the MCP reply. They assert what the
//! tools do for any backend, not what one backend's wire looks like.

use crate::agent::Agent;
use crate::backend::{
    ErrorKind, InputGuard, InterruptOutcome, InterruptResult, Key, ProgramStatus, ReadLimit,
    ReadSource, RemoveResult, RunSpec, ScreenText, SendResult, State, StopHow, TerminalBackend,
    TerminalInfo, TerminalList, WaitReason, WaitResult,
};
use crate::dialog::fixtures::{dialog_on_allow, dialog_on_deny, PROBE_HEAD, PROBE_TAIL};
use crate::mcp::handle_message;
use crate::profiles::LaunchPolicy;
use crate::testutil::scripted::{
    blocked_pane, fail, finished_pane, get, interrupt_ok, keys_ok, keys_sent, list_ok, methods,
    pane, pane_at, pane_with_status, read_ok, remove_ok, run_ok, screen, send_ok, status, wait_ok,
    Call, PaneBackend, ScriptedBackend, Step, PANE_ID, PANE_NAME,
};
use crate::testutil::TempDir;
use crate::tools::{self, Shepherd};
use serde_json::{json, Value};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

fn call_msg(name: &str, arguments: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
           "params": {"name": name, "arguments": arguments}})
}

/// The tool result object from a `tools/call` reply.
fn tool_out(reply: Option<Value>) -> Value {
    reply.expect("reply")["result"].clone()
}

fn run_tool(sh: &Shepherd, name: &str, arguments: Value) -> Value {
    tool_out(handle_message(sh, &call_msg(name, arguments)))
}

fn shepherd(backend: &(impl TerminalBackend + Clone + 'static)) -> Shepherd {
    let sh = Shepherd::with_policy(backend.clone(), LaunchPolicy::arbitrary());
    // These panes stand for kiro-cli panes started from a profile.
    sh.assume_agent("work", Agent::Kiro);
    sh
}

/// A backend like the one the tools are written for: it refuses typing into a
/// blocked pane itself, and has no atomic key guard, so Shepherd's own
/// pre-Enter re-check runs.
fn scripted(steps: Vec<Step>) -> ScriptedBackend {
    ScriptedBackend::new(true, false, steps)
}

fn text_of(out: &Value) -> String {
    out["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn answer(sh: &Shepherd, choice: &str) -> Value {
    run_tool(
        sh,
        "answer_dialog",
        json!({"terminal": "work", "choice": choice}),
    )
}

fn blocked_get(kind: &str) -> Step {
    get(blocked_pane(kind))
}

fn get_at(id: &str) -> Step {
    get(pane_at(id))
}

fn get_with_status(state: State) -> Step {
    get(pane_with_status(state, None))
}

fn guard_blocked() -> InputGuard {
    InputGuard {
        refuse_when_blocked: true,
        ..InputGuard::default()
    }
}

fn wait_result(
    ended: bool,
    reason: Option<WaitReason>,
    state: Option<State>,
    turn: u64,
) -> WaitResult {
    WaitResult {
        ended,
        reason,
        status: state.map(|state| ProgramStatus { state, kind: None }),
        exit_code: None,
        turn: Some(turn),
    }
}

#[test]
fn start_pane_passes_clean_line_to_the_backend() {
    let backend = scripted(vec![run_ok(TerminalInfo {
        name: None,
        ..pane("t1", Some(status(State::Idle, None)))
    })]);
    let out = run_tool(
        &shepherd(&backend),
        "start_pane",
        json!({"line": "  echo hi  "}),
    );
    let log = backend.finish();
    assert_eq!(out["isError"], false);
    let Call::Run(spec) = &log[0] else {
        panic!("run expected, got {log:?}");
    };
    // No name given: Shepherd picks one, sends it, and returns it.
    let name = spec.name.clone().unwrap();
    assert!(crate::input::validate_name(&name).is_ok(), "{name}");
    assert!(name.starts_with("pane-") && name.len() == 17, "{name}");
    assert_eq!(
        *spec,
        RunSpec {
            line: Some("echo hi".to_string()),
            name: Some(name.clone()),
            ..RunSpec::default()
        }
    );
    assert_eq!(out["structuredContent"]["name"], json!(name));
    // The backend's id is not part of the pane object.
    assert!(out["structuredContent"].get("id").is_none());
    assert!(!text_of(&out).contains("t1"));
}

#[test]
fn send_input_passes_the_text_with_a_blocked_guard() {
    let backend = scripted(vec![
        get_at("t1"),
        get_at("t1"),
        // The idle or done status is cross-checked against the screen (issue 62).
        read_ok(plain_screen("> ready\n")),
        send_ok(SendResult {
            turn: Some(3),
            started: Some(true),
            status: None,
        }),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "send_input",
        json!({"terminal": "work", "text": "echo hi"}),
    );
    let log = backend.finish();
    assert_eq!(out["isError"], false);
    assert_eq!(methods(&log), ["get", "get", "read", "send"]);
    assert_eq!(
        log[3],
        Call::Send {
            id: "t1".to_string(),
            text: "echo hi".to_string(),
            guard: guard_blocked(),
        }
    );
}

#[test]
fn read_output_strips_escapes_from_text() {
    let backend = scripted(vec![
        get_at("t1"),
        read_ok(ScreenText {
            text: "\u{1b}]7501;state=done\u{7}\u{1b}[32mok\u{1b}[0m".to_string(),
            complete: Some(true),
            source: Some(ReadSource::Screen),
            limit: None,
        }),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "read_output",
        json!({"terminal": "work"}),
    );
    let log = backend.finish();
    assert_eq!(methods(&log), ["get", "read"]);
    // The name resolves the pane; the read goes to its id.
    assert_eq!(log[0], Call::Get("work".to_string()));
    assert_eq!(log[1], Call::Read("t1".to_string(), 0));
    assert_eq!(out["structuredContent"]["text"], "ok");
    assert_eq!(out["structuredContent"]["source"], "screen");
    assert_eq!(out["structuredContent"]["reason"], Value::Null);
}

#[test]
fn wait_status_silent_reply_is_an_error_not_idle() {
    let backend = scripted(vec![
        get_at("t1"),
        fail("wait", ErrorKind::Protocol, "no ended flag in the reply"),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "wait_status",
        json!({"terminal": "work", "timeout": 5}),
    );
    backend.finish();
    assert_eq!(out["isError"], true);
    assert!(text_of(&out).contains("not treated as idle"));
}

#[test]
fn wait_status_timeout_reports_ended_false() {
    let result = wait_result(false, Some(WaitReason::Timeout), None, 2);
    let backend = scripted(vec![get_at("t1"), wait_ok(result)]);
    let out = run_tool(
        &shepherd(&backend),
        "wait_status",
        json!({"terminal": "work", "timeout": 5}),
    );
    let log = backend.finish();
    assert_eq!(log[1], Call::Wait("t1".to_string(), Duration::from_secs(5)));
    assert_eq!(out["structuredContent"]["terminal"], "work");
    assert_eq!(out["structuredContent"]["ended"], false);
    assert_eq!(out["structuredContent"]["reason"], "timeout");
    assert_eq!(out["structuredContent"]["outcome"], "timeout");
    assert_eq!(out["structuredContent"]["turn_complete"], false);
}

#[test]
fn list_panes_passes_all_flag() {
    let backend = scripted(vec![list_ok(TerminalList {
        terminals: vec![],
        hidden_finished: Some(0),
        live_limit: None,
    })]);
    let out = run_tool(&shepherd(&backend), "list_panes", json!({"all": true}));
    let log = backend.finish();
    assert_eq!(log, [Call::List(true)]);
    assert_eq!(out["structuredContent"]["hidden_finished"], 0);
    assert_eq!(out["structuredContent"]["panes"], json!([]));
}

#[test]
fn interrupt_pane_maps_to_interrupt() {
    let backend = scripted(vec![
        get_at("t1"),
        get_at("t1"),
        interrupt_ok(InterruptResult {
            key: Key::Esc,
            outcome: InterruptOutcome::Stopped,
        }),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "interrupt_pane",
        json!({"terminal": "work"}),
    );
    let log = backend.finish();
    assert_eq!(methods(&log), ["get", "get", "interrupt"]);
    assert_eq!(log[2], Call::Interrupt("t1".to_string(), None));
    assert_eq!(out["structuredContent"]["outcome"], "stopped");
}

/// remove_pane on a live pane: the two lookups, then one `remove` by id. How
/// the backend reports the end maps to Shepherd's words.
#[test]
fn remove_pane_stops_a_live_terminal_then_removes_it() {
    for (how, exit_code, stopped) in [
        (StopHow::Killed, None, "killed"),
        (StopHow::Exited, None, "exited"),
        (StopHow::Exited, Some(0), "exited"),
        (StopHow::Other, None, "other"),
    ] {
        let label = format!("{how:?} {exit_code:?}");
        let backend = scripted(vec![
            get_at("t8"),
            get_at("t8"),
            remove_ok(RemoveResult {
                was_live: true,
                stopped: Some(how),
                exit_code,
            }),
        ]);
        let out = run_tool(
            &shepherd(&backend),
            "remove_pane",
            json!({"terminal": "work", "timeout": 5}),
        );
        let log = backend.finish();
        assert_eq!(methods(&log), ["get", "get", "remove"], "{label}");
        assert_eq!(
            log[2],
            Call::Remove("t8".to_string(), Some(Duration::from_secs(5))),
            "{label}"
        );
        assert_eq!(out["isError"], false, "{label}: {}", text_of(&out));
        assert_eq!(
            out["structuredContent"],
            json!({"name": "work", "was_live": true, "stopped": stopped, "exit_code": exit_code}),
            "{label}"
        );
        assert_no_vendor_strings(&out);
        assert!(!out.to_string().contains("t8"), "{out}");
    }
}

/// A finished terminal is removed without a stop.
#[test]
fn remove_pane_removes_a_finished_terminal_without_stopping_it() {
    let backend = scripted(vec![
        get(finished_pane()),
        get(finished_pane()),
        remove_ok(RemoveResult {
            was_live: false,
            stopped: None,
            exit_code: Some(2),
        }),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "remove_pane",
        json!({"terminal": "work"}),
    );
    let log = backend.finish();
    assert_eq!(methods(&log), ["get", "get", "remove"]);
    assert_eq!(
        out["structuredContent"],
        json!({"name": "work", "was_live": false, "stopped": null, "exit_code": 2})
    );
    assert_no_vendor_strings(&out);
}

#[test]
fn wait_status_blocked_reports_outcome_and_kind_from_get() {
    let backend = scripted(vec![
        blocked_get("permission"),
        wait_ok(wait_result(
            true,
            Some(WaitReason::Status),
            Some(State::Blocked),
            1,
        )),
        blocked_get("permission"),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "wait_status",
        json!({"terminal": "work", "timeout": 5}),
    );
    let log = backend.finish();
    assert_eq!(methods(&log), ["get", "wait", "get"]);
    assert_eq!(log[2], Call::Get("t8".to_string()));
    let got = &out["structuredContent"];
    assert_eq!(out["isError"], false);
    assert_eq!(got["outcome"], "blocked");
    assert_eq!(got["kind"], "permission");
    assert_eq!(got["turn_complete"], false);
    assert_eq!(got["status"], "blocked");
    assert_eq!(got["status_now"]["state"], "blocked");
}

#[test]
fn wait_status_done_is_turn_complete_without_extra_call() {
    let backend = scripted(vec![
        get_at("t8"),
        wait_ok(wait_result(
            true,
            Some(WaitReason::Status),
            Some(State::Done),
            2,
        )),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "wait_status",
        json!({"terminal": "work", "timeout": 5}),
    );
    let log = backend.finish();
    // The lookup, then the wait; no get after it.
    assert_eq!(methods(&log), ["get", "wait"]);
    let got = &out["structuredContent"];
    assert_eq!(got["outcome"], "done");
    assert_eq!(got["turn_complete"], true);
    assert_eq!(got["kind"], Value::Null);
}

#[test]
fn wait_status_exit_and_other_status_words() {
    let exited = WaitResult {
        exit_code: Some(0),
        ..wait_result(true, Some(WaitReason::Exit), None, 3)
    };
    for (result, outcome, complete) in [
        (exited, "exited", true),
        (
            wait_result(true, Some(WaitReason::Status), Some(State::Idle), 3),
            "other",
            false,
        ),
        (
            wait_result(true, Some(WaitReason::Status), Some(State::Error), 3),
            "other",
            false,
        ),
        (
            wait_result(false, Some(WaitReason::Other), Some(State::Working), 3),
            "other",
            false,
        ),
    ] {
        let label = format!("{result:?}");
        let backend = scripted(vec![get_at("t8"), wait_ok(result)]);
        let out = run_tool(
            &shepherd(&backend),
            "wait_status",
            json!({"terminal": "work", "timeout": 5}),
        );
        backend.finish();
        assert_eq!(out["structuredContent"]["outcome"], outcome, "{label}");
        assert_eq!(
            out["structuredContent"]["turn_complete"], complete,
            "{label}"
        );
    }
}

#[test]
fn send_input_is_sent_when_pane_is_done() {
    let backend = scripted(vec![
        get_at("t8"),
        get_at("t8"),
        // The idle or done status is cross-checked against the screen (issue 62).
        read_ok(plain_screen("> ready\n")),
        send_ok(SendResult {
            turn: Some(2),
            started: Some(true),
            status: Some(State::Done),
        }),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "send_input",
        json!({"terminal": "work", "text": "Please retry"}),
    );
    let log = backend.finish();
    assert_eq!(out["isError"], false);
    assert_eq!(out["structuredContent"]["status"], "done");
    assert_eq!(
        log[3],
        Call::Send {
            id: "t8".to_string(),
            text: "Please retry".to_string(),
            guard: guard_blocked(),
        }
    );
}

/// Issue #62: the pane says done, the screen shows its permission dialog. No
/// send reaches the backend, and the refusal is in Shepherd's words.
#[test]
fn send_input_refuses_a_done_pane_whose_screen_shows_the_dialog() {
    let allow = dialog_on_allow();
    let backend = scripted(vec![get_at("t8"), get_at("t8"), screen(&allow)]);
    let out = run_tool(
        &shepherd(&backend),
        "send_input",
        json!({"terminal": "work", "text": "Please retry"}),
    );
    let log = backend.finish();
    assert_eq!(methods(&log), ["get", "get", "read"]);
    assert_eq!(out["isError"], true);
    let data = &out["structuredContent"];
    assert_eq!(data["error"], "refused");
    assert_eq!(data["terminal"], "work");
    assert!(data["message"]
        .as_str()
        .unwrap()
        .contains("screen still shows a permission dialog"));
    assert_no_vendor_strings(&out);
}

#[test]
fn answer_dialog_allow_once_needs_no_moves() {
    let backend = scripted(vec![
        blocked_get("permission"),
        blocked_get("permission"),
        screen(&dialog_on_allow()),
        screen(&dialog_on_allow()),
        blocked_get("permission"),
        keys_ok(State::Working),
    ]);
    let out = answer(&shepherd(&backend), "allow_once");
    let log = backend.finish();
    assert_eq!(out["isError"], false, "{}", text_of(&out));
    assert_eq!(methods(&log), ["get", "get", "read", "read", "get", "keys"]);
    assert_eq!(keys_sent(&log), [["enter"]]);
    let got = &out["structuredContent"];
    assert_eq!(got["choice"], "allow_once");
    assert_eq!(got["selected"], "Allow once");
    assert_eq!(got["moves"], 0);
    assert_eq!(
        got["title"],
        "Create an empty marker file requires approval"
    );
    assert_eq!(got["command"], "shell → touch shepherd_marker.txt");
}

#[test]
fn answer_dialog_deny_once_moves_down_once_then_enter() {
    let backend = scripted(vec![
        blocked_get("permission"),
        blocked_get("permission"),
        screen(&dialog_on_allow()),
        keys_ok(State::Blocked),
        screen(&dialog_on_deny()),
        blocked_get("permission"),
        keys_ok(State::Working),
    ]);
    let out = answer(&shepherd(&backend), "deny_once");
    let log = backend.finish();
    assert_eq!(out["isError"], false, "{}", text_of(&out));
    assert_eq!(keys_sent(&log), [vec!["down"], vec!["enter"]]);
    assert_eq!(out["structuredContent"]["moves"], 1);
    assert_eq!(out["structuredContent"]["selected"], "Deny once");
}

#[test]
fn answer_dialog_refuses_when_not_blocked() {
    let backend = scripted(vec![
        get_with_status(State::Done),
        get_with_status(State::Done),
    ]);
    let out = answer(&shepherd(&backend), "allow_once");
    let log = backend.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(methods(&log), ["get", "get"]);
    assert_eq!(out["structuredContent"]["error"], "refused");
    assert_eq!(out["structuredContent"]["seen"]["status"]["state"], "done");
    assert!(text_of(&out).contains("not blocked on a permission dialog"));
}

#[test]
fn answer_dialog_refuses_when_kind_is_not_permission() {
    let backend = scripted(vec![blocked_get("question"), blocked_get("question")]);
    let out = answer(&shepherd(&backend), "allow_once");
    let log = backend.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(methods(&log), ["get", "get"]);
    assert!(text_of(&out).contains("\"question\""), "{}", text_of(&out));
}

#[test]
fn answer_dialog_refuses_when_options_do_not_parse() {
    let broken = format!("{PROBE_HEAD} ❯ Yes\n   No\n{PROBE_TAIL}");
    let backend = scripted(render_wait_script(&broken, tools::RENDER_READS));
    let out = answer(&shepherd(&backend), "deny_once");
    let log = backend.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(log.len(), 2 + 2 * tools::RENDER_READS - 1);
    assert!(keys_sent(&log).is_empty());
    assert!(text_of(&out).contains("cannot parse the permission dialog"));
    assert!(out["structuredContent"]["seen"]["screen"]
        .as_str()
        .unwrap()
        .contains("❯ Yes"));
}

/// The screen kiro-cli shows right after it reports blocked/permission: the
/// tool call line is drawn, the dialog box is not (measured, e2e run).
const BEFORE_DIALOG: &str = "  Use the shell tool to run exactly this command in the current directory and nothing else: touch shepherd_marker.txt

↓ Shell touch shepherd_marker.txt
";

/// The two lock gets, then `reads` reads of `text` with a blocked get before
/// every read after the first.
fn render_wait_script(text: &str, reads: usize) -> Vec<Step> {
    let mut script = vec![blocked_get("permission"), blocked_get("permission")];
    for read in 0..reads {
        if read > 0 {
            script.push(blocked_get("permission"));
        }
        script.push(screen(text));
    }
    script
}

#[test]
fn answer_dialog_waits_for_the_dialog_to_render() {
    let mut script = render_wait_script(BEFORE_DIALOG, 2);
    script.extend([
        blocked_get("permission"),
        screen(&dialog_on_allow()),
        screen(&dialog_on_allow()),
        blocked_get("permission"),
        keys_ok(State::Working),
    ]);
    let backend = scripted(script);
    let out = answer(&shepherd(&backend), "allow_once");
    let log = backend.finish();
    assert_eq!(out["isError"], false, "{}", text_of(&out));
    assert_eq!(
        methods(&log),
        ["get", "get", "read", "get", "read", "get", "read", "read", "get", "keys"]
    );
    assert_eq!(keys_sent(&log), [["enter"]]);
    assert_eq!(out["structuredContent"]["selected"], "Allow once");
}

#[test]
fn answer_dialog_refuses_when_the_dialog_never_renders() {
    let backend = scripted(render_wait_script(BEFORE_DIALOG, tools::RENDER_READS));
    let out = answer(&shepherd(&backend), "allow_once");
    let log = backend.finish();
    assert_eq!(out["isError"], true);
    // Every scripted reply used, nothing after the last read.
    assert_eq!(log.len(), 2 + 2 * tools::RENDER_READS - 1);
    assert_eq!(log.last().unwrap().method(), "read");
    assert!(keys_sent(&log).is_empty());
    assert_eq!(out["structuredContent"]["keys_sent"], json!([]));
    let text = text_of(&out);
    assert!(
        text.contains("timed out waiting for the dialog to render"),
        "{text}"
    );
    assert!(text.contains("requires approval"), "{text}");
    assert!(text.contains("Enter was not sent"), "{text}");
}

#[test]
fn answer_dialog_stops_waiting_when_the_pane_changes() {
    let cases = [
        (
            get_with_status(State::Done),
            "not blocked with kind permission",
        ),
        (
            get(TerminalInfo {
                turn: Some(2),
                ..blocked_pane("permission")
            }),
            "the turn is now 2, not 1",
        ),
    ];
    for (changed, expected) in cases {
        let mut script = render_wait_script(BEFORE_DIALOG, 2);
        // The pane changes before the third read; a dialog would be there.
        script.push(changed);
        let backend = scripted(script);
        let out = answer(&shepherd(&backend), "allow_once");
        let log = backend.finish();
        assert_eq!(out["isError"], true, "{expected}");
        assert_eq!(
            methods(&log),
            ["get", "get", "read", "get", "read", "get"],
            "nothing after the changed get: {expected}"
        );
        assert!(keys_sent(&log).is_empty(), "{expected}");
        let text = text_of(&out);
        assert!(
            text.contains("changed while waiting for the dialog to render"),
            "{text}"
        );
        assert!(text.contains(expected), "{text}");
        assert!(text.contains("Enter was not sent"), "{text}");
    }
}

#[test]
fn answer_dialog_sends_no_enter_when_cursor_does_not_land() {
    let mut script = vec![
        blocked_get("permission"),
        blocked_get("permission"),
        screen(&dialog_on_allow()),
        keys_ok(State::Blocked),
    ];
    // The cursor never leaves Allow once.
    script.extend((0..tools::CONFIRM_READS).map(|_| screen(&dialog_on_allow())));
    let backend = scripted(script);
    let out = answer(&shepherd(&backend), "deny_once");
    let log = backend.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(keys_sent(&log), [["down"]]);
    assert_eq!(log.len(), 4 + tools::CONFIRM_READS);
    assert_eq!(out["structuredContent"]["keys_sent"], json!([["down"]]));
    let text = text_of(&out);
    assert!(text.contains("not confirmed on \"Deny once\""), "{text}");
    assert!(text.contains("Enter was not sent"), "{text}");
}

#[test]
fn answer_dialog_sends_no_enter_when_status_or_turn_changes_before_it() {
    let cases = [
        (
            get_with_status(State::Working),
            "not blocked with kind permission",
        ),
        (
            get(TerminalInfo {
                turn: Some(2),
                ..blocked_pane("permission")
            }),
            "the turn is now 2, not 1",
        ),
        (blocked_get("question"), "not blocked with kind permission"),
    ];
    for (recheck, expected) in cases {
        let backend = scripted(vec![
            blocked_get("permission"),
            blocked_get("permission"),
            screen(&dialog_on_allow()),
            keys_ok(State::Blocked),
            screen(&dialog_on_deny()),
            recheck,
        ]);
        let out = answer(&shepherd(&backend), "deny_once");
        let log = backend.finish();
        assert_eq!(out["isError"], true, "{expected}");
        assert_eq!(log.len(), 6, "nothing after the re-check: {expected}");
        assert_eq!(log[5].method(), "get");
        assert_eq!(keys_sent(&log), [["down"]], "no enter");
        let text = text_of(&out);
        assert!(
            text.contains("changed after the cursor was confirmed"),
            "{text}"
        );
        assert!(text.contains(expected), "{text}");
        assert!(text.contains("Enter was not sent"), "{text}");
    }
}

#[test]
fn locked_tool_refuses_when_name_moves_to_another_terminal() {
    let backend = scripted(vec![get_at("t8"), get_at("t9")]);
    let out = run_tool(
        &shepherd(&backend),
        "send_input",
        json!({"terminal": "work", "text": "hi"}),
    );
    let log = backend.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(methods(&log), ["get", "get"]);
    assert_eq!(out["structuredContent"]["error"], "terminal_changed");
    assert!(text_of(&out).contains("nothing was sent"));
}

/// A backend may resolve its own ids as well as a name. Shepherd must not: a
/// pane with no name, or a named pane given by its id, is not found, and only
/// the lookup reaches the backend.
#[test]
fn backend_ids_do_not_reach_panes_through_mcp() {
    let lookup = |named: bool| {
        if named {
            get_at("t8")
        } else {
            get(TerminalInfo {
                name: None,
                ..pane_at("t1")
            })
        }
    };
    let tools: [(&str, Value); 7] = [
        ("send_input", json!({"text": "hi"})),
        ("read_output", json!({})),
        ("get_status", json!({})),
        ("wait_status", json!({"timeout": 1})),
        ("interrupt_pane", json!({})),
        ("answer_dialog", json!({"choice": "allow_once"})),
        ("remove_pane", json!({})),
    ];
    // An unnamed pane by its id, then the pane named work by its id.
    for (id, named) in [("t1", false), ("t8", true)] {
        let backend = scripted((0..tools.len()).map(|_| lookup(named)).collect());
        let client = shepherd(&backend);
        for (tool, extra) in &tools {
            let mut arguments = extra.clone();
            arguments["terminal"] = json!(id);
            let out = run_tool(&client, tool, arguments);
            let label = format!("{tool} {id}");
            assert_eq!(out["isError"], true, "{label}");
            assert_eq!(out["structuredContent"]["error"], "not_found", "{label}");
            assert!(
                text_of(&out).contains(&format!("no pane is named {id}")),
                "{label}: {}",
                text_of(&out)
            );
            assert_no_vendor_strings(&out);
        }
        let log = backend.finish();
        assert_eq!(methods(&log), vec!["get"; tools.len()]);
    }

    // By name, the same pane works and results name it, not its id.
    let backend = scripted(vec![
        get_at("t8"),
        get_at("t8"),
        get_at("t8"),
        // The idle or done status is cross-checked against the screen (issue 62).
        read_ok(plain_screen("> ready\n")),
        send_ok(SendResult {
            turn: Some(2),
            started: Some(true),
            status: Some(State::Done),
        }),
        get_at("t8"),
        wait_ok(wait_result(
            true,
            Some(WaitReason::Status),
            Some(State::Done),
            2,
        )),
    ]);
    let client = shepherd(&backend);
    for (tool, arguments) in [
        ("get_status", json!({"terminal": "work"})),
        ("send_input", json!({"terminal": "work", "text": "hi"})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
    ] {
        let out = run_tool(&client, tool, arguments);
        assert_eq!(out["isError"], false, "{tool}: {}", text_of(&out));
        assert_eq!(out["structuredContent"]["terminal"], "work", "{tool}");
        assert!(!out.to_string().contains("t8"), "{tool}: {out}");
    }
    backend.finish();
}

/// Wait until a call is parked on the pane lock for `id`. The deadline only
/// turns a hang into a failure; the order of events does not depend on it.
fn wait_for_lock_waiter(client: &Shepherd, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.pane_locks().waiting(id) == 0 {
        assert!(
            Instant::now() < deadline,
            "no call waited for the {id} lock"
        );
        thread::yield_now();
    }
}

fn spawn_call(client: &Shepherd, name: &str, arguments: Value) -> thread::JoinHandle<Value> {
    let client = client.clone();
    let message = call_msg(name, arguments);
    thread::spawn(move || tool_out(handle_message(&client, &message)))
}

#[test]
fn concurrent_answer_dialog_calls_do_not_interleave() {
    // Hold the reply to the 2nd read: call A's confirming read.
    let backend = PaneBackend::new(Some(2));
    let client = shepherd(&backend);
    let a = spawn_call(
        &client,
        "answer_dialog",
        json!({"terminal": PANE_NAME, "choice": "allow_once"}),
    );
    backend.wait_paused();
    // A holds the lock and sits between its confirming read and Enter.
    let b = spawn_call(
        &client,
        "answer_dialog",
        json!({"terminal": PANE_NAME, "choice": "deny_once"}),
    );
    wait_for_lock_waiter(&client, PANE_ID);
    backend.release();
    let out_a = a.join().unwrap();
    let out_b = b.join().unwrap();
    let (log, selected) = backend.finish();

    assert_eq!(out_a["isError"], false, "{}", text_of(&out_a));
    assert_eq!(out_a["structuredContent"]["selected"], "Allow once");
    assert_eq!(out_b["isError"], true);
    assert!(
        text_of(&out_b).contains("not blocked on a permission dialog"),
        "{}",
        text_of(&out_b)
    );
    assert_eq!(out_b["structuredContent"]["keys_sent"], json!([]));
    // One Enter, on the option A confirmed.
    assert_eq!(selected, [0]);
    // A ran get, get, read, read(confirm), get(re-check), Enter with nothing of
    // B's in between except B's lookup before the lock. B's own get came after.
    assert_eq!(
        methods(&log),
        ["get", "get", "read", "read", "get", "get", "keys", "get"]
    );
    assert_eq!(keys_sent(&log), [["enter"]]);
}

#[test]
fn different_tools_on_one_pane_share_its_lock() {
    let backend = PaneBackend::new(Some(2));
    let client = shepherd(&backend);
    let a = spawn_call(
        &client,
        "answer_dialog",
        json!({"terminal": PANE_NAME, "choice": "allow_once"}),
    );
    backend.wait_paused();
    // B is another tool on the same pane; it must wait on the lock A holds
    // for the pane's id.
    let b = spawn_call(
        &client,
        "send_input",
        json!({"terminal": PANE_NAME, "text": "next prompt"}),
    );
    wait_for_lock_waiter(&client, PANE_ID);
    backend.release();
    let out_a = a.join().unwrap();
    let out_b = b.join().unwrap();
    let (log, selected) = backend.finish();

    assert_eq!(out_a["isError"], false, "{}", text_of(&out_a));
    assert_eq!(out_b["isError"], false, "{}", text_of(&out_b));
    let enter = log
        .iter()
        .position(|call| call.method() == "keys")
        .expect("enter sent");
    let send = log
        .iter()
        .position(|call| call.method() == "send")
        .expect("text sent");
    assert!(
        enter < send,
        "send_input ran inside answer_dialog's sequence"
    );
    assert!(matches!(&log[send], Call::Send { id, .. } if id == PANE_ID));
    assert_eq!(selected, [0]);
}

#[test]
fn send_input_refused_while_blocked_reports_pane_blocked_in_shepherd_words() {
    let hint = "typing would answer its dialog; read its screen first, or force it";
    let backend = scripted(vec![
        get_at("t8"),
        get_at("t8"),
        // The idle or done status is cross-checked against the screen (issue 62).
        read_ok(plain_screen("> ready\n")),
        fail("send", ErrorKind::Blocked { kind: None }, hint),
        blocked_get("permission"),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "send_input",
        json!({"terminal": "work", "text": "no"}),
    );
    let log = backend.finish();
    // The refusal does not name the dialog, so a get after it asks for the kind.
    assert_eq!(methods(&log), ["get", "get", "read", "send", "get"]);
    assert!(matches!(&log[3], Call::Send { id, guard, .. }
        if id == "t8" && *guard == guard_blocked()));
    assert_eq!(out["isError"], true);
    let data = &out["structuredContent"];
    assert_eq!(data["error"], "pane_blocked");
    assert_eq!(data["kind"], "permission");
    assert_eq!(data["terminal"], "work");
    assert!(data["next"].as_str().unwrap().contains("answer_dialog"));
    assert!(data.get("hint").is_none());
    let detail = data["backend_detail"].as_str().unwrap();
    assert!(detail.contains(hint), "{detail}");
    let text = text_of(&out);
    assert_eq!(
        text,
        "pane_blocked: work is blocked on a permission dialog; nothing was typed. \
         Read the screen with read_output, then answer with answer_dialog"
    );
    assert_no_vendor_strings(&out);
}

#[test]
fn send_input_blocked_kind_comes_from_the_read_after_the_refusal() {
    let backend = scripted(vec![
        blocked_get("permission"),
        blocked_get("permission"),
        fail("send", ErrorKind::Blocked { kind: None }, "blocked"),
        blocked_get("question"),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "send_input",
        json!({"terminal": "work", "text": "no"}),
    );
    backend.finish();
    assert_eq!(out["structuredContent"]["error"], "pane_blocked");
    assert_eq!(out["structuredContent"]["kind"], "question");
}

/// Every failure kind a backend can report reaches MCP as a Shepherd code, in
/// Shepherd's words, with the backend's text kept only as `backend_detail`.
#[test]
fn every_backend_error_kind_reaches_mcp_as_a_shepherd_code() {
    const DETAIL: &str = "backend says: try hc ls --force on terminal.x";
    for (kind, code) in [
        (ErrorKind::Unavailable, "unavailable"),
        (ErrorKind::Timeout, "timeout"),
        (ErrorKind::NotFound, "not_found"),
        (ErrorKind::NameTaken, "name_taken"),
        (ErrorKind::StatusMismatch, "status_mismatch"),
        (ErrorKind::Finished, "pane_finished"),
        (ErrorKind::Unsupported("x".to_string()), "unsupported"),
        (ErrorKind::InvalidInput, "invalid_input"),
        (ErrorKind::Protocol, "backend_error"),
        (ErrorKind::Other, "backend_error"),
    ] {
        let label = format!("{kind:?}");
        let backend = scripted(vec![fail("get", kind, DETAIL)]);
        let out = run_tool(
            &shepherd(&backend),
            "get_status",
            json!({"terminal": "work"}),
        );
        backend.finish();
        assert_eq!(out["isError"], true, "{label}");
        let data = &out["structuredContent"];
        assert_eq!(data["error"], code, "{label}");
        assert!(text_of(&out).starts_with(&format!("{code}: ")), "{label}");
        assert_eq!(data["backend_detail"], DETAIL, "{label}");
        assert_no_vendor_strings(&out);
    }

    // A blocked refusal of typing is pane_blocked.
    let backend = scripted(vec![
        get_at("t8"),
        get_at("t8"),
        // The idle or done status is cross-checked against the screen (issue 62).
        read_ok(plain_screen("> ready\n")),
        fail("send", ErrorKind::Blocked { kind: None }, DETAIL),
        blocked_get("permission"),
    ]);
    let out = run_tool(
        &shepherd(&backend),
        "send_input",
        json!({"terminal": "work", "text": "hi"}),
    );
    backend.finish();
    let data = &out["structuredContent"];
    assert_eq!(data["error"], "pane_blocked");
    assert_eq!(data["backend_detail"], DETAIL);
    assert_no_vendor_strings(&out);

    // Every backend call a tool makes reports a failure the same way.
    let allow = dialog_on_allow();
    let lock_gets = || vec![get_at("t8"), get_at("t8")];
    let cases: Vec<(&'static str, &str, Value, Vec<Step>)> = vec![
        ("run", "start_pane", json!({"line": "echo hi"}), vec![]),
        ("get", "get_status", json!({"terminal": "work"}), vec![]),
        ("list", "list_panes", json!({}), vec![]),
        (
            "read",
            "read_output",
            json!({"terminal": "work"}),
            vec![get_at("t8")],
        ),
        (
            "wait",
            "wait_status",
            json!({"terminal": "work", "timeout": 5}),
            vec![get_at("t8")],
        ),
        (
            "send",
            "send_input",
            json!({"terminal": "work", "text": "hi"}),
            // The idle or done status is cross-checked against the screen.
            [lock_gets(), vec![read_ok(plain_screen("> ready\n"))]].concat(),
        ),
        (
            "interrupt",
            "interrupt_pane",
            json!({"terminal": "work"}),
            lock_gets(),
        ),
        (
            "remove",
            "remove_pane",
            json!({"terminal": "work"}),
            lock_gets(),
        ),
        (
            "keys",
            "answer_dialog",
            json!({"terminal": "work", "choice": "allow_once"}),
            vec![
                blocked_get("permission"),
                blocked_get("permission"),
                screen(&allow),
                screen(&allow),
                blocked_get("permission"),
            ],
        ),
    ];
    for (method, tool, args, mut script) in cases {
        script.push(fail(method, ErrorKind::Other, "the backend failed"));
        let backend = scripted(script);
        let out = run_tool(&shepherd(&backend), tool, args);
        let log = backend.finish();
        assert_eq!(log.last().unwrap().method(), method, "{tool}");
        assert_eq!(out["isError"], true, "{tool}");
        assert_eq!(out["structuredContent"]["error"], "backend_error", "{tool}");
        assert!(text_of(&out).starts_with("backend_error: "), "{tool}");
        assert_no_vendor_strings(&out);
    }
}

#[test]
fn get_status_and_list_panes_return_normalized_panes() {
    let info = TerminalInfo {
        id: "t9".to_string(),
        name: Some("work".to_string()),
        live: true,
        exit_code: None,
        status: Some(status(State::Blocked, Some("permission"))),
        reported: true,
        turn: Some(4),
        status_ms: Some(17),
        argv: Some(vec!["kiro-cli".to_string(), "chat".to_string()]),
        cwd: Some("/tmp/w".to_string()),
    };
    let expected = json!({
        "name": "work", "live": true, "exit_code": null,
        "status": {"state": "blocked", "kind": "permission"}, "reported": true,
        "turn": 4, "status_ms": 17, "argv": ["kiro-cli", "chat"], "cwd": "/tmp/w"
    });
    let backend = scripted(vec![
        get(info.clone()),
        list_ok(TerminalList {
            terminals: vec![info.clone()],
            hidden_finished: Some(2),
            live_limit: None,
        }),
        run_ok(info),
    ]);
    let client = shepherd(&backend);
    let status = run_tool(&client, "get_status", json!({"terminal": "work"}));
    let list = run_tool(&client, "list_panes", json!({}));
    let started = run_tool(
        &client,
        "start_pane",
        json!({"name": "work", "argv": ["kiro-cli", "chat"]}),
    );
    let log = backend.finish();
    assert_eq!(methods(&log), ["get", "list", "run"]);
    let mut with_terminal = expected.clone();
    with_terminal["terminal"] = json!("work");
    assert_eq!(status["structuredContent"], with_terminal);
    assert_eq!(
        list["structuredContent"],
        json!({"panes": [expected.clone()], "hidden_finished": 2,
               "live_panes": 1, "live_pane_limit": null,
               "profiles": [], "allow_arbitrary_command": true})
    );
    assert_eq!(started["structuredContent"], expected);
    for out in [&status, &list, &started] {
        assert_no_vendor_strings(out);
        let text = text_of(out);
        for field in ["address", "machines", "identity", "\"app\"", "t9"] {
            assert!(!text.contains(field), "{field} in {text}");
        }
    }
}

#[test]
fn tool_definitions_and_results_carry_no_backend_vocabulary() {
    let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let listed = handle_message(&shepherd(&scripted(vec![])), &msg).unwrap();
    let text = listed["result"].to_string();
    for banned in BANNED {
        assert!(!text.contains(banned), "{banned} in tools/list");
    }
    assert!(!text.contains("herdr"), "herdr in tools/list");

    let backend = scripted(vec![
        // read_output
        get_at("t8"),
        read_ok(ScreenText {
            text: "hi".to_string(),
            complete: Some(true),
            source: Some(ReadSource::Screen),
            limit: None,
        }),
        // wait_status that fails with a protocol error
        get_at("t8"),
        fail("wait", ErrorKind::Protocol, "no ended flag"),
        // interrupt_pane
        get_at("t8"),
        get_at("t8"),
        interrupt_ok(InterruptResult {
            key: Key::Esc,
            outcome: InterruptOutcome::Stopped,
        }),
        // send_input, sent
        get_at("t8"),
        get_at("t8"),
        // The idle or done status is cross-checked against the screen (issue 62).
        read_ok(plain_screen("> ready\n")),
        send_ok(SendResult {
            turn: Some(2),
            started: Some(true),
            status: Some(State::Done),
        }),
        // wait_status blocked; the follow-up get fails
        get_at("t8"),
        wait_ok(wait_result(
            true,
            Some(WaitReason::Status),
            Some(State::Blocked),
            1,
        )),
        fail(
            "get",
            ErrorKind::NotFound,
            "get: backend says no (hc ls --force)",
        ),
    ]);
    let client = shepherd(&backend);
    let mut outs = Vec::new();
    for (name, args) in [
        ("read_output", json!({"terminal": "work"})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
        ("interrupt_pane", json!({"terminal": "work"})),
        ("send_input", json!({"terminal": "work", "text": "hi"})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
    ] {
        outs.push(run_tool(&client, name, args));
    }
    backend.finish();
    assert_eq!(outs[0]["isError"], false);
    assert_eq!(outs[1]["structuredContent"]["error"], "backend_error");
    assert!(text_of(&outs[1]).contains("not treated as idle"));
    assert_eq!(outs[2]["structuredContent"]["key"], "esc");
    assert_eq!(outs[3]["structuredContent"]["status"], "done");
    assert!(outs[3]["structuredContent"].get("wait").is_none());
    assert_eq!(outs[4]["structuredContent"]["error"], "not_found");
    assert!(text_of(&outs[4]).contains("reading the dialog kind failed"));
    for out in &outs {
        assert_no_vendor_strings(out);
    }

    // answer_dialog success and refusal.
    let backend = scripted(vec![
        blocked_get("permission"),
        blocked_get("permission"),
        screen(&dialog_on_allow()),
        screen(&dialog_on_allow()),
        blocked_get("permission"),
        keys_ok(State::Working),
        get_with_status(State::Done),
        get_with_status(State::Done),
    ]);
    let client = shepherd(&backend);
    let allowed = answer(&client, "allow_once");
    let refused = answer(&client, "allow_once");
    backend.finish();
    assert_eq!(allowed["isError"], false, "{}", text_of(&allowed));
    assert_eq!(
        allowed["structuredContent"]["keys_result"],
        json!({"status": "working", "turn": 1})
    );
    assert_eq!(refused["structuredContent"]["error"], "refused");
    assert_no_vendor_strings(&allowed);
    assert_no_vendor_strings(&refused);
}

const BANNED: [&str; 3] = ["hc ", "--force", "terminal."];

/// No backend vocabulary in the text a model sees, or in the structured
/// content apart from the `backend_detail` debug field.
fn assert_no_vendor_strings(out: &Value) {
    let text = text_of(out);
    let mut structured = out["structuredContent"].clone();
    if let Some(map) = structured.as_object_mut() {
        map.remove("backend_detail");
    }
    let structured = structured.to_string();
    for banned in BANNED {
        assert!(!text.contains(banned), "{banned:?} in text: {text}");
        assert!(
            !structured.contains(banned),
            "{banned:?} in structuredContent: {structured}"
        );
    }
}

/// Words a backend reports that Shepherd does not know are the `Other`
/// variants, and reach MCP as `other`, in every field that carries one.
#[test]
fn unknown_backend_words_reach_mcp_as_other() {
    let odd_pane = || {
        pane(
            "t8",
            Some(ProgramStatus {
                state: State::Other,
                kind: None,
            }),
        )
    };
    let backend = scripted(vec![
        // get_status: status.state
        get(odd_pane()),
        // list_panes: status.state
        list_ok(TerminalList {
            terminals: vec![odd_pane()],
            hidden_finished: Some(0),
            live_limit: None,
        }),
        // read_output: source, limit
        get_at("t8"),
        read_ok(ScreenText {
            text: "hi".to_string(),
            complete: Some(true),
            source: Some(ReadSource::Other),
            limit: Some(ReadLimit::Other),
        }),
        // wait_status: reason, status
        get_at("t8"),
        wait_ok(wait_result(
            true,
            Some(WaitReason::Other),
            Some(State::Other),
            1,
        )),
        // wait_status: no reason at all
        get_at("t8"),
        wait_ok(wait_result(true, None, Some(State::Done), 1)),
        // interrupt_pane: key, outcome
        get_at("t8"),
        get_at("t8"),
        interrupt_ok(InterruptResult {
            key: Key::Other,
            outcome: InterruptOutcome::Other,
        }),
        // send_input: status
        get_at("t8"),
        get_at("t8"),
        // The idle or done status is cross-checked against the screen (issue 62).
        read_ok(plain_screen("> ready\n")),
        send_ok(SendResult {
            turn: Some(2),
            started: Some(true),
            status: Some(State::Other),
        }),
    ]);
    let client = shepherd(&backend);
    let mut outs = Vec::new();
    for (name, args) in [
        ("get_status", json!({"terminal": "work"})),
        ("list_panes", json!({})),
        ("read_output", json!({"terminal": "work"})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
        ("interrupt_pane", json!({"terminal": "work"})),
        ("send_input", json!({"terminal": "work", "text": "hi"})),
    ] {
        let out = run_tool(&client, name, args);
        assert_eq!(out["isError"], false, "{name}: {}", text_of(&out));
        outs.push(out);
    }
    backend.finish();
    let got: Vec<&Value> = outs.iter().map(|out| &out["structuredContent"]).collect();
    assert_eq!(got[0]["status"]["state"], "other");
    assert_eq!(got[1]["panes"][0]["status"]["state"], "other");
    assert_eq!(got[2]["source"], "other");
    assert_eq!(got[2]["reason"], "other");
    assert_eq!(got[3]["reason"], "other");
    assert_eq!(got[3]["status"], "other");
    assert_eq!(got[3]["outcome"], "other");
    assert_eq!(got[4]["reason"], Value::Null);
    assert_eq!(got[4]["outcome"], "other");
    assert_eq!(got[5]["key"], "other");
    assert_eq!(got[5]["outcome"], "other");
    assert_eq!(got[6]["status"], "other");
    for out in &outs {
        assert_no_vendor_strings(out);
    }
}

#[test]
fn unknown_backend_dialog_kinds_reach_mcp_as_other() {
    const ODD: &str = "backend-odd terminal.x --force";
    let odd_blocked = || blocked_get(ODD);
    let backend = scripted(vec![
        // get_status
        odd_blocked(),
        // list_panes
        list_ok(TerminalList {
            terminals: vec![blocked_pane(ODD)],
            hidden_finished: Some(0),
            live_limit: None,
        }),
        // wait_status: the wait has no kind, the get after it does
        get_at("t8"),
        wait_ok(wait_result(
            true,
            Some(WaitReason::Status),
            Some(State::Blocked),
            1,
        )),
        odd_blocked(),
        // send_input refused by the backend; the kind comes from the get after it
        get_at("t8"),
        get_at("t8"),
        // The idle or done status is cross-checked against the screen (issue 62).
        read_ok(plain_screen("> ready\n")),
        fail("send", ErrorKind::Blocked { kind: None }, "blocked"),
        odd_blocked(),
    ]);
    let client = shepherd(&backend);
    let mut outs = Vec::new();
    for (name, args) in [
        ("get_status", json!({"terminal": "work"})),
        ("list_panes", json!({})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
        ("send_input", json!({"terminal": "work", "text": "hi"})),
    ] {
        outs.push(run_tool(&client, name, args));
    }
    backend.finish();
    let got: Vec<&Value> = outs.iter().map(|out| &out["structuredContent"]).collect();
    assert_eq!(got[0]["status"]["kind"], "other");
    assert_eq!(got[1]["panes"][0]["status"]["kind"], "other");
    assert_eq!(got[2]["outcome"], "blocked");
    assert_eq!(got[2]["kind"], "other");
    assert_eq!(got[2]["status_now"]["kind"], "other");
    assert_eq!(outs[3]["isError"], true);
    assert_eq!(got[3]["error"], "pane_blocked");
    assert_eq!(got[3]["kind"], "other");
    assert_eq!(
        text_of(&outs[3]),
        "pane_blocked: work is blocked on a dialog of another kind; nothing was typed. \
         Read the screen with read_output, then answer with answer_dialog"
    );
    for out in &outs {
        assert_no_vendor_strings(out);
        assert!(!out.to_string().contains("backend-odd"), "{out}");
    }
}

/// A devin profile: its program lives in `<dir>/bin`, and the policy's PATH
/// names that directory. Returns the policy and the program's canonical path.
fn devin_profile_policy(dir: &Path) -> (LaunchPolicy, String) {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    let work = dir.join("work");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    let program = bin.join("devin");
    std::fs::write(&program, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let canonical = std::fs::canonicalize(&program)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let bin_text = bin.to_string_lossy().into_owned();
    let text = json!({"profiles": [
        {"name": "devin", "argv": ["devin"], "cwds": [work], "agent": "devin"}
    ]});
    let policy = LaunchPolicy::from_json(&text.to_string())
        .unwrap()
        .with_env_lookup(move |key| (key == "PATH").then(|| bin_text.clone()));
    (policy, canonical)
}

/// A run reply for a pane at `t1` with no name, as a backend may give.
fn run_reply() -> Step {
    run_ok(TerminalInfo {
        name: None,
        ..pane("t1", Some(status(State::Idle, None)))
    })
}

#[test]
fn start_pane_from_a_devin_profile_records_the_agent_for_answer_dialog() {
    let dir = TempDir::new("flows-devin-start");
    let (policy, program) = devin_profile_policy(dir.path());
    // The screen shows no trust prompt for the whole window.
    let mut script = vec![run_reply()];
    script.extend((0..tools::RENDER_READS).map(|_| read_ok(plain_screen("$ "))));
    let backend = scripted(script);
    let sh = Shepherd::with_policy(backend.clone(), policy);
    let out = run_tool(
        &sh,
        "start_pane",
        json!({"profile": "devin", "name": "devin1"}),
    );
    let log = backend.finish();
    assert_eq!(out["isError"], false, "{}", text_of(&out));
    let Call::Run(spec) = &log[0] else {
        panic!("run expected, got {log:?}");
    };
    assert_eq!(
        spec.argv,
        Some(vec![
            program,
            "--permission-mode".to_string(),
            "auto".to_string()
        ])
    );
    assert_eq!(sh.agent_of("devin1"), Some(Agent::Devin));
    assert_eq!(sh.agent_of("unknown"), None);
}

fn plain_screen(text: &str) -> ScreenText {
    ScreenText {
        text: text.to_string(),
        complete: Some(false),
        source: Some(ReadSource::Screen),
        limit: Some(ReadLimit::Blocked),
    }
}

#[test]
fn start_pane_closes_a_devin_pane_stopped_at_the_trust_prompt() {
    let dir = TempDir::new("flows-devin-trust");
    let (policy, _) = devin_profile_policy(dir.path());
    let trust = "Do you trust the authors of this directory?\n 1 Yes, trust\n 2 No, exit\n";
    let backend = scripted(vec![
        run_reply(),
        screen(trust),
        // The pane is closed with one remove.
        remove_ok(RemoveResult {
            was_live: true,
            stopped: Some(StopHow::Exited),
            exit_code: None,
        }),
    ]);
    let sh = Shepherd::with_policy(backend.clone(), policy);
    let out = run_tool(
        &sh,
        "start_pane",
        json!({"profile": "devin", "name": "devin1"}),
    );
    let log = backend.finish();
    assert_eq!(out["isError"], true);
    let text = text_of(&out);
    assert!(text.contains("trust_prompt"), "{text}");
    assert!(
        text.contains("Do you trust the authors of this directory?"),
        "{text}"
    );
    assert_eq!(methods(&log), ["run", "read", "remove"]);
    // Nothing was typed or answered, and the pane is not remembered.
    assert!(log.iter().all(|call| call.method() != "send"));
    assert!(log.iter().all(|call| call.method() != "keys"));
    assert_eq!(sh.agent_of("devin1"), None);
}
