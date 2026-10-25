//! Share play for the AnyPlay network: the pure part of the share worker.
//!
//! This crate is the engine under both ends of a share and under the fuzz targets, as
//! `anyplay-voice` is for voice. It reads and writes the wire formats of `spec/share-play.md`
//! and decides things, and nothing else: no sockets, no threads, no clock (time comes in as
//! microseconds from the caller's own monotonic clock) and no codec library (codecs plug in
//! behind [`VideoEncoder`] and [`VideoDecoder`]).
//!
//! * [`wire`]: the video and audio datagrams, with a [`wire::Refusal`] for each reason the
//!   receiver rules give.
//! * [`packetiser`]: one encoded frame into datagrams with Reed-Solomon parity.
//! * [`reassembler`]: datagrams into complete frames, in order, never damaged.
//! * [`rate`]: the rate controller, a pure function of the reports and the connection.
//! * [`control`]: the control stream's JSON lines.
//! * [`testcodec`]: a lossless test codec and a test picture that carries its frame number.
//!
//! ```
//! use anyplay_share::codec::{VideoDecoder, VideoEncoder};
//! use anyplay_share::packetiser::Packetiser;
//! use anyplay_share::reassembler::Reassembler;
//! use anyplay_share::testcodec::{TestDecoder, TestEncoder, TestPattern};
//!
//! // host: picture -> encoder -> datagrams (1180 = max_datagram_size - 20)
//! let (mut source, mut encoder) = (TestPattern::default(), TestEncoder::new(60));
//! let mut packetiser = Packetiser::new(1180, 0);
//! let encoded = encoder.encode(&source.next_frame(), false).unwrap();
//! let datagrams = packetiser.packetise(&encoded, 10).unwrap();
//!
//! // guest: datagrams -> reassembler -> decoder (send each `datagram` on the share connection)
//! let (mut reassembler, mut decoder) = (Reassembler::new(0), TestDecoder::new());
//! for datagram in &datagrams {
//!     reassembler.on_datagram(1_000, datagram).unwrap();
//! }
//! let frame = reassembler.poll_frame(1_000).expect("a complete frame");
//! let (picture, number) = decoder.decode_numbered(&frame.encoded).unwrap();
//! assert_eq!((number, picture.width), (Some(0), 320));
//! ```

pub mod codec;
pub mod control;
pub mod link;
pub mod packetiser;
pub mod rate;
pub mod reassembler;
mod rs;
pub mod testcodec;
pub mod wire;

pub use codec::{CodecError, EncodedFrame, Frame, VideoDecoder, VideoEncoder};
pub use packetiser::{PacketiseError, Packetiser};
pub use rate::{PathKind, RateConfig, RateController, RateInput, Target};
pub use reassembler::{KeyRequest, Reassembled, Reassembler};
pub use wire::Refusal;
