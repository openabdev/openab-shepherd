//! Static check of the image's start-up lines (issue #76, PR7): tini stays PID 1's
//! entry point and the default command runs the pane holder. The bare
//! `openab-shepherd --host ... --port ...` stays reachable by overriding the command.

fn dockerfile_line(prefix: &str) -> String {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Dockerfile"))
        .expect("read the Dockerfile");
    let mut lines = text.lines().filter(|l| l.starts_with(prefix));
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("no {prefix} line"))
        .to_string();
    assert!(lines.next().is_none(), "more than one {prefix} line");
    line
}

#[test]
fn the_entrypoint_is_tini() {
    assert_eq!(
        dockerfile_line("ENTRYPOINT"),
        r#"ENTRYPOINT ["tini", "--"]"#
    );
}

#[test]
fn the_default_command_runs_the_holder_with_the_server_flags_after_the_separator() {
    assert_eq!(
        dockerfile_line("CMD"),
        r#"CMD ["openab-shepherd", "hold", "--", "--host", "127.0.0.1", "--port", "8765"]"#
    );
}
