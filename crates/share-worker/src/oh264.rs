//! H.264 in software on Linux: Cisco's OpenH264 binary, loaded while the worker runs (ADR 0027).
//!
//! The worker builds none of OpenH264. It loads Cisco's own build of the library, and only a
//! build whose SHA-256 the `openh264` crate (BSD-2-Clause) knows, because Cisco's licence for the
//! codec's patents covers that binary and nothing compiled from its source. The library is found
//! at `ANYPLAY_OPENH264`, else beside the worker under Cisco's file name; `ANYPLAY_OPENH264=off`
//! turns it off. Without it this system encodes and decodes nothing, `can_share` and `can_watch`
//! stay false, and `--capabilities` says why (`libraries`).
//!
//! The encoder is the spec's software encoder: at most 720p30, constant bit rate, no B-frames
//! (Baseline profile), one IDR on request and a safety IDR every 600 frames, BT.709 in limited
//! range. Pictures go in through the same BGRA to NV12 fitting as on Windows
//! (`anyplay_share::pixels`), so a Linux stream and a Windows stream look the same.

use std::ffi::{OsString, c_void};
use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use anyplay_share::codec::{
    CodecError, EncodedFrame, Frame, VideoDecoder, VideoEncoder, decoded_size_ok,
};
use anyplay_share::control::CodecCaps;
use anyplay_share::link::{CodecLibrary, LibraryState};
use anyplay_share::pixels;
use openh264::OpenH264API;
use openh264::Timestamp;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, UsageType, VuiConfig,
};
use openh264::formats::{YUVBuffer, YUVSource};
use openh264_sys2::Error as LoadError;

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

/// The value of `ANYPLAY_OPENH264` that turns OpenH264 off: the player's switch in the launcher.
pub const OFF: &str = "off";
/// The library's name in `--capabilities`.
pub const LIBRARY: &str = "openh264";

/// Where the library comes from.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    Off,
    File(PathBuf),
    Missing,
}

/// `ANYPLAY_OPENH264` when it is set and not empty (`off`, or the file), else Cisco's file
/// beside the worker.
fn source_of(env: Option<OsString>, beside: Option<PathBuf>) -> Source {
    match env {
        Some(value) if value == OFF => Source::Off,
        Some(value) if !value.is_empty() => Source::File(value.into()),
        _ => beside
            .filter(|path| path.is_file())
            .map_or(Source::Missing, Source::File),
    }
}

fn source() -> Source {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(library_name())));
    source_of(std::env::var_os(LIBRARY_ENV), beside)
}

fn status(state: LibraryState, detail: Option<String>) -> CodecLibrary {
    CodecLibrary {
        name: LIBRARY.into(),
        state,
        detail,
    }
}

/// Loads the library from an open file, refusing any file that is not a known build of Cisco's.
/// The hash check and the loader both read the file through this process's handle on it
/// (`/proc/self/fd`), so a file put at the same path after it was opened is never loaded.
fn load(file: &File, shown: &Path) -> Result<OpenH264API, CodecLibrary> {
    let held = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
    OpenH264API::from_blob_path(&held).map_err(|e| match e {
        LoadError::InvalidHash(sha256) => status(
            LibraryState::Refused,
            Some(format!(
                "{} is not a known build of Cisco's (SHA-256 {sha256})",
                shown.display()
            )),
        ),
        other => status(
            LibraryState::Failed,
            Some(format!("{}: {other}", shown.display())),
        ),
    })
}

/// The library, or how it stands when there is none to use.
/// The library file that loaded first, kept open for the worker's life and used for every later
/// load. glibc hands back a library it already has by the name it was loaded under, whatever file
/// that name points at now; with this file held, its `/proc/self/fd` name never points at another.
static LOADED: OnceLock<(File, PathBuf)> = OnceLock::new();

fn open_api() -> Result<OpenH264API, CodecLibrary> {
    let path = match source() {
        Source::Off => return Err(status(LibraryState::Off, None)),
        Source::Missing => return Err(status(LibraryState::Missing, None)),
        Source::File(path) => path,
    };
    if let Some((file, shown)) = LOADED.get() {
        return load(file, shown);
    }
    let file = File::open(&path).map_err(|e| {
        let state = if e.kind() == ErrorKind::NotFound {
            LibraryState::Missing
        } else {
            LibraryState::Failed
        };
        status(state, Some(format!("{}: {e}", path.display())))
    })?;
    let api = load(&file, &path)?;
    // Another thread may have loaded first; then this file is dropped, and its library goes with
    // the API made from it.
    let _ = LOADED.set((file, path));
    Ok(api)
}

fn api() -> Result<OpenH264API, String> {
    open_api().map_err(|status| match (status.state, status.detail) {
        (_, Some(detail)) => format!("OpenH264: {detail}"),
        (LibraryState::Off, None) => "OpenH264 is turned off".into(),
        _ => "Cisco's OpenH264 library is not installed".into(),
    })
}

/// What OpenH264 gives on this PC and how its library stands: nothing without the library,
/// else the software encoder's limits and a decoder up to 1080p60.
pub fn capabilities() -> (Vec<CodecCaps>, Vec<CodecCaps>, CodecLibrary) {
    if let Err(status) = open_api() {
        return (Vec::new(), Vec::new(), status);
    }
    let caps = |limits: Limits| CodecCaps {
        codec: CODEC_NAME.into(),
        hardware: false,
        max_width: limits.width,
        max_height: limits.height,
        max_fps: limits.fps,
    };
    (
        vec![caps(Limits::SOFTWARE)],
        vec![caps(Limits::HARDWARE)],
        status(LibraryState::Ready, None),
    )
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
        if !decoded_size_ok(width, height) {
            return Err(CodecError::BadFrame);
        }
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

    #[test]
    fn the_library_comes_from_the_variable_else_beside_the_worker_and_off_turns_it_off() {
        let dir = scratch("source");
        let beside = dir.join(library_name());
        let nowhere = dir.join("nowhere.so");
        std::fs::write(&beside, b"x").unwrap();
        let env = |value: &str| Some(OsString::from(value));

        assert_eq!(source_of(env("off"), Some(beside.clone())), Source::Off);
        assert_eq!(
            source_of(env("/opt/h264.so"), Some(beside.clone())),
            Source::File("/opt/h264.so".into())
        );
        assert_eq!(
            source_of(env(""), Some(beside.clone())),
            Source::File(beside.clone())
        );
        assert_eq!(source_of(None, Some(beside.clone())), Source::File(beside));
        assert_eq!(source_of(None, Some(nowhere)), Source::Missing);
        assert_eq!(source_of(None, None), Source::Missing);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_file_that_is_not_cisco_s_build_is_refused_and_named() {
        let dir = scratch("refused");
        let path = dir.join(library_name());
        std::fs::write(&path, b"not a library").unwrap();
        let refused = load(&File::open(&path).unwrap(), &path)
            .err()
            .expect("an unknown file must not load");
        assert_eq!(refused.name, LIBRARY);
        assert_eq!(refused.state, LibraryState::Refused);
        let detail = refused.detail.unwrap();
        assert!(detail.contains(&path.display().to_string()), "{detail}");
        assert!(
            detail.contains(&sha256_of(&dir, b"not a library")),
            "{detail}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_file_checked_is_the_file_opened_even_when_the_path_is_replaced() {
        let dir = scratch("swap");
        let path = dir.join(library_name());
        std::fs::write(&path, b"the file that was opened").unwrap();
        let opened = File::open(&path).unwrap();
        let other = dir.join("other.so");
        std::fs::write(&other, b"a file put there later").unwrap();
        std::fs::rename(&other, &path).unwrap();

        let detail = load(&opened, &path).err().unwrap().detail.unwrap();
        assert!(
            detail.contains(&sha256_of(&dir, b"the file that was opened")),
            "{detail}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The hash the loader reports for some bytes, read from a file of their own.
    fn sha256_of(dir: &Path, bytes: &[u8]) -> String {
        let path = dir.join("hash-me");
        std::fs::write(&path, bytes).unwrap();
        let detail = load(&File::open(&path).unwrap(), &path)
            .err()
            .unwrap()
            .detail
            .unwrap();
        let sha256 = detail.rsplit("SHA-256 ").next().unwrap();
        sha256.trim_end_matches(')').to_owned()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("anyplay-openh264-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Cisco's library, when this machine has it (`ANYPLAY_OPENH264` or beside the test
    /// binary). The round trip below skips without it, as the Windows one does without an
    /// encoder; with `ANYPLAY_OPENH264` set (CI sets it) a library that does not load fails.
    fn have_library() -> bool {
        match api() {
            Ok(_) => true,
            // Only a file named in the variable must load; empty and `off` skip like no file.
            Err(why)
                if matches!(
                    source_of(std::env::var_os(LIBRARY_ENV), None),
                    Source::File(_)
                ) =>
            {
                panic!("{LIBRARY_ENV} is set, but: {why}")
            }
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
        // Both came from the one file held open, so its name stayed that file's.
        assert!(LOADED.get().is_some(), "the loaded library's file is held");
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
