//! Tool-level tests over the herdr-core wire: each drives an MCP message through
//! Shepherd and [`HcBackend`] against a fake hc socket, then checks the hc calls
//! made and the MCP reply.

use super::fake::{reply, serve_once, serve_pane, serve_script, PANE_ADDRESS, PANE_NAME};
use super::HcBackend;
use crate::dialog::fixtures::{dialog_on_allow, dialog_on_deny, PROBE_HEAD, PROBE_TAIL};
use crate::mcp::handle_message;
use crate::profiles::LaunchPolicy;
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

fn hc_shepherd(sock: &Path) -> Shepherd {
    Shepherd::with_policy(HcBackend::new(sock), LaunchPolicy::arbitrary())
}

fn text_of(out: &Value) -> String {
    out["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn start_pane_passes_clean_line_to_herdr_core() {
    let dir = TempDir::new("mcp-clean");
    let sock = dir.path().join("core.sock");
    let handle = serve_once(&sock, |s, req| {
        reply(
            s,
            &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"address": "t1"}}),
        );
    });
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("start_pane", json!({"line": "  echo hi  "})),
    ));
    let (_, request) = handle.join().unwrap();
    assert_eq!(out["isError"], false);
    assert_eq!(request["method"], "terminal.run");
    // No name given: Shepherd picks one, sends it, and returns it.
    let name = request["params"]["name"].as_str().unwrap().to_string();
    assert!(crate::input::validate_name(&name).is_ok(), "{name}");
    assert!(name.starts_with("pane-") && name.len() == 17, "{name}");
    assert_eq!(
        request["params"],
        json!({"line": "echo hi", "name": name.clone()})
    );
    assert_eq!(out["structuredContent"]["name"], json!(name));
    assert_eq!(out["structuredContent"]["live"], false);
    // The backend's address is not part of the pane object.
    assert!(out["structuredContent"].get("id").is_none());
    assert!(!text_of(&out).contains("t1"));
}

#[test]
fn send_input_maps_to_terminal_send_without_force() {
    let dir = TempDir::new("mcp-send");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            get_at("t1"),
            get_at("t1"),
            ok(json!({"turn": 3, "started": true})),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("send_input", json!({"terminal": "work", "text": "echo hi"})),
    ));
    let requests = fake.finish();
    assert_eq!(out["isError"], false);
    assert_eq!(
        methods(&requests),
        ["terminal.get", "terminal.get", "terminal.send"]
    );
    let request = &requests[2];
    assert_eq!(
        request["params"],
        json!({"terminal": "t1", "text": "echo hi"})
    );
    assert!(request["params"].get("force").is_none());
}

#[test]
fn read_output_strips_escapes_from_text() {
    let dir = TempDir::new("mcp-read");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            get_at("t1"),
            ok(json!({
                "text": "\u{1b}]7501;state=done\u{7}\u{1b}[32mok\u{1b}[0m",
                "complete": true, "how": "screen"})),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("read_output", json!({"terminal": "work"})),
    ));
    let requests = fake.finish();
    assert_eq!(methods(&requests), ["terminal.get", "terminal.read"]);
    // The name resolves the pane; the read goes to its address.
    assert_eq!(requests[0]["params"]["terminal"], "work");
    assert_eq!(requests[1]["params"]["terminal"], "t1");
    assert_eq!(out["structuredContent"]["text"], "ok");
    assert_eq!(out["structuredContent"]["source"], "screen");
    assert_eq!(out["structuredContent"]["reason"], Value::Null);
    assert!(out["structuredContent"].get("how").is_none());
}

#[test]
fn wait_status_silent_reply_is_an_error_not_idle() {
    let dir = TempDir::new("mcp-wait");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            get_at("t1"),
            ok(json!({"reason": "status", "status": {"state": "idle"}})),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("wait_status", json!({"terminal": "work", "timeout": 5})),
    ));
    fake.finish();
    assert_eq!(out["isError"], true);
    assert!(text_of(&out).contains("not treated as idle"));
}

#[test]
fn wait_status_timeout_reports_ended_false() {
    let dir = TempDir::new("mcp-checkin");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            get_at("t1"),
            ok(json!({"ended": false, "reason": "timeout", "turn": 2, "children": []})),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("wait_status", json!({"terminal": "work", "timeout": 5})),
    ));
    let requests = fake.finish();
    let request = &requests[1];
    assert_eq!(request["method"], "terminal.wait");
    assert_eq!(request["params"]["terminal"], "t1");
    assert_eq!(request["params"]["timeout"], 5.0);
    assert_eq!(out["structuredContent"]["terminal"], "work");
    assert_eq!(out["structuredContent"]["ended"], false);
    assert_eq!(out["structuredContent"]["reason"], "timeout");
    assert_eq!(out["structuredContent"]["outcome"], "timeout");
    assert_eq!(out["structuredContent"]["turn_complete"], false);
}

#[test]
fn list_panes_passes_all_flag() {
    let dir = TempDir::new("mcp-list-panes");
    let sock = dir.path().join("core.sock");
    let handle = serve_once(&sock, |s, req| {
        reply(
            s,
            &json!({"jsonrpc": "2.0", "id": req["id"], "result": {
            "terminals": [], "hidden_finished": 0, "machines": []}}),
        );
    });
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("list_panes", json!({"all": true})),
    ));
    let (_, request) = handle.join().unwrap();
    assert_eq!(request["method"], "terminal.list");
    assert_eq!(request["params"], json!({"all": true}));
    assert_eq!(out["structuredContent"]["hidden_finished"], 0);
    assert_eq!(out["structuredContent"]["panes"], json!([]));
}

#[test]
fn interrupt_pane_maps_to_terminal_interrupt() {
    let dir = TempDir::new("mcp-int");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            get_at("t1"),
            get_at("t1"),
            ok(json!({"key": "esc", "outcome": "stopped"})),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("interrupt_pane", json!({"terminal": "work"})),
    ));
    let requests = fake.finish();
    assert_eq!(
        methods(&requests),
        ["terminal.get", "terminal.get", "terminal.interrupt"]
    );
    assert_eq!(requests[2]["params"]["terminal"], "t1");
    assert_eq!(out["structuredContent"]["outcome"], "stopped");
}

fn ok(result: Value) -> Value {
    json!({ "result": result })
}

/// A terminal.get reply for the pane named `work` at `address`.
fn get_at(address: &str) -> Value {
    ok(
        json!({"address": address, "name": "work", "state": "live", "turn": 1,
              "reported": true, "status": {"app": "kiro-cli", "state": "done"}}),
    )
}

fn get_with_status(status: Value) -> Value {
    ok(
        json!({"address": "t8", "name": "work", "state": "live", "turn": 1, "reported": true,
              "status": status}),
    )
}

fn blocked_get(kind: &str) -> Value {
    get_with_status(json!({"app": "kiro-cli", "kind": kind, "state": "blocked"}))
}

fn screen(text: &str) -> Value {
    ok(json!({"text": text, "complete": false, "how": "screen", "why": "blocked"}))
}

fn keys_ok(status: &str) -> Value {
    ok(json!({"status": status, "turn": 1}))
}

fn methods(requests: &[Value]) -> Vec<&str> {
    requests
        .iter()
        .map(|r| r["method"].as_str().unwrap())
        .collect()
}

/// The `keys` array of every terminal.keys request, in order.
fn keys_sent(requests: &[Value]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r["method"] == "terminal.keys")
        .map(|r| r["params"]["keys"].clone())
        .collect()
}

fn answer(sock: &Path, choice: &str) -> Value {
    tool_out(handle_message(
        &hc_shepherd(sock),
        &call_msg(
            "answer_dialog",
            json!({"terminal": "work", "choice": choice}),
        ),
    ))
}

#[test]
fn wait_status_blocked_reports_outcome_and_kind_from_terminal_get() {
    let dir = TempDir::new("mcp-wait-blocked");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            blocked_get("permission"),
            ok(
                json!({"children": [], "ended": true, "exit_code": null, "reason": "status",
                      "signal": null, "status": "blocked", "terminal": "t8", "turn": 1}),
            ),
            blocked_get("permission"),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("wait_status", json!({"terminal": "work", "timeout": 5})),
    ));
    let requests = fake.finish();
    assert_eq!(
        methods(&requests),
        ["terminal.get", "terminal.wait", "terminal.get"]
    );
    assert_eq!(requests[2]["params"]["terminal"], "t8");
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
    let dir = TempDir::new("mcp-wait-done");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            get_at("t8"),
            ok(
                json!({"children": [], "ended": true, "exit_code": null, "reason": "status",
                      "status": "done", "terminal": "t8", "turn": 2}),
            ),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("wait_status", json!({"terminal": "work", "timeout": 5})),
    ));
    let requests = fake.finish();
    // The lookup, then the wait; no get after it.
    assert_eq!(methods(&requests), ["terminal.get", "terminal.wait"]);
    let got = &out["structuredContent"];
    assert_eq!(got["outcome"], "done");
    assert_eq!(got["turn_complete"], true);
    assert_eq!(got["kind"], Value::Null);
}

#[test]
fn wait_status_exit_and_other_status_words() {
    for (reply_body, outcome, complete) in [
        (
            json!({"ended": true, "reason": "exit", "status": null, "exit_code": 0}),
            "exited",
            true,
        ),
        (
            json!({"ended": true, "reason": "status", "status": "idle"}),
            "other",
            false,
        ),
        (
            json!({"ended": true, "reason": "status", "status": "error"}),
            "other",
            false,
        ),
        (
            json!({"ended": false, "reason": "message", "status": "working"}),
            "other",
            false,
        ),
    ] {
        let dir = TempDir::new("mcp-wait-words");
        let sock = dir.path().join("core.sock");
        let mut body = reply_body.clone();
        body["turn"] = json!(3);
        body["terminal"] = json!("t8");
        body["children"] = json!([]);
        let fake = serve_script(&sock, vec![get_at("t8"), ok(body)]);
        let out = tool_out(handle_message(
            &hc_shepherd(&sock),
            &call_msg("wait_status", json!({"terminal": "work", "timeout": 5})),
        ));
        fake.finish();
        assert_eq!(out["structuredContent"]["outcome"], outcome, "{reply_body}");
        assert_eq!(
            out["structuredContent"]["turn_complete"], complete,
            "{reply_body}"
        );
    }
}

#[test]
fn send_input_is_sent_when_pane_is_done() {
    let dir = TempDir::new("mcp-send-done");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            get_at("t8"),
            get_at("t8"),
            ok(json!({"started": true, "status": "done", "turn": 2})),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg(
            "send_input",
            json!({"terminal": "work", "text": "Please retry"}),
        ),
    ));
    let requests = fake.finish();
    assert_eq!(out["isError"], false);
    assert_eq!(out["structuredContent"]["status"], "done");
    assert_eq!(
        requests[2]["params"],
        json!({"terminal": "t8", "text": "Please retry"})
    );
}

#[test]
fn answer_dialog_allow_once_needs_no_moves() {
    let dir = TempDir::new("mcp-dlg-allow");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            blocked_get("permission"),
            blocked_get("permission"),
            screen(&dialog_on_allow()),
            screen(&dialog_on_allow()),
            blocked_get("permission"),
            keys_ok("working"),
        ],
    );
    let out = answer(&sock, "allow_once");
    let requests = fake.finish();
    assert_eq!(out["isError"], false, "{}", text_of(&out));
    assert_eq!(
        methods(&requests),
        [
            "terminal.get",
            "terminal.get",
            "terminal.read",
            "terminal.read",
            "terminal.get",
            "terminal.keys"
        ]
    );
    assert_eq!(keys_sent(&requests), [json!(["enter"])]);
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
    let dir = TempDir::new("mcp-dlg-deny");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            blocked_get("permission"),
            blocked_get("permission"),
            screen(&dialog_on_allow()),
            keys_ok("blocked"),
            screen(&dialog_on_deny()),
            blocked_get("permission"),
            keys_ok("working"),
        ],
    );
    let out = answer(&sock, "deny_once");
    let requests = fake.finish();
    assert_eq!(out["isError"], false, "{}", text_of(&out));
    assert_eq!(keys_sent(&requests), [json!(["down"]), json!(["enter"])]);
    assert_eq!(out["structuredContent"]["moves"], 1);
    assert_eq!(out["structuredContent"]["selected"], "Deny once");
}

#[test]
fn answer_dialog_refuses_when_not_blocked() {
    let dir = TempDir::new("mcp-dlg-done");
    let sock = dir.path().join("core.sock");
    let done = get_with_status(json!({"app": "kiro-cli", "state": "done"}));
    let fake = serve_script(&sock, vec![done.clone(), done]);
    let out = answer(&sock, "allow_once");
    let requests = fake.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(methods(&requests), ["terminal.get", "terminal.get"]);
    assert_eq!(out["structuredContent"]["error"], "refused");
    assert_eq!(out["structuredContent"]["seen"]["status"]["state"], "done");
    assert!(text_of(&out).contains("not blocked on a permission dialog"));
}

#[test]
fn answer_dialog_refuses_when_kind_is_not_permission() {
    let dir = TempDir::new("mcp-dlg-question");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![blocked_get("question"), blocked_get("question")],
    );
    let out = answer(&sock, "allow_once");
    let requests = fake.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(methods(&requests), ["terminal.get", "terminal.get"]);
    assert!(text_of(&out).contains("\"question\""), "{}", text_of(&out));
}

#[test]
fn answer_dialog_refuses_when_options_do_not_parse() {
    let dir = TempDir::new("mcp-dlg-noparse");
    let sock = dir.path().join("core.sock");
    let broken = format!("{PROBE_HEAD} ❯ Yes\n   No\n{PROBE_TAIL}");
    let fake = serve_script(&sock, render_wait_script(&broken, tools::RENDER_READS));
    let out = answer(&sock, "deny_once");
    let requests = fake.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(requests.len(), 2 + 2 * tools::RENDER_READS - 1);
    assert!(keys_sent(&requests).is_empty());
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

/// The two lock gets, then `reads` reads of `text` with a blocked
/// terminal.get before every read after the first.
fn render_wait_script(text: &str, reads: usize) -> Vec<Value> {
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
    let dir = TempDir::new("mcp-dlg-render");
    let sock = dir.path().join("core.sock");
    let mut script = render_wait_script(BEFORE_DIALOG, 2);
    script.extend([
        blocked_get("permission"),
        screen(&dialog_on_allow()),
        screen(&dialog_on_allow()),
        blocked_get("permission"),
        keys_ok("working"),
    ]);
    let fake = serve_script(&sock, script);
    let out = answer(&sock, "allow_once");
    let requests = fake.finish();
    assert_eq!(out["isError"], false, "{}", text_of(&out));
    assert_eq!(
        methods(&requests),
        [
            "terminal.get",
            "terminal.get",
            "terminal.read",
            "terminal.get",
            "terminal.read",
            "terminal.get",
            "terminal.read",
            "terminal.read",
            "terminal.get",
            "terminal.keys"
        ]
    );
    assert_eq!(keys_sent(&requests), [json!(["enter"])]);
    assert_eq!(out["structuredContent"]["selected"], "Allow once");
}

#[test]
fn answer_dialog_refuses_when_the_dialog_never_renders() {
    let dir = TempDir::new("mcp-dlg-norender");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        render_wait_script(BEFORE_DIALOG, tools::RENDER_READS),
    );
    let out = answer(&sock, "allow_once");
    let requests = fake.finish();
    assert_eq!(out["isError"], true);
    // Every scripted reply used, nothing after the last read.
    assert_eq!(requests.len(), 2 + 2 * tools::RENDER_READS - 1);
    assert_eq!(requests.last().unwrap()["method"], "terminal.read");
    assert!(keys_sent(&requests).is_empty());
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
            get_with_status(json!({"app": "kiro-cli", "state": "done"})),
            "not blocked with kind permission",
        ),
        (
            ok(
                json!({"address": "t8", "name": "work", "state": "live", "turn": 2, "reported": true,
                      "status": {"app": "kiro-cli", "kind": "permission", "state": "blocked"}}),
            ),
            "the turn is now 2, not 1",
        ),
    ];
    for (changed, expected) in cases {
        let dir = TempDir::new("mcp-dlg-render-changed");
        let sock = dir.path().join("core.sock");
        let mut script = render_wait_script(BEFORE_DIALOG, 2);
        // The pane changes before the third read; a dialog would be there.
        script.extend([changed, screen(&dialog_on_allow())]);
        let fake = serve_script(&sock, script);
        let out = answer(&sock, "allow_once");
        let requests = fake.finish();
        assert_eq!(out["isError"], true, "{expected}");
        assert_eq!(
            methods(&requests),
            [
                "terminal.get",
                "terminal.get",
                "terminal.read",
                "terminal.get",
                "terminal.read",
                "terminal.get"
            ],
            "nothing after the changed get: {expected}"
        );
        assert!(keys_sent(&requests).is_empty(), "{expected}");
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
    let dir = TempDir::new("mcp-dlg-stuck");
    let sock = dir.path().join("core.sock");
    let mut script = vec![
        blocked_get("permission"),
        blocked_get("permission"),
        screen(&dialog_on_allow()),
        keys_ok("blocked"),
    ];
    // The cursor never leaves Allow once.
    script.extend((0..tools::CONFIRM_READS).map(|_| screen(&dialog_on_allow())));
    let fake = serve_script(&sock, script);
    let out = answer(&sock, "deny_once");
    let requests = fake.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(keys_sent(&requests), [json!(["down"])]);
    assert_eq!(requests.len(), 4 + tools::CONFIRM_READS);
    assert_eq!(out["structuredContent"]["keys_sent"], json!([["down"]]));
    let text = text_of(&out);
    assert!(text.contains("not confirmed on \"Deny once\""), "{text}");
    assert!(text.contains("Enter was not sent"), "{text}");
}

#[test]
fn answer_dialog_sends_no_enter_when_status_or_turn_changes_before_it() {
    let cases = [
        (
            get_with_status(json!({"app": "kiro-cli", "state": "working"})),
            "not blocked with kind permission",
        ),
        (
            ok(
                json!({"address": "t8", "name": "work", "state": "live", "turn": 2, "reported": true,
                      "status": {"app": "kiro-cli", "kind": "permission", "state": "blocked"}}),
            ),
            "the turn is now 2, not 1",
        ),
        (blocked_get("question"), "not blocked with kind permission"),
    ];
    for (recheck, expected) in cases {
        let dir = TempDir::new("mcp-dlg-changed");
        let sock = dir.path().join("core.sock");
        let fake = serve_script(
            &sock,
            vec![
                blocked_get("permission"),
                blocked_get("permission"),
                screen(&dialog_on_allow()),
                keys_ok("blocked"),
                screen(&dialog_on_deny()),
                recheck,
            ],
        );
        let out = answer(&sock, "deny_once");
        let requests = fake.finish();
        assert_eq!(out["isError"], true, "{expected}");
        assert_eq!(requests.len(), 6, "nothing after the re-check: {expected}");
        assert_eq!(requests[5]["method"], "terminal.get");
        assert_eq!(keys_sent(&requests), [json!(["down"])], "no enter");
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
    let dir = TempDir::new("mcp-name-moved");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(&sock, vec![get_at("t8"), get_at("t9")]);
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("send_input", json!({"terminal": "work", "text": "hi"})),
    ));
    let requests = fake.finish();
    assert_eq!(out["isError"], true);
    assert_eq!(methods(&requests), ["terminal.get", "terminal.get"]);
    assert_eq!(out["structuredContent"]["error"], "terminal_changed");
    assert!(text_of(&out).contains("nothing was sent"));
}

/// herdr-core resolves an address as well as a name. Shepherd must not: a pane
/// with no name, or a named pane given by its address, is not found, and only
/// the lookup reaches herdr-core.
#[test]
fn herdr_addresses_do_not_reach_panes_through_mcp() {
    let unnamed = || {
        ok(
            json!({"address": "t1", "state": "live", "turn": 1, "reported": true,
                  "status": {"app": "kiro-cli", "state": "done"}}),
        )
    };
    let tools: [(&str, Value); 6] = [
        ("send_input", json!({"text": "hi"})),
        ("read_output", json!({})),
        ("get_status", json!({})),
        ("wait_status", json!({"timeout": 1})),
        ("interrupt_pane", json!({})),
        ("answer_dialog", json!({"choice": "allow_once"})),
    ];
    // An unnamed pane by its address, then the pane named work by its address.
    for (address, lookup) in [("t1", unnamed()), ("t8", get_at("t8"))] {
        let dir = TempDir::new("mcp-address");
        let sock = dir.path().join("core.sock");
        let fake = serve_script(&sock, vec![lookup; tools.len()]);
        let client = hc_shepherd(&sock);
        for (tool, extra) in &tools {
            let mut arguments = extra.clone();
            arguments["terminal"] = json!(address);
            let out = tool_out(handle_message(&client, &call_msg(tool, arguments)));
            let label = format!("{tool} {address}");
            assert_eq!(out["isError"], true, "{label}");
            assert_eq!(out["structuredContent"]["error"], "not_found", "{label}");
            assert!(
                text_of(&out).contains(&format!("no pane is named {address}")),
                "{label}: {}",
                text_of(&out)
            );
            assert_no_hc_strings(&out);
        }
        let requests = fake.finish();
        assert_eq!(methods(&requests), vec!["terminal.get"; tools.len()]);
    }

    // By name, the same pane works and results name it, not its address.
    let dir = TempDir::new("mcp-address-name");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            get_at("t8"),
            get_at("t8"),
            get_at("t8"),
            ok(json!({"started": true, "status": "done", "turn": 2})),
            get_at("t8"),
            ok(json!({"ended": true, "reason": "status", "status": "done", "turn": 2})),
        ],
    );
    let client = hc_shepherd(&sock);
    for (tool, arguments) in [
        ("get_status", json!({"terminal": "work"})),
        ("send_input", json!({"terminal": "work", "text": "hi"})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
    ] {
        let out = tool_out(handle_message(&client, &call_msg(tool, arguments)));
        assert_eq!(out["isError"], false, "{tool}: {}", text_of(&out));
        assert_eq!(out["structuredContent"]["terminal"], "work", "{tool}");
        assert!(!out.to_string().contains("t8"), "{tool}: {out}");
    }
    fake.finish();
}

/// Wait until a call is parked on the pane lock for `address`. The deadline
/// only turns a hang into a failure; the order of events does not depend on it.
fn wait_for_lock_waiter(client: &Shepherd, address: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.pane_locks().waiting(address) == 0 {
        assert!(
            Instant::now() < deadline,
            "no call waited for the {address} lock"
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
    let dir = TempDir::new("mcp-dlg-race");
    let sock = dir.path().join("core.sock");
    // Hold the reply to the 2nd terminal.read: call A's confirming read.
    let fake = serve_pane(&sock, Some(2));
    let client = hc_shepherd(&sock);
    let a = spawn_call(
        &client,
        "answer_dialog",
        json!({"terminal": PANE_NAME, "choice": "allow_once"}),
    );
    fake.wait_paused();
    // A holds the lock and sits between its confirming read and Enter.
    let b = spawn_call(
        &client,
        "answer_dialog",
        json!({"terminal": PANE_NAME, "choice": "deny_once"}),
    );
    wait_for_lock_waiter(&client, PANE_ADDRESS);
    fake.release();
    let out_a = a.join().unwrap();
    let out_b = b.join().unwrap();
    let pane = fake.finish();

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
    assert_eq!(pane.selected, [0]);
    // A ran get, get, read, read(confirm), get(re-check), Enter with nothing of
    // B's in between except B's lookup before the lock. B's own get came after.
    assert_eq!(
        methods(&pane.log),
        [
            "terminal.get",
            "terminal.get",
            "terminal.read",
            "terminal.read",
            "terminal.get",
            "terminal.get",
            "terminal.keys",
            "terminal.get",
        ]
    );
    assert_eq!(keys_sent(&pane.log), [json!(["enter"])]);
}

#[test]
fn different_tools_on_one_pane_share_its_lock() {
    let dir = TempDir::new("mcp-name-lock");
    let sock = dir.path().join("core.sock");
    let fake = serve_pane(&sock, Some(2));
    let client = hc_shepherd(&sock);
    let a = spawn_call(
        &client,
        "answer_dialog",
        json!({"terminal": PANE_NAME, "choice": "allow_once"}),
    );
    fake.wait_paused();
    // B is another tool on the same pane; it must wait on the lock A holds
    // for the pane's address.
    let b = spawn_call(
        &client,
        "send_input",
        json!({"terminal": PANE_NAME, "text": "next prompt"}),
    );
    wait_for_lock_waiter(&client, PANE_ADDRESS);
    fake.release();
    let out_a = a.join().unwrap();
    let out_b = b.join().unwrap();
    let pane = fake.finish();

    assert_eq!(out_a["isError"], false, "{}", text_of(&out_a));
    assert_eq!(out_b["isError"], false, "{}", text_of(&out_b));
    let enter = pane
        .log
        .iter()
        .position(|r| r["method"] == "terminal.keys")
        .expect("enter sent");
    let send = pane
        .log
        .iter()
        .position(|r| r["method"] == "terminal.send")
        .expect("text sent");
    assert!(
        enter < send,
        "send_input ran inside answer_dialog's sequence"
    );
    assert_eq!(pane.log[send]["params"]["terminal"], PANE_ADDRESS);
    assert_eq!(pane.selected, [0]);
}

#[test]
fn send_input_refused_while_blocked_reports_pane_blocked_in_shepherd_words() {
    let dir = TempDir::new("mcp-send-blocked");
    let sock = dir.path().join("core.sock");
    let hint = "typing would answer its dialog; read its screen first (hc read t8), or --force";
    let fake = serve_script(
        &sock,
        vec![
            get_at("t8"),
            get_at("t8"),
            json!({"error": {"code": -32000, "message": "t8 is blocked (permission)",
                              "data": {"kind": "blocked", "hint": hint}}}),
            blocked_get("permission"),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("send_input", json!({"terminal": "work", "text": "no"})),
    ));
    let requests = fake.finish();
    // The refusal does not name the dialog, so a get after it asks for the kind.
    assert_eq!(
        methods(&requests),
        [
            "terminal.get",
            "terminal.get",
            "terminal.send",
            "terminal.get"
        ]
    );
    assert!(requests[2]["params"].get("force").is_none());
    assert_eq!(requests[2]["params"]["terminal"], "t8");
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
    assert_no_hc_strings(&out);
}

#[test]
fn send_input_blocked_kind_comes_from_the_read_after_the_refusal() {
    let dir = TempDir::new("mcp-send-kind");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            blocked_get("permission"),
            blocked_get("permission"),
            json!({"error": {"code": -32000, "message": "blocked",
                              "data": {"kind": "blocked"}}}),
            blocked_get("question"),
        ],
    );
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("send_input", json!({"terminal": "work", "text": "no"})),
    ));
    fake.finish();
    assert_eq!(out["structuredContent"]["error"], "pane_blocked");
    assert_eq!(out["structuredContent"]["kind"], "question");
}

/// Every herdr-core error kind, extracted from the herdr-core 0.1.19 OpenRPC
/// schema, and the kinds each method this adapter calls declares. Regenerate it
/// when the supported herdr-core version changes; the tests below then fail for
/// any kind without an expected Shepherd code.
const SCHEMA_KINDS: &str = include_str!("error_kinds.json");

/// Classes `rpc.rs` assigns itself when no herdr-core reply carries a kind.
const LOCAL_KINDS: [&str; 4] = ["unavailable", "timeout", "transport", "protocol"];

/// The one authoritative table: hc error kind -> Shepherd error code. It must
/// list exactly the schema kinds plus the local classes.
const HC_KIND_CODES: [(&str, &str); 28] = [
    ("already_added", "backend_error"),
    ("blocked", "pane_blocked"),
    ("config", "backend_error"),
    ("conflict", "backend_error"),
    ("edit_failed", "backend_error"),
    ("finished", "pane_finished"),
    ("hc_missing", "backend_error"),
    ("incompatible", "backend_error"),
    ("internal", "backend_error"),
    ("invalid_params", "invalid_input"),
    ("invalid_template", "backend_error"),
    ("io", "backend_error"),
    ("live", "backend_error"),
    ("machine_unknown", "not_found"),
    ("name_taken", "name_taken"),
    ("needs_framed", "backend_error"),
    ("needs_target", "backend_error"),
    ("needs_update", "backend_error"),
    ("not_found", "not_found"),
    ("not_text", "backend_error"),
    ("spawn_failed", "backend_error"),
    ("status_mismatch", "status_mismatch"),
    ("unavailable", "unavailable"),
    ("unreachable", "unavailable"),
    ("unsupported_generation", "backend_error"),
    // Local classes (`unavailable` is both).
    ("timeout", "timeout"),
    ("transport", "backend_error"),
    ("protocol", "backend_error"),
];

fn schema() -> Value {
    serde_json::from_str(SCHEMA_KINDS).expect("error_kinds.json is JSON")
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_string())
        .collect()
}

fn expected_code(hc_kind: &str) -> &'static str {
    HC_KIND_CODES
        .iter()
        .find(|(kind, _)| *kind == hc_kind)
        .map(|(_, code)| *code)
        .unwrap_or_else(|| panic!("no expected Shepherd code for hc kind {hc_kind:?}"))
}

#[test]
fn the_kind_table_covers_exactly_the_schema_kinds() {
    let schema = schema();
    let mut want: Vec<String> = strings(&schema["kinds"]);
    want.extend(LOCAL_KINDS.iter().map(|k| k.to_string()));
    want.sort_unstable();
    want.dedup();
    let mut have: Vec<String> = HC_KIND_CODES.iter().map(|(k, _)| k.to_string()).collect();
    have.sort_unstable();
    assert_eq!(have, want, "HC_KIND_CODES must list every hc kind once");
    // Every kind a used method declares is a schema kind.
    for (method, kinds) in schema["methods"].as_object().expect("methods") {
        for kind in strings(kinds) {
            assert!(want.contains(&kind), "{method}: {kind}");
        }
    }
}

#[test]
fn every_hc_error_kind_maps_to_a_shepherd_code() {
    for (hc_kind, code) in HC_KIND_CODES {
        let err = super::map_error(super::rpc::HerdrError {
            message: "t8 failed; hint (hc read t8), or --force".to_string(),
            code: Some(-32000),
            kind: Some(hc_kind.to_string()),
            hint: Some("run hc ls".to_string()),
        });
        assert_eq!(err.code(), code, "{hc_kind}");
        // The hc text survives only as debug detail.
        assert!(err.detail.contains("--force"), "{hc_kind}");
    }
    let blocked = super::map_error(super::rpc::HerdrError {
        message: "blocked".to_string(),
        code: Some(-32000),
        kind: Some("blocked".to_string()),
        hint: None,
    });
    assert_eq!(
        blocked.kind,
        crate::backend::ErrorKind::Blocked { kind: None }
    );
    // A kind herdr-core does not document, or none at all.
    for kind in [Some("unexpected_call"), None] {
        let err = super::map_error(super::rpc::HerdrError {
            message: "odd".to_string(),
            code: None,
            kind: kind.map(str::to_string),
            hint: None,
        });
        assert_eq!(err.code(), "backend_error", "{kind:?}");
    }
}

/// Drive the tool that reaches `method`, with the replies it needs before
/// that call, and fail `method` with `error`. Returns the MCP result.
fn fail_method(method: &str, error: Value) -> Value {
    let (tool, args, mut script) = match method {
        "terminal.run" => ("start_pane", json!({"line": "echo hi"}), vec![]),
        "terminal.get" => ("get_status", json!({"terminal": "work"}), vec![]),
        "terminal.list" => ("list_panes", json!({}), vec![]),
        "terminal.read" => (
            "read_output",
            json!({"terminal": "work"}),
            vec![get_at("t8")],
        ),
        "terminal.wait" => (
            "wait_status",
            json!({"terminal": "work", "timeout": 5}),
            vec![get_at("t8")],
        ),
        "terminal.send" => (
            "send_input",
            json!({"terminal": "work", "text": "hi"}),
            vec![get_at("t8"), get_at("t8")],
        ),
        "terminal.interrupt" => (
            "interrupt_pane",
            json!({"terminal": "work"}),
            vec![get_at("t8"), get_at("t8")],
        ),
        "terminal.keys" => (
            "answer_dialog",
            json!({"terminal": "work", "choice": "allow_once"}),
            vec![
                blocked_get("permission"),
                blocked_get("permission"),
                screen(&dialog_on_allow()),
                screen(&dialog_on_allow()),
                blocked_get("permission"),
            ],
        ),
        other => panic!("no tool reaches {other}"),
    };
    let prefix = script.len();
    script.push(error);
    // A blocked send refusal is followed by a get that names the dialog.
    script.push(blocked_get("permission"));
    let dir = TempDir::new("mcp-codes");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(&sock, script);
    let out = tool_out(handle_message(&hc_shepherd(&sock), &call_msg(tool, args)));
    let requests = fake.finish();
    assert_eq!(requests[prefix]["method"], method, "{tool}");
    out
}

#[test]
fn every_hc_error_kind_reaches_mcp_as_a_shepherd_code() {
    let schema = schema();
    let methods = schema["methods"].as_object().expect("methods");
    assert_eq!(methods.len(), 8, "the eight adapter methods");
    let mut covered = Vec::new();
    for (method, kinds) in methods {
        for hc_kind in strings(kinds) {
            let code = expected_code(&hc_kind);
            let out = fail_method(
                method,
                json!({"error": {"code": -32000,
                    "message": format!("{method} failed: {hc_kind}; try hc ls or --force"),
                    "data": {"kind": hc_kind, "hint": "run hc ls"}}}),
            );
            let label = format!("{method} {hc_kind}");
            assert_eq!(out["isError"], true, "{label}");
            let data = &out["structuredContent"];
            assert_eq!(data["error"], code, "{label}");
            assert!(text_of(&out).starts_with(&format!("{code}: ")), "{label}");
            let detail = data["backend_detail"].as_str().unwrap();
            assert!(
                detail.contains(&format!("({hc_kind})")),
                "{label}: {detail}"
            );
            assert_no_hc_strings(&out);
            covered.push(label);
        }
    }
    for label in [
        "terminal.run invalid_template",
        "terminal.wait needs_target",
    ] {
        assert!(covered.iter().any(|c| c == label), "{label}");
    }

    // Nothing listens: the socket connect fails.
    let dir = TempDir::new("mcp-codes-absent");
    let out = tool_out(handle_message(
        &hc_shepherd(&dir.path().join("absent.sock")),
        &call_msg("list_panes", json!({})),
    ));
    assert_eq!(out["structuredContent"]["error"], "unavailable");
    assert_no_hc_strings(&out);

    // A reply that is not JSON.
    let sock = dir.path().join("core.sock");
    let handle = serve_once(&sock, |s, _req| {
        use std::io::Write;
        s.write_all(b"not json\n").unwrap();
    });
    let out = tool_out(handle_message(
        &hc_shepherd(&sock),
        &call_msg("get_status", json!({"terminal": "work"})),
    ));
    handle.join().unwrap();
    assert_eq!(out["structuredContent"]["error"], "backend_error");
    assert_no_hc_strings(&out);
}

#[test]
fn get_status_and_list_panes_return_normalized_panes() {
    let hc_terminal = json!({
        "address": "t9", "name": "work", "state": "live", "argv": ["kiro-cli", "chat"],
        "cwd": "/tmp/w", "cols": 80, "rows": 24, "created_ms": 1, "screen": "exact",
        "labels": {}, "identity": "ghostty 1.4.0", "exit_code": null, "reported": true,
        "turn": 4, "status_ms": 17,
        "status": {"app": "kiro-cli", "state": "blocked", "kind": "permission"}
    });
    let expected = json!({
        "name": "work", "live": true, "exit_code": null,
        "status": {"state": "blocked", "kind": "permission"}, "reported": true,
        "turn": 4, "status_ms": 17, "argv": ["kiro-cli", "chat"], "cwd": "/tmp/w"
    });
    let dir = TempDir::new("mcp-normal");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            ok(hc_terminal.clone()),
            ok(
                json!({"terminals": [hc_terminal.clone()], "hidden_finished": 2,
                      "machines": [{"name": "other"}]}),
            ),
            ok(hc_terminal),
        ],
    );
    let client = hc_shepherd(&sock);
    let status = tool_out(handle_message(
        &client,
        &call_msg("get_status", json!({"terminal": "work"})),
    ));
    let list = tool_out(handle_message(&client, &call_msg("list_panes", json!({}))));
    let started = tool_out(handle_message(
        &client,
        &call_msg(
            "start_pane",
            json!({"name": "work", "argv": ["kiro-cli", "chat"]}),
        ),
    ));
    let requests = fake.finish();
    assert_eq!(
        methods(&requests),
        ["terminal.get", "terminal.list", "terminal.run"]
    );
    let mut with_terminal = expected.clone();
    with_terminal["terminal"] = json!("work");
    assert_eq!(status["structuredContent"], with_terminal);
    assert_eq!(
        list["structuredContent"],
        json!({"panes": [expected.clone()], "hidden_finished": 2,
               "profiles": [], "allow_arbitrary_command": true})
    );
    assert_eq!(started["structuredContent"], expected);
    for out in [&status, &list, &started] {
        assert_no_hc_strings(out);
        let text = text_of(out);
        for field in ["address", "machines", "identity", "\"app\""] {
            assert!(!text.contains(field), "{field} in {text}");
        }
    }
}

#[test]
fn no_hc_strings_in_tool_results_errors_or_definitions() {
    let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
    let dir = TempDir::new("mcp-nohc");
    let listed = handle_message(&hc_shepherd(&dir.path().join("absent.sock")), &msg).unwrap();
    let text = listed["result"].to_string();
    for banned in BANNED {
        assert!(!text.contains(banned), "{banned} in tools/list");
    }
    assert!(!text.contains("herdr"), "herdr in tools/list");

    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            // read_output
            get_at("t8"),
            ok(json!({"text": "hi", "complete": true, "how": "screen", "why": null})),
            // wait_status with no `ended`: a protocol error
            get_at("t8"),
            ok(json!({"reason": "status", "status": "idle"})),
            // interrupt_pane
            get_at("t8"),
            get_at("t8"),
            ok(json!({"key": "esc", "outcome": "stopped"})),
            // send_input, sent
            get_at("t8"),
            get_at("t8"),
            ok(json!({"started": true, "status": "done", "turn": 2, "wait": null})),
            // wait_status blocked; the follow-up get fails
            get_at("t8"),
            ok(
                json!({"ended": true, "reason": "status", "status": "blocked",
                      "terminal": "t8", "turn": 1, "children": []}),
            ),
            json!({"error": {"code": -32000, "message": "terminal.get: hc says no",
                              "data": {"kind": "not_found", "hint": "hc ls --force"}}}),
        ],
    );
    let client = hc_shepherd(&sock);
    let mut outs = Vec::new();
    for (name, args) in [
        ("read_output", json!({"terminal": "work"})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
        ("interrupt_pane", json!({"terminal": "work"})),
        ("send_input", json!({"terminal": "work", "text": "hi"})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
    ] {
        outs.push(tool_out(handle_message(&client, &call_msg(name, args))));
    }
    fake.finish();
    assert_eq!(outs[0]["isError"], false);
    assert_eq!(outs[1]["structuredContent"]["error"], "backend_error");
    assert!(text_of(&outs[1]).contains("not treated as idle"));
    assert_eq!(outs[2]["structuredContent"]["key"], "esc");
    assert_eq!(outs[3]["structuredContent"]["status"], "done");
    assert!(outs[3]["structuredContent"].get("wait").is_none());
    assert_eq!(outs[4]["structuredContent"]["error"], "not_found");
    assert!(text_of(&outs[4]).contains("reading the dialog kind failed"));
    for out in &outs {
        assert_no_hc_strings(out);
    }

    // answer_dialog success and refusal.
    let dir = TempDir::new("mcp-nohc-dlg");
    let sock = dir.path().join("core.sock");
    let fake = serve_script(
        &sock,
        vec![
            blocked_get("permission"),
            blocked_get("permission"),
            screen(&dialog_on_allow()),
            screen(&dialog_on_allow()),
            blocked_get("permission"),
            keys_ok("working"),
            get_with_status(json!({"app": "kiro-cli", "state": "done"})),
            get_with_status(json!({"app": "kiro-cli", "state": "done"})),
        ],
    );
    let allowed = answer(&sock, "allow_once");
    let refused = answer(&sock, "allow_once");
    fake.finish();
    assert_eq!(allowed["isError"], false, "{}", text_of(&allowed));
    assert_eq!(
        allowed["structuredContent"]["keys_result"],
        json!({"status": "working", "turn": 1})
    );
    assert_eq!(refused["structuredContent"]["error"], "refused");
    assert_no_hc_strings(&allowed);
    assert_no_hc_strings(&refused);
}

const BANNED: [&str; 3] = ["hc ", "--force", "terminal."];

/// No hc vocabulary in the text a model sees, or in the structured content
/// apart from the `backend_detail` debug field.
fn assert_no_hc_strings(out: &Value) {
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

/// Words hc might send that Shepherd does not know reach MCP as `other`,
/// never verbatim, in every field that carries a backend word.
#[test]
fn unknown_hc_words_reach_mcp_as_other() {
    const ODD: &str = "hc-odd terminal.x --force";
    let dir = TempDir::new("mcp-odd-words");
    let sock = dir.path().join("core.sock");
    let odd_get = || {
        ok(
            json!({"address": "t8", "name": "work", "state": "live", "turn": 1,
                  "reported": true, "status": {"app": "kiro-cli", "state": ODD}}),
        )
    };
    let fake = serve_script(
        &sock,
        vec![
            // get_status: status.state
            odd_get(),
            // list_panes: status.state
            ok(json!({"terminals": [odd_get()["result"].clone()], "hidden_finished": 0})),
            // read_output: how, why
            get_at("t8"),
            ok(json!({"text": "hi", "complete": true, "how": ODD, "why": ODD})),
            // wait_status: reason, status
            get_at("t8"),
            ok(json!({"ended": true, "reason": ODD, "status": ODD, "turn": 1})),
            // wait_status: no reason at all
            get_at("t8"),
            ok(json!({"ended": true, "status": "done", "turn": 1})),
            // interrupt_pane: key, outcome
            get_at("t8"),
            get_at("t8"),
            ok(json!({"key": ODD, "outcome": ODD})),
            // send_input: status
            get_at("t8"),
            get_at("t8"),
            ok(json!({"started": true, "status": ODD, "turn": 2})),
        ],
    );
    let client = hc_shepherd(&sock);
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
        let out = tool_out(handle_message(&client, &call_msg(name, args)));
        assert_eq!(out["isError"], false, "{name}: {}", text_of(&out));
        outs.push(out);
    }
    fake.finish();
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
        assert_no_hc_strings(out);
        assert!(!out.to_string().contains("hc-odd"), "{out}");
    }
}

#[test]
fn unknown_hc_dialog_kinds_reach_mcp_as_other() {
    const ODD: &str = "hc-odd terminal.x --force";
    let dir = TempDir::new("mcp-odd-kind");
    let sock = dir.path().join("core.sock");
    let odd_blocked = || blocked_get(ODD);
    let refusal = || {
        json!({"error": {"code": -32000, "message": "blocked",
                          "data": {"kind": "blocked"}}})
    };
    let fake = serve_script(
        &sock,
        vec![
            // get_status
            odd_blocked(),
            // list_panes
            ok(json!({"terminals": [odd_blocked()["result"].clone()], "hidden_finished": 0})),
            // wait_status: the wait has no kind, the get after it does
            get_at("t8"),
            ok(json!({"ended": true, "reason": "status", "status": "blocked", "turn": 1})),
            odd_blocked(),
            // send_input refused by hc; the kind comes from the get after it
            get_at("t8"),
            get_at("t8"),
            refusal(),
            odd_blocked(),
        ],
    );
    let client = hc_shepherd(&sock);
    let mut outs = Vec::new();
    for (name, args) in [
        ("get_status", json!({"terminal": "work"})),
        ("list_panes", json!({})),
        ("wait_status", json!({"terminal": "work", "timeout": 1})),
        ("send_input", json!({"terminal": "work", "text": "hi"})),
    ] {
        outs.push(tool_out(handle_message(&client, &call_msg(name, args))));
    }
    fake.finish();
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
        assert_no_hc_strings(out);
        assert!(!out.to_string().contains("hc-odd"), "{out}");
    }
}

#[test]
fn known_dialog_kinds_pass_through() {
    for kind in ["permission", "question", "auth"] {
        assert_eq!(crate::backend::dialog_kind(kind), kind);
    }
    assert_eq!(crate::backend::dialog_kind(""), "other");
    assert_eq!(crate::backend::dialog_kind("Permission"), "other");
}
