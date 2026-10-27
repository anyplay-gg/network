//! Acoustic echo cancellation: the far end's voice, played by the speakers, is taken out of what
//! the microphone hears.
//!
//! [`EchoCanceller`] is WebRTC's AEC3 (the pure-Rust port `sonora`) on mono 48 kHz in blocks of
//! 10 ms. It wants two things: every block the speakers play (the reference, "render"), and every
//! block the microphone captured ("capture"), which comes back with the echo removed.
//!
//! The engine writes the reference itself, so it knows it exactly. What it does not know is when
//! the speakers play it: it queues up to 60 ms ahead of the device. [`Path`] holds the reference
//! back until the device has taken it off the queue, so the delay the canceller has to find is
//! only the device's own buffers and the room, which AEC3's delay estimator follows.

use std::collections::VecDeque;

use sonora::config::EchoCanceller as AecConfig;
use sonora::{AudioProcessing, Config, StreamConfig};

/// Samples in the 10 ms block AEC3 works on, at 48 kHz.
pub const BLOCK: usize = 480;

/// What the device is guessed to add between the engine and the air, in milliseconds. AEC3
/// estimates the delay itself; this only tells it where to start.
const DEVICE_DELAY_HINT_MS: i32 = 40;
/// The most reference kept waiting for a device that has stopped taking samples: one second.
const PENDING_LIMIT: usize = 48_000;
/// Capture blocks (10 ms) without any reference released after which the canceller is bypassed:
/// a device that plays nothing (stopped, or not draining) makes no echo, and AEC3 without a
/// reference only colours the microphone.
const STALLED_BLOCKS: u32 = 50;

/// AEC3 on one mono 48 kHz stream pair.
pub struct EchoCanceller {
    apm: AudioProcessing,
}

impl Default for EchoCanceller {
    fn default() -> EchoCanceller {
        EchoCanceller::new()
    }
}

impl EchoCanceller {
    pub fn new() -> EchoCanceller {
        let config = Config {
            echo_canceller: Some(AecConfig::default()),
            ..Config::default()
        };
        let mut apm = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(48_000, 1))
            .render_config(StreamConfig::new(48_000, 1))
            .build();
        // Out of range is clamped and still processes; nothing to do about it here.
        let _ = apm.set_stream_delay_ms(DEVICE_DELAY_HINT_MS);
        EchoCanceller { apm }
    }

    /// The next 10 ms the speakers play. Anything but [`BLOCK`] samples is ignored.
    pub fn render(&mut self, block: &[f32]) {
        if block.len() != BLOCK {
            return;
        }
        let mut sink = [0.0f32; BLOCK];
        let _ = self.apm.process_render_f32(&[block], &mut [&mut sink[..]]);
    }

    /// The next 10 ms the microphone captured, in place: the echo of what [`render`] was given
    /// is taken out. If the canceller refuses the block it stays as it was.
    ///
    /// [`render`]: EchoCanceller::render
    pub fn capture(&mut self, block: &mut [f32]) {
        if block.len() != BLOCK {
            return;
        }
        let input: [f32; BLOCK] = std::array::from_fn(|i| block[i]);
        let mut output = [0.0f32; BLOCK];
        if self
            .apm
            .process_capture_f32(&[&input[..]], &mut [&mut output[..]])
            .is_ok()
        {
            block.copy_from_slice(&output);
        }
    }
}

/// An [`EchoCanceller`] plus the reference waiting for the speakers to play it.
pub(crate) struct Path {
    canceller: EchoCanceller,
    /// Reference at 48 kHz that was written to the device and not yet released to the canceller.
    pending: VecDeque<f32>,
    /// Capture blocks since the last reference block went to the canceller; `None` before the
    /// first.
    since_reference: Option<u32>,
}

impl Path {
    pub(crate) fn new() -> Path {
        Path {
            canceller: EchoCanceller::new(),
            pending: VecDeque::new(),
            since_reference: None,
        }
    }

    /// Samples just written to the device, at 48 kHz.
    pub(crate) fn reference(&mut self, samples: &[f32]) {
        self.pending.extend(samples);
        if self.pending.len() > PENDING_LIMIT {
            let excess = self.pending.len() - PENDING_LIMIT;
            self.pending.drain(..excess);
        }
    }

    /// The device still has `queued` samples (at 48 kHz) of what was written to it waiting; the
    /// rest has been taken to the speakers, so it goes to the canceller now.
    pub(crate) fn release(&mut self, queued: usize) {
        while self.pending.len() >= queued + BLOCK {
            let block: [f32; BLOCK] =
                std::array::from_fn(|_| self.pending.pop_front().unwrap_or(0.0));
            self.canceller.render(&block);
            self.since_reference = Some(0);
        }
    }

    pub(crate) fn capture(&mut self, block: &mut [f32]) {
        for half in block.as_chunks_mut::<BLOCK>().0 {
            match &mut self.since_reference {
                Some(n) if *n < STALLED_BLOCKS => {
                    *n += 1;
                    self.canceller.capture(half);
                }
                _ => {}
            }
        }
    }
}
