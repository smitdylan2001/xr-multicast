//! FFmpeg backend: `libavcodec` / `libavutil` loaded at runtime with `libloading`.
//!
//! Nothing is linked at build time, so the crate builds everywhere and works
//! without FFmpeg (falling back to OpenH264). Supported: libavcodec 59–62
//! (FFmpeg 5.x–8.x). Unknown majors are refused because the `AVFrame` layout we
//! read could differ.
//!
//! TODO(hw): hardware decoding (VA-API, V4L2 request, VideoToolbox, D3D11VA) needs
//! `AVCodecContext.hw_device_ctx` and a `get_format` callback, which are plain
//! struct fields at offsets that differ per major. Add per-major offsets (verified
//! like the `AVFrame` layouts in `sys.rs`), `av_hwdevice_ctx_create`, and
//! `av_hwframe_transfer_data` to download NV12/P010. Until then frames are decoded
//! in software and `hw` is reported as `false`.

pub mod sys;

use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
    pix_yuv420p: c_int,
    pix_yuvj420p: c_int,
    pix_nv12: c_int,
    pix_yuv420p10le: c_int,

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

    fn err(&self, code: c_int) -> String {
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
            FfmpegDecoder::open_named(self, codec, name, opts).is_ok()
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
        pix_yuv420p: pix(c"yuv420p"),
        pix_yuvj420p: pix(c"yuvj420p"),
        pix_nv12: pix(c"nv12"),
        pix_yuv420p10le: pix(c"yuv420p10le"),
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

/// One FFmpeg decoder instance.
pub struct FfmpegDecoder {
    lib: &'static Lib,
    ctx: *mut AVCodecContext,
    pkt: *mut AVPacket,
    frame: *mut AVFrame,
    name: String,
    stream: StreamState,
}

// SAFETY: the FFmpeg objects are owned exclusively by this value and only used
// through `&mut self`; FFmpeg contexts may move between threads.
unsafe impl Send for FfmpegDecoder {}

impl FfmpegDecoder {
    /// Open the first available FFmpeg decoder for `codec`.
    pub fn open(
        lib: &'static Lib,
        codec: Codec,
        options: DecoderOptions,
    ) -> Result<FfmpegDecoder, DecodeError> {
        let mut last = None;
        for name in decoder_names(codec) {
            match Self::open_named(lib, codec, name, options) {
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
                name: format!("ffmpeg:{name}"),
                stream: StreamState::new(codec),
            };
            if dec.ctx.is_null() || dec.pkt.is_null() || dec.frame.is_null() {
                return Err(DecodeError::Decode("FFmpeg allocation failed".into()));
            }
            // Low latency: no frame threading (it delays output by threads-1 frames),
            // output every picture as soon as it is decoded.
            dec.set_int(c"threads", i64::from(options.threads));
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
                    "avcodec_open2({name}): {}",
                    lib.err(ret)
                )));
            }
            Ok(dec)
        }
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

    fn send(&mut self, data: &[u8], pts: u64, key: bool) -> c_int {
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

    fn drain(&mut self, fallback_pts: u64, out: &mut Vec<Frame>) -> Result<(), DecodeError> {
        loop {
            // SAFETY: valid context and frame.
            let ret = unsafe { (self.lib.receive_frame)(self.ctx, self.frame) };
            if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
                return Ok(());
            }
            if ret < 0 {
                return Err(self.fail("receive_frame", ret));
            }
            // SAFETY: the frame was just filled by the libavutil this layout belongs to.
            let fields = unsafe { read_frame(self.lib.layout, self.frame) };
            let frame = self.convert(&fields, fallback_pts);
            // SAFETY: valid frame.
            unsafe { (self.lib.frame_unref)(self.frame) };
            out.push(frame?);
        }
    }

    fn convert(&self, f: &FrameFields, fallback_pts: u64) -> Result<Frame, DecodeError> {
        let lib = self.lib;
        let (format, full_from_format) = match f.format {
            x if x == lib.pix_yuv420p => (PixelFormat::I420, false),
            x if x == lib.pix_yuvj420p => (PixelFormat::I420, true),
            x if x == lib.pix_nv12 => (PixelFormat::Nv12, false),
            x if x == lib.pix_yuv420p10le => (PixelFormat::I420P10, false),
            other => {
                return Err(DecodeError::Decode(format!(
                    "{}: unsupported pixel format {other}",
                    self.name
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
        let layout: &[(u32, u32, usize)] = match format {
            PixelFormat::I420 => &[(w, h, 1), (cw, ch, 1), (cw, ch, 1)],
            PixelFormat::I420P10 => &[(w, h, 2), (cw, ch, 2), (cw, ch, 2)],
            PixelFormat::Nv12 => &[(w, h, 1), (cw, ch, 2)],
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
        let mut out = Vec::new();
        let mut ret = self.send(&payload, pts, key);
        if ret == AVERROR_EAGAIN {
            // Output pending (should not happen in low-delay mode): drain, then retry once.
            self.drain(pts, &mut out)?;
            ret = self.send(&payload, pts, key);
        }
        if ret < 0 {
            return Err(self.fail("send_packet", ret));
        }
        self.stream.fed();
        self.drain(pts, &mut out)?;
        Ok(out)
    }

    fn backend(&self) -> &str {
        &self.name
    }

    fn waiting_for_key_frame(&self) -> bool {
        self.stream.waiting_for_key()
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        // SAFETY: each free function accepts a pointer to a (possibly null) pointer.
        unsafe {
            (self.lib.frame_free)(&mut self.frame);
            (self.lib.packet_free)(&mut self.pkt);
            (self.lib.free_context)(&mut self.ctx);
        }
    }
}
