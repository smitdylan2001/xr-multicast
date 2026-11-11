//! Hand-written, minimal FFmpeg FFI for runtime loading.
//!
//! Only the parts of `AVPacket` and `AVFrame` that we touch are declared, and only
//! their leading fields. The layouts were checked against the public headers of
//! every supported release (`libavcodec/packet.h`, `libavutil/frame.h`,
//! `libavutil/version.h` from FFmpeg `release/5.1`, `6.0`, `6.1`, `7.0`, `7.1`,
//! `8.0` and `8.1`) and with `offsetof()` compiled against those headers:
//!
//! - `AVPacket` up to `flags` is identical in libavcodec 59–62.
//! - `AVFrame` up to `colorspace` comes in three layouts, selected by the
//!   libavutil major: 57/58 (FFmpeg 5.x/6.x), 59 (7.x: `coded_picture_number`,
//!   `display_picture_number`, `reordered_opaque`, `channel_layout` removed) and 60
//!   (8.x: `key_frame`, `interlaced_frame`, `top_field_first`,
//!   `palette_has_changed` removed). FFmpeg only appends fields within a major, so
//!   the prefix is stable for every minor of a major.
//!
//! `AVCodecContext` fields are never touched directly (their offsets change between
//! majors); settings go through AVOptions instead.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

pub type AVCodec = c_void;
pub type AVCodecContext = c_void;
pub type AVFrame = c_void;
pub type AVDictionary = c_void;

/// Leading fields of `AVPacket` (identical in libavcodec 59, 60, 61 and 62).
/// Always allocated by `av_packet_alloc`; never construct or size this type.
#[repr(C)]
pub struct AVPacket {
    pub buf: *mut c_void,
    pub pts: i64,
    pub dts: i64,
    pub data: *mut u8,
    pub size: c_int,
    pub stream_index: c_int,
    pub flags: c_int,
}

pub const AV_NUM_DATA_POINTERS: usize = 8;
pub const AV_PKT_FLAG_KEY: c_int = 1;
pub const AV_NOPTS_VALUE: i64 = i64::MIN;
pub const AV_OPT_SEARCH_CHILDREN: c_int = 1;
pub const AV_LOG_QUIET: c_int = -8;
pub const AV_LOG_WARNING: c_int = 24;
pub const AV_FRAME_FLAG_CORRUPT: c_int = 1;

/// `AVERROR(EAGAIN)`. FFmpeg uses the platform's errno values.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
pub const AVERROR_EAGAIN: c_int = -35;
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)))]
pub const AVERROR_EAGAIN: c_int = -11;
/// `AVERROR_EOF` = `-MKTAG('E','O','F',' ')`.
pub const AVERROR_EOF: c_int = -(0x45 | (0x4f << 8) | (0x46 << 16) | (0x20 << 24));

/// `AVFrame` prefix for libavutil 57 and 58 (FFmpeg 5.x, 6.x).
#[repr(C)]
pub struct AVFrameV57 {
    pub data: [*mut u8; AV_NUM_DATA_POINTERS],
    pub linesize: [c_int; AV_NUM_DATA_POINTERS],
    pub extended_data: *mut *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub nb_samples: c_int,
    pub format: c_int,
    pub key_frame: c_int,
    pub pict_type: c_int,
    pub sample_aspect_ratio: [c_int; 2],
    pub pts: i64,
    pub pkt_dts: i64,
    pub time_base: [c_int; 2],
    pub coded_picture_number: c_int,
    pub display_picture_number: c_int,
    pub quality: c_int,
    pub opaque: *mut c_void,
    pub repeat_pict: c_int,
    pub interlaced_frame: c_int,
    pub top_field_first: c_int,
    pub palette_has_changed: c_int,
    pub reordered_opaque: i64,
    pub sample_rate: c_int,
    pub channel_layout: u64,
    pub buf: [*mut c_void; AV_NUM_DATA_POINTERS],
    pub extended_buf: *mut *mut c_void,
    pub nb_extended_buf: c_int,
    pub side_data: *mut *mut c_void,
    pub nb_side_data: c_int,
    pub flags: c_int,
    pub color_range: c_int,
    pub color_primaries: c_int,
    pub color_trc: c_int,
    pub colorspace: c_int,
}

/// `AVFrame` prefix for libavutil 59 (FFmpeg 7.x).
#[repr(C)]
pub struct AVFrameV59 {
    pub data: [*mut u8; AV_NUM_DATA_POINTERS],
    pub linesize: [c_int; AV_NUM_DATA_POINTERS],
    pub extended_data: *mut *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub nb_samples: c_int,
    pub format: c_int,
    pub key_frame: c_int,
    pub pict_type: c_int,
    pub sample_aspect_ratio: [c_int; 2],
    pub pts: i64,
    pub pkt_dts: i64,
    pub time_base: [c_int; 2],
    pub quality: c_int,
    pub opaque: *mut c_void,
    pub repeat_pict: c_int,
    pub interlaced_frame: c_int,
    pub top_field_first: c_int,
    pub palette_has_changed: c_int,
    pub sample_rate: c_int,
    pub buf: [*mut c_void; AV_NUM_DATA_POINTERS],
    pub extended_buf: *mut *mut c_void,
    pub nb_extended_buf: c_int,
    pub side_data: *mut *mut c_void,
    pub nb_side_data: c_int,
    pub flags: c_int,
    pub color_range: c_int,
    pub color_primaries: c_int,
    pub color_trc: c_int,
    pub colorspace: c_int,
}

/// `AVFrame` prefix for libavutil 60 (FFmpeg 8.x).
#[repr(C)]
pub struct AVFrameV60 {
    pub data: [*mut u8; AV_NUM_DATA_POINTERS],
    pub linesize: [c_int; AV_NUM_DATA_POINTERS],
    pub extended_data: *mut *mut u8,
    pub width: c_int,
    pub height: c_int,
    pub nb_samples: c_int,
    pub format: c_int,
    pub pict_type: c_int,
    pub sample_aspect_ratio: [c_int; 2],
    pub pts: i64,
    pub pkt_dts: i64,
    pub time_base: [c_int; 2],
    pub quality: c_int,
    pub opaque: *mut c_void,
    pub repeat_pict: c_int,
    pub sample_rate: c_int,
    pub buf: [*mut c_void; AV_NUM_DATA_POINTERS],
    pub extended_buf: *mut *mut c_void,
    pub nb_extended_buf: c_int,
    pub side_data: *mut *mut c_void,
    pub nb_side_data: c_int,
    pub flags: c_int,
    pub color_range: c_int,
    pub color_primaries: c_int,
    pub color_trc: c_int,
    pub colorspace: c_int,
}

/// Which `AVFrame` prefix the loaded libavutil uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLayout {
    V57,
    V59,
    V60,
}

impl FrameLayout {
    pub fn for_avutil_major(major: u32) -> Option<FrameLayout> {
        match major {
            57 | 58 => Some(FrameLayout::V57),
            59 => Some(FrameLayout::V59),
            60 => Some(FrameLayout::V60),
            _ => None,
        }
    }
}

/// The fields we read from a decoded `AVFrame`.
#[derive(Debug, Clone, Copy)]
pub struct FrameFields {
    pub data: [*mut u8; AV_NUM_DATA_POINTERS],
    pub linesize: [c_int; AV_NUM_DATA_POINTERS],
    pub width: c_int,
    pub height: c_int,
    pub format: c_int,
    pub pts: i64,
    pub flags: c_int,
    pub color_range: c_int,
    pub colorspace: c_int,
}

macro_rules! read_fields {
    ($ty:ty, $ptr:expr) => {{
        let f = unsafe { &*($ptr as *const $ty) };
        FrameFields {
            data: f.data,
            linesize: f.linesize,
            width: f.width,
            height: f.height,
            format: f.format,
            pts: f.pts,
            flags: f.flags,
            color_range: f.color_range,
            colorspace: f.colorspace,
        }
    }};
}

/// Read the fields we need from a frame allocated by `av_frame_alloc`.
///
/// # Safety
/// `frame` must point to a valid `AVFrame` from the libavutil whose major matches `layout`.
pub unsafe fn read_frame(layout: FrameLayout, frame: *const AVFrame) -> FrameFields {
    match layout {
        FrameLayout::V57 => read_fields!(AVFrameV57, frame),
        FrameLayout::V59 => read_fields!(AVFrameV59, frame),
        FrameLayout::V60 => read_fields!(AVFrameV60, frame),
    }
}

pub type FnVersion = unsafe extern "C" fn() -> c_uint;
pub type FnVersionInfo = unsafe extern "C" fn() -> *const c_char;
pub type FnFindDecoderByName = unsafe extern "C" fn(*const c_char) -> *const AVCodec;
pub type FnAllocContext3 = unsafe extern "C" fn(*const AVCodec) -> *mut AVCodecContext;
pub type FnOpen2 =
    unsafe extern "C" fn(*mut AVCodecContext, *const AVCodec, *mut *mut AVDictionary) -> c_int;
pub type FnFreeContext = unsafe extern "C" fn(*mut *mut AVCodecContext);
pub type FnFlushBuffers = unsafe extern "C" fn(*mut AVCodecContext);
pub type FnPacketAlloc = unsafe extern "C" fn() -> *mut AVPacket;
pub type FnPacketFree = unsafe extern "C" fn(*mut *mut AVPacket);
pub type FnNewPacket = unsafe extern "C" fn(*mut AVPacket, c_int) -> c_int;
pub type FnPacketUnref = unsafe extern "C" fn(*mut AVPacket);
pub type FnSendPacket = unsafe extern "C" fn(*mut AVCodecContext, *const AVPacket) -> c_int;
pub type FnReceiveFrame = unsafe extern "C" fn(*mut AVCodecContext, *mut AVFrame) -> c_int;
pub type FnFrameAlloc = unsafe extern "C" fn() -> *mut AVFrame;
pub type FnFrameFree = unsafe extern "C" fn(*mut *mut AVFrame);
pub type FnFrameUnref = unsafe extern "C" fn(*mut AVFrame);
pub type FnOptSet = unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int;
pub type FnOptSetInt = unsafe extern "C" fn(*mut c_void, *const c_char, i64, c_int) -> c_int;
pub type FnGetPixFmt = unsafe extern "C" fn(*const c_char) -> c_int;
pub type FnStrError = unsafe extern "C" fn(c_int, *mut c_char, usize) -> c_int;
pub type FnLogSetLevel = unsafe extern "C" fn(c_int);

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    /// Offsets printed by `offsetof()` against the official headers on LP64/LLP64
    /// (x86_64, aarch64 Linux/macOS/Android, x86_64 Windows).
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn layouts_match_ffmpeg_headers() {
        assert_eq!(offset_of!(AVPacket, pts), 8);
        assert_eq!(offset_of!(AVPacket, data), 24);
        assert_eq!(offset_of!(AVPacket, size), 32);
        assert_eq!(offset_of!(AVPacket, flags), 40);

        macro_rules! offsets {
            ($t:ty) => {
                (
                    offset_of!($t, linesize),
                    offset_of!($t, width),
                    offset_of!($t, height),
                    offset_of!($t, format),
                    offset_of!($t, pts),
                    offset_of!($t, flags),
                    offset_of!($t, color_range),
                    offset_of!($t, colorspace),
                )
            };
        }
        assert_eq!(
            offsets!(AVFrameV57),
            (64, 104, 108, 116, 136, 316, 320, 332)
        );
        assert_eq!(
            offsets!(AVFrameV59),
            (64, 104, 108, 116, 136, 292, 296, 308)
        );
        assert_eq!(
            offsets!(AVFrameV60),
            (64, 104, 108, 116, 136, 276, 280, 292)
        );
    }

    #[test]
    fn error_codes() {
        assert_eq!(AVERROR_EOF, -0x20464F45);
    }
}
