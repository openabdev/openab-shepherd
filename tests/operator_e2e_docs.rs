//! Checks on the docs and the operator scripts of issue #76 (PR8) that need no server:
//! the scripts parse, the runbook names every script, the doc links resolve, the old
//! "registrations live only in memory" text is gone, and the profiles that
//! `restart-lib.sh` writes are accepted by the real profile loader with a dialog the real
//! parser reads. Nothing here starts the Shepherd server.

use openab_shepherd::clients::{Clients, Scope};
use openab_shepherd::dialog;
use openab_shepherd::profiles::{DynamicRoutines, LaunchPolicy};
use openab_shepherd::routine_model::{
    CEILING_EXPIRES_MIN_SECS, EVERY_MAX_SECS, EVERY_MIN_SECS, EXPIRES_MAX_SECS, EXPIRES_MIN_SECS,
    MAX_RUNS_MAX, MAX_RUNS_MIN, PER_PANE_MAX, PER_PANE_MIN, TEXT_MAX_BYTES,
};
use openab_shepherd::tools::TOOL_NAMES;
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
    // The routine scenario for agent-created routines has its own runbook.
    let dynamic = ["routine-dynamic.sh"];
    for script in scripts() {
        let name = script.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            expected.contains(&name.as_str())
                || older.contains(&name.as_str())
                || dynamic.contains(&name.as_str()),
            "{name} is not described in restart-survival.md, routine-dynamic.md or the older runbook"
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
        root().join("docs/credentials.md"),
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

// ---------------------------------------------------------------------------
// Agent-created routines (issue #98): the docs and routine-dynamic.sh.
// ---------------------------------------------------------------------------

const ROUTINE_TOOLS: [&str; 4] = [
    "routine_create",
    "routine_list",
    "routine_update",
    "routine_delete",
];

/// Every error code the routine tools return, as the handlers spell them.
const ROUTINE_ERRORS: [&str; 7] = [
    "routines_not_allowed",
    "routine_invalid",
    "routine_limit",
    "routine_not_found",
    "routine_read_only",
    "routine_not_saved",
    "pane_not_live",
];

#[test]
fn the_dynamic_routine_runbook_names_the_script_and_the_script_exists() {
    let runbook = read(&e2e_dir().join("routine-dynamic.md"));
    assert!(runbook.contains("routine-dynamic.sh"));
    assert!(e2e_dir().join("routine-dynamic.sh").is_file());
    // The ten steps of issue #98, and the facts the script checks.
    for needle in [
        "SHEPHERD_PANE",
        "routine_create",
        "skipped:working",
        "SIGUSR2",
        "DOWN_SECS",
        "audit_unavailable",
        "profile_changed",
        "expired",
        "RESULT: PASS",
        "#98 closes",
    ] {
        assert!(
            runbook.contains(needle),
            "routine-dynamic.md omits {needle}"
        );
    }
    // The other runbooks point at it.
    for file in ["README.md", "restart-survival.md"] {
        assert!(
            read(&e2e_dir().join(file)).contains("routine-dynamic.md"),
            "{file} does not link routine-dynamic.md"
        );
    }
}

#[test]
fn the_dynamic_routine_script_cleans_up_and_reports() {
    let text = read(&e2e_dir().join("routine-dynamic.sh"));
    assert!(text.contains("trap r_cleanup EXIT"));
    assert!(text.contains("r_finish"));
    assert!(text.contains("D_SOURCE_ONLY"));
    // A server is started only through the holder helper, and the token never goes on a
    // command line (the helpers read the token files).
    assert!(!text.contains("--auto") && !text.contains("allow_arbitrary_command\": True"));
    assert!(!text.contains("Authorization"));
}

/// Run `d_setup` of routine-dynamic.sh in a scratch directory and return it. No server starts.
fn dynamic_setup(base: &Path, dynamic: &str) -> (PathBuf, PathBuf) {
    let (home, work) = (
        base.join("shepherd-e2e-restart-doc-home"),
        base.join("shepherd-e2e-restart-doc-work"),
    );
    std::fs::create_dir_all(base).unwrap();
    let out = Command::new("bash")
        .arg("-c")
        .arg("D_SOURCE_ONLY=1 source \"$SCRIPT\" && d_setup && d_write_profiles \"$DYNAMIC\"")
        .env("SCRIPT", e2e_dir().join("routine-dynamic.sh"))
        .env("R_HOME", &home)
        .env("R_WORK", &work)
        .env("SHEPHERD_BIN", "/bin/sh")
        .env("DYNAMIC", dynamic)
        .output()
        .expect("run d_setup");
    assert!(
        out.status.success(),
        "d_setup failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (home, work)
}

#[test]
fn the_files_the_dynamic_script_writes_are_accepted_by_the_real_loaders() {
    let base =
        std::env::temp_dir().join(format!("shepherd-doctest-dynamic-{}", std::process::id()));
    let (home, work) = dynamic_setup(&base, "on");

    let policy = LaunchPolicy::from_json(&read(&home.join("profiles.json")))
        .expect("the profiles file is accepted by the loader");
    let dyn_profile = policy.profile("dyn").expect("profile dyn");
    let ceiling = dyn_profile.dynamic_routines.as_ref().expect("dyn opts in");
    assert_eq!(ceiling.min_every_secs, 60);
    assert_eq!(ceiling.max_expires_secs, 3600);
    assert_eq!(
        ceiling.max_per_pane,
        DynamicRoutines::default().max_per_pane
    );
    assert!(policy.profile("plain").unwrap().dynamic_routines.is_none());
    assert!(policy.has_dynamic_routines());
    for pane in ["dyn", "busy", "exp"] {
        assert!(dyn_profile.cwds.iter().any(|d| d.ends_with(pane)), "{pane}");
        assert!(work.join(pane).is_dir());
    }
    assert!(work.join("dyn/todo.md").is_file());

    // `off` leaves the key out and the file still loads.
    dynamic_setup(&base, "off");
    let off = LaunchPolicy::from_json(&read(&home.join("profiles.json"))).expect("loads");
    assert!(off.profile("dyn").unwrap().dynamic_routines.is_none());
    assert!(!off.has_dynamic_routines());

    // The clients file: the hashes match the token files, `agent-a` holds `routine` and not
    // `control`, `ops` holds no `routine`.
    let clients = Clients::load(&home.join("clients.json"), &root()).expect("clients file loads");
    let who = |id: &str| {
        let token = read(&home.join(format!("{id}.token")));
        assert!(token.starts_with(&format!("osh_{id}_")), "{id}");
        clients
            .authenticate(Some(&format!("Bearer {}", token.trim())), 1)
            .unwrap_or_else(|e| panic!("{id} does not authenticate: {e:?}"))
    };
    let agent = who("agent-a");
    assert!(agent.has(Scope::Routine) && agent.has(Scope::Observe));
    assert!(
        !agent.has(Scope::Control),
        "routine must not come with control"
    );
    let ops = who("ops");
    assert!(!ops.has(Scope::Routine) && ops.has(Scope::Start));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for file in [
            "ops.token",
            "agent-a.token",
            "clients.json",
            "profiles.json",
        ] {
            assert_eq!(mode(&home.join(file)), 0o600, "{file}");
        }
        assert_eq!(mode(&home.join("audit")), 0o700);
    }
    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn the_docs_name_every_routine_tool_scope_and_error_where_they_belong() {
    let ops = read(&root().join("docs/operations.md"));
    let routine = read(&root().join("docs/routine.md"));
    let readme = read(&root().join("README.md"));
    for tool in ROUTINE_TOOLS {
        assert!(TOOL_NAMES.contains(&tool), "{tool} is not a tool");
        assert!(
            ops.contains(&format!("| `{tool}` |")),
            "operations.md table omits {tool}"
        );
        assert!(
            routine.contains(&format!("| `{tool}` |")),
            "routine.md table omits {tool}"
        );
        assert!(readme.contains(&format!("`{tool}`")), "README omits {tool}");
    }
    assert_eq!(
        TOOL_NAMES.len(),
        14,
        "the docs say fourteen tools; update them"
    );
    assert!(ops.contains("The fourteen tools"));
    for code in ROUTINE_ERRORS {
        assert!(
            ops.contains(&format!("| `{code}` |")),
            "operations.md errors omit {code}"
        );
        assert!(
            routine.contains(&format!("| `{code}` |")),
            "routine.md errors omit {code}"
        );
    }
    assert!(ops.contains(
        "| `routine` | `routine_create`, `routine_list`, `routine_update`, `routine_delete` |"
    ));
    assert!(ops.contains("`routine` does not imply `control`"));
    assert!(read(&root().join("docs/credentials.md")).contains("## An agent as an MCP client"));
    assert!(read(&root().join("docs/adr/trust-model.md")).contains("`routine` scope"));

    // What the docs say about the legacy token and `ALL` is what the code does.
    assert!(!Scope::LEGACY.contains(&Scope::Routine));
    assert!(Scope::ALL.contains(&Scope::Routine));
    assert_eq!(Scope::parse("routine"), Some(Scope::Routine));
}

#[test]
fn the_documented_limits_are_the_limits_in_the_code() {
    let ops = read(&root().join("docs/operations.md"));
    let routine = read(&root().join("docs/routine.md"));
    let range = |lo: u64, hi: u64| format!("{lo} to {hi}");
    // routine.md: the ceiling table and the per-routine table.
    for needle in [
        range(EVERY_MIN_SECS, EVERY_MAX_SECS),
        range(PER_PANE_MIN, PER_PANE_MAX),
        range(CEILING_EXPIRES_MIN_SECS, EXPIRES_MAX_SECS),
        range(EXPIRES_MIN_SECS, EXPIRES_MAX_SECS),
        range(MAX_RUNS_MIN, MAX_RUNS_MAX),
        format!("{TEXT_MAX_BYTES} bytes"),
    ] {
        assert!(routine.contains(&needle), "routine.md omits {needle:?}");
    }
    // operations.md: the profile key.
    for needle in [
        range(EVERY_MIN_SECS, EVERY_MAX_SECS),
        range(PER_PANE_MIN, PER_PANE_MAX),
        range(CEILING_EXPIRES_MIN_SECS, EXPIRES_MAX_SECS),
    ] {
        assert!(ops.contains(&needle), "operations.md omits {needle:?}");
    }
    let defaults = DynamicRoutines::default();
    for needle in [
        format!(
            "`min_every_secs` (60 to 604800, default {})",
            defaults.min_every_secs
        ),
        format!(
            "`max_per_pane` (1 to 10, default\n  {})",
            defaults.max_per_pane
        ),
    ] {
        assert!(ops.contains(&needle), "operations.md omits {needle:?}");
    }
}

#[test]
fn the_dynamic_routines_example_in_routine_md_loads_with_the_documented_defaults() {
    let routine = read(&root().join("docs/routine.md"));
    let at = routine
        .find("\"dynamic_routines\":")
        .expect("routine.md has the example");
    let line = routine[at..].lines().next().unwrap();
    let object = line.trim_start_matches("\"dynamic_routines\":").trim();
    let base =
        std::env::temp_dir().join(format!("shepherd-doctest-ceiling-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let json = format!(
        r#"{{"profiles": [{{"name": "p", "agent": "kiro", "argv": ["/bin/sh"],
            "cwds": [{cwd:?}], "dynamic_routines": {object}}}]}}"#,
        cwd = base.to_string_lossy()
    );
    let policy = LaunchPolicy::from_json(&json).expect("the documented example loads");
    assert_eq!(
        policy.profile("p").unwrap().dynamic_routines,
        Some(DynamicRoutines::default()),
        "the example shows the defaults"
    );
    std::fs::remove_dir_all(&base).ok();
}
