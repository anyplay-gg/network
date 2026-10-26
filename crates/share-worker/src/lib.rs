//! The share worker: the program that, for a streamer, turns pictures into the datagrams of a
//! stream and, for a viewer, turns datagrams back into pictures on a screen.
//!
//! It touches no network and no account. The net service talks to it over the share link
//! (`anyplay_share::link`), on a loopback socket for the real program ([`socket`]) or on
//! channels in tests. Everything with a seam is a trait: [`host::FrameSource`] (where pictures
//! come from: window capture, or the test pattern), `VideoEncoder` and `VideoDecoder` (H.264
//! from the system on Windows, [`mf`], and on macOS, `mac::vt`; OpenH264 from Cisco's binary on
//! Linux, `oh264`; the test codec for tests) and [`viewer::FrameSink`]
//! (where pictures go: a window, or a counter in tests).

pub mod access;
pub mod annexb;
pub mod broadcast;
pub mod capture;
pub mod clips;
pub mod host;
pub mod limits;
#[cfg(windows)]
pub mod loopback;
#[cfg(target_os = "macos")]
pub mod mac;
#[cfg(windows)]
pub mod mf;
#[cfg(target_os = "linux")]
pub mod oh264;
pub mod pick;
#[cfg(target_os = "linux")]
pub mod portal;
pub mod socket;
pub mod sound;
#[cfg(windows)]
pub mod speakers;
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

/// What this build can do on this PC: H.264 from the system where the system has it (Windows,
/// macOS),
/// from Cisco's OpenH264 where it is installed (Linux), the test codec, and window capture where
/// it is built (Windows, Linux, macOS).
pub fn capabilities() -> Capabilities {
    #[cfg(windows)]
    let ((mut encode, mut decode), libraries) = (mf::capabilities(), Vec::new());
    #[cfg(target_os = "linux")]
    let (mut encode, mut decode, libraries) = {
        let (encode, decode, openh264) = oh264::capabilities();
        (encode, decode, vec![openh264])
    };
    #[cfg(target_os = "macos")]
    let (mut encode, mut decode, libraries) = {
        let (encode, decode) = mac::vt::capabilities();
        (encode, decode, Vec::new())
    };
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let (mut encode, mut decode, libraries) = (Vec::new(), Vec::new(), Vec::new());
    // After the real codecs: the net service prefers the first both sides have.
    encode.push(test_codec_caps());
    decode.push(test_codec_caps());
    Capabilities {
        encode,
        decode,
        capture: cfg!(any(windows, target_os = "linux", target_os = "macos")),
        libraries,
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
        #[cfg(target_os = "linux")]
        "h264" => {
            let setup = limits::Limits::SOFTWARE.clamp(start);
            let encoder = oh264::OhEncoder::open(setup)?;
            eprintln!("share worker: encoding with OpenH264 (software)");
            start.width = setup.width;
            start.height = setup.height;
            start.fps = setup.fps;
            Ok(Box::new(encoder))
        }
        #[cfg(target_os = "macos")]
        "h264" => {
            use mac::vt::{Kind, VtEncoder};
            let attempt = |limits: limits::Limits, kind| {
                let setup = limits.clamp(start);
                VtEncoder::open(setup, kind).map(|e| (e, setup))
            };
            let (encoder, setup) = attempt(limits::Limits::HARDWARE, Kind::Hardware).or_else(|why| {
                attempt(limits::Limits::SOFTWARE, Kind::Software)
                    .map_err(|software| format!("hardware: {why}; software: {software}"))
            })?;
            eprintln!(
                "share worker: encoding with {} ({})",
                encoder.name(),
                if encoder.is_hardware() { "hardware" } else { "software" }
            );
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
    #[cfg(target_os = "macos")]
    {
        use mac::vt::{Kind, VtEncoder};
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
        let lowered = limits::Limits::HARDWARE.clamp(&start);
        let encoder = VtEncoder::open(lowered, Kind::Hardware)?;
        eprintln!("share worker: encoding with {} (hardware)", encoder.name());
        setup.width = lowered.width;
        setup.height = lowered.height;
        setup.fps = lowered.fps;
        Ok(Box::new(encoder))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        // Linux has only OpenH264 in software, which clips never use.
        let _ = setup;
        Err("clips need a hardware encoder, which this system does not have yet".into())
    }
}

/// The encoder for a broadcast: a hardware H.264 encoder where there is one, else the system's
/// software one (Windows) or OpenH264 (Linux), lowered as a stream is. Lowers `setup` to what the
/// encoder gives.
pub fn broadcast_encoder(setup: &mut broadcast::Setup) -> Result<Box<dyn VideoEncoder>, String> {
    let mut start = Start {
        epoch: 0,
        codec: "h264".into(),
        width: setup.width,
        height: setup.height,
        fps: setup.fps,
        kbps: setup.kbps,
        parity_percent: 0,
        max_payload: 0,
    };
    let encoder = encoder_for(&mut start)?;
    setup.width = start.width;
    setup.height = start.height;
    setup.fps = start.fps;
    Ok(encoder)
}

/// The decoder for the codec a streamer chose; none when this PC does not have it.
pub fn decoder_for(codec: &str) -> Option<Box<dyn VideoDecoder>> {
    match codec {
        "test" => Some(Box::new(anyplay_share::testcodec::TestDecoder::new())),
        #[cfg(windows)]
        "h264" => mf::MfDecoder::open()
            .ok()
            .map(|d| Box::new(d) as Box<dyn VideoDecoder>),
        #[cfg(target_os = "linux")]
        "h264" => oh264::OhDecoder::open()
            .ok()
            .map(|d| Box::new(d) as Box<dyn VideoDecoder>),
        _ => None,
    }
}
