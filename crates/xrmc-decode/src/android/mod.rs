//! Android: hardware (and platform software) decoding with the NDK's
//! `AMediaCodec`, in ByteBuffer mode: decoded YUV is copied into a [`crate::Frame`].
//!
//! The NDK has no codec list before API 36, so the app passes the decoders it
//! found with Java's `MediaCodecList` to [`set_codec_list`] at startup (name,
//! type, hardware, low-latency and 10-bit support). Without a list, decoders are
//! created by MIME type and "hardware" is guessed from the name.
//!
//! TODO(zero-copy): decode into an `AImageReader` surface and import the
//! `AHardwareBuffer`s into Vulkan (see the UI spike's notes) instead of copying.

#[cfg(target_os = "android")]
mod codec;
pub(crate) mod layout;
pub(crate) mod sps;

#[cfg(target_os = "android")]
pub use codec::{MediaCodecDecoder, probe_capabilities};

use std::sync::OnceLock;

use crate::Codec;

/// One decoder as listed by `MediaCodecList(REGULAR_CODECS)`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AndroidCodecInfo {
    /// e.g. `c2.qti.hevc.decoder`, `c2.android.avc.decoder`.
    pub name: String,
    /// MIME type, e.g. `video/hevc`.
    pub mime: String,
    /// `MediaCodecInfo.isHardwareAccelerated()`.
    pub hardware: bool,
    /// `MediaCodecInfo.isSoftwareOnly()`.
    pub software_only: bool,
    /// `FEATURE_LowLatency`.
    pub low_latency: bool,
    /// Main 10 profile listed (HEVCProfileMain10 / AV1ProfileMain10).
    pub ten_bit: bool,
    pub max_width: u32,
    pub max_height: u32,
}

impl AndroidCodecInfo {
    /// Parse one line of `name|mime|hw|swOnly|lowLatency|tenBit|maxW|maxH`
    /// (the format the app's Java shell produces).
    pub fn parse_line(line: &str) -> Option<AndroidCodecInfo> {
        let f: Vec<&str> = line.trim().split('|').collect();
        if f.len() < 8 || f[0].is_empty() {
            return None;
        }
        let b = |s: &str| s == "1" || s == "true";
        Some(AndroidCodecInfo {
            name: f[0].to_string(),
            mime: f[1].to_ascii_lowercase(),
            hardware: b(f[2]),
            software_only: b(f[3]),
            low_latency: b(f[4]),
            ten_bit: b(f[5]),
            max_width: f[6].parse().unwrap_or(0),
            max_height: f[7].parse().unwrap_or(0),
        })
    }
}

static LIST: OnceLock<Vec<AndroidCodecInfo>> = OnceLock::new();

/// Register the decoders found by the app (call before the first [`crate::capabilities`]).
pub fn set_codec_list(list: Vec<AndroidCodecInfo>) {
    let _ = LIST.set(list);
}

pub(crate) fn codec_list() -> Option<&'static [AndroidCodecInfo]> {
    LIST.get().map(Vec::as_slice)
}

pub(crate) fn mime(codec: Codec) -> &'static str {
    match codec {
        Codec::H264 => "video/avc",
        Codec::H265 => "video/hevc",
        Codec::Av1 => "video/av01",
    }
}

/// Software decoders shipped with Android (`c2.android.*`, `OMX.google.*`) by name.
pub(crate) fn looks_software(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("c2.android.") || n.starts_with("omx.google.") || n.contains(".sw.")
}

/// Decoders to try for a codec, best first: hardware, then low-latency, then 10-bit capable.
pub(crate) fn candidates(list: &[AndroidCodecInfo], codec: Codec) -> Vec<&AndroidCodecInfo> {
    let mut c: Vec<&AndroidCodecInfo> = list.iter().filter(|i| i.mime == mime(codec)).collect();
    c.sort_by_key(|i| {
        (
            !(i.hardware && !i.software_only),
            !i.low_latency,
            !i.ten_bit,
        )
    });
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_java_list_and_ranks_hardware_first() {
        let list: Vec<AndroidCodecInfo> = [
            "c2.android.hevc.decoder|video/hevc|0|1|0|1|4096|4096",
            "c2.qti.hevc.decoder|video/hevc|1|0|1|1|8192|4320",
            "c2.qti.avc.decoder|video/avc|1|0|1|0|8192|4320",
            "garbage",
        ]
        .iter()
        .filter_map(|l| AndroidCodecInfo::parse_line(l))
        .collect();
        assert_eq!(list.len(), 3);
        let c = candidates(&list, Codec::H265);
        assert_eq!(c[0].name, "c2.qti.hevc.decoder");
        assert_eq!(c[1].name, "c2.android.hevc.decoder");
        assert!(candidates(&list, Codec::Av1).is_empty());
        assert!(looks_software("c2.android.avc.decoder"));
        assert!(!looks_software("c2.goldfish.h264.decoder"));
    }
}
