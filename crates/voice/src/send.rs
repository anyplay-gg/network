//! The sending half: microphone samples in, wire frames out.
//!
//! capture rate -> 48 kHz mono -> noise suppression -> gate (push-to-talk or voice activity) ->
//! Opus -> frame.

use nnnoiseless::DenoiseState;
use opus::{Application, Bitrate, Channels, Encoder, Signal};

use crate::gate::{Vad, rms, to_db};
use crate::jitter::FRAME_SAMPLES;
use crate::resample::Resampler;
use crate::wire::{Frame, MAX_PAYLOAD};
use crate::{Config, Error, Level, TalkMode};

/// The noise suppressor works on 10 ms.
const DENOISE_SAMPLES: usize = nnnoiseless::FRAME_SIZE;
/// Opus packets this short carry no speech: the encoder is in DTX and sends only comfort noise.
const DTX_PACKET: usize = 2;
/// While in DTX, a packet goes out every this many frames, 400 ms, so the receiver knows the
/// sender is still there.
const DTX_KEEPALIVE: u32 = 20;

pub(crate) struct Sender {
    resampler: Resampler,
    pending: Vec<f32>,
    denoise: Option<Box<DenoiseState<'static>>>,
    encoder: Encoder,
    vad: Vad,
    mode: TalkMode,
    talking: bool,
    was_open: bool,
    sequence: u16,
    timestamp: u32,
    dtx_run: u32,
    level: Level,
    out: Vec<Vec<u8>>,
}

impl Sender {
    pub(crate) fn new(config: &Config, capture_rate: u32) -> Result<Sender, Error> {
        let mut encoder =
            Encoder::new(48_000, Channels::Mono, Application::Voip).map_err(Error::Codec)?;
        let c = Error::Codec;
        encoder
            .set_bitrate(Bitrate::Bits(config.bitrate as i32))
            .map_err(c)?;
        encoder.set_signal(Signal::Voice).map_err(c)?;
        encoder.set_vbr(true).map_err(c)?;
        encoder.set_vbr_constraint(true).map_err(c)?;
        encoder.set_inband_fec(true).map_err(c)?;
        encoder
            .set_packet_loss_perc(i32::from(config.expected_loss_percent))
            .map_err(c)?;
        encoder.set_dtx(config.dtx).map_err(c)?;
        encoder.set_complexity(5).map_err(c)?;
        Ok(Sender {
            resampler: Resampler::new(capture_rate, 48_000),
            pending: Vec::new(),
            denoise: config.noise_suppression.then(DenoiseState::new),
            encoder,
            vad: Vad::new(config.vad),
            mode: config.mode,
            talking: false,
            was_open: false,
            sequence: 0,
            timestamp: 0,
            dtx_run: 0,
            level: Level::default(),
            out: Vec::new(),
        })
    }

    pub(crate) fn set_mode(&mut self, mode: TalkMode) {
        self.mode = mode;
    }

    pub(crate) fn set_talking(&mut self, talking: bool) {
        self.talking = talking;
    }

    pub(crate) fn level(&self) -> Level {
        self.level
    }

    pub(crate) fn take_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.out)
    }

    /// Feed captured mono samples at the capture rate.
    pub(crate) fn push(&mut self, samples: &[f32]) {
        self.resampler.process(samples, &mut self.pending);
        let mut start = 0;
        while self.pending.len() - start >= FRAME_SAMPLES {
            let mut frame = [0.0f32; FRAME_SAMPLES];
            frame.copy_from_slice(&self.pending[start..start + FRAME_SAMPLES]);
            start += FRAME_SAMPLES;
            self.frame(&mut frame);
        }
        self.pending.drain(..start);
    }

    fn frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) {
        for s in frame.iter_mut() {
            *s = s.clamp(-1.0, 1.0);
        }
        let probability = match &mut self.denoise {
            Some(dn) => {
                let mut probability = 0.0f32;
                let mut input = [0.0f32; DENOISE_SAMPLES];
                let mut output = [0.0f32; DENOISE_SAMPLES];
                for half in frame.as_chunks_mut::<DENOISE_SAMPLES>().0 {
                    // The suppressor works on the scale of 16-bit samples.
                    for (i, s) in input.iter_mut().zip(half.iter()) {
                        *i = s * 32768.0;
                    }
                    probability = probability.max(dn.process_frame(&mut output, &input));
                    for (h, o) in half.iter_mut().zip(&output) {
                        *h = o / 32768.0;
                    }
                }
                probability
            }
            None => 1.0,
        };

        let level = rms(frame);
        let open = match self.mode {
            TalkMode::PushToTalk => self.talking,
            TalkMode::VoiceActivity => self.vad.update(probability, to_db(level)),
        };
        self.level = Level {
            rms: level,
            speaking: open,
        };

        if open {
            self.was_open = true;
            self.send(frame, false);
        } else if self.was_open {
            // The first frame after the gate shut closes the talk.
            self.was_open = false;
            self.send(frame, true);
            self.dtx_run = 0;
            // A clean start for the next talk; the codec's own state would otherwise carry over.
            let _ = self.encoder.reset_state();
        }
        self.timestamp = self.timestamp.wrapping_add(FRAME_SAMPLES as u32);
    }

    fn send(&mut self, frame: &[f32; FRAME_SAMPLES], end_of_talk: bool) {
        let mut buf = [0u8; MAX_PAYLOAD];
        let Ok(n) = self.encoder.encode_float(frame, &mut buf) else {
            return;
        };
        if n == 0 {
            return;
        }
        if n <= DTX_PACKET && !end_of_talk {
            self.dtx_run += 1;
            if self.dtx_run != 1 && !self.dtx_run.is_multiple_of(DTX_KEEPALIVE) {
                return;
            }
        } else {
            self.dtx_run = 0;
        }
        let bytes = Frame {
            sequence: self.sequence,
            timestamp: self.timestamp,
            end_of_talk,
            payload: &buf[..n],
        }
        .encode();
        self.sequence = self.sequence.wrapping_add(1);
        self.out.push(bytes);
    }
}
