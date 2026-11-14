//! Streamable HTTP front end on loopback.
//!
//! Each request passes four gates in order: allowed Host header (a loopback name
//! or one the operator listed), bearer token,
//! the `/mcp` path, and the POST method. Only then is the body parsed and handed
//! to the MCP layer. Nothing is logged from request bodies, because they can carry
//! text typed into panes.

use crate::clients::{unix_now, Clients};
use crate::config::AllowedHost;
use crate::mcp;
use crate::tools::Shepherd;
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
    /// Who may call: the clients file, or the single legacy token.
    pub clients: Clients,
    /// Extra Host values from `OPENAB_SHEPHERD_ALLOWED_HOSTS`; empty by default.
    pub allowed_hosts: Vec<AllowedHost>,
    pub shepherd: Shepherd,
}

type Body = Response<std::io::Cursor<Vec<u8>>>;

/// Listen on `bind` and serve until the process exits. Each request runs on its own
/// thread, so a long `wait_status` does not block other calls.
pub fn serve(bind: SocketAddr, state: Arc<AppState>) -> Result<(), String> {
    let server = Server::http(bind).map_err(|err| format!("cannot listen on {bind}: {err}"))?;
    run(server, state)
}

/// Serve on a listener the caller already holds, so tests never release a port
/// between picking it and using it.
#[cfg(test)]
fn serve_listener(listener: std::net::TcpListener, state: Arc<AppState>) -> Result<(), String> {
    let server =
        Server::from_listener(listener, None).map_err(|err| format!("cannot serve: {err}"))?;
    run(server, state)
}

fn run(server: Server, state: Arc<AppState>) -> Result<(), String> {
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
    if !host
        .as_deref()
        .is_some_and(|h| is_loopback_host(h) || is_allowed_host(h, &state.allowed_hosts))
    {
        return json_reply(
            403,
            json!({"error": "forbidden", "message": "Host must be a loopback name or an allowed host"}),
            None,
        );
    }

    let authorization = header_value(request, "Authorization");
    let caller = match state
        .clients
        .authenticate(authorization.as_deref(), unix_now())
    {
        Ok(caller) => caller,
        Err(failure) => {
            if let Some(audit) = state.shepherd.audit() {
                audit.auth_failure(&failure.id_prefix, failure.reason);
            }
            return json_reply(
                401,
                json!({"error": "unauthorized", "message": "missing or invalid bearer token"}),
                Some("Bearer"),
            );
        }
    };

    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    if path != MCP_PATH {
        return json_reply(
            404,
            json!({"error": "not_found", "message": "use the /mcp path"}),
            None,
        );
    }
    if request.method().as_str() != "POST" {
        return json_reply(
            405,
            json!({"error": "method_not_allowed", "message": "use POST"}),
            None,
        );
    }

    let body = match body {
        Ok(body) => body,
        Err(status) => {
            let message = if status == 413 {
                "request body too large"
            } else {
                "cannot read request body"
            };
            return json_reply(
                status,
                json!({"error": "bad_request", "message": message}),
                None,
            );
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
    match mcp::handle_message_as(&state.shepherd, &caller, &message) {
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

/// True when the Host header names one of the operator's allowed hosts. The
/// name compares case-insensitively and exactly, after dropping one trailing
/// dot; there is no suffix or wildcard matching. An entry with a port needs that
/// port in the header; an entry without one matches any port (or none).
pub fn is_allowed_host(value: &str, allowed: &[AllowedHost]) -> bool {
    if allowed.is_empty() {
        return false;
    }
    let (name, port) = match value.split_once(':') {
        None => (value, None),
        Some((name, port)) => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            match port.parse::<u16>() {
                Ok(port) => (name, Some(port)),
                Err(_) => return false,
            }
        }
    };
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() {
        return false;
    }
    allowed.iter().any(|entry| {
        entry.name.eq_ignore_ascii_case(name) && entry.port.is_none_or(|p| port == Some(p))
    })
}

/// Compare two SHA-256 digests without an early exit on the first differing
/// byte. Digests only: both are 32 bytes, so no length leaks. Tokens are never
/// compared directly; `Clients::authenticate` hashes the supplied token first.
pub fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
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
        for ok in [
            "127.0.0.1",
            "127.0.0.1:8765",
            "localhost:8765",
            "[::1]:8765",
            "[::1]",
        ] {
            assert!(is_loopback_host(ok), "{ok} should be allowed");
        }
        // A bare IPv6 host is not valid in a Host header (it must be bracketed).
        for bad in [
            "::1",
            "evil.example",
            "evil.example:8765",
            "192.0.2.10:8765",
            "[2001:db8::1]:8765",
            "0.0.0.0",
        ] {
            assert!(!is_loopback_host(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn allowed_host_matching() {
        use crate::config::parse_allowed_hosts;
        let none: Vec<AllowedHost> = Vec::new();
        assert!(
            !is_allowed_host("mcp-host.example-tailnet.ts.net:8445", &none),
            "default allows no extra host"
        );

        let with_port = parse_allowed_hosts("mcp-host.example-tailnet.ts.net:8445").unwrap();
        for ok in [
            "mcp-host.example-tailnet.ts.net:8445",
            "MCP-HOST.Example-Tailnet.TS.NET:8445",
            "mcp-host.example-tailnet.ts.net.:8445",
        ] {
            assert!(is_allowed_host(ok, &with_port), "{ok} should be allowed");
        }
        for bad in [
            "mcp-host.example-tailnet.ts.net",
            "mcp-host.example-tailnet.ts.net:443",
            "mcp-host.example-tailnet.ts.net:08445x",
            "mcp-host.example-tailnet.ts.net:",
            "mcp-host.example-tailnet.ts.net..:8445",
            "evil-mcp-host.example-tailnet.ts.net:8445",
            "mcp-host.example-tailnet.ts.net.evil.com:8445",
            "x.mcp-host.example-tailnet.ts.net:8445",
            "example-tailnet.ts.net:8445",
            ":8445",
            "",
        ] {
            assert!(!is_allowed_host(bad, &with_port), "{bad} should be refused");
        }

        // A trailing dot in the entry is dropped too.
        let any_port = parse_allowed_hosts("mcp-host.example-tailnet.ts.net.").unwrap();
        for ok in [
            "mcp-host.example-tailnet.ts.net",
            "mcp-host.example-tailnet.ts.net.",
            "mcp-host.example-tailnet.ts.net:8445",
            "Mcp-Host.example-tailnet.ts.net:443",
        ] {
            assert!(is_allowed_host(ok, &any_port), "{ok} should be allowed");
        }
        for bad in [
            "evil-mcp-host.example-tailnet.ts.net",
            "mcp-host.example-tailnet.ts.net.evil.com",
            "mcp-host.example-tailnet.ts.net:99999",
            "[::1]:8445",
        ] {
            assert!(!is_allowed_host(bad, &any_port), "{bad} should be refused");
        }
    }

    #[test]
    fn bearer_must_match_exactly() {
        let clients = Clients::legacy_only(TOKEN);
        let ok = |header: Option<&str>| clients.authenticate(header, 0).is_ok();
        assert!(ok(Some(&format!("Bearer {TOKEN}"))));
        assert!(!ok(None));
        assert!(!ok(Some(TOKEN)), "scheme is required");
        assert!(!ok(Some("Bearer wrong-token-0000000000")));
        assert!(!ok(Some(&format!("Bearer {TOKEN}x"))));
        let caller = clients
            .authenticate(Some(&format!("Bearer {TOKEN}")), 0)
            .unwrap();
        assert_eq!(caller.client(), "legacy");
    }

    #[test]
    fn digest_compare_is_exact() {
        let a = [7u8; 32];
        let mut b = a;
        assert!(constant_time_eq(&a, &b));
        b[31] ^= 1;
        assert!(!constant_time_eq(&a, &b));
    }

    /// Send one raw HTTP/1.1 request and return (status, body).
    fn raw_request(
        port: u16,
        host: &str,
        auth: Option<&str>,
        method: &str,
        body: &str,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let auth_line = auth
            .map(|a| format!("Authorization: {a}\r\n"))
            .unwrap_or_default();
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
        let body = response
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    /// Start a server on a loopback port it already holds. The listener is bound
    /// here and handed over, so the port is never free for another test or process
    /// to take, and it accepts connections as soon as `bind` returns.
    fn start_server(allowed_hosts: Vec<AllowedHost>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(AppState {
            clients: Clients::legacy_only(TOKEN),
            allowed_hosts,
            shepherd: Shepherd::new(crate::testutil::FakeBackend::new(true)),
        });
        thread::spawn(move || {
            let _ = serve_listener(listener, state);
        });
        port
    }

    #[test]
    fn a_port_that_is_already_taken_is_reported() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = held.local_addr().unwrap();
        let state = Arc::new(AppState {
            clients: Clients::legacy_only(TOKEN),
            allowed_hosts: Vec::new(),
            shepherd: Shepherd::new(crate::testutil::FakeBackend::new(true)),
        });
        let err = serve(bind, state).unwrap_err();
        assert!(err.contains("cannot listen on"), "{err}");
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
    const TS_HOST: &str = "mcp-host.example-tailnet.ts.net:8445";

    #[test]
    fn default_server_rejects_a_ts_net_host() {
        let port = start_server(Vec::new());
        let bearer = format!("Bearer {TOKEN}");
        let (status, body) = raw_request(port, TS_HOST, Some(&bearer), "POST", INIT);
        assert_eq!(status, 403, "{body}");
        assert!(body.contains("forbidden"), "{body}");
    }

    #[test]
    fn allowed_host_still_needs_the_bearer_token() {
        let allowed = crate::config::parse_allowed_hosts(TS_HOST).unwrap();
        let port = start_server(allowed);
        let bearer = format!("Bearer {TOKEN}");

        let (status, body) = raw_request(port, TS_HOST, None, "POST", INIT);
        assert_eq!(status, 401, "allowed host without a token: {body}");
        let (status, _) = raw_request(
            port,
            TS_HOST,
            Some("Bearer wrong-token-0000000000"),
            "POST",
            INIT,
        );
        assert_eq!(status, 401, "allowed host with a wrong token");

        let (status, body) = raw_request(port, TS_HOST, Some(&bearer), "POST", INIT);
        assert_eq!(status, 200, "allowed host with the token: {body}");
        assert!(body.contains("protocolVersion"), "{body}");

        let (status, _) = raw_request(
            port,
            "mcp-host.example-tailnet.ts.net:443",
            Some(&bearer),
            "POST",
            INIT,
        );
        assert_eq!(status, 403, "port mismatch");
        let (status, _) = raw_request(
            port,
            "mcp-host.example-tailnet.ts.net.evil.com:8445",
            Some(&bearer),
            "POST",
            INIT,
        );
        assert_eq!(status, 403, "suffix attack");

        // Loopback names keep working with extra hosts configured.
        let (status, _) = raw_request(
            port,
            &format!("127.0.0.1:{port}"),
            Some(&bearer),
            "POST",
            INIT,
        );
        assert_eq!(status, 200, "loopback still allowed");
    }

    #[test]
    fn end_to_end_auth_host_path_and_method() {
        let port = start_server(Vec::new());

        let host = format!("127.0.0.1:{port}");
        let init = INIT;
        let bearer = format!("Bearer {TOKEN}");

        let (status, body) = raw_request(port, &host, None, "POST", init);
        assert_eq!(status, 401, "no token");
        assert!(
            body.contains("unauthorized"),
            "401 body must arrive intact: {body}"
        );

        let (status, body) = raw_request(port, "evil.example", Some(&bearer), "POST", init);
        assert_eq!(status, 403, "non-loopback Host");
        assert!(
            body.contains("forbidden"),
            "403 body must arrive intact: {body}"
        );

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

/// Issue #4, PR 1: authentication and authorization over the real HTTP layer,
/// on a held listener and the project's `FakeBackend`, with an injected clock
/// where time matters. Run with `cargo test issue4_pr1`.
#[cfg(test)]
mod issue4_pr1_http {
    use super::*;
    use crate::agent::Agent;
    use crate::audit::testing::{memory_audit, MemorySink};
    use crate::backend::State;
    use crate::clients::sha256_hex;
    use crate::dialog::fixtures::dialog_on_allow;
    use crate::profiles::LaunchPolicy;
    use crate::testutil::{FakeBackend, TempDir};
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    const SECRET_CI: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const SECRET_ALICE: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const SECRET_OBS: &str = "3333333333333333333333333333333333333333333333333333333333333333";
    const FAR_FUTURE: u64 = 4_102_444_800;

    fn token(id: &str, secret: &str) -> String {
        format!("osh_{id}_{secret}")
    }

    fn client(id: &str, secret: &str, scopes: &[&str], profiles: &[&str]) -> Value {
        json!({
            "id": id,
            "token_sha256": sha256_hex(token(id, secret).as_bytes()),
            "scopes": scopes,
            "profiles": profiles,
        })
    }

    fn table() -> Clients {
        let mut expired = client("old", SECRET_OBS, &["observe"], &[]);
        expired["expires"] = json!(1);
        let mut live = client("temp", SECRET_OBS, &["observe"], &[]);
        live["expires"] = json!(FAR_FUTURE);
        Clients::parse(
            &json!({"version": 1, "clients": [
                client("ci-1", SECRET_CI, &["observe", "read", "start"], &["ci"]),
                client(
                    "alice",
                    SECRET_ALICE,
                    &["observe", "read", "control", "dialog", "start", "remove"],
                    &["ci", "other"]
                ),
                expired,
                live,
            ]})
            .to_string(),
        )
        .unwrap()
    }

    struct Server {
        port: u16,
        fake: FakeBackend,
        sink: MemorySink,
    }

    fn policy(dir: &std::path::Path, arbitrary: bool) -> LaunchPolicy {
        let text = json!({
            "allow_arbitrary_command": arbitrary,
            "profiles": [
                {"name": "ci", "argv": ["/bin/sh"], "cwds": [dir]},
                {"name": "other", "argv": ["/bin/sh"], "cwds": [dir]}
            ]
        });
        LaunchPolicy::from_json(&text.to_string()).unwrap()
    }

    /// A server on a listener held from the start, over a blocked pane `work`.
    fn start(dir: &std::path::Path, arbitrary: bool) -> Server {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut fake = FakeBackend::new(false);
        fake.caps.conditional_keys = true;
        fake.add_pane("work", State::Blocked, Some("permission"));
        {
            let mut state = fake.state.lock().unwrap();
            state.screen = Some(dialog_on_allow());
            state.runnable = true;
        }
        let (audit, sink) = memory_audit();
        let shepherd =
            Shepherd::with_policy(fake.clone(), policy(dir, arbitrary)).with_audit(Arc::new(audit));
        shepherd.assume_agent("work", Agent::Kiro);
        let state = Arc::new(AppState {
            clients: table(),
            allowed_hosts: Vec::new(),
            shepherd,
        });
        thread::spawn(move || {
            let _ = serve_listener(listener, state);
        });
        Server { port, fake, sink }
    }

    fn post(port: u16, host: &str, authorization: Option<&str>, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let auth_line = authorization
            .map(|a| format!("Authorization: {a}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
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
        let body = response
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    fn call(server: &Server, id: &str, secret: &str, tool: &str, arguments: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": tool, "arguments": arguments}});
        let bearer = format!("Bearer {}", token(id, secret));
        let host = format!("127.0.0.1:{}", server.port);
        let (status, reply) = post(server.port, &host, Some(&bearer), &body.to_string());
        assert_eq!(status, 200, "{reply}");
        serde_json::from_str::<Value>(&reply).unwrap()["result"].clone()
    }

    fn code(out: &Value) -> &str {
        out["structuredContent"]["error"].as_str().unwrap_or("")
    }

    const INIT: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;

    // ---- group 2: authenticate, over the wire ----

    #[test]
    fn a_bad_token_is_401_and_the_host_check_still_comes_first() {
        let dir = TempDir::new("i4-http-401");
        let server = start(dir.path(), false);
        let host = format!("127.0.0.1:{}", server.port);
        let good = format!("Bearer {}", token("alice", SECRET_ALICE));
        let wrong_secret = format!("Bearer {}", token("alice", SECRET_CI));
        let unknown_id = format!("Bearer {}", token("nobody", SECRET_ALICE));
        let expired = format!("Bearer {}", token("old", SECRET_OBS));
        let legacy_shape = "Bearer a-long-legacy-token-value-0123456789".to_string();
        let truncated = format!("Bearer {}x", token("alice", SECRET_ALICE));
        let mut bodies = Vec::new();
        for header in [
            None,
            Some("Basic abc"),
            Some("Bearer"),
            Some("Bearer "),
            Some(wrong_secret.as_str()),
            Some(unknown_id.as_str()),
            Some(expired.as_str()),
            Some(legacy_shape.as_str()),
            Some(truncated.as_str()),
        ] {
            let (status, body) = post(server.port, &host, header, INIT);
            assert_eq!(status, 401, "{header:?}: {body}");
            assert!(body.contains("unauthorized"), "{body}");
            bodies.push(body);
        }
        // The reply does not say which of the reasons applied.
        assert!(
            bodies.windows(2).all(|pair| pair[0] == pair[1]),
            "{bodies:?}"
        );
        // Host first: a foreign Host is 403 whatever the token is.
        for header in [None, Some(wrong_secret.as_str()), Some(good.as_str())] {
            let (status, body) = post(server.port, "evil.example", header, INIT);
            assert_eq!(status, 403, "{header:?}: {body}");
        }
        // And a good token on a good host passes, so the checks above can fail.
        let (status, body) = post(server.port, &host, Some(&good), INIT);
        assert_eq!(status, 200, "{body}");
        let live = format!("Bearer {}", token("temp", SECRET_OBS));
        assert_eq!(post(server.port, &host, Some(&live), INIT).0, 200);
        // Nothing reached a tool.
        assert!(server.fake.calls().is_empty());
    }

    #[test]
    fn authentication_failures_are_audited_without_the_token() {
        let dir = TempDir::new("i4-http-auditauth");
        let server = start(dir.path(), false);
        let host = format!("127.0.0.1:{}", server.port);
        let wrong = format!("Bearer {}", token("alice", SECRET_CI));
        assert_eq!(post(server.port, &host, Some(&wrong), INIT).0, 401);
        let records = server.sink.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["phase"], "auth");
        assert_eq!(records[0]["id_prefix"], "alice");
        assert_eq!(records[0]["reason"], "bad_secret");
        assert!(!server.sink.bytes().contains(SECRET_CI));
        assert!(!server.sink.bytes().contains(&sha256_hex(wrong.as_bytes())));
        // A refused Host is not an authentication attempt and writes nothing.
        assert_eq!(post(server.port, "evil.example", Some(&wrong), INIT).0, 403);
        assert_eq!(server.sink.records().len(), 1);
    }

    #[test]
    fn expiry_follows_the_injected_clock_to_the_second() {
        let mut ci = client("ci-1", SECRET_CI, &["observe"], &[]);
        ci["expires"] = json!(1_000);
        let clients = Clients::parse(&json!({"version": 1, "clients": [ci]}).to_string()).unwrap();
        let header = format!("Bearer {}", token("ci-1", SECRET_CI));
        assert!(clients.authenticate(Some(&header), 0).is_ok());
        assert!(clients.authenticate(Some(&header), 999).is_ok());
        let failure = clients.authenticate(Some(&header), 1_000).unwrap_err();
        assert_eq!(failure.reason, "expired");
        assert!(clients.authenticate(Some(&header), 1_001).is_err());
        assert!(clients.authenticate(Some(&header), u64::MAX).is_err());
    }

    // ---- group 3: authorization over the wire ----

    #[test]
    fn a_missing_scope_is_an_ok_http_reply_with_a_forbidden_tool_result() {
        let dir = TempDir::new("i4-http-forbidden");
        let server = start(dir.path(), false);
        let out = call(
            &server,
            "ci-1",
            SECRET_CI,
            "send_input",
            json!({"terminal": "work", "text": "ls"}),
        );
        assert_eq!(out["isError"], true);
        assert_eq!(code(&out), "forbidden", "{out}");
        assert_eq!(out["structuredContent"]["scope"], "control");
        assert!(server.fake.calls().is_empty());
        let records = server.sink.records();
        assert_eq!(records[0]["phase"], "denied");
        assert_eq!(records[0]["client"], "ci-1");
    }

    #[test]
    fn a_client_without_dialog_sends_zero_keys_over_http() {
        let dir = TempDir::new("i4-http-dialog");
        let server = start(dir.path(), false);
        let args = json!({"terminal": "work", "choice": "allow_once"});
        let out = call(&server, "ci-1", SECRET_CI, "answer_dialog", args.clone());
        assert_eq!(code(&out), "forbidden", "{out}");
        assert!(server.fake.state.lock().unwrap().keys.is_empty());
        assert!(server.fake.calls().is_empty());
        // alice holds the scope: the call goes through and a key is sent.
        let out = call(&server, "alice", SECRET_ALICE, "answer_dialog", args);
        assert_eq!(out["isError"], false, "{out}");
        assert_eq!(server.fake.state.lock().unwrap().keys.len(), 1);
    }

    #[test]
    fn a_client_without_start_arbitrary_cannot_pass_argv_even_when_the_server_allows_it() {
        let dir = TempDir::new("i4-http-argv");
        let server = start(dir.path(), true);
        for arguments in [
            json!({"argv": ["true"]}),
            json!({"line": "true"}),
            json!({"profile": "ci", "cwd": dir.path(), "argv": ["true"]}),
        ] {
            let out = call(
                &server,
                "alice",
                SECRET_ALICE,
                "start_pane",
                arguments.clone(),
            );
            assert_eq!(code(&out), "forbidden", "{arguments}: {out}");
        }
        assert!(server.fake.state.lock().unwrap().runs.is_empty());
        assert!(server.fake.calls().is_empty());
    }

    #[test]
    fn a_profile_outside_the_allowlist_is_denied_over_http() {
        let dir = TempDir::new("i4-http-profile");
        let server = start(dir.path(), false);
        let out = call(
            &server,
            "ci-1",
            SECRET_CI,
            "start_pane",
            json!({"profile": "other", "cwd": dir.path()}),
        );
        assert_eq!(code(&out), "forbidden", "{out}");
        assert!(server.fake.state.lock().unwrap().runs.is_empty());
        let out = call(
            &server,
            "ci-1",
            SECRET_CI,
            "start_pane",
            json!({"profile": "ci", "cwd": dir.path(), "name": "p"}),
        );
        assert_eq!(out["isError"], false, "{out}");
        assert_eq!(server.fake.state.lock().unwrap().runs.len(), 1);
        assert_eq!(server.sink.records().last().unwrap()["client"], "ci-1");
    }

    #[test]
    fn the_client_is_the_token_holder_and_nothing_the_request_claims() {
        let dir = TempDir::new("i4-http-identity");
        let server = start(dir.path(), false);
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "client": "alice",
            "params": {"name": "get_status", "client": "alice", "actor": "system:watchdog",
                       "arguments": {"terminal": "work", "client": "alice"}}});
        let bearer = format!("Bearer {}", token("ci-1", SECRET_CI));
        let host = format!("127.0.0.1:{}", server.port);
        let (status, _) = post(server.port, &host, Some(&bearer), &body.to_string());
        assert_eq!(status, 200);
        let records = server.sink.records();
        assert_eq!(records.last().unwrap()["client"], "ci-1");
        assert!(!server.sink.bytes().contains("system:watchdog"));
    }

    #[test]
    fn a_batch_cannot_carry_a_call_past_authorize_over_http() {
        let dir = TempDir::new("i4-http-batch");
        let server = start(dir.path(), false);
        let one = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "remove_pane", "arguments": {"terminal": "work"}}});
        let host = format!("127.0.0.1:{}", server.port);
        for who in [
            format!("Bearer {}", token("ci-1", SECRET_CI)),
            format!("Bearer {}", token("alice", SECRET_ALICE)),
        ] {
            let (status, body) = post(
                server.port,
                &host,
                Some(&who),
                &json!([one.clone(), one.clone()]).to_string(),
            );
            assert_eq!(status, 200, "{body}");
            assert!(body.contains("-32600"), "{body}");
        }
        let removing = json!({"jsonrpc": "2.0", "id": 1, "method": "remove_pane",
            "params": {"terminal": "work"}});
        let who = format!("Bearer {}", token("alice", SECRET_ALICE));
        let (_, body) = post(server.port, &host, Some(&who), &removing.to_string());
        assert!(body.contains("-32601"), "{body}");
        assert!(server.fake.calls().is_empty());
        assert_eq!(server.fake.state.lock().unwrap().panes.len(), 1);
    }
}
