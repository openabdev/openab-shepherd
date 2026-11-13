//! The built binary refuses a backend other than native with exit status 2,
//! before it opens any listener.

use std::process::Command;

// Example value only; not a credential.
const TOKEN: &str = "test-token-0123456789abcdef";

fn run(backend: Option<&str>) -> std::process::Output {
    let dir = std::env::temp_dir();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_openab-shepherd"));
    cmd.env_clear()
        .current_dir(dir)
        .env("OPENAB_SHEPHERD_TOKEN", TOKEN)
        // Must be ignored: no code reads it.
        .env("HC_SOCKET", "/tmp/shepherd-test-absent.sock");
    if let Some(value) = backend {
        cmd.env("OPENAB_SHEPHERD_BACKEND", value);
    }
    cmd.output().unwrap()
}

#[test]
fn hc_exits_2_and_says_it_was_removed() {
    let out = run(Some("hc"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("hc"), "{stderr}");
    assert!(stderr.contains("removed"), "{stderr}");
    assert!(stderr.contains("native is the only backend"), "{stderr}");
}

#[test]
fn any_other_backend_exits_2_and_names_the_value() {
    for value in ["tmux", "HC"] {
        let out = run(Some(value));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{value}: {stderr}");
        assert!(stderr.contains(&format!("{value:?}")), "{value}: {stderr}");
        assert!(
            stderr.contains("native is the only backend"),
            "{value}: {stderr}"
        );
    }
}
