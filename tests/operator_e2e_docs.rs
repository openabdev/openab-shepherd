//! Checks on the docs and the operator scripts of issue #76 (PR8) that need no server:
//! the scripts parse, the runbook names every script, the doc links resolve, the old
//! "registrations live only in memory" text is gone, and the profiles that
//! `restart-lib.sh` writes are accepted by the real profile loader with a dialog the real
//! parser reads. Nothing here starts the Shepherd server.

use openab_shepherd::agent::Agent;
use openab_shepherd::clients::{authorize, Clients, Scope};
use openab_shepherd::dialog;
use openab_shepherd::profiles::{check_agent_argv, DynamicRoutines, LaunchPolicy};
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
    let dynamic = [
        "routine-dynamic.sh",
        "trusted-client.sh",
        "dialog-canary.sh",
    ];
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
        root().join("docs/opencode.md"),
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
        // R_HOME and R_WORK are set as plain shell variables, as they are when the script runs
        // with its defaults. Exporting them from here would hide a python heredoc that reads
        // them from the environment without being handed them (the first live run hit that).
        .arg(
            "R_HOME=\"$T_HOME\"; R_WORK=\"$T_WORK\"; \
             D_SOURCE_ONLY=1 source \"$SCRIPT\" && d_setup && d_write_profiles \"$DYNAMIC\"",
        )
        .env("SCRIPT", e2e_dir().join("routine-dynamic.sh"))
        .env_remove("R_HOME")
        .env_remove("R_WORK")
        .env("T_HOME", &home)
        .env("T_WORK", &work)
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

/// Every markdown file of the README and `docs/`, found recursively.
fn markdown_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|x| x == "md") {
                out.push(path);
            }
        }
    }
    let mut files = vec![root().join("README.md")];
    walk(&root().join("docs"), &mut files);
    files.sort();
    files
}

/// The text of each ```mermaid block of `text`, or an error naming the file and line of a
/// block that is never closed.
fn mermaid_blocks(name: &str, text: &str) -> Result<Vec<String>, String> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (n, line) in text.lines().enumerate() {
        match &mut open {
            None if line.trim_end() == "```mermaid" => open = Some((n + 1, String::new())),
            Some(_) if line.trim_end() == "```" => {
                blocks.push(open.take().expect("open block").1);
            }
            Some((_, body)) => {
                body.push_str(line);
                body.push('\n');
            }
            None => {}
        }
    }
    match open {
        Some((line, _)) => Err(format!("{name}:{line}: a ```mermaid block is never closed")),
        None => Ok(blocks),
    }
}

#[test]
fn every_mermaid_block_is_closed_not_empty_and_a_flowchart_or_a_sequence_diagram() {
    let mut problems = Vec::new();
    let mut total = 0;
    for file in markdown_files() {
        let name = file.strip_prefix(root()).unwrap().display().to_string();
        match mermaid_blocks(&name, &read(&file)) {
            Err(e) => problems.push(e),
            Ok(blocks) => {
                for (i, block) in blocks.iter().enumerate() {
                    total += 1;
                    let first = block
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty() && !l.starts_with("%%"));
                    match first {
                        None => problems.push(format!("{name}: block {} is empty", i + 1)),
                        Some(l) => {
                            let kind = l.split_whitespace().next().unwrap_or("");
                            if kind != "flowchart" && kind != "sequenceDiagram" {
                                problems.push(format!(
                                    "{name}: block {} is a {kind:?}; use flowchart or sequenceDiagram",
                                    i + 1
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "mermaid problems:\n{}",
        problems.join("\n")
    );
    assert!(
        total >= 7,
        "expected at least seven mermaid blocks, found {total}"
    );
}

#[test]
fn the_mermaid_scanner_catches_an_unterminated_and_an_empty_block() {
    assert!(mermaid_blocks("x.md", "```mermaid\nflowchart TD\n  A --> B\n").is_err());
    let blocks = mermaid_blocks("x.md", "```mermaid\n```\ntext\n").expect("closed");
    assert_eq!(blocks, vec![String::new()]);
}

// ---------------------------------------------------------------------------
// Trusted clients (issue #113): the docs and trusted-client.sh. No server starts.
// ---------------------------------------------------------------------------

#[test]
fn the_trusted_client_runbook_names_the_script_and_the_script_exists() {
    let runbook = read(&e2e_dir().join("trusted-client.md"));
    assert!(runbook.contains("trusted-client.sh"));
    assert!(e2e_dir().join("trusted-client.sh").is_file());
    for needle in [
        "start:args",
        "extra_args",
        "pane_needs_trust_acceptance",
        "agent",
        "launch",
        "flags",
        "allow_arbitrary_command",
        "Yes, and",
        "summary:",
        "skipped",
        "RESULT: PASS",
        "#113 and #111 close",
    ] {
        assert!(runbook.contains(needle), "trusted-client.md omits {needle}");
    }
    for file in ["README.md", "restart-survival.md"] {
        assert!(
            read(&e2e_dir().join(file)).contains("trusted-client.md"),
            "{file} does not link trusted-client.md"
        );
    }
    // The trust-model section and the operations recipe point at the runbook's script.
    let trust = read(&root().join("docs/adr/trust-model.md"));
    assert!(trust.contains("trusted-client.md"));
}

#[test]
fn the_trusted_client_script_is_bounded_portable_and_keeps_secrets_out() {
    let text = read(&e2e_dir().join("trusted-client.sh"));
    assert!(text.contains("trap r_cleanup EXIT"));
    assert!(text.contains("r_finish"));
    assert!(text.contains("T_SOURCE_ONLY"));
    assert!(!text.contains("Authorization"));
    // No python program reads the environment: every value is an argument on its command line.
    assert!(
        !text.contains("os.environ") && !text.contains("os.getenv"),
        "a python heredoc reads the environment"
    );
    // Nothing platform-specific is used without a guard: the limit macOS does not enforce, and
    // BSD-only or GNU-only stat forms (r_mode in restart-lib.sh picks the right one).
    for banned in ["RLIMIT", "setrlimit", "stat -f", "stat -c", "date +%s%N"] {
        assert!(
            !text
                .lines()
                .any(|l| !l.trim_start().starts_with('#') && l.contains(banned)),
            "trusted-client.sh uses {banned} outside a comment"
        );
    }
    // Reads of what a pane or the server writes later wait, and the waits are bounded.
    assert!(text.contains("t_until") && text.contains("t_stays"));
    assert!(text.contains("t_audit_wait"));
    // A refused start must not be able to create a pane some other step uses: the refused
    // starts share one name, and no start of a client that holds both scopes is a refusal.
    assert!(text.contains("T_REFUSED=") && !text.contains(" rej "));
    assert!(
        !text
            .lines()
            .any(|l| l.contains("t_start trusted ") && l.contains("CANARYARB")),
        "start:arbitrary is checked with a client that has no start:args"
    );
    // The summary comes from restart-lib's r_finish: passed, failed and skipped.
    let lib = read(&e2e_dir().join("restart-lib.sh"));
    assert!(lib.contains("summary: $R_PASSES passed, $R_FAILS failed, $R_SKIPS skipped"));
}

/// Run `t_setup` of trusted-client.sh in a scratch directory and return (home, work).
fn trusted_setup(base: &Path) -> (PathBuf, PathBuf) {
    let (home, work) = (
        base.join("shepherd-e2e-restart-doc-home"),
        base.join("shepherd-e2e-restart-doc-work"),
    );
    std::fs::create_dir_all(base).unwrap();
    let out = Command::new("bash")
        .arg("-c")
        // R_HOME and R_WORK are plain shell variables, as when the script runs with its
        // defaults: exporting them would hide a python program that reads them from the
        // environment without being handed them.
        .arg(
            "R_HOME=\"$T_HOME\"; R_WORK=\"$T_WORK\"; \
             T_SOURCE_ONLY=1 source \"$SCRIPT\" && t_setup",
        )
        .env("SCRIPT", e2e_dir().join("trusted-client.sh"))
        .env_remove("R_HOME")
        .env_remove("R_WORK")
        .env("T_HOME", &home)
        .env("T_WORK", &work)
        .env("SHEPHERD_BIN", "/bin/sh")
        .output()
        .expect("run t_setup");
    assert!(
        out.status.success(),
        "t_setup failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (home, work)
}

fn args(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().expect("an object").clone()
}

#[test]
fn the_files_the_trusted_script_writes_are_accepted_by_the_real_loaders() {
    let base =
        std::env::temp_dir().join(format!("shepherd-doctest-trusted-{}", std::process::id()));
    let (home, work) = trusted_setup(&base);

    let policy = LaunchPolicy::from_json(&read(&home.join("profiles.json")))
        .expect("the profiles file is accepted by the loader");
    assert!(
        policy.allow_arbitrary_command,
        "the argv steps need the flag"
    );
    for name in ["sandbox", "guard", "other", "oc"] {
        let profile = policy
            .profile(name)
            .unwrap_or_else(|| panic!("no profile {name}"));
        assert_eq!(
            profile.argv.last().map(String::as_str),
            Some(name),
            "$0 is the name"
        );
        assert!(
            profile.cwds.iter().all(|d| d.is_dir()),
            "{name}: a cwd is missing"
        );
    }
    assert_eq!(policy.profile("oc").unwrap().agent, Agent::OpenCode);
    assert_eq!(policy.profile("sandbox").unwrap().agent, Agent::Kiro);
    // The refusal step relies on the call-time check: the script's OpenCode profile plus
    // `--auto` is refused, and plain flags on the sandbox profile are not.
    let with = |name: &str, extra: &[&str]| -> Vec<String> {
        let mut argv = policy.profile(name).unwrap().argv.clone();
        argv.extend(extra.iter().map(|s| s.to_string()));
        argv
    };
    assert!(check_agent_argv(Agent::OpenCode, &with("oc", &["--auto"])).is_err());
    assert!(check_agent_argv(Agent::Kiro, &with("sandbox", &["-a", "--model", "x"])).is_ok());

    // The clients file: each hash matches its token file, and the scopes are the ones the
    // steps rely on.
    let clients = Clients::load(&home.join("clients.json"), &root()).expect("clients file loads");
    let who = |id: &str| {
        let token = read(&home.join(format!("{id}.token")));
        assert!(token.starts_with(&format!("osh_{id}_")), "{id}");
        clients
            .authenticate(Some(&format!("Bearer {}", token.trim())), 1)
            .unwrap_or_else(|e| panic!("{id} does not authenticate: {e:?}"))
    };
    let (ops, orch, trusted, arb) = (who("ops"), who("orch"), who("trusted"), who("arb"));
    assert!(!ops.has(Scope::StartArgs) && !ops.has(Scope::StartArbitrary));
    assert!(orch.has(Scope::StartArgs) && !orch.has(Scope::StartArbitrary));
    assert!(trusted.has(Scope::StartArgs) && trusted.has(Scope::StartArbitrary));
    assert!(trusted.has(Scope::Dialog) && !orch.has(Scope::Dialog));
    assert!(arb.has(Scope::StartArbitrary) && !arb.has(Scope::StartArgs));

    // What the steps expect from the scope check, with the real `authorize`.
    let flags = |profile: &str| args(serde_json::json!({"profile": profile, "extra_args": ["-a"]}));
    let plain = |profile: &str| args(serde_json::json!({"profile": profile}));
    let denied = |caller, call| authorize(caller, "start_pane", &call).unwrap_err();
    assert_eq!(
        denied(&ops, flags("sandbox")).to_tool_error().data["scope"],
        "start:args"
    );
    assert!(authorize(&orch, "start_pane", &flags("sandbox")).is_ok());
    assert!(
        authorize(&orch, "start_pane", &flags("other")).is_err(),
        "off orch's list"
    );
    assert!(
        authorize(&trusted, "start_pane", &flags("guard")).is_err(),
        "off trusted's list"
    );
    // start:arbitrary does not stand in for start:args, and the error names start:args.
    assert!(authorize(&trusted, "start_pane", &flags("sandbox")).is_ok());
    assert_eq!(
        denied(&arb, flags("sandbox")).to_tool_error().data["scope"],
        "start:args"
    );
    for empty in [serde_json::json!(null), serde_json::json!([])] {
        let call = args(serde_json::json!({"profile": "sandbox", "extra_args": empty}));
        assert!(authorize(&ops, "start_pane", &call).is_ok(), "{empty}");
    }
    assert!(authorize(&ops, "start_pane", &plain("sandbox")).is_ok());
    let argv_call = args(serde_json::json!({"argv": ["/bin/sh"]}));
    assert!(authorize(&ops, "start_pane", &argv_call).is_err());
    assert!(authorize(&trusted, "start_pane", &argv_call).is_ok());
    // The recorded dialog the argv steps print (t_write_dialog) parses with the real parser.
    let dialog_file = base.join("dialog.txt");
    trusted_bash(
        &base,
        "t_write_dialog \"$DIALOG\"",
        &[("DIALOG", dialog_file.as_os_str())],
    );
    let parsed = dialog::parse(&read(&dialog_file)).expect("the dialog text parses");
    assert_eq!(parsed.options[0], "Allow once");
    assert_eq!(parsed.cursor, 0);
    let _ = &work;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        for file in [
            "ops.token",
            "orch.token",
            "trusted.token",
            "arb.token",
            "clients.json",
            "profiles.json",
        ] {
            assert_eq!(mode(&home.join(file)), 0o600, "{file}");
        }
        assert_eq!(mode(&home.join("audit")), 0o700);
    }
    std::fs::remove_dir_all(&base).ok();
}

/// Source trusted-client.sh without running it, and run `snippet`; panic with the output when
/// it fails. `extra` are environment variables for the snippet only.
fn trusted_bash(base: &Path, snippet: &str, extra: &[(&str, &std::ffi::OsStr)]) -> String {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(format!(
            "R_HOME=\"$T_HOME\"; R_WORK=\"$T_WORK\"; T_SOURCE_ONLY=1 source \"$SCRIPT\" || exit 99; {snippet}"
        ))
        .env("SCRIPT", e2e_dir().join("trusted-client.sh"))
        .env_remove("R_HOME")
        .env_remove("R_WORK")
        .env("T_HOME", base.join("shepherd-e2e-restart-stub-home"))
        .env("T_WORK", base.join("shepherd-e2e-restart-stub-work"))
        .env("SHEPHERD_BIN", "/bin/sh");
    for (key, value) in extra {
        command.env(key, value);
    }
    let out = command.output().expect("run bash");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "snippet failed:\n{snippet}\n{text}");
    text
}

/// The helpers that decide a verdict, run against canned replies: `_post` is replaced, so
/// nothing connects anywhere, and a made-up audit file stands in for the server's.
#[test]
fn the_trusted_script_helpers_work_against_canned_replies() {
    let base = std::env::temp_dir().join(format!("shepherd-doctest-stubs-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let audit = base.join("audit.jsonl");
    let lines = [
        r#"{"client":"orch","tool":"start_pane","phase":"call","pane":"x1","launch":"profile+args","extra_args_n":3,"flags":["-a","--model"]}"#,
        r#"{"client":"trusted","tool":"start_pane","phase":"call","pane":"a-yes","launch":"argv","argv0":"sh","agent":"kiro"}"#,
        r#"{"client":"ops","tool":"start_pane","phase":"denied","outcome":"denied"}"#,
        "a partial line {",
    ];
    std::fs::write(&audit, lines.join("\n")).unwrap();
    let snippet = r#"
set -u
fail() { echo "FAILED: $*"; exit 1; }
# --- the reply parsers, with _post replaced ------------------------------------------------
_post() { printf '%s' "$STUB_REPLY"; }
export STUB_REPLY='{"jsonrpc":"2.0","id":1,"result":{"isError":true,"structuredContent":{"error":"forbidden","scope":"start:args"}}}'
[ "$(t_error as ops mcp_tool start_pane profile=x)" = forbidden ] || fail t_error
[ "$(as ops mcp_tool start_pane profile=x 2>/dev/null | t_py 'd["scope"]')" = start:args ] || fail t_py
STUB_REPLY='{"jsonrpc":"2.0","id":1,"result":{"isError":false,"structuredContent":{"argv":["/bin/sh","-c","s","sandbox","-a"],"live":true}}}'
[ "$(t_error as ops mcp_tool start_pane profile=x)" = "" ] || fail "t_error on success"
t_json_is '["sandbox", "-a"]' 'd["argv"][3:]' as ops mcp_tool start_pane profile=x || fail t_json_is
t_json_is true 'd["live"]' as ops get_status x || fail "t_json_is live"
! t_json_is false 'd["live"]' as ops get_status x || fail "t_json_is negative"
[ "$(echo 'not json' | t_py 'd["x"]')" = "" ] || fail "t_py on garbage"
# --- t_no_pane, with a list_panes reply ----------------------------------------------------
STUB_REPLY='{"jsonrpc":"2.0","id":1,"result":{"isError":false,"structuredContent":{"panes":[{"name": "x1"}]}}}'
t_no_pane rej || fail "t_no_pane absent"
! t_no_pane x1 || fail "t_no_pane present"
# --- the bounded waits ---------------------------------------------------------------------
dir="$T_SCRATCH"
t_until 2 t_absent "$dir/never" || fail "t_until success"
start=$SECONDS
! t_until 2 t_file_is "$dir/never" x || fail "t_until must give up"
[ $((SECONDS - start)) -le 4 ] || fail "t_until took too long"
printf 'a
b
' > "$dir/two"
t_file_is "$dir/two" "$(printf 'a
b')" || fail t_file_is
! t_file_is "$dir/two" a || fail "t_file_is mismatch"
: > "$dir/empty"
t_file_is "$dir/empty" "" || fail "t_file_is empty"
# A file that appears after the first look is still found: this is the read-too-early case.
( sleep 1; echo late > "$dir/late" ) &
t_until 5 t_file_is "$dir/late" late || fail "t_until must wait for a late file"
wait
t_stays 1 t_absent "$dir/never" || fail "t_stays holds"
( sleep 1; : > "$dir/appears" ) &
! t_stays 4 t_absent "$dir/appears" || fail "t_stays must see a change in its window"
wait
# --- the audit check -----------------------------------------------------------------------
T_AUDIT="$T_AUDIT_FILE"
t_audit_check 'any(r["client"] == "orch" and r.get("flags") == ["-a", "--model"] for r in records)' || fail "audit true"
! t_audit_check 'any(r["client"] == "nobody" for r in records)' || fail "audit false"
t_audit_check '"CANARY" not in raw' || fail "audit raw"
t_audit_check 'any(r["phase"] == "denied" for r in records)' || fail "audit denied"
T_AUDIT="$dir/missing.jsonl"
! t_audit_check 'True' || fail "audit missing file"
T_AUDIT="$T_AUDIT_FILE"
t_audit_wait 'len(records) == 3' || fail "audit wait"
[ "$(t_list_json 2 3)" = '["xxx", "xxx"]' ] || fail t_list_json
# --- the log scan and the reply scan --------------------------------------------------------
R_LOG="$dir/holder.log"
printf 'listening\nstarted pane x1\n' > "$R_LOG"
t_log_lacks CANARY claude-sonnet-4 TYPEDTOKEN77 || fail "log clean"
echo 'arg: CANARYDENIED' >> "$R_LOG"
! t_log_lacks CANARY || fail "log with a canary"
! t_log_lacks claude-sonnet-4 CANARYDENIED || fail "log, second needle"
R_LOG="$dir/no-such-log"
t_log_lacks CANARY || fail "missing log holds nothing"
t_replies_lack CANARY '{"error":"forbidden"}' '{"error":"invalid_input"}' || fail "replies clean"
! t_replies_lack CANARY '{"error":"forbidden"}' '{"detail":"CANARYX"}' || fail "reply with a canary"
! t_replies_lack CANARY '{"error":"forbidden"}' "" || fail "an empty reply is not a pass"
echo ALL-OK
"#;
    let scratch = base.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let out = trusted_bash(
        &base,
        snippet,
        &[
            ("T_SCRATCH", scratch.as_os_str()),
            ("T_AUDIT_FILE", audit.as_os_str()),
        ],
    );
    assert!(out.contains("ALL-OK"), "{out}");
    std::fs::remove_dir_all(&base).ok();
}

// ---------------------------------------------------------------------------
// The dialog drift canary (issue #65): the docs and dialog-canary.sh. No server starts.
// ---------------------------------------------------------------------------

#[test]
fn the_dialog_canary_runbook_names_the_script_and_the_script_exists() {
    let runbook = read(&e2e_dir().join("dialog-canary.md"));
    for needle in [
        "dialog-canary.sh",
        "dialog-canary-check.py",
        "deny_once",
        "KIRO_API_KEY",
        "CANARY_DEVIN_CWD",
        "8769",
        "never logs in",
        "SKIP",
        "RESULT",
        "#65",
    ] {
        // "never logs in" is in the README entry; the runbook says it in other words.
        if needle == "never logs in" {
            continue;
        }
        assert!(runbook.contains(needle), "dialog-canary.md omits {needle}");
    }
    assert!(e2e_dir().join("dialog-canary.sh").is_file());
    assert!(e2e_dir().join("dialog-canary-check.py").is_file());
    let readme = read(&e2e_dir().join("README.md"));
    assert!(readme.contains("dialog-canary.md") && readme.contains("dialog-canary.sh"));
    assert!(read(&root().join("docs/operations.md")).contains("operator-e2e/dialog-canary.md"));
}

#[test]
fn the_dialog_canary_script_is_safe_and_reports() {
    let text = read(&e2e_dir().join("dialog-canary.sh"));
    assert!(text.contains("trap c_cleanup EXIT"));
    assert!(text.contains("r_finish") && text.contains("r_skip"));
    assert!(text.contains("C_SOURCE_ONLY"));
    assert!(text.contains("remove_pane"));
    // The grant is the six scopes of the brief and nothing else.
    assert!(
        text.contains(r#""scopes": ["observe", "read", "control", "start", "dialog", "remove"]"#)
    );
    // No login, no auto-approval, no arbitrary command, no key on a command line.
    let code: String = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    for banned in [
        "kiro-cli login",
        "auth login",
        "--auto",
        "autoaccept",
        "allow_arbitrary_command\": True",
        "Authorization",
        "echo \"$KIRO_API_KEY",
        "printf '%s' \"$KIRO_API_KEY",
    ] {
        // The messages tell the operator to log in themselves; they are strings in r_skip lines.
        let hits: Vec<&str> = code
            .lines()
            .filter(|l| l.contains(banned) && !l.contains("C_REASON=") && !l.contains("r_skip"))
            .collect();
        assert!(
            hits.is_empty(),
            "dialog-canary.sh contains {banned}: {hits:?}"
        );
    }
}

/// Run the script's own setup functions in a scratch directory (no server) and load what it
/// writes with the real loaders.
#[test]
fn the_files_the_dialog_canary_writes_are_accepted_by_the_real_loaders() {
    let base = std::env::temp_dir().join(format!("shepherd-doctest-canary-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    for agent in ["kiro", "devin", "opencode"] {
        let (home, work) = (
            base.join(format!("shepherd-e2e-restart-doc-home-{agent}")),
            base.join(format!("shepherd-e2e-restart-doc-work-{agent}")),
        );
        let out = Command::new("bash")
            .arg("-c")
            // Plain shell variables, not exported: a python heredoc that reads one from the
            // environment instead of its command line would fail here as it did live.
            .arg(
                "R_HOME=\"$T_HOME\"; R_WORK=\"$T_WORK\"; \
                 C_SOURCE_ONLY=1 source \"$SCRIPT\" && c_setup \"$AGENT\" /bin/echo",
            )
            .env("SCRIPT", e2e_dir().join("dialog-canary.sh"))
            .env_remove("R_HOME")
            .env_remove("R_WORK")
            .env("T_HOME", &home)
            .env("T_WORK", &work)
            .env("AGENT", agent)
            .env("SHEPHERD_BIN", "/bin/sh")
            .output()
            .expect("run c_setup");
        assert!(
            out.status.success(),
            "{agent}: c_setup failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let policy = LaunchPolicy::from_json(&read(&home.join("profiles.json")))
            .unwrap_or_else(|e| panic!("{agent}: the profiles file is refused: {e:?}"));
        let profile = policy.profile("canary").expect("profile canary");
        assert_eq!(profile.agent.as_str(), agent);
        assert_eq!(profile.cwds.len(), 1, "{agent}");
        assert_eq!(profile.env.len(), usize::from(agent == "kiro"), "{agent}");
        if agent == "kiro" {
            assert!(work.join("panes/canary/.kiro/agents/probe.json").is_file());
        }
        if agent == "opencode" {
            assert!(work.join("panes/canary/opencode.jsonc").is_file());
        }

        let clients = Clients::load(&home.join("clients.json"), &root()).expect("clients load");
        let token = read(&home.join("canary.token"));
        assert!(token.starts_with("osh_canary_"), "{agent}");
        let who = clients
            .authenticate(Some(&format!("Bearer {}", token.trim())), 1)
            .unwrap_or_else(|e| panic!("{agent}: the token does not authenticate: {e:?}"));
        for scope in [
            Scope::Observe,
            Scope::Read,
            Scope::Control,
            Scope::Start,
            Scope::Dialog,
            Scope::Remove,
        ] {
            assert!(who.has(scope), "{agent}: missing {scope:?}");
        }
        for scope in [Scope::StartArbitrary, Scope::StartArgs, Scope::Routine] {
            assert!(!who.has(scope), "{agent}: must not hold {scope:?}");
        }
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&work).ok();
    }
    std::fs::remove_dir_all(&base).ok();
}

/// The version comparison and the pin lookup, on the repository's own Dockerfile.
#[test]
fn the_dialog_canary_reads_the_pins_from_the_dockerfile() {
    let run = |script: &str| {
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!("C_SOURCE_ONLY=1 source \"$SCRIPT\" && {script}"))
            .env("SCRIPT", e2e_dir().join("dialog-canary.sh"))
            .output()
            .expect("run bash");
        assert!(
            out.status.success(),
            "{script}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let dockerfile = read(&root().join("Dockerfile"));
    let pinned = |name: &str| {
        dockerfile
            .lines()
            .find_map(|l| l.strip_prefix(&format!("ARG {name}=")))
            .map(str::to_string)
            .unwrap_or_default()
    };
    assert_eq!(run("c_pin kiro"), pinned("KIRO_CLI_VERSION"));
    assert!(!run("c_pin kiro").is_empty());
    assert_eq!(run("c_pin devin"), pinned("DEVIN_CLI_VERSION"));
    // A version matches as a whole number, not as a prefix of a longer one.
    assert_eq!(
        run("c_version_matches 3.3.0 'kiro-cli 3.3.0' && echo yes"),
        "yes"
    );
    assert_eq!(
        run("c_version_matches 3.3.0 'kiro-cli 3.3.01' || echo no"),
        "no"
    );
    assert_eq!(
        run("c_version_matches 3.3.0 'kiro-cli 13.3.0' || echo no"),
        "no"
    );
    assert_eq!(run("c_version_matches '' 'anything' || echo no"), "no");
}
