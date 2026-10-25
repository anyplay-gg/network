//! The viewer's side: datagrams reassembled, decoded and handed to a sink.

use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, Frame, VideoDecoder};
use anyplay_share::control::{Key, Report};
use anyplay_share::link::{FromWorker, ToWorker, WorkerStats};
use anyplay_share::reassembler::Reassembler;

use crate::Port;

/// Where pictures go: a window, or a counter in tests.
pub trait FrameSink {
    fn show(&mut self, frame: &Frame);
    /// Called every few milliseconds when there is nothing new to show, so that a window can
    /// answer its own events (move, resize, close). Default: nothing to do.
    fn idle(&mut self) {}
    /// The sink was told to stop (the window should close). Default: nothing to do.
    fn close(&mut self) {}
    /// The viewer closed the window: the worker ends.
    fn closed(&self) -> bool {
        false
    }
}

/// A sink that shows nothing.
pub struct NullSink;

impl FrameSink for NullSink {
    fn show(&mut self, _frame: &Frame) {}
}

/// Makes a decoder for the codec the streamer chose; none when this build does not have it.
pub type DecoderFactory = Box<dyn Fn(&str) -> Option<Box<dyn VideoDecoder>>>;

const REPORT_EVERY: Duration = Duration::from_millis(100);
const STATS_EVERY: Duration = Duration::from_millis(500);
/// How long the loop waits for a message before it looks at its timers.
const SLICE: Duration = Duration::from_millis(5);

/// Runs the viewer until the net service stops it, hangs up or the sink is closed. Pictures
/// are shown as they arrive; a delta frame that cannot be decoded asks for a key frame and
/// shows nothing damaged.
pub fn run(decoders: DecoderFactory, mut sink: Box<dyn FrameSink>, port: Port) {
    let Port {
        from_service,
        to_service,
    } = port;
    let clock = Instant::now();
    let now_us = |clock: &Instant| clock.elapsed().as_micros() as u64;
    let mut reassembler = Reassembler::new(0);
    let mut decoder: Option<Box<dyn VideoDecoder>> = None;
    let mut epoch: u8 = 0;
    let mut shown: u64 = 0;
    let mut shown_at_stats: u64 = 0;
    let mut shown_window: u32 = 0;
    let mut decoded_window: u32 = 0;
    let mut stats = WorkerStats::default();
    let mut last_report = Instant::now();
    let mut last_stats = Instant::now();
    loop {
        if sink.closed() {
            let _ = to_service.send(FromWorker::Ended("the window was closed".into()));
            return;
        }
        sink.idle();
        match from_service.recv_timeout(SLICE) {
            Ok(ToWorker::Start(start)) => {
                epoch = start.epoch;
                reassembler.set_epoch(epoch);
                decoder = decoders(&start.codec);
                if decoder.is_none() {
                    let _ = to_service.send(FromWorker::Ended(format!(
                        "this PC cannot decode {}",
                        start.codec
                    )));
                    return;
                }
            }
            Ok(ToWorker::Datagram(bytes)) => {
                // Before the setup there is no decoder: such a picture is of no use.
                if decoder.is_some() {
                    let _ = reassembler.on_datagram(now_us(&clock), &bytes);
                }
            }
            Ok(ToWorker::Stop) | Err(RecvTimeoutError::Disconnected) => {
                sink.close();
                return;
            }
            Ok(ToWorker::Rate { .. }) | Ok(ToWorker::KeyFrame) => {}
            Err(RecvTimeoutError::Timeout) => {}
        }
        // Pictures that are complete now.
        while let Some(done) = reassembler.poll_frame(now_us(&clock)) {
            let Some(decoder) = decoder.as_mut() else {
                break;
            };
            decoded_window += 1;
            match decoder.decode(&done.encoded) {
                Ok(picture) => {
                    sink.show(&picture);
                    shown += 1;
                    shown_window += 1;
                    stats.width = picture.width;
                    stats.height = picture.height;
                }
                Err(CodecError::MissingReference) | Err(CodecError::Malformed(_)) => {
                    let _ = to_service.send(FromWorker::Key(Key {
                        epoch,
                        frame: done.frame,
                    }));
                }
                // The decoder holds the picture and shows it with the next one.
                Err(CodecError::BadFrame) | Err(CodecError::Pending) => {}
                Err(CodecError::Failed(why)) => {
                    let _ =
                        to_service.send(FromWorker::Ended(format!("the decoder failed: {why}")));
                    return;
                }
            }
        }
        if let Some(request) = reassembler.poll_key_request(now_us(&clock)) {
            let _ = to_service.send(FromWorker::Key(Key {
                epoch: request.epoch,
                frame: request.frame,
            }));
        }
        if last_report.elapsed() >= REPORT_EVERY {
            last_report = Instant::now();
            let interval = reassembler.take_interval();
            let report = Report {
                frame: reassembler.newest_frame().unwrap_or(0),
                received: interval.received,
                lost: interval.lost,
                recovered: interval.recovered,
                // The growth of the one-way delay needs the streamer's clock; version 0 sends 0.
                delay_trend_us: 0,
                queue_ms: 0,
                decoded_fps: decoded_window * 10,
                shown_fps: shown_window * 10,
            };
            decoded_window = 0;
            shown_window = 0;
            if to_service.send(FromWorker::Report(report)).is_err() {
                return;
            }
        }
        if last_stats.elapsed() >= STATS_EVERY {
            let seconds = last_stats.elapsed().as_secs_f64();
            last_stats = Instant::now();
            stats.frames = shown;
            stats.fps = ((shown - shown_at_stats) as f64 / seconds).round() as u32;
            shown_at_stats = shown;
            if to_service.send(FromWorker::Stats(stats)).is_err() {
                return;
            }
        }
    }
}
