//! A streaming windowed-sinc resampler for mono audio. Voice only needs it between the device's
//! rate and 48 kHz, so a short Blackman-windowed kernel is enough, and it keeps a heavier crate
//! out of the build.

use std::f64::consts::PI;

/// Kernel half width in input samples at unity ratio.
const HALF_WIDTH: f64 = 12.0;
/// Table entries per input sample; the kernel is interpolated linearly between them.
const PHASES: usize = 256;

pub(crate) struct Resampler {
    /// Input samples per output sample.
    step: f64,
    half: usize,
    kernel: Vec<f32>,
    /// Input not yet consumed. `buf[0]` is the input sample at absolute index `base`.
    buf: Vec<f32>,
    base: i64,
    /// Absolute input position of the next output sample.
    pos: f64,
    bypass: bool,
}

impl Resampler {
    pub(crate) fn new(from: u32, to: u32) -> Resampler {
        let bypass = from == to;
        let step = f64::from(from) / f64::from(to);
        // Below the lower Nyquist frequency, with a little room for the transition band.
        let cutoff = (1.0 / step).min(1.0) * 0.96;
        let half = (HALF_WIDTH / cutoff).ceil() as usize;
        let n = half * PHASES;
        let kernel = (0..=2 * n + 1)
            .map(|i| {
                let t = (i as f64 - n as f64) / PHASES as f64;
                let x = PI * cutoff * t;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { x.sin() / x };
                // 0 to 2 pi across the kernel.
                let w = (t / half as f64 + 1.0) * PI;
                let window = 0.42 - 0.5 * w.cos() + 0.08 * (2.0 * w).cos();
                (cutoff * sinc * window) as f32
            })
            .collect();
        Resampler {
            step,
            half,
            kernel,
            buf: vec![0.0; half],
            base: -(half as i64),
            pos: 0.0,
            bypass,
        }
    }

    /// Append the resampled `input` to `out`.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.bypass {
            out.extend_from_slice(input);
            return;
        }
        self.buf.extend_from_slice(input);
        let end = self.base + self.buf.len() as i64;
        let half = self.half as i64;
        let n = (self.half * PHASES) as f64;
        while self.pos.floor() as i64 + half < end {
            let centre = self.pos.floor() as i64;
            let mut acc = 0.0f32;
            for i in (centre - half + 1)..=(centre + half) {
                let t = (i as f64 - self.pos) * PHASES as f64 + n;
                let k = t.floor();
                let frac = (t - k) as f32;
                let k = k as usize;
                let w = self.kernel[k] + (self.kernel[k + 1] - self.kernel[k]) * frac;
                acc += w * self.buf[(i - self.base) as usize];
            }
            out.push(acc);
            self.pos += self.step;
        }
        let keep_from = self.pos.floor() as i64 - half;
        let drop = (keep_from - self.base).max(0) as usize;
        if drop > 0 {
            self.buf.drain(..drop);
            self.base += drop as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Error between the output and the ideal sine, after the filter's start-up.
    fn error_db(from: u32, to: u32, hz: f32) -> f32 {
        let mut r = Resampler::new(from, to);
        let mut out = Vec::new();
        let input = sine(from, hz, from as usize / 2);
        for chunk in input.chunks(333) {
            r.process(chunk, &mut out);
        }
        // The filter delays by a fixed amount; take the best of a few lags around it.
        let ideal = sine(to, hz, out.len() + 100);
        let skip = 200;
        let mut best = f32::MAX;
        for lag in -40i32..=40 {
            let mut num = 0.0;
            let mut den = 0.0;
            for (i, o) in out.iter().enumerate().take(out.len() - skip).skip(skip) {
                let j = (i as i32 + lag) as usize;
                let d = o - ideal[j];
                num += d * d;
                den += ideal[j] * ideal[j];
            }
            best = best.min(num / den);
        }
        10.0 * best.log10()
    }

    #[test]
    fn keeps_a_voice_tone_between_common_rates() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (16_000, 48_000),
            (96_000, 48_000),
        ] {
            let e = error_db(from, to, 1000.0);
            assert!(e < -35.0, "{from} -> {to}: error {e:.1} dB");
        }
    }

    #[test]
    fn removes_what_the_lower_rate_cannot_hold() {
        // 30 kHz at 96 kHz would fold into the audible band at 48 kHz without the low-pass.
        let mut r = Resampler::new(96_000, 48_000);
        let mut out = Vec::new();
        r.process(&sine(96_000, 30_000.0, 19_200), &mut out);
        assert!(rms(&out[200..]) < 0.005, "rms {}", rms(&out[200..]));
    }

    #[test]
    fn output_count_follows_the_ratio() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.process(&[0.0; 441], &mut out);
        }
        // 44100 samples in, 48000 out.
        assert!((out.len() as i64 - 48_000).abs() < 40, "{}", out.len());
        assert!(out.len() > 40_000);
    }

    #[test]
    fn same_rate_is_a_copy() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        r.process(&[1.0, 2.0, 3.0], &mut out);
        assert_eq!(out, [1.0, 2.0, 3.0]);
    }
}
