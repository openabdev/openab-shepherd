//! Real-Kiro leak check for the native backend (issue #10, ADR §11).
//!
//! Starts the launch profile `kiro-leak` (`kiro-cli chat --trust-workspace`,
//! `KIRO_API_KEY` from the keychain item `openab-shepherd-kiro-api-key`, into
//! that child only) in a throwaway `/tmp/shepherd-leak-check-<pid>/`, waits
//! for idle and for kiro-cli's `acp-server.js`, removes the pane with
//! `remove_pane`, and then checks the process table: no process of the pane
//! survives, no kiro-cli or acp-server.js has its cwd in the throwaway
//! directory, and this process has no zombie child. It sends no prompt and
//! opens no socket. Usage: `native_leak_check [--grace <seconds>]`, the
//! `remove_pane` timeout (default: the tool's, 2 s). Exit 0 only when all
//! holds.

use openab_shepherd::native::{NativeBackend, NativeConfig};
use openab_shepherd::profiles::LaunchPolicy;
use openab_shepherd::tools::{self, Args, Shepherd};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

const PANE: &str = "leak";
const PROFILE: &str = "kiro-leak";
const START_TIMEOUT: Duration = Duration::from_secs(60);
const ACP_TIMEOUT: Duration = Duration::from_secs(30);
const SETTLE: Duration = Duration::from_secs(3);

struct Proc {
    pid: i32,
    ppid: i32,
    pgid: i32,
    stat: String,
    command: String,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let grace = match args.as_slice() {
        [] => None,
        [flag, secs] if flag == "--grace" => match secs.parse::<f64>() {
            Ok(secs) if (0.0..=60.0).contains(&secs) => Some(secs),
            _ => None,
        },
        _ => None,
    };
    if !args.is_empty() && grace.is_none() {
        eprintln!("usage: native_leak_check [--grace <seconds, 0 to 60>]");
        return ExitCode::from(2);
    }
    let dir = PathBuf::from(format!("/tmp/shepherd-leak-check-{}", std::process::id()));
    let result = run(&dir, grace);
    if let Err(err) = std::fs::remove_dir_all(&dir) {
        println!("leak: cannot delete {}: {err}", dir.display());
    }
    match result {
        Ok(()) => {
            println!("PASS");
            ExitCode::SUCCESS
        }
        Err(err) => {
            println!("FAIL: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(dir: &Path, grace: Option<f64>) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", dir.display()))?;
    let profiles = json!({"profiles": [{
        "name": PROFILE,
        "argv": ["kiro-cli", "chat", "--trust-workspace"],
        "cwds": [dir],
        "env": {"KIRO_API_KEY": "keychain:openab-shepherd-kiro-api-key"},
    }]});
    let policy = LaunchPolicy::from_json(&profiles.to_string())
        .map_err(|e| format!("launch profile: {e:?}"))?;
    let native = NativeBackend::new(NativeConfig::default());
    let sh = Shepherd::with_policy(native.clone(), policy);
    let me = std::process::id() as i32;
    println!("leak: pid {me}, dir {}", dir.display());

    let started = call(
        &sh,
        "start_pane",
        json!({"profile": PROFILE, "cwd": dir.to_str(), "name": PANE}),
    )?;
    println!("start_pane: {started}");
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let status = call(&sh, "get_status", json!({"terminal": PANE}))?;
        if status["status"]["state"] == "idle" {
            println!("idle: {status}");
            break;
        }
        if status["live"] == false || Instant::now() >= deadline {
            let screen = call(&sh, "read_output", json!({"terminal": PANE}))?;
            println!("screen:\n{}", screen["text"].as_str().unwrap_or_default());
            return Err(format!("kiro-cli did not reach idle: {status}"));
        }
        thread::sleep(Duration::from_millis(200));
    }

    // kiro-cli's bun starts its acp-server in a new session (own group, no
    // terminal); wait for it.
    let deadline = Instant::now() + ACP_TIMEOUT;
    let acp = loop {
        let all = processes()?;
        let found: Vec<(i32, i32)> = in_dir(&all, &dir, "acp-server.js")
            .iter()
            .map(|p| (p.pid, p.pgid))
            .collect();
        if !found.is_empty() {
            break found;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "no acp-server.js with cwd in the dir within {} s; the check would prove nothing",
                ACP_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(250));
    };
    thread::sleep(SETTLE);
    let before = processes()?;
    let leader = before
        .iter()
        .find(|p| p.ppid == me && p.command.contains("kiro-cli"))
        .map(|p| p.pid)
        .ok_or("no kiro-cli child")?;
    let mut tree = descendants(&before, leader);
    tree.push(leader);
    println!("before remove: the pane's {} processes:", tree.len());
    for p in before.iter().filter(|p| tree.contains(&p.pid)) {
        print_proc(p);
    }
    for &(pid, pgid) in &acp {
        if !tree.contains(&pid) {
            return Err(format!("acp-server {pid} is not a descendant"));
        }
        println!(
            "acp-server {pid}: pgid {pgid} (pane group {leader}): {}",
            if pgid == leader {
                "same group"
            } else {
                "own group"
            }
        );
    }

    let started_rm = Instant::now();
    let mut remove_args = json!({"terminal": PANE});
    if let Some(secs) = grace {
        remove_args["timeout"] = json!(secs);
    }
    let removed = call(&sh, "remove_pane", remove_args)?;
    println!(
        "remove_pane ({} ms): {removed}",
        started_rm.elapsed().as_millis()
    );
    let code = removed["exit_code"].as_i64();
    match removed["stopped"].as_str() {
        Some("killed") if code != Some(128 + i64::from(libc::SIGKILL)) => {
            return Err(format!("killed but exit_code {code:?}, not 137"))
        }
        Some("killed") | Some("exited") if code.is_some() => {}
        _ => return Err(format!("unexpected remove_pane result {removed}")),
    }

    // Removal returns once the leader is reaped; give orphans a moment to
    // be reaped by launchd before looking.
    thread::sleep(Duration::from_millis(500));
    let after = processes()?;
    let mut failures = Vec::new();
    for p in after.iter().filter(|p| tree.contains(&p.pid)) {
        // A pid can be reused, but not within this second on a quiet box;
        // report it anyway.
        failures.push(format!("pane process survived: {} {}", p.pid, p.command));
    }
    for name in ["kiro-cli", "acp-server.js"] {
        for p in in_dir(&after, &dir, name) {
            failures.push(format!(
                "{name} with cwd in the dir: {} {}",
                p.pid, p.command
            ));
        }
    }
    // This check's own ps is a child of this process.
    for p in after
        .iter()
        .filter(|p| p.ppid == me && !p.command.starts_with("/bin/ps "))
    {
        let what = if p.stat.contains('Z') {
            "zombie child"
        } else {
            "child"
        };
        failures.push(format!("{what} left: {} {} {}", p.pid, p.stat, p.command));
    }
    println!(
        "after remove: {} of {} pane processes left, {} children of this process",
        after.iter().filter(|p| tree.contains(&p.pid)).count(),
        tree.len(),
        after
            .iter()
            .filter(|p| p.ppid == me && !p.command.starts_with("/bin/ps "))
            .count()
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn call(sh: &Shepherd, tool: &str, args: Value) -> Result<Value, String> {
    let args: Args = args.as_object().cloned().unwrap_or_default();
    tools::call(sh, tool, &args).map_err(|err| format!("{tool}: {}", err.data))
}

/// Every process, from `ps`.
fn processes() -> Result<Vec<Proc>, String> {
    let out = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,pgid=,stat=,command="])
        .output()
        .map_err(|e| format!("ps: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let pgid = it.next()?.parse().ok()?;
            let stat = it.next()?.to_string();
            let command = it.collect::<Vec<_>>().join(" ");
            Some(Proc {
                pid,
                ppid,
                pgid,
                stat,
                command,
            })
        })
        .collect())
}

/// The pids below `root` by parent pid.
fn descendants(procs: &[Proc], root: i32) -> Vec<i32> {
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for p in procs.iter().filter(|p| p.ppid == parent) {
            out.push(p.pid);
            frontier.push(p.pid);
        }
    }
    out
}

/// Processes whose command contains `needle` and whose cwd is in `dir`.
fn in_dir<'a>(procs: &'a [Proc], dir: &Path, needle: &str) -> Vec<&'a Proc> {
    procs
        .iter()
        .filter(|p| p.command.contains(needle))
        .filter(|p| cwd_of(p.pid).is_some_and(|cwd| Path::new(&cwd).starts_with(dir)))
        .collect()
}

fn cwd_of(pid: i32) -> Option<String> {
    let out = Command::new("/usr/sbin/lsof")
        .args(["-a", "-d", "cwd", "-Fn", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n').map(str::to_string))
}

fn print_proc(p: &Proc) {
    let command: String = p.command.chars().take(160).collect();
    println!(
        "  pid {} ppid {} pgid {} {} {command}",
        p.pid, p.ppid, p.pgid, p.stat
    );
}
