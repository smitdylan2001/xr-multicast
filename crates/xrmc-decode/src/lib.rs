//! Video decoding for XR MultiCast: scrcpy video packets (H.264 / H.265 Annex B,
//! AV1 OBUs) to CPU YUV frames that `xrmc-render` uploads to the GPU.
//!
//! Backends (chosen by [`create`]):
//! - **FFmpeg, loaded at runtime** (`libavcodec` found on the system: Ubuntu,
//!   Raspberry Pi OS, or shipped next to the app on Windows/macOS). H.264, H.265,
//!   AV1, 8 and 10 bit, multithreaded. No build-time FFmpeg dependency.
//! - **OpenH264, built in**: H.264 only, always available (fallback).
//!
//! STATUS: interface defined; implementation in progress (see the crate README).

use std::fmt;

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
        write!(f, "Plane({}x{}, stride {}, {} bytes)", self.width, self.height, self.stride, self.data.len())
    }
}

/// A decoded picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub color: ColorInfo,
    /// Y, U, V (I420 / I420P10) or Y, UV (NV12).
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
        DecoderOptions { prefer_hw: true, threads: 2 }
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
    fn push(&mut self, data: &[u8], pts: u64, key: bool, config: bool) -> Result<Vec<Frame>, DecodeError>;

    /// Backend name for diagnostics, e.g. `ffmpeg:h264` or `openh264`.
    fn backend(&self) -> &str;

    /// Whether frames are decoded in hardware.
    fn is_hardware(&self) -> bool {
        false
    }
}

/// What this machine can decode.
pub fn capabilities() -> Capabilities {
    Capabilities { backend: "none (not implemented yet)".into(), ..Default::default() }
}

/// Create a decoder for `codec`.
pub fn create(codec: Codec, _options: DecoderOptions) -> Result<Box<dyn Decoder>, DecodeError> {
    Err(DecodeError::Unsupported(codec))
}

/// A synthetic I420 test pattern (useful for UI and renderer development).
pub fn test_pattern(width: u32, height: u32, t: u32) -> Frame {
    let (w, h) = (width as usize, height as usize);
    let mut y = vec![0u8; w * h];
    for row in 0..h {
        for col in 0..w {
            let grid = (col + t as usize) % 64 < 3 || row % 64 < 3;
            y[row * w + col] = if grid { 235 } else { (16 + (col * 200 / w.max(1))) as u8 };
        }
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let u = vec![128u8; cw * ch];
    let v = vec![(128 + (t % 64) as i32 - 32) as u8; cw * ch];
    Frame {
        width,
        height,
        format: PixelFormat::I420,
        color: ColorInfo { bt709: true, full_range: false },
        planes: vec![
            Plane { data: y, stride: w, width, height },
            Plane { data: u, stride: cw, width: cw as u32, height: ch as u32 },
            Plane { data: v, stride: cw, width: cw as u32, height: ch as u32 },
        ],
        pts: t as u64,
    }
}
