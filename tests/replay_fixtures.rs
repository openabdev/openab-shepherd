//! The recorded replay fixtures of issue #76 (`tests/fixtures/replay/<agent>/`), made by
//! `examples/record_tail.rs` through `docs/operator-e2e/record-replay-fixture.sh`.
//!
//! Only kiro and opencode are recorded; claude and codex are on hold (issue #93). This test
//! checks the files; replaying each tail into a fresh emulator and comparing the final screen
//! is `native::tests_replay_fixtures` (the emulator is crate-private).

use std::path::PathBuf;

const AGENTS: [&str; 2] = ["kiro", "opencode"];

fn dir(agent: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay")
        .join(agent)
}

#[test]
fn every_agent_has_complete_recordings() {
    for agent in AGENTS {
        let raws: Vec<_> = std::fs::read_dir(dir(agent))
            .unwrap_or_else(|e| panic!("{agent}: no fixture directory: {e}"))
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "raw"))
            .collect();
        assert!(raws.len() >= 3, "{agent}: expected 3 .raw recordings");
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
