//! MCP message handling (JSON-RPC 2.0 over Streamable HTTP, JSON responses only).
//!
//! The server is stateless: each POST carries one message and gets one JSON reply,
//! or 202 for a notification. There is no SSE stream and no session id.

use crate::tools::{self, Shepherd};
use serde_json::{json, Map, Value};

pub const SERVER_NAME: &str = "openab-shepherd";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: [&str; 2] = ["2025-06-18", "2025-03-26"];

const CODE_INVALID_REQUEST: i64 = -32600;
const CODE_METHOD_NOT_FOUND: i64 = -32601;
const CODE_INVALID_PARAMS: i64 = -32602;

/// Handle one parsed JSON-RPC message. Returns `None` when no reply is due.
pub fn handle_message(sh: &Shepherd, message: &Value) -> Option<Value> {
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
    Some(match dispatch(sh, method, object.get("params")) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, text)) => rpc_error(id, code, &text),
    })
}

fn dispatch(sh: &Shepherd, method: &str, params: Option<&Value>) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::definitions()})),
        "tools/call" => call_tool(sh, params),
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

fn call_tool(sh: &Shepherd, params: Option<&Value>) -> Result<Value, (i64, String)> {
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
    Ok(tool_result(tools::call(sh, name, &args)))
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
        Err(tools::ToolError { text, data }) => json!({
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
    use crate::profiles::LaunchPolicy;
    use crate::testutil::FakeBackend;

    fn call_msg(name: &str, arguments: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
               "params": {"name": name, "arguments": arguments}})
    }

    /// The tool result object from a `tools/call` reply.
    fn tool_out(reply: Option<Value>) -> Value {
        reply.expect("reply")["result"].clone()
    }

    /// A Shepherd over a backend with no panes. The fake records every call, so
    /// a test can show validation ran before any backend call.
    fn fake_client() -> (Shepherd, FakeBackend) {
        let fake = FakeBackend::new(true);
        (
            Shepherd::with_policy(fake.clone(), LaunchPolicy::arbitrary()),
            fake,
        )
    }

    fn text_of(out: &Value) -> String {
        out["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn initialize_negotiates_protocol_and_advertises_tools() {
        let (client, _fake) = fake_client();
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                         "params": {"protocolVersion": "2025-03-26"}});
        let out = handle_message(&client, &msg).unwrap();
        assert_eq!(out["result"]["protocolVersion"], "2025-03-26");
        assert!(out["result"]["capabilities"]["tools"].is_object());
        assert_eq!(out["result"]["serverInfo"]["name"], SERVER_NAME);
    }

    #[test]
    fn initialized_notification_gets_no_reply() {
        let (client, _fake) = fake_client();
        let msg = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle_message(&client, &msg).is_none());
    }

    #[test]
    fn tools_list_has_the_eight_tools() {
        let (client, _fake) = fake_client();
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
        let (client, _fake) = fake_client();
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
        let (client, _fake) = fake_client();
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
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": "   "})),
        ));
        assert_eq!(out["isError"], true);
        let text = text_of(&out);
        assert!(text.contains("empty"), "{text}");
        // Validation ran first: no backend call was made.
        assert!(fake.calls().is_empty(), "{text}");
    }

    #[test]
    fn start_pane_rejects_oversized_and_control_only_argv_before_any_call() {
        let (client, fake) = fake_client();
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
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn start_pane_refuses_control_character_in_line_before_any_call() {
        let (client, fake) = fake_client();
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
        assert!(fake.calls().is_empty(), "{text}");
    }

    #[test]
    fn start_pane_refuses_newline_in_line_before_any_call() {
        let (client, _fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("start_pane", json!({"line": "echo a\necho b"})),
        ));
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("U+000A"), "{}", text_of(&out));
    }

    #[test]
    fn send_input_refuses_control_character_before_any_call() {
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("send_input", json!({"terminal": "work", "text": "ls\u{7}"})),
        ));
        assert_eq!(out["isError"], true);
        let text = text_of(&out);
        assert!(
            text.contains("text: input contains control character U+0007"),
            "{text}"
        );
        assert!(fake.calls().is_empty(), "{text}");
    }

    #[test]
    fn wait_status_rejects_out_of_range_timeout() {
        let (client, _fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("wait_status", json!({"terminal": "work", "timeout": 0})),
        ));
        assert_eq!(out["isError"], true);
        assert!(text_of(&out).contains("between"));
    }

    #[test]
    fn answer_dialog_refuses_rule_choices_before_any_call() {
        let (client, fake) = fake_client();
        for choice in ["create_allow_rule", "allow_rule", "deny_rule", "Allow once"] {
            let out = tool_out(handle_message(
                &client,
                &call_msg(
                    "answer_dialog",
                    json!({"terminal": "work", "choice": choice}),
                ),
            ));
            assert_eq!(out["isError"], true);
            let text = text_of(&out);
            assert!(text.contains("allow_once or deny_once"), "{text}");
            assert!(fake.calls().is_empty(), "{text}");
        }
    }

    #[test]
    fn batch_and_non_object_are_invalid_requests() {
        let (client, _fake) = fake_client();
        let out = handle_message(&client, &json!([])).unwrap();
        assert_eq!(out["error"]["code"], CODE_INVALID_REQUEST);
        let out = handle_message(&client, &json!("text")).unwrap();
        assert_eq!(out["error"]["code"], CODE_INVALID_REQUEST);
    }

    #[test]
    fn validation_errors_carry_the_invalid_input_code() {
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("send_input", json!({"terminal": "work"})),
        ));
        assert_eq!(out["isError"], true);
        assert_eq!(out["structuredContent"]["error"], "invalid_input");
        assert_eq!(out["structuredContent"]["message"], "text is required");
        assert_eq!(text_of(&out), "invalid_input: text is required");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn backend_errors_carry_a_shepherd_code() {
        let (client, fake) = fake_client();
        let out = tool_out(handle_message(
            &client,
            &call_msg("get_status", json!({"terminal": "missing"})),
        ));
        assert_eq!(fake.calls(), ["get"]);
        assert_eq!(out["isError"], true);
        let data = &out["structuredContent"];
        assert_eq!(data["error"], "not_found");
        assert_eq!(data["terminal"], "missing");
        assert_eq!(data["backend_detail"], "fake: no such pane");
        assert!(!text_of(&out).contains("fake:"), "{}", text_of(&out));
    }
}
