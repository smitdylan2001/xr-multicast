//! Shared constants and small value types (ported from `src/main/config.js`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Default adb TCP port used for Wi-Fi casting (`adb tcpip 5555`).
pub const DEFAULT_ADB_TCP_PORT: u16 = 5555;

/// Colours handed out to newly verified headsets, in order.
pub const HEADSET_COLORS: [&str; 12] = [
    "#F97316", "#22C55E", "#3B82F6", "#EAB308", "#EC4899", "#14B8A6", "#A855F7", "#EF4444",
    "#84CC16", "#06B6D4", "#F43F5E", "#6366F1",
];

/// How a headset is connected to this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Usb,
    Wifi,
}

impl TransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportKind::Usb => "usb",
            TransportKind::Wifi => "wifi",
        }
    }
}

/// Which transport a user prefers for casting. `Auto` prefers USB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportPreference {
    #[default]
    Auto,
    Usb,
    Wifi,
}

impl From<TransportKind> for TransportPreference {
    fn from(k: TransportKind) -> Self {
        match k {
            TransportKind::Usb => TransportPreference::Usb,
            TransportKind::Wifi => TransportPreference::Wifi,
        }
    }
}

/// A video codec the app can request from the headset encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Codec {
    #[serde(rename = "h264")]
    H264,
    #[serde(rename = "h265")]
    H265,
    #[serde(rename = "av1")]
    Av1,
}

/// "Auto" order: H.265 first (mature low-latency hardware encoders on XR chips),
/// then AV1, with H.264 as the universal fallback.
pub const AUTO_CODEC_ORDER: [Codec; 3] = [Codec::H265, Codec::Av1, Codec::H264];

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::H264, Codec::H265, Codec::Av1];

    /// The scrcpy / decoder-support name (`h264`, `h265`, `av1`).
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
            Codec::Av1 => "av1",
        }
    }

    /// Label shown in the UI.
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::H265 => "H.265 (HEVC)",
            Codec::Av1 => "AV1",
        }
    }

    /// One-line description shown in the UI.
    pub fn description(self) -> &'static str {
        match self {
            Codec::H264 => "Works everywhere",
            Codec::H265 => "~30–50% less bandwidth than H.264 at the same quality",
            Codec::Av1 => "Most efficient; hardware encoders are rare on headsets",
        }
    }

    /// Whether the codec has a 10-bit profile the app can use.
    pub fn supports_ten_bit(self) -> bool {
        matches!(self, Codec::H265 | Codec::Av1)
    }
}

impl fmt::Display for Codec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Codec {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "h264" => Ok(Codec::H264),
            "h265" => Ok(Codec::H265),
            "av1" => Ok(Codec::Av1),
            _ => Err(()),
        }
    }
}

/// UI description of a codec (`CODECS` in config.js).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CodecInfo {
    pub id: Codec,
    pub label: &'static str,
    pub description: &'static str,
}

/// All codecs with their labels, in display order.
pub fn codec_infos() -> Vec<CodecInfo> {
    Codec::ALL
        .iter()
        .map(|&id| CodecInfo {
            id,
            label: id.label(),
            description: id.description(),
        })
        .collect()
}

/// The user's codec choice: automatic, or one specific codec.
///
/// Serialised as `"auto"`, `"h264"`, `"h265"` or `"av1"`. Unknown strings read
/// as `Auto` (the Electron app would fall back to H.264 with a note instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CodecPreference {
    #[default]
    Auto,
    Fixed(Codec),
}

impl CodecPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            CodecPreference::Auto => "auto",
            CodecPreference::Fixed(c) => c.as_str(),
        }
    }

    /// Lenient parse: anything that isn't a known codec is `Auto`.
    pub fn parse(s: &str) -> Self {
        s.parse()
            .map(CodecPreference::Fixed)
            .unwrap_or(CodecPreference::Auto)
    }
}

impl From<Codec> for CodecPreference {
    fn from(c: Codec) -> Self {
        CodecPreference::Fixed(c)
    }
}

impl Serialize for CodecPreference {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CodecPreference {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(CodecPreference::parse(&s))
    }
}

/// Key of a quality preset in the settings (`usbQuality` / `wifiQuality`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QualityKey {
    Low,
    High,
    Max,
    Custom,
    /// Also what unknown values read as (like `QUALITY_PRESETS[key] || balanced`).
    #[default]
    #[serde(other)]
    Balanced,
}

impl QualityKey {
    /// The fixed preset for this key; `None` for `Custom`.
    pub fn preset(self) -> Option<QualityPreset> {
        let (label, max_size, bit_rate, max_fps) = match self {
            QualityKey::Low => ("Low (1280px · 4 Mbps · 30 fps)", 1280, 4_000_000, 30),
            QualityKey::Balanced => ("Balanced (1920px · 8 Mbps · 60 fps)", 1920, 8_000_000, 60),
            QualityKey::High => ("High (2560px · 16 Mbps · 60 fps)", 2560, 16_000_000, 60),
            QualityKey::Max => ("Native (full res · 24 Mbps · 72 fps)", 0, 24_000_000, 72),
            QualityKey::Custom => return None,
        };
        Some(QualityPreset {
            label: label.to_string(),
            max_size,
            bit_rate,
            max_fps,
        })
    }
}

/// Encoder parameters for one stream quality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityPreset {
    pub label: String,
    /// Long-side limit in pixels; 0 = native resolution.
    pub max_size: u32,
    /// Bits per second.
    pub bit_rate: u32,
    pub max_fps: u32,
}

/// The fixed presets in display order (`QUALITY_PRESETS` in config.js).
pub fn quality_presets() -> Vec<(QualityKey, QualityPreset)> {
    [
        QualityKey::Low,
        QualityKey::Balanced,
        QualityKey::High,
        QualityKey::Max,
    ]
    .into_iter()
    .filter_map(|k| k.preset().map(|p| (k, p)))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_keys_parse_leniently() {
        let k: QualityKey = serde_json::from_str("\"high\"").unwrap();
        assert_eq!(k, QualityKey::High);
        let k: QualityKey = serde_json::from_str("\"ultra\"").unwrap();
        assert_eq!(k, QualityKey::Balanced);
        assert_eq!(
            serde_json::to_string(&QualityKey::Custom).unwrap(),
            "\"custom\""
        );
    }

    #[test]
    fn codec_preference_round_trips() {
        let p: CodecPreference = serde_json::from_str("\"av1\"").unwrap();
        assert_eq!(p, CodecPreference::Fixed(Codec::Av1));
        assert_eq!(
            serde_json::to_string(&CodecPreference::Auto).unwrap(),
            "\"auto\""
        );
        let p: CodecPreference = serde_json::from_str("\"vp9\"").unwrap();
        assert_eq!(p, CodecPreference::Auto);
    }

    #[test]
    fn presets_match_the_electron_app() {
        let p = QualityKey::Balanced.preset().unwrap();
        assert_eq!((p.max_size, p.bit_rate, p.max_fps), (1920, 8_000_000, 60));
        assert_eq!(quality_presets().len(), 4);
        assert!(QualityKey::Custom.preset().is_none());
    }
}
