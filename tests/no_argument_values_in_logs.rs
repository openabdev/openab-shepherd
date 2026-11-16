//! Argument values (`argv`, `line`, call-level `extra_args`) are never written to a log
//! (issue #113, D11). The audit file is covered by the canary tests in `src/mcp.rs` and
//! `src/audit.rs`; this covers the other channel, stderr: no `eprintln!` in production code
//! names an identifier that holds a call's arguments.

use std::path::{Path, PathBuf};

/// Identifiers that hold, or are, the arguments of a call.
const FORBIDDEN: [&str; 4] = ["args", "argv", "extra_args", "line"];

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
    found
}

/// Unit tests sit at the end of each file, in a top-level `#[cfg(test)] mod ...`.
fn production_source(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap();
    match text.find("\n#[cfg(test)]\nmod ") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// The text of each `eprintln!(...)` call: from the open parenthesis to its match.
/// String literals are skipped over, so a parenthesis inside a message does not count.
fn eprintln_calls(source: &str) -> Vec<&str> {
    let mut calls = Vec::new();
    let mut from = 0;
    while let Some(at) = source[from..].find("eprintln!(") {
        let start = from + at + "eprintln!(".len();
        let (mut depth, mut in_string, mut escaped) = (1, false, false);
        let mut end = source.len();
        for (i, c) in source[start..].char_indices() {
            if in_string {
                match (escaped, c) {
                    (true, _) => escaped = false,
                    (false, '\\') => escaped = true,
                    (false, '"') => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        calls.push(&source[start..end]);
        from = end;
    }
    calls
}

fn identifiers(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .collect()
}

/// Identifiers of one call that are not inside a string literal's prose: the `{name}`
/// placeholders of the format string and everything after the string.
fn used_identifiers(call: &str) -> Vec<&str> {
    let mut used = Vec::new();
    let mut rest = call;
    while let Some(open) = rest.find('{') {
        match rest[open..].find('}') {
            Some(close) => {
                used.extend(identifiers(&rest[open + 1..open + close]));
                rest = &rest[open + close..];
            }
            None => break,
        }
    }
    // The arguments after the format string: the last string literal ends the message.
    if let Some(end) = call.rfind('"') {
        used.extend(identifiers(&call[end + 1..]));
    }
    used
}

#[test]
fn no_production_eprintln_names_a_call_argument() {
    let mut checked = 0;
    for path in rust_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src")) {
        let source = production_source(&path);
        for call in eprintln_calls(&source) {
            checked += 1;
            for word in used_identifiers(call) {
                assert!(
                    !FORBIDDEN.contains(&word),
                    "{}: eprintln! uses {word:?}:\n{call}",
                    path.display()
                );
            }
        }
    }
    assert!(
        checked > 30,
        "the scan found only {checked} eprintln! calls"
    );
}

#[test]
fn the_scan_sees_a_forbidden_name_in_a_placeholder_and_in_an_argument() {
    let calls = eprintln_calls("eprintln!(\"a (b) {argv}\"); eprintln!(\"x {}\", spec.line);");
    assert_eq!(calls.len(), 2);
    assert!(used_identifiers(calls[0]).contains(&"argv"));
    assert!(used_identifiers(calls[1]).contains(&"line"));
    // Prose that merely contains the word is not a use.
    let calls = eprintln_calls("eprintln!(\"the line was empty\");");
    assert!(used_identifiers(calls[0]).is_empty());
}
