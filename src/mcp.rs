//! MCP message handling (JSON-RPC 2.0 over Streamable HTTP, JSON responses only).
//!
//! The server is stateless: each POST carries one message and gets one JSON reply,
//! or 202 for a notification. There is no SSE stream and no session id.

use crate::herdr::HerdrClient;
use crate::tools;
use serde_json::{json, Map, Value};

pub const SERVER_NAME: &str = "openab-shepherd";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: [&str; 2] = ["2025-06-18", "2025-03-26"];

const CODE_INVALID_REQUEST: i64 = -32600;
const CODE_METHOD_NOT_FOUND: i64 = -32601;
const CODE_INVALID_PARAMS: i64 = -32602;

/// Handle one parsed JSON-RPC message. Returns `None` when no reply is due.
pub fn handle_message(client: &HerdrClient, message: &Value) -> Option<Value> {
    if message.is_array() {
        return Some(rpc_error(
            Value::Null,
            CODE_INVALID_REQUEST,
            "batch requests are not supported",
        ));
    }
    let Some(object) = message.as_object() else {
        return Some(rpc_error(
            Value::Null,
            CODE_INVALID_REQUEST,
            "request must be a JSON object",
        ));
    };
    let id = object.get("id").cloned();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A reply from the client (result or error, no method) needs no answer.
        if object.contains_key("result") || object.contains_key("error") {
            return None;
        }
        return Some(rpc_error(
            id.unwrap_or(Value::Null),
            CODE_INVALID_REQUEST,
            "missing method",
        ));
    };
    let Some(id) = id else {
        // A notification, such as notifications/initialized: no reply.
        return None;
    };
    Some(match dispatch(client, method, object.get("params")) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, text)) => rpc_error(id, code, &text),
    })
}

fn dispatch(
    client: &HerdrClient,
    method: &str,
    params: Option<&Value>,
) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::definitions()})),
        "tools/call" => call_tool(client, params),
        other => Err((CODE_METHOD_NOT_FOUND, format!("method not found: {other}"))),
    }
}

fn initialize(params: Option<&Value>) -> Value {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str);
    let version = match requested {
        Some(v) if SUPPORTED_PROTOCOLS.contains(&v) => v,
        _ => LATEST_PROTOCOL,
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
    })
}

fn call_tool(client: &HerdrClient, params: Option<&Value>) -> Result<Value, (i64, String)> {
    let params = params.and_then(Value::as_object).ok_or((
        CODE_INVALID_PARAMS,
        "tools/call needs an object of params".to_string(),
    ))?;
    let name = params.get("name").and_then(Value::as_str).ok_or((
        CODE_INVALID_PARAMS,
        "tools/call needs a tool name".to_string(),
    ))?;
    if !tools::is_tool(name) {
        return Err((CODE_INVALID_PARAMS, format!("unknown tool: {name}")));
    }
    let args: Map<String, Value> = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(args)) => args.clone(),
        Some(_) => {
            return Err((
                CODE_INVALID_PARAMS,
                "arguments must be an object".to_string(),
            ))
        }
    };
    Ok(tool_result(tools::call(client, name, &args)))
}

/// Tool failures are results with `isError: true`, not JSON-RPC errors, so the
/// client can show the reason to the model.
fn tool_result(outcome: Result<Value, tools::ToolError>) -> Value {
    match outcome {
        Ok(value) => {
            let structured = if value.is_object() {
                value
            } else {
                json!({ "result": value })
            };
            let text = serde_json::to_string_pretty(&structured).unwrap_or_default();
            json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": structured,
                "isError": false,
            })
        }
        Err(tools::ToolError { text, data: None }) => json!({
            "content": [{"type": "text", "text": text}],
            "isError": true,
        }),
        Err(tools::ToolError {
            text,
            data: Some(data),
        }) => json!({
            "content": [{"type": "text", "text": text}],
            "structuredContent": data,
            "isError": true,
        }),
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialog::fixtures::{dialog_on_allow, dialog_on_deny, PROBE_HEAD, PROBE_TAIL};
    use crate::testutil::{
        reply, serve_once, serve_pane, serve_script, TempDir, PANE_ADDRESS, PANE_NAME,
    };
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

    fn unreachable_client(dir: &Path) -> HerdrClient {
        // Nothing listens here. A call that reaches herdr-core reports "unavailable".
        HerdrClient::new(dir.join("absent.sock"))
    }

    fn text_of(out: &Value) -> String {
        out["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn initialize_negotiates_protocol_and_advertises_tools() {
        let dir = TempDir::new("mcp-init");
        let client = unreachable_client(dir.path());
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                         "params": {"protocolVersion": "2025-03-26"}});
        let out = handle_message(&client, &msg).unwrap();
        assert_eq!(out["result"]["protocolVersion"], "2025-03-26");
        assert!(out["result"]["capabilities"]["tools"].is_object());
        assert_eq!(out["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[test]
    fn initialized_notification_gets_no_reply() {
        let dir = TempDir::new("mcp-notify");
        let client = unreachable_client(dir.path());
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_message(&client, &msg).is_none());
    }

    #[test]
    fn tools_list_has_the_eight_tools() {
        let dir = TempDir::new("mcp-list");
        let client = unreachable_client(dir.path());
        let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let out = handle_message(&client, &msg).unwrap();
        let names: Vec<&str> = out["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, tools::TOOL_NAMES);
    }

    #[test]
    fn unknown_method_and_unknown_tool_are_jsonrpc_errors() {
        let dir = TempDir::new("mcp-unknown");
        let client = unreachable_client(dir.path());
        let out = handle_message(
            &client,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "nope"}),
        )
        .unwrap();
        assert_eq!(out["error"]["code"], CODE_METHOD_NOT_FOUND);
        let out = handle_message(&client, &call_msg("delete_everything", json!({}))).unwrap();
        assert_eq!(out["error"]["code"], CODE_INVALID_PARAMS);
    }

    #[test]
    fn start_pane_requires_exactly_one_of_argv_or_line() {
        let dir = TempDir::new("mcp-exactly");
        let client = unreachable_client(dir.path());
        let none = tool_out(handle_message(&client, &call_msg("start_pane", json!({}))));
        assert_eq!(none["isError"], true);
        assert!(text_of(&none).contains("exactly one"));
        let both = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"argv": ["sh"], "line": "echo hi"})),
        ));
        assert_eq!(both["isError"], true);
        assert!(text_of(&both).contains("exactly one"));
    }

    #[test]
    fn start_pane_rejects_whitespace_only_line_before_any_call() {
        let dir = TempDir::new("mcp-ws");
        let client = unreachable_client(dir.path());
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": "   "})),
        ));
        assert_eq!(out["isError"], true);
        let text = text_of(&out);
        assert!(text.contains("empty"), "{text}");
        // Validation ran first: no socket call was attempted.
        assert!(!text.contains("herdr-core"), "{text}");
    }

    #[test]
    fn start_pane_rejects_oversized_and_control_only_argv_before_any_call() {
        let dir = TempDir::new("mcp-big");
        let client = unreachable_client(dir.path());
        let big = "x".repeat(4097);
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": big})),
        ));
        assert!(text_of(&out).contains("limit is 4096"));
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"argv": ["sh", "\u{1b}"]})),
        ));
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("control character U+001B"));
        assert!(!text_of(&out).contains("herdr-core"));
    }

    #[test]
    fn start_pane_refuses_control_character_in_line_before_any_call() {
        let dir = TempDir::new("mcp-esc");
        let client = unreachable_client(dir.path());
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": "echo \u{1b}[31mhi\u{1b}[0m"})),
        ));
        assert_eq!(out["isError"], true);
        let text = text_of(&out);
        assert!(
            text.contains("line: input contains control character U+001B"),
            "{text}"
        );
        assert!(!text.contains("herdr-core"), "{text}");
    }

    #[test]
    fn start_pane_refuses_newline_in_line_before_any_call() {
        let dir = TempDir::new("mcp-nl");
        let client = unreachable_client(dir.path());
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": "echo a\necho b"})),
        ));
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("U+000A"), "{}", text_of(&out));
    }

    #[test]
    fn send_input_refuses_control_character_before_any_call() {
        let dir = TempDir::new("mcp-send-ctl");
        let client = unreachable_client(dir.path());
        let out = tool_out(handle_message(
            &client,
            &call_msg("send_input", json!({"terminal": "t1", "text": "ls\u{7}"})),
        ));
        assert_eq!(out["isError"], true);
        let text = text_of(&out);
        assert!(
            text.contains("text: input contains control character U+0007"),
            "{text}"
        );
        assert!(!text.contains("herdr-core"), "{text}");
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
            &HerdrClient::new(&sock),
            &call_msg("start_pane", json!({"line": "  echo hi  "})),
        ));
        let (_, request) = handle.join().unwrap();
        assert_eq!(out["isError"], false);
        assert_eq!(request["method"], "terminal.run");
        assert_eq!(request["params"]["line"], "echo hi");
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
            &HerdrClient::new(&sock),
            &call_msg("send_input", json!({"terminal": "t1", "text": "echo hi"})),
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
        let handle = serve_once(&sock, |s, req| {
            reply(
                s,
                &json!({"jsonrpc": "2.0", "id": req["id"], "result": {
                "text": "\u{1b}]7501;state=done\u{7}\u{1b}[32mok\u{1b}[0m",
                "complete": true, "how": "screen"}}),
            );
        });
        let out = tool_out(handle_message(
            &HerdrClient::new(&sock),
            &call_msg("read_output", json!({"terminal": "t1"})),
        ));
        handle.join().unwrap();
        assert_eq!(out["structuredContent"]["text"], "ok");
        assert_eq!(out["structuredContent"]["how"], "screen");
    }

    #[test]
    fn wait_status_silent_reply_is_an_error_not_idle() {
        let dir = TempDir::new("mcp-wait");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, req| {
            reply(
                s,
                &json!({"jsonrpc": "2.0", "id": req["id"], "result": {
                "reason": "status", "status": {"state": "idle"}}}),
            );
        });
        let out = tool_out(handle_message(
            &HerdrClient::new(&sock),
            &call_msg("wait_status", json!({"terminal": "t1", "timeout": 5})),
        ));
        handle.join().unwrap();
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("not treated as idle"));
    }

    #[test]
    fn wait_status_timeout_reports_ended_false() {
        let dir = TempDir::new("mcp-checkin");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, req| {
            reply(
                s,
                &json!({"jsonrpc": "2.0", "id": req["id"], "result": {
                "ended": false, "reason": "timeout", "turn": 2, "children": []}}),
            );
        });
        let out = tool_out(handle_message(
            &HerdrClient::new(&sock),
            &call_msg("wait_status", json!({"terminal": "t1", "timeout": 5})),
        ));
        let (_, request) = handle.join().unwrap();
        assert_eq!(request["method"], "terminal.wait");
        assert_eq!(request["params"]["timeout"], 5.0);
        assert_eq!(out["structuredContent"]["ended"], false);
        assert_eq!(out["structuredContent"]["reason"], "timeout");
        assert_eq!(out["structuredContent"]["outcome"], "timeout");
        assert_eq!(out["structuredContent"]["turn_complete"], false);
    }

    #[test]
    fn wait_status_rejects_out_of_range_timeout() {
        let dir = TempDir::new("mcp-range");
        let client = unreachable_client(dir.path());
        let out = tool_out(handle_message(
            &client,
            &call_msg("wait_status", json!({"terminal": "t1", "timeout": 0})),
        ));
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("between"));
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
            &HerdrClient::new(&sock),
            &call_msg("list_panes", json!({"all": true})),
        ));
        let (_, request) = handle.join().unwrap();
        assert_eq!(request["method"], "terminal.list");
        assert_eq!(request["params"], json!({"all": true}));
        assert_eq!(out["structuredContent"]["hidden_finished"], 0);
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
            &HerdrClient::new(&sock),
            &call_msg("interrupt_pane", json!({"terminal": "t1"})),
        ));
        let requests = fake.finish();
        assert_eq!(
            methods(&requests),
            ["terminal.get", "terminal.get", "terminal.interrupt"]
        );
        assert_eq!(requests[2]["params"]["terminal"], "t1");
        assert_eq!(out["structuredContent"]["outcome"], "stopped");
    }

    #[test]
    fn herdr_error_becomes_tool_error_with_kind() {
        let dir = TempDir::new("mcp-herr");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, req| {
            reply(
                s,
                &json!({"jsonrpc": "2.0", "id": req["id"], "error": {
                "code": -32000, "message": "no such terminal", "data": {"kind": "not_found"}}}),
            );
        });
        let out = tool_out(handle_message(
            &HerdrClient::new(&sock),
            &call_msg("get_status", json!({"terminal": "t9"})),
        ));
        handle.join().unwrap();
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("no such terminal (not_found)"));
    }

    #[test]
    fn batch_and_non_object_are_invalid_requests() {
        let dir = TempDir::new("mcp-batch");
        let client = unreachable_client(dir.path());
        let out = handle_message(&client, &json!([])).unwrap();
        assert_eq!(out["error"]["code"], CODE_INVALID_REQUEST);
        let out = handle_message(&client, &json!("text")).unwrap();
        assert_eq!(out["error"]["code"], CODE_INVALID_REQUEST);
    }

    fn ok(result: Value) -> Value {
        json!({ "result": result })
    }

    fn get_at(address: &str) -> Value {
        ok(
            json!({"address": address, "state": "live", "turn": 1, "reported": true,
                  "status": {"app": "kiro-cli", "state": "done"}}),
        )
    }

    fn get_with_status(status: Value) -> Value {
        ok(json!({"address": "t8", "state": "live", "turn": 1, "reported": true, "status": status}))
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
            &HerdrClient::new(sock),
            &call_msg("answer_dialog", json!({"terminal": "t8", "choice": choice})),
        ))
    }

    #[test]
    fn wait_status_blocked_reports_outcome_and_kind_from_terminal_get() {
        let dir = TempDir::new("mcp-wait-blocked");
        let sock = dir.path().join("core.sock");
        let fake = serve_script(
            &sock,
            vec![
                ok(
                    json!({"children": [], "ended": true, "exit_code": null, "reason": "status",
                          "signal": null, "status": "blocked", "terminal": "t8", "turn": 1}),
                ),
                blocked_get("permission"),
            ],
        );
        let out = tool_out(handle_message(
            &HerdrClient::new(&sock),
            &call_msg("wait_status", json!({"terminal": "t8", "timeout": 5})),
        ));
        let requests = fake.finish();
        assert_eq!(methods(&requests), ["terminal.wait", "terminal.get"]);
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
            vec![ok(
                json!({"children": [], "ended": true, "exit_code": null, "reason": "status",
                           "status": "done", "terminal": "t8", "turn": 2}),
            )],
        );
        let out = tool_out(handle_message(
            &HerdrClient::new(&sock),
            &call_msg("wait_status", json!({"terminal": "t8", "timeout": 5})),
        ));
        let requests = fake.finish();
        assert_eq!(methods(&requests), ["terminal.wait"]);
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
            let fake = serve_script(&sock, vec![ok(body)]);
            let out = tool_out(handle_message(
                &HerdrClient::new(&sock),
                &call_msg("wait_status", json!({"terminal": "t8", "timeout": 5})),
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
    fn send_input_refused_while_blocked_passes_kind_and_hint_through() {
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
            ],
        );
        let out = tool_out(handle_message(
            &HerdrClient::new(&sock),
            &call_msg("send_input", json!({"terminal": "t8", "text": "no"})),
        ));
        let requests = fake.finish();
        assert_eq!(
            methods(&requests),
            ["terminal.get", "terminal.get", "terminal.send"]
        );
        assert!(requests[2]["params"].get("force").is_none());
        assert_eq!(out["isError"], true);
        let data = &out["structuredContent"];
        assert_eq!(data["kind"], "blocked");
        assert_eq!(data["hint"], hint);
        assert_eq!(data["message"], "t8 is blocked (permission)");
        assert!(data["next"].as_str().unwrap().contains("answer_dialog"));
        let text = text_of(&out);
        assert!(text.contains("(blocked)") && text.contains(hint), "{text}");
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
            &HerdrClient::new(&sock),
            &call_msg(
                "send_input",
                json!({"terminal": "t8", "text": "Please retry"}),
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
        let fake = serve_script(
            &sock,
            vec![
                blocked_get("permission"),
                blocked_get("permission"),
                screen(&broken),
            ],
        );
        let out = answer(&sock, "deny_once");
        let requests = fake.finish();
        assert_eq!(out["isError"], true);
        assert_eq!(
            methods(&requests),
            ["terminal.get", "terminal.get", "terminal.read"]
        );
        assert!(keys_sent(&requests).is_empty());
        assert!(text_of(&out).contains("cannot parse the permission dialog"));
        assert!(out["structuredContent"]["seen"]["screen"]
            .as_str()
            .unwrap()
            .contains("❯ Yes"));
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
    fn answer_dialog_refuses_rule_choices_before_any_call() {
        let dir = TempDir::new("mcp-dlg-rule");
        let client = unreachable_client(dir.path());
        for choice in ["create_allow_rule", "allow_rule", "deny_rule", "Allow once"] {
            let out = tool_out(handle_message(
                &client,
                &call_msg("answer_dialog", json!({"terminal": "t8", "choice": choice})),
            ));
            assert_eq!(out["isError"], true);
            let text = text_of(&out);
            assert!(text.contains("allow_once or deny_once"), "{text}");
            assert!(!text.contains("herdr-core"), "{text}");
        }
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
                    json!({"address": "t8", "state": "live", "turn": 2, "reported": true,
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
            &HerdrClient::new(&sock),
            &call_msg("send_input", json!({"terminal": "work", "text": "hi"})),
        ));
        let requests = fake.finish();
        assert_eq!(out["isError"], true);
        assert_eq!(methods(&requests), ["terminal.get", "terminal.get"]);
        assert_eq!(out["structuredContent"]["error"], "terminal_changed");
        assert!(text_of(&out).contains("nothing was sent"));
    }

    /// Wait until a call is parked on the pane lock for `address`. The deadline
    /// only turns a hang into a failure; the order of events does not depend on it.
    fn wait_for_lock_waiter(client: &HerdrClient, address: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while client.pane_locks().waiting(address) == 0 {
            assert!(
                Instant::now() < deadline,
                "no call waited for the {address} lock"
            );
            thread::yield_now();
        }
    }

    fn spawn_call(client: &HerdrClient, name: &str, arguments: Value) -> thread::JoinHandle<Value> {
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
        let client = HerdrClient::new(&sock);
        let a = spawn_call(
            &client,
            "answer_dialog",
            json!({"terminal": PANE_ADDRESS, "choice": "allow_once"}),
        );
        fake.wait_paused();
        // A holds the lock and sits between its confirming read and Enter.
        let b = spawn_call(
            &client,
            "answer_dialog",
            json!({"terminal": PANE_ADDRESS, "choice": "deny_once"}),
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
    fn name_and_address_share_one_pane_lock() {
        let dir = TempDir::new("mcp-name-lock");
        let sock = dir.path().join("core.sock");
        let fake = serve_pane(&sock, Some(2));
        let client = HerdrClient::new(&sock);
        let a = spawn_call(
            &client,
            "answer_dialog",
            json!({"terminal": PANE_ADDRESS, "choice": "allow_once"}),
        );
        fake.wait_paused();
        // B names the pane; it must wait on the lock A holds for the address.
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
}
