//! A fake herdr-core socket for tests of the hc adapter. Compiled only for
//! `cargo test`.

use crate::dialog::fixtures::{dialog_on_allow, dialog_on_deny};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

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

/// A fake herdr-core socket that answers a sequence of calls, one per connection.
pub struct ScriptedHerdr {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Vec<Value>>,
}

impl ScriptedHerdr {
    /// Stop accepting and return every request received, in order. Call it after
    /// the client code returns: each client call blocks until its reply, so every
    /// connection has been handled by then.
    pub fn finish(self) -> Vec<Value> {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.join().expect("scripted server thread")
    }
}

/// Bind a fake herdr-core socket that answers the Nth call with `script[N]`.
/// Each script entry is `{"result": ...}` or `{"error": ...}`; the server adds
/// `jsonrpc` and the request id. A call past the end of the script gets an
/// error with kind `unexpected_call`.
pub fn serve_script(socket: &Path, script: Vec<Value>) -> ScriptedHerdr {
    let listener = UnixListener::bind(socket).expect("bind fake herdr-core socket");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let mut script = script.into_iter();
        let mut requests = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).expect("blocking stream");
                    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read request line");
                    let request: Value = serde_json::from_str(&line).expect("request is JSON");
                    let mut payload = script.next().unwrap_or_else(|| {
                        json!({"error": {"code": -32000, "message": "unexpected call",
                                         "data": {"kind": "unexpected_call"}}})
                    });
                    payload["jsonrpc"] = json!("2.0");
                    payload["id"] = request["id"].clone();
                    reply(&mut stream, &payload);
                    requests.push(request);
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    if stop_flag.load(Ordering::SeqCst) {
                        return requests;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("accept: {err}"),
            }
        }
    });
    ScriptedHerdr { stop, handle }
}

/// Write one reply line.
pub fn reply(stream: &mut UnixStream, payload: &Value) {
    writeln!(stream, "{payload}").expect("write reply");
}

/// The one pane a [`PaneHerdr`] serves: address `t8`, name `work`.
pub const PANE_ADDRESS: &str = "t8";
pub const PANE_NAME: &str = "work";

/// State of the fake pane. It starts blocked on the probe's permission dialog
/// with the cursor on Allow once.
#[derive(Debug)]
pub struct PaneModel {
    pub blocked: bool,
    pub turn: u64,
    pub cursor: usize,
    /// Every request, in the order the fake received it.
    pub log: Vec<Value>,
    /// The cursor position at each Enter pressed while blocked.
    pub selected: Vec<usize>,
    reads: usize,
}

/// A fake herdr-core with one pane that answers each connection on its own
/// thread, so concurrent client calls really overlap. It can pause before
/// replying to the Nth terminal.read, which lets a test fix the order of two
/// calls without sleeping.
pub struct PaneHerdr {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
    model: Arc<Mutex<PaneModel>>,
    paused: Receiver<()>,
    release: Sender<()>,
}

impl PaneHerdr {
    /// Wait until the paused read has arrived (its reply is held back).
    pub fn wait_paused(&self) {
        self.paused
            .recv_timeout(Duration::from_secs(10))
            .expect("the paused terminal.read never arrived");
    }

    /// Let the paused read reply.
    pub fn release(&self) {
        self.release.send(()).expect("release the paused read");
    }

    /// Stop the fake and return the final model. Call it after every client
    /// call has returned.
    pub fn finish(self) -> PaneModel {
        self.stop.store(true, Ordering::SeqCst);
        self.handle.join().expect("pane server thread");
        let model = Arc::try_unwrap(self.model).expect("handlers finished");
        model.into_inner().expect("model lock")
    }
}

/// Serve the fake pane on `socket`. `pause_at_read` is the 1-based index of the
/// terminal.read to hold until [`PaneHerdr::release`].
pub fn serve_pane(socket: &Path, pause_at_read: Option<usize>) -> PaneHerdr {
    let listener = UnixListener::bind(socket).expect("bind fake herdr-core socket");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let model = Arc::new(Mutex::new(PaneModel {
        blocked: true,
        turn: 1,
        cursor: 0,
        log: Vec::new(),
        selected: Vec::new(),
        reads: 0,
    }));
    let (paused_tx, paused) = mpsc::channel();
    let (release, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let shared = Arc::clone(&model);
    let handle = thread::spawn(move || {
        let mut handlers = Vec::new();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let model = Arc::clone(&shared);
                    let paused_tx = paused_tx.clone();
                    let release_rx = Arc::clone(&release_rx);
                    handlers.push(thread::spawn(move || {
                        pane_connection(stream, &model, pause_at_read, &paused_tx, &release_rx)
                    }));
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("accept: {err}"),
            }
        }
        for handler in handlers {
            handler.join().expect("pane connection thread");
        }
    });
    PaneHerdr {
        stop,
        handle,
        model,
        paused,
        release,
    }
}

fn pane_connection(
    mut stream: UnixStream,
    model: &Mutex<PaneModel>,
    pause_at_read: Option<usize>,
    paused_tx: &Sender<()>,
    release_rx: &Mutex<Receiver<()>>,
) {
    stream.set_nonblocking(false).expect("blocking stream");
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut line = String::new();
    reader.read_line(&mut line).expect("read request line");
    let request: Value = serde_json::from_str(&line).expect("request is JSON");
    let (mut payload, pause) = {
        let mut pane = model.lock().expect("model lock");
        pane.log.push(request.clone());
        let mut pause = false;
        if request["method"] == "terminal.read" {
            pane.reads += 1;
            pause = Some(pane.reads) == pause_at_read;
        }
        (pane_reply(&mut pane, &request), pause)
    };
    if pause {
        paused_tx.send(()).expect("signal pause");
        release_rx
            .lock()
            .expect("release lock")
            .recv()
            .expect("wait for release");
    }
    payload["jsonrpc"] = json!("2.0");
    payload["id"] = request["id"].clone();
    reply(&mut stream, &payload);
}

fn pane_reply(pane: &mut PaneModel, request: &Value) -> Value {
    let params = &request["params"];
    let terminal = params["terminal"].as_str().unwrap_or_default();
    if terminal != PANE_ADDRESS && terminal != PANE_NAME {
        return json!({"error": {"code": -32000, "message": "no such terminal",
                                "data": {"kind": "not_found"}}});
    }
    let status_word = if pane.blocked { "blocked" } else { "working" };
    match request["method"].as_str().unwrap_or_default() {
        "terminal.get" => {
            let status = if pane.blocked {
                json!({"app": "kiro-cli", "kind": "permission", "state": "blocked"})
            } else {
                json!({"app": "kiro-cli", "state": "working"})
            };
            json!({"result": {"address": PANE_ADDRESS, "name": PANE_NAME, "state": "live",
                              "turn": pane.turn, "reported": true, "status": status}})
        }
        "terminal.read" => {
            let text = match (pane.blocked, pane.cursor) {
                (false, _) => "working".to_string(),
                (true, 1) => dialog_on_deny(),
                (true, _) => dialog_on_allow(),
            };
            json!({"result": {"text": text, "complete": false, "how": "screen"}})
        }
        "terminal.keys" => {
            for key in params["keys"].as_array().into_iter().flatten() {
                match key.as_str() {
                    Some("down") => pane.cursor = (pane.cursor + 1).min(3),
                    Some("up") => pane.cursor = pane.cursor.saturating_sub(1),
                    Some("enter") if pane.blocked => {
                        pane.selected.push(pane.cursor);
                        pane.blocked = false;
                    }
                    _ => {}
                }
            }
            let status_word = if pane.blocked { "blocked" } else { "working" };
            json!({"result": {"status": status_word, "turn": pane.turn}})
        }
        "terminal.send" if pane.blocked => {
            json!({"error": {"code": -32000, "message": "t8 is blocked (permission)",
                             "data": {"kind": "blocked"}}})
        }
        "terminal.send" => {
            json!({"result": {"started": true, "status": status_word, "turn": pane.turn}})
        }
        _ => json!({"error": {"code": -32000, "message": "unexpected call",
                              "data": {"kind": "unexpected_call"}}}),
    }
}
