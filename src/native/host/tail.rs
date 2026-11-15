//! The tail ring: the last bytes of a pane's output, kept by the host so a
//! front that starts later can rebuild the screen (issue #76).
//!
//! Offsets are cumulative byte counts from the pane's first output byte, the
//! same numbering as the `seq` of [`super::HostEvents::output`]. `start` is
//! the offset of the oldest byte kept and `end` the offset after the newest,
//! so `end - start` is exactly the number of bytes held and `start == 0`
//! means nothing was ever dropped.

use std::collections::VecDeque;

/// The size of a pane's tail in a host that keeps one.
#[cfg_attr(not(test), allow(dead_code))] // the holder enables the ring with it later
pub(crate) const TAIL_CAP: usize = 1024 * 1024;

pub(crate) struct TailRing {
    cap: usize,
    bytes: VecDeque<u8>,
    /// Offset after the newest byte.
    end: u64,
}

impl TailRing {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            bytes: VecDeque::new(),
            end: 0,
        }
    }

    /// Add a chunk that follows the last one. The oldest bytes beyond `cap`
    /// are dropped, also from the chunk itself when it alone exceeds `cap`.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        self.end += chunk.len() as u64;
        let keep = &chunk[chunk.len().saturating_sub(self.cap)..];
        let excess = (self.bytes.len() + keep.len()).saturating_sub(self.cap);
        self.bytes.drain(..excess.min(self.bytes.len()));
        self.bytes.extend(keep);
    }

    /// Offset of the oldest byte held.
    pub(crate) fn start(&self) -> u64 {
        self.end - self.bytes.len() as u64
    }

    pub(crate) fn end(&self) -> u64 {
        self.end
    }

    /// Up to `max` bytes from offset `from`: empty when `from` is at or past
    /// the end, `Err(start)` when `from` was dropped and the ring now starts
    /// at `start`.
    pub(crate) fn read_chunk(&self, from: u64, max: usize) -> Result<Vec<u8>, u64> {
        let start = self.start();
        if from < start {
            return Err(start);
        }
        if from >= self.end {
            return Ok(Vec::new());
        }
        let skip = (from - start) as usize;
        let n = (self.bytes.len() - skip).min(max);
        let (head, tail) = self.bytes.as_slices();
        let mut out = Vec::with_capacity(n);
        if skip < head.len() {
            let from_head = (head.len() - skip).min(n);
            out.extend_from_slice(&head[skip..skip + from_head]);
            out.extend_from_slice(&tail[..n - from_head]);
        } else {
            out.extend_from_slice(&tail[skip - head.len()..skip - head.len() + n]);
        }
        Ok(out)
    }

    /// The bytes from offset `from` to the end. None when `from` is before
    /// the oldest byte held (it was dropped) or after the end.
    pub(crate) fn read_from(&self, from: u64) -> Option<Vec<u8>> {
        if from < self.start() || from > self.end {
            return None;
        }
        let skip = (from - self.start()) as usize;
        let (head, tail) = self.bytes.as_slices();
        let mut out = Vec::with_capacity(self.bytes.len() - skip);
        if skip < head.len() {
            out.extend_from_slice(&head[skip..]);
            out.extend_from_slice(tail);
        } else {
            out.extend_from_slice(&tail[skip - head.len()..]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes whose value tells their offset, so a wrong slice shows.
    fn run(from: u64, to: u64) -> Vec<u8> {
        (from..to).map(|i| (i % 251) as u8).collect()
    }

    fn fill(ring: &mut TailRing, total: u64, chunk: u64) {
        let mut at = ring.end();
        while at < total {
            let next = (at + chunk).min(total);
            ring.push(&run(at, next));
            at = next;
        }
    }

    fn check(ring: &TailRing, total: u64, cap: u64) {
        assert_eq!(ring.end(), total);
        assert_eq!(ring.start(), total.saturating_sub(cap));
        assert_eq!(ring.end() - ring.start(), total.min(cap));
        assert_eq!(
            ring.read_from(ring.start()).unwrap(),
            run(ring.start(), total)
        );
    }

    #[test]
    fn an_empty_ring_starts_and_ends_at_zero() {
        let ring = TailRing::new(8);
        assert_eq!((ring.start(), ring.end()), (0, 0));
        assert_eq!(ring.read_from(0), Some(Vec::new()));
        assert_eq!(ring.read_from(1), None);
    }

    #[test]
    fn below_the_cap_nothing_is_dropped() {
        let mut ring = TailRing::new(16);
        fill(&mut ring, 10, 3);
        check(&ring, 10, 16);
        assert_eq!(ring.start(), 0);
    }

    #[test]
    fn exactly_the_cap_still_starts_at_zero() {
        let mut ring = TailRing::new(16);
        fill(&mut ring, 16, 5);
        check(&ring, 16, 16);
        assert_eq!(ring.start(), 0);
    }

    #[test]
    fn one_byte_past_the_cap_drops_the_first() {
        let mut ring = TailRing::new(16);
        fill(&mut ring, 17, 5);
        check(&ring, 17, 16);
        assert_eq!(ring.start(), 1);
    }

    #[test]
    fn the_ring_wraps_many_times() {
        let mut ring = TailRing::new(16);
        fill(&mut ring, 1000, 7);
        check(&ring, 1000, 16);
    }

    #[test]
    fn a_first_chunk_larger_than_the_cap_keeps_its_last_bytes() {
        let mut ring = TailRing::new(16);
        ring.push(&run(0, 40));
        check(&ring, 40, 16);
        assert_eq!(ring.start(), 24);
        // A first chunk of exactly the cap drops nothing.
        let mut exact = TailRing::new(16);
        exact.push(&run(0, 16));
        assert_eq!(exact.start(), 0);
    }

    #[test]
    fn every_chunking_of_the_same_stream_gives_the_same_ring() {
        for chunk in 1..=40 {
            let mut ring = TailRing::new(16);
            fill(&mut ring, 100, chunk);
            check(&ring, 100, 16);
        }
    }

    #[test]
    fn read_chunk_matches_read_from_at_every_offset_and_size() {
        // Fill so the deque has wrapped and has two slices.
        for total in [10u64, 16, 17, 40, 100] {
            let mut ring = TailRing::new(16);
            fill(&mut ring, total, 5);
            for from in 0..=total + 2 {
                for max in [0usize, 1, 3, 15, 16, 17, 1000] {
                    match ring.read_chunk(from, max) {
                        Err(start) => {
                            assert!(from < ring.start(), "total {total} from {from}");
                            assert_eq!(start, ring.start());
                        }
                        Ok(got) => {
                            let all = ring.read_from(from.min(ring.end())).unwrap();
                            let want = &all[..all.len().min(max)];
                            assert_eq!(got, want, "total {total} from {from} max {max}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn read_from_a_dropped_offset_or_past_the_end_is_none() {
        let mut ring = TailRing::new(16);
        fill(&mut ring, 100, 9);
        assert_eq!(ring.read_from(83), None);
        assert_eq!(ring.read_from(84).unwrap(), run(84, 100));
        assert_eq!(ring.read_from(90).unwrap(), run(90, 100));
        assert_eq!(ring.read_from(100).unwrap(), Vec::<u8>::new());
        assert_eq!(ring.read_from(101), None);
    }

    #[test]
    fn random_chunk_sizes_from_a_fixed_seed_keep_the_ring_exact() {
        // A small linear congruential generator: deterministic, no crate.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = move |bound: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % bound
        };
        for cap in [1u64, 7, 64, 1000] {
            let mut ring = TailRing::new(cap as usize);
            let mut at = 0u64;
            for _ in 0..300 {
                let n = next(2 * cap + 3);
                ring.push(&run(at, at + n));
                at += n;
                check(&ring, at, cap);
                let from = ring.start() + next(ring.end() - ring.start() + 1);
                assert_eq!(ring.read_from(from).unwrap(), run(from, at));
            }
        }
    }
}
