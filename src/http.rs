//! Streamable HTTP front end on loopback.
//!
//! Each request passes four gates in order: loopback Host header, bearer token,
//! the `/mcp` path, and the POST method. Only then is the body parsed and handed
//! to the MCP layer. Nothing is logged from request bodies, because they can carry
//! text typed into panes.

use crate::herdr::HerdrClient;
use crate::mcp;
use serde_json::{json, Value};
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use tiny_http::{Header, Request, Response, Server, StatusCode};

pub const MCP_PATH: &str = "/mcp";
/// Largest accepted request body. Tool inputs are capped at 4096 bytes each, so
/// this bound only guards the parser.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

pub struct AppState {
    pub token: String,
    pub client: HerdrClient,
}

type Body = Response<std::io::Cursor<Vec<u8>>>;

/// Listen on `bind` and serve until the process exits. Each request runs on its own
/// thread, so a long `wait_status` does not block other calls.
pub fn serve(bind: SocketAddr, state: Arc<AppState>) -> Result<(), String> {
    let server = Server::http(bind).map_err(|err| format!("cannot listen on {bind}: {err}"))?;
    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        thread::spawn(move || handle(request, &state));
    }
    Ok(())
}

fn handle(mut request: Request, state: &AppState) {
    // Read the body before replying. If request bytes are still unread when the
    // response goes out, the kernel resets the connection and the client can
    // lose the status line.
    let body = read_body(&mut request);
    let response = route(&mut request, state, body);
    if let Err(err) = request.respond(response) {
        eprintln!("openab-shepherd: failed to write response: {err}");
    }
}

fn route(request: &mut Request, state: &AppState, body: Result<Vec<u8>, u16>) -> Body {
    let host = header_value(request, "Host");
    if !host.as_deref().is_some_and(is_loopback_host) {
        return json_reply(403, json!({"error": "forbidden", "message": "Host must be a loopback name"}), None);
    }

    let authorization = header_value(request, "Authorization");
    if !bearer_matches(authorization.as_deref(), &state.token) {
        return json_reply(
            401,
            json!({"error": "unauthorized", "message": "missing or invalid bearer token"}),
            Some("Bearer"),
        );
    }

    let path = request.url().split('?').next().unwrap_or_default().to_string();
    if path != MCP_PATH {
        return json_reply(404, json!({"error": "not_found", "message": "use the /mcp path"}), None);
    }
    if request.method().as_str() != "POST" {
        return json_reply(405, json!({"error": "method_not_allowed", "message": "use POST"}), None);
    }

    let body = match body {
        Ok(body) => body,
        Err(status) => {
            let message = if status == 413 { "request body too large" } else { "cannot read request body" };
            return json_reply(status, json!({"error": "bad_request", "message": message}), None);
        }
    };
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(_) => {
            return json_reply(
                400,
                json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}),
                None,
            )
        }
    };
    match mcp::handle_message(&state.client, &message) {
        Some(reply) => json_reply(200, reply, None),
        None => empty_reply(202),
    }
}

/// Read the body up to the limit. Returns the HTTP status to send on failure.
fn read_body(request: &mut Request) -> Result<Vec<u8>, u16> {
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| 400u16)?;
    if body.len() > MAX_BODY_BYTES {
        Err(413)
    } else {
        Ok(body)
    }
}

fn header_value(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str().to_string())
}

/// True for the loopback names a local client uses in its Host header:
/// 127.0.0.1, [::1], or localhost, with or without a port.
pub fn is_loopback_host(value: &str) -> bool {
    let host = if let Some(rest) = value.strip_prefix('[') {
        match rest.find(']') {
            Some(end) => &rest[..end],
            None => return false,
        }
    } else {
        match value.matches(':').count() {
            0 => value,
            1 => value.split(':').next().unwrap_or_default(),
            _ => return false,
        }
    };
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// Compare `Authorization` against `Bearer <token>` without an early exit on the
/// first differing byte.
pub fn bearer_matches(header: Option<&str>, token: &str) -> bool {
    let Some(supplied) = header else {
        return false;
    };
    let expected = format!("Bearer {token}");
    constant_time_eq(supplied.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    diff == 0
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header is valid")
}

fn json_reply(status: u16, body: Value, www_authenticate: Option<&str>) -> Body {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut response = Response::from_data(bytes)
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", "application/json"));
    if let Some(scheme) = www_authenticate {
        response = response.with_header(header("WWW-Authenticate", scheme));
    }
    response
}

fn empty_reply(status: u16) -> Body {
    Response::from_data(Vec::new()).with_status_code(StatusCode(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpStream;
    use std::sync::Arc;

    const TOKEN: &str = "test-token-0123456789abcdef";

    #[test]
    fn loopback_host_names() {
        for ok in ["127.0.0.1", "127.0.0.1:8765", "localhost:8765", "[::1]:8765", "[::1]"] {
            assert!(is_loopback_host(ok), "{ok} should be allowed");
        }
        // A bare IPv6 host is not valid in a Host header (it must be bracketed).
        for bad in ["::1", "evil.example", "evil.example:8765", "192.0.2.10:8765", "[2001:db8::1]:8765", "0.0.0.0"] {
            assert!(!is_loopback_host(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn bearer_must_match_exactly() {
        assert!(bearer_matches(Some(&format!("Bearer {TOKEN}")), TOKEN));
        assert!(!bearer_matches(None, TOKEN));
        assert!(!bearer_matches(Some(TOKEN), TOKEN), "scheme is required");
        assert!(!bearer_matches(Some("Bearer wrong-token-0000000000"), TOKEN));
        assert!(!bearer_matches(Some(&format!("Bearer {TOKEN}x")), TOKEN));
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Send one raw HTTP/1.1 request and return (status, body).
    fn raw_request(port: u16, host: &str, auth: Option<&str>, method: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let auth_line = auth.map(|a| format!("Authorization: {a}\r\n")).unwrap_or_default();
        let request = format!(
            "{method} /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n{auth_line}\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = response.split("\r\n\r\n").nth(1).unwrap_or_default().to_string();
        (status, body)
    }

    #[test]
    fn end_to_end_auth_host_path_and_method() {
        let port = free_port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let state = Arc::new(AppState {
            token: TOKEN.to_string(),
            client: HerdrClient::new("/tmp/shep-e2e-absent.sock"),
        });
        thread::spawn(move || {
            let _ = serve(bind, state);
        });
        // Wait until the listener accepts connections.
        for _ in 0..100 {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }

        let host = format!("127.0.0.1:{port}");
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let bearer = format!("Bearer {TOKEN}");

        let (status, body) = raw_request(port, &host, None, "POST", init);
        assert_eq!(status, 401, "no token");
        assert!(body.contains("unauthorized"), "401 body must arrive intact: {body}");

        let (status, body) = raw_request(port, "evil.example", Some(&bearer), "POST", init);
        assert_eq!(status, 403, "non-loopback Host");
        assert!(body.contains("forbidden"), "403 body must arrive intact: {body}");

        let (status, body) = raw_request(port, &host, Some(&bearer), "POST", init);
        assert_eq!(status, 200, "authenticated initialize");
        assert!(body.contains("protocolVersion"), "{body}");

        let notify = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let (status, _) = raw_request(port, &host, Some(&bearer), "POST", notify);
        assert_eq!(status, 202, "notification gets 202");

        let (status, _) = raw_request(port, &host, Some(&bearer), "GET", "");
        assert_eq!(status, 405, "GET is not supported");
    }
}
