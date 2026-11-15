//! Silent replay (issue #76): rebuild a pane's screen and tracker from the
//! tail a host kept, for a front that starts after the pane did.
//!
//! The bytes go through the same emulator as live output, but nothing the
//! emulator answers is kept: DA, DSR, XTVERSION, kitty and colour queries in
//! the tail were answered by the previous front long ago, and answering them
//! again would type the replies into whatever the program is reading now.
//! Replay also makes no status events, log entries or pending sends; the
//! only things it produces are the emulator, the tracker and two flags.

use super::meta::PaneMeta;
use super::status::{Report, Tracker};
use super::term::Emulator;
use crate::backend::State;

/// What replay made of a pane.
pub(super) struct Adopted {
    pub emu: Emulator,
    pub tracker: Tracker,
    /// The tracker has no trustworthy state: the tail was wrapped and no
    /// snapshot said where the tracker was. Treated as working until a fresh
    /// report or the exit.
    pub unknown: bool,
    /// The tail was wrapped: the screen is rebuilt from a line start inside
    /// the output, so modes set earlier may be missing. Writes are refused
    /// until the program repaints.
    pub replayed: bool,
    /// Things worth a log line, in the order they were found.
    pub notes: Vec<String>,
}

/// Rebuild from `tail`, whose first byte is at output offset `tail_start`.
/// `meta` is the pane's metadata as the previous front last wrote it.
///
/// * Complete tail (`tail_start == 0`): everything is replayed through the
///   tracker from nothing, and the metadata's turn is not used.
/// * Wrapped tail: replay starts at the first line start, so no escape
///   sequence is cut in half. With a snapshot the tracker is restored to it
///   and only the bytes after the snapshot count, so a turn finished after
///   the snapshot counts once and one already counted is not counted again.
///   Without one the pane is `unknown`.
pub(super) fn replay(
    tail: &[u8],
    tail_start: u64,
    meta: Option<&PaneMeta>,
    now_ms: u64,
) -> Adopted {
    let tail_end = tail_start + tail.len() as u64;
    let mut emu = Emulator::new();
    let mut tracker = Tracker::default();
    let mut notes = Vec::new();
    let slice =
        |from: u64, to: u64| &tail[(from - tail_start) as usize..(to - tail_start) as usize];

    if tail_start == 0 {
        feed_counted(&mut emu, &mut tracker, tail, now_ms);
        if let Some((snapshot, seq)) = meta.and_then(snapshot_of) {
            if seq == tail_end && snapshot.turn() != tracker.turn() {
                notes.push(format!(
                    "replayed turn {} differs from the metadata's {}; using the replay",
                    tracker.turn(),
                    snapshot.turn()
                ));
            }
        }
        return Adopted {
            emu,
            tracker,
            unknown: false,
            replayed: false,
            notes,
        };
    }

    let line_start = tail
        .iter()
        .position(|&b| b == b'\n')
        .map_or(tail_end, |at| tail_start + at as u64 + 1);
    let usable = meta
        .and_then(snapshot_of)
        .filter(|(_, seq)| *seq <= tail_end);
    let Some((snapshot, seq)) = usable else {
        feed_silent(&mut emu, slice(line_start, tail_end));
        notes.push("the tail is wrapped and no tracker snapshot is usable; status unknown".into());
        return Adopted {
            emu,
            tracker,
            unknown: true,
            replayed: true,
            notes,
        };
    };
    tracker = Tracker::restore(snapshot);
    if seq < line_start {
        // Reports between the snapshot and the first line start are gone:
        // the turn stays monotonic and may undercount.
        notes.push(format!(
            "the snapshot at {seq} is before the replay start {line_start}; \
             turns in between may be missing"
        ));
    }
    let split = seq.clamp(line_start, tail_end);
    // Up to the snapshot the tracker has seen these bytes already; they only
    // rebuild the screen. After it, a report counts.
    feed_silent(&mut emu, slice(line_start, split));
    feed_counted(&mut emu, &mut tracker, slice(split, tail_end), now_ms);
    Adopted {
        emu,
        tracker,
        unknown: false,
        replayed: true,
        notes,
    }
}

fn snapshot_of(meta: &PaneMeta) -> Option<(super::status::TrackerSnapshot, u64)> {
    meta.tracker.clone().map(|t| (t, meta.tracker_seq))
}

/// Feed bytes and drop everything the emulator says about them.
fn feed_silent(emu: &mut Emulator, bytes: &[u8]) {
    emu.process(bytes);
    let _ = emu.take_reports();
    let _ = emu.take_replies();
}

/// Feed bytes, drop the replies, and let the tracker count the reports.
fn feed_counted(emu: &mut Emulator, tracker: &mut Tracker, bytes: &[u8], now_ms: u64) {
    emu.process(bytes);
    let _ = emu.take_replies();
    for (_, report) in emu.take_reports() {
        if let Report::Status { state_word, kind } = report {
            tracker.apply(State::parse(&state_word), kind, now_ms);
        }
    }
}
