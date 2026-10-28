//! H.264 on the GPU through VA-API: Intel (iHD, i965) and AMD (Mesa's radeonsi), ADR 0037.
//!
//! `libva.so.2` and `libva-drm.so.2` are opened with `dlopen` while the worker runs, never
//! linked; the driver behind them (`*_drv_video.so`) is libva's business. Nothing here is a
//! codec: the application hands VA-API the sequence, picture and slice parameters and the
//! headers (packed headers, written by [`super::h264bits`]), and the GPU makes the bits.
//!
//! The stream is IPPP with one reference frame, one slice per picture, CAVLC, constant bit rate,
//! an IDR on request and at least every 600 pictures, SDR BT.709 in limited range (in the SPS's
//! VUI). Pictures go in as NV12 through `vaDeriveImage`, the same conversion as every other
//! encoder here (`anyplay_share::pixels`). Each picture is encoded and waited for before the next
//! (`vaSyncSurface`), which costs a frame of latency at most and needs no queue of surfaces.
//!
//! [`Vaapi::open`](super::Backend::open) goes through the render nodes in order, and for each
//! through the profiles High, Main and Constrained Baseline and the entry points `EncSliceLP`
//! and `EncSlice`; a combination counts when the config, the surfaces and the context can be
//! made and a first test picture comes out as an IDR. So a missing library, a PC without a GPU,
//! a GPU whose driver cannot encode H.264 and a driver that fails on the first picture all end
//! in a plain reason.
//!
//! Struct layouts are those of libva's `va.h` and `va_enc_h264.h` (MIT); their sizes are
//! asserted below.

use std::ffi::{CStr, c_int, c_void};
use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyplay_share::codec::{CodecError, EncodedFrame, Frame, VideoEncoder};
use anyplay_share::pixels;
use libloading::Library;

use super::h264bits::{self, Profile};
use crate::limits::EncoderSetup;

/// The codec's name in `hello`, `config` and `start`.
pub const CODEC_NAME: &str = "h264";
/// A safety net: an IDR at least this often even if nobody asks (pictures).
const GOP_FRAMES: u32 = 600;
/// The quantiser the first picture starts from.
const INIT_QP: u32 = 26;

// ---- libva's types and constants (va.h, va_enc_h264.h) ----------------------------------------

type VAStatus = c_int;
type VADisplay = *mut c_void;
type Id = u32;

const VA_INVALID_ID: Id = 0xffff_ffff;
const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

const VA_PROFILE_H264_MAIN: c_int = 6;
const VA_PROFILE_H264_HIGH: c_int = 7;
const VA_PROFILE_H264_CONSTRAINED_BASELINE: c_int = 13;
const VA_ENTRYPOINT_ENC_SLICE: c_int = 6;
const VA_ENTRYPOINT_ENC_SLICE_LP: c_int = 8;

const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;
const VA_CONFIG_ATTRIB_RATE_CONTROL: c_int = 5;
const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: c_int = 10;
const VA_CONFIG_ATTRIB_MAX_PICTURE_WIDTH: c_int = 18;
const VA_CONFIG_ATTRIB_MAX_PICTURE_HEIGHT: c_int = 19;

const VA_RT_FORMAT_YUV420: u32 = 0x1;
const VA_RC_CBR: u32 = 0x2;
const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x1;
const VA_ENC_PACKED_HEADER_PICTURE: u32 = 0x2;
const VA_ENC_PACKED_HEADER_SLICE: u32 = 0x4;
const PACKED_ALL: u32 =
    VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE | VA_ENC_PACKED_HEADER_SLICE;

const VA_FOURCC_NV12: u32 = 0x3231_564E;
const VA_PROGRESSIVE: c_int = 0x1;

const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x2;
const VA_SURFACE_ATTRIB_PIXEL_FORMAT: c_int = 1;
const VA_GENERIC_VALUE_TYPE_INTEGER: c_int = 1;

const VA_ENC_CODED_BUFFER_TYPE: c_int = 21;
const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: c_int = 22;
const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: c_int = 23;
const VA_ENC_SLICE_PARAMETER_BUFFER_TYPE: c_int = 24;
const VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE: c_int = 25;
const VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE: c_int = 26;
const VA_ENC_MISC_PARAMETER_BUFFER_TYPE: c_int = 27;

/// `VAEncPackedHeaderType`: sequence, picture, slice.
const PACKED_TYPE_SEQUENCE: u32 = 1;
const PACKED_TYPE_PICTURE: u32 = 2;
const PACKED_TYPE_SLICE: u32 = 3;

const VA_ENC_MISC_FRAME_RATE: u32 = 0;
const VA_ENC_MISC_RATE_CONTROL: u32 = 1;
const VA_ENC_MISC_HRD: u32 = 5;

const VA_PICTURE_H264_INVALID: u32 = 0x1;
const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x8;

const SLICE_TYPE_P: u8 = 0;
const SLICE_TYPE_I: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct ConfigAttrib {
    kind: c_int,
    value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
union GenericUnion {
    i: i32,
    p: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct GenericValue {
    kind: c_int,
    value: GenericUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SurfaceAttrib {
    kind: c_int,
    flags: u32,
    value: GenericValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PictureH264 {
    picture_id: Id,
    frame_idx: u32,
    flags: u32,
    top_field_order_cnt: i32,
    bottom_field_order_cnt: i32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct SequenceParams {
    seq_parameter_set_id: u8,
    level_idc: u8,
    intra_period: u32,
    intra_idr_period: u32,
    ip_period: u32,
    bits_per_second: u32,
    max_num_ref_frames: u32,
    picture_width_in_mbs: u16,
    picture_height_in_mbs: u16,
    seq_fields: u32,
    bit_depth_luma_minus8: u8,
    bit_depth_chroma_minus8: u8,
    num_ref_frames_in_pic_order_cnt_cycle: u8,
    offset_for_non_ref_pic: i32,
    offset_for_top_to_bottom_field: i32,
    offset_for_ref_frame: [i32; 256],
    frame_cropping_flag: u8,
    frame_crop_left_offset: u32,
    frame_crop_right_offset: u32,
    frame_crop_top_offset: u32,
    frame_crop_bottom_offset: u32,
    vui_parameters_present_flag: u8,
    vui_fields: u32,
    aspect_ratio_idc: u8,
    sar_width: u32,
    sar_height: u32,
    num_units_in_tick: u32,
    time_scale: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct PictureParams {
    curr_pic: PictureH264,
    reference_frames: [PictureH264; 16],
    coded_buf: Id,
    pic_parameter_set_id: u8,
    seq_parameter_set_id: u8,
    last_picture: u8,
    frame_num: u16,
    pic_init_qp: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    chroma_qp_index_offset: i8,
    second_chroma_qp_index_offset: i8,
    pic_fields: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct SliceParams {
    macroblock_address: u32,
    num_macroblocks: u32,
    macroblock_info: Id,
    slice_type: u8,
    pic_parameter_set_id: u8,
    idr_pic_id: u16,
    pic_order_cnt_lsb: u16,
    delta_pic_order_cnt_bottom: i32,
    delta_pic_order_cnt: [i32; 2],
    direct_spatial_mv_pred_flag: u8,
    num_ref_idx_active_override_flag: u8,
    num_ref_idx_l0_active_minus1: u8,
    num_ref_idx_l1_active_minus1: u8,
    ref_pic_list0: [PictureH264; 32],
    ref_pic_list1: [PictureH264; 32],
    luma_log2_weight_denom: u8,
    chroma_log2_weight_denom: u8,
    luma_weight_l0_flag: u8,
    luma_weight_l0: [i16; 32],
    luma_offset_l0: [i16; 32],
    chroma_weight_l0_flag: u8,
    chroma_weight_l0: [[i16; 2]; 32],
    chroma_offset_l0: [[i16; 2]; 32],
    luma_weight_l1_flag: u8,
    luma_weight_l1: [i16; 32],
    luma_offset_l1: [i16; 32],
    chroma_weight_l1_flag: u8,
    chroma_weight_l1: [[i16; 2]; 32],
    chroma_offset_l1: [[i16; 2]; 32],
    cabac_init_idc: u8,
    slice_qp_delta: i8,
    disable_deblocking_filter_idc: u8,
    slice_alpha_c0_offset_div2: i8,
    slice_beta_offset_div2: i8,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct PackedHeaderParams {
    kind: u32,
    bit_length: u32,
    has_emulation_bytes: u8,
    va_reserved: [u32; 4],
}

/// `VAEncMiscParameterBuffer` followed by its payload.
#[repr(C)]
struct Misc<T> {
    kind: u32,
    data: T,
}

#[repr(C)]
struct RateControl {
    bits_per_second: u32,
    target_percentage: u32,
    window_size: u32,
    initial_qp: u32,
    min_qp: u32,
    basic_unit_size: u32,
    /// Bit 0 is `reset`.
    rc_flags: u32,
    icq_quality_factor: u32,
    max_qp: u32,
    quality_factor: u32,
    target_frame_size: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct FrameRate {
    framerate: u32,
    framerate_flags: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
struct Hrd {
    initial_buffer_fullness: u32,
    buffer_size: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ImageFormat {
    fourcc: u32,
    byte_order: u32,
    bits_per_pixel: u32,
    depth: u32,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
    alpha_mask: u32,
    va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Image {
    image_id: Id,
    format: ImageFormat,
    buf: Id,
    width: u16,
    height: u16,
    data_size: u32,
    num_planes: u32,
    pitches: [u32; 3],
    offsets: [u32; 3],
    num_palette_entries: i32,
    entry_bytes: i32,
    component_order: [i8; 4],
    va_reserved: [u32; 4],
}

#[repr(C)]
struct CodedBufferSegment {
    size: u32,
    bit_offset: u32,
    status: u32,
    reserved: u32,
    buf: *mut c_void,
    next: *mut c_void,
    va_reserved: [u32; 4],
}

// The sizes libva's headers give these on a 64-bit Linux (computed by hand from the fields).
const _: () = assert!(size_of::<PictureH264>() == 36);
const _: () = assert!(size_of::<SequenceParams>() == 1132);
const _: () = assert!(size_of::<PictureParams>() == 648);
const _: () = assert!(size_of::<SliceParams>() == 3140);
const _: () = assert!(size_of::<PackedHeaderParams>() == 28);
const _: () = assert!(size_of::<Misc<RateControl>>() == 4 + 60);
const _: () = assert!(size_of::<Misc<FrameRate>>() == 4 + 24);
const _: () = assert!(size_of::<Misc<Hrd>>() == 4 + 24);
const _: () = assert!(size_of::<ImageFormat>() == 48);
const _: () = assert!(size_of::<Image>() == 120);
const _: () = assert!(size_of::<GenericValue>() == 16);
const _: () = assert!(size_of::<SurfaceAttrib>() == 24);
const _: () = assert!(size_of::<CodedBufferSegment>() == 48);

/// A struct of plain integers, arrays and raw pointers, all zero.
fn zeroed<T>() -> Box<T> {
    // SAFETY: only used for the `repr(C)` structs above, whose fields are integers, arrays of
    // them and raw pointers, for which all-zero bytes are a valid value.
    unsafe {
        let layout = std::alloc::Layout::new::<T>();
        let ptr = std::alloc::alloc_zeroed(layout) as *mut T;
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Box::from_raw(ptr)
    }
}

// ---- the library --------------------------------------------------------------------------------

type FnGetDisplayDrm = unsafe extern "C" fn(c_int) -> VADisplay;
type FnInitialize = unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
type FnTerminate = unsafe extern "C" fn(VADisplay) -> VAStatus;
type FnMax = unsafe extern "C" fn(VADisplay) -> c_int;
type FnQueryProfiles = unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
type FnQueryEntrypoints =
    unsafe extern "C" fn(VADisplay, c_int, *mut c_int, *mut c_int) -> VAStatus;
type FnGetConfigAttributes =
    unsafe extern "C" fn(VADisplay, c_int, c_int, *mut ConfigAttrib, c_int) -> VAStatus;
type FnCreateConfig =
    unsafe extern "C" fn(VADisplay, c_int, c_int, *mut ConfigAttrib, c_int, *mut Id) -> VAStatus;
type FnDestroyId = unsafe extern "C" fn(VADisplay, Id) -> VAStatus;
type FnCreateSurfaces = unsafe extern "C" fn(
    VADisplay,
    u32,
    u32,
    u32,
    *mut Id,
    u32,
    *mut SurfaceAttrib,
    u32,
) -> VAStatus;
type FnDestroySurfaces = unsafe extern "C" fn(VADisplay, *mut Id, c_int) -> VAStatus;
type FnCreateContext =
    unsafe extern "C" fn(VADisplay, Id, c_int, c_int, c_int, *mut Id, c_int, *mut Id) -> VAStatus;
type FnCreateBuffer =
    unsafe extern "C" fn(VADisplay, Id, c_int, u32, u32, *mut c_void, *mut Id) -> VAStatus;
type FnMapBuffer = unsafe extern "C" fn(VADisplay, Id, *mut *mut c_void) -> VAStatus;
type FnBeginPicture = unsafe extern "C" fn(VADisplay, Id, Id) -> VAStatus;
type FnRenderPicture = unsafe extern "C" fn(VADisplay, Id, *mut Id, c_int) -> VAStatus;
type FnEndPicture = unsafe extern "C" fn(VADisplay, Id) -> VAStatus;
type FnDeriveImage = unsafe extern "C" fn(VADisplay, Id, *mut Image) -> VAStatus;
type FnErrorStr = unsafe extern "C" fn(VAStatus) -> *const std::ffi::c_char;

/// The functions of libva and libva-drm this module calls.
struct Api {
    get_display_drm: FnGetDisplayDrm,
    initialize: FnInitialize,
    terminate: FnTerminate,
    max_num_profiles: FnMax,
    max_num_entrypoints: FnMax,
    query_profiles: FnQueryProfiles,
    query_entrypoints: FnQueryEntrypoints,
    get_config_attributes: FnGetConfigAttributes,
    create_config: FnCreateConfig,
    destroy_config: FnDestroyId,
    create_surfaces: FnCreateSurfaces,
    destroy_surfaces: FnDestroySurfaces,
    create_context: FnCreateContext,
    destroy_context: FnDestroyId,
    create_buffer: FnCreateBuffer,
    destroy_buffer: FnDestroyId,
    map_buffer: FnMapBuffer,
    unmap_buffer: FnDestroyId,
    begin_picture: FnBeginPicture,
    render_picture: FnRenderPicture,
    end_picture: FnEndPicture,
    sync_surface: FnDestroyId,
    derive_image: FnDeriveImage,
    destroy_image: FnDestroyId,
    error_str: FnErrorStr,
    // The libraries last: the functions above point into them.
    _drm: Library,
    _va: Library,
}

/// One symbol of a library, as the function type `T`.
///
/// # Safety
///
/// `T` must be the real type of the symbol.
unsafe fn symbol<T: Copy>(library: &Library, name: &str) -> Result<T, String> {
    let name = format!("{name}\0");
    // SAFETY: by this function's contract.
    unsafe { library.get::<T>(name.as_bytes()) }
        .map(|symbol| *symbol)
        .map_err(|e| format!("libva lacks {}: {e}", name.trim_end_matches('\0')))
}

impl Api {
    fn load() -> Result<Api, String> {
        // SAFETY: opening libva runs its initialisers, which only set up its tables.
        let va = unsafe { Library::new("libva.so.2") }
            .map_err(|e| format!("libva.so.2 not found ({e})"))?;
        // SAFETY: as above, for the DRM backend.
        let drm = unsafe { Library::new("libva-drm.so.2") }
            .map_err(|e| format!("libva-drm.so.2 not found ({e})"))?;
        // SAFETY: every type below is the function's prototype in va.h and va_drm.h.
        unsafe {
            Ok(Api {
                get_display_drm: symbol(&drm, "vaGetDisplayDRM")?,
                initialize: symbol(&va, "vaInitialize")?,
                terminate: symbol(&va, "vaTerminate")?,
                max_num_profiles: symbol(&va, "vaMaxNumProfiles")?,
                max_num_entrypoints: symbol(&va, "vaMaxNumEntrypoints")?,
                query_profiles: symbol(&va, "vaQueryConfigProfiles")?,
                query_entrypoints: symbol(&va, "vaQueryConfigEntrypoints")?,
                get_config_attributes: symbol(&va, "vaGetConfigAttributes")?,
                create_config: symbol(&va, "vaCreateConfig")?,
                destroy_config: symbol(&va, "vaDestroyConfig")?,
                create_surfaces: symbol(&va, "vaCreateSurfaces")?,
                destroy_surfaces: symbol(&va, "vaDestroySurfaces")?,
                create_context: symbol(&va, "vaCreateContext")?,
                destroy_context: symbol(&va, "vaDestroyContext")?,
                create_buffer: symbol(&va, "vaCreateBuffer")?,
                destroy_buffer: symbol(&va, "vaDestroyBuffer")?,
                map_buffer: symbol(&va, "vaMapBuffer")?,
                unmap_buffer: symbol(&va, "vaUnmapBuffer")?,
                begin_picture: symbol(&va, "vaBeginPicture")?,
                render_picture: symbol(&va, "vaRenderPicture")?,
                end_picture: symbol(&va, "vaEndPicture")?,
                sync_surface: symbol(&va, "vaSyncSurface")?,
                derive_image: symbol(&va, "vaDeriveImage")?,
                destroy_image: symbol(&va, "vaDestroyImage")?,
                error_str: symbol(&va, "vaErrorStr")?,
                _drm: drm,
                _va: va,
            })
        }
    }

    /// `Ok` for VA_STATUS_SUCCESS, else libva's words for the status.
    fn check(&self, status: VAStatus, what: &str) -> Result<(), String> {
        if status == 0 {
            return Ok(());
        }
        // SAFETY: vaErrorStr returns a static NUL-terminated string for any status.
        let text = unsafe {
            let ptr = (self.error_str)(status);
            if ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        Err(format!("{what}: {text} (status {status})"))
    }
}

// ---- a session: display, config, surfaces, context -------------------------------------------

/// The picture surfaces: the one pictures are written to, and two the encoder reconstructs
/// into, one of which is the reference of the next picture.
const INPUT: usize = 0;

/// What one profile and entry point gave: everything that must be destroyed when it ends.
struct Session {
    api: Arc<Api>,
    display: VADisplay,
    config: Id,
    context: Id,
    surfaces: [Id; 3],
    coded: Id,
    /// The packed headers the driver takes (all three or none).
    packed: bool,
    profile: Profile,
    /// The coded size: a whole number of macroblocks.
    aligned_width: u32,
    aligned_height: u32,
    // The render node's file last: the display uses it until `vaTerminate`.
    _node: File,
}

impl Session {
    /// Destroys what `configure` made. Safe to call twice.
    fn release(&mut self) {
        let api = &self.api;
        // SAFETY: each id was made by the call that matches its destroy call on this display,
        // and is set to invalid after it goes.
        unsafe {
            if self.coded != VA_INVALID_ID {
                (api.destroy_buffer)(self.display, self.coded);
                self.coded = VA_INVALID_ID;
            }
            if self.context != VA_INVALID_ID {
                (api.destroy_context)(self.display, self.context);
                self.context = VA_INVALID_ID;
            }
            if self.surfaces[0] != VA_INVALID_ID {
                (api.destroy_surfaces)(self.display, self.surfaces.as_mut_ptr(), 3);
                self.surfaces = [VA_INVALID_ID; 3];
            }
            if self.config != VA_INVALID_ID {
                (api.destroy_config)(self.display, self.config);
                self.config = VA_INVALID_ID;
            }
        }
    }

    /// Makes the config, surfaces, context and coded buffer for `profile` and `entrypoint`.
    fn configure(
        &mut self,
        profile: (Profile, c_int),
        entrypoint: c_int,
        setup: EncoderSetup,
    ) -> Result<(), String> {
        self.release();
        let result = self.configure_inner(profile, entrypoint, setup);
        if result.is_err() {
            self.release();
        }
        result
    }

    fn configure_inner(
        &mut self,
        (profile, va_profile): (Profile, c_int),
        entrypoint: c_int,
        setup: EncoderSetup,
    ) -> Result<(), String> {
        let api = self.api.clone();
        let (width, height) = (align16(setup.width), align16(setup.height));
        let mut asked = [
            ConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: 0,
            },
            ConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: 0,
            },
            ConfigAttrib {
                kind: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: 0,
            },
            ConfigAttrib {
                kind: VA_CONFIG_ATTRIB_MAX_PICTURE_WIDTH,
                value: 0,
            },
            ConfigAttrib {
                kind: VA_CONFIG_ATTRIB_MAX_PICTURE_HEIGHT,
                value: 0,
            },
        ];
        // SAFETY: `asked` is an array of the length passed.
        api.check(
            unsafe {
                (api.get_config_attributes)(
                    self.display,
                    va_profile,
                    entrypoint,
                    asked.as_mut_ptr(),
                    asked.len() as c_int,
                )
            },
            "vaGetConfigAttributes",
        )?;
        let known = |value: u32| value != VA_ATTRIB_NOT_SUPPORTED;
        if !known(asked[0].value) || asked[0].value & VA_RT_FORMAT_YUV420 == 0 {
            return Err("no 4:2:0 input".into());
        }
        if !known(asked[1].value) || asked[1].value & VA_RC_CBR == 0 {
            return Err("no constant bit rate".into());
        }
        for (value, size) in [(asked[3].value, width), (asked[4].value, height)] {
            if known(value) && value != 0 && value < size {
                return Err(format!(
                    "the encoder takes at most {value} pixels, not {size}"
                ));
            }
        }
        self.packed = known(asked[2].value) && asked[2].value & PACKED_ALL == PACKED_ALL;
        self.profile = profile;
        self.aligned_width = width;
        self.aligned_height = height;

        let mut attributes = vec![
            ConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: VA_RT_FORMAT_YUV420,
            },
            ConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: VA_RC_CBR,
            },
        ];
        if self.packed {
            attributes.push(ConfigAttrib {
                kind: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: PACKED_ALL,
            });
        }
        // SAFETY: the attribute array has the length passed; the id is written on success.
        api.check(
            unsafe {
                (api.create_config)(
                    self.display,
                    va_profile,
                    entrypoint,
                    attributes.as_mut_ptr(),
                    attributes.len() as c_int,
                    &mut self.config,
                )
            },
            "vaCreateConfig",
        )?;

        let mut format = SurfaceAttrib {
            kind: VA_SURFACE_ATTRIB_PIXEL_FORMAT,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: GenericValue {
                kind: VA_GENERIC_VALUE_TYPE_INTEGER,
                value: GenericUnion {
                    i: VA_FOURCC_NV12 as i32,
                },
            },
        };
        let mut surfaces = [VA_INVALID_ID; 3];
        // SAFETY: three surfaces are made into the array of three; one attribute is passed.
        api.check(
            unsafe {
                (api.create_surfaces)(
                    self.display,
                    VA_RT_FORMAT_YUV420,
                    width,
                    height,
                    surfaces.as_mut_ptr(),
                    3,
                    &mut format,
                    1,
                )
            },
            "vaCreateSurfaces",
        )?;
        self.surfaces = surfaces;
        // SAFETY: the surface array has the length passed; the id is written on success.
        api.check(
            unsafe {
                (api.create_context)(
                    self.display,
                    self.config,
                    width as c_int,
                    height as c_int,
                    VA_PROGRESSIVE,
                    surfaces.as_mut_ptr(),
                    3,
                    &mut self.context,
                )
            },
            "vaCreateContext",
        )?;
        // Room for a picture at its worst, which a constant rate keeps far below.
        let size = width * height * 3 / 2 + 65536;
        self.coded = self.buffer_raw(VA_ENC_CODED_BUFFER_TYPE, size, 1, std::ptr::null_mut())?;
        Ok(())
    }

    fn buffer_raw(
        &self,
        kind: c_int,
        size: u32,
        count: u32,
        data: *mut c_void,
    ) -> Result<Id, String> {
        let mut id = VA_INVALID_ID;
        // SAFETY: `data` is null or points to `size * count` readable bytes (the callers pass
        // a struct or a byte slice of exactly that size); libva copies it.
        self.api.check(
            unsafe {
                (self.api.create_buffer)(
                    self.display,
                    self.context,
                    kind,
                    size,
                    count,
                    data,
                    &mut id,
                )
            },
            "vaCreateBuffer",
        )?;
        Ok(id)
    }

    /// A buffer holding a copy of `value`.
    fn buffer<T>(&self, kind: c_int, value: &T) -> Result<Id, String> {
        self.buffer_raw(
            kind,
            size_of::<T>() as u32,
            1,
            value as *const T as *mut c_void,
        )
    }

    /// A buffer holding a copy of `bytes`.
    fn bytes(&self, kind: c_int, bytes: &[u8]) -> Result<Id, String> {
        self.buffer_raw(kind, bytes.len() as u32, 1, bytes.as_ptr() as *mut c_void)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.release();
        // SAFETY: the display was initialised by `vaInitialize` and is not used again.
        unsafe {
            (self.api.terminate)(self.display);
        }
    }
}

fn align16(value: u32) -> u32 {
    value.div_ceil(16) * 16
}

// ---- the parameters of a picture (pure, tested) -------------------------------------------------

/// `seq_fields` of the sequence parameters, as the header's bit fields lay them out (least
/// significant first). Must say what the SPS in `h264bits` says.
fn seq_fields() -> u32 {
    1 // chroma_format_idc: 4:2:0 (bits 0-1)
        | 1 << 2 // frame_mbs_only_flag
        | 1 << 5 // direct_8x8_inference_flag
        | h264bits::LOG2_MAX_FRAME_NUM_MINUS4 << 6
        // pic_order_cnt_type 0 (bits 10-11)
        | h264bits::LOG2_MAX_POC_LSB_MINUS4 << 12
}

/// `pic_fields` of the picture parameters.
fn pic_fields(idr: bool, transform_8x8: bool) -> u32 {
    u32::from(idr) // idr_pic_flag
        | 1 << 1 // reference_pic_flag (two bits): every picture is a reference
        | u32::from(transform_8x8) << 8
        | 1 << 9 // deblocking_filter_control_present_flag
}

fn no_picture() -> PictureH264 {
    PictureH264 {
        picture_id: VA_INVALID_ID,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; 4],
    }
}

fn reference(surface: Id, frame_num: u32, poc: i32) -> PictureH264 {
    PictureH264 {
        picture_id: surface,
        frame_idx: frame_num,
        flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
        top_field_order_cnt: poc,
        bottom_field_order_cnt: poc,
        va_reserved: [0; 4],
    }
}

/// The geometry the parameters and headers share.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shape {
    width_mbs: u32,
    height_mbs: u32,
    crop_right: u32,
    crop_bottom: u32,
    fps: u32,
}

impl Shape {
    fn of(setup: &EncoderSetup) -> Shape {
        let (aw, ah) = (align16(setup.width), align16(setup.height));
        Shape {
            width_mbs: aw / 16,
            height_mbs: ah / 16,
            crop_right: aw - setup.width,
            crop_bottom: ah - setup.height,
            fps: setup.fps,
        }
    }

    fn sps(&self, profile: Profile) -> h264bits::Sps {
        h264bits::Sps {
            profile,
            level_idc: h264bits::level_for(self.width_mbs, self.height_mbs, self.fps),
            width_mbs: self.width_mbs,
            height_mbs: self.height_mbs,
            crop_right: self.crop_right,
            crop_bottom: self.crop_bottom,
            fps: self.fps,
        }
    }
}

fn sequence_params(shape: &Shape, profile: Profile, bps: u32) -> Box<SequenceParams> {
    let mut p = zeroed::<SequenceParams>();
    p.seq_parameter_set_id = 0;
    p.level_idc = shape.sps(profile).level_idc;
    p.intra_period = GOP_FRAMES;
    p.intra_idr_period = GOP_FRAMES;
    p.ip_period = 1; // P pictures only
    p.bits_per_second = bps;
    p.max_num_ref_frames = 1;
    p.picture_width_in_mbs = shape.width_mbs as u16;
    p.picture_height_in_mbs = shape.height_mbs as u16;
    p.seq_fields = seq_fields();
    if shape.crop_right > 0 || shape.crop_bottom > 0 {
        p.frame_cropping_flag = 1;
        p.frame_crop_right_offset = shape.crop_right / 2;
        p.frame_crop_bottom_offset = shape.crop_bottom / 2;
    }
    p
}

/// Where a picture stands in the sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Position {
    idr: bool,
    /// `frame_num`: pictures since the IDR, modulo 256.
    frame_num: u32,
    /// The picture order count: twice the pictures since the IDR.
    poc: i32,
    idr_pic_id: u32,
}

impl Position {
    fn slice(&self, qp_delta: i32) -> h264bits::Slice {
        h264bits::Slice {
            idr: self.idr,
            frame_num: self.frame_num,
            idr_pic_id: self.idr_pic_id,
            poc_lsb: self.poc as u32 & 0xff,
            qp_delta,
        }
    }
}

fn picture_params(
    at: &Position,
    coded: Id,
    current: Id,
    previous: Option<Id>,
    transform_8x8: bool,
) -> Box<PictureParams> {
    let mut p = zeroed::<PictureParams>();
    p.curr_pic = reference(current, at.frame_num, at.poc);
    p.reference_frames = [no_picture(); 16];
    if let (false, Some(previous)) = (at.idr, previous) {
        let n = at.frame_num.wrapping_sub(1) & 0xff;
        p.reference_frames[0] = reference(previous, n, at.poc - 2);
    }
    p.coded_buf = coded;
    p.frame_num = at.frame_num as u16;
    p.pic_init_qp = INIT_QP as u8;
    p.pic_fields = pic_fields(at.idr, transform_8x8);
    p
}

fn slice_params(at: &Position, shape: &Shape, previous: Option<Id>) -> Box<SliceParams> {
    let mut s = zeroed::<SliceParams>();
    s.macroblock_address = 0;
    s.num_macroblocks = shape.width_mbs * shape.height_mbs;
    s.macroblock_info = VA_INVALID_ID;
    s.slice_type = if at.idr { SLICE_TYPE_I } else { SLICE_TYPE_P };
    s.idr_pic_id = at.idr_pic_id as u16;
    s.pic_order_cnt_lsb = (at.poc & 0xff) as u16;
    s.ref_pic_list0 = [no_picture(); 32];
    s.ref_pic_list1 = [no_picture(); 32];
    if let (false, Some(previous)) = (at.idr, previous) {
        s.ref_pic_list0[0] = reference(previous, at.frame_num.wrapping_sub(1) & 0xff, at.poc - 2);
    }
    s
}

fn rate_control(bps: u32, reset: bool) -> Misc<RateControl> {
    Misc {
        kind: VA_ENC_MISC_RATE_CONTROL,
        data: RateControl {
            bits_per_second: bps,
            target_percentage: 100,
            // The window the rate is held to, in milliseconds.
            window_size: 500,
            initial_qp: INIT_QP,
            min_qp: 0,
            basic_unit_size: 0,
            rc_flags: u32::from(reset),
            icq_quality_factor: 0,
            max_qp: 0,
            quality_factor: 0,
            target_frame_size: 0,
            va_reserved: [0; 4],
        },
    }
}

fn frame_rate(fps: u32) -> Misc<FrameRate> {
    Misc {
        kind: VA_ENC_MISC_FRAME_RATE,
        data: FrameRate {
            framerate: fps.clamp(1, 0xffff),
            framerate_flags: 0,
            va_reserved: [0; 4],
        },
    }
}

fn hrd(bps: u32) -> Misc<Hrd> {
    // Half a second of the rate, three quarters full at the start.
    let size = bps / 2;
    Misc {
        kind: VA_ENC_MISC_HRD,
        data: Hrd {
            initial_buffer_fullness: size / 4 * 3,
            buffer_size: size,
            va_reserved: [0; 4],
        },
    }
}

// ---- the encoder ------------------------------------------------------------------------------

/// H.264 from a GPU through VA-API.
pub struct VaEncoder {
    session: Session,
    setup: EncoderSetup,
    shape: Shape,
    nv12: Vec<u8>,
    /// Pictures since (and including) the last IDR; 0 before the first.
    since_idr: u32,
    idr_pic_id: u32,
    /// The surface the last picture was reconstructed into, the next one's reference.
    previous: Option<usize>,
    /// Ask for an IDR next (after a failure the reference cannot be trusted).
    need_idr: bool,
    /// The rate changed since the last picture: tell the rate control to restart.
    rate_changed: bool,
}

// SAFETY: the display and its ids are used from one thread at a time (the encoder is moved, not
// shared), which libva allows for a display it has not been told to share.
unsafe impl Send for VaEncoder {}

impl VaEncoder {
    /// Opens an encoder for `setup` (already lowered to the hardware limits) on the first render
    /// node and profile that works.
    pub fn open(setup: EncoderSetup) -> Result<VaEncoder, String> {
        let _one_at_a_time = super::starting_a_driver();
        let api = Arc::new(Api::load()?);
        let mut nodes = render_nodes();
        if nodes.is_empty() {
            return Err("no render node in /dev/dri".into());
        }
        nodes.sort();
        let mut why = Vec::new();
        for node in nodes {
            match open_node(&api, &node, setup) {
                Ok(encoder) => return Ok(encoder),
                Err(reason) => why.push(format!("{}: {reason}", node.display())),
            }
        }
        Err(format!(
            "no render node with an H.264 encoder ({})",
            why.join("; ")
        ))
    }

    fn bps(&self) -> u32 {
        self.setup.kbps.saturating_mul(1000)
    }

    /// Writes the NV12 picture in `self.nv12` into the input surface, padded to whole
    /// macroblocks by repeating the edge.
    fn upload(&mut self) -> Result<(), CodecError> {
        let session = &self.session;
        let api = &session.api;
        let failed = |why: String| CodecError::Failed(why);
        let mut image: Image = {
            // SAFETY: Image is plain integers; the driver fills it in.
            unsafe { std::mem::zeroed() }
        };
        // SAFETY: `image` is written by the driver on success.
        api.check(
            unsafe { (api.derive_image)(session.display, session.surfaces[INPUT], &mut image) },
            "vaDeriveImage",
        )
        .map_err(failed)?;
        let result = (|| {
            if image.format.fourcc != VA_FOURCC_NV12 || image.num_planes < 2 {
                return Err("the driver's picture is not NV12".to_owned());
            }
            let mut mapped: *mut c_void = std::ptr::null_mut();
            // SAFETY: the image's buffer is valid until the image is destroyed.
            api.check(
                unsafe { (api.map_buffer)(session.display, image.buf, &mut mapped) },
                "vaMapBuffer (picture)",
            )?;
            let written = if mapped.is_null() {
                Err("the driver mapped no picture".to_owned())
            } else {
                // SAFETY: the mapping is `data_size` bytes long and exclusively ours until the
                // unmap below.
                let data = unsafe {
                    std::slice::from_raw_parts_mut(mapped as *mut u8, image.data_size as usize)
                };
                copy_nv12(
                    &self.nv12,
                    (self.setup.width, self.setup.height),
                    data,
                    (&image.pitches, &image.offsets),
                    (session.aligned_width, session.aligned_height),
                )
            };
            // SAFETY: the buffer was mapped above.
            let unmapped = api.check(
                unsafe { (api.unmap_buffer)(session.display, image.buf) },
                "vaUnmapBuffer (picture)",
            );
            written.and(unmapped)
        })();
        // SAFETY: the image came from vaDeriveImage above.
        let destroyed = api.check(
            unsafe { (api.destroy_image)(session.display, image.image_id) },
            "vaDestroyImage",
        );
        result.and(destroyed).map_err(failed)
    }

    /// Hands one picture to the GPU and returns its bits.
    fn run(&mut self, at: &Position, current: usize) -> Result<Vec<u8>, String> {
        let session = &self.session;
        let api = &session.api;
        let bps = self.bps();
        let previous = self.previous.map(|n| session.surfaces[n]);
        let t8 = session.profile == Profile::High;
        let mut ids: Vec<Id> = Vec::new();
        let made = (|| -> Result<(), String> {
            let packed_header =
                |ids: &mut Vec<Id>, kind: u32, header: h264bits::Packed| -> Result<(), String> {
                    let params = PackedHeaderParams {
                        kind,
                        bit_length: header.bit_length,
                        has_emulation_bytes: 1,
                        va_reserved: [0; 4],
                    };
                    ids.push(session.buffer(VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE, &params)?);
                    ids.push(session.bytes(VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE, &header.bytes)?);
                    Ok(())
                };
            if at.idr {
                let sequence = sequence_params(&self.shape, session.profile, bps);
                ids.push(session.buffer(VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE, &*sequence)?);
                if session.packed {
                    let sps = h264bits::sps_nal(&self.shape.sps(session.profile));
                    packed_header(&mut ids, PACKED_TYPE_SEQUENCE, h264bits::packed(sps))?;
                }
                ids.push(session.buffer(
                    VA_ENC_MISC_PARAMETER_BUFFER_TYPE,
                    &frame_rate(self.setup.fps),
                )?);
                ids.push(session.buffer(VA_ENC_MISC_PARAMETER_BUFFER_TYPE, &hrd(bps))?);
            }
            let reset = at.idr || self.rate_changed;
            ids.push(session.buffer(VA_ENC_MISC_PARAMETER_BUFFER_TYPE, &rate_control(bps, reset))?);
            let picture =
                picture_params(at, session.coded, session.surfaces[current], previous, t8);
            ids.push(session.buffer(VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE, &*picture)?);
            if session.packed {
                if at.idr {
                    let pps = h264bits::pps_nal(&h264bits::Pps {
                        profile: session.profile,
                        init_qp: INIT_QP,
                        transform_8x8: t8,
                    });
                    packed_header(&mut ids, PACKED_TYPE_PICTURE, h264bits::packed(pps))?;
                }
                packed_header(
                    &mut ids,
                    PACKED_TYPE_SLICE,
                    h264bits::slice_header(&at.slice(0)),
                )?;
            }
            let slice = slice_params(at, &self.shape, previous);
            ids.push(session.buffer(VA_ENC_SLICE_PARAMETER_BUFFER_TYPE, &*slice)?);
            Ok(())
        })();

        let encoded = made.and_then(|()| {
            // SAFETY: the context and the input surface are ours.
            api.check(
                unsafe {
                    (api.begin_picture)(session.display, session.context, session.surfaces[INPUT])
                },
                "vaBeginPicture",
            )?;
            // SAFETY: `ids` holds buffers of this context and has the length passed.
            let rendered = api.check(
                unsafe {
                    (api.render_picture)(
                        session.display,
                        session.context,
                        ids.as_mut_ptr(),
                        ids.len() as c_int,
                    )
                },
                "vaRenderPicture",
            );
            // SAFETY: a picture was begun on this context.
            let ended = api.check(
                unsafe { (api.end_picture)(session.display, session.context) },
                "vaEndPicture",
            );
            rendered?;
            ended?;
            // SAFETY: the input surface was the render target.
            api.check(
                unsafe { (api.sync_surface)(session.display, session.surfaces[INPUT]) },
                "vaSyncSurface",
            )?;
            self.read_coded()
        });
        for id in ids {
            // SAFETY: each id is a buffer made above and not used again.
            unsafe {
                (api.destroy_buffer)(session.display, id);
            }
        }
        encoded
    }

    /// The bits the GPU wrote: the coded buffer's segments, joined.
    fn read_coded(&self) -> Result<Vec<u8>, String> {
        let session = &self.session;
        let api = &session.api;
        let mut mapped: *mut c_void = std::ptr::null_mut();
        // SAFETY: the coded buffer is ours; on success `mapped` points to its first segment.
        api.check(
            unsafe { (api.map_buffer)(session.display, session.coded, &mut mapped) },
            "vaMapBuffer (coded)",
        )?;
        let mut out = Vec::new();
        let mut segment = mapped as *const CodedBufferSegment;
        let mut guard = 0;
        while !segment.is_null() && guard < 64 {
            // SAFETY: the driver's segments form a list ending in a null `next`, each with
            // `size` readable bytes at `buf`, valid until the unmap below.
            unsafe {
                let s = &*segment;
                if !s.buf.is_null() {
                    out.extend_from_slice(std::slice::from_raw_parts(
                        s.buf as *const u8,
                        s.size as usize,
                    ));
                }
                segment = s.next as *const CodedBufferSegment;
            }
            guard += 1;
        }
        // SAFETY: the buffer was mapped above.
        api.check(
            unsafe { (api.unmap_buffer)(session.display, session.coded) },
            "vaUnmapBuffer (coded)",
        )?;
        Ok(out)
    }

    /// The headers an IDR must carry, when the driver wrote the slice but not them.
    fn with_headers(&self, mut data: Vec<u8>) -> Vec<u8> {
        let types = crate::annexb::nal_types(&data);
        let mut head = Vec::new();
        if !types.contains(&7) {
            head.extend(h264bits::sps_nal(&self.shape.sps(self.session.profile)));
        }
        if !types.contains(&8) {
            head.extend(h264bits::pps_nal(&h264bits::Pps {
                profile: self.session.profile,
                init_qp: INIT_QP,
                transform_8x8: self.session.profile == Profile::High,
            }));
        }
        if head.is_empty() {
            return data;
        }
        head.append(&mut data);
        head
    }
}

/// Copies a tight NV12 picture of `(w, h)` into a mapped surface of `(aw, ah)` with the given
/// pitches and offsets, repeating the last column and row into the padding.
fn copy_nv12(
    nv12: &[u8],
    (w, h): (u32, u32),
    data: &mut [u8],
    (pitches, offsets): (&[u32; 3], &[u32; 3]),
    (aw, ah): (u32, u32),
) -> Result<(), String> {
    let (w, h, aw, ah) = (w as usize, h as usize, aw as usize, ah as usize);
    if nv12.len() != w * h * 3 / 2 || !w.is_multiple_of(2) || !h.is_multiple_of(2) {
        return Err("the picture is not NV12 of an even size".into());
    }
    // Luma, then chroma: interleaved U/V pairs, counted in bytes.
    let planes = [(0usize, w, h, aw, ah), (1usize, w, h / 2, aw, ah / 2)];
    let mut source_at = 0;
    for (plane, row_bytes, rows, to_bytes, to_rows) in planes {
        let (pitch, offset) = (pitches[plane] as usize, offsets[plane] as usize);
        if pitch < to_bytes || offset + pitch * to_rows > data.len() {
            return Err(format!(
                "the driver's picture ({pitch} byte rows at {offset}) is smaller than {to_bytes} by {to_rows}"
            ));
        }
        // The padding repeats the last sample (for chroma the last U/V pair).
        let step = if plane == 0 { 1 } else { 2 };
        for row in 0..to_rows {
            let from = source_at + row.min(rows - 1) * row_bytes;
            let line = &nv12[from..from + row_bytes];
            let to = &mut data[offset + row * pitch..offset + row * pitch + to_bytes];
            to[..row_bytes].copy_from_slice(line);
            for x in row_bytes..to_bytes {
                to[x] = line[row_bytes - step + (x - row_bytes) % step];
            }
        }
        source_at += row_bytes * rows;
    }
    Ok(())
}

fn render_nodes() -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    dir.filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
        .map(|entry| entry.path())
        .collect()
}

/// The profiles tried, best first, with VA-API's number for each.
const PROFILES: [(Profile, c_int); 3] = [
    (Profile::High, VA_PROFILE_H264_HIGH),
    (Profile::Main, VA_PROFILE_H264_MAIN),
    (Profile::Baseline, VA_PROFILE_H264_CONSTRAINED_BASELINE),
];

fn open_node(api: &Arc<Api>, node: &Path, setup: EncoderSetup) -> Result<VaEncoder, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .map_err(|e| format!("cannot open: {e}"))?;
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor stays open as long as the session, which owns the file.
    let display = unsafe { (api.get_display_drm)(file.as_raw_fd()) };
    if display.is_null() {
        return Err("libva-drm gave no display".into());
    }
    let (mut major, mut minor) = (0, 0);
    // SAFETY: both outputs are ints.
    api.check(
        unsafe { (api.initialize)(display, &mut major, &mut minor) },
        "vaInitialize (no driver for this GPU?)",
    )?;
    let session = Session {
        api: api.clone(),
        display,
        config: VA_INVALID_ID,
        context: VA_INVALID_ID,
        surfaces: [VA_INVALID_ID; 3],
        coded: VA_INVALID_ID,
        packed: false,
        profile: Profile::Baseline,
        aligned_width: 0,
        aligned_height: 0,
        _node: file,
    };

    // SAFETY: the lists are as long as libva says they can be.
    let profiles = unsafe {
        let mut list = vec![0 as c_int; (api.max_num_profiles)(display).max(1) as usize];
        let mut count = 0;
        api.check(
            (api.query_profiles)(display, list.as_mut_ptr(), &mut count),
            "vaQueryConfigProfiles",
        )?;
        list.truncate(count.max(0) as usize);
        list
    };
    let mut encoder = VaEncoder::new(session, setup);
    let mut why = Vec::new();
    for &(profile, va_profile) in &PROFILES {
        if !profiles.contains(&va_profile) {
            continue;
        }
        // SAFETY: as above, for entry points.
        let entrypoints = unsafe {
            let mut list = vec![0 as c_int; (api.max_num_entrypoints)(display).max(1) as usize];
            let mut count = 0;
            api.check(
                (api.query_entrypoints)(display, va_profile, list.as_mut_ptr(), &mut count),
                "vaQueryConfigEntrypoints",
            )?;
            list.truncate(count.max(0) as usize);
            list
        };
        for entrypoint in [VA_ENTRYPOINT_ENC_SLICE_LP, VA_ENTRYPOINT_ENC_SLICE] {
            if !entrypoints.contains(&entrypoint) {
                continue;
            }
            let name = format!(
                "{profile:?}/{}",
                if entrypoint == 8 { "LP" } else { "slice" }
            );
            let tried = encoder
                .session
                .configure((profile, va_profile), entrypoint, setup)
                .and_then(|()| encoder.prove());
            match tried {
                Ok(()) => return Ok(encoder),
                Err(reason) => why.push(format!("{name}: {reason}")),
            }
        }
    }
    if why.is_empty() {
        Err("the driver has no H.264 encoder".into())
    } else {
        Err(why.join("; "))
    }
}

impl VaEncoder {
    fn new(session: Session, setup: EncoderSetup) -> VaEncoder {
        VaEncoder {
            shape: Shape::of(&setup),
            session,
            setup,
            nv12: Vec::new(),
            since_idr: 0,
            idr_pic_id: 0,
            previous: None,
            need_idr: true,
            rate_changed: false,
        }
    }

    /// Encodes the NV12 picture in `self.nv12`: whether it is an IDR, and its bits.
    fn encode_nv12(&mut self, force_key: bool) -> Result<(bool, Vec<u8>), CodecError> {
        self.upload()?;
        let idr =
            force_key || self.need_idr || self.previous.is_none() || self.since_idr >= GOP_FRAMES;
        if idr {
            self.idr_pic_id = (self.idr_pic_id + 1) & 0xffff;
        }
        let at = Position {
            idr,
            frame_num: if idr { 0 } else { self.since_idr % 256 },
            poc: if idr { 0 } else { self.since_idr as i32 * 2 },
            idr_pic_id: self.idr_pic_id,
        };
        let current = if self.previous == Some(1) { 2 } else { 1 };
        let data = match self.run(&at, current) {
            Ok(data) => data,
            Err(why) => {
                // The reference cannot be trusted any more.
                self.need_idr = true;
                return Err(CodecError::Failed(why));
            }
        };
        if data.is_empty() {
            self.need_idr = true;
            return Err(CodecError::Pending);
        }
        let data = if idr { self.with_headers(data) } else { data };
        self.previous = Some(current);
        self.since_idr = if idr { 1 } else { self.since_idr + 1 };
        self.need_idr = false;
        self.rate_changed = false;
        Ok((idr, data))
    }

    /// Encodes a grey test picture and checks it comes out as an IDR; the stream starts over
    /// afterwards. This is what makes opening a probe of the whole path.
    fn prove(&mut self) -> Result<(), String> {
        let (w, h) = (self.setup.width as usize, self.setup.height as usize);
        self.nv12 = vec![128; w * h * 3 / 2];
        let proved = match self.encode_nv12(true) {
            Ok((true, data)) if crate::annexb::nal_types(&data).contains(&5) => Ok(()),
            Ok(_) => Err("the first picture has no IDR slice".to_owned()),
            Err(why) => Err(why.to_string()),
        };
        self.since_idr = 0;
        self.previous = None;
        self.need_idr = true;
        proved
    }
}

impl VideoEncoder for VaEncoder {
    fn codec(&self) -> &str {
        CODEC_NAME
    }

    fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<EncodedFrame, CodecError> {
        if !frame.is_well_formed() || frame.width == 0 || frame.height == 0 {
            return Err(CodecError::BadFrame);
        }
        pixels::bgra_to_nv12(
            &frame.bgra,
            frame.width,
            frame.height,
            self.setup.width,
            self.setup.height,
            &mut self.nv12,
        );
        let (key, data) = self.encode_nv12(force_key)?;
        Ok(EncodedFrame {
            key,
            timestamp_us: frame.timestamp_us,
            data,
        })
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if kbps != self.setup.kbps {
            self.setup.kbps = kbps;
            // The next picture's rate control parameters carry it, with a restart flag.
            self.rate_changed = true;
        }
    }
}

/// The VA-API backend.
pub struct Vaapi;

impl super::Backend for Vaapi {
    fn name(&self) -> &'static str {
        "vaapi"
    }

    fn open(&self, setup: EncoderSetup) -> Result<Box<dyn VideoEncoder>, String> {
        VaEncoder::open(setup).map(|e| Box::new(e) as Box<dyn VideoEncoder>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyplay_share::testcodec::TestPattern;

    fn setup(width: u32, height: u32) -> EncoderSetup {
        EncoderSetup {
            width,
            height,
            fps: 30,
            kbps: 4000,
        }
    }

    #[test]
    fn the_bit_fields_are_laid_out_as_libva_s_headers_say() {
        // chroma_format_idc 1, frame_mbs_only 1, direct_8x8 1, frame num 4 bits at 6, POC
        // lsb 4 bits at 12.
        assert_eq!(seq_fields(), 1 | 4 | 32 | 4 << 6 | 4 << 12);
        // idr_pic_flag, reference_pic_flag = 1 at bit 1, transform_8x8_mode_flag at 8,
        // deblocking_filter_control_present_flag at 9.
        assert_eq!(pic_fields(true, true), 1 | 2 | 256 | 512);
        assert_eq!(pic_fields(false, false), 2 | 512);
    }

    #[test]
    fn sizes_are_aligned_to_macroblocks_and_cropped() {
        let shape = Shape::of(&setup(1920, 1080));
        assert_eq!((shape.width_mbs, shape.height_mbs), (120, 68));
        assert_eq!((shape.crop_right, shape.crop_bottom), (0, 8));
        let p = sequence_params(&shape, Profile::High, 6_000_000);
        assert_eq!(p.frame_cropping_flag, 1);
        assert_eq!(p.frame_crop_bottom_offset, 4);
        assert_eq!(p.level_idc, 41);
        assert_eq!((p.picture_width_in_mbs, p.picture_height_in_mbs), (120, 68));
        let shape = Shape::of(&setup(1280, 720));
        let p = sequence_params(&shape, Profile::Main, 4_000_000);
        assert_eq!((p.frame_cropping_flag, p.level_idc), (0, 31));
        assert_eq!((p.ip_period, p.max_num_ref_frames), (1, 1));
    }

    #[test]
    fn an_idr_has_no_reference_and_a_p_picture_has_the_last_one() {
        let idr = Position {
            idr: true,
            frame_num: 0,
            poc: 0,
            idr_pic_id: 1,
        };
        let p = picture_params(&idr, 9, 10, Some(11), true);
        assert!(
            p.reference_frames
                .iter()
                .all(|r| r.picture_id == VA_INVALID_ID)
        );
        assert_eq!(p.curr_pic.picture_id, 10);
        assert_eq!(p.pic_fields & 1, 1);

        let at = Position {
            idr: false,
            frame_num: 5,
            poc: 10,
            idr_pic_id: 1,
        };
        let p = picture_params(&at, 9, 10, Some(11), true);
        assert_eq!(p.reference_frames[0].picture_id, 11);
        assert_eq!(p.reference_frames[0].frame_idx, 4);
        assert_eq!(p.reference_frames[0].top_field_order_cnt, 8);
        assert_eq!(p.reference_frames[1].picture_id, VA_INVALID_ID);
        assert_eq!((p.frame_num, p.coded_buf, p.pic_fields & 1), (5, 9, 0));

        let shape = Shape::of(&setup(640, 360));
        let s = slice_params(&at, &shape, Some(11));
        assert_eq!((s.slice_type, s.num_macroblocks), (SLICE_TYPE_P, 40 * 23));
        assert_eq!(s.ref_pic_list0[0].picture_id, 11);
        assert_eq!(s.ref_pic_list1[0].picture_id, VA_INVALID_ID);
        assert_eq!(s.pic_order_cnt_lsb, 10);
        let s = slice_params(&idr, &shape, None);
        assert_eq!(s.slice_type, SLICE_TYPE_I);
        assert_eq!(s.ref_pic_list0[0].picture_id, VA_INVALID_ID);
        assert_eq!(at.slice(0).poc_lsb, 10);
        assert_eq!(
            Position { poc: 300, ..at }.slice(0).poc_lsb,
            300 & 0xff,
            "the count wraps in its 8 bits"
        );
    }

    #[test]
    fn the_rate_control_buffers_carry_the_rate_and_a_restart() {
        let rc = rate_control(4_000_000, true);
        assert_eq!(
            (rc.kind, rc.data.bits_per_second, rc.data.rc_flags),
            (VA_ENC_MISC_RATE_CONTROL, 4_000_000, 1)
        );
        assert_eq!(rate_control(1, false).data.rc_flags, 0);
        let h = hrd(4_000_000);
        assert_eq!(
            (h.data.buffer_size, h.data.initial_buffer_fullness),
            (2_000_000, 1_500_000)
        );
        assert_eq!(frame_rate(60).data.framerate, 60);
    }

    #[test]
    fn a_picture_is_copied_with_its_edges_repeated_into_the_padding() {
        // 4 by 2 pixels into a surface of 6 by 4 with pitches of 8.
        let nv12: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 100, 101, 102, 103];
        let mut data = vec![0xee; 8 * 4 + 8 * 2];
        let layout = (&[8, 8, 0], &[0, 32, 0]);
        copy_nv12(&nv12, (4, 2), &mut data, layout, (6, 4)).unwrap();
        assert_eq!(&data[0..8], [1, 2, 3, 4, 4, 4, 0xee, 0xee]);
        assert_eq!(&data[8..14], [5, 6, 7, 8, 8, 8]);
        assert_eq!(&data[16..22], [5, 6, 7, 8, 8, 8], "the last row repeats");
        assert_eq!(&data[24..30], [5, 6, 7, 8, 8, 8]);
        assert_eq!(
            &data[32..38],
            [100, 101, 102, 103, 102, 103],
            "U/V pairs repeat"
        );
        assert_eq!(&data[40..46], [100, 101, 102, 103, 102, 103]);
        // A surface smaller than the picture is refused, and so is a wrong picture.
        assert!(copy_nv12(&nv12, (4, 2), &mut data[..40], layout, (6, 4)).is_err());
        assert!(copy_nv12(&nv12[1..], (4, 2), &mut data, layout, (6, 4)).is_err());
    }

    /// A GPU is required when the environment says so (CI that has one sets it).
    fn gpu_required() -> bool {
        matches!(
            std::env::var("ANYPLAY_REQUIRE_HW").as_deref(),
            Ok("1") | Ok("vaapi")
        )
    }

    #[test]
    fn opening_without_a_gpu_is_an_error_and_never_a_crash() {
        // Whatever this machine has: Ok on a PC with an encoder, else a reason.
        match VaEncoder::open(setup(640, 360)) {
            Ok(_) => {}
            Err(why) => assert!(!why.is_empty()),
        }
    }

    #[test]
    fn round_trip_when_a_gpu_is_there() {
        let mut encoder = match VaEncoder::open(setup(640, 360)) {
            Ok(encoder) => encoder,
            Err(why) if gpu_required() => panic!("a VA-API encoder is required: {why}"),
            Err(why) => {
                eprintln!("skipped: no VA-API H.264 encoder ({why})");
                return;
            }
        };
        let mut decoder = crate::oh264::OhDecoder::open().ok();
        if decoder.is_none() {
            eprintln!("no OpenH264 library: the pictures are not decoded");
        }
        let mut pattern = TestPattern::new(640, 360);
        let mut keys = 0;
        let mut shown = 0;
        for n in 0..30 {
            let picture = pattern.next_frame();
            let encoded = match encoder.encode(&picture, n == 0) {
                Ok(encoded) => encoded,
                Err(CodecError::Pending) => continue,
                Err(e) => panic!("encode {n}: {e}"),
            };
            keys += usize::from(encoded.key);
            if n == 0 {
                assert!(encoded.key, "the first picture asked for a key frame");
                let types = crate::annexb::nal_types(&encoded.data);
                assert!(types.contains(&7), "an IDR carries its SPS: {types:?}");
                assert!(types.contains(&8), "an IDR carries its PPS: {types:?}");
                assert!(types.contains(&5), "an IDR has an IDR slice: {types:?}");
            }
            if let Some(decoder) = decoder.as_mut() {
                use anyplay_share::codec::VideoDecoder;
                match decoder.decode(&encoded) {
                    Ok(out) => {
                        assert_eq!((out.width, out.height), (640, 360));
                        shown += 1;
                    }
                    Err(CodecError::Pending) => {}
                    Err(e) => panic!("decode of picture {n}: {e}"),
                }
            }
        }
        assert_eq!(keys, 1, "one key frame unless asked");
        if decoder.is_some() {
            assert!(shown >= 25, "only {shown} of 30 pictures came out");
        }
        // The rate follows, and a key frame can be asked for again.
        encoder.set_bitrate(2000);
        let picture = pattern.next_frame();
        let again = encoder.encode(&picture, true).unwrap();
        assert!(again.key);
        assert!(crate::annexb::nal_types(&again.data).contains(&5));
    }
}
