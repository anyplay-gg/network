//! NVENC, the video encoder on NVIDIA GPUs, loaded from the driver while the worker runs.
//!
//! Nothing of NVIDIA's is built into the worker. `libcuda.so.1` and `libnvidia-encode.so.1` are
//! opened with `dlopen` (`libloading`, ISC) when an encoder is asked for, and a PC without the
//! NVIDIA driver finds neither and gets an error saying so. The declarations below are the
//! subset of `nvEncodeAPI.h` (NVENC API 11.1, MIT licence in FFmpeg's `nv-codec-headers`) that an
//! H.264 encoder needs, written by hand with the header's field order and reserved padding. API
//! 11.1 is the version of drivers from 470 on; a newer driver serves it too.
//!
//! The encoder is the low-latency one the spec asks of hardware: preset P4 tuned for ultra low
//! latency, constant bit rate, no B-frames, High profile, SPS and PPS before every IDR, an IDR on
//! request and a safety IDR every 600 frames, BT.709 in limited range (`anyplay_share::pixels`
//! makes the NV12 as for every other encoder). Input goes through a system-memory NV12 buffer the
//! driver owns and the call is synchronous: a picture goes in, its bitstream comes out.
//!
//! Every call into the driver runs with the GPU's primary CUDA context pushed on the calling
//! thread and popped after, so the encoder may be used from another thread than it was opened on.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem::{size_of, zeroed};
use std::ptr;

use anyplay_share::codec::{CodecError, EncodedFrame, Frame, VideoEncoder};
use anyplay_share::pixels;
use libloading::Library;

use super::Backend;
use crate::limits::EncoderSetup;

/// The NVENC backend.
pub struct Nvenc;

impl Backend for Nvenc {
    fn name(&self) -> &'static str {
        "nvenc"
    }

    fn open(&self, setup: EncoderSetup) -> Result<Box<dyn VideoEncoder>, String> {
        NvEncoder::open(setup).map(|e| Box::new(e) as Box<dyn VideoEncoder>)
    }
}

// ---------------------------------------------------------------------------------------------
// The declarations of nvEncodeAPI.h, API 11.1.
// ---------------------------------------------------------------------------------------------

const NVENCAPI_MAJOR: u32 = 11;
const NVENCAPI_MINOR: u32 = 1;
const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR | (NVENCAPI_MINOR << 24);
/// `NVENCAPI_STRUCT_VERSION(ver)`.
const fn struct_version(ver: u32) -> u32 {
    NVENCAPI_VERSION | (ver << 16) | (0x7 << 28)
}
const NV_ENC_CONFIG_VER: u32 = struct_version(7) | (1 << 31);
const NV_ENC_INITIALIZE_PARAMS_VER: u32 = struct_version(5) | (1 << 31);
const NV_ENC_RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);
const NV_ENC_PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
const NV_ENC_PIC_PARAMS_VER: u32 = struct_version(4) | (1 << 31);
const NV_ENC_LOCK_BITSTREAM_VER: u32 = struct_version(1);
const NV_ENC_LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);
const NV_ENC_CREATE_INPUT_BUFFER_VER: u32 = struct_version(1);
const NV_ENC_CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
const NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
const NV_ENCODE_API_FUNCTION_LIST_VER: u32 = struct_version(2);

const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;
const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x0000_0001;
const NV_ENC_PARAMS_RC_CBR: u32 = 0x2;
const NV_ENC_PIC_STRUCT_FRAME: u32 = 1;
const NV_ENC_PIC_TYPE_IDR: u32 = 3;
const NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x2;
const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;

const NV_ENC_SUCCESS: i32 = 0;
const NV_ENC_ERR_NEED_MORE_INPUT: i32 = 17;

/// A Windows-style GUID, as the header declares it on Linux.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
    Guid {
        data1,
        data2,
        data3,
        data4,
    }
}

const NV_ENC_CODEC_H264_GUID: Guid = guid(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
const NV_ENC_H264_PROFILE_HIGH_GUID: Guid = guid(
    0xe7cbc309,
    0x4f7a,
    0x4b89,
    [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10],
);
const NV_ENC_PRESET_P4_GUID: Guid = guid(
    0x90a7b826,
    0xdf06,
    0x4862,
    [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81],
);
/// The old low-latency preset, for a driver that does not know `GetEncodePresetConfigEx`.
const NV_ENC_PRESET_LOW_LATENCY_HP_GUID: Guid = guid(
    0x67082a44,
    0x4bad,
    0x48fa,
    [0x98, 0xea, 0x93, 0x5, 0x6d, 0x15, 0xa, 0x58],
);

#[repr(C)]
#[derive(Clone, Copy)]
struct Qp {
    inter_p: u32,
    inter_b: u32,
    intra: u32,
}

/// `NV_ENC_RC_PARAMS`.
#[repr(C)]
#[derive(Clone, Copy)]
struct RcParams {
    version: u32,
    rate_control_mode: u32,
    const_qp: Qp,
    average_bit_rate: u32,
    max_bit_rate: u32,
    vbv_buffer_size: u32,
    vbv_initial_delay: u32,
    /// The bit field: `enableAQ` is bit 3, `zeroReorderDelay` bit 9.
    flags: u32,
    min_qp: Qp,
    max_qp: Qp,
    initial_rc_qp: Qp,
    temporal_layer_idx_mask: u32,
    temporal_layer_qp: [u8; 8],
    target_quality: u8,
    target_quality_lsb: u8,
    lookahead_depth: u16,
    low_delay_key_frame_scale: u8,
    reserved1: [u8; 3],
    qp_map_mode: u32,
    multi_pass: u32,
    alpha_layer_bitrate_ratio: u32,
    cb_qp_index_offset: i8,
    cr_qp_index_offset: i8,
    reserved2: u16,
    reserved: [u32; 4],
}

/// `zeroReorderDelay` in [`RcParams::flags`].
const RC_ZERO_REORDER_DELAY: u32 = 1 << 9;
/// `enableAQ` in [`RcParams::flags`].
const RC_ENABLE_AQ: u32 = 1 << 3;
/// `repeatSPSPPS` in [`ConfigH264::flags`].
const H264_REPEAT_SPSPPS: u32 = 1 << 12;
/// `outputAUD` in [`ConfigH264::flags`].
const H264_OUTPUT_AUD: u32 = 1 << 6;

#[repr(C)]
#[derive(Clone, Copy)]
struct VuiParameters {
    overscan_info_present_flag: u32,
    overscan_info: u32,
    video_signal_type_present_flag: u32,
    video_format: u32,
    video_full_range_flag: u32,
    colour_description_present_flag: u32,
    colour_primaries: u32,
    transfer_characteristics: u32,
    colour_matrix: u32,
    chroma_sample_location_flag: u32,
    chroma_sample_location_top: u32,
    chroma_sample_location_bot: u32,
    bitstream_restriction_flag: u32,
    reserved: [u32; 15],
}

/// `NV_ENC_CONFIG_H264`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigH264 {
    flags: u32,
    level: u32,
    idr_period: u32,
    separate_colour_plane_flag: u32,
    disable_deblocking_filter_idc: u32,
    num_temporal_layers: u32,
    sps_id: u32,
    pps_id: u32,
    adaptive_transform_mode: u32,
    fmo_mode: u32,
    bdirect_mode: u32,
    entropy_coding_mode: u32,
    stereo_mode: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    vui: VuiParameters,
    ltr_num_frames: u32,
    ltr_trust_mode: u32,
    chroma_format_idc: u32,
    max_temporal_layers: u32,
    use_b_frames_as_ref: u32,
    num_ref_l0: u32,
    num_ref_l1: u32,
    reserved1: [u32; 267],
    reserved2: [*mut c_void; 64],
}

/// `NV_ENC_CODEC_CONFIG`: a union whose largest member is the H.264 one (the HEVC one is smaller).
#[repr(C)]
#[derive(Clone, Copy)]
union CodecConfig {
    h264: ConfigH264,
}

/// `NV_ENC_CONFIG`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Config {
    version: u32,
    profile_guid: Guid,
    gop_length: u32,
    frame_interval_p: i32,
    mono_chrome_encoding: u32,
    frame_field_mode: u32,
    mv_precision: u32,
    rc_params: RcParams,
    codec: CodecConfig,
    reserved: [u32; 278],
    reserved2: [*mut c_void; 64],
}

/// `NV_ENC_PRESET_CONFIG`.
#[repr(C)]
#[derive(Clone, Copy)]
struct PresetConfig {
    version: u32,
    preset_cfg: Config,
    reserved1: [u32; 255],
    reserved2: [*mut c_void; 64],
}

/// `NV_ENC_INITIALIZE_PARAMS`.
#[repr(C)]
#[derive(Clone, Copy)]
struct InitializeParams {
    version: u32,
    encode_guid: Guid,
    preset_guid: Guid,
    encode_width: u32,
    encode_height: u32,
    dar_width: u32,
    dar_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    enable_encode_async: u32,
    enable_ptd: u32,
    flags: u32,
    priv_data_size: u32,
    priv_data: *mut c_void,
    encode_config: *mut Config,
    max_encode_width: u32,
    max_encode_height: u32,
    max_me_hint_counts_per_block: [[u32; 4]; 2],
    tuning_info: u32,
    buffer_format: u32,
    reserved: [u32; 287],
    reserved2: [*mut c_void; 64],
}

/// `NV_ENC_RECONFIGURE_PARAMS`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ReconfigureParams {
    version: u32,
    re_init_encode_params: InitializeParams,
    /// `resetEncoder` is bit 0, `forceIDR` bit 1.
    flags: u32,
}

/// `NV_ENC_CREATE_INPUT_BUFFER`.
#[repr(C)]
struct CreateInputBuffer {
    version: u32,
    width: u32,
    height: u32,
    memory_heap: u32,
    buffer_fmt: u32,
    reserved: u32,
    input_buffer: *mut c_void,
    p_sys_mem_buffer: *mut c_void,
    reserved1: [u32; 57],
    reserved2: [*mut c_void; 63],
}

/// `NV_ENC_CREATE_BITSTREAM_BUFFER`.
#[repr(C)]
struct CreateBitstreamBuffer {
    version: u32,
    size: u32,
    memory_heap: u32,
    reserved: u32,
    bitstream_buffer: *mut c_void,
    bitstream_buffer_ptr: *mut c_void,
    reserved1: [u32; 58],
    reserved2: [*mut c_void; 64],
}

/// `NV_ENC_LOCK_INPUT_BUFFER`.
#[repr(C)]
struct LockInputBuffer {
    version: u32,
    flags: u32,
    input_buffer: *mut c_void,
    buffer_data_ptr: *mut c_void,
    pitch: u32,
    reserved1: [u32; 251],
    reserved2: [*mut c_void; 64],
}

/// `NV_ENC_LOCK_BITSTREAM`.
#[repr(C)]
struct LockBitstream {
    version: u32,
    /// `doNotWait` is bit 0.
    flags: u32,
    output_bitstream: *mut c_void,
    slice_offsets: *mut u32,
    frame_idx: u32,
    hw_encode_status: u32,
    num_slices: u32,
    bitstream_size_in_bytes: u32,
    output_time_stamp: u64,
    output_duration: u64,
    bitstream_buffer_ptr: *mut c_void,
    picture_type: u32,
    picture_struct: u32,
    frame_avg_qp: u32,
    frame_satd: u32,
    ltr_frame_idx: u32,
    ltr_frame_bitmap: u32,
    temporal_id: u32,
    reserved: [u32; 12],
    intra_mb_count: u32,
    inter_mb_count: u32,
    average_mvx: i32,
    average_mvy: i32,
    alpha_layer_size_in_bytes: u32,
    reserved1: [u32; 218],
    reserved2: [*mut c_void; 64],
}

/// `NV_ENC_PIC_PARAMS`.
#[repr(C)]
struct PicParams {
    version: u32,
    input_width: u32,
    input_height: u32,
    input_pitch: u32,
    encode_pic_flags: u32,
    frame_idx: u32,
    input_time_stamp: u64,
    input_duration: u64,
    input_buffer: *mut c_void,
    output_bitstream: *mut c_void,
    completion_event: *mut c_void,
    buffer_fmt: u32,
    picture_struct: u32,
    picture_type: u32,
    /// `NV_ENC_CODEC_PIC_PARAMS`, a union of 1536 bytes; all zero is the default for H.264.
    codec_pic_params: [u64; 192],
    me_hint_counts_per_block: [[u32; 4]; 2],
    me_external_hints: *mut c_void,
    reserved1: [u32; 6],
    reserved2: [*mut c_void; 2],
    qp_delta_map: *mut i8,
    qp_delta_map_size: u32,
    reserved_bit_fields: u32,
    me_hint_ref_pic_dist: [u16; 2],
    alpha_buffer: *mut c_void,
    reserved3: [u32; 286],
    reserved4: [*mut c_void; 59],
}

/// `NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS`.
#[repr(C)]
struct OpenSessionExParams {
    version: u32,
    device_type: u32,
    device: *mut c_void,
    reserved: *mut c_void,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [*mut c_void; 64],
}

type Status = i32;
type Session = *mut c_void;

/// `NV_ENCODE_API_FUNCTION_LIST`. The functions this encoder does not call are plain pointers.
#[repr(C)]
struct FunctionList {
    version: u32,
    reserved: u32,
    open_encode_session: *mut c_void,
    get_encode_guid_count: *mut c_void,
    get_encode_profile_guid_count: *mut c_void,
    get_encode_profile_guids: *mut c_void,
    get_encode_guids: Option<unsafe extern "C" fn(Session, *mut Guid, u32, *mut u32) -> Status>,
    get_input_format_count: *mut c_void,
    get_input_formats: *mut c_void,
    get_encode_caps: *mut c_void,
    get_encode_preset_count: *mut c_void,
    get_encode_preset_guids: *mut c_void,
    get_encode_preset_config:
        Option<unsafe extern "C" fn(Session, Guid, Guid, *mut PresetConfig) -> Status>,
    initialize_encoder: Option<unsafe extern "C" fn(Session, *mut InitializeParams) -> Status>,
    create_input_buffer: Option<unsafe extern "C" fn(Session, *mut CreateInputBuffer) -> Status>,
    destroy_input_buffer: Option<unsafe extern "C" fn(Session, *mut c_void) -> Status>,
    create_bitstream_buffer:
        Option<unsafe extern "C" fn(Session, *mut CreateBitstreamBuffer) -> Status>,
    destroy_bitstream_buffer: Option<unsafe extern "C" fn(Session, *mut c_void) -> Status>,
    encode_picture: Option<unsafe extern "C" fn(Session, *mut PicParams) -> Status>,
    lock_bitstream: Option<unsafe extern "C" fn(Session, *mut LockBitstream) -> Status>,
    unlock_bitstream: Option<unsafe extern "C" fn(Session, *mut c_void) -> Status>,
    lock_input_buffer: Option<unsafe extern "C" fn(Session, *mut LockInputBuffer) -> Status>,
    unlock_input_buffer: Option<unsafe extern "C" fn(Session, *mut c_void) -> Status>,
    get_encode_stats: *mut c_void,
    get_sequence_params: *mut c_void,
    register_async_event: *mut c_void,
    unregister_async_event: *mut c_void,
    map_input_resource: *mut c_void,
    unmap_input_resource: *mut c_void,
    destroy_encoder: Option<unsafe extern "C" fn(Session) -> Status>,
    invalidate_ref_frames: *mut c_void,
    open_encode_session_ex:
        Option<unsafe extern "C" fn(*mut OpenSessionExParams, *mut Session) -> Status>,
    register_resource: *mut c_void,
    unregister_resource: *mut c_void,
    reconfigure_encoder: Option<unsafe extern "C" fn(Session, *mut ReconfigureParams) -> Status>,
    reserved1: *mut c_void,
    create_mv_buffer: *mut c_void,
    destroy_mv_buffer: *mut c_void,
    run_motion_estimation_only: *mut c_void,
    get_last_error_string: Option<unsafe extern "C" fn(Session) -> *const c_char>,
    set_io_cuda_streams: *mut c_void,
    get_encode_preset_config_ex:
        Option<unsafe extern "C" fn(Session, Guid, Guid, u32, *mut PresetConfig) -> Status>,
    get_sequence_param_ex: *mut c_void,
    reserved2: [*mut c_void; 277],
}

// The sizes the header gives (counted field by field, 64-bit Linux): a layout mistake above is a
// compile error here, not a driver that reads the wrong bytes.
const _: () = {
    assert!(size_of::<RcParams>() == 128);
    assert!(size_of::<VuiParameters>() == 112);
    assert!(size_of::<ConfigH264>() == 1792);
    assert!(size_of::<Config>() == 3584);
    assert!(size_of::<PresetConfig>() == 5128);
    assert!(size_of::<InitializeParams>() == 1808);
    assert!(size_of::<CreateInputBuffer>() == 776);
    assert!(size_of::<CreateBitstreamBuffer>() == 776);
    assert!(size_of::<LockInputBuffer>() == 1544);
    assert!(size_of::<OpenSessionExParams>() == 1552);
    assert!(size_of::<FunctionList>() == 2552);
};

// ---------------------------------------------------------------------------------------------
// The driver's libraries.
// ---------------------------------------------------------------------------------------------

type CuResult = c_int;
type CuDevice = c_int;
type CuContext = *mut c_void;

const CUDA_SUCCESS: CuResult = 0;

/// The few CUDA driver calls NVENC needs: a context to run in.
struct Cuda {
    init: unsafe extern "C" fn(u32) -> CuResult,
    count: unsafe extern "C" fn(*mut c_int) -> CuResult,
    device_get: unsafe extern "C" fn(*mut CuDevice, c_int) -> CuResult,
    retain: unsafe extern "C" fn(*mut CuContext, CuDevice) -> CuResult,
    release: unsafe extern "C" fn(CuDevice) -> CuResult,
    push: unsafe extern "C" fn(CuContext) -> CuResult,
    pop: unsafe extern "C" fn(*mut CuContext) -> CuResult,
    error_name: Option<unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult>,
    /// Keeps the library mapped for as long as the functions above are used; dropped last.
    _lib: Library,
}

impl Cuda {
    fn load() -> Result<Cuda, String> {
        // SAFETY: loading the driver's own library runs its initialisers, which is what every
        // CUDA program does; the symbols below are declared as cuda.h declares them.
        unsafe {
            let lib = Library::new("libcuda.so.1").map_err(|e| {
                format!("libcuda.so.1 not found (is the NVIDIA driver installed?): {e}")
            })?;
            macro_rules! sym {
                ($first:literal $(, $rest:literal)*) => {{
                    let mut found = None;
                    found = found.or(lib.get($first).ok().map(|s| *s));
                    $( found = found.or(lib.get($rest).ok().map(|s| *s)); )*
                    found.ok_or_else(|| {
                        format!(
                            "libcuda.so.1 has no {}",
                            String::from_utf8_lossy(&$first[..$first.len() - 1])
                        )
                    })?
                }};
            }
            Ok(Cuda {
                init: sym!(b"cuInit\0"),
                count: sym!(b"cuDeviceGetCount\0"),
                device_get: sym!(b"cuDeviceGet\0"),
                retain: sym!(b"cuDevicePrimaryCtxRetain\0"),
                release: sym!(
                    b"cuDevicePrimaryCtxRelease_v2\0",
                    b"cuDevicePrimaryCtxRelease\0"
                ),
                push: sym!(b"cuCtxPushCurrent_v2\0"),
                pop: sym!(b"cuCtxPopCurrent_v2\0"),
                error_name: lib.get(b"cuGetErrorName\0").ok().map(|s| *s),
                _lib: lib,
            })
        }
    }

    fn describe(&self, code: CuResult) -> String {
        if let Some(name) = self.error_name {
            let mut text: *const c_char = ptr::null();
            // SAFETY: cuGetErrorName stores a pointer to a static string, or leaves it null.
            let ok = unsafe { name(code, &mut text) } == CUDA_SUCCESS;
            if ok && !text.is_null() {
                // SAFETY: the pointer is a NUL-terminated static string.
                return unsafe { CStr::from_ptr(text) }
                    .to_string_lossy()
                    .into_owned();
            }
        }
        format!("CUDA error {code}")
    }

    fn check(&self, code: CuResult, what: &str) -> Result<(), String> {
        if code == CUDA_SUCCESS {
            Ok(())
        } else {
            Err(format!("{what}: {}", self.describe(code)))
        }
    }

    fn device_count(&self) -> Result<c_int, String> {
        // SAFETY: plain driver calls with valid out pointers.
        unsafe {
            self.check((self.init)(0), "cuInit")?;
            let mut count = 0;
            self.check((self.count)(&mut count), "cuDeviceGetCount")?;
            Ok(count)
        }
    }
}

/// A primary context of one device, retained for the encoder's life.
struct Context<'a> {
    cuda: &'a Cuda,
    device: CuDevice,
    handle: CuContext,
}

/// The context made current on this thread until dropped.
struct Pushed(unsafe extern "C" fn(*mut CuContext) -> CuResult);

impl Drop for Pushed {
    fn drop(&mut self) {
        let mut old: CuContext = ptr::null_mut();
        // SAFETY: pops what `Context::push` pushed on this thread.
        unsafe { (self.0)(&mut old) };
    }
}

impl Context<'_> {
    fn retain(cuda: &Cuda, ordinal: c_int) -> Result<Context<'_>, String> {
        let mut device = 0;
        let mut handle: CuContext = ptr::null_mut();
        // SAFETY: valid out pointers; the context is released in `Drop`.
        unsafe {
            cuda.check((cuda.device_get)(&mut device, ordinal), "cuDeviceGet")?;
            cuda.check(
                (cuda.retain)(&mut handle, device),
                "cuDevicePrimaryCtxRetain",
            )?;
        }
        Ok(Context {
            cuda,
            device,
            handle,
        })
    }

    fn push(&self) -> Result<Pushed, String> {
        // SAFETY: the handle is a live context; the guard pops it.
        let code = unsafe { (self.cuda.push)(self.handle) };
        self.cuda.check(code, "cuCtxPushCurrent")?;
        Ok(Pushed(self.cuda.pop))
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: releases the reference `retain` took.
        unsafe { (self.cuda.release)(self.device) };
    }
}

/// The functions of `libnvidia-encode.so.1`, all present (checked at load).
struct Api {
    guids: unsafe extern "C" fn(Session, *mut Guid, u32, *mut u32) -> Status,
    preset_config: Option<unsafe extern "C" fn(Session, Guid, Guid, *mut PresetConfig) -> Status>,
    preset_config_ex:
        Option<unsafe extern "C" fn(Session, Guid, Guid, u32, *mut PresetConfig) -> Status>,
    initialize: unsafe extern "C" fn(Session, *mut InitializeParams) -> Status,
    create_input: unsafe extern "C" fn(Session, *mut CreateInputBuffer) -> Status,
    destroy_input: unsafe extern "C" fn(Session, *mut c_void) -> Status,
    create_bitstream: unsafe extern "C" fn(Session, *mut CreateBitstreamBuffer) -> Status,
    destroy_bitstream: unsafe extern "C" fn(Session, *mut c_void) -> Status,
    encode: unsafe extern "C" fn(Session, *mut PicParams) -> Status,
    lock_bitstream: unsafe extern "C" fn(Session, *mut LockBitstream) -> Status,
    unlock_bitstream: unsafe extern "C" fn(Session, *mut c_void) -> Status,
    lock_input: unsafe extern "C" fn(Session, *mut LockInputBuffer) -> Status,
    unlock_input: unsafe extern "C" fn(Session, *mut c_void) -> Status,
    destroy_encoder: unsafe extern "C" fn(Session) -> Status,
    open_session_ex: unsafe extern "C" fn(*mut OpenSessionExParams, *mut Session) -> Status,
    reconfigure: unsafe extern "C" fn(Session, *mut ReconfigureParams) -> Status,
    last_error: Option<unsafe extern "C" fn(Session) -> *const c_char>,
    /// Keeps the library mapped; dropped last.
    _lib: Library,
}

/// An all-zero value of a plain-data struct of this file (integers, pointers, `Option`s of
/// function pointers and arrays of them: zero is valid for every field).
fn zeroed_struct<T>() -> T {
    // SAFETY: only called for the `repr(C)` structs above, whose fields accept all-zero bytes.
    unsafe { zeroed() }
}

impl Api {
    fn load() -> Result<Api, String> {
        // SAFETY: loading the driver's library; the symbols are declared as nvEncodeAPI.h does.
        unsafe {
            let lib = Library::new("libnvidia-encode.so.1").map_err(|e| {
                format!("libnvidia-encode.so.1 not found (is the NVIDIA driver installed?): {e}")
            })?;
            let max_version: unsafe extern "C" fn(*mut u32) -> Status = lib
                .get(b"NvEncodeAPIGetMaxSupportedVersion\0")
                .map(|s| *s)
                .map_err(|e| format!("libnvidia-encode.so.1: {e}"))?;
            let mut version = 0u32;
            check_plain(
                max_version(&mut version),
                "NvEncodeAPIGetMaxSupportedVersion",
            )?;
            let wanted = (NVENCAPI_MAJOR << 4) | NVENCAPI_MINOR;
            if version < wanted {
                return Err(format!(
                    "driver too old: needs NVENC API {NVENCAPI_MAJOR}.{NVENCAPI_MINOR}, has {}.{}",
                    version >> 4,
                    version & 0xf
                ));
            }
            let create: unsafe extern "C" fn(*mut FunctionList) -> Status = lib
                .get(b"NvEncodeAPICreateInstance\0")
                .map(|s| *s)
                .map_err(|e| format!("libnvidia-encode.so.1: {e}"))?;
            let mut list: Box<FunctionList> = Box::new(zeroed_struct());
            list.version = NV_ENCODE_API_FUNCTION_LIST_VER;
            check_plain(create(&mut *list), "NvEncodeAPICreateInstance")?;
            macro_rules! need {
                ($field:ident) => {
                    list.$field.ok_or_else(|| {
                        format!("libnvidia-encode.so.1 has no {}", stringify!($field))
                    })?
                };
            }
            Ok(Api {
                guids: need!(get_encode_guids),
                preset_config: list.get_encode_preset_config,
                preset_config_ex: list.get_encode_preset_config_ex,
                initialize: need!(initialize_encoder),
                create_input: need!(create_input_buffer),
                destroy_input: need!(destroy_input_buffer),
                create_bitstream: need!(create_bitstream_buffer),
                destroy_bitstream: need!(destroy_bitstream_buffer),
                encode: need!(encode_picture),
                lock_bitstream: need!(lock_bitstream),
                unlock_bitstream: need!(unlock_bitstream),
                lock_input: need!(lock_input_buffer),
                unlock_input: need!(unlock_input_buffer),
                destroy_encoder: need!(destroy_encoder),
                open_session_ex: need!(open_encode_session_ex),
                reconfigure: need!(reconfigure_encoder),
                last_error: list.get_last_error_string,
                _lib: lib,
            })
        }
    }
}

/// The name of an `NVENCSTATUS`.
fn status_name(status: Status) -> &'static str {
    const NAMES: [&str; 26] = [
        "NV_ENC_SUCCESS",
        "NV_ENC_ERR_NO_ENCODE_DEVICE",
        "NV_ENC_ERR_UNSUPPORTED_DEVICE",
        "NV_ENC_ERR_INVALID_ENCODERDEVICE",
        "NV_ENC_ERR_INVALID_DEVICE",
        "NV_ENC_ERR_DEVICE_NOT_EXIST",
        "NV_ENC_ERR_INVALID_PTR",
        "NV_ENC_ERR_INVALID_EVENT",
        "NV_ENC_ERR_INVALID_PARAM",
        "NV_ENC_ERR_INVALID_CALL",
        "NV_ENC_ERR_OUT_OF_MEMORY",
        "NV_ENC_ERR_ENCODER_NOT_INITIALIZED",
        "NV_ENC_ERR_UNSUPPORTED_PARAM",
        "NV_ENC_ERR_LOCK_BUSY",
        "NV_ENC_ERR_NOT_ENOUGH_BUFFER",
        "NV_ENC_ERR_INVALID_VERSION",
        "NV_ENC_ERR_MAP_FAILED",
        "NV_ENC_ERR_NEED_MORE_INPUT",
        "NV_ENC_ERR_ENCODER_BUSY",
        "NV_ENC_ERR_EVENT_NOT_REGISTERD",
        "NV_ENC_ERR_GENERIC",
        "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY",
        "NV_ENC_ERR_UNIMPLEMENTED",
        "NV_ENC_ERR_RESOURCE_REGISTER_FAILED",
        "NV_ENC_ERR_RESOURCE_NOT_REGISTERED",
        "NV_ENC_ERR_RESOURCE_NOT_MAPPED",
    ];
    usize::try_from(status)
        .ok()
        .and_then(|i| NAMES.get(i))
        .copied()
        .unwrap_or("an unknown NVENC status")
}

fn check_plain(status: Status, what: &str) -> Result<(), String> {
    if status == NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(format!("{what}: {} ({status})", status_name(status)))
    }
}

/// The input buffer's size: the picture rounded up to 32 in both directions, as NVIDIA's own
/// samples allocate it, so the chroma plane starts at a known row.
const fn align32(n: u32) -> u32 {
    (n + 31) & !31
}

/// Copies a tight NV12 picture of `width` by `height` into a pitched buffer of `rows` luma rows:
/// the luma rows at the top, the interleaved chroma rows from row `rows` on.
fn copy_nv12(dst: &mut [u8], pitch: usize, rows: usize, src: &[u8], width: usize, height: usize) {
    let luma = width * height;
    for y in 0..height {
        dst[y * pitch..y * pitch + width].copy_from_slice(&src[y * width..(y + 1) * width]);
    }
    let chroma = &src[luma..luma + width * (height / 2)];
    for y in 0..height / 2 {
        let at = (rows + y) * pitch;
        dst[at..at + width].copy_from_slice(&chroma[y * width..(y + 1) * width]);
    }
}

/// The video buffering verifier size in frames' worth of bits: a few pictures of slack so that
/// a key frame is not starved, small enough to keep the delay a viewer sees low.
const VBV_FRAMES: u32 = 3;

fn vbv_bits(kbps: u32, fps: u32) -> u32 {
    kbps.saturating_mul(1000) / fps.max(1) * VBV_FRAMES
}

// ---------------------------------------------------------------------------------------------
// The encoder.
// ---------------------------------------------------------------------------------------------

/// A hardware H.264 encoder on an NVIDIA GPU.
pub struct NvEncoder {
    setup: EncoderSetup,
    /// The configuration the encoder runs with, kept for `set_bitrate`.
    config: Box<Config>,
    init: Box<InitializeParams>,
    session: Session,
    input: *mut c_void,
    output: *mut c_void,
    /// Rows of the input buffer (the picture's height rounded up).
    rows: usize,
    nv12: Vec<u8>,
    frames: u64,
    // Dropped in this order, after `Drop::drop` has destroyed the session: the context, then the
    // libraries it and the session came from.
    context: Option<ContextOwner>,
    api: Api,
}

/// The CUDA library and the context retained from it, in one value so that the context is
/// released before the library is unmapped.
struct ContextOwner {
    // `Context` borrows `Cuda`; the pair is kept in one heap allocation and the borrow is
    // widened to `'static`, which is sound because `context` is dropped before `cuda` (field
    // order) and the box is never moved out of.
    context: Context<'static>,
    _cuda: Box<Cuda>,
}

impl ContextOwner {
    fn new(cuda: Cuda, ordinal: c_int) -> Result<ContextOwner, String> {
        let cuda = Box::new(cuda);
        // SAFETY: `cuda` lives on the heap for as long as this value, and `context` is declared
        // (and so dropped) before it; the reference is never handed out.
        let borrowed: &'static Cuda = unsafe { &*(&*cuda as *const Cuda) };
        let context = Context::retain(borrowed, ordinal)?;
        Ok(ContextOwner {
            context,
            _cuda: cuda,
        })
    }
}

// SAFETY: the session and its buffers are used only through `&mut self`, and every call runs with
// the CUDA context pushed on the calling thread, which NVENC allows from any thread.
unsafe impl Send for NvEncoder {}

impl NvEncoder {
    /// Opens an encoder for `setup`, on the first GPU that has one.
    pub fn open(setup: EncoderSetup) -> Result<NvEncoder, String> {
        let count = Cuda::load()?.device_count()?;
        if count == 0 {
            return Err("no CUDA device".into());
        }
        let mut last = String::new();
        for ordinal in 0..count {
            match NvEncoder::open_on(setup, ordinal) {
                Ok(encoder) => return Ok(encoder),
                Err(why) => last = format!("GPU {ordinal}: {why}"),
            }
        }
        Err(last)
    }

    fn open_on(setup: EncoderSetup, ordinal: c_int) -> Result<NvEncoder, String> {
        let owner = ContextOwner::new(Cuda::load()?, ordinal)?;
        let api = Api::load()?;
        let handle = owner.context.handle;
        // Current on this thread until the encoder is open; the guard borrows nothing.
        let pushed = owner.context.push()?;
        let (width, height) = (setup.width, setup.height);
        let rows = align32(height);
        let mut encoder = NvEncoder {
            setup,
            config: Box::new(zeroed_struct()),
            init: Box::new(zeroed_struct()),
            session: ptr::null_mut(),
            input: ptr::null_mut(),
            output: ptr::null_mut(),
            rows: rows as usize,
            nv12: Vec::new(),
            frames: 0,
            context: Some(owner),
            api,
        };
        // On an error the partly built encoder is dropped and `Drop` frees what exists.

        let mut open: OpenSessionExParams = zeroed_struct();
        open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        open.device_type = NV_ENC_DEVICE_TYPE_CUDA;
        open.device = handle;
        open.api_version = NVENCAPI_VERSION;
        let mut session: Session = ptr::null_mut();
        // SAFETY: valid parameter block and out pointer; the context is current.
        let status = unsafe { (encoder.api.open_session_ex)(&mut open, &mut session) };
        if status != NV_ENC_SUCCESS {
            return Err(match status {
                15 => format!(
                    "driver too old: needs NVENC API {NVENCAPI_MAJOR}.{NVENCAPI_MINOR} ({})",
                    status_name(status)
                ),
                2 | 1 => format!("this GPU has no NVENC encoder ({})", status_name(status)),
                _ => format!(
                    "nvEncOpenEncodeSessionEx: {} ({status})",
                    status_name(status)
                ),
            });
        }
        encoder.session = session;

        encoder.require_h264()?;
        encoder.configure()?;
        // SAFETY: the init parameters and the config they point to live in `encoder`'s boxes.
        let status = unsafe { (encoder.api.initialize)(session, &mut *encoder.init) };
        encoder.check(status, "nvEncInitializeEncoder")?;

        let mut input: CreateInputBuffer = zeroed_struct();
        input.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
        input.width = align32(width);
        input.height = rows;
        input.buffer_fmt = NV_ENC_BUFFER_FORMAT_NV12;
        // SAFETY: valid parameter block; the buffer belongs to the session.
        let status = unsafe { (encoder.api.create_input)(session, &mut input) };
        encoder.check(status, "nvEncCreateInputBuffer")?;
        encoder.input = input.input_buffer;

        let mut output: CreateBitstreamBuffer = zeroed_struct();
        output.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        output.size = (width * height * 2).max(1 << 20);
        // SAFETY: as above.
        let status = unsafe { (encoder.api.create_bitstream)(session, &mut output) };
        encoder.check(status, "nvEncCreateBitstreamBuffer")?;
        encoder.output = output.bitstream_buffer;
        drop(pushed);
        Ok(encoder)
    }

    /// `what` failed with `status`: the error with the driver's own words when it has some.
    fn check(&self, status: Status, what: &str) -> Result<(), String> {
        if status == NV_ENC_SUCCESS {
            return Ok(());
        }
        let mut text = format!("{what}: {} ({status})", status_name(status));
        if let Some(last) = self.api.last_error {
            // SAFETY: returns a NUL-terminated string owned by the session, or null.
            let said = unsafe { last(self.session) };
            if !said.is_null() {
                // SAFETY: non-null, NUL-terminated.
                let said = unsafe { CStr::from_ptr(said) }.to_string_lossy();
                if !said.is_empty() {
                    text.push_str(": ");
                    text.push_str(&said);
                }
            }
        }
        Err(text)
    }

    fn require_h264(&self) -> Result<(), String> {
        let mut count = 0u32;
        let mut guids = [NV_ENC_CODEC_H264_GUID; 16];
        // SAFETY: the array holds 16 GUIDs, as passed.
        let status = unsafe { (self.api.guids)(self.session, guids.as_mut_ptr(), 16, &mut count) };
        self.check(status, "nvEncGetEncodeGUIDs")?;
        if guids[..(count as usize).min(16)].contains(&NV_ENC_CODEC_H264_GUID) {
            Ok(())
        } else {
            Err("this GPU cannot encode H.264".into())
        }
    }

    /// Fills `self.init` and `self.config` from the preset and the setup.
    fn configure(&mut self) -> Result<(), String> {
        let setup = self.setup;
        let mut preset: Box<PresetConfig> = Box::new(zeroed_struct());
        preset.version = NV_ENC_PRESET_CONFIG_VER;
        preset.preset_cfg.version = NV_ENC_CONFIG_VER;
        let (preset_guid, status) = match (self.api.preset_config_ex, self.api.preset_config) {
            (Some(ex), _) => (
                NV_ENC_PRESET_P4_GUID,
                // SAFETY: valid GUIDs and a preset block of the right version.
                unsafe {
                    ex(
                        self.session,
                        NV_ENC_CODEC_H264_GUID,
                        NV_ENC_PRESET_P4_GUID,
                        NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                        &mut *preset,
                    )
                },
            ),
            (None, Some(old)) => (
                NV_ENC_PRESET_LOW_LATENCY_HP_GUID,
                // SAFETY: as above.
                unsafe {
                    old(
                        self.session,
                        NV_ENC_CODEC_H264_GUID,
                        NV_ENC_PRESET_LOW_LATENCY_HP_GUID,
                        &mut *preset,
                    )
                },
            ),
            (None, None) => return Err("the driver has no encoder presets".into()),
        };
        self.check(status, "nvEncGetEncodePresetConfig")?;

        *self.config = preset.preset_cfg;
        let config = &mut *self.config;
        config.version = NV_ENC_CONFIG_VER;
        config.profile_guid = NV_ENC_H264_PROFILE_HIGH_GUID;
        config.gop_length = GOP_FRAMES;
        config.frame_interval_p = 1;
        let rc = &mut config.rc_params;
        rc.rate_control_mode = NV_ENC_PARAMS_RC_CBR;
        rc.average_bit_rate = setup.kbps.saturating_mul(1000);
        rc.max_bit_rate = rc.average_bit_rate;
        rc.vbv_buffer_size = vbv_bits(setup.kbps, setup.fps);
        rc.vbv_initial_delay = rc.vbv_buffer_size;
        rc.flags = (rc.flags | RC_ZERO_REORDER_DELAY) & !RC_ENABLE_AQ;
        // SAFETY: the H.264 member is the one the encode GUID selects, and it is the largest.
        let h264 = unsafe { &mut config.codec.h264 };
        h264.flags = (h264.flags | H264_REPEAT_SPSPPS) & !H264_OUTPUT_AUD;
        h264.idr_period = GOP_FRAMES;
        h264.chroma_format_idc = 1;
        // BT.709 in limited range, as the pictures are made.
        h264.vui.video_signal_type_present_flag = 1;
        h264.vui.video_format = 5;
        h264.vui.video_full_range_flag = 0;
        h264.vui.colour_description_present_flag = 1;
        h264.vui.colour_primaries = 1;
        h264.vui.transfer_characteristics = 1;
        h264.vui.colour_matrix = 1;

        let init = &mut *self.init;
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encode_guid = NV_ENC_CODEC_H264_GUID;
        init.preset_guid = preset_guid;
        init.encode_width = setup.width;
        init.encode_height = setup.height;
        init.dar_width = setup.width;
        init.dar_height = setup.height;
        init.frame_rate_num = setup.fps.max(1);
        init.frame_rate_den = 1;
        init.enable_encode_async = 0;
        init.enable_ptd = 1;
        init.encode_config = &mut *self.config;
        init.max_encode_width = setup.width;
        init.max_encode_height = setup.height;
        init.tuning_info = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        init.buffer_format = NV_ENC_BUFFER_FORMAT_NV12;
        Ok(())
    }

    /// Puts the NV12 picture in `self.nv12` into the driver's input buffer.
    fn fill_input(&mut self) -> Result<u32, String> {
        let mut lock: LockInputBuffer = zeroed_struct();
        lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock.input_buffer = self.input;
        // SAFETY: valid lock block for this session's input buffer.
        let status = unsafe { (self.api.lock_input)(self.session, &mut lock) };
        self.check(status, "nvEncLockInputBuffer")?;
        let pitch = lock.pitch as usize;
        let (width, height) = (self.setup.width as usize, self.setup.height as usize);
        // SAFETY: the locked buffer holds `pitch` bytes for each of its `rows` luma rows and half
        // as many chroma rows, as NV12 does; it stays valid until the unlock below.
        let buffer = unsafe {
            std::slice::from_raw_parts_mut(
                lock.buffer_data_ptr.cast::<u8>(),
                pitch * self.rows * 3 / 2,
            )
        };
        copy_nv12(buffer, pitch, self.rows, &self.nv12, width, height);
        // SAFETY: unlocks what was locked.
        let status = unsafe { (self.api.unlock_input)(self.session, self.input) };
        self.check(status, "nvEncUnlockInputBuffer")?;
        Ok(lock.pitch)
    }

    fn encode_locked(
        &mut self,
        force_key: bool,
        timestamp_us: u32,
    ) -> Result<EncodedFrame, CodecError> {
        let failed = |why: String| CodecError::Failed(why);
        let pitch = self.fill_input().map_err(failed)?;
        let mut pic: Box<PicParams> = Box::new(zeroed_struct());
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.input_width = self.setup.width;
        pic.input_height = self.setup.height;
        pic.input_pitch = pitch;
        pic.encode_pic_flags = if force_key {
            NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS
        } else {
            0
        };
        pic.frame_idx = self.frames as u32;
        pic.input_time_stamp = self.frames;
        pic.input_buffer = self.input;
        pic.output_bitstream = self.output;
        pic.buffer_fmt = NV_ENC_BUFFER_FORMAT_NV12;
        pic.picture_struct = NV_ENC_PIC_STRUCT_FRAME;
        self.frames += 1;
        // SAFETY: a complete picture block for this session's buffers.
        let status = unsafe { (self.api.encode)(self.session, &mut *pic) };
        if status == NV_ENC_ERR_NEED_MORE_INPUT {
            return Err(CodecError::Pending);
        }
        self.check(status, "nvEncEncodePicture").map_err(failed)?;

        let mut lock: Box<LockBitstream> = Box::new(zeroed_struct());
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.output_bitstream = self.output;
        // SAFETY: valid lock block for this session's bitstream buffer.
        let status = unsafe { (self.api.lock_bitstream)(self.session, &mut *lock) };
        self.check(status, "nvEncLockBitstream").map_err(failed)?;
        // SAFETY: the driver says the locked buffer holds this many bytes until the unlock.
        let data = unsafe {
            std::slice::from_raw_parts(
                lock.bitstream_buffer_ptr.cast::<u8>(),
                lock.bitstream_size_in_bytes as usize,
            )
        }
        .to_vec();
        let key = lock.picture_type == NV_ENC_PIC_TYPE_IDR;
        // SAFETY: unlocks what was locked.
        let status = unsafe { (self.api.unlock_bitstream)(self.session, self.output) };
        self.check(status, "nvEncUnlockBitstream").map_err(failed)?;
        if data.is_empty() {
            return Err(CodecError::Pending);
        }
        Ok(EncodedFrame {
            key,
            timestamp_us,
            data,
        })
    }
}

/// A safety net: an IDR at least this often even if nobody asks (frames).
const GOP_FRAMES: u32 = 600;

impl VideoEncoder for NvEncoder {
    fn codec(&self) -> &str {
        "h264"
    }

    fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError> {
        if !frame.is_well_formed() || frame.width == 0 || frame.height == 0 {
            return Err(CodecError::BadFrame);
        }
        let (w, h) = (self.setup.width, self.setup.height);
        pixels::bgra_to_nv12(&frame.bgra, frame.width, frame.height, w, h, &mut self.nv12);
        let pushed = self
            .context
            .as_ref()
            .ok_or_else(|| CodecError::Failed("the encoder is closed".into()))?
            .context
            .push()
            .map_err(CodecError::Failed)?;
        let result = self.encode_locked(force_key, frame.timestamp_us);
        drop(pushed);
        result
    }

    fn set_bitrate(&mut self, kbps: u32) {
        let Some(owner) = self.context.as_ref() else {
            return;
        };
        let Ok(pushed) = owner.context.push() else {
            return;
        };
        let bps = kbps.saturating_mul(1000);
        let (old_rc, old_init) = (self.config.rc_params, *self.init);
        let rc = &mut self.config.rc_params;
        rc.average_bit_rate = bps;
        rc.max_bit_rate = bps;
        rc.vbv_buffer_size = vbv_bits(kbps, self.setup.fps);
        rc.vbv_initial_delay = rc.vbv_buffer_size;
        let mut reconfigure: Box<ReconfigureParams> = Box::new(zeroed_struct());
        reconfigure.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        reconfigure.re_init_encode_params = *self.init;
        reconfigure.re_init_encode_params.encode_config = &mut *self.config;
        // No reset and no forced IDR: the rate changes under the running stream.
        reconfigure.flags = 0;
        // SAFETY: a complete reconfigure block; the config it points to is alive.
        let status = unsafe { (self.api.reconfigure)(self.session, &mut *reconfigure) };
        drop(pushed);
        match self.check(status, "nvEncReconfigureEncoder") {
            Ok(()) => self.setup.kbps = kbps,
            Err(why) => {
                eprintln!("share worker: NVENC kept its bit rate: {why}");
                self.config.rc_params = old_rc;
                *self.init = old_init;
            }
        }
    }
}

impl Drop for NvEncoder {
    fn drop(&mut self) {
        let Some(owner) = self.context.as_ref() else {
            return;
        };
        let pushed = owner.context.push().ok();
        // SAFETY: each handle was made by this session and is destroyed once; without the pushed
        // context the calls would fail, so they are skipped (the process is ending then).
        if pushed.is_some() {
            unsafe {
                if !self.input.is_null() {
                    (self.api.destroy_input)(self.session, self.input);
                }
                if !self.output.is_null() {
                    (self.api.destroy_bitstream)(self.session, self.output);
                }
                if !self.session.is_null() {
                    (self.api.destroy_encoder)(self.session);
                }
            }
        }
        drop(pushed);
        // The context (and then its library) go next, in field order; the NVENC library last.
        self.context = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyplay_share::codec::VideoDecoder;
    use anyplay_share::testcodec::TestPattern;

    #[test]
    fn struct_versions_follow_the_header_macros() {
        // NVENCAPI_VERSION is 11 | (1 << 24).
        assert_eq!(NVENCAPI_VERSION, 0x0100_000B);
        assert_eq!(struct_version(1), 0x7101_000B);
        assert_eq!(NV_ENC_LOCK_BITSTREAM_VER, 0x7101_000B);
        assert_eq!(NV_ENCODE_API_FUNCTION_LIST_VER, 0x7102_000B);
        // The ones the header marks with bit 31.
        assert_eq!(NV_ENC_CONFIG_VER, 0xF107_000B);
        assert_eq!(NV_ENC_INITIALIZE_PARAMS_VER, 0xF105_000B);
        assert_eq!(NV_ENC_PIC_PARAMS_VER, 0xF104_000B);
        assert_eq!(NV_ENC_PRESET_CONFIG_VER, 0xF104_000B);
        assert_eq!(NV_ENC_RECONFIGURE_PARAMS_VER, 0xF101_000B);
    }

    #[test]
    fn the_guids_are_the_headers() {
        assert_eq!(NV_ENC_CODEC_H264_GUID.data1, 0x6bc8_2762);
        assert_eq!(NV_ENC_CODEC_H264_GUID.data4[7], 0xbf);
        assert_eq!(NV_ENC_H264_PROFILE_HIGH_GUID.data1, 0xe7cb_c309);
        assert_eq!(NV_ENC_PRESET_P4_GUID.data1, 0x90a7_b826);
        assert_eq!(
            NV_ENC_PRESET_P4_GUID.data4,
            [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81]
        );
        assert_ne!(NV_ENC_PRESET_P4_GUID, NV_ENC_PRESET_LOW_LATENCY_HP_GUID);
        assert_eq!(size_of::<Guid>(), 16);
    }

    #[test]
    fn the_bit_fields_sit_where_the_header_puts_them() {
        assert_eq!(RC_ENABLE_AQ, 0b1000);
        assert_eq!(RC_ZERO_REORDER_DELAY, 0x200);
        assert_eq!(H264_OUTPUT_AUD, 0x40);
        assert_eq!(H264_REPEAT_SPSPPS, 0x1000);
    }

    #[test]
    fn status_codes_have_their_names() {
        assert_eq!(status_name(0), "NV_ENC_SUCCESS");
        assert_eq!(status_name(2), "NV_ENC_ERR_UNSUPPORTED_DEVICE");
        assert_eq!(status_name(15), "NV_ENC_ERR_INVALID_VERSION");
        assert_eq!(status_name(17), "NV_ENC_ERR_NEED_MORE_INPUT");
        assert_eq!(status_name(25), "NV_ENC_ERR_RESOURCE_NOT_MAPPED");
        assert_eq!(status_name(26), "an unknown NVENC status");
        assert_eq!(status_name(-1), "an unknown NVENC status");
        assert_eq!(NV_ENC_ERR_NEED_MORE_INPUT, 17);
        let why = check_plain(8, "nvEncX").unwrap_err();
        assert_eq!(why, "nvEncX: NV_ENC_ERR_INVALID_PARAM (8)");
    }

    #[test]
    fn the_input_buffer_is_padded_to_32_and_the_picture_copied_by_pitch() {
        assert_eq!(align32(720), 736);
        assert_eq!(align32(1280), 1280);
        assert_eq!(align32(2), 32);
        // 4 by 2 picture, pitch 6, 32-row buffer shrunk to 4 luma rows for the test.
        let src: Vec<u8> = (1..=12).collect();
        let (pitch, rows) = (6, 4);
        let mut dst = vec![0xAA; pitch * rows * 3 / 2];
        copy_nv12(&mut dst, pitch, rows, &src, 4, 2);
        assert_eq!(&dst[0..6], &[1, 2, 3, 4, 0xAA, 0xAA]);
        assert_eq!(&dst[6..12], &[5, 6, 7, 8, 0xAA, 0xAA]);
        // Chroma starts at row 4 (byte 24).
        assert_eq!(&dst[24..30], &[9, 10, 11, 12, 0xAA, 0xAA]);
        assert!(dst[12..24].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn the_verifier_holds_a_few_pictures() {
        assert_eq!(vbv_bits(6000, 30), 600_000);
        assert_eq!(vbv_bits(6000, 0), 18_000_000);
    }

    /// Whether this run must find a GPU (CI machines with one set `ANYPLAY_REQUIRE_HW`).
    fn gpu_required() -> bool {
        matches!(
            std::env::var("ANYPLAY_REQUIRE_HW").as_deref(),
            Ok("1") | Ok("nvenc")
        )
    }

    #[test]
    fn round_trip_when_a_gpu_is_there() {
        let setup = EncoderSetup {
            width: 640,
            height: 360,
            fps: 30,
            kbps: 4000,
        };
        let mut encoder = match NvEncoder::open(setup) {
            Ok(encoder) => encoder,
            Err(why) if gpu_required() => panic!("NVENC is required here, but: {why}"),
            Err(why) => {
                eprintln!("skipped: no NVENC encoder ({why})");
                return;
            }
        };
        assert_eq!(encoder.codec(), "h264");
        let mut decoder = crate::oh264::OhDecoder::open().ok();
        if decoder.is_none() {
            eprintln!("note: no OpenH264 library, the stream is checked but not decoded");
        }
        let mut pattern = TestPattern::default();
        let (mut keys, mut shown, mut sent) = (0, 0, 0);
        for n in 0..30 {
            let picture = pattern.next_frame();
            let encoded = match encoder.encode(&picture, n == 0) {
                Ok(encoded) => encoded,
                Err(CodecError::Pending) => continue,
                Err(e) => panic!("encode {n}: {e}"),
            };
            sent += 1;
            keys += usize::from(encoded.key);
            if sent == 1 {
                assert!(encoded.key, "the first picture asked for a key frame");
                let types = crate::annexb::nal_types(&encoded.data);
                assert!(types.contains(&7), "an IDR carries its SPS: {types:?}");
                assert!(types.contains(&8), "and its PPS: {types:?}");
            }
            if let Some(decoder) = decoder.as_mut() {
                match decoder.decode(&encoded) {
                    Ok(out) => {
                        assert_eq!((out.width, out.height), (setup.width, setup.height));
                        shown += 1;
                    }
                    Err(CodecError::Pending) => {}
                    Err(e) => panic!("decode of picture {n}: {e}"),
                }
            }
        }
        assert!(sent >= 25, "only {sent} of 30 pictures came out");
        assert_eq!(keys, 1, "one key frame unless asked");
        if decoder.is_some() {
            assert!(shown >= 25, "only {shown} pictures were decoded");
        }
        // A second key frame on request.
        let again = encoder.encode(&pattern.next_frame(), true).unwrap();
        assert!(again.key);
        encoder.set_bitrate(8000);
        assert_eq!(encoder.setup.kbps, 8000);
        encoder.set_bitrate(1000);
        assert_eq!(encoder.setup.kbps, 1000);
    }
}
