//! The "Video codecs" view: a headset's hardware encoders next to this
//! device's decoders, a ✓/✗ matrix for H.264 / H.265 / AV1 (8 and 10 bit),
//! and which codec the policy picks for the current and the next stream (and why).
//!
//! Green = hardware on both sides, amber = works but software on one side (or
//! not known yet), red = not possible. Amber and red rows explain the fix when tapped.

use eframe::egui::{self, Color32, Margin, RichText, Sense, Stroke, vec2};
use xrmc_core::codec_policy::{self, DecodeCaps, DecoderSupport, EncoderInfo};
use xrmc_core::config::{AUTO_CODEC_ORDER, Codec, CodecPreference};
use xrmc_core::stream_manager::EncodersReply;
use xrmc_core::{Settings, StreamState, TransportKind};
use xrmc_decode::CodecDetail;

use super::icons::{self, Icon};
use super::widgets::{self, Level, Run};
use crate::model::EnvInfo;
use crate::theme;

/// The codec profiles shown, by decoder-support key.
pub const PROFILES: [(&str, Codec, bool); 5] = [
    ("h264", Codec::H264, false),
    ("h265", Codec::H265, false),
    ("h265_10", Codec::H265, true),
    ("av1", Codec::Av1, false),
    ("av1_10", Codec::Av1, true),
];

pub fn profile_label(codec: Codec, ten_bit: bool) -> String {
    format!(
        "{} {}",
        codec.label(),
        if ten_bit { "10-bit" } else { "8-bit" }
    )
}

/// The headset side of one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncSide {
    /// The encoder list is not known (headset never connected with this app).
    Unknown,
    None,
    Software(String),
    Hardware(String),
}

/// This device's side of one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecSide {
    None,
    Software(String),
    /// Decoder name and hardware API.
    Hardware(String, Option<String>),
}

/// One row of the matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct MatrixRow {
    pub key: &'static str,
    pub codec: Codec,
    pub ten_bit: bool,
    pub enc: EncSide,
    pub dec: DecSide,
    pub level: Level,
    /// What is wrong and how to fix it (amber and red rows).
    pub explain: Option<String>,
}

fn encoder_side(encoders: Option<&[EncoderInfo]>, codec: Codec) -> EncSide {
    let Some(list) = encoders else {
        return EncSide::Unknown;
    };
    let of = |hw: bool| {
        list.iter()
            .filter(|e| e.codec == codec.as_str() && e.hw == hw)
            .min_by_key(|e| e.alias)
    };
    match (of(true), of(false)) {
        (Some(e), _) => EncSide::Hardware(e.name.clone()),
        (None, Some(e)) => EncSide::Software(e.name.clone()),
        (None, None) => EncSide::None,
    }
}

fn decoder_side(details: &[CodecDetail], support: &DecoderSupport, key: &str) -> DecSide {
    if let Some(d) = details.iter().find(|d| d.key == key) {
        return match (d.caps.any, d.caps.hw) {
            (false, _) => DecSide::None,
            (true, true) => DecSide::Hardware(d.decoder.clone(), d.hw_api.clone()),
            (true, false) => DecSide::Software(d.decoder.clone()),
        };
    }
    match support.get(key) {
        Some(DecodeCaps {
            any: true,
            hw: true,
        }) => DecSide::Hardware(String::new(), None),
        Some(DecodeCaps { any: true, .. }) => DecSide::Software(String::new()),
        _ => DecSide::None,
    }
}

/// Decoder support (as the codec policy sees it) from the environment.
pub fn env_support(env: &EnvInfo) -> DecoderSupport {
    env.decoder_support
        .iter()
        .map(|(k, (any, hw))| (k.clone(), DecodeCaps { any: *any, hw: *hw }))
        .collect()
}

/// The ✓/✗ matrix (pure; `noun` is "computer" or "phone or tablet").
pub fn matrix(
    encoders: Option<&[EncoderInfo]>,
    env: &EnvInfo,
    noun: &str,
    mobile: bool,
) -> Vec<MatrixRow> {
    let support = env_support(env);
    PROFILES
        .iter()
        .map(|&(key, codec, ten_bit)| {
            let enc = encoder_side(encoders, codec);
            let dec = decoder_side(&env.decoder_details, &support, key);
            let label = profile_label(codec, ten_bit);
            let mut why: Vec<String> = Vec::new();
            let level = match (&enc, &dec) {
                (EncSide::None, _) => {
                    why.push(format!(
                        "This headset has no {} encoder, so it can't send {}. Auto never picks it; if Settings → Video forces {}, the stream uses H.264 instead.",
                        codec.label(),
                        codec.label(),
                        codec.label()
                    ));
                    Level::Bad
                }
                (_, DecSide::None) => {
                    why.push(no_decoder_fix(&label, noun, mobile));
                    Level::Bad
                }
                (EncSide::Unknown, _) => {
                    why.push("The headset's encoders are not known yet: connect it (USB or Wi-Fi) and open this view again.".into());
                    Level::Neutral
                }
                (EncSide::Software(_), _) | (_, DecSide::Software(_)) => {
                    if matches!(enc, EncSide::Software(_)) {
                        why.push(format!(
                            "The headset only has a software {} encoder: it can overheat the headset and drop frames. Auto only picks codecs the headset encodes in hardware.",
                            codec.label()
                        ));
                    }
                    if matches!(dec, DecSide::Software(_)) {
                        why.push(software_decoder_fix(&label, noun, mobile));
                    }
                    Level::Warn
                }
                _ => Level::Good,
            };
            if ten_bit && level != Level::Bad {
                why.push(
                    "10-bit is only used when Settings → Video → 10-bit colour is on. Many headset encoders refuse 10-bit even with a hardware encoder; the stream then falls back to 8-bit automatically.".into(),
                );
            }
            MatrixRow {
                key,
                codec,
                ten_bit,
                enc,
                dec,
                level,
                explain: (!why.is_empty()).then(|| why.join("\n\n")),
            }
        })
        .collect()
}

fn no_decoder_fix(label: &str, noun: &str, mobile: bool) -> String {
    if mobile {
        format!(
            "This {noun} has no {label} decoder (Android lists none in MediaCodec). Use H.264 or H.265 for this receiver; Auto already avoids {label}."
        )
    } else {
        format!(
            "This {noun} can't decode {label}. Linux / Raspberry Pi OS: install FFmpeg's libraries (sudo apt install ffmpeg) and restart. Windows / macOS: the decoder ships with the app — reinstall it. Auto avoids codecs this {noun} can't decode."
        )
    }
}

fn software_decoder_fix(label: &str, noun: &str, mobile: bool) -> String {
    if mobile {
        format!(
            "This {noun} decodes {label} in software (on the CPU): fine for one or two headsets, but it drains the battery and limits the frame rate with more. Auto only picks H.265/AV1 when they are hardware-decoded here; H.264 is always available."
        )
    } else {
        format!(
            "This {noun} decodes {label} on the CPU. With many headsets this can limit the frame rate. Linux: install your GPU's VA-API driver (mesa-va-drivers / intel-media-va-driver) for hardware decoding. Auto only picks H.265/AV1 when they are hardware-decoded here."
        )
    }
}

/// What the codec policy picks for the next stream, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiation {
    pub codec: Codec,
    pub ten_bit: bool,
    pub encoder: Option<String>,
    pub reason: String,
}

/// Explain [`codec_policy::resolve_codec`] + [`codec_policy::use_ten_bit`] for one transport.
pub fn negotiate(
    settings: &Settings,
    kind: TransportKind,
    encoders: Option<&[EncoderInfo]>,
    support: &DecoderSupport,
    noun: &str,
) -> Negotiation {
    let pref = settings.codec_preference(kind);
    let none = std::collections::BTreeSet::new();
    let choice = codec_policy::resolve_codec(pref, encoders, support, &none);
    let mut reason = Vec::new();
    match pref {
        CodecPreference::Auto => {
            let list = encoders.unwrap_or(&[]);
            for c in AUTO_CODEC_ORDER {
                if c == Codec::H264 {
                    break;
                }
                let hw_enc = list.iter().any(|e| e.codec == c.as_str() && e.hw);
                let hw_dec = support.get(c.as_str()).is_some_and(|d| d.hw);
                if c == choice.codec {
                    reason.push(format!(
                        "{}: hardware encoder on the headset and hardware decoder here.",
                        c.label()
                    ));
                    break;
                }
                reason.push(format!(
                    "{}: skipped — {}.",
                    c.label(),
                    match (encoders.is_some(), hw_enc, hw_dec) {
                        (false, _, _) => "the headset's encoders are not known yet".to_string(),
                        (_, false, false) => format!(
                            "no hardware encoder on the headset and no hardware decoder on this {noun}"
                        ),
                        (_, false, true) => "no hardware encoder on the headset".to_string(),
                        _ => format!("this {noun} can't decode it in hardware"),
                    }
                ));
            }
            if choice.codec == Codec::H264 {
                reason.push("H.264: works everywhere (the fallback).".into());
            }
            reason.insert(0, "Auto (Settings → Video).".into());
        }
        CodecPreference::Fixed(c) => {
            reason.push(format!("Set to {} in Settings → Video.", c.label()));
        }
    }
    if let Some(n) = &choice.note {
        reason.push(n.clone());
    }
    let ten_bit = codec_policy::use_ten_bit(settings.ten_bit, false, choice.codec, false, support);
    if settings.ten_bit {
        reason.push(if ten_bit {
            "10-bit requested (falls back to 8-bit if the headset refuses).".into()
        } else if !choice.codec.supports_ten_bit() {
            format!(
                "10-bit colour is on, but {} is 8-bit only.",
                choice.codec.label()
            )
        } else {
            format!(
                "10-bit colour is on, but this {noun} can't decode 10-bit {}.",
                choice.codec.label()
            )
        });
    }
    Negotiation {
        codec: choice.codec,
        ten_bit,
        encoder: choice.encoder,
        reason: reason.join(" "),
    }
}

/// One line about a running stream: codec, encoder and decoder actually used.
pub fn current_line(st: &StreamState, decoder: &str, hardware: bool) -> Option<String> {
    let codec = st.codec?;
    let mut s = profile_label(codec, st.ten_bit);
    if let Some(e) = &st.encoder {
        s.push_str(&format!(" · headset encoder {e}"));
    }
    if !decoder.is_empty() {
        s.push_str(&format!(
            " · decoded by {decoder} ({})",
            if hardware { "hardware" } else { "software" }
        ));
    }
    Some(s)
}

// -------------------------------------------------------------------- UI

/// State of the codecs dialog.
#[derive(Debug, Clone, PartialEq)]
pub struct CodecsState {
    pub id: Option<String>,
    pub enc_ticket: Option<u64>,
    pub encoders: Option<Result<EncodersReply, String>>,
    /// The row whose explanation is open.
    pub open: Option<String>,
}

/// Data the dialog body shows about the selected headset's stream.
pub struct StreamInfo<'a> {
    pub state: Option<&'a StreamState>,
    pub decoder: String,
    pub hardware: bool,
}

fn level_icon(level: Level) -> Icon {
    match level {
        Level::Good => Icon::Check,
        Level::Bad => Icon::X,
        _ => Icon::Alert,
    }
}

/// The matrix and negotiation for one headset (or only this device's decoders).
pub fn body(
    ui: &mut egui::Ui,
    st: &mut CodecsState,
    env: &EnvInfo,
    settings: &Settings,
    stream: StreamInfo<'_>,
    noun: &str,
    mobile: bool,
) {
    let encoders: Option<Vec<EncoderInfo>> = match &st.encoders {
        Some(Ok(r)) => r.encoders.clone(),
        _ => None,
    };
    if st.id.is_some() {
        match &st.encoders {
            None => {
                widgets::small_muted(ui, "Asking the headset for its encoders…");
            }
            Some(Err(e)) => {
                widgets::small_muted(ui, format!("Could not list the headset's encoders: {e}"));
            }
            Some(Ok(r)) if !r.live && r.encoders.is_some() => {
                widgets::small_muted(
                    ui,
                    "Headset offline: showing the encoders it reported last time.",
                );
            }
            _ => {}
        }
    }
    let support = env_support(env);
    if let Some(state) = stream.state
        && let Some(line) = current_line(state, &stream.decoder, stream.hardware)
    {
        widgets::heading(ui, "Now streaming");
        widgets::rich(ui, 14.0, &[(&line, Run::Normal)]);
    }
    if st.id.is_some() {
        widgets::heading(ui, "Next stream");
        let usb = negotiate(
            settings,
            TransportKind::Usb,
            encoders.as_deref(),
            &support,
            noun,
        );
        let wifi = negotiate(
            settings,
            TransportKind::Wifi,
            encoders.as_deref(),
            &support,
            noun,
        );
        let same =
            usb.codec == wifi.codec && usb.ten_bit == wifi.ten_bit && usb.reason == wifi.reason;
        let show = |ui: &mut egui::Ui, title: &str, n: &Negotiation| {
            let head = format!("{}{}", title, profile_label(n.codec, n.ten_bit));
            let enc = n
                .encoder
                .as_ref()
                .map(|e| format!("  ({e})"))
                .unwrap_or_default();
            widgets::rich(ui, 14.5, &[(&head, Run::Bold), (&enc, Run::Muted)]);
            widgets::small_muted(ui, &n.reason);
        };
        if same {
            show(ui, "", &usb);
        } else {
            show(ui, "USB: ", &usb);
            show(ui, "Wi-Fi: ", &wifi);
        }
    }
    widgets::heading(
        ui,
        if st.id.is_some() {
            "Headset encoders vs. this receiver's decoders"
        } else {
            "This receiver's decoders"
        },
    );
    let rows = matrix(
        if st.id.is_some() {
            encoders.as_deref()
        } else {
            Some(&[])
        },
        env,
        noun,
        mobile,
    );
    for row in rows {
        let row = if st.id.is_none() {
            // Decoder-only view: judge by the decoder alone.
            MatrixRow {
                level: match row.dec {
                    DecSide::None => Level::Bad,
                    DecSide::Software(_) => Level::Warn,
                    DecSide::Hardware(..) => Level::Good,
                },
                explain: match row.dec {
                    DecSide::None => Some(no_decoder_fix(
                        &profile_label(row.codec, row.ten_bit),
                        noun,
                        mobile,
                    )),
                    DecSide::Software(_) => Some(software_decoder_fix(
                        &profile_label(row.codec, row.ten_bit),
                        noun,
                        mobile,
                    )),
                    DecSide::Hardware(..) => None,
                },
                ..row
            }
        } else {
            row
        };
        matrix_row(ui, st, &row, noun);
    }
    widgets::small_muted(
        ui,
        "Green: hardware on both sides. Amber: works, but software on one side (or not known yet). Red: not possible. Tap amber or red rows for the fix.",
    );
}

fn matrix_row(ui: &mut egui::Ui, st: &mut CodecsState, row: &MatrixRow, noun: &str) {
    let color = widgets::level_color(row.level);
    let clickable =
        matches!(row.level, Level::Warn | Level::Bad | Level::Neutral) && row.explain.is_some();
    let open = st.open.as_deref() == Some(row.key);
    let resp = egui::Frame::new()
        .fill(theme::BG_2)
        .stroke(Stroke::new(1.0, theme::mix(color, theme::LINE, 0.65)))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 3.0;
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                icons::paint(ui.painter(), r, level_icon(row.level), color);
                ui.label(
                    RichText::new(profile_label(row.codec, row.ten_bit))
                        .font(theme::font_bold(14.5))
                        .color(theme::TEXT),
                );
                if clickable {
                    ui.label(
                        RichText::new(if open { "hide" } else { "why? / fix" })
                            .font(theme::font(12.0))
                            .color(color),
                    );
                }
            });
            let enc = match &row.enc {
                EncSide::Unknown => "not known yet".to_string(),
                EncSide::None => "none".to_string(),
                EncSide::Software(n) => format!("software ({n})"),
                EncSide::Hardware(n) => format!("hardware ({n})"),
            };
            let dec = match &row.dec {
                DecSide::None => "none".to_string(),
                DecSide::Software(n) if n.is_empty() => "software".to_string(),
                DecSide::Software(n) => format!("software ({n})"),
                DecSide::Hardware(n, api) => {
                    let mut s = "hardware".to_string();
                    let extra: Vec<&str> = [api.as_deref(), Some(n.as_str())]
                        .into_iter()
                        .flatten()
                        .filter(|s| !s.is_empty())
                        .collect();
                    if !extra.is_empty() {
                        s.push_str(&format!(" ({})", extra.join(", ")));
                    }
                    s
                }
            };
            if st.id.is_some() {
                widgets::rich(ui, 13.0, &[("Headset: ", Run::Muted), (&enc, Run::Normal)]);
            }
            let this = format!("This {noun}: ");
            widgets::rich(ui, 13.0, &[(&this, Run::Muted), (&dec, Run::Normal)]);
            if open && let Some(e) = &row.explain {
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(e)
                            .font(theme::font(13.0))
                            .color(theme::HINT_WARN),
                    )
                    .wrap(),
                );
            }
        })
        .response;
    if clickable {
        let resp = ui.interact(
            resp.rect,
            egui::Id::unique(("codec-row", row.key)),
            Sense::click(),
        );
        if resp.hovered() {
            ui.painter()
                .rect_filled(resp.rect, 10.0, Color32::from_white_alpha(8));
        }
        if resp.clicked() {
            st.open = if open {
                None
            } else {
                Some(row.key.to_string())
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(codec: &str, name: &str, hw: bool) -> EncoderInfo {
        EncoderInfo {
            codec: codec.into(),
            name: name.into(),
            hw,
            alias: false,
        }
    }

    fn env(details: &[(&str, bool, bool)]) -> EnvInfo {
        EnvInfo {
            decoder_support: details
                .iter()
                .map(|(k, any, hw)| (k.to_string(), (*any, *hw)))
                .collect(),
            decoder_details: details
                .iter()
                .map(|(k, any, hw)| CodecDetail {
                    key: k.to_string(),
                    caps: xrmc_decode::DecodeCaps { any: *any, hw: *hw },
                    decoder: if *any {
                        format!("dec-{k}")
                    } else {
                        String::new()
                    },
                    hw_api: hw.then(|| "MediaCodec".to_string()),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn matrix_colours() {
        let e = env(&[
            ("h264", true, true),
            ("h265", true, false),
            ("h265_10", true, false),
            ("av1", false, false),
            ("av1_10", false, false),
        ]);
        let encs = [
            enc("h264", "c2.qti.avc.encoder", true),
            enc("h265", "c2.qti.hevc.encoder", true),
        ];
        let rows = matrix(Some(&encs), &e, "phone or tablet", true);
        let level = |k: &str| rows.iter().find(|r| r.key == k).unwrap().level;
        assert_eq!(level("h264"), Level::Good);
        assert_eq!(level("h265"), Level::Warn, "software decoder here");
        assert_eq!(level("av1"), Level::Bad, "no AV1 encoder");
        assert!(
            rows.iter()
                .filter(|r| r.level != Level::Good)
                .all(|r| r.explain.is_some())
        );
        // Unknown encoders: neutral, but red when this device can't decode.
        let rows = matrix(None, &e, "computer", false);
        assert_eq!(rows[0].level, Level::Neutral);
        assert_eq!(rows[0].enc, EncSide::Unknown);
    }

    #[test]
    fn negotiation_explains_the_choice() {
        let mut s = Settings::default();
        let encs = [
            enc("h264", "c2.qti.avc.encoder", true),
            enc("h265", "c2.qti.hevc.encoder", true),
        ];
        let hw = env(&[
            ("h264", true, true),
            ("h265", true, true),
            ("h265_10", true, true),
        ]);
        let n = negotiate(
            &s,
            TransportKind::Wifi,
            Some(&encs),
            &env_support(&hw),
            "computer",
        );
        assert_eq!(n.codec, Codec::H265);
        assert_eq!(n.encoder.as_deref(), Some("c2.qti.hevc.encoder"));
        assert!(n.reason.contains("hardware encoder on the headset"));

        let sw = env(&[("h264", true, true), ("h265", true, false)]);
        let n = negotiate(
            &s,
            TransportKind::Wifi,
            Some(&encs),
            &env_support(&sw),
            "computer",
        );
        assert_eq!(n.codec, Codec::H264);
        assert!(
            n.reason.contains("can't decode it in hardware"),
            "{}",
            n.reason
        );

        s.ten_bit = true;
        let n = negotiate(
            &s,
            TransportKind::Wifi,
            Some(&encs),
            &env_support(&hw),
            "computer",
        );
        assert!(n.ten_bit);
        let n = negotiate(&s, TransportKind::Wifi, None, &env_support(&hw), "computer");
        assert_eq!(n.codec, Codec::H264);
        assert!(n.reason.contains("not known yet"));
    }
}
