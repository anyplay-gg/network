//! Clips: the last seconds of a game, kept in memory as encoded H.264 and saved as an MP4 file
//! when the player asks (spec/clips.md, ADR 0028).
//!
//! The worker runs in this mode on its own, without a share link: the launcher starts it with
//! `--clips`, sends it one JSON command per line on its input and reads one JSON event per line
//! from its output. The module has three parts:
//!
//! - [`ring`]: the groups of pictures, bounded by time and by bytes (pure);
//! - [`mux`]: the MP4 writer (pure);
//! - [`recorder`]: the loop that takes pictures from a [`FrameSource`](crate::host::FrameSource),
//!   encodes them and answers the commands.

pub mod mux;
pub mod recorder;
pub mod ring;

use serde::{Deserialize, Serialize};

/// What the launcher tells the recorder, one JSON object per line.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Command {
    /// Write the kept seconds to this file.
    Save { path: String },
    /// End, without saving.
    Quit,
}

/// What the recorder tells the launcher, one JSON object per line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// The encoder runs and the ring fills.
    Ready {
        width: u32,
        height: u32,
        fps: u32,
        kbps: u32,
    },
    /// A clip was written.
    Saved {
        path: String,
        seconds: f64,
        bytes: u64,
    },
    /// A save did not work (`empty`: nothing kept yet; else the reason in words). The recorder
    /// goes on.
    SaveFailed { reason: String },
    /// The recorder ended. `reason`: `no_encoder`, `no_window`, `window_closed`,
    /// `encoder_failed`, `quit`, or `unsupported` where the system cannot do it.
    Ended { reason: String },
}

impl Event {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The key frame interval of a clip: a save can start no later than this before the window.
pub const KEY_SECONDS: u32 = 2;
/// The most memory the ring may hold.
pub const MAX_BYTES: usize = 160 * 1024 * 1024;
/// The picture size, rate and bit rate of a clip: modest, because the game has the machine.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
pub const FPS: u32 = 30;
pub const KBPS: u32 = 6_000;
