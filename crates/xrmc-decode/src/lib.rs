//! Video decoding for XR MultiCast: scrcpy video packets (H.264 / H.265 Annex B,
//! AV1 OBUs) to CPU YUV frames that `xrmc-render` uploads to the GPU.
//!
//! Backends (chosen by [`create`]):
//! - **FFmpeg, loaded at runtime** (`libavcodec` found on the system: Ubuntu,
//!   Raspberry Pi OS, or shipped next to the app on Windows/macOS). H.264, H.265,
//!   AV1, 8 and 10 bit, multithreaded. No build-time FFmpeg dependency.
//! - **OpenH264, built in**: H.264 only, always available (fallback).
//!
//! Hardware decoding goes through FFmpeg's hwaccels (VA-API, V4L2, VideoToolbox,
//! D3D11VA/DXVA2; see `ffmpeg/hw.rs`), probed once with tiny embedded key
//! frames, with a per-stream fallback to software.
//!
//! Environment overrides: `XRMC_FFMPEG_DIR` (folder with the FFmpeg libraries),
//! `XRMC_FFMPEG_DISABLE=1`, `XRMC_FFMPEG_LOG=1` (FFmpeg warnings to stderr),
//! `XRMC_OPENH264_LIB` (Cisco OpenH264 binary on targets without the built-in one),
//! `XRMC_HWDEC=0` (no hardware decoding) or `XRMC_HWDEC=vaapi|v4l2|videotoolbox|…`
//! (only that device type), `XRMC_HWDEC_DEVICE` (device path / adapter).

use std::fmt;
use std::sync::OnceLock;

#[cfg(any(target_os = "android", test))]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
mod android;
mod ffmpeg;
mod openh264;
mod stream;

#[cfg(target_os = "android")]
pub use android::{AndroidCodecInfo, set_codec_list};

/// Video codecs scrcpy can send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

impl Codec {
    /// Lower-case name as used by scrcpy and the core (`h264`, `h265`, `av1`).
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "av1",
        }
    }

    pub fn from_name(name: &str) -> Option<Codec> {
        match name {
            "h264" => Some(Codec::H264),
            "h265" | "hevc" => Some(Codec::H265),
            "av1" => Some(Codec::Av1),
            _ => None,
        }
    }
}

/// Memory layout of a decoded frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit planar Y, U, V (4:2:0). Three planes.
    I420,
    /// 8-bit Y plane + interleaved UV plane (4:2:0). Two planes.
    Nv12,
    /// 10-bit planar 4:2:0, each sample a little-endian `u16` (values 0..1023). Three planes.
    I420P10,
    /// 10-bit Y plane + interleaved UV plane (4:2:0), each sample a little-endian
    /// `u16` with the value in the high 10 bits (`code << 6`). Two planes; what
    /// hardware decoders download for 10-bit streams.
    P010,
}

/// YUV → RGB conversion parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColorInfo {
    /// BT.709 matrix (HD); otherwise BT.601.
    pub bt709: bool,
    /// Full range (0..255) instead of limited/TV range (16..235).
    pub full_range: bool,
}

/// One image plane.
#[derive(Clone, PartialEq, Eq)]
pub struct Plane {
    pub data: Vec<u8>,
    /// Bytes per row (≥ row width in bytes).
    pub stride: usize,
    /// Plane width/height in samples (chroma planes are half size for 4:2:0).
    pub width: u32,
    pub height: u32,
}

impl fmt::Debug for Plane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Plane({}x{}, stride {}, {} bytes)",
            self.width,
            self.height,
            self.stride,
            self.data.len()
        )
    }
}

/// A decoded picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub color: ColorInfo,
    /// Y, U, V (I420 / I420P10) or Y, UV (NV12 / P010).
    pub planes: Vec<Plane>,
    /// Presentation timestamp in microseconds (from the packet).
    pub pts: u64,
}

/// Whether a codec can be decoded at all (`any`) and in hardware (`hw`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeCaps {
    pub any: bool,
    pub hw: bool,
}

/// What this machine can decode (fed to the core's codec policy).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub h264: DecodeCaps,
    pub h265: DecodeCaps,
    pub av1: DecodeCaps,
    pub h265_10: DecodeCaps,
    pub av1_10: DecodeCaps,
    /// Human-readable backend description, e.g. `FFmpeg 8.0 (libavcodec 62.28) + OpenH264`.
    pub backend: String,
    /// Per codec profile: which decoder would be used and how (for the UI).
    pub details: Vec<CodecDetail>,
}

/// How one codec profile is decoded on this machine.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CodecDetail {
    /// Decoder-support key: `h264`, `h265`, `av1`, `h265_10`, `av1_10`.
    pub key: String,
    pub caps: DecodeCaps,
    /// Decoder that would be used, e.g. `hevc`, `libdav1d`, `OpenH264`,
    /// `c2.qti.hevc.decoder`; empty if unsupported.
    pub decoder: String,
    /// Hardware API when `caps.hw`, e.g. `VA-API (AMD Radeon 780M)`, `V4L2`,
    /// `VideoToolbox`, `D3D11VA`, `MediaCodec`.
    pub hw_api: Option<String>,
}

/// Decoder settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecoderOptions {
    /// Try hardware decoding first (VA-API / V4L2 / VideoToolbox / D3D11 where available).
    pub prefer_hw: bool,
    /// Decoder threads (0 = automatic). Keep low per stream when decoding many streams.
    pub threads: u32,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        DecoderOptions {
            prefer_hw: true,
            threads: 2,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("no decoder available for {0:?}")]
    Unsupported(Codec),
    #[error("decoder error: {0}")]
    Decode(String),
}

/// A stateful decoder for one stream.
pub trait Decoder: Send {
    /// Feed one scrcpy packet. `config` packets (SPS/PPS/VPS, AV1 sequence header /
    /// av1C) are remembered and prepended to key frames as needed. Returns the
    /// frames that became available (usually 0 or 1; low-latency, no reordering delay).
    fn push(
        &mut self,
        data: &[u8],
        pts: u64,
        key: bool,
        config: bool,
    ) -> Result<Vec<Frame>, DecodeError>;

    /// Backend name for diagnostics, e.g. `ffmpeg:h264` or `openh264`.
    fn backend(&self) -> &str;

    /// Whether frames are decoded in hardware.
    fn is_hardware(&self) -> bool {
        false
    }

    /// Whether delta frames are being dropped until the next key frame (start of
    /// stream, after a config change, or after a decoder error). The caller may ask
    /// the device for a key frame.
    fn waiting_for_key_frame(&self) -> bool {
        false
    }
}

/// Which decoder implementation [`create_with`] should use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// FFmpeg if it is available and supports the codec, else OpenH264 (H.264).
    #[default]
    Auto,
    /// FFmpeg only (runtime-loaded `libavcodec`).
    Ffmpeg,
    /// OpenH264 only (H.264).
    OpenH264,
    /// Android NDK `AMediaCodec` (Android only; hardware where the device has it).
    MediaCodec,
}

/// Description of the FFmpeg found at runtime (e.g.
/// `FFmpeg 8.0.1 (libavcodec 62.28.102, libavutil 60.26.102)`), or `None`.
pub fn ffmpeg_version() -> Option<String> {
    ffmpeg::lib().map(ffmpeg::Lib::describe)
}

/// OpenH264 version (e.g. `2.6.0`) if the OpenH264 backend is usable.
pub fn openh264_version() -> Option<&'static str> {
    openh264::version()
}

/// What this machine can decode (probed once by opening each decoder, then cached).
pub fn capabilities() -> Capabilities {
    static CAPS: OnceLock<Capabilities> = OnceLock::new();
    #[cfg(target_os = "android")]
    let probe = android::probe_capabilities;
    #[cfg(not(target_os = "android"))]
    let probe = probe_capabilities;
    CAPS.get_or_init(probe).clone()
}

#[cfg_attr(target_os = "android", allow(dead_code))]
fn probe_capabilities() -> Capabilities {
    let ff = ffmpeg::lib();
    let probe = |codec| ff.and_then(|lib| lib.probe(codec));
    let (h264, hevc, av1) = (probe(Codec::H264), probe(Codec::H265), probe(Codec::Av1));
    let oh = openh264::version();
    let hw = ff.map(ffmpeg::hw::support);
    let hw_path = |key: &str| hw.and_then(|s| s.path(key));

    let mut parts = Vec::new();
    if let Some(lib) = ff {
        let names: Vec<&str> = [h264, hevc, av1].into_iter().flatten().collect();
        let mut s = format!("{} [{}]", lib.describe(), names.join(", "));
        if let Some(sum) = hw.and_then(|s| s.summary()) {
            s += &format!(" · hardware: {sum}");
        }
        parts.push(s);
    }
    if let Some(v) = oh {
        parts.push(format!("OpenH264 {v}"));
    }
    // FFmpeg's hevc decoder and libdav1d/libaom also handle 10-bit streams.
    let sw = |key: &str| match key {
        "h264" => h264.or(oh.map(|_| "OpenH264")),
        "h265" | "h265_10" => hevc,
        _ => av1,
    };
    let caps = |key: &str| DecodeCaps {
        any: sw(key).is_some() || hw_path(key).is_some(),
        hw: hw_path(key).is_some(),
    };
    let detail = |key: &str| CodecDetail {
        key: key.into(),
        caps: caps(key),
        decoder: hw_path(key)
            .map(|p| p.decoder)
            .or(sw(key))
            .unwrap_or_default()
            .into(),
        hw_api: hw_path(key).map(|p| p.label.clone()),
    };
    Capabilities {
        h264: caps("h264"),
        h265: caps("h265"),
        av1: caps("av1"),
        h265_10: caps("h265_10"),
        av1_10: caps("av1_10"),
        backend: if parts.is_empty() {
            "none".into()
        } else {
            parts.join(" + ")
        },
        details: ["h264", "h265", "av1", "h265_10", "av1_10"]
            .into_iter()
            .map(detail)
            .collect(),
    }
}

/// Create a decoder for `codec`: FFmpeg if available, else OpenH264 for H.264.
pub fn create(codec: Codec, options: DecoderOptions) -> Result<Box<dyn Decoder>, DecodeError> {
    create_with(codec, options, Backend::Auto)
}

/// Create a decoder using a specific backend.
pub fn create_with(
    codec: Codec,
    options: DecoderOptions,
    backend: Backend,
) -> Result<Box<dyn Decoder>, DecodeError> {
    // Android: MediaCodec first (no FFmpeg there), then OpenH264.
    #[cfg(target_os = "android")]
    if matches!(backend, Backend::Auto | Backend::MediaCodec) {
        match android::MediaCodecDecoder::open(codec, options) {
            Ok(dec) => return Ok(Box::new(dec)),
            Err(e) if backend == Backend::MediaCodec => return Err(e),
            Err(e) => tracing::warn!("MediaCodec {}: {e}", codec.as_str()),
        }
    }
    let mut ff_err = None;
    if backend == Backend::Ffmpeg || (backend == Backend::Auto && !cfg!(target_os = "android")) {
        match ffmpeg::lib() {
            Some(lib) => match ffmpeg::FfmpegDecoder::open(lib, codec, options) {
                Ok(dec) => return Ok(Box::new(dec)),
                Err(e) => {
                    tracing::debug!("FFmpeg {}: {e}", codec.as_str());
                    ff_err = Some(e);
                }
            },
            None => ff_err = Some(DecodeError::Unsupported(codec)),
        }
    }
    if matches!(backend, Backend::Auto | Backend::OpenH264) && codec == Codec::H264 {
        return Ok(Box::new(openh264::OpenH264Decoder::open()?));
    }
    Err(match (backend, ff_err) {
        (Backend::Ffmpeg, Some(e)) => e,
        _ => DecodeError::Unsupported(codec),
    })
}

/// A synthetic I420 test pattern (useful for UI and renderer development).
pub fn test_pattern(width: u32, height: u32, t: u32) -> Frame {
    let (w, h) = (width as usize, height as usize);
    let mut y = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..w {
            let grid = (col + t as usize) % 64 < 3 || row % 64 < 3;
            y[row * w + col] = if grid {
                235
            } else {
                (16 + (col * 200 / w.max(1))) as u8
            };
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let u = vec![128u8; cw * ch];
    let v = vec![(128 + (t % 64) as i32 - 32) as u8; cw * ch];
    Frame {
        width,
        height,
        format: PixelFormat::I420,
        color: ColorInfo {
            bt709: true,
            full_range: false,
        },
        planes: vec![
            Plane {
                data: y,
                stride: w,
                width,
                height,
            },
            Plane {
                data: u,
                stride: cw,
                width: cw as u32,
                height: ch as u32,
            },
            Plane {
                data: v,
                stride: cw,
                width: cw as u32,
                height: ch as u32,
            },
        ],
        pts: t as u64,
    }
}
