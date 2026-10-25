//! H.264 through the system: the encoders and decoders of Windows Media Foundation.
//!
//! The operating system ships them, so the worker carries no codec code and owes no codec
//! licence of its own (ADR 0027). The encoder is the first hardware encoder the system offers
//! (the GPU vendors register theirs as Media Foundation transforms: NVENC, AMF, Quick Sync),
//! else the system's own software encoder; the decoder is the system's H.264 decoder. Both
//! speak Annex B, the form the rest of the share engine cuts into datagrams.
//!
//! Everything here runs on the thread that made the object; none of it is `Send`.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::Once;
use std::time::{Duration, Instant};

use anyplay_share::codec::{CodecError, EncodedFrame, Frame, VideoDecoder, VideoEncoder};
use anyplay_share::control::CodecCaps;
use anyplay_share::pixels;
use windows::Win32::Media::MediaFoundation::{
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVDefaultBPictureCount,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode,
    ICodecAPI, IMFActivate, IMFAttributes, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, MF_E_NO_EVENTS_AVAILABLE, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_LOW_LATENCY, MF_MT_AVG_BITRATE,
    MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_MPEG_SEQUENCE_HEADER,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION,
    MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_TRANSFORM_ASYNC,
    MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFNominalRange_16_235, MFSTARTUP_FULL, MFSampleExtension_CleanPoint,
    MFStartup, MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, eAVEncH264VProfile_Base, eAVEncH264VProfile_High,
    eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface};

/// The codec's name in `hello`, `config` and `start`.
pub const CODEC_NAME: &str = "h264";

/// A safety net: an IDR at least this often even if nobody asks (frames).
const GOP_FRAMES: u32 = 600;
/// How long an encoder gets to take a picture or give one back before it counts as stuck.
const STUCK: Duration = Duration::from_secs(2);
/// How long an encode call waits for the encoder's answer before it returns [`CodecError::Pending`].
const ANSWER: Duration = Duration::from_millis(40);

/// Media Foundation and COM, once for the process; COM again for every thread.
fn start() -> Result<(), String> {
    static ONCE: Once = Once::new();
    static mut RESULT: Option<String> = None;
    // SAFETY: plain calls; the thread's COM state is set up by CoInitializeEx and left alone
    // (an apartment that is already set up by someone else is fine for Media Foundation).
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        ONCE.call_once(|| {
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                RESULT = Some(format!("Media Foundation did not start: {error}"));
            }
        });
        #[allow(static_mut_refs)]
        match &RESULT {
            Some(why) => Err(why.clone()),
            None => Ok(()),
        }
    }
}

fn pack(high: u32, low: u32) -> u64 {
    u64::from(high) << 32 | u64::from(low)
}

fn text(error: windows::core::Error) -> String {
    error.to_string().trim().to_owned()
}

/// A list of transforms the system offers for a category, best first.
fn enumerate(
    category: GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<GUID>,
    output: Option<GUID>,
) -> Vec<IMFActivate> {
    let info = |subtype: GUID| MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };
    let (input, output) = (input.map(info), output.map(info));
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: the out parameters are valid; the array MFTEnumEx allocates is read once and freed
    // with CoTaskMemFree, and every activate in it is moved out exactly once.
    unsafe {
        let found = MFTEnumEx(
            category,
            flags,
            input.as_ref().map(|i| i as *const _),
            output.as_ref().map(|o| o as *const _),
            &mut list,
            &mut count,
        );
        if found.is_err() || list.is_null() {
            return Vec::new();
        }
        let mut activates = Vec::with_capacity(count as usize);
        for n in 0..count as usize {
            if let Some(activate) = (*list.add(n)).take() {
                activates.push(activate);
            }
        }
        CoTaskMemFree(Some(list as *const _));
        activates
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut buffer = [0u16; 128];
    let mut length = 0u32;
    // SAFETY: the buffer is as long as the call is told.
    unsafe {
        if activate
            .GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buffer, Some(&mut length))
            .is_ok()
        {
            return String::from_utf16_lossy(&buffer[..(length as usize).min(buffer.len())]);
        }
    }
    "an H.264 encoder".into()
}

fn variant_u32(value: u32) -> VARIANT {
    VARIANT::from(value)
}

fn set_api(api: &Option<ICodecAPI>, key: &GUID, value: VARIANT) -> bool {
    match api {
        // SAFETY: both pointers are valid for the call.
        Some(api) => unsafe { api.SetValue(key, &value) }.is_ok(),
        None => false,
    }
}

// ---------------------------------------------------------------------------------------------
// Annex B

/// Position of the first start code (`00 00 01`) at or after `from`.
fn find_start(data: &[u8], from: usize) -> Option<usize> {
    data.get(from..)?
        .windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|p| p + from)
}

/// The types of the NAL units in an Annex B stream.
pub fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut at = find_start(data, 0);
    while let Some(start) = at {
        if let Some(byte) = data.get(start + 3) {
            types.push(byte & 0x1f);
        }
        at = find_start(data, start + 3);
    }
    types
}

// ---------------------------------------------------------------------------------------------
// The encoder

/// What the encoder is asked to produce.
#[derive(Clone, Copy, Debug)]
pub struct EncoderSetup {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub kbps: u32,
}

/// The most an encoder is asked for.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Limits {
    /// A hardware encoder: 1080p60.
    pub const HARDWARE: Limits = Limits {
        width: 1920,
        height: 1080,
        fps: 60,
    };
    /// The system's software encoder: 720p30, so that the game keeps its processor.
    pub const SOFTWARE: Limits = Limits {
        width: 1280,
        height: 720,
        fps: 30,
    };

    /// The setup of a stream start lowered to these limits, keeping the shape and even sizes.
    pub fn clamp(self, start: &anyplay_share::link::Start) -> EncoderSetup {
        let (mut w, mut h) = (start.width.max(2), start.height.max(2));
        if w > self.width || h > self.height {
            let scale =
                (f64::from(self.width) / f64::from(w)).min(f64::from(self.height) / f64::from(h));
            w = (f64::from(w) * scale) as u32;
            h = (f64::from(h) * scale) as u32;
        }
        EncoderSetup {
            width: (w & !1).max(2),
            height: (h & !1).max(2),
            fps: start.fps.clamp(1, self.fps),
            kbps: start.kbps,
        }
    }
}

/// Which kind of encoder to open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hardware,
    Software,
}

/// An H.264 encoder from the system.
pub struct MfEncoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    api: Option<ICodecAPI>,
    setup: EncoderSetup,
    hardware: bool,
    name: String,
    /// The SPS and PPS the encoder announced, in Annex B, for key frames that lack them.
    header: Vec<u8>,
    provides_samples: bool,
    output_size: u32,
    /// Inputs the asynchronous encoder is ready for.
    credits: u32,
    /// Outputs the asynchronous encoder announced.
    announced: u32,
    ready: VecDeque<EncodedFrame>,
    nv12: Vec<u8>,
    /// Microseconds on a clock that does not wrap, for the encoder's rate control.
    clock: i64,
    last_stamp: Option<u32>,
    sent: u64,
    /// A key frame was asked for and has not come out yet.
    forced: bool,
}

impl MfEncoder {
    /// Opens the first encoder of `kind` that accepts the setup.
    pub fn open(setup: EncoderSetup, kind: Kind) -> Result<MfEncoder, String> {
        start()?;
        let flags = match kind {
            Kind::Hardware => MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Kind::Software => MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        };
        let activates = enumerate(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(MFVideoFormat_H264),
        );
        if activates.is_empty() {
            return Err(match kind {
                Kind::Hardware => "this PC has no hardware H.264 encoder".into(),
                Kind::Software => "this PC has no H.264 encoder".into(),
            });
        }
        let mut last = String::new();
        for activate in &activates {
            match MfEncoder::from_activate(activate, setup, kind == Kind::Hardware) {
                Ok(encoder) => return Ok(encoder),
                Err(why) => last = format!("{}: {why}", friendly_name(activate)),
            }
        }
        Err(last)
    }

    /// The best encoder this PC has: a hardware one, else the system's software one.
    pub fn open_best(setup: EncoderSetup) -> Result<MfEncoder, String> {
        MfEncoder::open(setup, Kind::Hardware).or_else(|hardware| {
            MfEncoder::open(setup, Kind::Software)
                .map_err(|software| format!("hardware: {hardware}; software: {software}"))
        })
    }

    fn from_activate(
        activate: &IMFActivate,
        setup: EncoderSetup,
        hardware: bool,
    ) -> Result<MfEncoder, String> {
        if setup.width < 2
            || setup.height < 2
            || !setup.width.is_multiple_of(2)
            || !setup.height.is_multiple_of(2)
        {
            return Err("the size must be even".into());
        }
        let name = friendly_name(activate);
        // SAFETY: Media Foundation calls on objects this function owns; every pointer passed
        // is to a local that lives through the call.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(text)?;
            let attributes = transform.GetAttributes().ok();
            let asynchronous = attributes
                .as_ref()
                .is_some_and(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0);
            if asynchronous && let Some(a) = &attributes {
                a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).map_err(text)?;
            }
            let api = transform.cast::<ICodecAPI>().ok();
            set_api(&api, &CODECAPI_AVLowLatencyMode, VARIANT::from(true));
            // 0 is constant bit rate.
            set_api(&api, &CODECAPI_AVEncCommonRateControlMode, variant_u32(0));
            set_api(&api, &CODECAPI_AVEncMPVDefaultBPictureCount, variant_u32(0));
            set_api(&api, &CODECAPI_AVEncMPVGOPSize, variant_u32(GOP_FRAMES));
            set_api(
                &api,
                &CODECAPI_AVEncCommonMeanBitRate,
                variant_u32(setup.kbps.max(100) * 1000),
            );
            // About one second of the rate as the buffer keeps the picture steady; two frames
            // keeps the delay low. A frame and a half is the compromise.
            let buffer_bits = setup.kbps.max(100) * 1000 / setup.fps.max(1) * 3 / 2;
            set_api(
                &api,
                &CODECAPI_AVEncCommonBufferSize,
                variant_u32(buffer_bits),
            );

            let mut chosen = Err("no profile accepted".to_owned());
            for profile in [
                eAVEncH264VProfile_High,
                eAVEncH264VProfile_Main,
                eAVEncH264VProfile_Base,
            ] {
                let output = media_type(&MFVideoFormat_H264, setup)?;
                output
                    .SetUINT32(&MF_MT_AVG_BITRATE, setup.kbps.max(100) * 1000)
                    .map_err(text)?;
                output
                    .SetUINT32(&MF_MT_MPEG2_PROFILE, profile.0 as u32)
                    .map_err(text)?;
                match transform.SetOutputType(0, &output, 0) {
                    Ok(()) => {
                        chosen = Ok(());
                        break;
                    }
                    Err(error) => chosen = Err(text(error)),
                }
            }
            chosen?;
            // The input: NV12 of the same size. Some encoders list their types only now.
            let input = media_type(&MFVideoFormat_NV12, setup)?;
            transform.SetInputType(0, &input, 0).map_err(text)?;

            // The announced parameter sets, if the encoder has them already.
            let header = sequence_header(&transform);

            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(text)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(text)?;
            let info = transform.GetOutputStreamInfo(0).map_err(text)?;
            let events = if asynchronous {
                transform.cast::<IMFMediaEventGenerator>().ok()
            } else {
                None
            };
            Ok(MfEncoder {
                transform,
                events,
                api,
                setup,
                hardware,
                name,
                header,
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                output_size: info.cbSize,
                credits: 0,
                announced: 0,
                ready: VecDeque::new(),
                nv12: Vec::new(),
                clock: 0,
                last_stamp: None,
                sent: 0,
                forced: false,
            })
        }
    }

    /// `true` for an encoder on the graphics card.
    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// The encoder's name as the system gives it, for logs.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Reads the events of an asynchronous encoder: input wanted, output ready.
    fn pump(&mut self, wait: Duration) -> Result<(), CodecError> {
        let Some(events) = self.events.clone() else {
            return Ok(());
        };
        let deadline = Instant::now() + wait;
        loop {
            // SAFETY: plain call on an interface this object owns.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    // SAFETY: plain call on the event just received.
                    match unsafe { event.GetType() } {
                        Ok(601) => self.credits += 1,
                        Ok(602) => self.announced += 1,
                        _ => {}
                    }
                }
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if self.announced > 0 || Instant::now() >= deadline {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(CodecError::Failed(text(error))),
            }
        }
    }

    /// Takes every output the encoder has now into `ready`.
    fn collect(&mut self) -> Result<(), CodecError> {
        loop {
            if self.events.is_some() {
                if self.announced == 0 {
                    return Ok(());
                }
                self.announced -= 1;
            }
            match self.take_output()? {
                Some(frame) => self.ready.push_back(frame),
                None => return Ok(()),
            }
        }
    }

    fn take_output(&mut self) -> Result<Option<EncodedFrame>, CodecError> {
        // SAFETY: the output buffer holds a sample this call owns or none; after the call the
        // sample (if any) is moved out exactly once and the unused event collection is dropped.
        unsafe {
            let own = if self.provides_samples {
                None
            } else {
                Some(
                    sample_with_buffer(self.output_size.max(1 << 20))
                        .map_err(CodecError::Failed)?,
                )
            };
            let mut buffer = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffer, &mut status);
            let sample = ManuallyDrop::take(&mut buffer[0].pSample);
            drop(ManuallyDrop::take(&mut buffer[0].pEvents));
            match result {
                Ok(()) => {}
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder renegotiates its output; take the type it offers.
                    if let Ok(offered) = self.transform.GetOutputAvailableType(0, 0) {
                        let _ = self.transform.SetOutputType(0, &offered, 0);
                    }
                    return Ok(None);
                }
                Err(error) => return Err(CodecError::Failed(text(error))),
            }
            let Some(sample) = sample else {
                return Ok(None);
            };
            let mut data = read_sample(&sample).map_err(CodecError::Failed)?;
            let stamp = (sample.GetSampleTime().unwrap_or(0) / 10) as u32;
            let types = nal_types(&data);
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0
                || types.contains(&5);
            if key && !types.contains(&7) && !self.header.is_empty() {
                let mut with = self.header.clone();
                with.extend_from_slice(&data);
                data = with;
            }
            if key {
                self.forced = false;
            }
            Ok(Some(EncodedFrame {
                key,
                timestamp_us: stamp,
                data,
            }))
        }
    }
}

impl VideoEncoder for MfEncoder {
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
        // Time on a clock that does not wrap.
        let step = match self.last_stamp {
            Some(last) => i64::from(frame.timestamp_us.wrapping_sub(last)),
            None => i64::from(frame.timestamp_us),
        };
        self.last_stamp = Some(frame.timestamp_us);
        self.clock += step.max(1);

        // An asynchronous encoder says when it wants a picture.
        if self.events.is_some() {
            let started = Instant::now();
            self.pump(Duration::ZERO)?;
            while self.credits == 0 {
                self.pump(ANSWER)?;
                if self.credits == 0 {
                    if started.elapsed() > STUCK {
                        return Err(CodecError::Failed(
                            "the encoder stopped taking pictures".into(),
                        ));
                    }
                    // Busy: this picture is dropped; the output of earlier ones may be ready.
                    self.collect()?;
                    return self.next_ready();
                }
            }
        }
        if force_key && !self.forced {
            set_api(&self.api, &CODECAPI_AVEncVideoForceKeyFrame, variant_u32(1));
            self.forced = true;
        }
        // SAFETY: the buffer is locked, written within its length and unlocked; the sample is
        // handed to the encoder, which keeps its own reference.
        unsafe {
            let sample = sample_with_buffer(self.nv12.len() as u32).map_err(CodecError::Failed)?;
            let buffer = sample
                .GetBufferByIndex(0)
                .map_err(|e| CodecError::Failed(text(e)))?;
            let mut at = std::ptr::null_mut();
            buffer
                .Lock(&mut at, None, None)
                .map_err(|e| CodecError::Failed(text(e)))?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), at, self.nv12.len());
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(self.nv12.len() as u32);
            let _ = sample.SetSampleTime(self.clock * 10);
            let _ = sample.SetSampleDuration(10_000_000 / i64::from(setup.fps.max(1)));
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {
                    self.credits = self.credits.saturating_sub(1);
                    self.sent += 1;
                }
                Err(error) if error.code() == MF_E_NOTACCEPTING => {
                    self.collect()?;
                    return self.next_ready();
                }
                Err(error) => return Err(CodecError::Failed(text(error))),
            }
        }
        // Wait a moment for the picture to come out; a pipeline that is a frame behind answers
        // with the one before.
        self.pump(ANSWER)?;
        self.collect()?;
        self.next_ready()
    }

    fn set_bitrate(&mut self, kbps: u32) {
        self.setup.kbps = kbps;
        set_api(
            &self.api,
            &CODECAPI_AVEncCommonMeanBitRate,
            variant_u32(kbps.max(100) * 1000),
        );
    }
}

impl MfEncoder {
    fn next_ready(&mut self) -> Result<EncodedFrame, CodecError> {
        self.ready.pop_front().ok_or(CodecError::Pending)
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: plain calls; a failing flush on the way out changes nothing.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

/// A media type for the H.264 or NV12 side of an encoder.
fn media_type(subtype: &GUID, setup: EncoderSetup) -> Result<IMFMediaType, String> {
    // SAFETY: plain calls on a type this function creates.
    unsafe {
        let media = MFCreateMediaType().map_err(text)?;
        media
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(text)?;
        media.SetGUID(&MF_MT_SUBTYPE, subtype).map_err(text)?;
        media
            .SetUINT64(&MF_MT_FRAME_SIZE, pack(setup.width, setup.height))
            .map_err(text)?;
        media
            .SetUINT64(&MF_MT_FRAME_RATE, pack(setup.fps.max(1), 1))
            .map_err(text)?;
        media
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))
            .map_err(text)?;
        media
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(text)?;
        // SDR BT.709 in limited range, as the share spec says.
        let _ = media.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
        let _ = media.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = media.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = media.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
        Ok(media)
    }
}

/// The SPS and PPS from the encoder's output type, as Annex B.
fn sequence_header(transform: &IMFTransform) -> Vec<u8> {
    // SAFETY: the blob is read into a buffer of the size the type reports.
    unsafe {
        let Ok(output) = transform.GetOutputCurrentType(0) else {
            return Vec::new();
        };
        let Ok(size) = output.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
            return Vec::new();
        };
        let mut blob = vec![0u8; size as usize];
        if output
            .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
            .is_err()
        {
            return Vec::new();
        }
        // The blob is Annex B already for the system's encoders; keep it only if it is.
        if find_start(&blob, 0).is_some() {
            blob
        } else {
            Vec::new()
        }
    }
}

fn sample_with_buffer(size: u32) -> Result<IMFSample, String> {
    // SAFETY: plain calls on objects this function creates.
    unsafe {
        let sample = MFCreateSample().map_err(text)?;
        let buffer = MFCreateMemoryBuffer(size).map_err(text)?;
        sample.AddBuffer(&buffer).map_err(text)?;
        Ok(sample)
    }
}

fn read_sample(sample: &IMFSample) -> Result<Vec<u8>, String> {
    // SAFETY: the buffer is locked for the copy and unlocked after it.
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(text)?;
        let mut at = std::ptr::null_mut();
        let mut length = 0u32;
        buffer
            .Lock(&mut at, None, Some(&mut length))
            .map_err(text)?;
        let data = std::slice::from_raw_parts(at, length as usize).to_vec();
        let _ = buffer.Unlock();
        Ok(data)
    }
}

// ---------------------------------------------------------------------------------------------
// The decoder

/// The system's H.264 decoder.
pub struct MfDecoder {
    transform: IMFTransform,
    /// Size of the picture and the layout of its buffer, known after the first key frame.
    layout: Option<Layout>,
    output_size: u32,
    provides_samples: bool,
    need_key: bool,
    clock: i64,
    last_stamp: Option<u32>,
}

#[derive(Clone, Copy)]
struct Layout {
    /// Visible size.
    width: u32,
    height: u32,
    /// Size of the buffer's rows (the codec's alignment) and bytes per row.
    coded_width: u32,
    coded_height: u32,
    stride: u32,
}

impl MfDecoder {
    pub fn open() -> Result<MfDecoder, String> {
        start()?;
        let activates = enumerate(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(MFVideoFormat_H264),
            Some(MFVideoFormat_NV12),
        );
        let mut last = "this PC has no H.264 decoder".to_owned();
        for activate in &activates {
            match MfDecoder::from_activate(activate) {
                Ok(decoder) => return Ok(decoder),
                Err(why) => last = why,
            }
        }
        Err(last)
    }

    fn from_activate(activate: &IMFActivate) -> Result<MfDecoder, String> {
        // SAFETY: Media Foundation calls on objects this function owns.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().map_err(text)?;
            if let Ok(attributes) = transform.GetAttributes() {
                let attributes: IMFAttributes = attributes;
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let input = MFCreateMediaType().map_err(text)?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(text)?;
            input
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
                .map_err(text)?;
            input
                .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                .map_err(text)?;
            transform.SetInputType(0, &input, 0).map_err(text)?;
            let mut decoder = MfDecoder {
                transform,
                layout: None,
                output_size: 0,
                provides_samples: false,
                need_key: true,
                clock: 0,
                last_stamp: None,
            };
            // The decoder lists NV12 among its outputs even before it has seen a stream.
            decoder.choose_output();
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(text)?;
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(text)?;
            Ok(decoder)
        }
    }

    /// Picks NV12 among the output types, reads the layout and the buffer size.
    fn choose_output(&mut self) -> bool {
        // SAFETY: Media Foundation calls on objects this decoder owns.
        unsafe {
            for index in 0.. {
                let Ok(offered) = self.transform.GetOutputAvailableType(0, index) else {
                    return false;
                };
                if offered.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
                    continue;
                }
                if self.transform.SetOutputType(0, &offered, 0).is_err() {
                    continue;
                }
                let size = offered.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                let (coded_w, coded_h) = ((size >> 32) as u32, size as u32);
                let mut visible = (coded_w, coded_h);
                // The display aperture is 16 bytes: x and y offsets (each a fixed-point pair)
                // then width and height (the same). Only a size smaller than the coded one
                // matters here.
                let mut aperture = [0u8; 16];
                if offered
                    .GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut aperture, None)
                    .is_ok()
                {
                    let w = i32::from_le_bytes(aperture[8..12].try_into().unwrap_or([0; 4]));
                    let h = i32::from_le_bytes(aperture[12..16].try_into().unwrap_or([0; 4]));
                    if w > 0 && h > 0 && w as u32 <= coded_w && h as u32 <= coded_h {
                        visible = (w as u32, h as u32);
                    }
                }
                let stride = offered
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .map(|s| (s as i32).unsigned_abs())
                    .unwrap_or(coded_w);
                if let Ok(info) = self.transform.GetOutputStreamInfo(0) {
                    self.output_size = info.cbSize;
                    self.provides_samples =
                        info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                }
                self.layout = (coded_w > 0 && coded_h > 0).then_some(Layout {
                    width: visible.0 & !1,
                    height: visible.1 & !1,
                    coded_width: coded_w,
                    coded_height: coded_h,
                    stride: stride.max(coded_w),
                });
                return true;
            }
            false
        }
    }

    /// Takes the pictures the decoder has now; the newest is returned.
    fn drain(&mut self) -> Result<Option<Frame>, CodecError> {
        let mut newest = None;
        loop {
            // SAFETY: as in `MfEncoder::take_output`.
            let (result, sample) = unsafe {
                let own = if self.provides_samples || self.layout.is_none() {
                    None
                } else {
                    Some(
                        sample_with_buffer(self.output_size.max(1 << 16))
                            .map_err(CodecError::Failed)?,
                    )
                };
                let mut buffer = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(own),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let result = self.transform.ProcessOutput(0, &mut buffer, &mut status);
                let sample = ManuallyDrop::take(&mut buffer[0].pSample);
                drop(ManuallyDrop::take(&mut buffer[0].pEvents));
                (result, sample)
            };
            match result {
                Ok(()) => {
                    if let (Some(sample), Some(layout)) = (sample, self.layout) {
                        let stamp = unsafe { sample.GetSampleTime() }.unwrap_or(0) / 10;
                        if let Some(frame) = self.convert(&sample, layout, stamp as u32)? {
                            newest = Some(frame);
                        }
                    }
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    if !self.choose_output() {
                        return Err(CodecError::Failed("the decoder offers no NV12".into()));
                    }
                }
                Err(error) => return Err(CodecError::Failed(text(error))),
            }
        }
        Ok(newest)
    }

    fn convert(
        &self,
        sample: &IMFSample,
        layout: Layout,
        timestamp_us: u32,
    ) -> Result<Option<Frame>, CodecError> {
        let data = read_sample(sample).map_err(CodecError::Failed)?;
        // Y at `stride` for `coded_height` rows, then UV; the visible size is cropped from the
        // top left.
        let Some(full) = pixels::nv12_to_bgra(
            &data,
            layout.coded_width & !1,
            layout.coded_height & !1,
            layout.stride as usize,
        ) else {
            return Ok(None);
        };
        let full_w = (layout.coded_width & !1) as usize;
        let (w, h) = (layout.width as usize, layout.height as usize);
        let bgra = if full_w == w && (layout.coded_height & !1) as usize == h {
            full
        } else {
            let mut cropped = Vec::with_capacity(w * h * 4);
            for row in full.chunks_exact(full_w * 4).take(h) {
                cropped.extend_from_slice(&row[..w * 4]);
            }
            cropped
        };
        Ok(Some(Frame {
            width: layout.width,
            height: layout.height,
            timestamp_us,
            bgra,
        }))
    }
}

impl VideoDecoder for MfDecoder {
    fn codec(&self) -> &str {
        CODEC_NAME
    }

    fn decode(&mut self, frame: &EncodedFrame) -> Result<Frame, CodecError> {
        if self.need_key {
            if !frame.key {
                return Err(CodecError::MissingReference);
            }
            self.need_key = false;
        }
        if frame.data.is_empty() {
            return Err(CodecError::Malformed("empty frame"));
        }
        let step = match self.last_stamp {
            Some(last) => i64::from(frame.timestamp_us.wrapping_sub(last)),
            None => i64::from(frame.timestamp_us),
        };
        self.last_stamp = Some(frame.timestamp_us);
        self.clock += step.max(1);
        // SAFETY: the sample's buffer is locked, written within its length and unlocked.
        unsafe {
            let sample = sample_with_buffer(frame.data.len() as u32).map_err(CodecError::Failed)?;
            let buffer = sample
                .GetBufferByIndex(0)
                .map_err(|e| CodecError::Failed(text(e)))?;
            let mut at = std::ptr::null_mut();
            buffer
                .Lock(&mut at, None, None)
                .map_err(|e| CodecError::Failed(text(e)))?;
            std::ptr::copy_nonoverlapping(frame.data.as_ptr(), at, frame.data.len());
            let _ = buffer.Unlock();
            let _ = buffer.SetCurrentLength(frame.data.len() as u32);
            let _ = sample.SetSampleTime(self.clock * 10);
            if frame.key {
                let _ = sample.SetUINT32(&MFSampleExtension_CleanPoint, 1);
            }
            let mut result = self.transform.ProcessInput(0, &sample, 0);
            if let Err(error) = &result
                && error.code() == MF_E_NOTACCEPTING
            {
                // Full: take what is ready, then try again.
                let _ = self.drain()?;
                result = self.transform.ProcessInput(0, &sample, 0);
            }
            if result.is_err() {
                // Garbage in: ask for a key frame rather than ending the stream.
                self.need_key = true;
                let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                return Err(CodecError::Malformed("the decoder refused the frame"));
            }
        }
        match self.drain()? {
            Some(picture) => Ok(picture),
            None => Err(CodecError::Pending),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// What this PC can do

/// What the system's H.264 can encode and decode on this PC, found by opening the codecs.
pub fn capabilities() -> (Vec<CodecCaps>, Vec<CodecCaps>) {
    let probe = EncoderSetup {
        width: 1280,
        height: 720,
        fps: 30,
        kbps: 4000,
    };
    let mut encode = Vec::new();
    if MfEncoder::open(probe, Kind::Hardware).is_ok() {
        encode.push(CodecCaps {
            codec: CODEC_NAME.into(),
            hardware: true,
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
        });
    } else if MfEncoder::open(probe, Kind::Software).is_ok() {
        // The spec's limit for software encoding: the game keeps its processor.
        encode.push(CodecCaps {
            codec: CODEC_NAME.into(),
            hardware: false,
            max_width: 1280,
            max_height: 720,
            max_fps: 30,
        });
    }
    let mut decode = Vec::new();
    if MfDecoder::open().is_ok() {
        decode.push(CodecCaps {
            codec: CODEC_NAME.into(),
            hardware: false,
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
        });
    }
    (encode, decode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyplay_share::testcodec::TestPattern;

    #[test]
    fn nal_types_are_read_from_annex_b() {
        let data = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9,
        ];
        assert_eq!(nal_types(&data), vec![7, 8, 5]);
        assert!(nal_types(&[1, 2, 3]).is_empty());
    }

    /// A moving picture with edges, so that an encoder has something to code.
    fn picture(n: u32, w: u32, h: u32) -> Frame {
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let on = ((x + n * 4) / 32 + y / 32).is_multiple_of(2);
                bgra[i] = if on { 40 } else { 220 };
                bgra[i + 1] = (x * 255 / w) as u8;
                bgra[i + 2] = (y * 255 / h) as u8;
                bgra[i + 3] = 255;
            }
        }
        Frame {
            width: w,
            height: h,
            timestamp_us: n * 16_667,
            bgra,
        }
    }

    /// The H.264 round trip through the system's own codecs. Skips where the system has none
    /// (a server image without Media Foundation).
    #[test]
    fn a_picture_survives_the_encoder_and_the_decoder() {
        let setup = EncoderSetup {
            width: 640,
            height: 360,
            fps: 30,
            kbps: 3000,
        };
        for kind in [Kind::Software, Kind::Hardware] {
            let Ok(mut encoder) = MfEncoder::open(setup, kind) else {
                eprintln!("skipped: no {kind:?} H.264 encoder here");
                continue;
            };
            let Ok(mut decoder) = MfDecoder::open() else {
                eprintln!("skipped: no H.264 decoder here");
                return;
            };
            eprintln!(
                "encoder: {} (hardware: {})",
                encoder.name(),
                encoder.is_hardware()
            );
            let (mut keys, mut shown) = (0, 0);
            let mut last: Option<Frame> = None;
            for n in 0..40 {
                let input = picture(n, 640, 360);
                match encoder.encode(&input, n == 0) {
                    Ok(encoded) => {
                        keys += usize::from(encoded.key);
                        if n < 8 && encoded.key {
                            let types = nal_types(&encoded.data);
                            assert!(
                                types.contains(&7) && types.contains(&8) && types.contains(&5),
                                "{types:?}"
                            );
                        }
                        match decoder.decode(&encoded) {
                            Ok(out) => {
                                shown += 1;
                                last = Some(out);
                            }
                            Err(CodecError::Pending) => {}
                            Err(CodecError::MissingReference) => {}
                            Err(other) => panic!("decode: {other}"),
                        }
                    }
                    Err(CodecError::Pending) => {}
                    Err(other) => panic!("encode: {other}"),
                }
            }
            assert!(keys >= 1, "no key frame");
            assert!(shown >= 20, "only {shown} pictures came out");
            let out = last.unwrap();
            assert_eq!((out.width, out.height), (640, 360));
            // The same picture comes back within what a lossy codec at this rate does.
            let reference = picture(39, 640, 360);
            let mut error = 0u64;
            for (a, b) in reference
                .bgra
                .as_chunks::<4>()
                .0
                .iter()
                .zip(out.bgra.as_chunks::<4>().0)
            {
                for c in 0..3 {
                    error += u64::from(a[c].abs_diff(b[c]));
                }
            }
            let mean = error as f64 / (640.0 * 360.0 * 3.0);
            assert!(mean < 12.0, "mean error {mean}");
        }
    }

    #[test]
    fn a_delta_frame_without_its_key_is_refused() {
        let Ok(mut decoder) = MfDecoder::open() else {
            return;
        };
        let delta = EncodedFrame {
            key: false,
            timestamp_us: 0,
            data: vec![0, 0, 0, 1, 0x41, 0x9a, 0, 0],
        };
        assert_eq!(decoder.decode(&delta), Err(CodecError::MissingReference));
    }

    #[test]
    fn the_test_pattern_goes_through_at_the_streams_size() {
        // The pattern is 320x180 and the stream 1280x720: the encoder scales.
        let Ok(mut encoder) = MfEncoder::open_best(EncoderSetup {
            width: 1280,
            height: 720,
            fps: 30,
            kbps: 4000,
        }) else {
            return;
        };
        let Ok(mut decoder) = MfDecoder::open() else {
            return;
        };
        let mut pattern = TestPattern::default();
        let mut sizes = Vec::new();
        for n in 0..20 {
            let mut frame = pattern.next_frame();
            frame.timestamp_us = n * 33_333;
            if let Ok(encoded) = encoder.encode(&frame, n == 0)
                && let Ok(out) = decoder.decode(&encoded)
            {
                sizes.push((out.width, out.height));
            }
        }
        assert!(!sizes.is_empty());
        assert!(sizes.iter().all(|&s| s == (1280, 720)), "{sizes:?}");
    }
}
