//! The share worker: the program that, for a streamer, turns pictures into the datagrams of a
//! stream and, for a viewer, turns datagrams back into pictures on a screen.
//!
//! It touches no network and no account. The net service talks to it over the share link
//! (`anyplay_share::link`), on a loopback socket for the real program ([`socket`]) or on
//! channels in tests. Everything with a seam is a trait: [`host::FrameSource`] (where pictures
//! come from: window capture, or the test pattern), `VideoEncoder` and `VideoDecoder` (H.264
//! from the system on Windows, [`mf`]; the test codec for tests) and [`viewer::FrameSink`]
//! (where pictures go: a window, or a counter in tests).

pub mod capture;
pub mod clips;
pub mod host;
#[cfg(windows)]
pub mod mf;
#[cfg(target_os = "linux")]
pub mod portal;
pub mod socket;
pub mod viewer;
#[cfg(any(windows, target_os = "linux"))]
pub mod window;

use std::sync::mpsc;

use anyplay_share::codec::{VideoDecoder, VideoEncoder};
use anyplay_share::control::CodecCaps;
use anyplay_share::link::{Capabilities, FromWorker, Start, ToWorker};

/// The worker's end of the share link on channels.
pub struct Port {
    pub from_service: mpsc::Receiver<ToWorker>,
    pub to_service: mpsc::Sender<FromWorker>,
}

/// A key frame at least this often from the test codec, so that a viewer who joins late waits
/// at most 2 seconds.
pub const TEST_KEY_INTERVAL: u32 = 120;

/// The test codec's capabilities. It is advertised so that tests can stream the test pattern;
/// the net service uses it only for a stream whose source is the test pattern.
pub fn test_codec_caps() -> CodecCaps {
    CodecCaps {
        codec: "test".into(),
        hardware: false,
        max_width: 1920,
        max_height: 1080,
        max_fps: 60,
    }
}

/// What this build can do on this PC: H.264 from the system where the system has it (Windows),
/// the test codec, and window capture where it is built (Windows).
pub fn capabilities() -> Capabilities {
    #[cfg(windows)]
    let (mut encode, mut decode) = mf::capabilities();
    #[cfg(not(windows))]
    let (mut encode, mut decode) = (Vec::new(), Vec::new());
    // After the real codecs: the net service prefers the first both sides have.
    encode.push(test_codec_caps());
    decode.push(test_codec_caps());
    Capabilities {
        encode,
        decode,
        capture: cfg!(windows),
    }
}

/// The encoder for a stream setup. Lowers `start` to what the encoder can do: a software
/// encoder gives at most 720p30 so that the game keeps its processor (the spec's rule), a
/// hardware one 1080p60.
pub fn encoder_for(start: &mut Start) -> Result<Box<dyn VideoEncoder>, String> {
    match start.codec.as_str() {
        "test" => Ok(Box::new(anyplay_share::testcodec::TestEncoder::new(
            TEST_KEY_INTERVAL,
        ))),
        #[cfg(windows)]
        "h264" => {
            let hardware = mf::Limits::HARDWARE;
            let software = mf::Limits::SOFTWARE;
            let attempt = |limits: mf::Limits, kind| {
                let setup = limits.clamp(start);
                mf::MfEncoder::open(setup, kind).map(|e| (e, setup))
            };
            let (encoder, setup) = attempt(hardware, mf::Kind::Hardware).or_else(|why| {
                attempt(software, mf::Kind::Software)
                    .map_err(|software| format!("hardware: {why}; software: {software}"))
            })?;
            note_encoder(&encoder);
            start.width = setup.width;
            start.height = setup.height;
            start.fps = setup.fps;
            Ok(Box::new(encoder))
        }
        other => Err(format!("this PC cannot encode {other}")),
    }
}

#[cfg(windows)]
fn note_encoder(encoder: &mf::MfEncoder) {
    eprintln!(
        "share worker: encoding with {} ({})",
        encoder.name(),
        if encoder.is_hardware() {
            "hardware"
        } else {
            "software"
        }
    );
}

/// The encoder for clips: a hardware H.264 encoder, never the system's software one, because a
/// clip recorder runs for as long as the game does and must not take its processor. Lowers
/// `setup` to what the encoder gives.
pub fn clips_encoder(setup: &mut clips::recorder::Setup) -> Result<Box<dyn VideoEncoder>, String> {
    #[cfg(windows)]
    {
        let start = Start {
            epoch: 0,
            codec: "h264".into(),
            width: setup.width,
            height: setup.height,
            fps: setup.fps,
            kbps: setup.kbps,
            parity_percent: 0,
            max_payload: 0,
        };
        let lowered = mf::Limits::HARDWARE.clamp(&start);
        let encoder = mf::MfEncoder::open(lowered, mf::Kind::Hardware)?;
        note_encoder(&encoder);
        setup.width = lowered.width;
        setup.height = lowered.height;
        setup.fps = lowered.fps;
        Ok(Box::new(encoder))
    }
    #[cfg(not(windows))]
    {
        let _ = setup;
        Err("clips are not built for this system yet".into())
    }
}

/// The decoder for the codec a streamer chose; none when this PC does not have it.
pub fn decoder_for(codec: &str) -> Option<Box<dyn VideoDecoder>> {
    match codec {
        "test" => Some(Box::new(anyplay_share::testcodec::TestDecoder::new())),
        #[cfg(windows)]
        "h264" => mf::MfDecoder::open()
            .ok()
            .map(|d| Box::new(d) as Box<dyn VideoDecoder>),
        _ => None,
    }
}
