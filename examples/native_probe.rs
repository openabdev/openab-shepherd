//! Real-Kiro probe for the native backend (ADR §14).
//!
//! Drives `tools::call` on a `Shepherd` over `NativeBackend`, so the safety
//! checks of the tools (pane_blocked, answer_dialog) run unchanged. It opens no
//! socket and starts nothing except through `NativeBackend`.
//!
//! Usage: `native_probe [--keep-dir]`. It creates
//! `/tmp/shepherd-native-probe-<pid>/` with a `probe` agent and starts the
//! launch profile `kiro-probe` there: `kiro-cli chat --agent probe
//! --trust-workspace`, with `KIRO_API_KEY` read from the keychain item
//! `openab-shepherd-kiro-api-key` into that child only. It sends at most
//! three prompts. Exit 0 only when every step went as expected.

use openab_shepherd::backend::TerminalRef;
use openab_shepherd::dialog;
use openab_shepherd::native::{NativeBackend, NativeConfig, StatusLogEntry};
use openab_shepherd::profiles::LaunchPolicy;
use openab_shepherd::tools::{self, Args, Shepherd, ToolError};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PANE: &str = "probe";
const PROFILE: &str = "kiro-probe";
const MARKER: &str = "shepherd_native_marker.txt";
const AGENT_JSON: &str =
    r#"{"name":"probe","tools":["@builtin"],"allowedTools":[],"toolsSettings":{}}"#;
const PROMPT_OK: &str = "Reply with only the word OK. Do not use any tools.";
const PROMPT_TOUCH: &str = "Use the shell tool to run exactly this command in the current directory and nothing else: touch shepherd_native_marker.txt";
/// The probe's own budget per run (the owner allowed 3 more for the
/// launch-profile run, after 3 in the earlier runs).
const MAX_PROMPTS: usize = 3;
const START_TIMEOUT: Duration = Duration::from_secs(60);
const TURN_TIMEOUT: Duration = Duration::from_secs(300);
const DIALOG_TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(200);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let keep_dir = match args.as_slice() {
        [] => false,
        [flag] if flag == "--keep-dir" => true,
        _ => {
            eprintln!("usage: native_probe [--keep-dir]");
            return ExitCode::from(2);
        }
    };
    let dir = PathBuf::from(format!("/tmp/shepherd-native-probe-{}", std::process::id()));
    if let Err(err) = write_agent(&dir) {
        eprintln!("FAIL: cannot create {}: {err}", dir.display());
        return ExitCode::FAILURE;
    }
    let profiles = json!({"profiles": [{
        "name": PROFILE,
        "argv": ["kiro-cli", "chat", "--agent", "probe", "--trust-workspace"],
        "cwds": [dir],
        "env": {"KIRO_API_KEY": "keychain:openab-shepherd-kiro-api-key"},
    }]});
    let policy = match LaunchPolicy::from_json(&profiles.to_string()) {
        Ok(policy) => policy,
        Err(err) => {
            eprintln!("FAIL: launch profile: {err}");
            return ExitCode::FAILURE;
        }
    };
    let native = NativeBackend::new(NativeConfig::default());
    let mut probe = Probe {
        sh: Shepherd::with_policy(native.clone(), policy),
        native,
        dir: dir.clone(),
        prompts: 0,
        log: Vec::new(),
        log_gaps: 0,
        t0_ms: now_ms(),
    };
    println!("probe: dir {}", dir.display());
    println!(
        "probe: profile {PROFILE}: argv [\"kiro-cli\",\"chat\",\"--agent\",\"probe\",\"--trust-workspace\"], env KIRO_API_KEY from keychain:openab-shepherd-kiro-api-key, keep_dir {keep_dir}"
    );
    let result = probe.run();
    probe.snapshot_log();
    probe.print_transitions();
    let turns_ok = probe.check_turns();
    println!("probe: Kiro prompts used: {}", probe.prompts);
    if let Err(err) = probe.native.remove(&TerminalRef::new(PANE)) {
        println!("probe: remove failed: {err:?}");
    }
    if keep_dir {
        println!("probe: kept {}", dir.display());
    } else if let Err(err) = std::fs::remove_dir_all(&dir) {
        println!("probe: cannot delete {}: {err}", dir.display());
    }
    match (result, turns_ok) {
        (Ok(()), Ok(())) => {
            println!("PASS");
            ExitCode::SUCCESS
        }
        (Err(err), _) | (Ok(()), Err(err)) => {
            println!("FAIL: {err}");
            ExitCode::FAILURE
        }
    }
}

fn write_agent(dir: &Path) -> std::io::Result<()> {
    let agents = dir.join(".kiro").join("agents");
    std::fs::create_dir_all(&agents)?;
    std::fs::write(agents.join("probe.json"), AGENT_JSON)
}

struct Probe {
    sh: Shepherd,
    native: NativeBackend,
    dir: PathBuf,
    prompts: usize,
    /// Every status-log entry seen, merged across snapshots (the log keeps 64).
    log: Vec<StatusLogEntry>,
    /// Snapshots that did not overlap the previous one (entries were lost).
    log_gaps: usize,
    t0_ms: u64,
}

impl Probe {
    fn run(&mut self) -> Result<(), String> {
        // Start.
        let started = self.call(
            "start_pane",
            json!({
                "profile": PROFILE,
                "cwd": self.dir.to_str(),
                "name": PANE,
            }),
        )?;
        println!("start_pane: {started}");

        // 1. Wait for idle; never press keys blindly.
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            let status = self.call("get_status", json!({"terminal": PANE}))?;
            self.snapshot_log();
            if status["status"]["state"] == "idle" {
                println!("step 1: idle: {status}");
                break;
            }
            if status["live"] == false {
                self.print_screen();
                return Err(format!("step 1: kiro-cli exited before idle: {status}"));
            }
            if Instant::now() >= deadline {
                self.print_screen();
                return Err(format!(
                    "step 1: no idle status within {} s: {status}",
                    START_TIMEOUT.as_secs()
                ));
            }
            thread::sleep(POLL);
        }
        self.snapshot_log();

        // 2. Prompt 1.
        self.prompt(PROMPT_OK)?;
        let done = self.wait_end(1)?;
        self.expect(&done, "done", 1, "step 2 (prompt 1)")?;

        // 3. Prompt 2: the shell tool needs permission.
        self.prompt(PROMPT_TOUCH)?;
        let blocked = self.wait_end(2)?;
        self.expect(&blocked, "blocked", 2, "step 3 (prompt 2)")?;
        if blocked["kind"] != "permission" {
            return Err(format!(
                "step 3: blocked with kind {}, not permission",
                blocked["kind"]
            ));
        }

        // 4. The raw blocked report and the keyboard mode.
        self.print_blocked_report("step 4");

        // 5. Typing into a blocked pane is refused.
        match self.call_raw(
            "send_input",
            json!({"terminal": PANE, "text": "this text must not be typed"}),
        ) {
            Err(err) if err.data["error"] == "pane_blocked" => {
                println!("step 5: send_input refused: {}", err.data);
            }
            Err(err) => return Err(format!("step 5: send_input failed with {}", err.data)),
            Ok(out) => {
                return Err(format!(
                    "step 5: send_input was accepted while blocked: {out}"
                ))
            }
        }

        // 6. Parse the dialog from read_output.
        let parsed = self.parse_dialog("step 6")?;
        println!(
            "step 6: dialog title {:?}\nstep 6: dialog command {:?}\nstep 6: dialog options {:?}\nstep 6: dialog cursor {}",
            parsed.title, parsed.command, parsed.options, parsed.cursor
        );

        // 7. Deny once: no marker.
        let answer = self.call(
            "answer_dialog",
            json!({"terminal": PANE, "choice": "deny_once"}),
        )?;
        println!("step 7: answer_dialog deny_once: {answer}");
        let after_deny = self.wait_end(3)?;
        self.expect(&after_deny, "done", 3, "step 7 (deny_once)")?;
        if self.marker().exists() {
            return Err("step 7: the marker exists after deny_once".to_string());
        }
        println!("step 7: marker absent after deny_once");

        // 8. Prompt 3: allow once, marker present.
        self.prompt(PROMPT_TOUCH)?;
        let blocked = self.wait_end(4)?;
        self.expect(&blocked, "blocked", 4, "step 8 (prompt 3)")?;
        if blocked["kind"] != "permission" {
            return Err(format!(
                "step 8: blocked with kind {}, not permission",
                blocked["kind"]
            ));
        }
        self.print_blocked_report("step 8");
        let parsed = self.parse_dialog("step 8")?;
        println!(
            "step 8: dialog {:?} / {:?} cursor {}",
            parsed.title, parsed.command, parsed.cursor
        );
        let answer = self.call(
            "answer_dialog",
            json!({"terminal": PANE, "choice": "allow_once"}),
        )?;
        println!("step 8: answer_dialog allow_once: {answer}");
        let after_allow = self.wait_end(5)?;
        self.expect(&after_allow, "done", 5, "step 8 (allow_once)")?;
        if !self.marker().exists() {
            return Err("step 8: the marker is missing after allow_once".to_string());
        }
        println!("step 8: marker present after allow_once");
        let status = self.call("get_status", json!({"terminal": PANE}))?;
        println!("final get_status: {status}");
        Ok(())
    }

    fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        self.call_raw(tool, args)
            .map_err(|err| format!("{tool}: {}", err.data))
    }

    fn call_raw(&self, tool: &str, args: Value) -> Result<Value, ToolError> {
        let Value::Object(map) = args else {
            panic!("{tool}: arguments are not an object");
        };
        let args: Args = map;
        tools::call(&self.sh, tool, &args)
    }

    fn prompt(&mut self, text: &str) -> Result<(), String> {
        if self.prompts >= MAX_PROMPTS {
            return Err(format!("the prompt budget of {MAX_PROMPTS} is spent"));
        }
        self.prompts += 1;
        let sent = self.call("send_input", json!({"terminal": PANE, "text": text}))?;
        println!("prompt {}: {text:?} -> {sent}", self.prompts);
        Ok(())
    }

    /// Wait until the pane stops working in turn `min_turn` or later. A pane
    /// still blocked in an earlier turn (just answered) is polled.
    fn wait_end(&mut self, min_turn: u64) -> Result<Value, String> {
        let deadline = Instant::now() + TURN_TIMEOUT;
        loop {
            // Short waits, so the 64-entry status log is snapshotted often.
            let out = self.call("wait_status", json!({"terminal": PANE, "timeout": 2}))?;
            self.snapshot_log();
            let turn = out["turn"].as_u64().unwrap_or(0);
            if out["outcome"] == "exited" {
                self.print_screen();
                return Err(format!("kiro-cli exited: {out}"));
            }
            if out["outcome"] != "timeout" && turn >= min_turn {
                println!("wait_status: {out}");
                return Ok(out);
            }
            if Instant::now() >= deadline {
                self.print_screen();
                return Err(format!(
                    "no end of turn {min_turn} within {} s: {out}",
                    TURN_TIMEOUT.as_secs()
                ));
            }
            if out["outcome"] != "timeout" {
                thread::sleep(POLL);
            }
        }
    }

    fn expect(&self, out: &Value, outcome: &str, turn: u64, step: &str) -> Result<(), String> {
        if out["outcome"] == outcome && out["turn"] == turn {
            println!("{step}: {outcome} in turn {turn}");
            return Ok(());
        }
        self.print_screen();
        Err(format!(
            "{step}: expected {outcome} in turn {turn}, got {out}"
        ))
    }

    fn parse_dialog(&self, step: &str) -> Result<dialog::Dialog, String> {
        let deadline = Instant::now() + DIALOG_TIMEOUT;
        loop {
            let read = self.call("read_output", json!({"terminal": PANE, "lines": 0}))?;
            let text = read["text"].as_str().unwrap_or_default().to_string();
            match dialog::parse(&text) {
                Ok(parsed) => return Ok(parsed),
                Err(err) if Instant::now() >= deadline => {
                    println!("{step}: screen:\n{text}");
                    return Err(format!("{step}: dialog::parse failed: {err}"));
                }
                Err(_) => thread::sleep(POLL),
            }
        }
    }

    fn marker(&self) -> PathBuf {
        self.dir.join(MARKER)
    }

    fn print_screen(&self) {
        match self.call("read_output", json!({"terminal": PANE, "lines": 0})) {
            Ok(read) => println!("screen:\n{}", read["text"].as_str().unwrap_or_default()),
            Err(err) => println!("screen: read failed: {err}"),
        }
    }

    /// The last blocked report from the status log, the whole OSC from the raw
    /// output ring, and the keyboard mode.
    fn print_blocked_report(&mut self, step: &str) {
        self.snapshot_log();
        let pane = TerminalRef::new(PANE);
        match self
            .log
            .iter()
            .rev()
            .find(|e| e.outcome.starts_with("blocked"))
        {
            Some(entry) => {
                println!("{step}: blocked body hex: {}", hex(&entry.body));
                println!("{step}: blocked body text: {}", escaped(&entry.body));
            }
            None => println!("{step}: no blocked entry in the status log"),
        }
        match self.native.raw_output(&pane) {
            Ok(raw) => match last_blocked_osc(&raw) {
                Some(osc) => {
                    println!("{step}: raw blocked OSC hex: {}", hex(osc));
                    println!("{step}: raw blocked OSC text: {}", escaped(osc));
                }
                None => println!("{step}: no blocked OSC in the raw output ring"),
            },
            Err(err) => println!("{step}: raw_output failed: {err:?}"),
        }
        match self.native.keyboard_mode(&pane) {
            Ok(mode) => println!(
                "{step}: keyboard mode: kitty_flags {} (0b{:05b}), DECCKM {}",
                mode.kitty_flags, mode.kitty_flags, mode.app_cursor
            ),
            Err(err) => println!("{step}: keyboard_mode failed: {err:?}"),
        }
    }

    /// Merge the pane's status log into `self.log`.
    fn snapshot_log(&mut self) {
        let Ok(snapshot) = self.native.status_log(&TerminalRef::new(PANE)) else {
            return;
        };
        let max = snapshot.len().min(self.log.len());
        let overlap = (0..=max)
            .rev()
            .find(|&k| self.log.ends_with(&snapshot[..k]))
            .unwrap_or(0);
        if overlap == 0 && !self.log.is_empty() && !snapshot.is_empty() {
            self.log_gaps += 1;
        }
        self.log.extend_from_slice(&snapshot[overlap..]);
    }

    fn print_transitions(&self) {
        println!(
            "status log: {} entries ({} gaps); at_ms is relative to the probe start",
            self.log.len(),
            self.log_gaps
        );
        for entry in &self.log {
            println!(
                "  at_ms {:>7}  turn {}  outcome {:<32}  body {}",
                entry.at_ms.saturating_sub(self.t0_ms),
                entry.turn_after,
                entry.outcome,
                escaped(&entry.body)
            );
        }
        let transitions: Vec<String> = self
            .log
            .iter()
            .filter(|e| is_transition(e))
            .map(|e| format!("{}@{}", e.outcome, e.turn_after))
            .collect();
        println!("transitions: {}", transitions.join(" -> "));
    }

    /// The turns the status log went through must be exactly 0..=5.
    fn check_turns(&self) -> Result<(), String> {
        let mut turns: Vec<u64> = Vec::new();
        for entry in self.log.iter().filter(|e| is_transition(e)) {
            if turns.last() != Some(&entry.turn_after) {
                turns.push(entry.turn_after);
            }
        }
        let text: Vec<String> = turns.iter().map(u64::to_string).collect();
        println!("turns: {}", text.join(" -> "));
        if self.log_gaps > 0 {
            // Entries were lost between snapshots; the per-step turn checks in
            // `run` still hold, so only require increasing turns ending at 5.
            println!(
                "warning: the status log lost entries ({} gaps)",
                self.log_gaps
            );
            if turns.windows(2).all(|w| w[0] < w[1]) && turns.last() == Some(&5) {
                return Ok(());
            }
        }
        if turns != [0, 1, 2, 3, 4, 5] {
            return Err(format!(
                "turns were {}, expected 0 -> 1 -> 2 -> 3 -> 4 -> 5",
                text.join(" -> ")
            ));
        }
        Ok(())
    }
}

/// A status change (not a repeat, a query or a discarded report).
fn is_transition(entry: &StatusLogEntry) -> bool {
    !entry.outcome.ends_with("(unchanged)")
        && entry.outcome != "query"
        && !entry.outcome.starts_with("discarded")
        && !entry.outcome.starts_with("ignored")
}

/// The last `ESC ] 7501 ; ... state=blocked ...` sequence with its terminator
/// (BEL or ESC \), as the program wrote it.
fn last_blocked_osc(raw: &[u8]) -> Option<&[u8]> {
    const START: &[u8] = b"\x1b]7501;";
    let mut found = None;
    let mut from = 0;
    while let Some(pos) = find(&raw[from..], START).map(|p| p + from) {
        let rest = &raw[pos + START.len()..];
        let end = rest.iter().enumerate().find_map(|(i, &b)| match b {
            0x07 => Some(i + 1),
            0x1b if rest.get(i + 1) == Some(&b'\\') => Some(i + 2),
            _ => None,
        });
        let Some(end) = end else { break };
        let osc = &raw[pos..pos + START.len() + end];
        if find(osc, b"state=blocked").is_some() {
            found = Some(osc);
        }
        from = pos + START.len() + end;
    }
    found
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn escaped(bytes: &[u8]) -> String {
    bytes.escape_ascii().to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
