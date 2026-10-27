//! Delay figures for the launcher: the median and the 95th percentile of the last 10 seconds
//! (`spec/share-play.md`, Latency), and the choice of the clock offset that the figures are
//! corrected with. Pure code: time comes in as microseconds from the caller's own clock.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// How far back the figures look.
pub const WINDOW_US: u64 = 10_000_000;
/// The most samples a window keeps: 240 a second would still fit.
const MAX_SAMPLES: usize = 2_400;
/// A frame older than this is not a delay but a wrong clock; it is not counted.
pub const IMPLAUSIBLE_US: u32 = 10_000_000;
/// How far back [`ClockPicker`] looks for the exchange with the smallest round trip.
pub const CLOCK_WINDOW_US: u64 = 60_000_000;

/// Median and 95th percentile of a delay, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delay {
    pub p50: u32,
    pub p95: u32,
}

/// The delays of the last [`WINDOW_US`].
#[derive(Clone, Debug, Default)]
pub struct DelayWindow {
    samples: VecDeque<(u64, u32)>,
}

impl DelayWindow {
    pub fn new() -> DelayWindow {
        DelayWindow::default()
    }

    /// One delay, in microseconds, measured at `now_us`. A delay that cannot be real is
    /// dropped.
    pub fn push(&mut self, now_us: u64, delay_us: u32) {
        if delay_us > IMPLAUSIBLE_US {
            return;
        }
        self.samples.push_back((now_us, delay_us));
        if self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    /// The figures at `now_us`; none while the window holds no sample.
    pub fn figures(&mut self, now_us: u64) -> Option<Delay> {
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| at.saturating_add(WINDOW_US) < now_us)
        {
            self.samples.pop_front();
        }
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<u32> = self.samples.iter().map(|(_, d)| *d).collect();
        sorted.sort_unstable();
        let n = sorted.len();
        let at = |percent: usize| sorted[(n * percent).div_ceil(100).clamp(1, n) - 1];
        let ms = |us: u32| us.saturating_add(500) / 1_000;
        Some(Delay {
            p50: ms(at(50)),
            p95: ms(at(95)),
        })
    }
}

/// Keeps the clock offsets that `ping` and `pong` measured and picks the one of the exchange
/// with the smallest round trip in the last minute: the least asymmetric path is the most
/// likely to be the one that is right (`spec/share-play.md`, How it is measured).
#[derive(Clone, Debug, Default)]
pub struct ClockPicker {
    samples: VecDeque<(u64, i64, i64)>,
}

impl ClockPicker {
    pub fn new() -> ClockPicker {
        ClockPicker::default()
    }

    /// One exchange: its round trip and the other clock's offset, both in microseconds.
    pub fn push(&mut self, now_us: u64, rtt_us: i64, offset_us: i64) {
        if rtt_us < 0 {
            return;
        }
        self.samples.push_back((now_us, rtt_us, offset_us));
    }

    /// The offset (the other clock minus ours) of the exchange with the smallest round trip,
    /// as of `now_us`.
    pub fn best(&mut self, now_us: u64) -> Option<i64> {
        while self
            .samples
            .front()
            .is_some_and(|(at, _, _)| at.saturating_add(CLOCK_WINDOW_US) < now_us)
        {
            self.samples.pop_front();
        }
        self.samples
            .iter()
            .min_by_key(|(_, rtt, _)| *rtt)
            .map(|(_, _, offset)| *offset)
    }
}

/// The age of a frame stamped `stamp` (the host's clock in microseconds, wrapping at 2^32) when
/// the viewer's clock reads `now_us` and the host's clock reads `offset_us` more than the
/// viewer's. Microseconds; a frame that seems to come from the future has age 0.
pub fn frame_age_us(now_us: u64, offset_us: i64, stamp: u32) -> u32 {
    let host_now = (now_us as i64).wrapping_add(offset_us) as u32;
    let age = host_now.wrapping_sub(stamp) as i32;
    age.max(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_sample_no_figures() {
        assert_eq!(DelayWindow::new().figures(5), None);
    }

    #[test]
    fn median_and_p95_in_milliseconds() {
        let mut w = DelayWindow::new();
        // 1 ms to 100 ms, one of each
        for ms in 1..=100u32 {
            w.push(1_000_000, ms * 1_000);
        }
        assert_eq!(w.figures(1_000_000), Some(Delay { p50: 50, p95: 95 }));
    }

    #[test]
    fn one_sample_is_both_figures() {
        let mut w = DelayWindow::new();
        w.push(0, 42_400);
        assert_eq!(w.figures(0), Some(Delay { p50: 42, p95: 42 }));
    }

    #[test]
    fn old_samples_leave_after_ten_seconds() {
        let mut w = DelayWindow::new();
        w.push(0, 90_000);
        w.push(9_000_000, 10_000);
        assert_eq!(w.figures(10_000_000).unwrap().p95, 90);
        assert_eq!(w.figures(10_000_001), Some(Delay { p50: 10, p95: 10 }));
        assert_eq!(w.figures(19_000_001), None);
    }

    #[test]
    fn a_wrong_clock_is_not_a_delay() {
        let mut w = DelayWindow::new();
        w.push(0, IMPLAUSIBLE_US + 1);
        assert_eq!(w.figures(0), None);
    }

    #[test]
    fn the_window_is_bounded() {
        let mut w = DelayWindow::new();
        for i in 0..10_000u32 {
            w.push(u64::from(i), 1_000);
        }
        assert!(w.samples.len() <= MAX_SAMPLES);
    }

    #[test]
    fn the_smallest_round_trip_gives_the_offset() {
        let mut c = ClockPicker::new();
        assert_eq!(c.best(0), None);
        c.push(1_000_000, 80_000, 500);
        c.push(2_000_000, 20_000, 1_000);
        c.push(3_000_000, 50_000, 700);
        assert_eq!(c.best(3_000_000), Some(1_000));
        // the best one is a minute old: the next best takes over
        assert_eq!(c.best(62_000_001), Some(700));
        assert_eq!(c.best(64_000_000), None);
    }

    #[test]
    fn a_negative_round_trip_is_ignored() {
        let mut c = ClockPicker::new();
        c.push(0, -5, 99);
        assert_eq!(c.best(0), None);
    }

    #[test]
    fn age_of_a_frame_across_two_clocks_and_the_wrap() {
        // the host's clock is 7 s ahead of the viewer's; the frame is 30 ms old
        let now = 100_000_000u64;
        let offset = 7_000_000i64;
        let stamp = ((now as i64 + offset) as u32).wrapping_sub(30_000);
        assert_eq!(frame_age_us(now, offset, stamp), 30_000);
        // across the wrap of the 32-bit stamp
        let now = u64::from(u32::MAX) - 5_000;
        let stamp = (now as u32).wrapping_sub(12_000);
        assert_eq!(frame_age_us(now, 0, stamp), 12_000);
        assert_eq!(frame_age_us(now + 100_000, 0, stamp), 112_000);
        // a stamp in the future is age zero
        assert_eq!(frame_age_us(1_000, 0, 5_000), 0);
    }
}
