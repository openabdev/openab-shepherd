//! Helpers shared by unit tests: scratch directories, a fake herdr-core socket,
//! and a git repository initializer. Compiled only for `cargo test`.

use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

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

/// Bind a fake herdr-core socket, accept one connection, and read one request line.
/// `respond` writes whatever reply the test needs. The returned handle yields the
/// raw request line (with its newline) and its parsed JSON.
pub fn serve_once<F>(socket: &Path, respond: F) -> JoinHandle<(String, Value)>
where
    F: FnOnce(&mut UnixStream, &Value) + Send + 'static,
{
    // Bind before returning so the client cannot race the listener.
    let listener = UnixListener::bind(socket).expect("bind fake herdr-core socket");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read request line");
        let request: Value = serde_json::from_str(&line).expect("request is JSON");
        respond(&mut stream, &request);
        (line, request)
    })
}

/// Write one reply line.
pub fn reply(stream: &mut UnixStream, payload: &Value) {
    writeln!(stream, "{payload}").expect("write reply");
}
