//! Issue #76: replay each recorded tail (`tests/fixtures/replay/<agent>/*.raw`) into a fresh
//! emulator through the production `replay()` and compare the final screen with the saved
//! `.screen.txt`. kiro and opencode only; claude and codex are on hold (issue #93).

use super::replay::replay;
use std::path::PathBuf;

const RECORDED: [&str; 2] = ["kiro", "opencode"];

#[test]
fn recorded_tails_replay_to_the_saved_screen() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay");
    for agent in RECORDED {
        let mut raws: Vec<_> = std::fs::read_dir(root.join(agent))
            .unwrap_or_else(|e| panic!("{agent}: no fixture directory: {e}"))
            .map(|e| e.expect("entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "raw"))
            .collect();
        raws.sort();
        assert!(
            raws.len() >= 3,
            "{agent}: expected 3 recordings, found {}",
            raws.len()
        );
        for raw in raws {
            let bytes = std::fs::read(&raw).unwrap();
            assert!(!bytes.is_empty(), "{}: empty", raw.display());
            let saved = std::fs::read_to_string(raw.with_extension("screen.txt")).unwrap();
            assert!(
                !saved.trim().is_empty(),
                "{}: empty saved screen",
                raw.display()
            );
            let adopted = replay(&bytes, 0, None, 0);
            assert!(
                !adopted.replayed,
                "{}: complete tail is not wrapped",
                raw.display()
            );
            assert_eq!(
                adopted.emu.screen_text().trim_end(),
                saved.trim_end(),
                "{}: replayed screen differs from the saved screen",
                raw.display()
            );
        }
    }
}
