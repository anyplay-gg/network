//! A stand-in for real codecs, small and exact: a test picture that carries its own frame
//! number in big blocks, and a lossless codec ("test") that makes a lost frame visible.
//!
//! * [`TestPattern`] makes BGRA frames: moving bands of colour, and the frame number as 32
//!   black or white blocks along the top edge, so [`TestPattern::read_number`] reads it back
//!   exactly.
//! * [`TestEncoder`] writes key frames as run-lengths of pixels and other frames as run-lengths
//!   of the XOR against the previous frame. A key frame comes every N frames, on request and
//!   at the start.
//! * [`TestDecoder`] reverses it and errors with [`CodecError::MissingReference`] when a delta
//!   frame does not follow the frame it was made against, so a lost frame cannot go unnoticed.
//!
//! Layout of an encoded frame: `'T'`, kind (0 key, 1 delta), sequence `u32`, width `u32`,
//! height `u32`, then runs of `(count u8 in 1..=255, pixel 4 bytes)`.

use crate::codec::{CodecError, EncodedFrame, Frame, VideoDecoder, VideoEncoder};

/// The codec name in `hello` and `config`.
pub const CODEC_NAME: &str = "test";

const MAGIC: u8 = b'T';
const HEADER: usize = 14;
const KIND_KEY: u8 = 0;
const KIND_DELTA: u8 = 1;

/// Width and height of the default test picture.
pub const DEFAULT_WIDTH: u32 = 320;
pub const DEFAULT_HEIGHT: u32 = 180;

/// Bits of the frame number drawn into the picture.
const NUMBER_BITS: u32 = 32;

/// Microseconds between two frames of the pattern (60 frames per second).
pub const FRAME_INTERVAL_US: u32 = 16_667;

fn block_size(width: u32) -> u32 {
    width / NUMBER_BITS
}

/// A deterministic frame source. Needs a width of at least 128 and a height of at least one
/// block.
#[derive(Clone, Debug)]
pub struct TestPattern {
    width: u32,
    height: u32,
    number: u32,
}

impl Default for TestPattern {
    fn default() -> TestPattern {
        TestPattern::new(DEFAULT_WIDTH, DEFAULT_HEIGHT)
    }
}

impl TestPattern {
    pub fn new(width: u32, height: u32) -> TestPattern {
        assert!(
            width >= 128 && height >= block_size(width),
            "picture too small"
        );
        TestPattern {
            width,
            height,
            number: 0,
        }
    }

    /// The next frame, numbered 0, 1, 2, ...
    pub fn next_frame(&mut self) -> Frame {
        let f = TestPattern::render(self.width, self.height, self.number);
        self.number = self.number.wrapping_add(1);
        f
    }

    /// The frame with a given number. The same number always gives the same bytes.
    pub fn render(width: u32, height: u32, number: u32) -> Frame {
        let (w, h) = (width as usize, height as usize);
        let bs = block_size(width) as usize;
        let mut bgra = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let band = ((x as u64 + u64::from(number)) / 32) % 8;
                let px = [
                    (band * 32) as u8,
                    ((y / 6) * 5) as u8,
                    (band * 36 + (y as u64 / 45) * 20) as u8,
                    255,
                ];
                bgra[(y * w + x) * 4..][..4].copy_from_slice(&px);
            }
        }
        for bit in 0..NUMBER_BITS as usize {
            let on = (number >> (NUMBER_BITS as usize - 1 - bit)) & 1 == 1;
            let v = if on { 255 } else { 0 };
            for y in 0..bs.min(h) {
                for x in bit * bs..(bit + 1) * bs {
                    bgra[(y * w + x) * 4..][..4].copy_from_slice(&[v, v, v, 255]);
                }
            }
        }
        Frame {
            width,
            height,
            timestamp_us: number.wrapping_mul(FRAME_INTERVAL_US),
            bgra,
        }
    }

    /// Read the frame number back from the blocks of a picture. `None` when the picture is too
    /// small or malformed.
    pub fn read_number(frame: &Frame) -> Option<u32> {
        if !frame.is_well_formed() || frame.width < 128 {
            return None;
        }
        let bs = block_size(frame.width) as usize;
        if frame.height < bs as u32 {
            return None;
        }
        let w = frame.width as usize;
        let mut n = 0u32;
        for bit in 0..NUMBER_BITS as usize {
            let (x, y) = (bit * bs + bs / 2, bs / 2);
            let b = frame.bgra[(y * w + x) * 4];
            n = (n << 1) | u32::from(b >= 128);
        }
        Some(n)
    }
}

fn put_runs(out: &mut Vec<u8>, pixels: impl Iterator<Item = [u8; 4]>) {
    let mut run: Option<([u8; 4], u8)> = None;
    for p in pixels {
        run = match run {
            Some((q, n)) if q == p && n < 255 => Some((q, n + 1)),
            Some((q, n)) => {
                out.push(n);
                out.extend_from_slice(&q);
                Some((p, 1))
            }
            None => Some((p, 1)),
        };
    }
    if let Some((q, n)) = run {
        out.push(n);
        out.extend_from_slice(&q);
    }
}

/// The encoder of the "test" codec.
#[derive(Clone, Debug)]
pub struct TestEncoder {
    key_interval: u32,
    since_key: u32,
    sequence: u32,
    previous: Option<(u32, u32, Vec<u8>)>,
    key_requested: bool,
}

impl TestEncoder {
    /// A key frame every `key_interval` frames (0 means only at the start and on request).
    pub fn new(key_interval: u32) -> TestEncoder {
        TestEncoder {
            key_interval,
            since_key: 0,
            sequence: 0,
            previous: None,
            key_requested: false,
        }
    }

    /// The next frame will be a key frame (the control message `key`).
    pub fn request_key(&mut self) {
        self.key_requested = true;
    }
}

impl VideoEncoder for TestEncoder {
    fn codec(&self) -> &str {
        CODEC_NAME
    }

    fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError> {
        if !frame.is_well_formed() {
            return Err(CodecError::BadFrame);
        }
        let same_size = self
            .previous
            .as_ref()
            .is_some_and(|(w, h, _)| *w == frame.width && *h == frame.height);
        let key = force_key
            || self.key_requested
            || !same_size
            || (self.key_interval > 0 && self.since_key + 1 >= self.key_interval);
        let mut data = Vec::with_capacity(HEADER + 1024);
        data.push(MAGIC);
        data.push(if key { KIND_KEY } else { KIND_DELTA });
        data.extend_from_slice(&self.sequence.to_be_bytes());
        data.extend_from_slice(&frame.width.to_be_bytes());
        data.extend_from_slice(&frame.height.to_be_bytes());
        let pixels = frame.bgra.as_chunks::<4>().0.iter().copied();
        if key {
            put_runs(&mut data, pixels);
            self.since_key = 0;
            self.key_requested = false;
        } else {
            let before = self
                .previous
                .as_ref()
                .map(|p| &p.2)
                .expect("same_size implies a previous frame");
            let xor = pixels
                .zip(before.as_chunks::<4>().0.iter())
                .map(|(c, b)| [c[0] ^ b[0], c[1] ^ b[1], c[2] ^ b[2], c[3] ^ b[3]]);
            put_runs(&mut data, xor);
            self.since_key += 1;
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.previous = Some((frame.width, frame.height, frame.bgra.clone()));
        Ok(EncodedFrame {
            key,
            timestamp_us: frame.timestamp_us,
            data,
        })
    }
}

/// The decoder of the "test" codec.
#[derive(Clone, Debug, Default)]
pub struct TestDecoder {
    previous: Option<(u32, u32, Vec<u8>)>,
    last_sequence: Option<u32>,
}

impl TestDecoder {
    pub fn new() -> TestDecoder {
        TestDecoder::default()
    }

    /// Decode, and read the frame number out of the picture's blocks.
    pub fn decode_numbered(
        &mut self,
        frame: &EncodedFrame,
    ) -> Result<(Frame, Option<u32>), CodecError> {
        let f = self.decode(frame)?;
        let n = TestPattern::read_number(&f);
        Ok((f, n))
    }
}

impl VideoDecoder for TestDecoder {
    fn codec(&self) -> &str {
        CODEC_NAME
    }

    fn decode(&mut self, frame: &EncodedFrame) -> Result<Frame, CodecError> {
        let d = &frame.data;
        if d.len() < HEADER || d[0] != MAGIC {
            return Err(CodecError::Malformed("header"));
        }
        let kind = d[1];
        let sequence = u32::from_be_bytes([d[2], d[3], d[4], d[5]]);
        let width = u32::from_be_bytes([d[6], d[7], d[8], d[9]]);
        let height = u32::from_be_bytes([d[10], d[11], d[12], d[13]]);
        if (kind == KIND_KEY) != frame.key || (kind != KIND_KEY && kind != KIND_DELTA) {
            return Err(CodecError::Malformed("kind"));
        }
        // 16 million pixels is more than 4K; it bounds memory.
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .filter(|&p| p > 0 && p <= 1 << 24)
            .ok_or(CodecError::Malformed("size"))?;
        let base = if kind == KIND_DELTA {
            let ok = self.last_sequence == Some(sequence.wrapping_sub(1));
            match (&self.previous, ok) {
                (Some((w, h, px)), true) if *w == width && *h == height => Some(px),
                _ => return Err(CodecError::MissingReference),
            }
        } else {
            None
        };
        let body = &d[HEADER..];
        if !body.len().is_multiple_of(5) {
            return Err(CodecError::Malformed("runs"));
        }
        if body.as_chunks::<5>().0.iter().any(|r| r[0] == 0)
            || body
                .as_chunks::<5>()
                .0
                .iter()
                .map(|r| usize::from(r[0]))
                .sum::<usize>()
                != pixels
        {
            return Err(CodecError::Malformed("pixel count"));
        }
        let mut out = Vec::with_capacity(pixels * 4);
        for run in body.as_chunks::<5>().0 {
            for _ in 0..run[0] {
                out.extend_from_slice(&run[1..5]);
            }
        }
        if let Some(base) = base {
            for (o, b) in out.iter_mut().zip(base) {
                *o ^= *b;
            }
        }
        self.previous = Some((width, height, out.clone()));
        self.last_sequence = Some(sequence);
        Ok(Frame {
            width,
            height,
            timestamp_us: frame.timestamp_us,
            bgra: out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pattern_is_deterministic_and_numbered() {
        let mut p = TestPattern::default();
        let a = p.next_frame();
        let b = p.next_frame();
        assert_eq!((a.width, a.height), (320, 180));
        assert!(a.is_well_formed());
        assert_ne!(a.bgra, b.bgra);
        assert_eq!(TestPattern::read_number(&a), Some(0));
        assert_eq!(TestPattern::read_number(&b), Some(1));
        assert_eq!(TestPattern::render(320, 180, 1), b);
        for n in [0, 1, 2, 255, 256, 0xDEAD_BEEF, u32::MAX] {
            let f = TestPattern::render(320, 180, n);
            assert_eq!(TestPattern::read_number(&f), Some(n));
        }
    }

    #[test]
    fn lossless_round_trip_with_key_frames() {
        let mut p = TestPattern::default();
        let mut e = TestEncoder::new(10);
        let mut d = TestDecoder::new();
        for i in 0..35u32 {
            let f = p.next_frame();
            let enc = e.encode(&f, false).unwrap();
            assert_eq!(enc.key, i % 10 == 0, "frame {i}");
            let (got, n) = d.decode_numbered(&enc).unwrap();
            assert_eq!(got, f);
            assert_eq!(n, Some(i));
        }
    }

    #[test]
    fn key_on_request_and_forced() {
        let mut p = TestPattern::default();
        let mut e = TestEncoder::new(0);
        assert!(e.encode(&p.next_frame(), false).unwrap().key);
        assert!(!e.encode(&p.next_frame(), false).unwrap().key);
        e.request_key();
        assert!(e.encode(&p.next_frame(), false).unwrap().key);
        assert!(!e.encode(&p.next_frame(), false).unwrap().key);
        assert!(e.encode(&p.next_frame(), true).unwrap().key);
    }

    #[test]
    fn delta_frames_are_smaller_than_the_picture() {
        let mut p = TestPattern::default();
        let mut e = TestEncoder::new(0);
        let k = e.encode(&p.next_frame(), false).unwrap();
        let d = e.encode(&p.next_frame(), false).unwrap();
        assert!(k.data.len() < 320 * 180 * 4 / 4, "{}", k.data.len());
        assert!(d.data.len() < 320 * 180 * 4 / 4, "{}", d.data.len());
    }

    #[test]
    fn a_lost_frame_is_detected() {
        let mut p = TestPattern::default();
        let mut e = TestEncoder::new(0);
        let mut d = TestDecoder::new();
        let frames: Vec<_> = (0..4)
            .map(|_| e.encode(&p.next_frame(), false).unwrap())
            .collect();
        d.decode(&frames[0]).unwrap();
        d.decode(&frames[1]).unwrap();
        assert_eq!(d.decode(&frames[3]), Err(CodecError::MissingReference));
        // a key frame repairs it
        e.request_key();
        let key = e.encode(&p.next_frame(), false).unwrap();
        assert!(d.decode(&key).is_ok());
        // a delta frame without any key first
        let mut fresh = TestDecoder::new();
        assert_eq!(fresh.decode(&frames[1]), Err(CodecError::MissingReference));
    }

    #[test]
    fn garbage_is_refused_not_trusted() {
        let mut d = TestDecoder::new();
        let mk = |data: Vec<u8>, key: bool| EncodedFrame {
            key,
            timestamp_us: 0,
            data,
        };
        assert!(d.decode(&mk(vec![], true)).is_err());
        assert!(d.decode(&mk(vec![b'X'; 20], true)).is_err());
        let mut hdr = vec![b'T', 0, 0, 0, 0, 0];
        hdr.extend_from_slice(&2u32.to_be_bytes());
        hdr.extend_from_slice(&2u32.to_be_bytes());
        // too few pixels
        let mut few = hdr.clone();
        few.extend_from_slice(&[1, 0, 0, 0, 255]);
        assert!(d.decode(&mk(few, true)).is_err());
        // a run past the end
        let mut over = hdr.clone();
        over.extend_from_slice(&[200, 0, 0, 0, 255]);
        assert!(d.decode(&mk(over, true)).is_err());
        // a huge size does not allocate
        let mut huge = vec![b'T', 0, 0, 0, 0, 0];
        huge.extend_from_slice(&u32::MAX.to_be_bytes());
        huge.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(d.decode(&mk(huge, true)).is_err());
        // kind and key flag must agree
        let mut ok = hdr.clone();
        ok.extend_from_slice(&[4, 1, 2, 3, 255]);
        assert!(d.decode(&mk(ok.clone(), false)).is_err());
        assert!(d.decode(&mk(ok, true)).is_ok());
    }

    #[test]
    fn the_wrong_size_is_refused_by_the_encoder() {
        let mut e = TestEncoder::new(0);
        let f = Frame {
            width: 4,
            height: 4,
            timestamp_us: 0,
            bgra: vec![0; 10],
        };
        assert_eq!(e.encode(&f, false), Err(CodecError::BadFrame));
    }
}
