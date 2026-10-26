//! The broadcast's sound track: the game's sound (a [`SoundSource`]) through an [`AudioEncoder`]
//! into AAC messages, or silence where there is no sound to send.
//!
//! The track follows the wall clock the pictures follow. Captured sound arrives in bursts and
//! stops entirely while the game is quiet (process loopback delivers nothing then), so the
//! [`Timeline`] keeps the number of samples given to the encoder in step with the clock: it
//! fills a quiet stretch with silence, drops what piled up when the capture ran ahead, and never
//! lets the track drift away from the picture. A message's time is its frame's place on that
//! timeline (1024 samples at 48 kHz, 21.3 ms).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::flv;
use super::rtmp::Publisher;
use crate::sound::SoundSource;

/// Samples per second, and channels, of what the encoder is given.
pub const RATE: u64 = flv::AAC_RATE;
pub const CHANNELS: usize = 2;
/// A quiet stretch at least this long is filled with silence at once; a shorter lag is the
/// capture being late and is waited out.
const FILL_AFTER: Duration = Duration::from_millis(60);
/// Sound that piled up beyond this is dropped (the oldest first), so the track cannot lag.
const MOST_AHEAD: Duration = Duration::from_millis(100);
/// How long the loop waits for the source when it has nothing to do.
const TICK: Duration = Duration::from_millis(10);

/// Turns 48 kHz stereo (interleaved, 32-bit float) into raw AAC-LC frames of 1024 samples.
pub trait AudioEncoder {
    /// Takes any amount of sound; answers the frames that are complete now (none while the
    /// encoder still collects its first).
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>, String>;
}

/// What a broadcast needs for real sound: made on the sound thread, because the system's encoder
/// must live on the thread that uses it.
pub type SoundFactory =
    Box<dyn FnOnce() -> Result<(Box<dyn SoundSource>, Box<dyn AudioEncoder>), String> + Send>;

fn frames_in(duration: Duration) -> u64 {
    duration.as_micros() as u64 * RATE / 1_000_000
}

/// Keeps the samples handed to the encoder in step with the clock.
pub struct Timeline {
    started: Instant,
    /// Frames (one sample per channel) handed out so far.
    given: u64,
    queued: Vec<f32>,
}

impl Timeline {
    pub fn new(started: Instant) -> Self {
        Timeline {
            started,
            given: 0,
            queued: Vec::new(),
        }
    }

    /// Captured sound.
    pub fn feed(&mut self, samples: &[f32]) {
        self.queued.extend_from_slice(samples);
    }

    /// The sound due at `now`: what has been fed, with silence where the capture is late by
    /// more than [`FILL_AFTER`], and never more than the clock allows.
    pub fn take(&mut self, now: Instant) -> Vec<f32> {
        let due = frames_in(now.saturating_duration_since(self.started));
        let behind = due.saturating_sub(self.given);
        let queued = (self.queued.len() / CHANNELS) as u64;
        let out = if queued >= behind {
            // On time or early. Keep the extra for the next tick, up to a limit.
            let take = behind as usize * CHANNELS;
            let out: Vec<f32> = self.queued.drain(..take).collect();
            let cap = frames_in(MOST_AHEAD) as usize * CHANNELS;
            if self.queued.len() > cap {
                let excess = self.queued.len() - cap;
                self.queued.drain(..excess);
            }
            out
        } else if behind - queued >= frames_in(FILL_AFTER) {
            // Quiet for a while: silence up to the sound that is here now.
            let mut out = vec![0.0; (behind - queued) as usize * CHANNELS];
            out.append(&mut self.queued);
            out
        } else {
            // A little late: pass on what there is, the next tick catches up.
            std::mem::take(&mut self.queued)
        };
        self.given += (out.len() / CHANNELS) as u64;
        out
    }
}

/// The time of the AAC frame with this index, in milliseconds.
pub fn frame_time_ms(index: u64) -> u32 {
    (index * flv::AAC_FRAME_SAMPLES * 1000 / RATE) as u32
}

/// Sends the sound track until `stop` is set or the connection breaks; with no `sound`, or when
/// it fails, the track is silence. `clock` is the time the pictures' times count from.
pub fn run(
    sound: Option<SoundFactory>,
    publisher: &Publisher,
    clock: Instant,
    stop: &AtomicBool,
    failure: &Mutex<Option<String>>,
) {
    let fail = |why: String| {
        if let Ok(mut slot) = failure.lock() {
            slot.get_or_insert(why);
        }
    };
    // A real source and its encoder, until one of them fails.
    let mut real = sound.and_then(|make| match make() {
        Ok(pair) => Some(pair),
        Err(why) => {
            eprintln!("broadcast: no game sound, the stream carries silence: {why}");
            None
        }
    });
    if let Some((source, _)) = real.as_mut() {
        // What the source held from before the broadcast started is not sent.
        let mut old = Vec::new();
        let _ = source.read(&mut old);
    }
    let mut timeline = Timeline::new(clock);
    let mut sent: u64 = 0;
    let mut captured = Vec::new();
    let mut ended = false;
    while !stop.load(Ordering::SeqCst) {
        let mut frames: Vec<Vec<u8>> = Vec::new();
        if let Some((source, encoder)) = real.as_mut() {
            captured.clear();
            match source.read(&mut captured) {
                Ok(true) => timeline.feed(&captured),
                Ok(false) => ended = true,
                Err(why) => {
                    eprintln!("broadcast: the game's sound stopped: {why}");
                    ended = true;
                }
            }
            if !ended {
                let samples = timeline.take(Instant::now());
                match encoder.encode(&samples) {
                    Ok(done) => frames = done,
                    Err(why) => {
                        eprintln!("broadcast: the sound encoder stopped: {why}");
                        ended = true;
                    }
                }
            }
            if ended {
                real = None;
            }
        } else {
            std::thread::sleep(TICK);
            // Silence, as many frames as the clock has made room for.
            let due = clock.elapsed().as_millis() as u64;
            let mut index = sent;
            while u64::from(frame_time_ms(index)) <= due {
                frames.push(flv::AAC_SILENCE.to_vec());
                index += 1;
            }
        }
        for frame in frames {
            if let Err(why) = publisher.audio(frame_time_ms(sent), &flv::audio_tag(&frame)) {
                return fail(why);
            }
            sent += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn frames(samples: &[f32]) -> usize {
        samples.len() / CHANNELS
    }

    #[test]
    fn sound_that_arrives_on_time_passes_through_unchanged() {
        let start = Instant::now();
        let mut line = Timeline::new(start);
        // 10 ms of a ramp, fed and asked for at 10 ms.
        let sound: Vec<f32> = (0..480 * 2).map(|n| n as f32).collect();
        line.feed(&sound);
        assert_eq!(line.take(start + ms(10)), sound);
        assert!(line.take(start + ms(10)).is_empty());
    }

    #[test]
    fn a_short_lag_is_waited_out_and_a_long_one_filled_with_silence() {
        let start = Instant::now();
        let mut line = Timeline::new(start);
        // 30 ms late: nothing is invented.
        assert!(line.take(start + ms(30)).is_empty());
        // At 100 ms the lag is over the limit: silence up to the sound that is here.
        line.feed(&vec![0.5; 480 * 2]);
        let out = line.take(start + ms(100));
        assert_eq!(frames(&out), 4800, "100 ms in all");
        assert_eq!(out[0], 0.0);
        assert_eq!(*out.last().unwrap(), 0.5, "the real sound ends the stretch");
    }

    #[test]
    fn sound_ahead_of_the_clock_waits_and_a_pile_up_is_cut() {
        let start = Instant::now();
        let mut line = Timeline::new(start);
        // A whole second at once: only the first 20 ms are due, and no more than 100 ms
        // (the newest) are kept for later.
        let second: Vec<f32> = (0..48_000 * 2).map(|n| (n / 2) as f32).collect();
        line.feed(&second);
        let first = line.take(start + ms(20));
        assert_eq!(frames(&first), 960);
        assert_eq!(first[0], 0.0);
        let next = line.take(start + ms(30));
        assert_eq!(frames(&next), 480);
        // The newest sound survived the cut: the first kept frame is near the end.
        assert!(next[0] > 40_000.0, "{}", next[0]);
    }

    #[test]
    fn the_total_given_never_runs_ahead_of_the_clock() {
        let start = Instant::now();
        let mut line = Timeline::new(start);
        let mut total = 0;
        for tick in 1..=200u64 {
            // The source delivers 15 ms every 10 ms: faster than real time.
            line.feed(&vec![0.1; 720 * 2]);
            total += frames(&line.take(start + ms(tick * 10)));
            assert!(total as u64 <= tick * 480, "tick {tick}: {total}");
        }
        assert!(total as u64 >= 200 * 480 - 480, "{total}");
    }

    #[test]
    fn frame_times_follow_the_frame_length() {
        assert_eq!(frame_time_ms(0), 0);
        assert_eq!(frame_time_ms(1), 21);
        assert_eq!(frame_time_ms(47), 1002);
        assert_eq!(frame_time_ms(1000), 21_333);
    }
}
