//! The seam between the share engine and a real codec. The engine never links a codec; the
//! worker plugs hardware or software encoders and decoders in behind these two traits. The
//! [`testcodec`](crate::testcodec) module is the stand-in used until then.

use std::fmt;

/// One picture, BGRA8, SDR, `width * height * 4` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Host microseconds at capture, wrapping at 2^32.
    pub timestamp_us: u32,
    pub bgra: Vec<u8>,
}

impl Frame {
    /// `true` when the buffer holds exactly `width * height` BGRA pixels.
    pub fn is_well_formed(&self) -> bool {
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|p| p.checked_mul(4))
            == Some(self.bgra.len())
    }
}

/// One encoded picture: what the packetiser cuts and the reassembler hands back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    /// A key frame decodes without any earlier frame.
    pub key: bool,
    /// Host microseconds at capture, wrapping at 2^32.
    pub timestamp_us: u32,
    pub data: Vec<u8>,
}

/// Why a codec refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// The input is not a frame this codec wrote.
    Malformed(&'static str),
    /// A delta frame arrived without the frame it builds on.
    MissingReference,
    /// The frame buffer does not match its size.
    BadFrame,
    /// A codec with a pipeline (the hardware ones) took the input and has no output yet. Not a
    /// failure: the output of this input comes with a later call, in order.
    Pending,
    /// The codec stopped working (the driver reset, the device was lost). The stream ends.
    Failed(String),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Malformed(why) => write!(f, "malformed encoded frame: {why}"),
            CodecError::MissingReference => write!(f, "the frame this one builds on is missing"),
            CodecError::BadFrame => write!(f, "the frame buffer does not match its size"),
            CodecError::Pending => write!(f, "the codec has no output for this input yet"),
            CodecError::Failed(why) => write!(f, "the codec failed: {why}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Turns pictures into encoded frames.
pub trait VideoEncoder {
    /// The codec's name as announced in `hello` and `config`.
    fn codec(&self) -> &str;
    /// Encode one picture. `force_key` makes it a key frame.
    fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError>;
    /// A new target rate in kbit/s. Codecs without a rate (the test codec) ignore it.
    fn set_bitrate(&mut self, _kbps: u32) {}
}

/// Turns encoded frames back into pictures.
pub trait VideoDecoder {
    fn codec(&self) -> &str;
    /// Decode one frame. A decoder that misses a reference returns
    /// [`CodecError::MissingReference`] instead of a damaged picture.
    fn decode(&mut self, frame: &EncodedFrame) -> Result<Frame, CodecError>;
}
