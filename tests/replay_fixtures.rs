//! The recorded replay fixtures of issue #76 (`tests/fixtures/replay/<agent>/`), made by
//! `examples/record_tail.rs` through `docs/operator-e2e/record-replay-fixture.sh`.
//!
//! Ignored until the user has recorded them: the recordings need a logged-in CLI per
//! agent, and closing #76 waits for them, not the merge of any code. Run with
//! `cargo test --test replay_fixtures -- --ignored`.

use std::path::PathBuf;

const AGENTS: [&str; 4] = ["claude", "codex", "kiro", "opencode"];

fn dir(agent: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay")
        .join(agent)
}

#[test]
#[ignore = "waits for the user's recordings (issue #76)"]
fn every_agent_has_complete_recordings() {
    for agent in AGENTS {
        let raws: Vec<_> = std::fs::read_dir(dir(agent))
            .unwrap_or_else(|e| panic!("{agent}: no fixture directory: {e}"))
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "raw"))
            .collect();
        assert!(!raws.is_empty(), "{agent}: no .raw recording");
        for raw in raws {
            assert!(
                std::fs::metadata(&raw).unwrap().len() > 0,
                "{}: empty",
                raw.display()
            );
            let summary = raw.with_extension("json");
            let text = std::fs::read_to_string(&summary)
                .unwrap_or_else(|e| panic!("{}: {e}", summary.display()));
            let value: serde_json::Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", summary.display()));
            assert_eq!(
                value["gaps"],
                0,
                "{}: the recording has gaps",
                summary.display()
            );
            assert!(
                raw.with_extension("screen.txt").is_file(),
                "{}: no screen file",
                raw.display()
            );
        }
    }
}
