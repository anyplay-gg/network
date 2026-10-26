//! The game's sound: captured on the streamer's PC, sent as Opus in the audio datagrams of
//! `spec/share-play.md`, played by the viewer's worker. Seams: [`SoundSource`] (the game's
//! processes on Windows, `loopback`; a tone in tests) and [`SoundOut`] (the speakers on
//! Windows, `CpalOut`; a clock in tests).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyplay_share::audio::{AudioJitter, AudioPacketiser, CHANNELS, FRAME_SAMPLES, Playout, RATE};
use anyplay_share::link::FromWorker;
use opus::{Application, Bitrate, Channels, Decoder, Encoder};

/// Samples (both channels) in one 10 ms packet.
pub const PACKET_SAMPLES: usize = FRAME_SAMPLES * CHANNELS;
/// The bit rate of the game's sound (`spec/share-play.md`, audio datagram).
const BITRATE: i32 = 96_000;
/// The loss the encoder plans its redundancy for, in percent.
const EXPECTED_LOSS: i32 = 10;
/// Longer than this without sound from the source, and the next packet says the stream was
/// interrupted.
const GAP: Duration = Duration::from_millis(200);
/// What the viewer keeps queued at the speakers, in samples per channel: 20 ms.
const QUEUED: usize = 2 * FRAME_SAMPLES;

/// Where the game's sound comes from: 48 kHz stereo, interleaved, 32-bit float.
pub trait SoundSource: Send {
    /// Appends what has been captured since the last call; waits at most about 20 ms for it.
    /// `Ok(false)` when the source has ended for good.
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String>;
}

/// A 440 Hz tone at the pace of a real device, for tests and the test pattern.
pub struct ToneSource {
    started: Instant,
    produced: u64,
}

impl Default for ToneSource {
    fn default() -> Self {
        ToneSource {
            started: Instant::now(),
            produced: 0,
        }
    }
}

impl SoundSource for ToneSource {
    fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        std::thread::sleep(Duration::from_millis(10));
        let due = self.started.elapsed().as_micros() as u64 * u64::from(RATE) / 1_000_000;
        while self.produced < due {
            let t = self.produced as f32 / RATE as f32;
            let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.25;
            out.extend([s, s]);
            self.produced += 1;
        }
        Ok(true)
    }
}

/// The streamer's end: 10 ms of sound at a time into Opus and into datagrams.
pub struct SoundSender {
    encoder: Encoder,
    packetiser: AudioPacketiser,
    pending: Vec<f32>,
    out: Vec<u8>,
}

impl SoundSender {
    pub fn new(epoch: u8) -> Result<SoundSender, String> {
        let mut encoder = Encoder::new(RATE, Channels::Stereo, Application::Audio)
            .map_err(|e| format!("the sound encoder: {e}"))?;
        let set = |r: opus::Result<()>| r.map_err(|e| format!("the sound encoder: {e}"));
        set(encoder.set_bitrate(Bitrate::Bits(BITRATE)))?;
        set(encoder.set_vbr(true))?;
        set(encoder.set_vbr_constraint(true))?;
        set(encoder.set_inband_fec(true))?;
        set(encoder.set_packet_loss_perc(EXPECTED_LOSS))?;
        Ok(SoundSender {
            encoder,
            packetiser: AudioPacketiser::new(epoch),
            pending: Vec::new(),
            out: vec![0; anyplay_share::wire::MAX_AUDIO_PAYLOAD],
        })
    }

    /// The source stopped giving sound for a while.
    pub fn interrupt(&mut self) {
        self.pending.clear();
        self.packetiser.interrupt();
    }

    /// Takes captured sound; answers the datagrams of every full 10 ms in it.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>, String> {
        self.pending.extend_from_slice(samples);
        let mut datagrams = Vec::new();
        let mut used = 0;
        while self.pending.len() - used >= PACKET_SAMPLES {
            let packet = &self.pending[used..used + PACKET_SAMPLES];
            used += PACKET_SAMPLES;
            let len = self
                .encoder
                .encode_float(packet, &mut self.out)
                .map_err(|e| format!("the sound encoder: {e}"))?;
            let datagram = self
                .packetiser
                .packet(&self.out[..len])
                .map_err(|e| format!("the sound packet: {e}"))?;
            datagrams.push(datagram);
        }
        self.pending.drain(..used);
        Ok(datagrams)
    }
}

/// The viewer's end: datagrams through the jitter buffer and the decoder, 10 ms at a time.
pub struct SoundReceiver {
    jitter: AudioJitter,
    decoder: Decoder,
}

impl SoundReceiver {
    pub fn new() -> Result<SoundReceiver, String> {
        Ok(SoundReceiver {
            jitter: AudioJitter::new(),
            decoder: Decoder::new(RATE, Channels::Stereo)
                .map_err(|e| format!("the sound decoder: {e}"))?,
        })
    }

    pub fn insert(&mut self, datagram: &[u8]) {
        // A datagram that does not parse is a lost packet.
        let _ = self.jitter.insert(datagram);
    }

    pub fn stats(&self) -> anyplay_share::audio::AudioStats {
        self.jitter.stats()
    }

    /// The next 10 ms: decoded, rebuilt, concealed or silent.
    pub fn next_packet(&mut self) -> Vec<f32> {
        let mut out = vec![0.0; PACKET_SAMPLES];
        let decoded = match self.jitter.pop() {
            Playout::Silence => return out,
            Playout::Packet(payload) => self.decoder.decode_float(&payload, &mut out, false),
            Playout::Recover(next) => self.decoder.decode_float(&next, &mut out, true),
            Playout::Conceal => self.decoder.decode_float(&[], &mut out, false),
        };
        match decoded {
            Ok(samples) if samples == FRAME_SAMPLES => out,
            // A packet the decoder refuses, or one of another length, plays as silence.
            _ => vec![0.0; PACKET_SAMPLES],
        }
    }
}

/// Where the viewer's sound goes: the speakers, or a clock in tests.
pub trait SoundOut {
    /// Samples per channel waiting to be played.
    fn queued(&self) -> usize;
    /// Queues 48 kHz stereo, interleaved.
    fn play(&mut self, samples: &[f32]);
}

/// Plays nothing, at the pace of a real device: what it was given is gone after its duration.
pub struct ClockOut {
    started: Instant,
    given: u64,
    /// Everything played, for tests.
    pub played: Arc<Mutex<Vec<f32>>>,
}

impl Default for ClockOut {
    fn default() -> Self {
        ClockOut {
            started: Instant::now(),
            given: 0,
            played: Arc::default(),
        }
    }
}

impl SoundOut for ClockOut {
    fn queued(&self) -> usize {
        let gone = self.started.elapsed().as_micros() as u64 * u64::from(RATE) / 1_000_000;
        self.given.saturating_sub(gone) as usize
    }

    fn play(&mut self, samples: &[f32]) {
        let gone = self.started.elapsed().as_micros() as u64 * u64::from(RATE) / 1_000_000;
        // After an underrun the clock starts from now.
        self.given = self.given.max(gone) + (samples.len() / CHANNELS) as u64;
        if let Ok(mut played) = self.played.lock() {
            played.extend_from_slice(samples);
        }
    }
}

/// Keeps `out` fed from `receiver`: about 20 ms ahead of what the speakers play.
pub fn feed(receiver: &mut SoundReceiver, out: &mut dyn SoundOut) {
    while out.queued() < QUEUED {
        let packet = receiver.next_packet();
        out.play(&packet);
    }
}

/// Linear interpolation from 48 kHz to the device's rate, one stereo frame at a time.
pub struct Resampler {
    step: f64,
    phase: f64,
    last: [f32; 2],
    next: [f32; 2],
}

impl Resampler {
    pub fn new(device_rate: u32) -> Resampler {
        Resampler {
            step: f64::from(RATE) / f64::from(device_rate),
            // The first call takes the first frame.
            phase: 1.0,
            last: [0.0; 2],
            next: [0.0; 2],
        }
    }

    /// The next device frame; takes 48 kHz frames from `ring` as it goes, silence when it is
    /// empty.
    pub fn frame(&mut self, ring: &mut VecDeque<f32>) -> [f32; 2] {
        while self.phase >= 1.0 {
            self.last = self.next;
            self.next = match (ring.pop_front(), ring.pop_front()) {
                (Some(l), Some(r)) => [l, r],
                _ => [0.0; 2],
            };
            self.phase -= 1.0;
        }
        let t = self.phase as f32;
        self.phase += self.step;
        [
            self.last[0] + (self.next[0] - self.last[0]) * t,
            self.last[1] + (self.next[1] - self.last[1]) * t,
        ]
    }
}

/// Stops the capture thread when dropped.
pub struct Capturing {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Capturing {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Captures from `source` on a thread of its own and sends the datagrams to the net service
/// until the returned guard is dropped, the source ends or the link closes. A source that
/// fails ends the sound, not the stream: the picture goes on.
pub fn capture(
    mut source: Box<dyn SoundSource>,
    epoch: u8,
    to_service: Sender<FromWorker>,
) -> Result<Capturing, String> {
    let mut sender = SoundSender::new(epoch)?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let thread = std::thread::Builder::new()
        .name("share-sound".into())
        .spawn(move || {
            let mut samples = Vec::new();
            let mut last_sound = Instant::now();
            while !stopped.load(Ordering::SeqCst) {
                samples.clear();
                match source.read(&mut samples) {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(why) => {
                        eprintln!("share worker: the game's sound stopped: {why}");
                        return;
                    }
                }
                if samples.is_empty() {
                    if last_sound.elapsed() > GAP {
                        sender.interrupt();
                    }
                    continue;
                }
                last_sound = Instant::now();
                let datagrams = match sender.push(&samples) {
                    Ok(datagrams) => datagrams,
                    Err(why) => {
                        eprintln!("share worker: the game's sound stopped: {why}");
                        return;
                    }
                };
                for datagram in datagrams {
                    if to_service.send(FromWorker::Datagram(datagram)).is_err() {
                        return;
                    }
                }
            }
        })
        .map_err(|e| format!("the sound thread: {e}"))?;
    Ok(Capturing {
        stop,
        thread: Some(thread),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Peak level of a stretch of samples.
    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0, |m, s| m.max(s.abs()))
    }

    fn tone(n: usize) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let s = (i as f32 / RATE as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.25;
                [s, s]
            })
            .collect()
    }

    #[test]
    fn a_tone_goes_through_opus_and_comes_out_as_a_tone() {
        let mut sender = SoundSender::new(0).unwrap();
        let mut receiver = SoundReceiver::new().unwrap();
        // 200 ms, sent in odd pieces as a device gives them.
        let input = tone(9600);
        let mut datagrams = Vec::new();
        for piece in input.chunks(734) {
            datagrams.extend(sender.push(piece).unwrap());
        }
        assert_eq!(datagrams.len(), 20, "one datagram per 10 ms");
        for datagram in &datagrams {
            assert!(datagram.len() <= anyplay_share::wire::AUDIO_HEADER_LEN + 1275);
            receiver.insert(datagram);
        }
        let output: Vec<f32> = (0..20).flat_map(|_| receiver.next_packet()).collect();
        // The decoder needs a few milliseconds to settle; after that the tone is there.
        let settled = &output[PACKET_SAMPLES * 4..];
        assert!(
            peak(settled) > 0.15 && peak(settled) < 0.35,
            "{}",
            peak(settled)
        );
    }

    #[test]
    fn a_lost_packet_is_filled_in_without_a_gap() {
        let mut sender = SoundSender::new(0).unwrap();
        let mut receiver = SoundReceiver::new().unwrap();
        let datagrams = sender.push(&tone(9600)).unwrap();
        // Packets arrive as they are played; packet 10 never comes. Playing starts after 40 ms,
        // so output n is packet n.
        let mut output = Vec::new();
        for (n, datagram) in datagrams.iter().enumerate() {
            if n != 10 {
                receiver.insert(datagram);
            }
            if n >= 3 {
                output.push(receiver.next_packet());
            }
        }
        while output.len() < 20 {
            output.push(receiver.next_packet());
        }
        assert!(peak(&output[10]) > 0.1, "the lost 10 ms are silent");
        assert_eq!(receiver.stats().lost, 1);
    }

    #[test]
    fn the_resampler_keeps_48_khz_and_doubles_for_96() {
        let input: Vec<f32> = (0..8).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let mut ring: VecDeque<f32> = input.iter().copied().collect();
        let mut same = Resampler::new(48_000);
        let out: Vec<[f32; 2]> = (0..8).map(|_| same.frame(&mut ring)).collect();
        // One frame later, the same frames.
        assert_eq!(
            out[1..],
            (0..7).map(|i| [i as f32, -(i as f32)]).collect::<Vec<_>>()[..]
        );
        let mut ring: VecDeque<f32> = input.iter().copied().collect();
        let mut double = Resampler::new(96_000);
        let out: Vec<f32> = (0..10).map(|_| double.frame(&mut ring)[0]).collect();
        assert_eq!(out[2..], [0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5]);
        // Nothing left: silence, not a stuck sample.
        let mut empty = VecDeque::new();
        let mut resampler = Resampler::new(48_000);
        assert_eq!(resampler.frame(&mut empty), [0.0, 0.0]);
    }

    #[test]
    fn the_viewer_keeps_20_ms_queued_and_plays_silence_before_the_sound() {
        let mut receiver = SoundReceiver::new().unwrap();
        let mut out = ClockOut::default();
        feed(&mut receiver, &mut out);
        assert!(out.queued() >= QUEUED);
        assert_eq!(peak(&out.played.lock().unwrap()), 0.0);
    }

    #[test]
    fn the_capture_thread_sends_datagrams_until_dropped() {
        let (tx, rx) = std::sync::mpsc::channel();
        let capturing = capture(Box::new(ToneSource::default()), 7, tx).unwrap();
        let mut count = 0;
        let deadline = Instant::now() + Duration::from_secs(2);
        while count < 10 && Instant::now() < deadline {
            if let Ok(FromWorker::Datagram(bytes)) = rx.recv_timeout(Duration::from_millis(100)) {
                let datagram = anyplay_share::wire::AudioDatagram::parse(&bytes).unwrap();
                assert_eq!(datagram.epoch, 7);
                count += 1;
            }
        }
        assert_eq!(count, 10);
        drop(capturing);
        while rx.try_recv().is_ok() {}
        std::thread::sleep(Duration::from_millis(50));
        assert!(rx.try_recv().is_err(), "nothing after the stop");
    }
}
