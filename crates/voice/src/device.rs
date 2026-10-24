//! The seam between the engine and the sound hardware.
//!
//! The engine never opens a device. It is given an [`AudioIo`] and moves mono `f32` samples through
//! it. The real one (cpal, feature `devices`) is [`CpalDevice`](crate::CpalDevice); the one the
//! tests use is [`MemoryDevice`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

/// Capture and playback of one mono stream each, at rates the device chooses.
///
/// Nothing here may block: the engine calls it from [`Engine::pump`](crate::Engine::pump), which
/// runs on whatever task or thread the net service gives it.
pub trait AudioIo: Send {
    /// Samples per second of what [`read_capture`](AudioIo::read_capture) returns.
    fn capture_rate(&self) -> u32;
    /// Samples per second [`write_playback`](AudioIo::write_playback) expects.
    fn playback_rate(&self) -> u32;
    /// Append everything captured since the last call to `out`.
    fn read_capture(&mut self, out: &mut Vec<f32>);
    /// How many samples written are still waiting to be played.
    fn playback_queued(&self) -> usize;
    /// Queue samples for playing.
    fn write_playback(&mut self, samples: &[f32]);
}

#[derive(Default)]
struct Shared {
    capture: VecDeque<f32>,
    playback: VecDeque<f32>,
    underrun_samples: u64,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// A device that lives in memory: the test feeds the microphone and takes what the speakers would
/// have played. Time passes only when the test says so.
pub struct MemoryDevice {
    shared: Arc<Mutex<Shared>>,
    capture_rate: u32,
    playback_rate: u32,
}

/// The test's end of a [`MemoryDevice`].
#[derive(Clone)]
pub struct MemoryHandle {
    shared: Arc<Mutex<Shared>>,
}

impl MemoryDevice {
    /// A device and the handle to drive it.
    pub fn new(capture_rate: u32, playback_rate: u32) -> (MemoryDevice, MemoryHandle) {
        let shared = Arc::new(Mutex::new(Shared::default()));
        (
            MemoryDevice {
                shared: shared.clone(),
                capture_rate,
                playback_rate,
            },
            MemoryHandle { shared },
        )
    }
}

impl MemoryHandle {
    /// Samples the microphone heard.
    pub fn feed_capture(&self, samples: &[f32]) {
        lock(&self.shared).capture.extend(samples);
    }

    /// Let the speakers play `samples` samples: returns what they played. If the engine had not
    /// queued enough, the rest is silence and counts as an underrun.
    pub fn play(&self, samples: usize) -> Vec<f32> {
        let mut s = lock(&self.shared);
        let n = samples.min(s.playback.len());
        let mut out: Vec<f32> = s.playback.drain(..n).collect();
        s.underrun_samples += (samples - n) as u64;
        out.resize(samples, 0.0);
        out
    }

    /// Samples the speakers lacked so far.
    pub fn underrun_samples(&self) -> u64 {
        lock(&self.shared).underrun_samples
    }
}

impl AudioIo for MemoryDevice {
    fn capture_rate(&self) -> u32 {
        self.capture_rate
    }

    fn playback_rate(&self) -> u32 {
        self.playback_rate
    }

    fn read_capture(&mut self, out: &mut Vec<f32>) {
        out.extend(lock(&self.shared).capture.drain(..));
    }

    fn playback_queued(&self) -> usize {
        lock(&self.shared).playback.len()
    }

    fn write_playback(&mut self, samples: &[f32]) {
        lock(&self.shared).playback.extend(samples);
    }
}
