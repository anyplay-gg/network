//! H.264 through the system on macOS: a VideoToolbox compression session.
//!
//! The operating system ships the encoder (the media engine of Apple silicon, or Apple's own
//! software encoder), so the worker carries no codec code and owes no codec licence of its own
//! (ADR 0036, which extends ADR 0027). Settings as on Windows: low-latency real-time mode, no
//! B-frames, High profile, a key frame on request and every [`GOP_FRAMES`] frames, SDR BT.709 in
//! limited range. The encoder speaks Annex B, the form the rest of the share engine cuts into
//! datagrams: VideoToolbox's length-prefixed output is rewritten with start codes, and a key
//! frame carries its SPS and PPS in front.
//!
//! A picture goes in as NV12 made by `anyplay_share::pixels` (the same code, and so the same
//! colours and black bars, as on Windows). The session calls back on a thread of its own; the
//! callback leaves the finished frames in [`Shared`] and `encode` takes the oldest.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use anyplay_share::codec::{CodecError, EncodedFrame, Frame, VideoEncoder};
use anyplay_share::control::CodecCaps;
use anyplay_share::pixels;
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    kCMFormatDescriptionColorPrimaries_ITU_R_709_2, kCMFormatDescriptionTransferFunction_ITU_R_709_2,
    kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2, kCMVideoCodecType_H264,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_H264_Main_AutoLevel,
};

use crate::annexb::nal_types;
use crate::limits::{EncoderSetup, Limits};

/// The codec's name in `hello`, `config` and `start`.
pub const CODEC_NAME: &str = "h264";

/// A safety net: a key frame at least this often even if nobody asks (frames).
const GOP_FRAMES: i32 = 600;
/// How long an encode call waits for the encoder's answer before it returns
/// [`CodecError::Pending`].
const ANSWER: Duration = Duration::from_millis(60);
/// The clock of the presentation times: microseconds.
const TIMESCALE: i32 = 1_000_000;

/// Which encoder to ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Only the media engine; opening fails where there is none.
    Hardware,
    /// Apple's software encoder.
    Software,
}

/// What the callback leaves for `encode`.
#[derive(Default)]
struct Out {
    frames: VecDeque<EncodedFrame>,
    failed: Option<String>,
}

/// Shared between the encoder and the session's callback thread.
#[derive(Default)]
struct Shared {
    out: Mutex<Out>,
    arrived: Condvar,
    /// The capture time of the first picture; a frame's time is this plus its presentation time.
    base_stamp: AtomicU32,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Out> {
        self.out.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The H.264 encoder of VideoToolbox.
pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    shared: Arc<Shared>,
    /// One strong count of `shared`, held for the callback; given back in `drop`.
    refcon: *const Shared,
    setup: EncoderSetup,
    kind: Kind,
    nv12: Vec<u8>,
    /// Time on a clock that does not wrap, in microseconds from the first picture.
    clock: i64,
    last_stamp: Option<u32>,
}

/// A dictionary of CF strings to CF values.
fn dictionary(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<&CFString> = pairs.iter().map(|p| p.0).collect();
    let values: Vec<&CFType> = pairs.iter().map(|p| p.1).collect();
    CFDictionary::from_slices(&keys, &values)
}

fn set_property(session: &VTCompressionSession, key: &CFString, value: &CFType) -> bool {
    // SAFETY: the session is alive (we hold it) and key and value are CF objects.
    unsafe { VTSessionSetProperty(session, key, Some(value)) == 0 }
}

fn number(value: i32) -> CFRetained<CFNumber> {
    CFNumber::new_i32(value)
}

fn time(value: i64) -> CMTime {
    CMTime {
        value,
        timescale: TIMESCALE,
        flags: CMTimeFlags::Valid,
        epoch: 0,
    }
}

impl VtEncoder {
    /// Opens an encoder for `setup`. Fails where the system has no encoder of this kind.
    pub fn open(setup: EncoderSetup, kind: Kind) -> Result<VtEncoder, String> {
        // Low-latency rate control first (it keeps the delay of a game stream down); an encoder
        // that does not know it gets the plain real-time mode.
        Self::open_with(setup, kind, true).or_else(|why| {
            Self::open_with(setup, kind, false)
                .map_err(|plain| format!("{why}; without low latency: {plain}"))
        })
    }

    fn open_with(setup: EncoderSetup, kind: Kind, low_latency: bool) -> Result<VtEncoder, String> {
        let yes = CFBoolean::new(true);
        let no = CFBoolean::new(false);
        let mut pairs: Vec<(&CFString, &CFType)> = Vec::new();
        // SAFETY: these are the framework's own constants.
        unsafe {
            match kind {
                Kind::Hardware => {
                    pairs.push((
                        kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                        yes,
                    ));
                    pairs.push((
                        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
                        yes,
                    ));
                }
                Kind::Software => pairs.push((
                    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
                    no,
                )),
            }
            if low_latency {
                pairs.push((kVTVideoEncoderSpecification_EnableLowLatencyRateControl, yes));
            }
        }
        let specification = dictionary(&pairs);

        let shared = Arc::new(Shared::default());
        let refcon = Arc::into_raw(shared.clone());
        let mut raw: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: `raw` is a valid out pointer; the callback and `refcon` stay valid until the
        // session is invalidated in `drop`.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                setup.width as i32,
                setup.height as i32,
                kCMVideoCodecType_H264,
                Some(specification.as_opaque()),
                None,
                None,
                Some(on_output),
                refcon as *mut c_void,
                NonNull::from(&mut raw),
            )
        };
        let release = |refcon: *const Shared| {
            // SAFETY: `refcon` came from `Arc::into_raw` above and nobody else frees it.
            drop(unsafe { Arc::from_raw(refcon) });
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            release(refcon);
            return Err(format!(
                "VideoToolbox has no {} H.264 encoder for this setup (status {status})",
                if kind == Kind::Hardware { "hardware" } else { "software" }
            ));
        };
        // SAFETY: Create returns a session we own (+1).
        let session = unsafe { CFRetained::from_raw(raw) };

        // SAFETY: the framework's own constants.
        unsafe {
            set_property(&session, kVTCompressionPropertyKey_RealTime, yes);
            set_property(&session, kVTCompressionPropertyKey_AllowFrameReordering, no);
            set_property(
                &session,
                kVTCompressionPropertyKey_MaxKeyFrameInterval,
                &number(GOP_FRAMES),
            );
            set_property(
                &session,
                kVTCompressionPropertyKey_ExpectedFrameRate,
                &number(setup.fps.max(1) as i32),
            );
            // High profile; Main where the encoder refuses it.
            if !set_property(
                &session,
                kVTCompressionPropertyKey_ProfileLevel,
                kVTProfileLevel_H264_High_AutoLevel,
            ) {
                set_property(
                    &session,
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_H264_Main_AutoLevel,
                );
            }
            set_property(
                &session,
                kVTCompressionPropertyKey_ColorPrimaries,
                kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
            );
            set_property(
                &session,
                kVTCompressionPropertyKey_TransferFunction,
                kCMFormatDescriptionTransferFunction_ITU_R_709_2,
            );
            set_property(
                &session,
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
            );
        }
        set_rate(&session, setup.kbps);
        // SAFETY: the session is alive.
        let prepared = unsafe { session.prepare_to_encode_frames() };
        if prepared != 0 {
            // SAFETY: the session is alive and not used after this.
            unsafe { session.invalidate() };
            release(refcon);
            return Err(format!("VideoToolbox did not prepare the encoder (status {prepared})"));
        }
        Ok(VtEncoder {
            session,
            shared,
            refcon,
            setup,
            kind,
            nv12: Vec::new(),
            clock: 0,
            last_stamp: None,
        })
    }

    /// The first hardware encoder, else Apple's software one.
    pub fn open_best(setup: EncoderSetup) -> Result<VtEncoder, String> {
        Self::open(setup, Kind::Hardware).or_else(|hardware| {
            Self::open(setup, Kind::Software)
                .map_err(|software| format!("hardware: {hardware}; software: {software}"))
        })
    }

    pub fn is_hardware(&self) -> bool {
        self.kind == Kind::Hardware
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            Kind::Hardware => "VideoToolbox (media engine)",
            Kind::Software => "VideoToolbox (software)",
        }
    }

    /// An NV12 pixel buffer filled from `self.nv12`.
    fn picture(&self) -> Result<CFRetained<CVPixelBuffer>, CodecError> {
        let (w, h) = (self.setup.width as usize, self.setup.height as usize);
        // IOSurface-backed, so that the media engine reads it without a copy.
        let surface = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: the framework's own constant.
        let attributes = unsafe {
            dictionary(&[(kCVPixelBufferIOSurfacePropertiesKey, &surface)])
        };
        let mut raw: *mut CVPixelBuffer = ptr::null_mut();
        // SAFETY: `raw` is a valid out pointer.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                w,
                h,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                Some(attributes.as_opaque()),
                NonNull::from(&mut raw),
            )
        };
        let Some(raw) = NonNull::new(raw).filter(|_| status == 0) else {
            return Err(CodecError::Failed(format!(
                "no picture buffer (status {status})"
            )));
        };
        // SAFETY: Create returns a buffer we own (+1).
        let buffer = unsafe { CFRetained::from_raw(raw) };
        // SAFETY: the buffer is locked for the copy and unlocked after; each plane is written
        // within its rows (`stride` bytes apart, at least `w` wide) and the heights below.
        unsafe {
            if CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0)) != 0 {
                return Err(CodecError::Failed("the picture buffer did not lock".into()));
            }
            let luma = CVPixelBufferGetBaseAddressOfPlane(&buffer, 0) as *mut u8;
            let chroma = CVPixelBufferGetBaseAddressOfPlane(&buffer, 1) as *mut u8;
            let luma_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 0);
            let chroma_stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, 1);
            if luma.is_null() || chroma.is_null() || luma_stride < w || chroma_stride < w {
                CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
                return Err(CodecError::Failed("the picture buffer has no planes".into()));
            }
            for row in 0..h {
                ptr::copy_nonoverlapping(
                    self.nv12.as_ptr().add(row * w),
                    luma.add(row * luma_stride),
                    w,
                );
            }
            let chroma_source = self.nv12.as_ptr().add(w * h);
            for row in 0..h / 2 {
                ptr::copy_nonoverlapping(
                    chroma_source.add(row * w),
                    chroma.add(row * chroma_stride),
                    w,
                );
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        }
        Ok(buffer)
    }

    /// The oldest finished frame, waiting a moment for one.
    fn take(&mut self) -> Result<EncodedFrame, CodecError> {
        let mut out = self.shared.lock();
        if out.frames.is_empty() && out.failed.is_none() {
            out = self
                .shared
                .arrived
                .wait_timeout(out, ANSWER)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if let Some(why) = out.failed.take() {
            return Err(CodecError::Failed(why));
        }
        out.frames.pop_front().ok_or(CodecError::Pending)
    }
}

/// Sets the mean rate, and a ceiling of one and a half times it over one second, so that a
/// busy scene cannot flood the link.
fn set_rate(session: &VTCompressionSession, kbps: u32) {
    let bits = (kbps.max(100) as i64 * 1000).min(i64::from(i32::MAX)) as i32;
    // SAFETY: the framework's own constants.
    unsafe {
        set_property(session, kVTCompressionPropertyKey_AverageBitRate, &number(bits));
        let bytes_per_second = (i64::from(bits) / 8 * 3 / 2).min(i64::from(i32::MAX)) as i32;
        let limits = CFArray::<CFNumber>::from_retained_objects(&[number(bytes_per_second), number(1)]);
        set_property(session, kVTCompressionPropertyKey_DataRateLimits, &limits);
    }
}

impl VideoEncoder for VtEncoder {
    fn codec(&self) -> &str {
        CODEC_NAME
    }

    fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError> {
        if !frame.is_well_formed() || frame.width == 0 || frame.height == 0 {
            return Err(CodecError::BadFrame);
        }
        let setup = self.setup;
        pixels::bgra_to_nv12(
            &frame.bgra,
            frame.width,
            frame.height,
            setup.width,
            setup.height,
            &mut self.nv12,
        );
        match self.last_stamp {
            None => {
                self.shared
                    .base_stamp
                    .store(frame.timestamp_us, Ordering::SeqCst);
                self.clock = 0;
            }
            Some(last) => {
                let step = i64::from(frame.timestamp_us.wrapping_sub(last) as i32);
                self.clock += step.max(1);
            }
        }
        self.last_stamp = Some(frame.timestamp_us);

        let buffer = self.picture()?;
        let yes = CFBoolean::new(true);
        // SAFETY: the framework's own constant.
        let options = force_key
            .then(|| unsafe { dictionary(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, yes)]) });
        let mut info = VTEncodeInfoFlags::empty();
        // SAFETY: the session and the buffer are alive; the call copies what it needs.
        let status = unsafe {
            self.session.encode_frame(
                &buffer,
                time(self.clock),
                time(i64::from(TIMESCALE) / i64::from(setup.fps.max(1))),
                options.as_ref().map(|o| o.as_opaque()),
                ptr::null_mut(),
                &mut info,
            )
        };
        if status != 0 {
            return Err(CodecError::Failed(format!(
                "VideoToolbox refused a picture (status {status})"
            )));
        }
        self.take()
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.setup.kbps = kbps;
        set_rate(&self.session, kbps);
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // SAFETY: after the invalidate no callback runs, so the count held for it can go.
        unsafe {
            self.session.invalidate();
            drop(Arc::from_raw(self.refcon));
        }
    }
}

/// The session's callback: a finished frame (or none, when the encoder dropped it).
unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is the pointer `open_with` passed, which lives until `drop`.
    let shared = unsafe { &*(refcon as *const Shared) };
    if status != 0 {
        shared.lock().failed = Some(format!("VideoToolbox failed a picture (status {status})"));
        shared.arrived.notify_all();
        return;
    }
    if flags.contains(VTEncodeInfoFlags::FrameDropped) {
        shared.arrived.notify_all();
        return;
    }
    // SAFETY: the sample is valid for the duration of the callback.
    let Some(sample) = (unsafe { sample.as_ref() }) else {
        shared.arrived.notify_all();
        return;
    };
    match unsafe { annex_b(sample) } {
        Ok((key, data)) => {
            // SAFETY: plain accessor on a valid sample.
            let pts = unsafe { sample.presentation_time_stamp() };
            let stamp = shared
                .base_stamp
                .load(Ordering::SeqCst)
                .wrapping_add(pts.value as u32);
            shared.lock().frames.push_back(EncodedFrame {
                key,
                timestamp_us: stamp,
                data,
            });
        }
        Err(why) => shared.lock().failed = Some(why),
    }
    shared.arrived.notify_all();
}

/// One sample as Annex B, and whether it is a key frame. VideoToolbox gives NAL units after
/// 4-byte lengths and keeps the SPS and PPS in the sample's format description; a key frame is
/// written with both in front, as the Windows encoder's output has them.
///
/// # Safety
///
/// `sample` must be a valid sample buffer from a compression session.
unsafe fn annex_b(sample: &CMSampleBuffer) -> Result<(bool, Vec<u8>), String> {
    // SAFETY: the accessors take a valid sample.
    let block = unsafe { sample.data_buffer() }.ok_or("a sample without data")?;
    let (mut available, mut total) = (0usize, 0usize);
    let mut at: *mut c_char = ptr::null_mut();
    // SAFETY: the out pointers are valid.
    let status = unsafe { block.data_pointer(0, &mut available, &mut total, &mut at) };
    if status != 0 || at.is_null() || available != total {
        return Err(format!("a sample in pieces (status {status})"));
    }
    // SAFETY: the block holds `total` bytes at `at` while `block` lives.
    let avcc = unsafe { slice::from_raw_parts(at as *const u8, total) };

    // The NAL units behind their lengths.
    let mut header = 4usize;
    let mut sets: Vec<Vec<u8>> = Vec::new();
    // SAFETY: the format description of a valid sample.
    if let Some(description) = unsafe { sample.format_description() } {
        let mut count = 0usize;
        let mut length = 0i32;
        // SAFETY: the out pointers are valid; index 0 reports the count.
        let first = unsafe {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                &description,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut count,
                &mut length,
            )
        };
        if first == 0 {
            if (1..=4).contains(&length) {
                header = length as usize;
            }
            for index in 0..count {
                let (mut set, mut size) = (ptr::null::<u8>(), 0usize);
                // SAFETY: the out pointers are valid; the set lives with the description.
                let status = unsafe {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &description,
                        index,
                        &mut set,
                        &mut size,
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                };
                if status == 0 && !set.is_null() {
                    // SAFETY: `size` bytes at `set`, as the call says.
                    sets.push(unsafe { slice::from_raw_parts(set, size) }.to_vec());
                }
            }
        }
    }

    let mut body = Vec::with_capacity(avcc.len() + 16);
    let mut position = 0usize;
    while position + header <= avcc.len() {
        let length = avcc[position..position + header]
            .iter()
            .fold(0usize, |n, b| n << 8 | usize::from(*b));
        position += header;
        let end = position
            .checked_add(length)
            .filter(|end| *end <= avcc.len())
            .ok_or("a NAL unit longer than its sample")?;
        body.extend_from_slice(&[0, 0, 0, 1]);
        body.extend_from_slice(&avcc[position..end]);
        position = end;
    }
    let key = nal_types(&body).contains(&5);
    if !key {
        return Ok((false, body));
    }
    let mut data = Vec::with_capacity(body.len() + 64);
    for set in &sets {
        data.extend_from_slice(&[0, 0, 0, 1]);
        data.extend_from_slice(set);
    }
    data.extend_from_slice(&body);
    Ok((true, data))
}

/// What the system's encoders give: a hardware encoder up to 1080p60 where the media engine
/// answers, else Apple's software encoder at 720p30, else none. The decoder side is empty: the
/// viewer's window and decoder are not built for macOS yet.
pub fn capabilities() -> (Vec<CodecCaps>, Vec<CodecCaps>) {
    let probe = |limits: Limits, kind: Kind| {
        let setup = EncoderSetup {
            width: limits.width.min(1280),
            height: limits.height.min(720),
            fps: limits.fps.min(30),
            kbps: 4000,
        };
        VtEncoder::open(setup, kind).is_ok()
    };
    let caps = |limits: Limits, hardware: bool| CodecCaps {
        codec: CODEC_NAME.into(),
        hardware,
        max_width: limits.width,
        max_height: limits.height,
        max_fps: limits.fps,
    };
    let encode = if probe(Limits::HARDWARE, Kind::Hardware) {
        vec![caps(Limits::HARDWARE, true)]
    } else if probe(Limits::SOFTWARE, Kind::Software) {
        vec![caps(Limits::SOFTWARE, false)]
    } else {
        Vec::new()
    };
    (encode, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annexb::nal_types;
    use anyplay_share::testcodec::TestPattern;

    fn setup() -> EncoderSetup {
        EncoderSetup {
            width: 640,
            height: 360,
            fps: 30,
            kbps: 2000,
        }
    }

    /// Opens an encoder or says why the machine has none: a CI runner is a virtual machine and
    /// may have no media engine, which is not a failure of the code.
    fn encoder() -> Option<VtEncoder> {
        match VtEncoder::open_best(setup()) {
            Ok(encoder) => Some(encoder),
            Err(why) => {
                eprintln!("skipped: {why}");
                None
            }
        }
    }

    #[test]
    fn the_first_picture_comes_out_as_a_key_frame_with_its_parameter_sets() {
        let Some(mut encoder) = encoder() else { return };
        let mut pattern = TestPattern::default();
        let mut first = None;
        for n in 0..90 {
            let mut frame = pattern.next_frame();
            frame.timestamp_us = 1000 + n * 33_333;
            match encoder.encode(&frame, n == 0) {
                Ok(done) => {
                    first = Some(done);
                    break;
                }
                Err(CodecError::Pending) => {}
                Err(other) => panic!("{other}"),
            }
        }
        let first = first.expect("a frame within 90 pictures");
        assert!(first.key, "the first frame is a key frame");
        let types = nal_types(&first.data);
        assert!(types.starts_with(&[7, 8]), "SPS and PPS first: {types:?}");
        assert!(types.contains(&5), "an IDR picture: {types:?}");
        assert_eq!(&first.data[..4], &[0, 0, 0, 1]);
    }

    #[test]
    fn later_pictures_are_delta_frames_until_a_key_is_asked_for() {
        let Some(mut encoder) = encoder() else { return };
        let mut pattern = TestPattern::default();
        let mut keys = Vec::new();
        for n in 0..120u32 {
            let mut frame = pattern.next_frame();
            frame.timestamp_us = n * 33_333;
            if let Ok(done) = encoder.encode(&frame, n == 0 || n == 60) {
                keys.push(done.key);
            }
        }
        assert!(keys.len() > 60, "most pictures come out: {}", keys.len());
        assert!(keys[0]);
        assert!(keys.iter().filter(|k| **k).count() >= 2, "the second key frame came");
        assert!(keys.iter().any(|k| !*k), "and delta frames in between");
    }

    #[test]
    fn a_picture_of_another_size_is_fitted() {
        let Some(mut encoder) = encoder() else { return };
        let frame = Frame {
            width: 200,
            height: 100,
            timestamp_us: 0,
            bgra: vec![128; 200 * 100 * 4],
        };
        let mut done = None;
        for n in 0..60 {
            let mut f = Frame {
                bgra: frame.bgra.clone(),
                ..frame
            };
            f.timestamp_us = n * 33_333;
            if let Ok(out) = encoder.encode(&f, n == 0) {
                done = Some(out);
                break;
            }
        }
        assert!(done.is_some_and(|d| d.key));
    }

    #[test]
    fn the_rate_can_change_while_it_runs() {
        let Some(mut encoder) = encoder() else { return };
        encoder.set_bitrate(500);
        encoder.set_bitrate(8000);
    }
}
