//! JSON-RPC 2.0 client for the herdr-core Unix socket.
//!
//! Plain mode: one request per connection, one JSON object per line, no `hello`.
//! Replies whose id does not match the request (notifications, other ids) are
//! skipped. Whether herdr-core requires `hello` before other calls is unverified;
//! the integration step checks it.

use serde_json::{json, Map, Value};
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds added to a herdr-core wait timeout, so the socket outlives the wait.
pub const WAIT_SOCKET_SLACK: Duration = Duration::from_secs(15);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A failed call. `kind` is either a local class (`unavailable`, `transport`,
/// `timeout`, `protocol`) or the `data.kind` herdr-core attached to its error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrError {
    pub message: String,
    pub code: Option<i64>,
    pub kind: Option<String>,
}

impl HerdrError {
    fn local(kind: &str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
            kind: Some(kind.to_string()),
        }
    }
}

impl fmt::Display for HerdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            Some(kind) => write!(f, "{} ({kind})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for HerdrError {}

#[derive(Debug, Clone)]
pub struct HerdrClient {
    socket: PathBuf,
}

impl HerdrClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// Send one request and return its `result`. `timeout` bounds the socket
    /// reads and writes for this call.
    pub fn call(
        &self,
        method: &str,
        params: Map<String, Value>,
        timeout: Duration,
    ) -> Result<Value, HerdrError> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        // Drop null values so optional schema parameters are simply absent.
        let params: Map<String, Value> = params.into_iter().filter(|(_, v)| !v.is_null()).collect();
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        // serde_json escapes newlines inside strings, so the newline below is the
        // only frame delimiter on the line.
        let mut line = serde_json::to_string(&request)
            .map_err(|err| HerdrError::local("protocol", format!("cannot encode request: {err}")))?;
        line.push('\n');

        let reply = self.exchange(line.as_bytes(), id, timeout)?;
        if let Some(err) = reply.get("error") {
            let message = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("herdr-core error")
                .to_string();
            let code = err.get("code").and_then(Value::as_i64);
            let kind = err
                .get("data")
                .and_then(|data| data.get("kind"))
                .and_then(Value::as_str)
                .map(str::to_string);
            return Err(HerdrError { message, code, kind });
        }
        reply
            .get("result")
            .cloned()
            .ok_or_else(|| HerdrError::local("protocol", "herdr-core reply has neither result nor error"))
    }

    fn exchange(&self, payload: &[u8], id: u64, timeout: Duration) -> Result<Value, HerdrError> {
        let mut stream = UnixStream::connect(&self.socket).map_err(|err| {
            HerdrError::local("unavailable", format!("cannot connect to herdr-core socket: {err}"))
        })?;
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(timeout)))
            .map_err(|err| HerdrError::local("transport", format!("cannot set socket timeout: {err}")))?;
        stream
            .write_all(payload)
            .map_err(|err| HerdrError::local("transport", format!("send failed: {err}")))?;
        read_reply(&mut stream, id)
    }
}

/// Read newline-delimited messages until the one with `id` arrives.
fn read_reply<R: Read>(reader: &mut R, id: u64) -> Result<Value, HerdrError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=pos).collect();
            let line = &raw[..raw.len() - 1];
            if line.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            let message: Value = serde_json::from_slice(line)
                .map_err(|err| HerdrError::local("protocol", format!("malformed reply line: {err}")))?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(message);
            }
            // A notification or a reply to another id: skip it.
        }
        let n = match reader.read(&mut chunk) {
            Ok(n) => n,
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err(HerdrError::local("timeout", "timed out waiting for herdr-core reply"));
            }
            Err(err) => {
                return Err(HerdrError::local("transport", format!("receive failed: {err}")));
            }
        };
        if n == 0 {
            return Err(HerdrError::local(
                "transport",
                "herdr-core closed the connection before replying",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REPLY_BYTES {
            return Err(HerdrError::local("protocol", "herdr-core reply exceeds size limit"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{reply, serve_once, TempDir};
    use std::thread;

    fn params(pairs: Vec<(&str, Value)>) -> Map<String, Value> {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn request_is_one_json_line_with_jsonrpc_fields() {
        let dir = TempDir::new("frame");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, req| {
            reply(s, &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"ok": true}}));
        });
        let result = HerdrClient::new(&sock)
            .call("terminal.list", params(vec![("all", json!(false))]), DEFAULT_TIMEOUT)
            .unwrap();
        assert_eq!(result, json!({"ok": true}));

        let (line, request) = handle.join().unwrap();
        assert_eq!(line.matches('\n').count(), 1, "exactly one newline, at the end");
        assert!(line.ends_with('\n'));
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["method"], "terminal.list");
        assert_eq!(request["params"], json!({"all": false}));
        assert!(request["id"].is_u64());
    }

    #[test]
    fn null_params_are_omitted() {
        let dir = TempDir::new("nulls");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, req| {
            reply(s, &json!({"jsonrpc": "2.0", "id": req["id"], "result": {}}));
        });
        HerdrClient::new(&sock)
            .call(
                "terminal.run",
                params(vec![("argv", json!(["sh"])), ("cwd", Value::Null)]),
                DEFAULT_TIMEOUT,
            )
            .unwrap();
        let (_, request) = handle.join().unwrap();
        assert_eq!(request["params"], json!({"argv": ["sh"]}));
    }

    #[test]
    fn notification_and_mismatched_id_are_skipped() {
        let dir = TempDir::new("skip");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, req| {
            let id = req["id"].as_u64().unwrap();
            reply(s, &json!({"jsonrpc": "2.0", "method": "event", "params": {}}));
            reply(s, &json!({"jsonrpc": "2.0", "id": id + 999, "result": "wrong"}));
            reply(s, &json!({"jsonrpc": "2.0", "id": id, "result": "right"}));
        });
        let result = HerdrClient::new(&sock)
            .call("terminal.get", params(vec![("terminal", json!("t1"))]), DEFAULT_TIMEOUT)
            .unwrap();
        handle.join().unwrap();
        assert_eq!(result, json!("right"));
    }

    #[test]
    fn reply_split_across_writes_is_reassembled() {
        let dir = TempDir::new("split");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, req| {
            let data = format!(
                "{}\n",
                json!({"jsonrpc": "2.0", "id": req["id"], "result": {"text": "héllo 你好"}})
            );
            let bytes = data.as_bytes();
            let mid = bytes.len() / 2;
            s.write_all(&bytes[..mid]).unwrap();
            thread::sleep(Duration::from_millis(50));
            s.write_all(&bytes[mid..]).unwrap();
        });
        let result = HerdrClient::new(&sock)
            .call("terminal.read", params(vec![("terminal", json!("t1"))]), DEFAULT_TIMEOUT)
            .unwrap();
        handle.join().unwrap();
        assert_eq!(result, json!({"text": "héllo 你好"}));
    }

    #[test]
    fn error_reply_carries_code_and_kind() {
        let dir = TempDir::new("err");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, req| {
            reply(
                s,
                &json!({"jsonrpc": "2.0", "id": req["id"], "error": {
                    "code": -32000, "message": "terminal is blocked", "data": {"kind": "blocked"}}}),
            );
        });
        let err = HerdrClient::new(&sock)
            .call("terminal.send", params(vec![("terminal", json!("t1"))]), DEFAULT_TIMEOUT)
            .unwrap_err();
        handle.join().unwrap();
        assert_eq!(err.kind.as_deref(), Some("blocked"));
        assert_eq!(err.code, Some(-32000));
        assert!(err.to_string().contains("terminal is blocked"));
    }

    #[test]
    fn closed_without_reply_is_an_error() {
        let dir = TempDir::new("closed");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |_s, _req| {});
        let err = HerdrClient::new(&sock)
            .call("terminal.list", Map::new(), DEFAULT_TIMEOUT)
            .unwrap_err();
        handle.join().unwrap();
        assert!(err.message.contains("closed the connection"), "{err}");
    }

    #[test]
    fn malformed_reply_is_an_error() {
        let dir = TempDir::new("malformed");
        let sock = dir.path().join("core.sock");
        let handle = serve_once(&sock, |s, _req| {
            s.write_all(b"not json\n").unwrap();
        });
        let err = HerdrClient::new(&sock)
            .call("terminal.list", Map::new(), DEFAULT_TIMEOUT)
            .unwrap_err();
        handle.join().unwrap();
        assert!(err.message.contains("malformed"), "{err}");
        assert_eq!(err.kind.as_deref(), Some("protocol"));
    }

    #[test]
    fn missing_socket_reports_unavailable() {
        let dir = TempDir::new("absent");
        let sock = dir.path().join("absent.sock");
        let err = HerdrClient::new(&sock)
            .call("terminal.list", Map::new(), DEFAULT_TIMEOUT)
            .unwrap_err();
        assert_eq!(err.kind.as_deref(), Some("unavailable"));
    }
}
