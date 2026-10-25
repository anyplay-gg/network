//! The ring of encoded pictures a clip is cut from: whole groups of pictures (a key frame and
//! the delta frames after it), newest last, bounded by time and by bytes.

use std::collections::VecDeque;
use std::sync::Arc;

/// One encoded picture, Annex B, as the encoder wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipFrame {
    /// Decodes without an earlier picture; carries the SPS and PPS.
    pub key: bool,
    /// When the picture was taken, in microseconds on the recorder's own clock.
    pub pts_us: u64,
    /// Shared, so that a save copies pointers and not the megabytes while the recorder goes on.
    pub data: Arc<[u8]>,
}

#[derive(Debug)]
struct Gop {
    frames: Vec<ClipFrame>,
    bytes: usize,
}

/// The newest pictures, as many whole groups as cover the window.
///
/// The oldest group is dropped as soon as the groups after it still cover the window, so a clip
/// is never shorter than the window and at most one group (the key frame interval) longer. A
/// byte limit drops older groups too, down to the newest one, so that a very busy picture cannot
/// grow the ring without end.
#[derive(Debug)]
pub struct Ring {
    gops: VecDeque<Gop>,
    window_us: u64,
    max_bytes: usize,
    bytes: usize,
}

impl Ring {
    pub fn new(window_us: u64, max_bytes: usize) -> Ring {
        Ring {
            gops: VecDeque::new(),
            window_us,
            max_bytes,
            bytes: 0,
        }
    }

    /// Adds the newest picture. A delta frame before the first key frame cannot be decoded and
    /// is dropped.
    pub fn push(&mut self, frame: ClipFrame) {
        if frame.key {
            self.gops.push_back(Gop {
                frames: Vec::new(),
                bytes: 0,
            });
        }
        let Some(gop) = self.gops.back_mut() else {
            return;
        };
        gop.bytes += frame.data.len();
        self.bytes += frame.data.len();
        let newest = frame.pts_us;
        gop.frames.push(frame);
        self.evict(newest);
    }

    fn evict(&mut self, newest: u64) {
        while self.gops.len() > 1 {
            let second_start = self.gops[1].frames.first().map_or(newest, |f| f.pts_us);
            let covered = newest.saturating_sub(second_start) >= self.window_us;
            if !covered && self.bytes <= self.max_bytes {
                break;
            }
            if let Some(old) = self.gops.pop_front() {
                self.bytes -= old.bytes;
            }
        }
    }

    /// Everything the ring holds, oldest first; empty before the first key frame.
    pub fn snapshot(&self) -> Vec<ClipFrame> {
        self.gops
            .iter()
            .flat_map(|g| g.frames.iter().cloned())
            .collect()
    }

    /// How much time the ring covers, from its first picture to its last.
    pub fn span_us(&self) -> u64 {
        let first = self.gops.front().and_then(|g| g.frames.first());
        let last = self.gops.back().and_then(|g| g.frames.last());
        match (first, last) {
            (Some(a), Some(b)) => b.pts_us.saturating_sub(a.pts_us),
            _ => 0,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.gops.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_US: u64 = 33_333;

    fn frame(n: u64, key: bool, size: usize) -> ClipFrame {
        ClipFrame {
            key,
            pts_us: n * FRAME_US,
            data: vec![0u8; size].into(),
        }
    }

    /// 30 pictures a second, a key frame every 60.
    fn fill(ring: &mut Ring, frames: u64, size: usize) {
        for n in 0..frames {
            ring.push(frame(n, n % 60 == 0, size));
        }
    }

    #[test]
    fn a_delta_frame_before_any_key_frame_is_dropped() {
        let mut ring = Ring::new(10_000_000, usize::MAX);
        ring.push(frame(0, false, 10));
        assert!(ring.is_empty());
        ring.push(frame(1, true, 10));
        ring.push(frame(2, false, 10));
        assert_eq!(ring.snapshot().len(), 2);
    }

    #[test]
    fn the_ring_covers_at_least_the_window_and_at_most_a_group_more() {
        let mut ring = Ring::new(15_000_000, usize::MAX);
        fill(&mut ring, 30 * 120, 100);
        let span = ring.span_us();
        assert!(span >= 15_000_000, "{span}");
        assert!(span <= 15_000_000 + 2_000_000 + FRAME_US, "{span}");
        assert!(ring.snapshot()[0].key, "a clip starts on a key frame");
    }

    #[test]
    fn a_short_recording_keeps_everything() {
        let mut ring = Ring::new(60_000_000, usize::MAX);
        fill(&mut ring, 30 * 5, 100);
        assert_eq!(ring.snapshot().len(), 150);
    }

    #[test]
    fn the_byte_limit_drops_old_groups_but_keeps_the_newest() {
        let mut ring = Ring::new(120_000_000, 50_000);
        fill(&mut ring, 30 * 60, 1_000);
        assert!(ring.bytes() <= 50_000 + 60 * 1_000, "{}", ring.bytes());
        assert!(!ring.is_empty());
        assert!(ring.snapshot()[0].key);
        // Even a limit smaller than one group keeps the group being written.
        let mut tiny = Ring::new(120_000_000, 10);
        fill(&mut tiny, 130, 1_000);
        assert!(!tiny.snapshot().is_empty());
    }

    #[test]
    fn the_byte_count_follows_what_is_held() {
        let mut ring = Ring::new(5_000_000, usize::MAX);
        fill(&mut ring, 30 * 40, 77);
        let held: usize = ring.snapshot().iter().map(|f| f.data.len()).sum();
        assert_eq!(ring.bytes(), held);
    }
}
