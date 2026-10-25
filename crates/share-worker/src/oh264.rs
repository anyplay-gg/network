//! H.264 in software on Linux: Cisco's OpenH264 binary, loaded while the worker runs (ADR 0027).
//!
//! The worker builds none of OpenH264. It loads Cisco's own build of the library, and only a
//! build whose SHA-256 the `openh264` crate (BSD-2-Clause) knows, because Cisco's licence for the
//! codec's patents covers that binary and nothing compiled from its source. The library is found
//! at `ANYPLAY_OPENH264`, else beside the worker under Cisco's file name; without it this system
//! encodes and decodes nothing, and `can_share` and `can_watch` stay false.
//!
//! The encoder is the spec's software encoder: at most 720p30, constant bit rate, no B-frames
//! (Baseline profile), one IDR on request and a safety IDR every 600 frames, BT.709 in limited
//! range. Pictures go in through the same BGRA to NV12 fitting as on Windows
//! (`anyplay_share::pixels`), so a Linux stream and a Windows stream look the same.

use std::ffi::c_void;
use std::path::PathBuf;
use std::time::Instant;

use anyplay_share::codec::{CodecError, EncodedFrame, Frame, VideoDecoder, VideoEncoder};
use anyplay_share::control::CodecCaps;
use anyplay_share::pixels;
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::{YUVBuffer, YUVSource};

use crate::limits::{EncoderSetup, Limits};

/// The codec's name in `hello`, `config` and `start`.
pub const CODEC_NAME: &str = "h264";
/// Where the library is when it is not beside the worker.
pub const LIBRARY_ENV: &str = "ANYPLAY_OPENH264";
/// A safety net: an IDR at least this often even if nobody asks (frames).
const GOP_FRAMES: u32 = 600;

/// Cisco's file name for the build this worker loads.
pub fn library_name() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "libopenh264-2.6.0-linux-arm64.8.so"
    } else {
        "libopenh264-2.6.0-linux64.8.so"
    }
}

/// The library to load: `ANYPLAY_OPENH264`, else Cisco's file beside the worker.
pub fn library() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(LIBRARY_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let beside = std::env::current_exe().ok()?.parent()?.join(library_name());
    beside.is_file().then_some(beside)
}

/// Loads the library, refusing any file that is not a known build of Cisco's.
fn api() -> Result<OpenH264API, String> {
    let path = library().ok_or("Cisco's OpenH264 library is not installed")?;
    OpenH264API::from_blob_path(&path).map_err(|e| format!("OpenH264 at {}: {e}", path.display()))
}

/// What OpenH264 gives on this PC: nothing without the library, else the software encoder's
/// limits and a decoder up to 1080p60.
pub fn capabilities() -> (Vec<CodecCaps>, Vec<CodecCaps>) {
    if api().is_err() {
        return (Vec::new(), Vec::new());
    }
    let caps = |limits: Limits| CodecCaps {
        codec: CODEC_NAME.into(),
        hardware: false,
        max_width: limits.width,
        max_height: limits.height,
        max_fps: limits.fps,
    };
    (vec![caps(Limits::SOFTWARE)], vec![caps(Limits::HARDWARE)])
}

/// NV12 (Y, then U and V interleaved) to I420 (Y, then all U, then all V), both tight.
pub fn nv12_to_i420(nv12: &[u8], width: usize, height: usize) -> Vec<u8> {
    let luma = width * height;
    let mut out = Vec::with_capacity(luma * 3 / 2);
    out.extend_from_slice(&nv12[..luma]);
    let (pairs, _) = nv12[luma..luma * 3 / 2].as_chunks::<2>();
    out.extend(pairs.iter().map(|p| p[0]));
    out.extend(pairs.iter().map(|p| p[1]));
    out
}

/// Three I420 planes with their strides to tight NV12. `None` when a plane is too short.
pub fn i420_to_nv12(
    planes: (&[u8], &[u8], &[u8]),
    width: usize,
    height: usize,
    strides: (usize, usize, usize),
) -> Option<Vec<u8>> {
    let (y, u, v) = planes;
    let (cw, ch) = (width / 2, height / 2);
    let fits = |plane: &[u8], stride: usize, w: usize, h: usize| {
        h == 0 || (stride >= w && plane.len() >= stride * (h - 1) + w)
    };
    if !fits(y, strides.0, width, height)
        || !fits(u, strides.1, cw, ch)
        || !fits(v, strides.2, cw, ch)
    {
        return None;
    }
    let mut out = Vec::with_capacity(width * height * 3 / 2);
    for row in 0..height {
        out.extend_from_slice(&y[row * strides.0..row * strides.0 + width]);
    }
    for row in 0..ch {
        let (u, v) = (&u[row * strides.1..], &v[row * strides.2..]);
        for x in 0..cw {
            out.push(u[x]);
            out.push(v[x]);
        }
    }
    Some(out)
}

/// The software H.264 encoder.
pub struct OhEncoder {
    encoder: Encoder,
    setup: EncoderSetup,
    nv12: Vec<u8>,
    /// A clock that does not wrap, for the encoder's rate control.
    started: Instant,
}

impl OhEncoder {
    /// Opens an encoder for `setup` (already lowered to [`Limits::SOFTWARE`]).
    pub fn open(setup: EncoderSetup) -> Result<OhEncoder, String> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(setup.kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(setup.fps as f32))
            .skip_frames(false)
            .profile(Profile::Baseline)
            .intra_frame_period(IntraFramePeriod::from_num_frames(GOP_FRAMES))
            .vui(VuiConfig::bt709());
        let encoder = Encoder::with_api_config(api()?, config)
            .map_err(|e| format!("OpenH264 refused the setup: {e}"))?;
        Ok(OhEncoder {
            encoder,
            setup,
            nv12: Vec::new(),
            started: Instant::now(),
        })
    }

    /// Sets one of the encoder's bit rates (`ENCODER_OPTION_BITRATE` or `_MAX_BITRATE`) for
    /// `layer`.
    fn set_rate(
        &mut self,
        option: openh264_sys2::ENCODER_OPTION,
        layer: openh264_sys2::LAYER_NUM,
        bps: u32,
    ) -> bool {
        let mut info = openh264_sys2::SBitrateInfo {
            iLayer: layer,
            iBitrate: bps.min(i32::MAX as u32) as i32,
        };
        // SAFETY: the encoder is open; the option takes a pointer to an SBitrateInfo, which
        // lives for the call. A bit rate is one of the parameters OpenH264 lets a caller change
        // while it runs.
        unsafe {
            self.encoder
                .raw_api()
                .set_option(option, &mut info as *mut _ as *mut c_void)
                == 0
        }
    }
}

impl VideoEncoder for OhEncoder {
    fn codec(&self) -> &str {
        CODEC_NAME
    }

    fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError> {
        if !frame.is_well_formed() || frame.width == 0 || frame.height == 0 {
            return Err(CodecError::BadFrame);
        }
        let (w, h) = (self.setup.width, self.setup.height);
        pixels::bgra_to_nv12(&frame.bgra, frame.width, frame.height, w, h, &mut self.nv12);
        let yuv = YUVBuffer::from_vec(
            nv12_to_i420(&self.nv12, w as usize, h as usize),
            w as usize,
            h as usize,
        );
        if force_key {
            self.encoder.force_intra_frame();
        }
        let millis = self.started.elapsed().as_millis() as u64;
        let bits = self
            .encoder
            .encode_at(&yuv, Timestamp::from_millis(millis))
            .map_err(|e| CodecError::Failed(e.to_string()))?;
        let kind = bits.frame_type();
        if matches!(kind, FrameType::Skip | FrameType::Invalid) {
            return Err(CodecError::Pending);
        }
        let data = bits.to_vec();
        if data.is_empty() {
            return Err(CodecError::Pending);
        }
        Ok(EncodedFrame {
            key: kind == FrameType::IDR,
            timestamp_us: frame.timestamp_us,
            data,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) {
        use openh264_sys2::{
            ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, SPATIAL_LAYER_ALL,
        };
        let bps = kbps.saturating_mul(1000);
        // OpenH264 refuses a target above the maximum, and keeps a maximum for the whole stream
        // and one for its single layer (the crate sets both to the opening rate). Both move with
        // the target: up before it when raising, down after it when lowering.
        let max = |encoder: &mut Self| {
            encoder.set_rate(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_ALL, bps)
                && encoder.set_rate(ENCODER_OPTION_MAX_BITRATE, SPATIAL_LAYER_0, bps)
        };
        let target =
            |encoder: &mut Self| encoder.set_rate(ENCODER_OPTION_BITRATE, SPATIAL_LAYER_ALL, bps);
        let raising = kbps > self.setup.kbps;
        let raised = !raising || max(self);
        let set = raised && target(self);
        let lowered = set && (raising || max(self));
        if lowered {
            self.setup.kbps = kbps;
        }
    }
}

/// The software H.264 decoder.
pub struct OhDecoder {
    decoder: Decoder,
}

impl OhDecoder {
    pub fn open() -> Result<OhDecoder, String> {
        Decoder::with_api_config(api()?, DecoderConfig::new())
            .map(|decoder| OhDecoder { decoder })
            .map_err(|e| format!("the OpenH264 decoder did not open: {e}"))
    }
}

impl VideoDecoder for OhDecoder {
    fn codec(&self) -> &str {
        CODEC_NAME
    }

    fn decode(&mut self, frame: &EncodedFrame) -> Result<Frame, CodecError> {
        let picture = match self.decoder.decode(&frame.data) {
            Ok(Some(picture)) => picture,
            // The frame was taken in, with no picture out of it yet.
            Ok(None) => return Err(CodecError::Pending),
            // OpenH264 reports a frame it cannot build (its reference is missing or it is cut)
            // as an error; the viewer then asks for a key frame.
            Err(_) => return Err(CodecError::MissingReference),
        };
        let (width, height) = picture.dimensions();
        let nv12 = i420_to_nv12(
            (picture.y(), picture.u(), picture.v()),
            width,
            height,
            picture.strides(),
        )
        .ok_or(CodecError::BadFrame)?;
        let bgra = pixels::nv12_to_bgra(&nv12, width as u32, height as u32, width)
            .ok_or(CodecError::BadFrame)?;
        Ok(Frame {
            width: width as u32,
            height: height as u32,
            timestamp_us: frame.timestamp_us,
            bgra,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_and_i420_turn_into_each_other() {
        // 4 by 2: 8 luma bytes, then 2 chroma pairs.
        let nv12: Vec<u8> = (0..12).collect();
        let i420 = nv12_to_i420(&nv12, 4, 2);
        assert_eq!(i420, [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 9, 11]);
        let back = i420_to_nv12((&i420[..8], &i420[8..10], &i420[10..]), 4, 2, (4, 2, 2));
        assert_eq!(back.as_deref(), Some(&nv12[..]));
    }

    #[test]
    fn planes_with_padding_are_packed_and_short_ones_refused() {
        // Luma stride 6 for a width of 4, chroma stride 3 for a width of 2.
        let y = [1, 2, 3, 4, 0, 0, 5, 6, 7, 8];
        let (u, v) = ([9, 10], [11, 12]);
        let nv12 = i420_to_nv12((&y, &u, &v), 4, 2, (6, 3, 3)).unwrap();
        assert_eq!(nv12, [1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 10, 12]);
        assert!(i420_to_nv12((&y[..9], &u, &v), 4, 2, (6, 3, 3)).is_none());
    }

    /// Cisco's library, when this machine has it (`ANYPLAY_OPENH264` or beside the test
    /// binary). The round trip below skips without it, as the Windows one does without an
    /// encoder.
    fn have_library() -> bool {
        match api() {
            Ok(_) => true,
            Err(why) => {
                eprintln!("skipped: {why}");
                false
            }
        }
    }

    #[test]
    fn the_test_pattern_survives_a_round_trip_through_openh264() {
        if !have_library() {
            return;
        }
        let setup = Limits::SOFTWARE.clamp(&anyplay_share::link::Start {
            epoch: 0,
            codec: CODEC_NAME.into(),
            width: 640,
            height: 360,
            fps: 30,
            kbps: 4000,
            parity_percent: 0,
            max_payload: 0,
        });
        let mut encoder = OhEncoder::open(setup).unwrap();
        let mut decoder = OhDecoder::open().unwrap();
        let mut pattern = anyplay_share::testcodec::TestPattern::default();
        let mut shown = 0;
        let mut keys = 0;
        for n in 0..30 {
            let picture = pattern.next_frame();
            let encoded = match encoder.encode(&picture, n == 0) {
                Ok(encoded) => encoded,
                Err(CodecError::Pending) => continue,
                Err(e) => panic!("encode: {e}"),
            };
            keys += usize::from(encoded.key);
            if n == 0 {
                assert!(encoded.key, "the first frame asked for a key frame");
                assert!(
                    crate::annexb::nal_types(&encoded.data).contains(&7),
                    "an IDR carries its SPS"
                );
            }
            match decoder.decode(&encoded) {
                Ok(out) => {
                    assert_eq!((out.width, out.height), (setup.width, setup.height));
                    shown += 1;
                }
                Err(CodecError::Pending) => {}
                Err(e) => panic!("decode of frame {n}: {e}"),
            }
        }
        assert!(shown >= 25, "only {shown} of 30 pictures came out");
        assert_eq!(keys, 1, "one key frame unless asked");
        // Above the opening rate: the encoder takes it (its maximum moves with it).
        encoder.set_bitrate(8000);
        assert_eq!(encoder.setup.kbps, 8000);
        encoder.set_bitrate(1000);
        assert_eq!(encoder.setup.kbps, 1000);
    }
}
