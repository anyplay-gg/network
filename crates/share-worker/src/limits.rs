//! How much a stream asks of an encoder: the setup, and the most a kind of encoder is given
//! (the spec's rule: software at most 720p30 so that the game keeps its processor, hardware
//! 1080p60). The same on every system.

/// What the encoder is asked to produce.
#[derive(Clone, Copy, Debug)]
pub struct EncoderSetup {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
}

/// The most an encoder is asked for.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Limits {
    /// A hardware encoder: 1080p60.
    pub const HARDWARE: Limits = Limits {
        width: 1920,
        height: 1080,
        fps: 60,
    };
    /// The system's software encoder: 720p30, so that the game keeps its processor.
    pub const SOFTWARE: Limits = Limits {
        width: 1280,
        height: 720,
        fps: 30,
    };

    /// The setup of a stream start lowered to these limits, keeping the shape and even sizes.
    pub fn clamp(self, start: &anyplay_share::link::Start) -> EncoderSetup {
        let (mut w, mut h) = (start.width.max(2), start.height.max(2));
        if w > self.width || h > self.height {
            let scale =
                (f64::from(self.width) / f64::from(w)).min(f64::from(self.height) / f64::from(h));
            w = (f64::from(w) * scale) as u32;
            h = (f64::from(h) * scale) as u32;
        }
        EncoderSetup {
            width: (w & !1).max(2),
            height: (h & !1).max(2),
            fps: start.fps.clamp(1, self.fps),
            kbps: start.kbps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyplay_share::link::Start;

    fn start(width: u32, height: u32, fps: u32) -> Start {
        Start {
            epoch: 0,
            codec: "h264".into(),
            width,
            height,
            fps,
            kbps: 6000,
            parity_percent: 0,
            max_payload: 0,
        }
    }

    #[test]
    fn software_lowers_1080p60_to_720p30_keeping_the_shape() {
        let setup = Limits::SOFTWARE.clamp(&start(1920, 1080, 60));
        assert_eq!((setup.width, setup.height, setup.fps), (1280, 720, 30));
    }

    #[test]
    fn sizes_stay_even_and_a_small_setup_is_kept() {
        let setup = Limits::HARDWARE.clamp(&start(1001, 563, 24));
        assert_eq!((setup.width, setup.height, setup.fps), (1000, 562, 24));
    }
}
