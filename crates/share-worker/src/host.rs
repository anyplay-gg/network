//! The streamer's side: pictures from a source, through an encoder, cut into datagrams.

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, Frame, VideoEncoder};
use anyplay_share::link::{FromWorker, Start, ToWorker, WorkerStats};
use anyplay_share::packetiser::Packetiser;
use anyplay_share::testcodec::TestPattern;

use crate::Port;
use crate::sound::SoundSource;

/// What a source gives when asked for a picture.
#[derive(Debug)]
pub enum Next {
    Picture(Frame),
    /// No picture yet since the start (a window that has not drawn); the source waited a
    /// little and the caller asks again after taking its messages.
    NotYet,
    /// The source has ended (the window closed).
    Ended,
}

/// Where pictures come from.
pub trait FrameSource {
    /// The next picture. Never blocks for long: before the first picture it answers
    /// [`Next::NotYet`] after a short wait, so that a stop still gets through.
    fn next_frame(&mut self) -> Next;

    /// Waits up to `limit` for a picture: `None` when the source ended or none came in time.
    fn wait_frame(&mut self, limit: Duration) -> Option<Frame> {
        let deadline = Instant::now() + limit;
        loop {
            match self.next_frame() {
                Next::Picture(frame) => return Some(frame),
                Next::Ended => return None,
                Next::NotYet if Instant::now() >= deadline => return None,
                Next::NotYet => {}
            }
        }
    }
}

/// The test pattern as a source: 320 by 180, the frame number in its pixels.
#[derive(Default)]
pub struct PatternSource(pub TestPattern);

impl FrameSource for PatternSource {
    fn next_frame(&mut self) -> Next {
        Next::Picture(self.0.next_frame())
    }
}

/// Makes the encoder for a setup. It may lower the setup to what the encoder can do (a software
/// encoder cannot do 1080p60); the loop then follows the lowered numbers.
pub type EncoderFactory = Box<dyn FnMut(&mut Start) -> Result<Box<dyn VideoEncoder>, String>>;

/// How often the stats leave.
const STATS_EVERY: Duration = Duration::from_millis(500);
/// The fastest the loop produces pictures, whatever the setup asks for.
const MAX_FPS: u32 = 60;

/// What the loop keeps between pictures.
struct State {
    start: Start,
    packetiser: Packetiser,
    parity: u8,
    key_wanted: bool,
    /// A new `Start` came: the encoder is made again.
    restart: bool,
    /// A new rate for the encoder.
    rate: Option<u32>,
}

impl State {
    /// Takes in one message; false when the worker should end.
    fn handle(&mut self, message: ToWorker) -> bool {
        match message {
            ToWorker::Start(new) => {
                self.packetiser.set_max_payload(new.max_payload);
                self.packetiser.set_epoch(new.epoch);
                self.parity = new.parity_percent;
                self.start = new;
                self.key_wanted = true;
                self.restart = true;
            }
            ToWorker::Rate {
                kbps,
                parity_percent,
            } => {
                self.start.kbps = kbps;
                self.parity = parity_percent;
                self.rate = Some(kbps);
            }
            ToWorker::KeyFrame => self.key_wanted = true,
            ToWorker::Datagram(_) => {}
            ToWorker::Stop => return false,
        }
        true
    }
}

/// Runs the streamer until the net service stops it or hangs up. Waits for the first
/// [`ToWorker::Start`]; then, at the setup's frame rate, takes a picture from `source`, encodes
/// it and sends its datagrams.
pub fn run(source: Box<dyn FrameSource>, encoder: Box<dyn VideoEncoder>, port: Port) {
    let mut encoder = Some(encoder);
    run_with(
        source,
        Box::new(move |_| encoder.take().ok_or_else(|| "only one encoder".to_owned())),
        port,
    );
}

/// Like [`run`], with an encoder made from each `Start`: a change of size or codec makes a new
/// one, and the rate of a `Rate` message goes to the running one.
pub fn run_with(source: Box<dyn FrameSource>, make_encoder: EncoderFactory, port: Port) {
    run_with_sound(source, make_encoder, None, port);
}

/// Like [`run_with`], and the game's sound from `sound` once the stream starts, on a thread of
/// its own, until the stream ends. Without a source the stream has no sound.
pub fn run_with_sound(
    mut source: Box<dyn FrameSource>,
    mut make_encoder: EncoderFactory,
    sound: Option<Box<dyn SoundSource>>,
    port: Port,
) {
    let Port {
        from_service,
        to_service,
    } = port;
    let start = loop {
        match from_service.recv() {
            Ok(ToWorker::Start(start)) => break start,
            Ok(ToWorker::Stop) | Err(_) => return,
            Ok(_) => {}
        }
    };
    // Stops with this function.
    let _sound = sound.and_then(|sound| {
        crate::sound::capture(sound, start.epoch, to_service.clone())
            .inspect_err(|why| eprintln!("share worker: no sound: {why}"))
            .ok()
    });
    let mut state = State {
        packetiser: Packetiser::new(start.max_payload, start.epoch),
        parity: start.parity_percent,
        start,
        key_wanted: true,
        restart: true,
        rate: None,
    };
    let mut encoder: Option<Box<dyn VideoEncoder>> = None;
    let mut frames: u64 = 0;
    let mut window_bytes: u64 = 0;
    let mut window_frames: u32 = 0;
    let mut window_start = Instant::now();
    let mut stats = WorkerStats::default();
    let mut next = Instant::now();
    loop {
        // Wait for the next picture's time, taking messages as they come.
        loop {
            let wait = next.saturating_duration_since(Instant::now());
            match from_service.recv_timeout(wait) {
                Ok(message) => {
                    if !state.handle(message) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if state.restart {
            state.restart = false;
            state.rate = None;
            // The old encoder goes first: two hardware encoders at once is more than some
            // drivers give.
            drop(encoder.take());
            match make_encoder(&mut state.start) {
                Ok(made) => encoder = Some(made),
                Err(reason) => {
                    let _ = to_service.send(FromWorker::Ended(reason));
                    return;
                }
            }
            state.packetiser.set_max_payload(state.start.max_payload);
        }
        let Some(encoder) = encoder.as_mut() else {
            return;
        };
        if let Some(kbps) = state.rate.take() {
            encoder.set_bitrate(kbps);
        }
        let period = Duration::from_secs_f64(1.0 / f64::from(state.start.fps.clamp(1, MAX_FPS)));
        next += period;
        let now = Instant::now();
        if now > next + period * 4 {
            // Far behind: do not rush a burst of pictures to catch up.
            next = now;
        }
        let frame = match source.next_frame() {
            Next::Picture(frame) => frame,
            // Back to the messages: a stop gets through while the window has not drawn.
            Next::NotYet => continue,
            Next::Ended => {
                let _ = to_service.send(FromWorker::Ended("the picture source ended".into()));
                return;
            }
        };
        let encoded = match encoder.encode(&frame, state.key_wanted) {
            Ok(encoded) => encoded,
            // The encoder keeps the picture and answers later; a key frame asked for now is
            // still wanted.
            Err(CodecError::Pending) => continue,
            Err(CodecError::BadFrame) => continue,
            Err(error) => {
                let _ = to_service.send(FromWorker::Ended(format!("the encoder failed: {error}")));
                return;
            }
        };
        if encoded.key {
            state.key_wanted = false;
        }
        let datagrams = match state.packetiser.packetise(&encoded, state.parity) {
            Ok(datagrams) => datagrams,
            Err(_) => {
                // A frame too large for the setup: the next one is a key frame.
                state.key_wanted = true;
                continue;
            }
        };
        for datagram in datagrams {
            window_bytes += datagram.len() as u64;
            if to_service.send(FromWorker::Datagram(datagram)).is_err() {
                return;
            }
        }
        frames += 1;
        window_frames += 1;
        stats.width = frame.width;
        stats.height = frame.height;
        if window_start.elapsed() >= STATS_EVERY {
            let seconds = window_start.elapsed().as_secs_f64();
            stats.fps = (f64::from(window_frames) / seconds).round() as u32;
            stats.kbps = (window_bytes as f64 * 8.0 / 1000.0 / seconds).round() as u32;
            stats.frames = frames;
            window_start = Instant::now();
            window_bytes = 0;
            window_frames = 0;
            if to_service.send(FromWorker::Stats(stats)).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use anyplay_share::testcodec::TestEncoder;

    use super::*;

    /// A window that never draws.
    struct NeverDraws;

    impl FrameSource for NeverDraws {
        fn next_frame(&mut self) -> Next {
            std::thread::sleep(Duration::from_millis(20));
            Next::NotYet
        }
    }

    #[test]
    fn a_stop_gets_through_while_the_window_has_not_drawn() {
        let (to_worker, from_service) = mpsc::channel();
        let (to_service, _from_worker) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        std::thread::spawn(move || {
            let port = Port {
                from_service,
                to_service,
            };
            run(Box::new(NeverDraws), Box::new(TestEncoder::new(60)), port);
            let _ = done_tx.send(());
        });
        let start = Start {
            epoch: 0,
            codec: "test".into(),
            width: 320,
            height: 180,
            fps: 30,
            kbps: 1000,
            parity_percent: 0,
            max_payload: 1200,
        };
        to_worker.send(ToWorker::Start(start)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        to_worker.send(ToWorker::Stop).unwrap();
        done.recv_timeout(Duration::from_secs(2))
            .expect("the stop got through");
        // Held until here: a hang-up would end the loop too, and is not what is tested.
        drop(to_worker);
    }
}
