//! The structural check of `docs/operator-e2e/dialog-canary.sh` (issue #65), run on the
//! committed fixtures. The script's check (`dialog-canary-check.py`) must accept the recorded
//! dialog of each agent and reject a deliberately broken copy, and on every screen it must
//! reach the same verdict as the real parser the server ships: if the two could disagree, the
//! canary would prove nothing. No server starts and no agent runs.

use openab_shepherd::{devin_dialog, dialog, opencode_dialog};
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn checker() -> PathBuf {
    root().join("docs/operator-e2e/dialog-canary-check.py")
}

fn fixture(agent: &str) -> PathBuf {
    root().join(format!(
        "tests/fixtures/replay/{agent}/03-permission-dialog.screen.txt"
    ))
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A Devin 3000.12.57 shell dialog (the layout src/devin_dialog.rs measured; no file fixture
/// exists), with the highlight on `Allow`.
const DEVIN_SCREEN: &str = "\n\u{276d} Use the shell tool to run exactly: touch /tmp/x/canary_marker.txt . Do nothing else.\n\n \u{23fa} Running command\n \u{2514} $ touch /tmp/x/canary_marker.txt\n\n\u{276d} Allow       \u{2190} \u{25fc}\u{25fc}\u{25fc}\u{25fc} \u{2192} Once     Exec(touch)\n  Bypass mode\n  Deny\n\n\u{2191}\u{2193} select \u{b7} \u{2190}\u{2192} 1-4 scope \u{b7} e edit \u{b7} \u{21b5} confirm \u{b7} esc cancel\n";

/// The Devin dialog without the user's own echoed prompt line, which also starts with the
/// cursor glyph on a real screen; the parser reads only the options.
fn devin_screen() -> String {
    DEVIN_SCREEN
        .lines()
        .filter(|l| !l.contains("Use the shell tool"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// What the real parser says about a screen: `true` when it accepts it.
fn parser_accepts(agent: &str, screen: &str) -> bool {
    match agent {
        "kiro" => dialog::parse(screen).is_ok(),
        "devin" => devin_dialog::parse(screen).is_ok(),
        "opencode" => opencode_dialog::parse(screen).is_ok(),
        other => panic!("unknown agent {other}"),
    }
}

/// Run the structural check; `(accepted, output)`.
fn check(agent: &str, screen: &str, marker: Option<&str>, tag: &str) -> (bool, String) {
    let dir = std::env::temp_dir().join(format!(
        "shepherd-dialog-canary-{}-{agent}-{tag}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("screen.txt");
    std::fs::write(&file, screen).unwrap();
    let mut cmd = Command::new("python3");
    cmd.arg(checker()).arg("structure").arg(agent).arg(&file);
    if let Some(marker) = marker {
        cmd.arg("--marker").arg(marker);
    }
    if agent != "devin" {
        cmd.arg("--fixture").arg(fixture(agent));
    }
    let out = cmd.output().expect("run python3");
    std::fs::remove_dir_all(&dir).ok();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    match out.status.code() {
        Some(0) => (true, text),
        Some(1) => (false, text),
        other => panic!("{agent}/{tag}: the checker failed to run ({other:?}): {text}"),
    }
}

/// The recorded screen of each agent, with the marker the fixtures use.
fn recorded() -> Vec<(&'static str, String, &'static str)> {
    vec![
        ("kiro", read(&fixture("kiro")), "replay_marker.txt"),
        ("opencode", read(&fixture("opencode")), "replay_marker.txt"),
        ("devin", devin_screen(), "canary_marker.txt"),
    ]
}

#[test]
fn the_check_accepts_the_recorded_dialog_of_each_agent_and_so_does_the_parser() {
    for (agent, screen, marker) in recorded() {
        assert!(
            parser_accepts(agent, &screen),
            "{agent}: the real parser refuses the recorded screen"
        );
        let (ok, text) = check(agent, &screen, Some(marker), "good");
        assert!(ok, "{agent}: the check refuses the recorded screen: {text}");
        assert!(text.starts_with("OK "), "{agent}: {text}");
    }
}

/// Each agent's recorded screen with one deliberate change.
fn broken() -> Vec<(&'static str, &'static str, String)> {
    let kiro = read(&fixture("kiro"));
    let opencode = read(&fixture("opencode"));
    let devin = devin_screen();
    let without = |text: &str, needle: &str| {
        text.lines()
            .filter(|l| !l.contains(needle))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    };
    vec![
        (
            "kiro",
            "title suffix changed",
            kiro.replace("requires approval", "requires permission"),
        ),
        ("kiro", "an option removed", without(&kiro, "Deny once")),
        (
            "kiro",
            "the tool arrow changed",
            kiro.replace(" shell \u{2192} ", " shell - "),
        ),
        (
            "kiro",
            "the options swapped",
            kiro.replace("Allow once", "TMP")
                .replace("Deny once", "Allow once")
                .replace("TMP", "Deny once"),
        ),
        (
            "opencode",
            "heading changed",
            opencode.replace("\u{25b3} Permission required", "\u{25b3} Permission needed"),
        ),
        (
            "opencode",
            "an option removed",
            opencode.replace("Always allow   ", ""),
        ),
        (
            "opencode",
            "the command marker changed",
            opencode.replace("\u{2503}  $ touch", "\u{2503}  > touch"),
        ),
        (
            "devin",
            "heading changed",
            devin.replace("Running command", "Running tool"),
        ),
        ("devin", "an option removed", without(&devin, "Bypass mode")),
        (
            "devin",
            "the command marker changed",
            devin.replace("$ touch", "> touch"),
        ),
    ]
}

#[test]
fn the_check_rejects_a_broken_copy_and_the_parser_agrees() {
    for (index, (agent, what, screen)) in broken().into_iter().enumerate() {
        assert!(
            !parser_accepts(agent, &screen),
            "{agent} ({what}): the real parser still accepts the broken screen; fix the test"
        );
        let (ok, text) = check(agent, &screen, None, &format!("broken{index}"));
        assert!(!ok, "{agent} ({what}): the check accepts it: {text}");
        assert!(text.starts_with("FAIL "), "{agent} ({what}): {text}");
    }
}

#[test]
fn the_check_requires_the_marker_in_the_command() {
    for (agent, screen, _) in recorded() {
        let (ok, text) = check(agent, &screen, Some("another_marker.txt"), "marker");
        assert!(
            !ok,
            "{agent}: accepted a command without the marker: {text}"
        );
        assert!(text.contains("marker"), "{agent}: {text}");
    }
}

#[test]
fn the_check_agrees_with_the_parser_on_screens_without_a_dialog() {
    let startup = [
        (
            "kiro",
            read(&root().join("tests/fixtures/replay/kiro/01-startup.screen.txt")),
        ),
        (
            "opencode",
            read(&root().join("tests/fixtures/replay/opencode/01-startup.screen.txt")),
        ),
        ("devin", String::from("Welcome to Devin\n\n> \n")),
    ];
    for (agent, screen) in startup {
        assert!(!parser_accepts(agent, &screen), "{agent}");
        let (ok, text) = check(agent, &screen, None, "startup");
        assert!(
            !ok,
            "{agent}: the check accepts a screen with no dialog: {text}"
        );
    }
}

#[test]
fn the_login_check_flags_a_login_prompt_and_no_recorded_screen() {
    let run = |text: &str, tag: &str| {
        let file = std::env::temp_dir().join(format!(
            "shepherd-dialog-canary-login-{}-{tag}.txt",
            std::process::id()
        ));
        std::fs::write(&file, text).unwrap();
        let out = Command::new("python3")
            .arg(checker())
            .arg("login")
            .arg(&file)
            .output()
            .expect("run python3");
        std::fs::remove_file(&file).ok();
        out.status.code()
    };
    for (i, prompt) in [
        "You are not logged in\n",
        "Open this URL to log in: https://example.com/device\n",
        "Please log in with your account\n",
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(run(prompt, &format!("p{i}")), Some(0), "{prompt}");
    }
    // Every recorded screen of every agent is a screen with no login prompt.
    let mut screens = vec![
        read(&fixture("kiro")),
        read(&fixture("opencode")),
        devin_screen(),
    ];
    for agent in ["kiro", "opencode"] {
        for name in ["01-startup", "02-turn"] {
            screens.push(read(
                &root().join(format!("tests/fixtures/replay/{agent}/{name}.screen.txt")),
            ));
        }
    }
    for entry in std::fs::read_dir(root().join("tests/fixtures/kiro-3.3.0")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|x| x == "txt") {
            screens.push(read(&path));
        }
    }
    for (i, screen) in screens.iter().enumerate() {
        assert_eq!(run(screen, &format!("s{i}")), Some(1), "screen {i}");
    }
}
