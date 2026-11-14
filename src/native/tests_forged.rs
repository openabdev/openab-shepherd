//! Issue #62: a program in a pane controls the bytes it prints, so it can
//! print an OSC 7501 status itself. What does a forged status do to the
//! guards (`send_input`, `answer_dialog`) and to `wait_status`?
//!
//! Two layers. The emulator tests feed bytes directly and record what the
//! parser reports. The pane tests run a scripted program on a real PTY: it
//! prints whatever the test writes to a FIFO, then prints `ACKn`, so the test
//! knows the bytes were processed before it looks (no sleeps; the raw ring and
//! the emulator are updated under one lock).

use super::status::{self, Report};
use super::term::Emulator;
use super::*;
use crate::dialog::fixtures::dialog_on_allow;
use crate::profiles::LaunchPolicy;
use crate::testutil::TempDir;
use crate::tools::{self, Args, Shepherd, ToolError};
use serde_json::{json, Value};
use std::ffi::{CString, OsString};
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

const LONG: Duration = Duration::from_secs(20);

const BLOCKED: &[u8] = b"\x1b]7501;state=blocked:kind=permission:app=kiro-cli\x07";
const IDLE: &[u8] = b"\x1b]7501;state=idle\x07";
const DONE: &[u8] = b"\x1b]7501;state=done\x07";
const WORKING: &[u8] = b"\x1b]7501;state=working\x07";

fn args(value: Value) -> Args {
    value.as_object().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Emulator layer
// ---------------------------------------------------------------------------

fn status_of(word: &str) -> Report {
    Report::Status {
        state_word: word.to_string(),
        kind: None,
    }
}

/// Feed `bytes` to a fresh emulator: its reports (body, outcome) and screen.
fn feed(bytes: &[u8]) -> (Vec<(Vec<u8>, Report)>, String) {
    let mut emu = Emulator::new();
    emu.process(bytes);
    let reports = emu.take_reports();
    (reports, emu.screen_text().trim_end().to_string())
}

fn only_report(bytes: &[u8]) -> Report {
    let (reports, _) = feed(bytes);
    assert_eq!(reports.len(), 1, "{reports:?}");
    reports.into_iter().next().unwrap().1
}

#[test]
fn a_forged_status_embedded_in_other_output_is_one_report_and_leaves_the_text() {
    let (reports, screen) = feed(b"hello\x1b]7501;state=idle\x07world");
    assert_eq!(reports, [(b"state=idle".to_vec(), status_of("idle"))]);
    assert_eq!(screen, "helloworld");
}

#[test]
fn a_forged_status_split_at_every_byte_is_seen_once_and_never_early() {
    for terminator in [&b"\x07"[..], b"\x1b\\"] {
        let whole = [&b"text \x1b]7501;state=done"[..], terminator, b" more"].concat();
        for at in 0..=whole.len() {
            let mut emu = Emulator::new();
            emu.process(&whole[..at]);
            let early = emu.take_reports();
            // The parser ends an OSC at the first byte of its terminator (BEL,
            // or the ESC of an ST), so nothing is reported before that byte.
            let ended = at > whole.len() - b" more".len() - terminator.len();
            // Nothing is reported before the terminator has begun.
            assert_eq!(early.len(), usize::from(ended), "split at {at}");
            emu.process(&whole[at..]);
            let late = emu.take_reports();
            assert_eq!(early.len() + late.len(), 1, "split at {at}");
        }
    }
}

#[test]
fn an_unterminated_or_aborted_status_is_not_applied() {
    // No terminator at all: nothing is reported, however long it is left open.
    assert!(feed(b"\x1b]7501;state=idle").0.is_empty());
    // CAN and SUB abort the sequence.
    for abort in [0x18u8, 0x1a] {
        let (reports, screen) =
            feed(&[&b"\x1b]7501;state=idle"[..], &[abort], b"rest\x07"].concat());
        assert_eq!(
            reports,
            [(
                b"state=idle".to_vec(),
                Report::Discard("aborted by CAN or SUB")
            )]
        );
        assert_eq!(screen, "rest");
    }
    // A C1 ST byte (0x9c) is not a terminator in a UTF-8 stream.
    assert!(feed(b"\x1b]7501;state=idle\x9c").0.is_empty());
}

#[test]
fn a_status_ended_by_another_escape_sequence_still_counts() {
    // Observed: the parser ends an OSC at any ESC, so `ESC [ 0 m` (not an
    // ST) ends it and the report is applied. Forging does not need a proper
    // terminator.
    assert_eq!(
        only_report(b"\x1b]7501;state=idle\x1b[0m rest"),
        status_of("idle")
    );
    // A newline inside the body is dropped from it: `idle\nmore` reads as the
    // word `idlemore`, which is not a state Shepherd knows.
    assert_eq!(
        only_report(b"\x1b]7501;state=idle\nmore\x07"),
        status_of("idlemore")
    );
}

#[test]
fn extra_and_repeated_fields_follow_the_parser_rules() {
    assert_eq!(
        only_report(b"\x1b]7501;state=idle:app=evil:foo=bar\x07"),
        status_of("idle")
    );
    // The first state wins; the second cannot override it.
    assert_eq!(
        only_report(b"\x1b]7501;state=idle:state=blocked:kind=permission\x07"),
        status_of("idle")
    );
    // A kind is kept only for blocked.
    assert_eq!(
        only_report(b"\x1b]7501;kind=permission:state=idle\x07"),
        status_of("idle")
    );
    // A state word Shepherd does not know is still a report (it maps to `other`).
    assert_eq!(
        only_report(b"\x1b]7501;state=bypass\x07"),
        status_of("bypass")
    );
    // Non-ASCII fields are skipped.
    assert_eq!(
        only_report("\x1b]7501;state=idle:x=é\x07".as_bytes()),
        status_of("idle")
    );
}

#[test]
fn a_huge_field_is_discarded_and_memory_stays_bounded() {
    // A field far past the OSC body limit: discarded as truncated, and the
    // body kept for the log is at most the limit.
    let huge = [&b"\x1b]7501;state=idle:x="[..], &vec![b'a'; 4000], b"\x07"].concat();
    let (reports, _) = feed(&huge);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].1, Report::Discard("truncated"));
    assert!(
        reports[0].0.len() <= status::OSC_BODY_MAX,
        "{}",
        reports[0].0.len()
    );

    // Ten megabytes of an unterminated body, fed in 64 KiB reads: no report
    // while it is open; the terminator then yields one discard of bounded size
    // and the next real status parses normally.
    let mut emu = Emulator::new();
    emu.process(b"\x1b]7501;state=idle:x=");
    let chunk = vec![b'a'; 64 * 1024];
    for _ in 0..160 {
        emu.process(&chunk);
        assert!(emu.take_reports().is_empty());
    }
    emu.process(b"\x07\x1b]7501;state=working\x07");
    let reports = emu.take_reports();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_eq!(reports[0].1, Report::Discard("truncated"));
    assert!(reports[0].0.len() <= status::OSC_BODY_MAX);
    assert_eq!(reports[1].1, status_of("working"));

    // Too many fields is also discarded.
    let many = format!("\x1b]7501;{}\x07", "a=b;".repeat(40));
    assert_eq!(only_report(many.as_bytes()), Report::Discard("truncated"));
}

#[test]
fn a_status_smuggled_in_a_title_or_a_string_sequence() {
    // Text that merely contains `7501;state=idle` is not a status: a title
    // (OSC 0), and the payload of a DCS or APC string without an ESC.
    for inert in [
        &b"\x1b]0;7501;state=idle\x07"[..],
        b"\x1bPq7501;state=idle\x07\x1b\\",
        b"\x1b_]7501;state=idle\x07\x1b\\",
    ] {
        assert!(feed(inert).0.is_empty(), "{inert:?}");
    }
    // Observed: an ESC inside a title, DCS, APC, PM or SOS string starts a new
    // escape sequence, so a real OSC 7501 inside one IS a status. A string
    // sequence is no shelter from the parser, and no way around it either.
    for smuggled in [
        &b"\x1b]2;title\x1b]7501;state=idle\x07"[..],
        b"\x1bPq\x1b]7501;state=idle\x07\x1b\\",
        b"\x1b_x\x1b]7501;state=idle\x07\x1b\\",
        b"\x1b^x\x1b]7501;state=idle\x07\x1b\\",
        b"\x1bXx\x1b]7501;state=idle\x07\x1b\\",
        // On the alternate screen: the status does not care which screen is up.
        b"\x1b[?1049h\x1b]7501;state=idle\x07",
    ] {
        assert_eq!(only_report(smuggled), status_of("idle"), "{smuggled:?}");
    }
}

// ---------------------------------------------------------------------------
// Pane layer: a scripted program on a real PTY
// ---------------------------------------------------------------------------

/// A kiro (or arbitrary) pane whose program prints what the test says. The
/// program reports `idle`, then loops: read a line from the FIFO, `cat` the
/// file the line names, print `ACKn`.
struct Rig {
    dir: TempDir,
    b: NativeBackend,
    sh: Shepherd,
    fifo: PathBuf,
    name: &'static str,
    step: usize,
}

impl Rig {
    /// `profile`: start from a kiro launch profile (the dialect is known),
    /// else as an arbitrary command (no dialect).
    fn new(profile: bool) -> Self {
        let dir = TempDir::new("native-forged");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fifo = dir.path().join("ctl");
        let c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let script = bin.join("kiro-cli");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 exec 3<>{fifo}\n\
                 printf '\\033]7501;state=idle\\007ACK0\\n'\n\
                 n=0\n\
                 while IFS= read -r l <&3; do\n\
                 n=$((n+1))\n\
                 case \"$l\" in\n\
                 cat:*) cat \"${{l#cat:}}\"; printf 'ACK%d\\n' \"$n\" ;;\n\
                 catq:*) cat \"${{l#catq:}}\" ;;\n\
                 esac\n\
                 done\n",
                fifo = fifo.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("/usr/bin:/bin:{}", bin.display());
        let parent_path = path.clone();
        let b = NativeBackend::with_env_lookup(NativeConfig::default(), move |key| match key {
            "PATH" => Some(OsString::from(parent_path.clone())),
            _ => None,
        });
        let profiles = json!({
            "allow_arbitrary_command": true,
            "profiles": [{"name": "kiro", "argv": ["kiro-cli"], "cwds": [dir.path()]}]
        });
        let policy = LaunchPolicy::from_json(&profiles.to_string())
            .unwrap()
            .with_env_lookup(move |key| match key {
                "PATH" => Some(path.clone()),
                _ => None,
            });
        let sh = Shepherd::with_policy(b.clone(), policy);
        let name = "forged";
        let start = if profile {
            json!({"profile": "kiro", "name": name})
        } else {
            json!({"argv": [script], "name": name, "cwd": dir.path().to_str()})
        };
        tools::call(&sh, "start_pane", &args(start)).unwrap();
        let rig = Self {
            dir,
            b,
            sh,
            fifo,
            name,
            step: 0,
        };
        rig.wait_raw("ACK0");
        rig
    }

    fn reference(&self) -> TerminalRef {
        TerminalRef::new(self.name)
    }

    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.b.raw_output(&self.reference()).unwrap()).into_owned()
    }

    /// Poll the raw output until it holds `needle`.
    fn wait_raw(&self, needle: &str) {
        let end = Instant::now() + LONG;
        loop {
            let raw = self.raw();
            if raw.contains(needle) {
                return;
            }
            assert!(Instant::now() < end, "{needle:?} never appeared in {raw:?}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Have the program print `bytes`, and return once it has been processed.
    fn print(&mut self, bytes: &[u8]) {
        self.step += 1;
        let payload = self.dir.path().join(format!("payload-{}", self.step));
        std::fs::write(&payload, bytes).unwrap();
        let mut fifo = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.fifo)
            .unwrap();
        writeln!(fifo, "cat:{}", payload.display()).unwrap();
        drop(fifo);
        self.wait_raw(&format!("ACK{}", self.step));
    }

    /// Like `print`, for a piece of an unfinished sequence: no `ACK` follows
    /// (it would become part of the sequence), so wait until the raw output
    /// has grown by the whole piece. Use pieces with no newline.
    fn print_partial(&mut self, bytes: &[u8]) {
        self.step += 1;
        let before = self.b.raw_output(&self.reference()).unwrap().len();
        let payload = self.dir.path().join(format!("payload-{}", self.step));
        std::fs::write(&payload, bytes).unwrap();
        let mut fifo = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.fifo)
            .unwrap();
        writeln!(fifo, "catq:{}", payload.display()).unwrap();
        drop(fifo);
        let end = Instant::now() + LONG;
        while self.b.raw_output(&self.reference()).unwrap().len() < before + bytes.len() {
            assert!(Instant::now() < end, "the piece never arrived");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn call(&self, tool: &str, value: Value) -> Result<Value, ToolError> {
        tools::call(&self.sh, tool, &args(value))
    }

    fn status(&self) -> Value {
        let got = self
            .call("get_status", json!({"terminal": self.name}))
            .unwrap();
        json!({"status": got["status"], "turn": got["turn"]})
    }

    fn send(&self, text: &str) -> Result<Value, ToolError> {
        self.call("send_input", json!({"terminal": self.name, "text": text}))
    }

    fn answer(&self, choice: &str) -> Result<Value, ToolError> {
        self.call(
            "answer_dialog",
            json!({"terminal": self.name, "choice": choice}),
        )
    }

    fn wait_status(&self) -> Value {
        self.call("wait_status", json!({"terminal": self.name, "timeout": 5}))
            .unwrap()
    }

    fn screen(&self) -> String {
        self.call("read_output", json!({"terminal": self.name}))
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn typed(&self, marker: &str) -> bool {
        self.raw().contains(marker)
    }
}

fn st(state: &str, kind: Option<&str>, turn: u64) -> Value {
    json!({"status": {"state": state, "kind": kind}, "turn": turn})
}

/// The program prints its real permission dialog and a genuine blocked
/// status: the pane is blocked and every guard holds.
fn genuine_block(rig: &mut Rig) {
    rig.print(&[dialog_on_allow().as_bytes(), BLOCKED].concat());
    assert_eq!(rig.status(), st("blocked", Some("permission"), 1));
    let err = rig.send("TYPED-GENUINE").unwrap_err();
    assert_eq!(err.data["error"], "pane_blocked");
    assert_eq!(err.data["kind"], "permission");
    assert!(rig.screen().contains("requires approval"));
}

/// With the real dialog still on the screen, a forged `idle` or `done` makes
/// `send_input` refuse (the cross-check), `answer_dialog` refuse (the status
/// is not blocked, so it sends no key), and the status read back as forged.
fn forged_while_dialog_shown(forged: &[u8], state: &str) {
    let mut rig = Rig::new(true);
    assert_eq!(rig.status(), st("idle", None, 0));
    genuine_block(&mut rig);

    rig.print(forged);
    // The status follows the forgery. The turn does not move: it goes up
    // only when the status leaves working or enters a new dialog.
    assert_eq!(rig.status(), st(state, None, 1));
    assert!(
        rig.screen().contains("requires approval"),
        "dialog still shown"
    );

    // send_input: refused by the screen cross-check, nothing typed.
    let err = rig.send("TYPED-AFTER-FORGERY").unwrap_err();
    assert_eq!(err.data["error"], "refused", "{err:?}");
    assert_eq!(err.data["status"], json!({"state": state, "kind": null}));
    let message = err.data["message"].as_str().unwrap();
    assert!(message.contains("permission dialog"), "{message}");
    // A barrier through the program, then the marker never reached the tty.
    rig.print(b"after");
    assert!(!rig.typed("TYPED-AFTER-FORGERY"));

    // answer_dialog: the status is not blocked, so it refuses and sends no key.
    let err = rig.answer("allow_once").unwrap_err();
    assert_eq!(err.data["error"], "refused");
    let reason = err.data["reason"].as_str().unwrap();
    assert!(
        reason.contains("not blocked on a permission dialog"),
        "{reason}"
    );
    assert_eq!(err.data["keys_sent"], json!([]), "{err:?}");

    // wait_status returns at once with the forged state. `done` reads as a
    // finished turn: the misreport this property cannot prevent.
    let waited = rig.wait_status();
    assert_eq!(waited["status"], state);
    assert_eq!(waited["turn"], 1);
    if state == "done" {
        assert_eq!(waited["outcome"], "done");
        assert_eq!(waited["turn_complete"], true);
    } else {
        assert_eq!(waited["outcome"], "other");
        assert_eq!(waited["turn_complete"], false);
    }

    // The real program blocks again (its dialog is unchanged, but the status
    // came back from idle/done): a new turn, and the guards hold again.
    rig.print(BLOCKED);
    assert_eq!(rig.status(), st("blocked", Some("permission"), 2));
    assert_eq!(
        rig.send("TYPED-REBLOCKED").unwrap_err().data["error"],
        "pane_blocked"
    );
}

#[test]
fn a_forged_idle_over_a_real_dialog_refuses_send_input() {
    forged_while_dialog_shown(IDLE, "idle");
}

#[test]
fn a_forged_done_over_a_real_dialog_refuses_send_input() {
    forged_while_dialog_shown(DONE, "done");
}

/// A pane with no launch profile has no dialect, so there is no screen to
/// cross-check: a forged idle over a real dialog lets `send_input` type.
#[test]
fn without_a_dialect_a_forged_idle_still_lets_send_input_type() {
    let mut rig = Rig::new(false);
    genuine_block(&mut rig);
    rig.print(IDLE);
    assert_eq!(rig.status(), st("idle", None, 1));
    let sent = rig.send("TYPED-NO-DIALECT").unwrap();
    assert_eq!(sent["status"], "idle");
    rig.wait_raw("TYPED-NO-DIALECT");
}

/// A forged `working` over a real dialog is not covered by the cross-check:
/// `send_input` allows a working pane, and a genuine pane can show a dialog
/// for a moment after it was answered. Recorded as it is.
#[test]
fn a_forged_working_over_a_real_dialog_still_lets_send_input_type() {
    let mut rig = Rig::new(true);
    genuine_block(&mut rig);
    rig.print(WORKING);
    assert_eq!(rig.status(), st("working", None, 1));
    let sent = rig.send("TYPED-WHILE-WORKING").unwrap();
    assert_eq!(sent["status"], "working");
    rig.wait_raw("TYPED-WHILE-WORKING");
    // Leaving working moves the turn, so a send conditioned on the old turn
    // would be refused: working then idle is turn 2.
    rig.print(IDLE);
    assert_eq!(rig.status(), st("idle", None, 2));
    assert_eq!(rig.send("X").unwrap_err().data["error"], "refused");
}

/// The cross-check reads the visible screen. A dialog drawn on the main
/// screen and hidden behind the alternate screen is not seen. Recorded as it
/// is: a program that draws the dialog and then hides it has full control.
#[test]
fn a_dialog_hidden_behind_the_alternate_screen_is_not_cross_checked() {
    let mut rig = Rig::new(true);
    genuine_block(&mut rig);
    rig.print(&[&b"\x1b[?1049h"[..], IDLE].concat());
    assert_eq!(rig.status(), st("idle", None, 1));
    assert!(!rig.screen().contains("requires approval"));
    rig.send("TYPED-HIDDEN").unwrap();
    rig.wait_raw("TYPED-HIDDEN");
    // The dialog drawn on the alternate screen itself is seen.
    let mut rig = Rig::new(true);
    rig.print(&[&b"\x1b[?1049h"[..], dialog_on_allow().as_bytes(), BLOCKED].concat());
    rig.print(IDLE);
    assert_eq!(rig.send("TYPED-ALT").unwrap_err().data["error"], "refused");
}

/// A forged `blocked` over an idle pane: `send_input` is refused (a denial of
/// service), `answer_dialog` finds no dialog and sends nothing,
/// `interrupt_pane` still reaches the program, and the pane recovers at the
/// next genuine status.
#[test]
fn a_forged_blocked_refuses_send_input_but_the_operator_can_still_act() {
    let mut rig = Rig::new(true);
    assert_eq!(rig.status(), st("idle", None, 0));
    rig.print(BLOCKED);
    assert_eq!(rig.status(), st("blocked", Some("permission"), 1));

    let err = rig.send("TYPED-FORGED-BLOCK").unwrap_err();
    assert_eq!(err.data["error"], "pane_blocked");
    assert_eq!(err.data["kind"], "permission");

    let waited = rig.wait_status();
    assert_eq!(waited["outcome"], "blocked");
    assert_eq!(waited["kind"], "permission");

    // No dialog is on the screen: answer_dialog refuses and sends no key.
    let err = rig.answer("allow_once").unwrap_err();
    assert_eq!(err.data["error"], "refused");
    let reason = err.data["reason"].as_str().unwrap();
    assert!(
        reason.contains("cannot parse the permission dialog"),
        "{reason}"
    );
    assert_eq!(err.data["keys_sent"], json!([]), "{err:?}");

    // interrupt_pane is not guarded by the status: it sends Esc.
    let out = rig
        .call(
            "interrupt_pane",
            json!({"terminal": rig.name, "timeout": 5}),
        )
        .unwrap();
    assert_eq!(out["key"], "esc");
    assert_eq!(out["outcome"], "stopped");

    // The next genuine status ends it, and sends work again.
    rig.print(IDLE);
    assert_eq!(rig.status(), st("idle", None, 1));
    rig.send("TYPED-AFTER-RECOVERY").unwrap();
    rig.wait_raw("TYPED-AFTER-RECOVERY");
    assert!(!rig.typed("TYPED-FORGED-BLOCK"));
}

/// A status split over two reads changes nothing until it is complete.
#[test]
fn a_forged_status_split_across_reads_applies_only_when_complete() {
    let mut rig = Rig::new(true);
    genuine_block(&mut rig);
    rig.print_partial(b"\x1b]7501;state=id");
    assert_eq!(rig.status(), st("blocked", Some("permission"), 1));
    rig.print_partial(b"le");
    assert_eq!(rig.status(), st("blocked", Some("permission"), 1));
    rig.print(b"\x07");
    assert_eq!(rig.status(), st("idle", None, 1));
}

/// Statuses that are discarded, or that are aborted, change nothing.
#[test]
fn a_discarded_or_aborted_forgery_leaves_the_status_alone() {
    let mut rig = Rig::new(true);
    genuine_block(&mut rig);
    let huge = [&b"\x1b]7501;state=idle:x="[..], &vec![b'a'; 4000], b"\x07"].concat();
    rig.print(&huge);
    rig.print(b"\x1b]7501;state=idle\x18rest\x07");
    rig.print(b"\x1b]7501;state=bypass-not-a-state-word-that-is-far-too-long-for-a-word\x07");
    assert_eq!(rig.status(), st("blocked", Some("permission"), 1));
    let log = rig.b.status_log(&rig.reference()).unwrap();
    let outcomes: Vec<&str> = log.iter().map(|e| e.outcome.as_str()).collect();
    assert_eq!(outcomes.len(), 5, "{outcomes:?}");
    assert!(outcomes[2].starts_with("discarded"), "{outcomes:?}");
    assert!(outcomes[3].starts_with("discarded"), "{outcomes:?}");
    assert!(outcomes[4].starts_with("discarded"), "{outcomes:?}");
    assert!(log.iter().all(|e| e.body.len() <= status::OSC_BODY_MAX));
}

/// There is no cap on how fast a pane may flip its state: the status log keeps
/// the last 64 entries, and the turn counts every flip that leaves working.
#[test]
fn a_flood_of_flips_is_bounded_in_the_log_but_not_in_the_turn() {
    let mut rig = Rig::new(true);
    let mut flood = Vec::new();
    for _ in 0..100 {
        flood.extend_from_slice(WORKING);
        flood.extend_from_slice(IDLE);
    }
    rig.print(&flood);
    // 100 leaves of working, none before the first report: turn 100.
    assert_eq!(rig.status(), st("idle", None, 100));
    let log = rig.b.status_log(&rig.reference()).unwrap();
    assert_eq!(log.len(), status::STATUS_LOG_LEN);
}
