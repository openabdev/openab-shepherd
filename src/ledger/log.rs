//! The log format and replay.
//!
//! One record per line: a JSON object `{"v":1,"seq":N,"type":...}` ended by
//! `\n`. A record exists only if its newline was written, so a final line
//! without one is a torn write and is dropped. A complete line that does not
//! parse, skips a sequence number or does not fit the state is corruption, and
//! replay stops with an error instead of guessing.

use super::event::Event;
use super::state::{LedgerState, TransitionError};
use serde_json::Value;
use std::fmt;

/// The format version written in every record.
pub const FORMAT_VERSION: u64 = 1;

/// Encode one record, newline included.
pub fn encode_record(seq: u64, event: &Event) -> Vec<u8> {
    let mut object = event.to_object();
    object.insert("v".into(), FORMAT_VERSION.into());
    object.insert("seq".into(), seq.into());
    let mut line = Value::Object(object).to_string().into_bytes();
    line.push(b'\n');
    line
}

/// The log cannot be replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayError {
    /// 1-based line number of the bad record.
    pub line: u64,
    pub reason: String,
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ledger log line {}: {}", self.line, self.reason)
    }
}

impl std::error::Error for ReplayError {}

/// The result of reading a log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replay {
    pub state: LedgerState,
    /// The `seq` the next record gets.
    pub next_seq: u64,
    /// Records read.
    pub records: u64,
    /// Length of the log up to and including the last complete record.
    pub valid_len: u64,
    /// Bytes after `valid_len`: a torn final record, to be cut off.
    pub torn_bytes: u64,
}

/// Rebuild the state from the bytes of a log. Pure and repeatable: the same
/// bytes always give the same result.
pub fn replay(bytes: &[u8]) -> Result<Replay, ReplayError> {
    let mut state = LedgerState::default();
    let mut pos = 0usize;
    let mut seq = 1u64;
    let mut line_no = 0u64;
    while let Some(rel) = bytes[pos..].iter().position(|&b| b == b'\n') {
        line_no += 1;
        let fail = |reason: String| ReplayError {
            line: line_no,
            reason,
        };
        let line = &bytes[pos..pos + rel];
        let value: Value =
            serde_json::from_slice(line).map_err(|e| fail(format!("not JSON: {e}")))?;
        let object = value
            .as_object()
            .ok_or_else(|| fail("not a JSON object".into()))?;
        if object.get("v").and_then(Value::as_u64) != Some(FORMAT_VERSION) {
            return Err(fail("unsupported format version".into()));
        }
        if object.get("seq").and_then(Value::as_u64) != Some(seq) {
            return Err(fail(format!("sequence number is not {seq}")));
        }
        let event = Event::from_object(object).map_err(fail)?;
        state
            .apply(&event)
            .map_err(|e: TransitionError| fail(format!("{} does not fit: {e}", event.kind())))?;
        seq += 1;
        pos += rel + 1;
    }
    Ok(Replay {
        state,
        next_seq: seq,
        records: seq - 1,
        valid_len: pos as u64,
        torn_bytes: (bytes.len() - pos) as u64,
    })
}
