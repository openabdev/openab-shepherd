//! Records a program's raw PTY output and its final screen, for the replay fixtures of
//! restart survival (issue #76, `tests/fixtures/replay/<agent>/`). Run by hand, never by
//! CI: it needs a logged-in agent CLI and a person to read the result before it is
//! committed.
//!
//! Usage:
//!
//! ```text
//! record_tail --out DIR --name NAME [--cwd DIR] [--quiet-ms 4000] [--max-secs 300]
//!             [--send TEXT | --key KEY]... -- COMMAND [ARG]...
//! ```
//!
//! It starts COMMAND on a native pane (200 columns by 120 rows, the same environment as
//! a Shepherd pane: only `HOME`, `PATH`, `USER`, `LANG` and `TERM`), waits until the
//! program has been quiet for `--quiet-ms`, then performs the steps in order, each
//! followed by another quiet wait: `--send TEXT` pastes TEXT and presses Enter,
//! `--key KEY` presses `enter`, `esc`, `up`, `down` or `ctrl-c`. When the steps are done
//! and the program is quiet (or has exited, or `--max-secs` has passed) it writes
//! `DIR/NAME.raw` (the bytes the program wrote, in order), `DIR/NAME.screen.txt` (the
//! front emulator's screen) and `DIR/NAME.json` (what was observed: status, turn, sizes,
//! gaps), and ends the pane.
//!
//! The ring a pane exposes holds 256 KiB, so the recorder polls it every 20 ms and
//! stitches the snapshots by their overlap. If output outran the poll, the stitch finds
//! no overlap, the recorder counts a gap and says so; record again with fewer or slower
//! steps. Replay itself needs the last 1 MiB only, and a test truncates a longer
//! recording to that.
//!
//! Before writing anything it replaces the home directory with `/home/user` and refuses
//! to write when the bytes look like a credential (a `Bearer` header, an `sk-` or `ghp_`
//! key, or `token=` followed by a value). Read the files before you commit them: the
//! screen of an agent can show other things you may not want in a repository.
//!
//! Exit status: 0 written, 1 failed, 2 usage, 3 refused because of a possible secret.

use openab_shepherd::backend::{InputGuard, Key, RunSpec, TerminalBackend, TerminalRef};
use openab_shepherd::native::{NativeBackend, NativeConfig};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(20);
const PANE: &str = "record";
/// An overlap shorter than this between two snapshots is not trusted as one.
const MIN_OVERLAP: usize = 64;

enum Step {
    Send(String),
    Key(Key),
}

struct Options {
    out: PathBuf,
    name: String,
    cwd: PathBuf,
    quiet: Duration,
    max: Duration,
    steps: Vec<Step>,
    command: Vec<String>,
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: record_tail --out DIR --name NAME [--cwd DIR] [--quiet-ms N] [--max-secs N] \
         [--send TEXT | --key enter|esc|up|down|ctrl-c]... -- COMMAND [ARG]..."
    );
    ExitCode::from(2)
}

fn parse(args: &[String]) -> Option<Options> {
    let mut out = None;
    let mut name = None;
    let mut cwd = std::env::current_dir().ok()?;
    let mut quiet = Duration::from_millis(4000);
    let mut max = Duration::from_secs(300);
    let mut steps = Vec::new();
    let mut iter = args.iter();
    let mut command = Vec::new();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--" => {
                command = iter.by_ref().cloned().collect();
                break;
            }
            "--out" => out = Some(PathBuf::from(iter.next()?)),
            "--name" => name = Some(iter.next()?.clone()),
            "--cwd" => cwd = PathBuf::from(iter.next()?),
            "--quiet-ms" => quiet = Duration::from_millis(iter.next()?.parse().ok()?),
            "--max-secs" => max = Duration::from_secs(iter.next()?.parse().ok()?),
            "--send" => steps.push(Step::Send(iter.next()?.clone())),
            "--key" => steps.push(Step::Key(match iter.next()?.as_str() {
                "enter" => Key::Enter,
                "esc" => Key::Esc,
                "up" => Key::Up,
                "down" => Key::Down,
                "ctrl-c" => Key::CtrlC,
                _ => return None,
            })),
            _ => return None,
        }
    }
    let name = name?;
    let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    if command.is_empty() || name.is_empty() || !name.chars().all(plain) {
        return None;
    }
    Some(Options {
        out: out?,
        name,
        cwd,
        quiet,
        max,
        steps,
        command,
    })
}

/// Joins successive snapshots of a sliding window into one stream.
#[derive(Default)]
struct Stitch {
    all: Vec<u8>,
    last: Vec<u8>,
    gaps: u32,
}

impl Stitch {
    /// Takes the newest snapshot; true when it brought new bytes.
    fn absorb(&mut self, snap: Vec<u8>) -> bool {
        if snap == self.last {
            return false;
        }
        let max = self.last.len().min(snap.len());
        let floor = MIN_OVERLAP.min(self.last.len());
        // The largest k such that the end of the old window is the start of the new.
        let overlap = (floor..=max)
            .rev()
            .find(|&k| self.last[self.last.len() - k..] == snap[..k]);
        match overlap {
            Some(k) => self.all.extend_from_slice(&snap[k..]),
            None => {
                self.gaps += 1;
                self.all.extend_from_slice(&snap);
            }
        }
        self.last = snap;
        true
    }
}

/// Whether `bytes` contain something that looks like a credential. The match is on the
/// shape (a prefix and a run of key characters), so words such as `task-list` pass.
fn looks_secret(bytes: &[u8]) -> Option<&'static str> {
    let key_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    let run_after =
        |at: usize, ok: &dyn Fn(u8) -> bool| bytes[at..].iter().take_while(|&&b| ok(b)).count();
    let find_all = |needle: &[u8]| {
        bytes
            .windows(needle.len())
            .enumerate()
            .filter(move |(_, w)| *w == needle)
            .map(|(i, _)| i)
            .collect::<Vec<_>>()
    };
    for at in find_all(b"Bearer ") {
        if run_after(at + 7, &|b| key_char(b) || b == b'.') >= 8 {
            return Some("a Bearer header");
        }
    }
    for at in find_all(b"sk-") {
        let word_start = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        if word_start && run_after(at + 3, &key_char) >= 20 {
            return Some("an sk- key");
        }
    }
    for at in find_all(b"ghp_") {
        if run_after(at + 4, &|b| b.is_ascii_alphanumeric()) >= 20 {
            return Some("a ghp_ token");
        }
    }
    for at in find_all(b"token=") {
        if run_after(at + 6, &|b| b.is_ascii_graphic()) >= 8 {
            return Some("token=<value>");
        }
    }
    None
}

/// Replaces every occurrence of `from` with `to`.
fn replace(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn run(opts: &Options) -> Result<ExitCode, String> {
    let native = NativeBackend::new(NativeConfig::default());
    let pane = TerminalRef::new(PANE);
    let spec = RunSpec {
        argv: Some(opts.command.clone()),
        cwd: Some(opts.cwd.to_string_lossy().into_owned()),
        name: Some(PANE.to_string()),
        ..RunSpec::default()
    };
    native
        .run(&spec)
        .map_err(|e| format!("cannot start: {}", e.detail))?;
    eprintln!(
        "record_tail: started {:?} in {}",
        opts.command[0],
        opts.cwd.display()
    );

    let started = Instant::now();
    let mut stitch = Stitch::default();
    let mut quiet_since = Instant::now();
    let mut next = 0;
    let guard = InputGuard::default();
    let outcome = loop {
        thread::sleep(POLL);
        if let Ok(snap) = native.raw_output(&pane) {
            if stitch.absorb(snap) {
                quiet_since = Instant::now();
            }
        }
        let live = native.get(&pane).map(|i| i.live).unwrap_or(false);
        if !live {
            break "the program exited";
        }
        if started.elapsed() >= opts.max {
            break "--max-secs reached";
        }
        if quiet_since.elapsed() < opts.quiet {
            continue;
        }
        match opts.steps.get(next) {
            None => break "quiet after the last step",
            Some(Step::Send(text)) => {
                eprintln!("record_tail: step {}: send {} bytes", next + 1, text.len());
                native
                    .send(&pane, text, &guard)
                    .map_err(|e| format!("send failed: {}", e.detail))?;
            }
            Some(Step::Key(key)) => {
                eprintln!("record_tail: step {}: key {}", next + 1, key.as_str());
                native
                    .keys(&pane, std::slice::from_ref(key), &guard)
                    .map_err(|e| format!("key failed: {}", e.detail))?;
            }
        }
        next += 1;
        quiet_since = Instant::now();
    };
    eprintln!("record_tail: stopping: {outcome}");

    let info = native.get(&pane).ok();
    let screen = native
        .read(&pane, 0)
        .map(|s| s.text)
        .map_err(|e| format!("cannot read the screen: {}", e.detail))?;
    let _ = native.remove(&pane, None);

    let home = std::env::var("HOME").unwrap_or_default();
    let scrub = |bytes: &[u8]| replace(bytes, home.as_bytes(), b"/home/user");
    let raw = scrub(&stitch.all);
    let screen = scrub(screen.as_bytes());
    for (what, bytes) in [("output", &raw), ("screen", &screen)] {
        if let Some(kind) = looks_secret(bytes) {
            eprintln!("record_tail: REFUSED: the {what} contains {kind}; nothing was written");
            return Ok(ExitCode::from(3));
        }
    }

    let summary = json!({
        "command": opts.command.first(),
        "steps": opts.steps.len(),
        "steps_done": next,
        "ended_by": outcome,
        "raw_bytes": raw.len(),
        "gaps": stitch.gaps,
        "status": info.as_ref().and_then(|i| i.status.as_ref()).map(|s| s.to_json()),
        "turn": info.as_ref().and_then(|i| i.turn),
        "live_at_end": info.as_ref().map(|i| i.live),
    });
    std::fs::create_dir_all(&opts.out)
        .map_err(|e| format!("cannot create the output directory: {e}"))?;
    let path = |ext: &str| opts.out.join(format!("{}.{ext}", opts.name));
    std::fs::write(path("raw"), &raw).map_err(|e| format!("cannot write the raw file: {e}"))?;
    std::fs::write(path("screen.txt"), &screen)
        .map_err(|e| format!("cannot write the screen file: {e}"))?;
    std::fs::write(path("json"), format!("{summary:#}\n"))
        .map_err(|e| format!("cannot write the summary: {e}"))?;
    println!(
        "record_tail: wrote {} ({} bytes, {} gaps), {} and {}",
        path("raw").display(),
        raw.len(),
        stitch.gaps,
        path("screen.txt").display(),
        path("json").display()
    );
    if stitch.gaps > 0 {
        eprintln!(
            "record_tail: WARNING: {} gaps; the recording is not contiguous",
            stitch.gaps
        );
    }
    eprintln!("record_tail: read the files before you commit them");
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(opts) = parse(&args) else {
        return usage();
    };
    match run(&opts) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("record_tail: FAIL: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stitched(parts: &[&[u8]]) -> Stitch {
        let mut s = Stitch::default();
        for part in parts {
            s.absorb(part.to_vec());
        }
        s
    }

    #[test]
    fn a_growing_window_is_appended() {
        let s = stitched(&[b"abc", b"abcdef", b"abcdefghi"]);
        assert_eq!(s.all, b"abcdefghi");
        assert_eq!(s.gaps, 0);
    }

    #[test]
    fn a_sliding_window_is_joined_by_its_overlap() {
        let old: Vec<u8> = (0..200u8).collect();
        let new: Vec<u8> = (50..250u8).collect();
        let s = stitched(&[&old, &new]);
        assert_eq!(s.all, (0..250u8).collect::<Vec<_>>());
        assert_eq!(s.gaps, 0);
    }

    #[test]
    fn no_overlap_is_counted_as_a_gap() {
        let old = vec![1u8; 200];
        let new = vec![2u8; 200];
        let s = stitched(&[&old, &new]);
        assert_eq!(s.gaps, 1);
        assert_eq!(s.all.len(), 400);
    }

    #[test]
    fn secrets_are_found_by_shape_and_plain_words_pass() {
        assert!(looks_secret(b"Authorization: Bearer abcdefghijkl").is_some());
        assert!(looks_secret(b"key sk-abcdefghijklmnopqrstuvwx").is_some());
        assert!(looks_secret(b"ghp_abcdefghijklmnopqrstuvwxyz").is_some());
        assert!(looks_secret(b"?token=abcdefghij").is_some());
        assert!(looks_secret(b"the task-list and a disk-usage line, token= x").is_none());
        assert!(looks_secret(b"Bearer: none").is_none());
    }

    #[test]
    fn the_home_directory_is_replaced() {
        assert_eq!(
            replace(b"cd /Users/a/x /Users/a", b"/Users/a", b"/home/user"),
            b"cd /home/user/x /home/user"
        );
        assert_eq!(replace(b"abc", b"", b"x"), b"abc");
    }

    #[test]
    fn the_command_line_needs_a_name_and_a_command() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(parse(&v(&["--out", "d", "--name", "n", "--", "sh"])).is_some());
        assert!(parse(&v(&["--out", "d", "--name", "n"])).is_none());
        assert!(parse(&v(&["--out", "d", "--name", "../n", "--", "sh"])).is_none());
        assert!(parse(&v(&[
            "--out", "d", "--name", "n", "--key", "f5", "--", "sh"
        ]))
        .is_none());
    }
}
