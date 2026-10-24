//! Adds the peers' frames together and keeps the sum inside the range a device can play.

use crate::jitter::FRAME_SAMPLES;

/// Below this magnitude the limiter does not touch the signal.
const KNEE: f32 = 0.8;

/// Sums frames; [`finish`](Mixer::finish) returns the limited result and clears it.
pub(crate) struct Mixer {
    sum: [f32; FRAME_SAMPLES],
}

impl Mixer {
    pub(crate) fn new() -> Mixer {
        Mixer {
            sum: [0.0; FRAME_SAMPLES],
        }
    }

    pub(crate) fn add(&mut self, frame: &[f32; FRAME_SAMPLES], gain: f32) {
        for (s, f) in self.sum.iter_mut().zip(frame) {
            *s += f * gain;
        }
    }

    pub(crate) fn finish(&mut self) -> [f32; FRAME_SAMPLES] {
        let mut out = self.sum;
        for s in &mut out {
            *s = soft_limit(*s);
        }
        self.sum = [0.0; FRAME_SAMPLES];
        out
    }
}

/// Identity up to [`KNEE`], then a smooth bend that approaches but never reaches 1.0, so that
/// seven loud peers distort gently instead of wrapping or clipping hard.
pub(crate) fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let room = 1.0 - KNEE;
        x.signum() * (KNEE + room * ((a - KNEE) / room).tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_signals_pass_unchanged() {
        for x in [-0.8, -0.3, 0.0, 0.01, 0.5, 0.8] {
            assert_eq!(soft_limit(x), x);
        }
    }

    #[test]
    fn never_exceeds_full_scale_however_many_peers_shout() {
        for x in [0.9, 1.0, 2.0, 7.0, 100.0, f32::MAX] {
            let y = soft_limit(x);
            assert!((0.8..=1.0).contains(&y), "{x} -> {y}");
            assert_eq!(soft_limit(-x), -y);
        }
    }

    #[test]
    fn is_monotonic_and_continuous_at_the_knee() {
        let mut last = 0.0;
        for i in 0..4000 {
            let y = soft_limit(i as f32 / 1000.0);
            assert!(y >= last);
            assert!(y - last < 0.002, "jump at {}", i);
            last = y;
        }
    }

    #[test]
    fn seven_full_scale_peers_stay_in_range_and_the_mixer_resets() {
        let mut m = Mixer::new();
        let loud = [0.9f32; FRAME_SAMPLES];
        for _ in 0..7 {
            m.add(&loud, 1.0);
        }
        let out = m.finish();
        assert!(out.iter().all(|s| s.abs() <= 1.0 && *s > 0.9));
        assert!(m.finish().iter().all(|s| *s == 0.0));
    }

    #[test]
    fn gain_scales_a_peer() {
        let mut m = Mixer::new();
        m.add(&[0.4; FRAME_SAMPLES], 0.5);
        assert!(m.finish().iter().all(|s| (s - 0.2).abs() < 1e-6));
    }
}
