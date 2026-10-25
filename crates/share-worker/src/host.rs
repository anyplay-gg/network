//! The streamer's side: pictures from a source, through an encoder, cut into datagrams.

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyplay_share::codec::{Frame, VideoEncoder};
use anyplay_share::link::{FromWorker, Start, ToWorker, WorkerStats};
use anyplay_share::packetiser::Packetiser;
use anyplay_share::testcodec::TestPattern;

use crate::Port;

/// Where pictures come from.
pub trait FrameSource {
    /// The next picture, or `None` when the source has ended (the window closed).
    fn next_frame(&mut self) -> Option<Frame>;
}

/// The test pattern as a source: 320 by 180, the frame number in its pixels.
#[derive(Default)]
pub struct PatternSource(pub TestPattern);

impl FrameSource for PatternSource {
    fn next_frame(&mut self) -> Option<Frame> {
        Some(self.0.next_frame())
    }
}

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
            }
            ToWorker::Rate {
                kbps,
                parity_percent,
            } => {
                self.start.kbps = kbps;
                self.parity = parity_percent;
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
pub fn run(mut source: Box<dyn FrameSource>, mut encoder: Box<dyn VideoEncoder>, port: Port) {
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
    let mut state = State {
        packetiser: Packetiser::new(start.max_payload, start.epoch),
        parity: start.parity_percent,
        start,
        key_wanted: true,
    };
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
        let period = Duration::from_secs_f64(1.0 / f64::from(state.start.fps.clamp(1, MAX_FPS)));
        next += period;
        let now = Instant::now();
        if now > next + period * 4 {
            // Far behind: do not rush a burst of pictures to catch up.
            next = now;
        }
        let Some(frame) = source.next_frame() else {
            let _ = to_service.send(FromWorker::Ended("the picture source ended".into()));
            return;
        };
        let encoded = match encoder.encode(&frame, state.key_wanted) {
            Ok(encoded) => encoded,
            Err(error) => {
                let _ = to_service.send(FromWorker::Ended(format!("the encoder failed: {error}")));
                return;
            }
        };
        state.key_wanted = false;
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
