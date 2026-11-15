//! Checks on the docs and the operator scripts of issue #76 (PR8) that need no server:
//! the scripts parse, the runbook names every script, the doc links resolve, the old
//! "registrations live only in memory" text is gone, and the profiles that
//! `restart-lib.sh` writes are accepted by the real profile loader with a dialog the real
//! parser reads. Nothing here starts the Shepherd server.

use openab_shepherd::dialog;
use openab_shepherd::profiles::LaunchPolicy;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn e2e_dir() -> PathBuf {
    root().join("docs/operator-e2e")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn scripts() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(e2e_dir())
        .expect("read docs/operator-e2e")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "sh"))
        .collect();
    found.sort();
    found
}

#[test]
fn every_operator_script_parses() {
    let found = scripts();
    assert!(found.len() >= 10, "found only {found:?}");
    for script in found {
        let out = Command::new("bash")
            .arg("-n")
            .arg(&script)
            .output()
            .expect("run bash -n");
        assert!(
            out.status.success(),
            "bash -n {}: {}",
            script.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn the_runbook_names_every_restart_script_and_each_name_exists() {
    let runbook = read(&e2e_dir().join("restart-survival.md"));
    let expected = [
        "holder-start.sh",
        "front-kill.sh",
        "front-kill-mid-dialog.sh",
        "wrapped-tail.sh",
        "sigusr2.sh",
        "service-restart-macos.sh",
        "service-restart-linux.sh",
        "version-mismatch.sh",
        "routine-after-restart.sh",
        "zombie-under-tini.sh",
        "record-replay-fixture.sh",
        "restart-cleanup.sh",
        "restart-lib.sh",
    ];
    for name in expected {
        assert!(runbook.contains(name), "restart-survival.md omits {name}");
        assert!(e2e_dir().join(name).is_file(), "{name} does not exist");
    }
    // The other way: a script added later must be described.
    let older = [
        "setup.sh",
        "start-server.sh",
        "ready.sh",
        "mcp.sh",
        "cleanup.sh",
    ];
    for script in scripts() {
        let name = script.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            expected.contains(&name.as_str()) || older.contains(&name.as_str()),
            "{name} is not described in restart-survival.md or the older runbook"
        );
    }
}

#[test]
fn the_scenario_scripts_clean_up_after_themselves() {
    for name in [
        "front-kill.sh",
        "front-kill-mid-dialog.sh",
        "wrapped-tail.sh",
        "sigusr2.sh",
        "version-mismatch.sh",
        "routine-after-restart.sh",
    ] {
        let text = read(&e2e_dir().join(name));
        assert!(text.contains("r_begin"), "{name} does not use r_begin");
        assert!(text.contains("r_finish"), "{name} does not report a result");
    }
    let lib = read(&e2e_dir().join("restart-lib.sh"));
    assert!(lib.contains("trap r_cleanup EXIT"));
    assert!(lib.contains("r_stop_holder"));
}

#[test]
fn the_docs_no_longer_say_registrations_live_only_in_memory() {
    for file in ["docs/operations.md", "docs/routine.md", "README.md"] {
        let text = read(&root().join(file)).replace('\n', " ");
        assert!(
            !text.contains("Registrations live only in memory")
                && !text.contains("Registrations live in memory"),
            "{file} still says registrations live in memory"
        );
    }
    let ops = read(&root().join("docs/operations.md"));
    assert!(ops.contains("## Restart survival"));
    assert!(ops.contains("`pane_replayed`"));
    assert!(ops.contains("HOLDER_VERSION"));
}

/// The anchor GitHub makes for a heading.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

fn anchors(markdown: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut found = Vec::new();
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence && line.starts_with('#') {
            found.push(slug(line.trim_start_matches('#')));
        }
    }
    found
}

/// The `(target)` of every inline markdown link in `text`.
fn link_targets(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        let after = &rest[at + 2..];
        if let Some(end) = after.find(')') {
            found.push(after[..end].to_string());
        }
        rest = after;
    }
    found
}

#[test]
fn the_relative_links_in_the_docs_resolve() {
    let mut files = vec![
        root().join("README.md"),
        root().join("docs/operations.md"),
        root().join("docs/routine.md"),
        root().join("docs/terminal-clients.md"),
    ];
    for dir in ["docs/adr", "docs/operator-e2e"] {
        for entry in std::fs::read_dir(root().join(dir)).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|x| x == "md") {
                files.push(path);
            }
        }
    }
    let mut broken = Vec::new();
    for file in &files {
        let text = read(file);
        for target in link_targets(&text) {
            if target.contains("://") || target.starts_with("mailto:") || target.contains(' ') {
                continue;
            }
            let (path_part, anchor) = match target.split_once('#') {
                Some((p, a)) => (p, Some(a)),
                None => (target.as_str(), None),
            };
            let resolved = if path_part.is_empty() {
                file.clone()
            } else {
                file.parent().unwrap().join(path_part)
            };
            if !resolved.exists() {
                broken.push(format!("{}: {target} (no such file)", file.display()));
                continue;
            }
            if let Some(anchor) = anchor {
                if resolved.extension().is_some_and(|x| x == "md")
                    && !anchors(&read(&resolved)).iter().any(|a| a == anchor)
                {
                    broken.push(format!("{}: {target} (no such heading)", file.display()));
                }
            }
        }
    }
    assert!(broken.is_empty(), "broken links:\n{}", broken.join("\n"));
}

#[test]
fn the_profiles_the_scripts_write_load_and_the_dialog_parses() {
    let base = std::env::temp_dir().join(format!(
        "shepherd-e2e-restart-doctest-{}",
        std::process::id()
    ));
    let (home, work) = (base.join("home"), base.join("work"));
    std::fs::create_dir_all(&base).unwrap();
    let out = Command::new("bash")
        .arg("-c")
        .arg("source \"$LIB\" && r_setup")
        .env("LIB", e2e_dir().join("restart-lib.sh"))
        .env("R_HOME", &home)
        .env("R_WORK", &work)
        .env("SHEPHERD_BIN", "/bin/sh")
        .output()
        .expect("run r_setup");
    assert!(
        out.status.success(),
        "r_setup failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let policy = LaunchPolicy::from_json(&read(&home.join("profiles.json")))
        .expect("the profiles file is accepted by the loader");
    for name in [
        "turns",
        "dialog-live",
        "dialog-wrapped",
        "wrapped-idle",
        "wrapped-ticker",
        "routine-fresh",
        "routine-giveup",
    ] {
        let profile = policy
            .profile(name)
            .unwrap_or_else(|| panic!("no profile {name}"));
        assert!(
            profile.cwds.iter().all(|d| d.is_dir()),
            "{name}: a cwd is missing"
        );
    }
    assert!(policy.profile("routine-fresh").unwrap().routine.is_some());
    assert!(policy.profile("routine-giveup").unwrap().routine.is_some());
    assert!(policy.profile("turns").unwrap().routine.is_none());
    assert!(policy.has_routine());

    let text = read(&work.join("dialog-live/dialog.txt"));
    let parsed = dialog::parse(&text).expect("the dialog text parses");
    assert_eq!(parsed.options[0], "Allow once");
    assert_eq!(parsed.cursor, 0);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home.join("token")), 0o600);
        assert_eq!(mode(&home.join("profiles.json")), 0o600);
    }
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn the_recorder_and_the_fixture_wrapper_agree_on_their_paths() {
    let wrapper = read(&e2e_dir().join("record-replay-fixture.sh"));
    assert!(wrapper.contains("target/release/examples/record_tail"));
    assert!(wrapper.contains("tests/fixtures/replay/"));
    assert!(root().join("examples/record_tail.rs").is_file());
}
