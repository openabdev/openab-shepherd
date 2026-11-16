//! A pane's metadata, kept in the host beside the pane (issue #76): what a
//! front that starts later needs to pick the pane up again and cannot read
//! from the screen. The host stores the bytes and never reads them.
//!
//! One [`MetaStore`] per pane is the only code that writes the host's `meta`.
//! Every writer (the tracker hook, the human-input flag, and later the tools
//! layer and the routine) changes the struct under the store's mutex and
//! writes the whole blob before it lets go, so two writers cannot overwrite
//! each other's fields and the host sees the writes in the order they were
//! made.

use super::host::PtyHost;
use super::status::TrackerSnapshot;
use crate::registry::RoutineSet;
use serde_json::{json, Value};
use std::io;
use std::sync::{Arc, Mutex, PoisonError};

/// The schema version this build writes and reads.
pub(crate) const META_VERSION: u64 = 1;

/// The largest meta blob a routines write may produce. The host refuses a
/// blob over 64 KiB (`META_CAP`); the rest is headroom for argv and the
/// tracker snapshot, whose size is not bounded here.
pub(crate) const ROUTINES_BLOB_CAP: usize = 48 * 1024;

/// The routine's counters for a pane, kept so a restart does not rearm a
/// routine that gave up or reset its nudge cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RoutineMeta {
    pub consecutive: u32,
    pub gave_up: bool,
    pub nudge_allowance: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PaneMeta {
    pub name: Option<String>,
    pub labels: Vec<(String, String)>,
    pub argv: Vec<String>,
    pub cwd: String,
    /// The agent kind and profile name, set by the tools layer.
    pub agent: Option<String>,
    pub profile: Option<String>,
    /// The tracker as it was after the output up to `tracker_seq`.
    pub tracker: Option<TrackerSnapshot>,
    /// The output offset the snapshot is exact at: the offset after the last
    /// byte the tracker had seen.
    pub tracker_seq: u64,
    /// A human typed into the pane. Set, and acknowledged by the host,
    /// before the human's first byte is written.
    pub human_draft: bool,
    pub routine: Option<RoutineMeta>,
    /// Agent-created routines (issue #98). An additive key: meta `v` stays 1,
    /// an older build ignores the key, and a value of the wrong shape reads
    /// as empty without losing the rest of the meta.
    pub routines: RoutineSet,
    /// A dialog was answered, so a following idle is a cancel.
    pub answered: bool,
}

impl PaneMeta {
    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let labels: Vec<Value> = self.labels.iter().map(|(k, v)| json!([k, v])).collect();
        let routine = self.routine.as_ref().map(|r| {
            json!({
                "consecutive": r.consecutive,
                "gave_up": r.gave_up,
                "nudge_allowance": r.nudge_allowance,
            })
        });
        let mut blob = json!({
            "v": META_VERSION,
            "name": self.name,
            "labels": labels,
            "argv": self.argv,
            "cwd": self.cwd,
            "agent": self.agent,
            "profile": self.profile,
            "tracker": self.tracker.as_ref().map(TrackerSnapshot::to_json),
            "tracker_seq": self.tracker_seq,
            "human_draft": self.human_draft,
            "routine": routine,
            "answered": self.answered,
        });
        if !self.routines.is_blank() {
            blob["routines"] = self.routines.to_json();
        }
        blob.to_string().into_bytes()
    }

    /// Why the blob is not usable, as a log line: not JSON, another version,
    /// or a field of the wrong shape. A pane with such meta is adopted
    /// without a restore, never refused.
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        Self::from_bytes_with_notes(bytes).map(|(meta, _)| meta)
    }

    /// [`PaneMeta::from_bytes`], with the log lines for what was dropped from
    /// the lenient `routines` key.
    pub(crate) fn from_bytes_with_notes(bytes: &[u8]) -> Result<(Self, Vec<String>), String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|err| format!("meta is not JSON: {err}"))?;
        match value.get("v").and_then(Value::as_u64) {
            Some(META_VERSION) => {}
            Some(other) => return Err(format!("meta has version {other}, not {META_VERSION}")),
            None => return Err("meta has no version".to_string()),
        }
        let mut notes = Vec::new();
        let meta = Self::from_value(&value, &mut notes)
            .ok_or_else(|| "meta has a field of the wrong shape".to_string())?;
        Ok((meta, notes))
    }

    fn from_value(value: &Value, notes: &mut Vec<String>) -> Option<Self> {
        let text = |key: &str| match value.get(key)? {
            Value::Null => Some(None),
            other => Some(Some(other.as_str()?.to_string())),
        };
        let labels = value
            .get("labels")?
            .as_array()?
            .iter()
            .map(|pair| {
                let pair = pair.as_array()?;
                Some((
                    pair.first()?.as_str()?.to_string(),
                    pair.get(1)?.as_str()?.to_string(),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let argv = value
            .get("argv")?
            .as_array()?
            .iter()
            .map(|arg| arg.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?;
        let tracker = match value.get("tracker")? {
            Value::Null => None,
            snapshot => Some(TrackerSnapshot::from_json(snapshot)?),
        };
        let routine = match value.get("routine")? {
            Value::Null => None,
            r => Some(RoutineMeta {
                consecutive: u32::try_from(r.get("consecutive")?.as_u64()?).ok()?,
                gave_up: r.get("gave_up")?.as_bool()?,
                nudge_allowance: r.get("nudge_allowance")?.as_u64()?,
            }),
        };
        // Read on its own, never with `?`: a bad `routines` value must not
        // cost the pane its tracker or its human-draft flag.
        let routines = match value.get("routines") {
            None | Some(Value::Null) => RoutineSet::default(),
            Some(stored) => {
                let (set, lost) = RoutineSet::from_json_lenient(stored);
                notes.extend(lost);
                set
            }
        };
        Some(Self {
            name: text("name")?,
            labels,
            argv,
            cwd: value.get("cwd")?.as_str()?.to_string(),
            agent: text("agent")?,
            profile: text("profile")?,
            tracker,
            tracker_seq: value.get("tracker_seq")?.as_u64()?,
            human_draft: value.get("human_draft")?.as_bool()?,
            routine,
            routines,
            answered: value.get("answered")?.as_bool()?,
        })
    }
}

/// The only writer of one pane's host `meta`.
pub(crate) struct MetaStore {
    host: Arc<dyn PtyHost>,
    id: String,
    meta: Mutex<PaneMeta>,
}

#[cfg_attr(not(test), allow(dead_code))] // the front wires the rest in with adoption
impl MetaStore {
    pub(crate) fn new(host: Arc<dyn PtyHost>, id: String, meta: PaneMeta) -> Self {
        Self {
            host,
            id,
            meta: Mutex::new(meta),
        }
    }

    /// Change the meta and write the whole blob to the host, in one critical
    /// section. The change stays in memory when the write fails, so the next
    /// write carries it.
    pub(crate) fn update<R>(&self, change: impl FnOnce(&mut PaneMeta) -> R) -> io::Result<R> {
        let mut meta = self.meta.lock().unwrap_or_else(PoisonError::into_inner);
        let out = change(&mut meta);
        self.host.meta_set(&self.id, &meta.to_bytes())?;
        Ok(out)
    }

    /// Replace the pane's routines and write the whole blob. All or nothing:
    /// a blob over [`ROUTINES_BLOB_CAP`] is refused, and a failed host write
    /// leaves the routines in memory as they were, so the caller can report
    /// "not saved" and change nothing of its own.
    pub(crate) fn set_routines(&self, routines: &RoutineSet) -> io::Result<()> {
        let mut meta = self.meta.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = meta.clone();
        next.routines = routines.clone();
        let bytes = next.to_bytes();
        if bytes.len() > ROUTINES_BLOB_CAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the pane metadata would be {} bytes with these routines; the limit is {ROUTINES_BLOB_CAP}",
                    bytes.len()
                ),
            ));
        }
        self.host.meta_set(&self.id, &bytes)?;
        *meta = next;
        Ok(())
    }

    /// The meta as last set.
    pub(crate) fn get(&self) -> PaneMeta {
        self.meta
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Record that the tracker is `snapshot` after the output up to `seq`.
    pub(crate) fn set_tracker(&self, snapshot: TrackerSnapshot, seq: u64) -> io::Result<()> {
        self.update(|m| {
            m.tracker = Some(snapshot);
            m.tracker_seq = seq;
        })
    }

    /// Set the human-draft flag. Returns once the host has it, which is the
    /// caller's cue that the human's byte may go.
    pub(crate) fn set_human_draft(&self) -> io::Result<()> {
        self.update(|m| m.human_draft = true)
    }

    /// Clear the human-draft flag. Returns once the host has it; on failure
    /// the in-memory meta is already clear, so the caller keeps its own flag
    /// set and tries again.
    pub(crate) fn clear_human_draft(&self) -> io::Result<()> {
        self.update(|m| m.human_draft = false)
    }
}

/// Read a pane's meta from the host. None, with a log line, when the host has
/// none, it cannot be read, or it is not usable (see [`PaneMeta::from_bytes`]).
pub(crate) fn load(host: &dyn PtyHost, id: &str) -> Option<PaneMeta> {
    let bytes = match host.meta_get(id) {
        Ok(bytes) if bytes.is_empty() => return None,
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("openab-shepherd: native: {id}: cannot read meta: {err}");
            return None;
        }
    };
    match PaneMeta::from_bytes_with_notes(&bytes) {
        Ok((meta, notes)) => {
            for note in notes {
                eprintln!(
                    "openab-shepherd: native: {id}: meta: {note}; routines kept as far as readable"
                );
            }
            Some(meta)
        }
        Err(why) => {
            eprintln!("openab-shepherd: native: {id}: {why}; adopting without it");
            None
        }
    }
}
