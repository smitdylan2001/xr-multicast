//! Codec, bit depth and quality decisions (ported from `src/main/stream-manager.js`).
//!
//! Everything here is pure: the stream manager feeds in the headset's encoder
//! list, what the viewers can decode and what failed earlier, and gets back the
//! codec/encoder to request.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::config::{
    AUTO_CODEC_ORDER, Codec, CodecPreference, QualityKey, QualityPreset, TransportKind,
};
use crate::settings::Settings;

/// One video encoder on the headset, as listed by scrcpy-server (`list_encoders=true`;
/// parsed by `xrmc_scrcpy::encoders::parse_encoder_list`, which has the same fields).
/// Stored in the headset record as `encoders`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderInfo {
    /// `h264`, `h265`, `av1`, ...
    pub codec: String,
    /// MediaCodec name, e.g. `c2.qti.hevc.encoder`.
    pub name: String,
    /// Hardware (or hybrid) encoder.
    pub hw: bool,
    /// The entry is an alias for another encoder.
    #[serde(default)]
    pub alias: bool,
}

/// Whether a viewer can decode a codec at all (`any`) and in hardware (`hw`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecodeCaps {
    pub any: bool,
    pub hw: bool,
}

/// Decoder support by key: `h264`, `h265`, `av1`, `h265_10`, `av1_10`.
pub type DecoderSupport = BTreeMap<String, DecodeCaps>;

/// The decoder-support key for a codec's 10-bit profile.
pub fn ten_bit_key(codec: Codec) -> String {
    format!("{}_10", codec.as_str())
}

fn baseline() -> DecoderSupport {
    BTreeMap::from([(
        "h264".to_string(),
        DecodeCaps {
            any: true,
            hw: false,
        },
    )])
}

/// What every viewer can decode: the hub's own decoders intersected with each
/// remote viewer's (`setDecoderSupport` / `setViewerSupport` / `recomputeDecoderSupport`).
#[derive(Debug, Clone, PartialEq)]
pub struct DecoderSupportSet {
    local: Option<DecoderSupport>,
    viewers: BTreeMap<String, DecoderSupport>,
    merged: DecoderSupport,
}

impl Default for DecoderSupportSet {
    fn default() -> Self {
        DecoderSupportSet {
            local: None,
            viewers: BTreeMap::new(),
            merged: baseline(),
        }
    }
}

impl DecoderSupportSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// The hub's own decoders. `None` keeps the previous report (and only recomputes).
    pub fn set_local(&mut self, support: Option<DecoderSupport>) {
        if let Some(s) = support {
            let mut local = baseline();
            local.extend(s);
            self.local = Some(local);
        }
        self.recompute();
    }

    /// A remote viewer's decoders; `None` when it leaves. Returns whether the merged support changed.
    pub fn set_viewer(&mut self, client_id: &str, support: Option<DecoderSupport>) -> bool {
        match support {
            Some(s) => {
                self.viewers.insert(client_id.to_string(), s);
            }
            None => {
                self.viewers.remove(client_id);
            }
        }
        let before = self.merged.clone();
        self.recompute();
        self.merged != before
    }

    fn recompute(&mut self) {
        let local = self.local.clone().unwrap_or_else(baseline);
        let mut merged: DecoderSupport = local
            .into_iter()
            .map(|(codec, d)| {
                let mut caps = d;
                for viewer in self.viewers.values() {
                    let v = viewer.get(&codec).copied().unwrap_or_default();
                    caps.any = caps.any && v.any;
                    caps.hw = caps.hw && v.hw;
                }
                (codec, caps)
            })
            .collect();
        // H.264 is the universal baseline; never let it be "unsupported".
        let h264_hw = merged.get("h264").is_some_and(|d| d.hw);
        merged.insert(
            "h264".to_string(),
            DecodeCaps {
                any: true,
                hw: h264_hw,
            },
        );
        self.merged = merged;
    }

    /// The merged support every viewer shares.
    pub fn merged(&self) -> &DecoderSupport {
        &self.merged
    }

    /// Whether every viewer can decode `codec` (in hardware when `need_hw`).
    pub fn can_decode(&self, codec: Codec, need_hw: bool) -> bool {
        can_decode(&self.merged, codec.as_str(), need_hw)
    }
}

fn can_decode(support: &DecoderSupport, key: &str, need_hw: bool) -> bool {
    support
        .get(key)
        .is_some_and(|d| if need_hw { d.hw } else { d.any })
}

/// Result of [`resolve_codec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodecChoice {
    pub codec: Codec,
    /// Encoder to request (`video_encoder=`); `None` lets the headset choose.
    pub encoder: Option<String>,
    /// Shown to the user once per stream (fallbacks, slow software encoders).
    pub note: Option<String>,
}

/// Pick the codec for a stream from the user preference, the headset's
/// encoders (`None` = they couldn't be listed) and the viewers' decoders.
pub fn resolve_codec(
    pref: CodecPreference,
    encoders: Option<&[EncoderInfo]>,
    decoders: &DecoderSupport,
    failed: &BTreeSet<Codec>,
) -> CodecChoice {
    let list = encoders.unwrap_or(&[]);
    let hw_encoder = |c: Codec| {
        list.iter()
            .find(|e| e.codec == c.as_str() && e.hw && !e.alias)
            .or_else(|| list.iter().find(|e| e.codec == c.as_str() && e.hw))
    };
    let any_encoder = |c: Codec| list.iter().find(|e| e.codec == c.as_str());
    let h264 = || CodecChoice {
        codec: Codec::H264,
        encoder: hw_encoder(Codec::H264).map(|e| e.name.clone()),
        note: None,
    };
    let fallback = |note: String| CodecChoice {
        codec: Codec::H264,
        encoder: None,
        note: Some(note),
    };

    match pref {
        CodecPreference::Auto => {
            for c in AUTO_CODEC_ORDER {
                if c == Codec::H264 {
                    break;
                }
                if let Some(enc) = hw_encoder(c)
                    && !failed.contains(&c)
                    && can_decode(decoders, c.as_str(), true)
                {
                    return CodecChoice {
                        codec: c,
                        encoder: Some(enc.name.clone()),
                        note: None,
                    };
                }
            }
            h264()
        }
        CodecPreference::Fixed(Codec::H264) => h264(),
        CodecPreference::Fixed(c) => {
            let label = c.label();
            if failed.contains(&c) {
                return fallback(format!(
                    "{label} failed earlier on this headset — using H.264."
                ));
            }
            if encoders.is_some() && any_encoder(c).is_none() {
                return fallback(format!(
                    "This headset has no {label} encoder — using H.264."
                ));
            }
            if !can_decode(decoders, c.as_str(), false) {
                return fallback(format!("This computer can't decode {label} — using H.264."));
            }
            let enc = hw_encoder(c).or_else(|| any_encoder(c));
            let note = enc
                .filter(|e| !e.hw)
                .map(|_| format!("{label} on this headset is a software encoder and may be slow."));
            CodecChoice {
                codec: c,
                encoder: enc.map(|e| e.name.clone()),
                note,
            }
        }
    }
}

/// Whether to ask for 10-bit (H.265 Main 10 / AV1 Main 10-bit): only when the
/// experimental setting is on, the codec supports it, every viewer can decode
/// it, and it hasn't already failed for this headset this run.
pub fn use_ten_bit(
    enabled: bool,
    force_eight_bit: bool,
    codec: Codec,
    failed_before: bool,
    decoders: &DecoderSupport,
) -> bool {
    enabled
        && !force_eight_bit
        && codec.supports_ten_bit()
        && !failed_before
        && can_decode(decoders, &ten_bit_key(codec), false)
}

/// scrcpy `video_codec_options` entry selecting the 10-bit profile
/// (MediaCodecInfo.CodecProfileLevel: HEVCProfileMain10 = 2, AV1ProfileMain10 = 2).
pub const TEN_BIT_CODEC_OPTION: &str = "profile=2";

/// Codecs (and 10-bit modes) that failed per headset during this run; they're
/// skipped until the app restarts.
#[derive(Debug, Clone, Default)]
pub struct CodecFailures {
    codecs: HashMap<String, BTreeSet<Codec>>,
    ten_bit: HashMap<String, BTreeSet<Codec>>,
}

impl CodecFailures {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember that a codec failed on a headset. H.264 is never marked.
    pub fn mark_codec_failed(&mut self, serial: &str, codec: Codec) {
        if codec != Codec::H264 {
            self.codecs
                .entry(serial.to_string())
                .or_default()
                .insert(codec);
        }
    }

    pub fn mark_ten_bit_failed(&mut self, serial: &str, codec: Codec) {
        self.ten_bit
            .entry(serial.to_string())
            .or_default()
            .insert(codec);
    }

    /// Codecs that failed on this headset.
    pub fn failed_codecs(&self, serial: &str) -> BTreeSet<Codec> {
        self.codecs.get(serial).cloned().unwrap_or_default()
    }

    pub fn ten_bit_failed(&self, serial: &str, codec: Codec) -> bool {
        self.ten_bit.get(serial).is_some_and(|s| s.contains(&codec))
    }
}

/// Encoder parameters for a transport from the settings (presets or custom).
pub fn quality_for(settings: &Settings, kind: TransportKind) -> QualityPreset {
    let key = settings.quality_key(kind);
    if key == QualityKey::Custom {
        let c = settings.custom_quality(kind);
        let mbps = if c.bit_rate_mbps.is_finite() && c.bit_rate_mbps != 0.0 {
            c.bit_rate_mbps
        } else {
            8.0
        };
        let mbps = mbps.clamp(0.5, 200.0);
        let fps = if c.max_fps > 0 { c.max_fps } else { 60 };
        let size = if c.max_size > 0 {
            format!("{}px", c.max_size)
        } else {
            "full res".to_string()
        };
        return QualityPreset {
            label: format!(
                "Custom ({size} · {} Mbps · {fps} fps)",
                crate::js::num_to_string(mbps)
            ),
            max_size: c.max_size,
            bit_rate: crate::js::round(mbps * 1_000_000.0) as u32,
            max_fps: fps,
        };
    }
    key.preset()
        .or_else(|| QualityKey::Balanced.preset())
        .unwrap_or_else(|| QualityPreset {
            label: String::new(),
            max_size: 1920,
            bit_rate: 8_000_000,
            max_fps: 60,
        })
}

#[cfg(test)]
mod tests {
    //! Port of `test/codec-choice.test.js`.
    use super::*;
    use crate::settings::CustomQuality;

    fn enc(codec: &str, name: &str, hw: bool) -> EncoderInfo {
        EncoderInfo {
            codec: codec.into(),
            name: name.into(),
            hw,
            alias: false,
        }
    }

    fn quest() -> Vec<EncoderInfo> {
        vec![
            enc("h264", "c2.qti.avc.encoder", true),
            enc("h265", "c2.qti.hevc.encoder", true),
            enc("av1", "c2.android.av1.encoder", false),
        ]
    }

    fn caps(any: bool, hw: bool) -> DecodeCaps {
        DecodeCaps { any, hw }
    }

    fn all_hw() -> DecoderSupport {
        [
            ("h264", caps(true, true)),
            ("h265", caps(true, true)),
            ("av1", caps(true, true)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    fn ten() -> DecoderSupport {
        let mut d = all_hw();
        d.insert("h265_10".into(), caps(true, true));
        d.insert("av1_10".into(), caps(true, true));
        d
    }

    /// Like the JS test helper: decoder support as the stream manager merges it.
    fn support(decoders: Option<DecoderSupport>) -> DecoderSupportSet {
        let mut s = DecoderSupportSet::new();
        if decoders.is_some() {
            s.set_local(decoders);
        }
        s
    }

    fn resolve(
        pref: CodecPreference,
        encoders: Option<&[EncoderInfo]>,
        decoders: Option<DecoderSupport>,
    ) -> CodecChoice {
        resolve_codec(pref, encoders, support(decoders).merged(), &BTreeSet::new())
    }

    #[test]
    fn auto_picks_h265_when_both_sides_have_hardware_support() {
        let r = resolve(CodecPreference::Auto, Some(&quest()), Some(all_hw()));
        assert_eq!(
            r,
            CodecChoice {
                codec: Codec::H265,
                encoder: Some("c2.qti.hevc.encoder".into()),
                note: None
            }
        );
    }

    #[test]
    fn auto_skips_software_only_av1_and_falls_to_h264_without_h265_decode() {
        let mut d = all_hw();
        d.insert("h265".into(), caps(false, false));
        assert_eq!(
            resolve(CodecPreference::Auto, Some(&quest()), Some(d)).codec,
            Codec::H264
        );
    }

    #[test]
    fn auto_uses_h264_before_the_renderer_reports_decoder_support() {
        assert_eq!(
            resolve(CodecPreference::Auto, Some(&quest()), None).codec,
            Codec::H264
        );
    }

    #[test]
    fn explicit_av1_on_a_software_encoder_is_allowed_with_a_warning() {
        let r = resolve(
            CodecPreference::Fixed(Codec::Av1),
            Some(&quest()),
            Some(all_hw()),
        );
        assert_eq!(r.codec, Codec::Av1);
        assert!(r.note.unwrap().contains("software encoder"));
    }

    #[test]
    fn explicit_codec_missing_on_the_headset_falls_back_to_h264() {
        let list: Vec<_> = quest().into_iter().filter(|e| e.codec != "h265").collect();
        let r = resolve(
            CodecPreference::Fixed(Codec::H265),
            Some(&list),
            Some(all_hw()),
        );
        assert_eq!(r.codec, Codec::H264);
        assert!(r.note.unwrap().contains("no H.265"));
    }

    #[test]
    fn a_codec_that_failed_earlier_is_not_retried() {
        let mut failures = CodecFailures::new();
        failures.mark_codec_failed("S1", Codec::H265);
        let r = resolve_codec(
            CodecPreference::Auto,
            Some(&quest()),
            support(Some(all_hw())).merged(),
            &failures.failed_codecs("S1"),
        );
        assert_eq!(r.codec, Codec::H264);
    }

    #[test]
    fn encoder_enumeration_failure_means_h264() {
        assert_eq!(
            resolve(CodecPreference::Auto, None, Some(all_hw())).codec,
            Codec::H264
        );
    }

    #[test]
    fn ten_bit_is_only_used_when_enabled_for_h265_av1_and_decodable() {
        let d = support(Some(ten()));
        assert!(!use_ten_bit(false, false, Codec::H265, false, d.merged()));
        assert!(use_ten_bit(true, false, Codec::H265, false, d.merged()));
        assert!(use_ten_bit(true, false, Codec::Av1, false, d.merged()));
        assert!(!use_ten_bit(true, false, Codec::H264, false, d.merged()));
        let mut no = all_hw();
        no.insert("h265_10".into(), caps(false, false));
        assert!(!use_ten_bit(
            true,
            false,
            Codec::H265,
            false,
            support(Some(no)).merged()
        ));
    }

    #[test]
    fn ten_bit_is_skipped_after_it_failed_or_when_forced_to_8_bit() {
        let d = support(Some(ten()));
        let mut failures = CodecFailures::new();
        assert!(!use_ten_bit(true, true, Codec::H265, false, d.merged()));
        failures.mark_ten_bit_failed("S1", Codec::H265);
        let check = |c| use_ten_bit(true, false, c, failures.ten_bit_failed("S1", c), d.merged());
        assert!(!check(Codec::H265));
        assert!(check(Codec::Av1));
    }

    #[test]
    fn a_remote_viewer_without_10_bit_support_disables_it() {
        let mut d = support(Some(ten()));
        let ipad: DecoderSupport = [
            ("h264".to_string(), caps(true, true)),
            ("h265".to_string(), caps(true, true)),
        ]
        .into_iter()
        .collect();
        assert!(d.set_viewer("ipad", Some(ipad)));
        assert!(!use_ten_bit(true, false, Codec::H265, false, d.merged()));
        assert!(d.set_viewer("ipad", None));
        assert!(use_ten_bit(true, false, Codec::H265, false, d.merged()));
    }

    #[test]
    fn custom_quality_maps_to_encoder_parameters() {
        let s = Settings {
            wifi_quality: QualityKey::Custom,
            wifi_custom: CustomQuality {
                max_size: 1600,
                bit_rate_mbps: 12.5,
                max_fps: 72,
            },
            ..Default::default()
        };
        let q = quality_for(&s, TransportKind::Wifi);
        assert_eq!(q.max_size, 1600);
        assert_eq!(q.bit_rate, 12_500_000);
        assert_eq!(q.max_fps, 72);
    }

    // ---- additional Rust-side checks

    #[test]
    fn custom_quality_label_and_fallbacks() {
        let mut s = Settings {
            usb_quality: QualityKey::Custom,
            usb_custom: CustomQuality {
                max_size: 0,
                bit_rate_mbps: 0.0,
                max_fps: 0,
            },
            ..Default::default()
        };
        let q = quality_for(&s, TransportKind::Usb);
        assert_eq!(q.label, "Custom (full res · 8 Mbps · 60 fps)");
        assert_eq!((q.max_size, q.bit_rate, q.max_fps), (0, 8_000_000, 60));
        s.usb_custom = CustomQuality {
            max_size: 2560,
            bit_rate_mbps: 500.0,
            max_fps: 90,
        };
        let q = quality_for(&s, TransportKind::Usb);
        assert_eq!(q.label, "Custom (2560px · 200 Mbps · 90 fps)");
        assert_eq!(q.bit_rate, 200_000_000);
        // Defaults: USB high, Wi-Fi balanced.
        let d = Settings::default();
        assert_eq!(quality_for(&d, TransportKind::Usb).max_size, 2560);
        assert_eq!(quality_for(&d, TransportKind::Wifi).max_size, 1920);
    }

    #[test]
    fn explicit_codec_notes() {
        let mut failed = BTreeSet::new();
        failed.insert(Codec::Av1);
        let r = resolve_codec(
            CodecPreference::Fixed(Codec::Av1),
            Some(&quest()),
            &all_hw(),
            &failed,
        );
        assert_eq!(
            r.note.as_deref(),
            Some("AV1 failed earlier on this headset — using H.264.")
        );
        let r = resolve(CodecPreference::Fixed(Codec::H265), Some(&quest()), None);
        assert_eq!(
            r.note.as_deref(),
            Some("This computer can't decode H.265 (HEVC) — using H.264.")
        );
        // Unknown encoders but decodable: allowed, no encoder name.
        let r = resolve(CodecPreference::Fixed(Codec::H265), None, Some(all_hw()));
        assert_eq!(
            r,
            CodecChoice {
                codec: Codec::H265,
                encoder: None,
                note: None
            }
        );
        let r = resolve(CodecPreference::Fixed(Codec::H264), Some(&quest()), None);
        assert_eq!(r.encoder.as_deref(), Some("c2.qti.avc.encoder"));
        assert!(r.note.is_none());
    }

    #[test]
    fn hardware_encoders_prefer_non_aliases() {
        let list = vec![
            EncoderInfo {
                codec: "h265".into(),
                name: "OMX.alias.hevc".into(),
                hw: true,
                alias: true,
            },
            enc("h265", "c2.real.hevc", true),
        ];
        let r = resolve(CodecPreference::Auto, Some(&list), Some(all_hw()));
        assert_eq!(r.encoder.as_deref(), Some("c2.real.hevc"));
    }

    #[test]
    fn decoder_support_is_intersected_and_h264_always_allowed() {
        let mut s = DecoderSupportSet::new();
        assert_eq!(s.merged().get("h264"), Some(&caps(true, false)));
        s.set_local(Some(BTreeMap::from([(
            "h264".to_string(),
            caps(false, true),
        )])));
        assert_eq!(s.merged().get("h264"), Some(&caps(true, true)));
        s.set_local(Some(all_hw()));
        let tablet = BTreeMap::from([("h265".to_string(), caps(true, false))]);
        assert!(s.set_viewer("tablet", Some(tablet.clone())));
        assert!(
            !s.set_viewer("tablet", Some(tablet)),
            "same report, no change"
        );
        assert_eq!(s.merged().get("h265"), Some(&caps(true, false)));
        assert_eq!(s.merged().get("av1"), Some(&caps(false, false)));
        assert_eq!(
            s.merged().get("h264"),
            Some(&caps(true, false)),
            "H.264 hw is intersected, any is forced"
        );
        assert!(s.can_decode(Codec::H265, false));
        assert!(!s.can_decode(Codec::H265, true));
        // A `None` local report keeps the previous one.
        s.set_local(None);
        assert!(s.can_decode(Codec::H265, false));
    }

    #[test]
    fn h264_is_never_marked_failed() {
        let mut f = CodecFailures::new();
        f.mark_codec_failed("S1", Codec::H264);
        assert!(f.failed_codecs("S1").is_empty());
    }
}
