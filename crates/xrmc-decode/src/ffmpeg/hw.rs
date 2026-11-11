//! Hardware decoding through FFmpeg's hwaccels, probed once per process.
//!
//! Device types tried automatically, per platform:
//! - **Linux:** VA-API on every `/dev/dri/renderD*` node, then the `drm` device for
//!   V4L2 request hwaccels (Raspberry Pi 5 HEVC in Raspberry Pi's FFmpeg), then the
//!   stateful `h264_v4l2m2m` / `hevc_v4l2m2m` decoders (Raspberry Pi 4 and other
//!   SBCs) when a V4L2 decoder device is present.
//! - **macOS:** VideoToolbox.
//! - **Windows:** D3D11VA, then DXVA2.
//!
//! `XRMC_HWDEC=0` disables hardware decoding. `XRMC_HWDEC=<type>` tries only that
//! type: `vaapi`, `v4l2` (= `drm` + `v4l2m2m`), `drm`/`v4l2request`, `v4l2m2m`,
//! `videotoolbox`, `d3d11va`, `dxva2`, or any other FFmpeg device type name
//! (`vulkan`, `cuda`, ...). `XRMC_HWDEC_DEVICE` overrides the device (e.g. a
//! render node path or a D3D11 adapter index).
//!
//! A codec profile (`h264`, `h265`, `h265_10`, `av1`, `av1_10`) only counts as
//! hardware-capable if a tiny embedded key frame (128×128 SMPTE bars) decodes
//! through the hardware path to the right picture. The probe runs on its own
//! thread with a timeout, so a hanging driver can't block the app.

use std::ffi::{CStr, CString, c_char, c_int};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::sys::*;
use super::{FfmpegDecoder, Lib};
use crate::{Codec, DecoderOptions, Frame, PixelFormat};

/// libavutil / libavcodec functions for hardware decoding (present in every
/// supported release; optional so a stripped build still loads for software).
pub struct HwFns {
    pub get_hw_config: FnGetHwConfig,
    pub device_ctx_create: FnHwDeviceCtxCreate,
    pub find_type_by_name: FnHwDeviceFindTypeByName,
    pub get_type_name: FnHwDeviceGetTypeName,
    pub buffer_ref: FnBufferRef,
    pub buffer_unref: FnBufferUnref,
    pub transfer_data: FnHwFrameTransferData,
    pub pix_fmt_name: FnGetPixFmtName,
}

/// An opened FFmpeg hardware device, shared by every decoder that uses it
/// (lives for the rest of the process).
pub(crate) struct HwDevice {
    pub type_id: c_int,
    /// Device path / name it was opened with (`None` = FFmpeg's default).
    pub path: Option<String>,
    buf: *mut AVBufferRef,
}

// SAFETY: the device reference is only read (av_buffer_ref) after creation;
// FFmpeg hardware device contexts may be shared between threads and decoders.
unsafe impl Send for HwDevice {}
unsafe impl Sync for HwDevice {}

impl HwDevice {
    /// A device for one decoder: a separate device of the same type and path
    /// (drivers serialise work per device: Mesa's VA-API driver holds one lock per
    /// VADisplay while it waits for the GPU, so 12 streams on one display reach
    /// less than half the throughput of 12 displays), else a new reference to
    /// this one. The caller owns the returned reference.
    pub fn open_ref(&self, lib: &Lib, own: bool) -> *mut AVBufferRef {
        let Some(fns) = lib.hw.as_ref() else {
            return std::ptr::null_mut();
        };
        if own {
            let dev_c = self
                .path
                .as_deref()
                .map(|d| CString::new(d).unwrap_or_default());
            let mut buf: *mut AVBufferRef = std::ptr::null_mut();
            // SAFETY: as in the probe.
            let ret = unsafe {
                (fns.device_ctx_create)(
                    &mut buf,
                    self.type_id,
                    dev_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if ret >= 0 && !buf.is_null() {
                return buf;
            }
        }
        // SAFETY: valid buffer reference.
        unsafe { (fns.buffer_ref)(self.buf) }
    }
}

/// How one codec profile is decoded in hardware.
#[derive(Clone)]
pub(crate) struct HwPath {
    /// FFmpeg decoder: `h264`, `hevc`, `av1`, `h264_v4l2m2m`, ...
    pub decoder: &'static str,
    /// `None` for decoders that are hardware by themselves (V4L2 M2M).
    pub device: Option<&'static HwDevice>,
    /// FFmpeg device type (`vaapi`, `drm`, `videotoolbox`, ...) or `v4l2m2m`.
    pub api: String,
    /// For the UI, e.g. `VA-API (AMD Radeon 780M Graphics)`.
    pub label: String,
    /// Give each decoder its own device (see [`HwDevice::open_ref`]); the probe
    /// shares one.
    pub own_device: bool,
}

/// Decoder-support keys, in the order of [`HwSupport::paths`].
pub(crate) const KEYS: [&str; 5] = ["h264", "h265", "h265_10", "av1", "av1_10"];

/// Result of the hardware probe.
pub(crate) struct HwSupport {
    /// Per key of [`KEYS`].
    pub paths: [Option<HwPath>; 5],
    /// Why nothing (or what) was found, for logs.
    pub note: String,
}

impl HwSupport {
    fn none(note: impl Into<String>) -> HwSupport {
        HwSupport {
            paths: Default::default(),
            note: note.into(),
        }
    }

    pub fn path(&self, key: &str) -> Option<&HwPath> {
        let i = KEYS.iter().position(|k| *k == key)?;
        self.paths[i].as_ref()
    }

    /// The path to open a decoder for `codec` with (8-bit profile, else 10-bit).
    pub fn for_codec(&self, codec: Codec) -> Option<&HwPath> {
        let key = codec.as_str();
        self.path(key).or_else(|| self.path(&format!("{key}_10")))
    }

    /// E.g. `VA-API (h264, h265, h265_10, av1, av1_10)`; `None` without hardware decoding.
    pub fn summary(&self) -> Option<String> {
        let mut groups: Vec<(String, Vec<&str>)> = Vec::new();
        for (key, path) in KEYS.iter().zip(&self.paths) {
            let Some(p) = path else { continue };
            let name = api_name(&p.api);
            match groups.iter_mut().find(|(n, _)| *n == name) {
                Some((_, keys)) => keys.push(key),
                None => groups.push((name, vec![key])),
            }
        }
        (!groups.is_empty()).then(|| {
            groups
                .iter()
                .map(|(n, keys)| format!("{n} ({})", keys.join(", ")))
                .collect::<Vec<_>>()
                .join(", ")
        })
    }
}

/// Display name of a device type.
fn api_name(api: &str) -> String {
    match api {
        "vaapi" => "VA-API".into(),
        "drm" => "V4L2 request".into(),
        "v4l2m2m" => "V4L2 M2M".into(),
        "videotoolbox" => "VideoToolbox".into(),
        "d3d11va" => "D3D11VA".into(),
        "d3d12va" => "D3D12VA".into(),
        "dxva2" => "DXVA2".into(),
        "vulkan" => "Vulkan".into(),
        "cuda" => "NVDEC (CUDA)".into(),
        other => other.to_uppercase(),
    }
}

/// FFmpeg decoder used with a hwaccel for `codec`.
fn hw_decoder_name(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "h264",
        Codec::H265 => "hevc",
        // The native decoder: it only decodes through a hwaccel (libdav1d stays
        // the software decoder).
        Codec::Av1 => "av1",
    }
}

fn key_codec(key: &str) -> Codec {
    match key {
        "h264" => Codec::H264,
        "h265" | "h265_10" => Codec::H265,
        _ => Codec::Av1,
    }
}

/// One 128×128 SMPTE-bar key frame per profile (see `probe/generate.sh`).
fn sample(key: &str) -> &'static [u8] {
    match key {
        "h264" => include_bytes!("probe/h264.h264"),
        "h265" => include_bytes!("probe/h265.h265"),
        "h265_10" => include_bytes!("probe/h265_10.h265"),
        "av1" => include_bytes!("probe/av1.obu"),
        _ => include_bytes!("probe/av1_10.obu"),
    }
}

/// What `XRMC_HWDEC` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HwSetting {
    Off,
    Auto,
    /// Only these device types (`v4l2m2m` = the stateful V4L2 decoders).
    Only(Vec<String>),
}

pub(crate) fn parse_setting(value: Option<&str>) -> HwSetting {
    let v = value.unwrap_or("").trim().to_ascii_lowercase();
    match v.as_str() {
        "" | "1" | "auto" | "on" | "yes" | "true" => HwSetting::Auto,
        "0" | "off" | "no" | "false" | "none" | "sw" | "software" => HwSetting::Off,
        "v4l2" => HwSetting::Only(vec!["drm".into(), "v4l2m2m".into()]),
        "v4l2request" | "v4l2-request" => HwSetting::Only(vec!["drm".into()]),
        "vt" => HwSetting::Only(vec!["videotoolbox".into()]),
        "d3d11" => HwSetting::Only(vec!["d3d11va".into()]),
        other => HwSetting::Only(vec![other.into()]),
    }
}

/// Device types tried automatically on this platform, in order.
fn auto_types() -> Vec<String> {
    let types: &[&str] = if cfg!(target_os = "android") {
        // Android uses the MediaCodec backend.
        &[]
    } else if cfg!(target_os = "linux") {
        &["vaapi", "drm", "v4l2m2m"]
    } else if cfg!(target_os = "macos") {
        &["videotoolbox"]
    } else if cfg!(windows) {
        &["d3d11va", "dxva2"]
    } else {
        &["vaapi"]
    };
    types.iter().map(|t| t.to_string()).collect()
}

/// Devices to try for a type: `XRMC_HWDEC_DEVICE`, else every render node for
/// VA-API on Linux, else FFmpeg's default device.
fn devices_for(type_name: &str) -> Vec<Option<String>> {
    if let Some(dev) = std::env::var("XRMC_HWDEC_DEVICE")
        .ok()
        .filter(|d| !d.is_empty())
    {
        return vec![Some(dev)];
    }
    if type_name == "vaapi" && cfg!(target_os = "linux") {
        let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("renderD"))
            .map(|n| format!("/dev/dri/{n}"))
            .collect();
        nodes.sort();
        if !nodes.is_empty() {
            return nodes.into_iter().map(Some).collect();
        }
    }
    vec![None]
}

/// The process-wide hardware support (probed on first use, with a timeout).
pub(crate) fn support(lib: &'static Lib) -> &'static HwSupport {
    static SUPPORT: OnceLock<HwSupport> = OnceLock::new();
    SUPPORT.get_or_init(|| {
        let start = Instant::now();
        let s = probe_with_timeout(lib, PROBE_TIMEOUT);
        let ms = start.elapsed().as_millis();
        match s.summary() {
            Some(sum) => tracing::info!("hardware decoding: {sum} [{}] ({ms} ms)", s.note),
            None => tracing::info!("hardware decoding: none ({}; {ms} ms)", s.note),
        }
        s
    })
}

/// Longest a hardware probe may take before hardware decoding is given up for
/// this run (a hung driver keeps its thread, but nothing waits for it).
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

fn probe_with_timeout(lib: &'static Lib, timeout: Duration) -> HwSupport {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("xrmc-hw-probe".into())
        .spawn(move || {
            let _ = tx.send(probe(lib));
        });
    if let Err(e) = spawned {
        return HwSupport::none(format!("could not start the probe thread: {e}"));
    }
    match rx.recv_timeout(timeout) {
        Ok(s) => s,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            tracing::warn!(
                "hardware decoder probe did not finish within {} s (driver hang?); using software decoding",
                timeout.as_secs()
            );
            HwSupport::none("probe timed out")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => HwSupport::none("probe failed"),
    }
}

fn cstr(p: *const c_char) -> Option<String> {
    // SAFETY: FFmpeg returns NUL-terminated static strings or NULL.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

/// The hw pixel format a decoder outputs for a device type, if it supports it.
pub(crate) fn device_pix_fmt(lib: &Lib, codec: *const AVCodec, type_id: c_int) -> Option<c_int> {
    let fns = lib.hw.as_ref()?;
    for i in 0..64 {
        // SAFETY: valid AVCodec; returns NULL past the last config.
        let cfg = unsafe { (fns.get_hw_config)(codec, i) };
        if cfg.is_null() {
            break;
        }
        // SAFETY: AVCodecHWConfig layout verified for every supported major.
        let cfg = unsafe { *cfg };
        if cfg.methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX != 0 && cfg.device_type == type_id
        {
            return Some(cfg.pix_fmt);
        }
    }
    None
}

fn decoder_supports(lib: &Lib, name: &str, type_id: c_int) -> bool {
    let cname = CString::new(name).expect("decoder name");
    // SAFETY: plain lookup.
    let codec = unsafe { (lib.find_decoder_by_name)(cname.as_ptr()) };
    !codec.is_null() && device_pix_fmt(lib, codec, type_id).is_some()
}

fn probe(lib: &'static Lib) -> HwSupport {
    let setting = parse_setting(std::env::var("XRMC_HWDEC").ok().as_deref());
    let types = match &setting {
        HwSetting::Off => return HwSupport::none("disabled by XRMC_HWDEC"),
        HwSetting::Auto => auto_types(),
        HwSetting::Only(t) => t.clone(),
    };
    let (Some(fns), Some(_)) = (&lib.hw, lib.ctx_offsets) else {
        return HwSupport::none(format!(
            "not available with libavcodec {} on this target (verified: 59-62, 64-bit)",
            lib.avcodec_version >> 16
        ));
    };
    let mut paths: [Option<HwPath>; 5] = Default::default();
    let mut notes = Vec::new();
    for type_name in &types {
        if paths.iter().all(Option::is_some) {
            break;
        }
        if type_name == "v4l2m2m" {
            probe_v4l2m2m(lib, &setting, &mut paths, &mut notes);
            continue;
        }
        let tname = CString::new(type_name.as_str()).unwrap_or_default();
        // SAFETY: plain lookup.
        let type_id = unsafe { (fns.find_type_by_name)(tname.as_ptr()) };
        if type_id == AV_HWDEVICE_TYPE_NONE {
            notes.push(format!("{type_name}: unknown to this FFmpeg"));
            continue;
        }
        // SAFETY: returns a static string or NULL.
        let type_name =
            &cstr(unsafe { (fns.get_type_name)(type_id) }).unwrap_or_else(|| type_name.clone());
        let wanted: Vec<usize> = (0..KEYS.len())
            .filter(|&i| {
                paths[i].is_none()
                    && decoder_supports(lib, hw_decoder_name(key_codec(KEYS[i])), type_id)
            })
            .collect();
        if wanted.is_empty() {
            notes.push(format!("{type_name}: no decoder supports it"));
            continue;
        }
        for device in devices_for(type_name) {
            if wanted.iter().all(|&i| paths[i].is_some()) {
                break;
            }
            let dev_c = device
                .as_deref()
                .map(|d| CString::new(d).unwrap_or_default());
            let mut buf: *mut AVBufferRef = std::ptr::null_mut();
            // SAFETY: creates a refcounted device; `buf` is set on success.
            let ret = unsafe {
                (fns.device_ctx_create)(
                    &mut buf,
                    type_id,
                    dev_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                    std::ptr::null_mut(),
                    0,
                )
            };
            let where_ = device.as_deref().unwrap_or("default device");
            if ret < 0 || buf.is_null() {
                notes.push(format!("{type_name} {where_}: {}", lib.err(ret)));
                continue;
            }
            let dev: &'static HwDevice = Box::leak(Box::new(HwDevice {
                type_id,
                path: device.clone(),
                buf,
            }));
            let label = device_label(type_name, device.as_deref());
            let mut used = false;
            for &i in &wanted {
                if paths[i].is_some() {
                    continue;
                }
                let path = HwPath {
                    decoder: hw_decoder_name(key_codec(KEYS[i])),
                    device: Some(dev),
                    api: type_name.clone(),
                    label: label.clone(),
                    own_device: false,
                };
                match try_path(lib, KEYS[i], &path) {
                    Ok(()) => {
                        paths[i] = Some(HwPath {
                            own_device: true,
                            ..path
                        });
                        used = true;
                    }
                    Err(e) => notes.push(format!("{type_name} {}: {e}", KEYS[i])),
                }
            }
            if used {
                notes.push(format!("{label} on {where_}"));
            } else {
                let mut b = buf;
                // SAFETY: drops our only reference (no decoder kept one).
                unsafe { (fns.buffer_unref)(&mut b) };
            }
        }
    }
    HwSupport {
        paths,
        note: notes.join("; "),
    }
}

/// Stateful V4L2 memory-to-memory decoders (hardware by themselves; no device).
fn probe_v4l2m2m(
    lib: &'static Lib,
    setting: &HwSetting,
    paths: &mut [Option<HwPath>; 5],
    notes: &mut Vec<String>,
) {
    if !cfg!(target_os = "linux") {
        return;
    }
    // Opening the decoder scans /dev/video*; skip that on machines whose video
    // devices are only cameras (it could wake them up), unless forced.
    if *setting == HwSetting::Auto && !v4l2_decoder_present() {
        return;
    }
    for (i, key) in KEYS.iter().enumerate() {
        let name = match *key {
            "h264" => "h264_v4l2m2m",
            "h265" | "h265_10" => "hevc_v4l2m2m",
            _ => continue,
        };
        if paths[i].is_some() {
            continue;
        }
        let path = HwPath {
            decoder: name,
            device: None,
            api: "v4l2m2m".into(),
            label: "V4L2 M2M".into(),
            own_device: false,
        };
        match try_path(lib, key, &path) {
            Ok(()) => paths[i] = Some(path),
            Err(e) => notes.push(format!("{name} {key}: {e}")),
        }
    }
}

/// A V4L2 device that looks like a video decoder (by its driver-given name).
fn v4l2_decoder_present() -> bool {
    std::fs::read_dir("/sys/class/video4linux")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path().join("name")).ok())
        .any(|n| {
            let n = n.to_ascii_lowercase();
            ["dec", "codec", "vpu", "hantro", "cedrus", "m2m"]
                .iter()
                .any(|k| n.contains(k))
        })
}

/// Decode the profile's embedded key frame through `path` and check the picture.
fn try_path(lib: &'static Lib, key: &str, path: &HwPath) -> Result<(), String> {
    let codec = key_codec(key);
    let opts = DecoderOptions {
        prefer_hw: true,
        threads: 1,
    };
    let mut dec = FfmpegDecoder::open_named(lib, codec, path.decoder, opts, Some(path))
        .map_err(|e| e.to_string())?;
    let frame = dec.probe_decode(sample(key))?;
    check_sample(&frame, key.ends_with("_10"))
}

/// 8-bit Y, U, V at a pixel of any frame format.
pub(crate) fn yuv8_at(f: &Frame, x: u32, y: u32) -> Option<(u8, u8, u8)> {
    let (cx, cy) = ((x / 2) as usize, (y / 2) as usize);
    let (x, y) = (x as usize, y as usize);
    let p = &f.planes;
    let b = |pl: usize, i: usize| p.get(pl)?.data.get(i).copied();
    let w = |pl: usize, i: usize| Some(u16::from_le_bytes([b(pl, i)?, b(pl, i + 1)?]));
    let s = |pl: usize| p.get(pl).map_or(0, |p| p.stride);
    Some(match f.format {
        PixelFormat::I420 => (
            b(0, y * s(0) + x)?,
            b(1, cy * s(1) + cx)?,
            b(2, cy * s(2) + cx)?,
        ),
        PixelFormat::Nv12 => (
            b(0, y * s(0) + x)?,
            b(1, cy * s(1) + 2 * cx)?,
            b(1, cy * s(1) + 2 * cx + 1)?,
        ),
        PixelFormat::I420P10 => (
            (w(0, y * s(0) + 2 * x)? >> 2) as u8,
            (w(1, cy * s(1) + 2 * cx)? >> 2) as u8,
            (w(2, cy * s(2) + 2 * cx)? >> 2) as u8,
        ),
        PixelFormat::P010 => (
            (w(0, y * s(0) + 2 * x)? >> 8) as u8,
            (w(1, cy * s(1) + 4 * cx)? >> 8) as u8,
            (w(1, cy * s(1) + 4 * cx + 2)? >> 8) as u8,
        ),
    })
}

/// The probe picture: 128×128 SMPTE bars (white … blue, BT.601 limited range).
fn check_sample(f: &Frame, ten_bit: bool) -> Result<(), String> {
    if (f.width, f.height) != (128, 128) {
        return Err(format!(
            "decoded {}x{} instead of 128x128",
            f.width, f.height
        ));
    }
    let is_ten = matches!(f.format, PixelFormat::I420P10 | PixelFormat::P010);
    if is_ten != ten_bit {
        return Err(format!("unexpected pixel format {:?}", f.format));
    }
    const BARS: [(u8, u8, u8); 7] = [
        (235, 128, 128),
        (210, 16, 146),
        (170, 166, 16),
        (145, 54, 34),
        (106, 202, 222),
        (81, 90, 240),
        (41, 240, 110),
    ];
    for (i, want) in BARS.iter().enumerate() {
        let x = ((2 * i as u32 + 1) * 128) / 14;
        let got = yuv8_at(f, x, 32).ok_or("frame too small")?;
        let close = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs() <= 24;
        if !(close(got.0, want.0) && close(got.1, want.1) && close(got.2, want.2)) {
            return Err(format!("wrong picture (bar {i}: {got:?}, want {want:?})"));
        }
    }
    Ok(())
}

/// UI label for a device, e.g. `VA-API (AMD Radeon 780M Graphics)`.
fn device_label(type_name: &str, device: Option<&str>) -> String {
    let name = api_name(type_name);
    if type_name == "vaapi" {
        let dev = device.unwrap_or("default");
        return match va_vendor(device) {
            Some(v) => format!("{name} ({v})"),
            None => format!("{name} ({dev})"),
        };
    }
    match device {
        Some(d) => format!("{name} ({d})"),
        None => name,
    }
}

/// The GPU name from the VA driver's vendor string (`Mesa Gallium driver 26.2.4
/// for AMD Radeon 780M Graphics (radeonsi, …)` → `AMD Radeon 780M Graphics`).
#[cfg_attr(
    not(all(target_os = "linux", not(target_os = "android"))),
    allow(dead_code)
)]
pub(crate) fn short_vendor(vendor: &str) -> String {
    let s = vendor.split_once(" for ").map_or(vendor, |(_, r)| r);
    let s = s.split(" (").next().unwrap_or(s).trim();
    s.chars().take(64).collect()
}

/// Queries the VA driver vendor string with a separate, short-lived VADisplay.
#[cfg(all(target_os = "linux", not(target_os = "android")))]
fn va_vendor(device: Option<&str>) -> Option<String> {
    use libloading::Library;
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;

    type VaDisplay = *mut c_void;
    type MsgCb = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;
    let path = device?;
    // SAFETY: libva is already loaded by FFmpeg's VA-API device; these are its
    // stable public entry points.
    unsafe {
        let va = Library::new("libva.so.2").ok()?;
        let va_drm = Library::new("libva-drm.so.2").ok()?;
        let get_display: libloading::Symbol<unsafe extern "C" fn(c_int) -> VaDisplay> =
            va_drm.get(b"vaGetDisplayDRM").ok()?;
        let initialize: libloading::Symbol<
            unsafe extern "C" fn(VaDisplay, *mut c_int, *mut c_int) -> c_int,
        > = va.get(b"vaInitialize").ok()?;
        let vendor: libloading::Symbol<unsafe extern "C" fn(VaDisplay) -> *const c_char> =
            va.get(b"vaQueryVendorString").ok()?;
        let terminate: libloading::Symbol<unsafe extern "C" fn(VaDisplay) -> c_int> =
            va.get(b"vaTerminate").ok()?;
        let set_cb = |name: &[u8]| -> Option<
            libloading::Symbol<'_, unsafe extern "C" fn(VaDisplay, MsgCb, *mut c_void) -> MsgCb>,
        > { va.get(name).ok() };
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .ok()?;
        let dpy = get_display(file.as_raw_fd());
        if dpy.is_null() {
            return None;
        }
        // Silence libva's "libva info: ..." lines.
        for name in [&b"vaSetInfoCallback"[..], b"vaSetErrorCallback"] {
            if let Some(f) = set_cb(name) {
                f(dpy, None, std::ptr::null_mut());
            }
        }
        let (mut major, mut minor) = (0, 0);
        let out = if initialize(dpy, &mut major, &mut minor) == 0 {
            cstr(vendor(dpy)).map(|v| short_vendor(&v))
        } else {
            None
        };
        terminate(dpy);
        drop(file);
        out.filter(|v| !v.is_empty())
    }
}

#[cfg(not(all(target_os = "linux", not(target_os = "android"))))]
fn va_vendor(_device: Option<&str>) -> Option<String> {
    None
}

/// Name of an FFmpeg pixel format (for messages).
pub(crate) fn pix_fmt_name(lib: &Lib, fmt: c_int) -> String {
    lib.hw
        .as_ref()
        // SAFETY: returns a static string or NULL.
        .and_then(|f| cstr(unsafe { (f.pix_fmt_name)(fmt) }))
        .unwrap_or_else(|| fmt.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xrmc_hwdec() {
        assert_eq!(parse_setting(None), HwSetting::Auto);
        assert_eq!(parse_setting(Some("auto")), HwSetting::Auto);
        assert_eq!(parse_setting(Some("0")), HwSetting::Off);
        assert_eq!(parse_setting(Some("off")), HwSetting::Off);
        assert_eq!(
            parse_setting(Some("VAAPI")),
            HwSetting::Only(vec!["vaapi".into()])
        );
        assert_eq!(
            parse_setting(Some("v4l2")),
            HwSetting::Only(vec!["drm".into(), "v4l2m2m".into()])
        );
        assert_eq!(
            parse_setting(Some("bogus")),
            HwSetting::Only(vec!["bogus".into()])
        );
    }

    #[test]
    fn vendor_strings_shorten_to_the_gpu_name() {
        assert_eq!(
            short_vendor(
                "Mesa Gallium driver 26.2.4-1ubuntu1 for AMD Radeon 780M Graphics (radeonsi, phoenix, ACO, DRM 3.64, 7.3.0-10-generic)"
            ),
            "AMD Radeon 780M Graphics"
        );
        assert_eq!(
            short_vendor("Intel iHD driver for Intel(R) Gen Graphics - 24.1.0 ()"),
            "Intel(R) Gen Graphics - 24.1.0"
        );
        assert_eq!(
            short_vendor("Splitted-Desktop Systems VDPAU backend"),
            "Splitted-Desktop Systems VDPAU backend"
        );
    }

    /// `profile` / `sw_pix_fmt` read in `get_format` at the verified offsets match
    /// the stream (checks the offsets against the loaded libavcodec at runtime).
    #[test]
    fn get_format_reads_profile_and_sw_pix_fmt() {
        let Some(lib) = super::super::lib() else {
            eprintln!("SKIPPED: no FFmpeg");
            return;
        };
        let s = support(lib);
        let opts = DecoderOptions {
            prefer_hw: true,
            threads: 1,
        };
        for (key, profile, sw) in [
            ("h264", 100, lib.pix_yuv420p),      // High
            ("h265", 1, lib.pix_yuv420p),        // Main
            ("h265_10", 2, lib.pix_yuv420p10le), // Main 10
        ] {
            let Some(path) = s.path(key).filter(|p| p.device.is_some()) else {
                eprintln!("SKIPPED {key}: no hwaccel ({})", s.note);
                continue;
            };
            let mut dec =
                FfmpegDecoder::open_named(lib, key_codec(key), path.decoder, opts, Some(path))
                    .unwrap();
            dec.probe_decode(sample(key)).unwrap();
            assert_eq!(dec.hw_seen(), Some((profile, sw)), "{key}");
        }
    }

    /// The SPS/VPS of the 10-bit HEVC sample relabelled as Main (8-bit) profile.
    fn relabel_main(data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        let mut i = 0;
        while i + 8 < out.len() {
            if out[i..i + 3] == [0, 0, 1] {
                let t = (out[i + 3] >> 1) & 0x3f;
                // general_profile_space(2) tier(1) profile_idc(5): after the VPS's
                // 4 header bytes + 0xffff, or the SPS's 1 header byte.
                let at = match t {
                    32 => Some(i + 3 + 2 + 4),
                    33 => Some(i + 3 + 2 + 1),
                    _ => None,
                };
                if let Some(at) = at {
                    assert_eq!(out[at] & 0x1f, 2, "Main 10 profile_idc");
                    out[at] = (out[at] & 0xe0) | 1;
                }
            }
            i += 1;
        }
        out
    }

    /// An HEVC stream that claims the Main profile but has 10-bit samples is never
    /// given to the hwaccel (FFmpeg would set up 8-bit VA surfaces, and AMD's VCN
    /// faults writing 10-bit pictures into them), and decodes in software instead.
    #[test]
    fn hevc_main_profile_with_10_bit_samples_falls_back_to_software() {
        let Some(lib) = super::super::lib() else {
            eprintln!("SKIPPED: no FFmpeg");
            return;
        };
        let s = support(lib);
        let Some(path) = s.path("h265_10").filter(|p| p.device.is_some()) else {
            eprintln!("SKIPPED: no HEVC Main 10 hwaccel ({})", s.note);
            return;
        };
        let data = relabel_main(sample("h265_10"));
        let opts = DecoderOptions {
            prefer_hw: true,
            threads: 1,
        };
        let mut dec =
            FfmpegDecoder::open_named(lib, Codec::H265, path.decoder, opts, Some(path)).unwrap();
        let err = dec.probe_decode(&data).unwrap_err();
        assert!(err.contains("doesn't support this stream"), "{err}");
        assert_eq!(dec.hw_seen(), Some((1, lib.pix_yuv420p10le)));

        use crate::Decoder;
        let mut dec = FfmpegDecoder::open(lib, Codec::H265, opts).unwrap();
        assert!(dec.is_hardware());
        let frames = dec.push(&data, 0, true, false).unwrap();
        assert!(!dec.is_hardware(), "{}", dec.backend());
        assert_eq!(frames.len(), 1);
        check_sample(&frames[0], true).unwrap();
    }

    #[test]
    fn probe_samples_are_tiny() {
        let total: usize = KEYS.iter().map(|k| sample(k).len()).sum();
        assert!(total < 50_000, "{total} bytes");
    }
}
