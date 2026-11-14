//! Snapshot of the MCP tool contract: tool names and input schemas, as
//! `tools/list` reports them (`openab_shepherd::tools::definitions()`).
//!
//! The snapshot is `tests/snapshots/tool_schemas.json`. A mismatch means a tool
//! name, argument, type, bound or argument description changed. Those are the
//! contract every MCP client relies on, so the change must be visible in review:
//! update the snapshot in the same PR.
//!
//! Update: `UPDATE_SNAPSHOTS=1 cargo test --test tool_schema_snapshot`.
//! The variable is ignored when `CI` is set, so CI never rewrites the file.
//!
//! Only names and input schemas are snapshotted. The tool descriptions are prose,
//! and the result fields docs/operations.md calls stable exist only as prose and
//! tables there, not in machine-readable form.

use serde_json::{json, Map, Value};
use std::path::PathBuf;

const SNAPSHOT: &str = "tests/snapshots/tool_schemas.json";

/// Rebuild a value with object keys sorted, whatever order serde_json keeps.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), canonical(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

fn current() -> String {
    let defs = openab_shepherd::tools::definitions();
    let mut tools: Vec<(String, Value)> = defs
        .as_array()
        .expect("definitions() is an array")
        .iter()
        .map(|d| {
            let name = d["name"].as_str().expect("tool has a name").to_string();
            let schema = d.get("inputSchema").expect("tool has inputSchema");
            (
                name.clone(),
                json!({"name": name, "inputSchema": canonical(schema)}),
            )
        })
        .collect();
    tools.sort_by(|a, b| a.0.cmp(&b.0));
    let list: Vec<Value> = tools.into_iter().map(|t| t.1).collect();
    let mut s = serde_json::to_string_pretty(&canonical(&json!({"tools": list}))).unwrap();
    s.push('\n');
    s
}

#[test]
fn tool_names_and_input_schemas_match_the_snapshot() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(SNAPSHOT);
    let now = current();
    let in_ci = std::env::var_os("CI").is_some();
    let update = !in_ci && std::env::var("UPDATE_SNAPSHOTS").is_ok_and(|v| v == "1");
    if update {
        std::fs::write(&path, &now).expect("write snapshot");
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        want == now,
        "\nThe MCP tool names or input schemas differ from {SNAPSHOT}.\n\
         These are the contract every client relies on, so a change needs review:\n\
         a client may break on a renamed tool, a removed or retyped argument, or a\n\
         tighter bound. If the change is intended, regenerate the snapshot and commit\n\
         it in the same PR so the diff shows the contract change:\n\
         \n    UPDATE_SNAPSHOTS=1 cargo test --test tool_schema_snapshot\n\n\
         (UPDATE_SNAPSHOTS is ignored when CI is set.)\n\n--- current ---\n{now}"
    );
}
