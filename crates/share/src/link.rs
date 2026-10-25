//! The share link: what the net service and the share worker say to each other.
//!
//! Between the net service and the worker there is a loopback socket with the framing of the
//! bridge protocol and kinds of its own (`spec/share-play.md#the-share-link`). This module holds
//! the messages as plain data, so that the net service, the worker and the tests speak the same
//! types whether they sit on a socket or on in-memory channels, and the framing for the socket.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::control::{CodecCaps, Key, Report};

/// The version of the share link a worker and a net service speak.
pub const LINK_VERSION: u32 = 0;

/// The most a frame's payload may hold: one datagram, or a small JSON object.
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// What a worker can do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub encode: Vec<CodecCaps>,
    pub decode: Vec<CodecCaps>,
    /// It can capture the game's window.
    #[serde(default)]
    pub capture: bool,
    /// The codec libraries it loads while it runs (OpenH264 on Linux) and how each stands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub libraries: Vec<CodecLibrary>,
}

/// A codec library the worker loads while it runs instead of carrying it, and whether it could.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodecLibrary {
    /// Which library: `openh264`.
    pub name: String,
    pub state: LibraryState,
    /// Why, when it is not `ready`: the file, the error, the hash it refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// How a codec library stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryState {
    /// Loaded: its codecs are in `encode` and `decode`.
    Ready,
    /// The player turned it off.
    Off,
    /// Not installed.
    Missing,
    /// A file is there, but not a build the worker may load (its hash is not a known one).
    Refused,
    /// A known build that would not load.
    Failed,
}

impl LibraryState {
    /// The state's name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryState::Ready => "ready",
            LibraryState::Off => "off",
            LibraryState::Missing => "missing",
            LibraryState::Refused => "refused",
            LibraryState::Failed => "failed",
        }
    }
}

/// A new setup for a running worker: the streamer's worker starts encoding with it and the
/// viewer's worker starts decoding with it. A change of `epoch` is a change of codec or size.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    pub epoch: u8,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
    pub parity_percent: u8,
    /// The most bytes of a datagram's payload (the connection's datagram size less the header).
    pub max_payload: usize,
}

/// What the net service tells a worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToWorker {
    Start(Start),
    Rate {
        kbps: u32,
        parity_percent: u8,
    },
    /// An IDR is needed.
    KeyFrame,
    /// A datagram received from the other side (viewer's worker).
    Datagram(Vec<u8>),
    Stop,
}

/// What a worker tells the net service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromWorker {
    /// A datagram to send to the viewers (streamer's worker).
    Datagram(Vec<u8>),
    /// What the controller needs (viewer's worker).
    Report(Report),
    /// An IDR is needed from the streamer (viewer's worker).
    Key(Key),
    Stats(WorkerStats),
    /// The worker ended (crashed, was closed, lost its source).
    Ended(String),
}

/// How a worker is doing, about twice a second.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerStats {
    /// Pictures sent (streamer) or shown (viewer) so far.
    pub frames: u64,
    pub fps: u32,
    pub kbps: u32,
    pub width: u32,
    pub height: u32,
}

/// The first frame from the worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub token: String,
    /// `host` or `guest`.
    pub role: crate::control::Role,
}

/// The answer to a [`Hello`]: the link is accepted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub version: u32,
}

// Kinds of the link's frames.
pub const KIND_HELLO: u16 = 0x0001;
pub const KIND_WELCOME: u16 = 0x0002;
pub const KIND_START: u16 = 0x0011;
pub const KIND_RATE: u16 = 0x0012;
pub const KIND_KEY: u16 = 0x0013;
pub const KIND_DATAGRAM_OUT: u16 = 0x0014;
pub const KIND_DATAGRAM_IN: u16 = 0x0015;
pub const KIND_REPORT: u16 = 0x0016;
pub const KIND_STATS: u16 = 0x0018;
pub const KIND_STOP: u16 = 0x0019;
pub const KIND_KEY_REQUEST: u16 = 0x001A;
pub const KIND_ENDED: u16 = 0x001B;

/// One frame of the link: `length: u32`, `kind: u16`, `flags: u16`, `request: u32`, payload;
/// little-endian, as in the bridge protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u16,
    pub payload: Vec<u8>,
}

/// Why a frame could not be read.
#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    /// A payload over [`MAX_PAYLOAD`].
    TooLarge(usize),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "{e}"),
            FrameError::TooLarge(n) => write!(f, "a link frame of {n} bytes is too large"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        FrameError::Io(e)
    }
}

impl Frame {
    pub fn new(kind: u16, payload: Vec<u8>) -> Frame {
        Frame { kind, payload }
    }

    pub fn json<T: Serialize>(kind: u16, value: &T) -> Frame {
        Frame {
            kind,
            payload: serde_json::to_vec(value).unwrap_or_default(),
        }
    }

    /// The bytes on the wire.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.payload.len());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(&self.encode())
    }

    /// Reads one frame; `Ok(None)` at a clean end of the stream.
    pub fn read_from(reader: &mut impl Read) -> Result<Option<Frame>, FrameError> {
        let mut header = [0u8; 12];
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let length = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        if length > MAX_PAYLOAD {
            return Err(FrameError::TooLarge(length));
        }
        let kind = u16::from_le_bytes([header[4], header[5]]);
        let mut payload = vec![0u8; length];
        reader.read_exact(&mut payload)?;
        Ok(Some(Frame { kind, payload }))
    }
}

impl ToWorker {
    pub fn to_frame(&self) -> Frame {
        match self {
            ToWorker::Start(start) => Frame::json(KIND_START, start),
            ToWorker::Rate {
                kbps,
                parity_percent,
            } => {
                let mut payload = kbps.to_le_bytes().to_vec();
                payload.push(*parity_percent);
                Frame::new(KIND_RATE, payload)
            }
            ToWorker::KeyFrame => Frame::new(KIND_KEY, Vec::new()),
            ToWorker::Datagram(bytes) => Frame::new(KIND_DATAGRAM_IN, bytes.clone()),
            ToWorker::Stop => Frame::new(KIND_STOP, Vec::new()),
        }
    }

    /// `None` for a kind this version does not know, which is skipped.
    pub fn from_frame(frame: &Frame) -> Option<ToWorker> {
        match frame.kind {
            KIND_START => serde_json::from_slice(&frame.payload)
                .ok()
                .map(ToWorker::Start),
            KIND_RATE if frame.payload.len() == 5 => Some(ToWorker::Rate {
                kbps: u32::from_le_bytes(frame.payload[..4].try_into().ok()?),
                parity_percent: frame.payload[4],
            }),
            KIND_KEY => Some(ToWorker::KeyFrame),
            KIND_DATAGRAM_IN => Some(ToWorker::Datagram(frame.payload.clone())),
            KIND_STOP => Some(ToWorker::Stop),
            _ => None,
        }
    }
}

impl FromWorker {
    pub fn to_frame(&self) -> Frame {
        match self {
            FromWorker::Datagram(bytes) => Frame::new(KIND_DATAGRAM_OUT, bytes.clone()),
            FromWorker::Report(report) => Frame::json(KIND_REPORT, report),
            FromWorker::Key(key) => Frame::json(KIND_KEY_REQUEST, key),
            FromWorker::Stats(stats) => Frame::json(KIND_STATS, stats),
            FromWorker::Ended(reason) => Frame::new(KIND_ENDED, reason.as_bytes().to_vec()),
        }
    }

    pub fn from_frame(frame: &Frame) -> Option<FromWorker> {
        match frame.kind {
            KIND_DATAGRAM_OUT => Some(FromWorker::Datagram(frame.payload.clone())),
            KIND_REPORT => serde_json::from_slice(&frame.payload)
                .ok()
                .map(FromWorker::Report),
            KIND_KEY_REQUEST => serde_json::from_slice(&frame.payload)
                .ok()
                .map(FromWorker::Key),
            KIND_STATS => serde_json::from_slice(&frame.payload)
                .ok()
                .map(FromWorker::Stats),
            KIND_ENDED => Some(FromWorker::Ended(
                String::from_utf8_lossy(&frame.payload).into_owned(),
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start() -> Start {
        Start {
            epoch: 3,
            codec: "test".into(),
            width: 320,
            height: 180,
            fps: 60,
            kbps: 4000,
            parity_percent: 5,
            max_payload: 1180,
        }
    }

    #[test]
    fn messages_survive_the_wire() {
        let to = [
            ToWorker::Start(start()),
            ToWorker::Rate {
                kbps: 2500,
                parity_percent: 10,
            },
            ToWorker::KeyFrame,
            ToWorker::Datagram(vec![1, 2, 3]),
            ToWorker::Stop,
        ];
        for message in to {
            let bytes = message.to_frame().encode();
            let frame = Frame::read_from(&mut bytes.as_slice()).unwrap().unwrap();
            assert_eq!(ToWorker::from_frame(&frame), Some(message));
        }
        let from = [
            FromWorker::Datagram(vec![9; 1200]),
            FromWorker::Key(Key { epoch: 1, frame: 7 }),
            FromWorker::Stats(WorkerStats {
                frames: 5,
                fps: 60,
                kbps: 4000,
                width: 320,
                height: 180,
            }),
            FromWorker::Ended("capture lost".into()),
        ];
        for message in from {
            let bytes = message.to_frame().encode();
            let frame = Frame::read_from(&mut bytes.as_slice()).unwrap().unwrap();
            assert_eq!(FromWorker::from_frame(&frame), Some(message));
        }
    }

    #[test]
    fn a_frame_over_the_limit_or_cut_short_is_refused() {
        let mut bytes = Frame::new(KIND_STOP, vec![0; 4]).encode();
        bytes[0..4].copy_from_slice(&((MAX_PAYLOAD as u32) + 1).to_le_bytes());
        assert!(matches!(
            Frame::read_from(&mut bytes.as_slice()),
            Err(FrameError::TooLarge(_))
        ));
        let whole = Frame::new(KIND_STOP, vec![1, 2, 3]).encode();
        assert!(Frame::read_from(&mut &whole[..whole.len() - 1]).is_err());
        assert_eq!(Frame::read_from(&mut &[][..]).unwrap(), None);
    }

    #[test]
    fn an_unknown_kind_is_skipped_and_a_bad_payload_is_not_a_message() {
        assert_eq!(ToWorker::from_frame(&Frame::new(0x7777, vec![])), None);
        assert_eq!(
            ToWorker::from_frame(&Frame::new(KIND_START, b"{".to_vec())),
            None
        );
        assert_eq!(
            ToWorker::from_frame(&Frame::new(KIND_RATE, vec![1, 2])),
            None
        );
    }

    #[test]
    fn capabilities_name_their_codec_library_and_older_ones_still_read() {
        let old: Capabilities =
            serde_json::from_str(r#"{"encode":[],"decode":[],"capture":true}"#).unwrap();
        assert!(old.libraries.is_empty());
        assert!(!serde_json::to_string(&old).unwrap().contains("libraries"));

        let now = Capabilities {
            libraries: vec![CodecLibrary {
                name: "openh264".into(),
                state: LibraryState::Refused,
                detail: Some("not a known build".into()),
            }],
            ..old
        };
        let json = serde_json::to_string(&now).unwrap();
        assert!(json.contains(
            r#""libraries":[{"name":"openh264","state":"refused","detail":"not a known build"}]"#
        ));
        assert_eq!(serde_json::from_str::<Capabilities>(&json).unwrap(), now);
        for state in [
            LibraryState::Ready,
            LibraryState::Off,
            LibraryState::Missing,
            LibraryState::Refused,
            LibraryState::Failed,
        ] {
            let wire = serde_json::to_string(&state).unwrap();
            assert_eq!(wire, format!("\"{}\"", state.as_str()));
        }
    }
}
