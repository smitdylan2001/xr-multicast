//! FFmpeg backend: `libavcodec` / `libavutil` loaded at runtime with `libloading`.
//!
//! Nothing is linked at build time, so the crate builds everywhere and works
//! without FFmpeg (falling back to OpenH264). Supported: libavcodec 59–62
//! (FFmpeg 5.x–8.x). Unknown majors are refused because the `AVFrame` layout we
//! read could differ.
//!
//! Hardware decoding (see [`hw`]): the decoder gets `hw_device_ctx` and a
//! `get_format` callback that only accepts the device's hw pixel format; decoded
//! surfaces are downloaded with `av_hwframe_transfer_data` (NV12, or P010 for
//! 10-bit). If the hardware path fails before it produced a picture, the
//! download fails, or frames come out wrong, the stream is reopened in software
//! (keeping its config; a failed key frame is re-decoded at once).

pub mod hw;
pub mod sys;

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use libloading::Library;

use crate::stream::{Prepared, StreamState};
use crate::{Codec, ColorInfo, DecodeError, Decoder, DecoderOptions, Frame, PixelFormat, Plane};
use sys::*;

/// (libavcodec major, libavutil major, FFmpeg release), newest first.
const MAJORS: [(u32, u32, &str); 4] = [(62, 60, "8"), (61, 59, "7"), (60, 58, "6"), (59, 57, "5")];

/// A loaded FFmpeg. Lives for the whole process (kept in a static).
pub struct Lib {
    _avutil: Library,
    _avcodec: Library,
    pub avcodec_version: u32,
    pub avutil_version: u32,
    /// `av_version_info()`, e.g. `8.0.1` or `n7.1`.
    pub version_info: String,
    pub path: PathBuf,
    layout: FrameLayout,
    /// `AVCodecContext` offsets for hardware decoding (`None`: unverified major / 32-bit).
    pub(crate) ctx_offsets: Option<CodecCtxOffsets>,
    pub(crate) hw: Option<hw::HwFns>,
    pix_yuv420p: c_int,
    pix_yuvj420p: c_int,
    pix_nv12: c_int,
    pix_yuv420p10le: c_int,
    pix_p010le: c_int,

    find_decoder_by_name: FnFindDecoderByName,
    alloc_context3: FnAllocContext3,
    open2: FnOpen2,
    free_context: FnFreeContext,
    flush_buffers: FnFlushBuffers,
    packet_alloc: FnPacketAlloc,
    packet_free: FnPacketFree,
    new_packet: FnNewPacket,
    packet_unref: FnPacketUnref,
    send_packet: FnSendPacket,
    receive_frame: FnReceiveFrame,
    frame_alloc: FnFrameAlloc,
    frame_free: FnFrameFree,
    frame_unref: FnFrameUnref,
    opt_set: FnOptSet,
    opt_set_int: FnOptSetInt,
    strerror: FnStrError,
}

fn ver(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

impl Lib {
    /// Human-readable version, e.g. `FFmpeg 8.0.1 (libavcodec 62.28.102)`.
    pub fn describe(&self) -> String {
        format!(
            "FFmpeg {} (libavcodec {}, libavutil {})",
            self.version_info,
            ver(self.avcodec_version),
            ver(self.avutil_version)
        )
    }

    pub(crate) fn err(&self, code: c_int) -> String {
        let mut buf = [0 as c_char; 128];
        // SAFETY: buffer and length are valid; av_strerror always NUL-terminates.
        unsafe { (self.strerror)(code, buf.as_mut_ptr(), buf.len()) };
        let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        format!("{msg} ({code})")
    }

    /// Whether a decoder for `codec` exists and opens.
    pub fn probe(&'static self, codec: Codec) -> Option<&'static str> {
        decoder_names(codec).iter().copied().find(|name| {
            let opts = DecoderOptions {
                prefer_hw: false,
                threads: 1,
            };
            FfmpegDecoder::open_named(self, codec, name, opts, None).is_ok()
        })
    }
}

fn decoder_names(codec: Codec) -> &'static [&'static str] {
    match codec {
        Codec::H264 => &["h264"],
        Codec::H265 => &["hevc"],
        // FFmpeg's native "av1" decoder only works with a hardware accelerator,
        // so it is not used for software decoding.
        Codec::Av1 => &["libdav1d", "libaom-av1"],
    }
}

/// The process-wide FFmpeg, if one was found (searched once).
pub fn lib() -> Option<&'static Lib> {
    static LIB: OnceLock<Option<Lib>> = OnceLock::new();
    LIB.get_or_init(|| {
        if std::env::var_os("XRMC_FFMPEG_DISABLE").is_some_and(|v| !v.is_empty() && v != "0") {
            tracing::info!("FFmpeg disabled by XRMC_FFMPEG_DISABLE");
            return None;
        }
        let lib = find();
        match &lib {
            Some(l) => tracing::info!("decoder: {} from {}", l.describe(), l.path.display()),
            None => tracing::info!(
                "decoder: no supported FFmpeg found (libavcodec 59-62); using OpenH264 only"
            ),
        }
        lib
    })
    .as_ref()
}

fn lib_names(codec_major: u32, util_major: u32) -> (String, String) {
    if cfg!(target_os = "windows") {
        (
            format!("avcodec-{codec_major}.dll"),
            format!("avutil-{util_major}.dll"),
        )
    } else if cfg!(any(target_os = "macos", target_os = "ios")) {
        (
            format!("libavcodec.{codec_major}.dylib"),
            format!("libavutil.{util_major}.dylib"),
        )
    } else {
        (
            format!("libavcodec.so.{codec_major}"),
            format!("libavutil.so.{util_major}"),
        )
    }
}

fn unversioned_names() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("avcodec.dll", "avutil.dll")
    } else if cfg!(any(target_os = "macos", target_os = "ios")) {
        ("libavcodec.dylib", "libavutil.dylib")
    } else {
        ("libavcodec.so", "libavutil.so")
    }
}

/// Directories searched before the system library path.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("XRMC_FFMPEG_DIR").filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        dirs.push(exe_dir.join("ffmpeg"));
        if cfg!(target_os = "macos") {
            dirs.push(exe_dir.join("../Frameworks"));
            dirs.push(exe_dir.join("../lib"));
        }
        dirs.push(exe_dir);
    }
    if cfg!(target_os = "macos") {
        for d in ["/opt/homebrew/lib", "/usr/local/lib", "/opt/local/lib"] {
            dirs.push(PathBuf::from(d));
        }
    }
    dirs
}

fn find() -> Option<Lib> {
    let dirs = search_dirs();
    // Explicit directories first (newest major first in each), then the system path.
    for dir in &dirs {
        for (cm, um, _) in MAJORS {
            let (codec, util) = lib_names(cm, um);
            let (cp, up) = (dir.join(&codec), dir.join(&util));
            if cp.exists() && up.exists() {
                match unsafe { load(&cp, &up, Some((cm, um))) } {
                    Ok(lib) => return Some(lib),
                    Err(e) => tracing::warn!("FFmpeg at {}: {e}", cp.display()),
                }
            }
        }
    }
    for (cm, um, _) in MAJORS {
        let (codec, util) = lib_names(cm, um);
        match unsafe { load(Path::new(&codec), Path::new(&util), Some((cm, um))) } {
            Ok(lib) => return Some(lib),
            Err(e) => tracing::debug!("FFmpeg {codec}: {e}"),
        }
    }
    // Unversioned names (Android apps, dev symlinks): accepted only if the version is supported.
    let (codec, util) = unversioned_names();
    for dir in dirs.iter().map(|d| Some(d.as_path())).chain([None]) {
        let (cp, up) = match dir {
            Some(d) => (d.join(codec), d.join(util)),
            None => (PathBuf::from(codec), PathBuf::from(util)),
        };
        if dir.is_some() && !(cp.exists() && up.exists()) {
            continue;
        }
        match unsafe { load(&cp, &up, None) } {
            Ok(lib) => return Some(lib),
            Err(e) => tracing::debug!("FFmpeg {}: {e}", cp.display()),
        }
    }
    None
}

#[cfg(windows)]
unsafe fn open_library(path: &Path) -> Result<Library, libloading::Error> {
    use libloading::os::windows::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, Library as WinLibrary,
    };
    if path.is_absolute() || path.components().count() > 1 {
        // Let dependent DLLs (avutil, swresample, dav1d, ...) resolve from the same folder.
        unsafe {
            WinLibrary::load_with_flags(
                path,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        }
        .map(Into::into)
    } else {
        unsafe { Library::new(path) }
    }
}

#[cfg(not(windows))]
unsafe fn open_library(path: &Path) -> Result<Library, libloading::Error> {
    unsafe { Library::new(path) }
}

/// Copy a function pointer out of a library (valid while the library stays loaded).
///
/// # Safety
/// `T` must be the symbol's real type.
unsafe fn symbol<T: Copy>(lib: &Library, name: &str) -> Result<T, String> {
    let s: libloading::Symbol<'_, T> =
        unsafe { lib.get(name.as_bytes()) }.map_err(|e| format!("{name}: {e}"))?;
    Ok(*s)
}

/// Load libavutil then libavcodec and resolve every symbol we use.
///
/// # Safety
/// Loads and initialises foreign libraries.
unsafe fn load(
    codec_path: &Path,
    util_path: &Path,
    expect: Option<(u32, u32)>,
) -> Result<Lib, String> {
    // libavutil first: libavcodec depends on it, and loading it by full path first
    // makes a bundled copy win over a system one with the same soname.
    let avutil = unsafe { open_library(util_path) }.map_err(|e| e.to_string())?;
    let avcodec = unsafe { open_library(codec_path) }.map_err(|e| e.to_string())?;

    macro_rules! sym {
        ($lib:expr, $name:literal) => {
            unsafe { symbol(&$lib, $name) }?
        };
    }

    let avutil_version: FnVersion = sym!(avutil, "avutil_version");
    let avcodec_version: FnVersion = sym!(avcodec, "avcodec_version");
    let (cv, uv) = unsafe { (avcodec_version(), avutil_version()) };
    let (cm, um) = (cv >> 16, uv >> 16);
    if let Some((ecm, eum)) = expect
        && (cm != ecm || um != eum)
    {
        return Err(format!(
            "version mismatch: libavcodec {cm} / libavutil {um}, expected {ecm} / {eum}"
        ));
    }
    if !MAJORS.iter().any(|&(c, u, _)| c == cm && u == um) {
        return Err(format!(
            "unsupported FFmpeg: libavcodec {} / libavutil {}",
            ver(cv),
            ver(uv)
        ));
    }
    let layout =
        FrameLayout::for_avutil_major(um).ok_or_else(|| format!("unsupported libavutil {um}"))?;

    let version_info: FnVersionInfo = sym!(avutil, "av_version_info");
    let get_pix_fmt: FnGetPixFmt = sym!(avutil, "av_get_pix_fmt");
    let log_set_level: FnLogSetLevel = sym!(avutil, "av_log_set_level");

    let version_info = unsafe {
        let p = version_info();
        if p.is_null() {
            String::from("?")
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    let pix = |name: &CStr| unsafe { get_pix_fmt(name.as_ptr()) };
    let hw = (|| -> Result<hw::HwFns, String> {
        Ok(hw::HwFns {
            get_hw_config: sym!(avcodec, "avcodec_get_hw_config"),
            device_ctx_create: sym!(avutil, "av_hwdevice_ctx_create"),
            find_type_by_name: sym!(avutil, "av_hwdevice_find_type_by_name"),
            get_type_name: sym!(avutil, "av_hwdevice_get_type_name"),
            buffer_ref: sym!(avutil, "av_buffer_ref"),
            buffer_unref: sym!(avutil, "av_buffer_unref"),
            transfer_data: sym!(avutil, "av_hwframe_transfer_data"),
            pix_fmt_name: sym!(avutil, "av_get_pix_fmt_name"),
        })
    })()
    .map_err(|e| tracing::debug!("FFmpeg without hardware decoding: {e}"))
    .ok();
    let level = if std::env::var_os("XRMC_FFMPEG_LOG").is_some() {
        AV_LOG_WARNING
    } else {
        AV_LOG_QUIET
    };
    unsafe { log_set_level(level) };

    Ok(Lib {
        avcodec_version: cv,
        avutil_version: uv,
        version_info,
        path: codec_path.to_path_buf(),
        layout,
        ctx_offsets: CodecCtxOffsets::for_avcodec_major(cm),
        hw,
        pix_yuv420p: pix(c"yuv420p"),
        pix_yuvj420p: pix(c"yuvj420p"),
        pix_nv12: pix(c"nv12"),
        pix_yuv420p10le: pix(c"yuv420p10le"),
        pix_p010le: pix(c"p010le"),
        find_decoder_by_name: sym!(avcodec, "avcodec_find_decoder_by_name"),
        alloc_context3: sym!(avcodec, "avcodec_alloc_context3"),
        open2: sym!(avcodec, "avcodec_open2"),
        free_context: sym!(avcodec, "avcodec_free_context"),
        flush_buffers: sym!(avcodec, "avcodec_flush_buffers"),
        packet_alloc: sym!(avcodec, "av_packet_alloc"),
        packet_free: sym!(avcodec, "av_packet_free"),
        new_packet: sym!(avcodec, "av_new_packet"),
        packet_unref: sym!(avcodec, "av_packet_unref"),
        send_packet: sym!(avcodec, "avcodec_send_packet"),
        receive_frame: sym!(avcodec, "avcodec_receive_frame"),
        frame_alloc: sym!(avutil, "av_frame_alloc"),
        frame_free: sym!(avutil, "av_frame_free"),
        frame_unref: sym!(avutil, "av_frame_unref"),
        opt_set: sym!(avutil, "av_opt_set"),
        opt_set_int: sym!(avutil, "av_opt_set_int"),
        strerror: sym!(avutil, "av_strerror"),
        _avutil: avutil,
        _avcodec: avcodec,
    })
}

/// State shared with the `get_format` callback (`AVCodecContext.opaque` points here).
struct HwFormat {
    /// The hw pixel format of the decoder's device (e.g. `vaapi`).
    pix_fmt: c_int,
    /// HEVC: refuse streams whose profile says 8-bit but whose samples aren't.
    hevc: bool,
    /// 8-bit 4:2:0 software formats (`yuv420p`, `yuvj420p`).
    eight_bit: [c_int; 2],
    /// The decoder offered no hw format (unsupported profile, size, …) or the
    /// stream was refused.
    refused: AtomicBool,
    /// `profile` and `sw_pix_fmt` seen by the last `get_format` call (for tests).
    seen: [AtomicI32; 2],
}

/// `get_format`: take the device's hw format when it is offered, else fail the
/// decode (the stream then falls back to a software decoder with full threading).
/// Must not panic or allocate.
///
/// HEVC streams that signal the Main (8-bit) profile but carry 10-bit samples
/// (seen from GStreamer's `vah265enc`) are refused: FFmpeg's VA-API hwaccel picks
/// the VA profile from `profile` and gets 8-bit surfaces, and the AMD VCN then
/// writes past them (GPU page fault, ring reset, and Mesa aborts the process).
unsafe extern "C" fn get_hw_format(ctx: *mut AVCodecContext, fmts: *const c_int) -> c_int {
    let Some(off) = lib().and_then(|l| l.ctx_offsets) else {
        return AV_PIX_FMT_NONE;
    };
    // SAFETY: `opaque` was set to a live `HwFormat` owned by the decoder, which
    // frees the context before the `HwFormat`; `fmts` is terminated by NONE.
    unsafe {
        let state = CodecCtxOffsets::read::<*const HwFormat>(ctx, off.opaque);
        if state.is_null() || fmts.is_null() {
            return AV_PIX_FMT_NONE;
        }
        let state = &*state;
        let profile: c_int = CodecCtxOffsets::read(ctx, off.profile);
        let sw_pix_fmt: c_int = CodecCtxOffsets::read(ctx, off.sw_pix_fmt);
        state.seen[0].store(profile, Ordering::Relaxed);
        state.seen[1].store(sw_pix_fmt, Ordering::Relaxed);
        if state.hevc
            && (profile == AV_PROFILE_HEVC_MAIN || profile == AV_PROFILE_HEVC_MAIN_STILL_PICTURE)
            && !state.eight_bit.contains(&sw_pix_fmt)
        {
            state.refused.store(true, Ordering::Relaxed);
            return AV_PIX_FMT_NONE;
        }
        let mut p = fmts;
        while *p != AV_PIX_FMT_NONE {
            if *p == state.pix_fmt {
                return *p;
            }
            p = p.add(1);
        }
        state.refused.store(true, Ordering::Relaxed);
    }
    AV_PIX_FMT_NONE
}

/// Hardware decoding state of a decoder.
struct HwActive {
    /// `None` for decoders that are hardware by themselves (V4L2 M2M: frames are
    /// already in system memory).
    format: Option<Box<HwFormat>>,
    /// A correct frame came out of the hardware path.
    verified: bool,
    /// Consecutive key frames that failed to decode.
    key_errors: u32,
}

/// Why a decode call failed.
enum Fail {
    /// The hardware path is unusable: switch to software.
    Hw(String),
    /// A decoder error: flush, then wait for the next key frame.
    Decode(&'static str, c_int),
    /// Returned as is.
    Other(DecodeError),
}

/// Largest frame size accepted from a hardware decoder (anything else is garbage).
const MAX_HW_DIM: c_int = 16384;

/// One FFmpeg decoder instance.
pub struct FfmpegDecoder {
    lib: &'static Lib,
    ctx: *mut AVCodecContext,
    pkt: *mut AVPacket,
    frame: *mut AVFrame,
    /// Download target for hw frames (null for software decoders).
    sw_frame: *mut AVFrame,
    name: String,
    stream: StreamState,
    codec: Codec,
    options: DecoderOptions,
    hw: Option<HwActive>,
}

// SAFETY: the FFmpeg objects are owned exclusively by this value and only used
// through `&mut self`; FFmpeg contexts may move between threads.
unsafe impl Send for FfmpegDecoder {}

impl FfmpegDecoder {
    /// Open a decoder for `codec`: in hardware when `options.prefer_hw` and the
    /// probe found a working hardware path, else the first available software decoder.
    pub fn open(
        lib: &'static Lib,
        codec: Codec,
        options: DecoderOptions,
    ) -> Result<FfmpegDecoder, DecodeError> {
        if options.prefer_hw
            && let Some(path) = hw::support(lib).for_codec(codec)
        {
            match Self::open_named(lib, codec, path.decoder, options, Some(path)) {
                Ok(d) => return Ok(d),
                Err(e) => tracing::debug!("{} {}: {e}", path.label, codec.as_str()),
            }
        }
        Self::open_software(lib, codec, options)
    }

    fn open_software(
        lib: &'static Lib,
        codec: Codec,
        options: DecoderOptions,
    ) -> Result<FfmpegDecoder, DecodeError> {
        let mut last = None;
        for name in decoder_names(codec) {
            match Self::open_named(lib, codec, name, options, None) {
                Ok(d) => return Ok(d),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or(DecodeError::Unsupported(codec)))
    }

    fn open_named(
        lib: &'static Lib,
        codec: Codec,
        name: &str,
        options: DecoderOptions,
        hw_path: Option<&hw::HwPath>,
    ) -> Result<FfmpegDecoder, DecodeError> {
        let cname = CString::new(name).expect("decoder name");
        // SAFETY: plain FFmpeg API calls; every allocation is owned by `dec` (freed in Drop).
        unsafe {
            let avcodec = (lib.find_decoder_by_name)(cname.as_ptr());
            if avcodec.is_null() {
                return Err(DecodeError::Unsupported(codec));
            }
            let mut dec = FfmpegDecoder {
                lib,
                ctx: (lib.alloc_context3)(avcodec),
                pkt: (lib.packet_alloc)(),
                frame: (lib.frame_alloc)(),
                sw_frame: std::ptr::null_mut(),
                name: format!("ffmpeg:{name}"),
                stream: StreamState::new(codec),
                codec,
                options,
                hw: None,
            };
            if dec.ctx.is_null() || dec.pkt.is_null() || dec.frame.is_null() {
                return Err(DecodeError::Decode("FFmpeg allocation failed".into()));
            }
            if let Some(path) = hw_path {
                dec.attach_hw(avcodec, path)?;
            }
            // Low latency: no frame threading (it delays output by threads-1 frames),
            // output every picture as soon as it is decoded. Hardware decoders
            // don't use the CPU threads.
            let threads = if dec.hw.is_some() { 1 } else { options.threads };
            dec.set_int(c"threads", i64::from(threads));
            dec.set(c"thread_type", c"slice");
            dec.set(c"flags", c"+low_delay");
            dec.set(c"flags2", c"+fast");
            if name == "libdav1d" {
                dec.set(c"max_frame_delay", c"1");
            }
            if name == "libaom-av1" {
                dec.set_int(c"row-mt", 1);
            }
            let ret = (lib.open2)(dec.ctx, avcodec, std::ptr::null_mut());
            if ret < 0 {
                return Err(DecodeError::Decode(format!(
                    "avcodec_open2({}): {}",
                    dec.name,
                    lib.err(ret)
                )));
            }
            Ok(dec)
        }
    }

    /// Set up hardware decoding on a fresh (unopened) context.
    fn attach_hw(&mut self, avcodec: *const AVCodec, path: &hw::HwPath) -> Result<(), DecodeError> {
        let lib = self.lib;
        let unsupported = |why: String| DecodeError::Decode(format!("{}: {why}", path.label));
        self.name = if path.api == "v4l2m2m" {
            format!("ffmpeg:{}", path.decoder)
        } else {
            format!("ffmpeg:{}+{}", path.decoder, path.api)
        };
        let Some(device) = path.device else {
            self.hw = Some(HwActive {
                format: None,
                verified: false,
                key_errors: 0,
            });
            return Ok(());
        };
        let (Some(fns), Some(off)) = (&lib.hw, lib.ctx_offsets) else {
            return Err(unsupported("needs a verified libavcodec".into()));
        };
        let pix_fmt = hw::device_pix_fmt(lib, avcodec, device.type_id)
            .ok_or_else(|| unsupported("decoder doesn't support the device".into()))?;
        // SAFETY: `ctx` is a fresh AVCodecContext of the libavcodec major the
        // offsets were verified for. The defaults are checked first as a guard
        // against a wrong layout.
        unsafe {
            let opaque: *mut c_void = CodecCtxOffsets::read(self.ctx, off.opaque);
            let device_ctx: *mut c_void = CodecCtxOffsets::read(self.ctx, off.hw_device_ctx);
            let get_format: *const c_void = CodecCtxOffsets::read(self.ctx, off.get_format);
            if !opaque.is_null() || !device_ctx.is_null() || get_format.is_null() {
                return Err(unsupported("unexpected AVCodecContext layout".into()));
            }
            let device_ref = device.open_ref(lib, path.own_device);
            if device_ref.is_null() {
                return Err(unsupported("av_buffer_ref failed".into()));
            }
            self.sw_frame = (lib.frame_alloc)();
            if self.sw_frame.is_null() {
                let mut r = device_ref;
                (fns.buffer_unref)(&mut r);
                return Err(DecodeError::Decode("FFmpeg allocation failed".into()));
            }
            let format = Box::new(HwFormat {
                pix_fmt,
                hevc: self.codec == Codec::H265,
                eight_bit: [lib.pix_yuv420p, lib.pix_yuvj420p],
                refused: AtomicBool::new(false),
                seen: [AtomicI32::new(i32::MIN), AtomicI32::new(i32::MIN)],
            });
            // The context owns `device_ref` from here (avcodec_free_context unrefs it).
            CodecCtxOffsets::write(self.ctx, off.hw_device_ctx, device_ref);
            CodecCtxOffsets::write(
                self.ctx,
                off.opaque,
                &*format as *const HwFormat as *mut c_void,
            );
            CodecCtxOffsets::write::<GetFormatFn>(self.ctx, off.get_format, get_hw_format);
            self.hw = Some(HwActive {
                format: Some(format),
                verified: false,
                key_errors: 0,
            });
        }
        Ok(())
    }

    fn set(&mut self, key: &CStr, val: &CStr) {
        // SAFETY: ctx is a valid AVCodecContext; options are searched in the codec's private class too.
        let ret = unsafe {
            (self.lib.opt_set)(self.ctx, key.as_ptr(), val.as_ptr(), AV_OPT_SEARCH_CHILDREN)
        };
        if ret < 0 {
            tracing::debug!(
                "{}: option {:?}={:?}: {}",
                self.name,
                key,
                val,
                self.lib.err(ret)
            );
        }
    }

    fn set_int(&mut self, key: &CStr, val: i64) {
        // SAFETY: as above.
        let ret =
            unsafe { (self.lib.opt_set_int)(self.ctx, key.as_ptr(), val, AV_OPT_SEARCH_CHILDREN) };
        if ret < 0 {
            tracing::debug!(
                "{}: option {:?}={val}: {}",
                self.name,
                key,
                self.lib.err(ret)
            );
        }
    }

    fn fail(&mut self, what: &str, code: c_int) -> DecodeError {
        // SAFETY: valid context; drops references so the next key frame starts clean.
        unsafe { (self.lib.flush_buffers)(self.ctx) };
        self.stream.on_error();
        DecodeError::Decode(format!("{}: {what}: {}", self.name, self.lib.err(code)))
    }

    /// Classify a decoder error: hardware faults switch to software.
    fn error(&mut self, what: &'static str, code: c_int, key: bool) -> Fail {
        let Some(hw) = &mut self.hw else {
            return Fail::Decode(what, code);
        };
        if key {
            hw.key_errors += 1;
        }
        let refused = hw
            .format
            .as_ref()
            .is_some_and(|f| f.refused.load(Ordering::Relaxed));
        if refused {
            Fail::Hw("the hardware doesn't support this stream".into())
        } else if !hw.verified || hw.key_errors >= 3 {
            Fail::Hw(format!("{what}: {}", self.lib.err(code)))
        } else {
            Fail::Decode(what, code)
        }
    }

    fn send(&mut self, data: Option<&[u8]>, pts: u64, key: bool) -> c_int {
        let Some(data) = data else {
            // SAFETY: a NULL packet starts draining.
            return unsafe { (self.lib.send_packet)(self.ctx, std::ptr::null()) };
        };
        let Ok(len) = c_int::try_from(data.len()) else {
            return -22; // AVERROR(EINVAL)
        };
        // SAFETY: av_new_packet allocates `len` bytes (+ zeroed padding) at pkt->data.
        unsafe {
            let ret = (self.lib.new_packet)(self.pkt, len);
            if ret < 0 {
                return ret;
            }
            let pkt = &mut *self.pkt;
            std::ptr::copy_nonoverlapping(data.as_ptr(), pkt.data, data.len());
            pkt.pts = pts.min(i64::MAX as u64) as i64;
            pkt.dts = pkt.pts;
            pkt.flags = if key { AV_PKT_FLAG_KEY } else { 0 };
            let ret = (self.lib.send_packet)(self.ctx, self.pkt);
            (self.lib.packet_unref)(self.pkt);
            ret
        }
    }

    /// Decode one access unit (no fallback handling).
    fn decode(&mut self, payload: &[u8], pts: u64, key: bool) -> Result<Vec<Frame>, Fail> {
        let mut out = Vec::new();
        let mut ret = self.send(Some(payload), pts, key);
        if ret == AVERROR_EAGAIN {
            // Output pending (should not happen in low-delay mode): drain, then retry once.
            self.drain(pts, key, &mut out)?;
            ret = self.send(Some(payload), pts, key);
        }
        if ret < 0 {
            return Err(self.error("send_packet", ret, key));
        }
        self.stream.fed();
        self.drain(pts, key, &mut out)?;
        if key
            && !out.is_empty()
            && let Some(hw) = &mut self.hw
        {
            hw.key_errors = 0;
        }
        Ok(out)
    }

    fn drain(&mut self, fallback_pts: u64, key: bool, out: &mut Vec<Frame>) -> Result<(), Fail> {
        loop {
            // SAFETY: valid context and frame.
            let ret = unsafe { (self.lib.receive_frame)(self.ctx, self.frame) };
            if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
                return Ok(());
            }
            if ret < 0 {
                return Err(self.error("receive_frame", ret, key));
            }
            let frame = self.take_frame(fallback_pts);
            // SAFETY: valid frames.
            unsafe {
                (self.lib.frame_unref)(self.frame);
                if !self.sw_frame.is_null() {
                    (self.lib.frame_unref)(self.sw_frame);
                }
            }
            let frame = frame?;
            if let Some(hw) = &mut self.hw {
                hw.verified = true;
            }
            out.push(frame);
        }
    }

    /// Convert the received frame (downloading it first if it's a hw surface).
    fn take_frame(&mut self, fallback_pts: u64) -> Result<Frame, Fail> {
        let lib = self.lib;
        // SAFETY: the frame was just filled by the libavutil this layout belongs to.
        let fields = unsafe { read_frame(lib.layout, self.frame) };
        let Some(hw) = &self.hw else {
            return self.convert(&fields, fallback_pts).map_err(Fail::Other);
        };
        let Some(format) = &hw.format else {
            // V4L2 M2M: already in system memory.
            return self
                .convert(&fields, fallback_pts)
                .map_err(|e| Fail::Hw(e.to_string()));
        };
        if fields.format != format.pix_fmt {
            return Err(Fail::Hw(format!(
                "decoder output {} instead of hardware frames",
                hw::pix_fmt_name(lib, fields.format)
            )));
        }
        let fns = lib.hw.as_ref().expect("hw functions");
        // SAFETY: `frame` holds a hw surface; `sw_frame` is empty, so FFmpeg
        // allocates it in the surface's download format (NV12 / P010).
        let ret = unsafe { (fns.transfer_data)(self.sw_frame, self.frame, 0) };
        if ret < 0 {
            return Err(Fail::Hw(format!(
                "av_hwframe_transfer_data: {}",
                lib.err(ret)
            )));
        }
        // SAFETY: as above.
        let sw = unsafe { read_frame(lib.layout, self.sw_frame) };
        if sw.width != fields.width
            || sw.height != fields.height
            || !(1..=MAX_HW_DIM).contains(&sw.width)
            || !(1..=MAX_HW_DIM).contains(&sw.height)
        {
            return Err(Fail::Hw(format!(
                "bad frame size {}x{} (surface {}x{})",
                sw.width, sw.height, fields.width, fields.height
            )));
        }
        let merged = FrameFields {
            data: sw.data,
            linesize: sw.linesize,
            width: sw.width,
            height: sw.height,
            format: sw.format,
            ..fields
        };
        self.convert(&merged, fallback_pts)
            .map_err(|e| Fail::Hw(e.to_string()))
    }

    /// Replace this decoder with a software one for the same stream (keeps the
    /// config and key-frame state).
    fn fall_back_to_software(&mut self, why: &str) -> Result<(), DecodeError> {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if LOGGED.swap(true, Ordering::Relaxed) {
            tracing::debug!("{}: {why}; switching to software decoding", self.name);
        } else {
            tracing::warn!(
                "{}: hardware decoding failed ({why}); switching to software decoding for this stream",
                self.name
            );
        }
        let opts = DecoderOptions {
            prefer_hw: false,
            ..self.options
        };
        let mut sw = Self::open_software(self.lib, self.codec, opts)?;
        std::mem::swap(&mut sw.stream, &mut self.stream);
        *self = sw;
        Ok(())
    }

    /// Decode one sample key frame (for the hardware probe): the first frame, or
    /// why none came out of the hardware path.
    fn probe_decode(&mut self, data: &[u8]) -> Result<Frame, String> {
        let why = |f: Fail| match f {
            Fail::Hw(s) => s,
            Fail::Decode(what, code) => format!("{what}: {}", self.lib.err(code)),
            Fail::Other(e) => e.to_string(),
        };
        let mut out = match self.decode(data, 0, true) {
            Ok(out) => out,
            Err(f) => return Err(why(f)),
        };
        if out.is_empty() {
            // Decoders with output delay (V4L2 M2M): drain.
            if self.send(None, 0, true) >= 0
                && let Err(f) = self.drain(0, true, &mut out)
            {
                return Err(why(f));
            }
        }
        if self.hw.is_none() {
            return Err("not decoded in hardware".into());
        }
        out.into_iter().next().ok_or_else(|| "no frame".into())
    }

    /// `profile` and `sw_pix_fmt` seen by the last `get_format` call.
    #[cfg(test)]
    fn hw_seen(&self) -> Option<(c_int, c_int)> {
        let f = self.hw.as_ref()?.format.as_ref()?;
        Some((
            f.seen[0].load(Ordering::Relaxed),
            f.seen[1].load(Ordering::Relaxed),
        ))
    }

    fn convert(&self, f: &FrameFields, fallback_pts: u64) -> Result<Frame, DecodeError> {
        let lib = self.lib;
        let (format, full_from_format) = match f.format {
            x if x == lib.pix_yuv420p => (PixelFormat::I420, false),
            x if x == lib.pix_yuvj420p => (PixelFormat::I420, true),
            x if x == lib.pix_nv12 => (PixelFormat::Nv12, false),
            x if x == lib.pix_yuv420p10le => (PixelFormat::I420P10, false),
            x if x == lib.pix_p010le => (PixelFormat::P010, false),
            other => {
                return Err(DecodeError::Decode(format!(
                    "{}: unsupported pixel format {}",
                    self.name,
                    hw::pix_fmt_name(lib, other)
                )));
            }
        };
        if f.width <= 0 || f.height <= 0 {
            return Err(DecodeError::Decode(format!(
                "{}: bad frame size {}x{}",
                self.name, f.width, f.height
            )));
        }
        let (w, h) = (f.width as u32, f.height as u32);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        // (width, height, bytes per sample position) per plane.
        let layout: &[(u32, u32, usize)] = match format {
            PixelFormat::I420 => &[(w, h, 1), (cw, ch, 1), (cw, ch, 1)],
            PixelFormat::I420P10 => &[(w, h, 2), (cw, ch, 2), (cw, ch, 2)],
            PixelFormat::Nv12 => &[(w, h, 1), (cw, ch, 2)],
            PixelFormat::P010 => &[(w, h, 2), (cw, ch, 4)],
        };
        let mut planes = Vec::with_capacity(layout.len());
        for (i, &(pw, ph, bpp)) in layout.iter().enumerate() {
            let row = pw as usize * bpp;
            let stride = f.linesize[i];
            if f.data[i].is_null() || stride < row as c_int {
                return Err(DecodeError::Decode(format!(
                    "{}: unexpected plane {i} (stride {stride})",
                    self.name
                )));
            }
            let mut data = Vec::with_capacity(row * ph as usize);
            for y in 0..ph as usize {
                // SAFETY: FFmpeg guarantees `height` rows of `linesize` bytes per plane.
                let src =
                    unsafe { std::slice::from_raw_parts(f.data[i].add(y * stride as usize), row) };
                data.extend_from_slice(src);
            }
            planes.push(Plane {
                data,
                stride: row,
                width: pw,
                height: ph,
            });
        }
        Ok(Frame {
            width: w,
            height: h,
            format,
            color: color_info(f.colorspace, f.color_range, full_from_format, w, h),
            planes,
            pts: if f.pts == AV_NOPTS_VALUE || f.pts < 0 {
                fallback_pts
            } else {
                f.pts as u64
            },
        })
    }
}

/// Matrix and range from FFmpeg's `AVColorSpace` / `AVColorRange`, with the usual
/// fallback (BT.709 for HD, BT.601 for SD, limited range) when unspecified.
pub(crate) fn color_info(
    colorspace: c_int,
    range: c_int,
    full_from_format: bool,
    w: u32,
    h: u32,
) -> ColorInfo {
    let bt709 = match colorspace {
        1 | 7 => true,  // BT709, SMPTE240M
        4..=6 => false, // FCC, BT470BG, SMPTE170M
        9 | 10 => true, // BT.2020: closest of the two matrices we support
        _ => crate::stream::default_bt709(w, h),
    };
    ColorInfo {
        bt709,
        full_range: full_from_format || range == 2,
    }
}

impl Decoder for FfmpegDecoder {
    fn push(
        &mut self,
        data: &[u8],
        pts: u64,
        key: bool,
        config: bool,
    ) -> Result<Vec<Frame>, DecodeError> {
        let payload = match self.stream.prepare(data, key, config) {
            Prepared::Skip => return Ok(Vec::new()),
            Prepared::Feed(p) => p,
        };
        let mut result = self.decode(&payload, pts, key);
        if let Err(Fail::Hw(why)) = &result {
            let why = why.clone();
            self.fall_back_to_software(&why)?;
            if !key {
                // References are gone: resume at the next key frame.
                self.stream.on_error();
                return Ok(Vec::new());
            }
            result = self.decode(&payload, pts, key);
        }
        match result {
            Ok(out) => Ok(out),
            Err(Fail::Decode(what, code)) => Err(self.fail(what, code)),
            Err(Fail::Hw(why)) => {
                self.stream.on_error();
                Err(DecodeError::Decode(format!("{}: {why}", self.name)))
            }
            Err(Fail::Other(e)) => Err(e),
        }
    }

    fn backend(&self) -> &str {
        &self.name
    }

    fn is_hardware(&self) -> bool {
        self.hw.is_some()
    }

    fn waiting_for_key_frame(&self) -> bool {
        self.stream.waiting_for_key()
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        // SAFETY: each free function accepts a pointer to a (possibly null) pointer.
        // The context (whose `opaque` points at `hw.format`) goes before `hw`.
        unsafe {
            (self.lib.frame_free)(&mut self.frame);
            if !self.sw_frame.is_null() {
                (self.lib.frame_free)(&mut self.sw_frame);
            }
            (self.lib.packet_free)(&mut self.pkt);
            (self.lib.free_context)(&mut self.ctx);
        }
    }
}
