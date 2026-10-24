//! Decides, frame by frame, whether the microphone is open: voice activity detection for the
//! open-microphone mode. Push-to-talk needs no decision here; the player makes it.

/// How voice activity detection tells talking from background.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VadConfig {
    /// The least speech probability (0 to 1) the noise suppressor must report. Ignored when noise
    /// suppression is off, which leaves the level alone to decide.
    pub min_probability: f32,
    /// The least level of the cleaned frame, in dB below full scale.
    pub min_level_db: f32,
    /// How long the gate stays open after the last frame that sounded like speech.
    pub hangover_ms: u32,
}

impl Default for VadConfig {
    fn default() -> VadConfig {
        VadConfig {
            min_probability: 0.6,
            min_level_db: -50.0,
            hangover_ms: 300,
        }
    }
}

pub(crate) struct Vad {
    config: VadConfig,
    hang: u32,
}

impl Vad {
    pub(crate) fn new(config: VadConfig) -> Vad {
        Vad { config, hang: 0 }
    }

    /// Feed the next 20 ms; says whether the gate is open for it.
    pub(crate) fn update(&mut self, probability: f32, level_db: f32) -> bool {
        let speech =
            probability >= self.config.min_probability && level_db >= self.config.min_level_db;
        if speech {
            self.hang = self.config.hangover_ms.div_ceil(20);
            true
        } else if self.hang > 0 {
            self.hang -= 1;
            true
        } else {
            false
        }
    }
}

/// Root mean square of a frame.
pub(crate) fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt()
}

pub(crate) fn to_db(linear: f32) -> f32 {
    20.0 * linear.max(1e-9).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_stays_closed() {
        let mut vad = Vad::new(VadConfig::default());
        for _ in 0..100 {
            assert!(!vad.update(0.0, -90.0));
        }
    }

    #[test]
    fn speech_opens_and_the_hangover_holds_it() {
        let mut vad = Vad::new(VadConfig::default());
        assert!(vad.update(0.95, -25.0));
        // 300 ms is 15 frames of hangover, then it closes.
        for i in 0..15 {
            assert!(vad.update(0.0, -90.0), "frame {i}");
        }
        assert!(!vad.update(0.0, -90.0));
    }

    #[test]
    fn loud_noise_that_is_not_speech_does_not_open_it() {
        let mut vad = Vad::new(VadConfig::default());
        for _ in 0..50 {
            assert!(!vad.update(0.1, -20.0));
        }
    }

    #[test]
    fn quiet_speech_below_the_level_floor_does_not_open_it() {
        let mut vad = Vad::new(VadConfig::default());
        assert!(!vad.update(0.99, -70.0));
    }

    #[test]
    fn level_helpers() {
        assert!((rms(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
        assert!((to_db(0.1) + 20.0).abs() < 1e-4);
        assert!(to_db(0.0) < -150.0);
    }
}
