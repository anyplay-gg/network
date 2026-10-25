//! The share worker: the program that, for a streamer, turns pictures into the datagrams of a
//! stream and, for a viewer, turns datagrams back into pictures on a screen.
//!
//! It touches no network and no account. The net service talks to it over the share link
//! (`anyplay_share::link`), on a loopback socket for the real program ([`socket`]) or on
//! channels in tests. Everything with a seam is a trait: [`host::FrameSource`] (where pictures
//! come from: the test pattern now, window capture later), `VideoEncoder` and `VideoDecoder`
//! (the test codec now, hardware codecs later) and [`viewer::FrameSink`] (where pictures go: a
//! window, or a counter in tests).

pub mod host;
pub mod socket;
pub mod viewer;

use std::sync::mpsc;

use anyplay_share::control::CodecCaps;
use anyplay_share::link::{Capabilities, FromWorker, ToWorker};

/// The worker's end of the share link on channels.
pub struct Port {
    pub from_service: mpsc::Receiver<ToWorker>,
    pub to_service: mpsc::Sender<FromWorker>,
}

/// What this build can do: the test codec, from the test pattern. Real codecs and window
/// capture are added behind the same traits.
pub fn capabilities() -> Capabilities {
    let test = CodecCaps {
        codec: "test".into(),
        hardware: false,
        max_width: 1920,
        max_height: 1080,
        max_fps: 60,
    };
    Capabilities {
        encode: vec![test.clone()],
        decode: vec![test],
        capture: false,
    }
}
