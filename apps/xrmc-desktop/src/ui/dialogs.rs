//! Dialogs: verify, rename/colour, add by IP, settings, connection guide,
//! fix explanations (for amber/red statuses), forget and quit confirmations.

use eframe::egui::{self, Align, Color32, Layout, Margin, RichText, Sense, Stroke, vec2};
use serde_json::json;
use xrmc_core::config::{Codec, CodecPreference, HEADSET_COLORS, QualityKey, quality_presets};
use xrmc_core::settings::CustomQuality;
use xrmc_core::stream_manager::EncodersReply;
use xrmc_core::view_size::ViewMode;
use xrmc_core::{Settings, TransportKind, TransportPreference};

use super::icons::{self, Icon};
use super::widgets::{self, Btn, Kind, Run};
use super::{App, Cmd, UI_SCALE_KEY};
use crate::bridge::Action;
use crate::model::{self, Model, VIEW_MODES};
use crate::theme;

/// What an amber or red status means and how to fix it.
#[derive(Debug, Clone, PartialEq)]
pub enum Fix {
    AdbError(String),
    ServerMissing,
    BatteryLow(String),
    NoDecoder(String),
    SoftwareDecoder(String),
    Demo,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dialog {
    Verify {
        id: String,
        name: String,
        color: String,
        wifi: bool,
        busy: Option<u64>,
    },
    Edit {
        id: String,
        name: String,
        color: String,
        transport: TransportPreference,
        busy: Option<u64>,
        enc_ticket: Option<u64>,
        encoders: Option<Result<EncodersReply, String>>,
    },
    AddIp {
        address: String,
        busy: Option<u64>,
    },
    Settings {
        draft: Box<Settings>,
        /// The settings when the dialog opened: only what the user changed is saved.
        orig: Box<Settings>,
        scale: f32,
        busy: Option<u64>,
    },
    Guide {
        brand: String,
    },
    Fix(Fix),
    Forget {
        id: String,
    },
    Quit,
}

enum Outcome {
    Close,
    Send(Action),
    Cmd(Cmd),
}

impl Dialog {
    pub fn verify(m: &Model, id: &str) -> Dialog {
        let used: Vec<&str> = m
            .headsets
            .iter()
            .filter(|h| h.trusted)
            .map(|h| h.color.as_str())
            .collect();
        let color = HEADSET_COLORS
            .iter()
            .find(|c| !used.contains(c))
            .unwrap_or(&HEADSET_COLORS[0]);
        Dialog::Verify {
            id: id.into(),
            name: String::new(),
            color: color.to_string(),
            wifi: true,
            busy: None,
        }
    }

    pub fn edit(m: &Model, id: &str) -> Dialog {
        let h = m.headset(id);
        Dialog::Edit {
            id: id.into(),
            name: h.map(|h| h.name.clone()).unwrap_or_default(),
            color: h.map(|h| h.color.clone()).unwrap_or_default(),
            transport: h.map(|h| h.transport).unwrap_or_default(),
            busy: None,
            enc_ticket: None,
            encoders: None,
        }
    }

    pub fn add_ip() -> Dialog {
        Dialog::AddIp {
            address: String::new(),
            busy: None,
        }
    }

    pub fn settings(m: &Model, scale: f32) -> Dialog {
        Dialog::Settings {
            draft: Box::new(m.settings.clone()),
            orig: Box::new(m.settings.clone()),
            scale,
            busy: None,
        }
    }

    pub fn busy_ticket(&self) -> Option<u64> {
        match self {
            Dialog::Verify { busy, .. }
            | Dialog::Edit { busy, .. }
            | Dialog::AddIp { busy, .. }
            | Dialog::Settings { busy, .. } => *busy,
            _ => None,
        }
    }

    pub fn set_busy(&mut self, t: Option<u64>) {
        match self {
            Dialog::Verify { busy, .. }
            | Dialog::Edit { busy, .. }
            | Dialog::AddIp { busy, .. }
            | Dialog::Settings { busy, .. } => *busy = t,
            _ => {}
        }
    }
}

const MAX_SIZE_OPTIONS: [(u32, &str); 6] = [
    (1280, "1280 px"),
    (1600, "1600 px"),
    (1920, "1920 px"),
    (2560, "2560 px"),
    (3200, "3200 px"),
    (0, "Full resolution"),
];
const FPS_OPTIONS: [u32; 6] = [30, 45, 60, 72, 90, 120];
const SCALES: [(f32, &str); 8] = [
    (0.75, "75 %"),
    (0.9, "90 %"),
    (1.0, "100 % (system)"),
    (1.15, "115 %"),
    (1.25, "125 %"),
    (1.5, "150 %"),
    (1.75, "175 %"),
    (2.0, "200 %"),
];

impl App {
    pub(super) fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.dialog.take() else {
            return;
        };
        let screen = ctx.content_rect();
        let wide = matches!(d, Dialog::Settings { .. } | Dialog::Guide { .. });
        let width = (if wide { 780.0_f32 } else { 540.0 }).min(screen.width() - 24.0);
        let max_body = (screen.height() - 190.0).max(160.0);
        let title = match &d {
            Dialog::Verify { .. } => "Verify headset".to_string(),
            Dialog::Edit { name, .. } => format!("Edit {name}"),
            Dialog::AddIp { .. } => "Connect by IP address".into(),
            Dialog::Settings { .. } => "Settings".into(),
            Dialog::Guide { .. } => "Connecting headsets".into(),
            Dialog::Fix(f) => fix_title(f).into(),
            Dialog::Forget { .. } => "Forget headset?".into(),
            Dialog::Quit => "Quit XR MultiCast?".into(),
        };
        let busy = d.busy_ticket().is_some();
        let mut outcome = None;
        let modal = egui::Modal::new(egui::Id::unique("dialog"))
            .backdrop_color(Color32::from_black_alpha(150))
            .frame(
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .stroke(Stroke::new(1.0, theme::LINE_2))
                    .corner_radius(14)
                    .inner_margin(Margin::same(0)),
            )
            .show(ctx, |ui| {
                ui.set_width(width);
                // Header.
                egui::Frame::new()
                    .inner_margin(Margin {
                        left: 20,
                        right: 8,
                        top: 8,
                        bottom: 8,
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&title)
                                    .font(theme::font_bold(18.0))
                                    .color(theme::TEXT),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if Btn::icon(Icon::X)
                                    .kind(Kind::Ghost)
                                    .tooltip("Close")
                                    .show(ui)
                                    .clicked()
                                {
                                    outcome = Some(Outcome::Close);
                                }
                            });
                        });
                    });
                let line = ui.max_rect();
                ui.painter().line_segment(
                    [
                        egui::pos2(line.min.x, ui.cursor().min.y),
                        egui::pos2(line.max.x, ui.cursor().min.y),
                    ],
                    Stroke::new(1.0, theme::LINE),
                );
                // Body.
                egui::ScrollArea::vertical()
                    .max_height(max_body)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(20, 14))
                            .show(ui, |ui| {
                                ui.set_width(width - 40.0);
                                if let Some(o) = self.dialog_body(ui, &mut d) {
                                    outcome = Some(o);
                                }
                            });
                    });
                // Footer.
                let y = ui.cursor().min.y;
                ui.painter().line_segment(
                    [egui::pos2(line.min.x, y), egui::pos2(line.max.x, y)],
                    Stroke::new(1.0, theme::LINE),
                );
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        ui.set_width(width - 32.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if let Some(o) = self.dialog_footer(ui, &mut d, busy) {
                                outcome = Some(o);
                            }
                        });
                    });
            });
        if modal.should_close() && !busy && outcome.is_none() {
            outcome = Some(Outcome::Close);
        }
        match outcome {
            Some(Outcome::Close) => {}
            Some(Outcome::Send(action)) => {
                let keep = matches!(
                    d,
                    Dialog::Verify { .. }
                        | Dialog::Edit { .. }
                        | Dialog::AddIp { .. }
                        | Dialog::Settings { .. }
                );
                let ticket = self.send(action);
                if keep {
                    d.set_busy(Some(ticket));
                    self.dialog = Some(d);
                }
            }
            Some(Outcome::Cmd(c)) => {
                self.cmds.push(c);
            }
            None => self.dialog = Some(d),
        }
    }

    fn dialog_body(&mut self, ui: &mut egui::Ui, d: &mut Dialog) -> Option<Outcome> {
        ui.spacing_mut().item_spacing.y = 10.0;
        match d {
            Dialog::Verify {
                id,
                name,
                color,
                wifi,
                ..
            } => {
                let h = self.model.headset(id).cloned();
                let brand = model::brand_of(h.as_ref());
                let bc = theme::parse_hex(&brand.color, theme::MUTED);
                egui::Frame::new()
                    .fill(theme::BG_2)
                    .stroke(Stroke::new(1.0, theme::LINE_2))
                    .corner_radius(10)
                    .inner_margin(Margin::same(12))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
                            ui.painter().rect_filled(r, 10.0, bc.gamma_multiply(0.25));
                            icons::paint(ui.painter(), r.shrink(10.0), Icon::Headset, bc);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(
                                    RichText::new(format!(
                                        "{} — {}",
                                        brand.family,
                                        h.as_ref().map(|h| h.model.as_str()).unwrap_or("")
                                    ))
                                    .font(theme::font_bold(15.0)),
                                );
                                let details: Vec<String> = [
                                    h.as_ref()
                                        .and_then(|h| h.serial.clone())
                                        .map(|s| format!("Serial {s}")),
                                    h.as_ref().and_then(|h| h.manufacturer.clone()),
                                    h.as_ref()
                                        .and_then(|h| h.android_version.clone())
                                        .map(|v| format!("Android {v}")),
                                ]
                                .into_iter()
                                .flatten()
                                .collect();
                                widgets::small_muted(ui, details.join(" · "));
                            });
                        });
                    });
                widgets::muted(
                    ui,
                    "Verifying confirms this exact headset (by hardware serial) is allowed to cast to this computer. Give it a clear label — it is shown on its stream at all times.",
                );
                field_label(ui, "Label");
                ui.add(
                    egui::TextEdit::singleline(name)
                        .hint_text("e.g. Station 3, Trainee Alex")
                        .char_limit(40)
                        .desired_width(f32::INFINITY)
                        .min_size(vec2(0.0, 44.0))
                        .align(egui::Align2::LEFT_CENTER),
                );
                field_label(ui, "Colour");
                swatches(ui, color);
                widgets::check_row(
                    ui,
                    wifi,
                    "Enable Wi-Fi casting now",
                    "You can unplug the cable afterwards.",
                );
                None
            }
            Dialog::Edit {
                id,
                name,
                color,
                transport,
                encoders,
                ..
            } => {
                let h = self.model.headset(id).cloned();
                field_label(ui, "Label");
                ui.add(
                    egui::TextEdit::singleline(name)
                        .char_limit(40)
                        .desired_width(f32::INFINITY)
                        .min_size(vec2(0.0, 44.0))
                        .align(egui::Align2::LEFT_CENTER),
                );
                field_label(ui, "Colour");
                swatches(ui, color);
                field_label(ui, "Preferred connection");
                let label = |t: TransportPreference| match t {
                    TransportPreference::Auto => "Automatic (USB when plugged in, else Wi-Fi)",
                    TransportPreference::Usb => "USB only",
                    TransportPreference::Wifi => "Wi-Fi only",
                };
                egui::ComboBox::from_id_salt("e-transport")
                    .width(ui.available_width())
                    .selected_text(label(*transport))
                    .show_ui(ui, |ui| {
                        for t in [
                            TransportPreference::Auto,
                            TransportPreference::Usb,
                            TransportPreference::Wifi,
                        ] {
                            ui.selectable_value(transport, t, label(t));
                        }
                    });
                field_label(ui, "Video encoders on this headset");
                match encoders {
                    None => {
                        widgets::small_muted(
                            ui,
                            if h.as_ref().is_some_and(|h| h.is_online()) {
                                "Checking…"
                            } else {
                                "Connect the headset to check."
                            },
                        );
                    }
                    Some(Err(e)) => {
                        widgets::small_muted(ui, format!("Could not check: {e}"));
                    }
                    Some(Ok(r)) => match &r.encoders {
                        Some(list) if !list.is_empty() => {
                            for c in Codec::ALL {
                                let encs: Vec<_> = list
                                    .iter()
                                    .filter(|e| e.codec == c.as_str() && !e.alias)
                                    .collect();
                                let label = format!("{}:  ", c.label());
                                if encs.is_empty() {
                                    widgets::rich(
                                        ui,
                                        13.0,
                                        &[
                                            (&label, Run::Normal),
                                            ("not available (H.264 is used instead)", Run::Muted),
                                        ],
                                    );
                                } else {
                                    let hw = encs.iter().any(|e| e.hw);
                                    let names = format!(
                                        "  ({})",
                                        encs.iter()
                                            .map(|e| e.name.as_str())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    );
                                    widgets::rich(
                                        ui,
                                        13.0,
                                        &[
                                            (&label, Run::Normal),
                                            (
                                                if hw { "hardware" } else { "software only" },
                                                Run::Bold,
                                            ),
                                            (&names, Run::Muted),
                                        ],
                                    );
                                }
                            }
                        }
                        _ => {
                            widgets::small_muted(
                                ui,
                                if r.live {
                                    "None reported."
                                } else {
                                    "Not checked yet — connect the headset."
                                },
                            );
                        }
                    },
                }
                if let Some(h) = &h {
                    widgets::small_muted(
                        ui,
                        format!(
                            "Serial {} · verified {}",
                            h.serial.clone().unwrap_or_default(),
                            h.verified_at
                                .map(|t| model::time_ago(t, model::now_ms()))
                                .unwrap_or_else(|| "—".into())
                        ),
                    );
                }
                None
            }
            Dialog::AddIp { address, .. } => {
                widgets::muted(
                    ui,
                    "The headset must already be in network-debugging mode (enable Wi-Fi casting for it once via USB, or turn on Wireless debugging on Android 11+ headsets that support it).",
                );
                field_label(ui, "Address");
                let r = ui.add(
                    egui::TextEdit::singleline(address)
                        .hint_text("192.168.1.50 or 192.168.1.50:5555")
                        .desired_width(f32::INFINITY)
                        .min_size(vec2(0.0, 44.0))
                        .align(egui::Align2::LEFT_CENTER),
                );
                if !r.has_focus() && address.is_empty() {
                    r.request_focus();
                }
                if r.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !address.trim().is_empty()
                {
                    return Some(Outcome::Send(Action::Connect {
                        address: address.trim().to_string(),
                    }));
                }
                None
            }
            Dialog::Settings { draft, scale, .. } => {
                self.settings_body(ui, draft, scale);
                None
            }
            Dialog::Guide { brand } => {
                self.guide_body(ui, brand);
                None
            }
            Dialog::Fix(f) => {
                fix_body(ui, f);
                None
            }
            Dialog::Forget { id } => {
                let name = self
                    .model
                    .headset(id)
                    .map(|h| h.name.clone())
                    .unwrap_or_default();
                ui.add(egui::Label::new(
                    RichText::new(format!("{name} will be removed from this computer. It must be verified over USB again before it can cast.")),
                ).wrap());
                None
            }
            Dialog::Quit => {
                widgets::muted(
                    ui,
                    "This hub is running in kiosk mode. Quitting stops all streams.",
                );
                None
            }
        }
    }

    fn dialog_footer(&mut self, ui: &mut egui::Ui, d: &mut Dialog, busy: bool) -> Option<Outcome> {
        // Right to left.
        let primary = |ui: &mut egui::Ui, label: &str, enabled: bool| {
            Btn::new(label)
                .primary()
                .min_width(110.0)
                .enabled(enabled && !busy)
                .busy(busy)
                .show(ui)
                .clicked()
        };
        let cancel = |ui: &mut egui::Ui, label: &str| {
            Btn::new(label)
                .min_width(100.0)
                .enabled(!busy)
                .show(ui)
                .clicked()
        };
        match d {
            Dialog::Verify {
                id,
                name,
                color,
                wifi,
                ..
            } => {
                if primary(ui, "Verify & save", true) {
                    return Some(Outcome::Send(Action::Verify {
                        id: id.clone(),
                        name: name.trim().to_string(),
                        color: color.clone(),
                        enable_wireless: *wifi,
                    }));
                }
                if cancel(ui, "Cancel") {
                    return Some(Outcome::Close);
                }
            }
            Dialog::Edit {
                id,
                name,
                color,
                transport,
                ..
            } => {
                if primary(ui, "Save", true) {
                    let mut patch = serde_json::Map::new();
                    let fallback = self
                        .model
                        .headset(id)
                        .map(|h| h.name.clone())
                        .unwrap_or_default();
                    let n = if name.trim().is_empty() {
                        fallback
                    } else {
                        name.trim().to_string()
                    };
                    patch.insert("name".into(), json!(n));
                    patch.insert("color".into(), json!(color));
                    patch.insert(
                        "transport".into(),
                        serde_json::to_value(*transport).unwrap_or(json!("auto")),
                    );
                    return Some(Outcome::Send(Action::UpdateHeadset {
                        id: id.clone(),
                        patch,
                    }));
                }
                if cancel(ui, "Cancel") {
                    return Some(Outcome::Close);
                }
            }
            Dialog::AddIp { address, .. } => {
                if primary(ui, "Connect", !address.trim().is_empty()) {
                    return Some(Outcome::Send(Action::Connect {
                        address: address.trim().to_string(),
                    }));
                }
                if cancel(ui, "Cancel") {
                    return Some(Outcome::Close);
                }
            }
            Dialog::Settings {
                draft, orig, scale, ..
            } => {
                if primary(ui, "Save", true) {
                    self.ui_scale = *scale;
                    let mut patch = settings_patch(orig, draft);
                    patch.insert(UI_SCALE_KEY.into(), json!(*scale));
                    return Some(Outcome::Send(Action::UpdateSettings { patch }));
                }
                if cancel(ui, "Cancel") {
                    return Some(Outcome::Close);
                }
                if self.opts.kiosk {
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        if Btn::new("Quit…").danger().show(ui).clicked() {
                            *d = Dialog::Quit;
                        }
                    });
                }
            }
            Dialog::Fix(f) => {
                if primary(ui, "Close", true) {
                    return Some(Outcome::Close);
                }
                if matches!(f, Fix::AdbError(_) | Fix::ServerMissing | Fix::NoDecoder(_))
                    && Btn::new("Connection guide").show(ui).clicked()
                {
                    *d = Dialog::Guide {
                        brand: "meta".into(),
                    };
                }
            }
            Dialog::Guide { .. } => {
                if primary(ui, "Close", true) {
                    return Some(Outcome::Close);
                }
            }
            Dialog::Forget { id } => {
                if Btn::new("Forget")
                    .danger()
                    .min_width(110.0)
                    .show(ui)
                    .clicked()
                {
                    return Some(Outcome::Send(Action::Forget { id: id.clone() }));
                }
                if cancel(ui, "Cancel") {
                    return Some(Outcome::Close);
                }
            }
            Dialog::Quit => {
                if Btn::new("Quit")
                    .danger()
                    .min_width(110.0)
                    .show(ui)
                    .clicked()
                {
                    return Some(Outcome::Cmd(Cmd::Quit));
                }
                if cancel(ui, "Cancel") {
                    return Some(Outcome::Close);
                }
            }
        }
        None
    }

    fn settings_body(&mut self, ui: &mut egui::Ui, s: &mut Settings, scale: &mut f32) {
        let env = self.model.env.clone();
        widgets::heading(ui, "Security");
        widgets::check_row(
            ui,
            &mut s.require_usb_verification,
            "Only cast headsets verified over USB",
            "Recommended. Unverified devices found on the network are listed but cannot cast.",
        );
        widgets::heading(ui, "Connections");
        widgets::check_row(
            ui,
            &mut s.auto_reconnect_wifi,
            "Automatically reconnect verified headsets over Wi-Fi",
            "",
        );
        widgets::check_row(
            ui,
            &mut s.auto_cast_on_connect,
            "Start casting automatically when a verified headset connects",
            "",
        );
        widgets::check_row(
            ui,
            &mut s.auto_retry_streams,
            "Retry streams automatically if they drop",
            "",
        );
        ui.horizontal(|ui| {
            ui.label("Network scan port");
            ui.add(
                egui::DragValue::new(&mut s.scan_port)
                    .range(1..=65535)
                    .speed(1),
            );
        });

        widgets::heading(ui, "Video");
        for kind in [TransportKind::Usb, TransportKind::Wifi] {
            video_block(ui, s, kind);
        }
        widgets::small_muted(
            ui,
            Codec::ALL
                .iter()
                .map(|c| format!("{}: {}.", c.label(), c.description()))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let ten = |k: &str, name: &str| match env.decoder_support.get(k) {
            Some((true, hw)) => format!("{name} ({})", if *hw { "hardware" } else { "software" }),
            _ => format!("{name} not supported"),
        };
        widgets::check_row(
            ui,
            &mut s.ten_bit,
            "10-bit colour (experimental)",
            &format!(
                "Asks the headset for 10-bit H.265 (Main 10) or AV1 when one of those codecs is used. Smoother gradients (less banding). Many headset encoders refuse 10-bit; the stream then falls back to 8-bit automatically. This computer: {}, {}.",
                ten("h265_10", "H.265 10-bit"),
                ten("av1_10", "AV1 10-bit")
            ),
        );
        let decode = ["h264", "h265", "av1"]
            .iter()
            .zip(["H.264", "H.265", "AV1"])
            .map(|(k, n)| ten(k, n))
            .collect::<Vec<_>>()
            .join(", ");
        widgets::small_muted(
            ui,
            format!(
                "This computer can decode: {decode} ({}). Auto uses H.265, then AV1, only when the headset has a hardware encoder for it and this computer can hardware-decode it; otherwise H.264. A stream that fails to start or decode falls back to H.264 automatically. Changes apply to streams started afterwards.",
                env.decoder
            ),
        );
        field_label(ui, "Default view");
        let mode_label = |m: ViewMode| {
            VIEW_MODES
                .iter()
                .find(|v| v.0 == m)
                .map(|v| format!("{} — {}", v.1, v.2))
                .unwrap_or_default()
        };
        egui::ComboBox::from_id_salt("s-view")
            .width(ui.available_width())
            .selected_text(mode_label(s.default_view_mode))
            .show_ui(ui, |ui| {
                for (m, _, _) in VIEW_MODES {
                    ui.selectable_value(&mut s.default_view_mode, m, mode_label(m));
                }
            });
        widgets::check_row(
            ui,
            &mut s.auto_resolution,
            "Match stream resolution to the size on screen (recommended)",
            "Each headset streams only as many pixels as its tile shows, up to the quality above: less bandwidth and GPU load with many headsets. When a tile grows or shrinks enough, that stream briefly restarts at the new resolution.",
        );
        widgets::check_row(
            ui,
            &mut s.single_eye_stream,
            "Stream one eye only",
            "The headset crops to the selected eye before encoding: about twice the sharpness at the same bandwidth. \"Both eyes\" view is unavailable while on. Applies to new streams.",
        );

        widgets::heading(ui, "Display");
        ui.horizontal(|ui| {
            ui.label("Interface size");
            let current = SCALES
                .iter()
                .find(|(v, _)| (v - *scale).abs() < 0.01)
                .map(|(_, l)| l.to_string())
                .unwrap_or_else(|| format!("{:.0} %", *scale * 100.0));
            egui::ComboBox::from_id_salt("s-scale")
                .width(200.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (v, l) in SCALES {
                        ui.selectable_value(scale, v, l);
                    }
                });
        });
        widgets::small_muted(
            ui,
            "Scales on top of the display's own scaling. Larger sizes suit small touchscreens; start with --kiosk to run fullscreen without window decorations.",
        );

        widgets::heading(ui, "About");
        widgets::small_muted(
            ui,
            format!(
                "XR MultiCast {} (native) · scrcpy-server {} · Connection: {} · Video decoding: {} · GPU: {}",
                env.version, env.scrcpy_version, env.adb_mode, env.decoder, self.gpu
            ),
        );
        if let Some(p) = &env.store_path {
            widgets::small_muted(
                ui,
                format!(
                    "Settings and verified headsets: {p} (shared with the XR MultiCast desktop app)"
                ),
            );
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Made by Dunk XR (Dunk Technologies). Whitelabel and custom hardware solutions:").small().color(theme::MUTED));
            if ui.link(RichText::new("dunktechnologies.com").small()).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab("https://dunktechnologies.com"));
            }
        });
    }

    fn guide_body(&mut self, ui: &mut egui::Ui, active: &mut String) {
        let ids = ["meta", "pico", "yvr", "htc", "androidxr"];
        ui.horizontal_wrapped(|ui| {
            for id in ids {
                let b = xrmc_core::brands::brand_or_fallback(id);
                let on = active == id;
                let r = Btn::new(&b.name)
                    .kind(if on { Kind::Normal } else { Kind::Ghost })
                    .show(ui);
                if on {
                    let bc = theme::parse_hex(&b.color, theme::ACCENT);
                    ui.painter().rect_filled(
                        egui::Rect::from_min_max(
                            egui::pos2(r.rect.min.x + 6.0, r.rect.max.y - 3.0),
                            egui::pos2(r.rect.max.x - 6.0, r.rect.max.y),
                        ),
                        1.5,
                        bc,
                    );
                }
                if r.clicked() {
                    *active = id.to_string();
                }
            }
        });
        let b = xrmc_core::brands::brand_or_fallback(active);
        let two_col = ui.available_width() > 640.0;
        let steps = |ui: &mut egui::Ui, items: &[String]| {
            ui.spacing_mut().item_spacing.y = 6.0;
            for (i, s) in items.iter().enumerate() {
                widgets::item(
                    ui,
                    &format!("{}.", i + 1),
                    14.5,
                    &[(s.as_str(), Run::Normal)],
                );
            }
        };
        let col1 = |ui: &mut egui::Ui| {
            ui.label(
                RichText::new(format!("1 · Enable developer mode on {}", b.family))
                    .font(theme::font_bold(15.0)),
            );
            steps(ui, &b.setup);
            widgets::small_muted(ui, "Menu names vary slightly between firmware versions.");
        };
        let col2 = |ui: &mut egui::Ui| {
            ui.label(RichText::new("2 · Verify over USB").font(theme::font_bold(15.0)));
            steps(
                ui,
                &[
                    "With the headset plugged in, it appears in the headset list under Connected as Unverified.".into(),
                    "Tap Verify, give it a clear label and colour. Keep Enable Wi-Fi casting ticked.".into(),
                    "When you see Wi-Fi casting enabled, you can unplug the cable.".into(),
                ],
            );
            ui.label(RichText::new("3 · Cast").font(theme::font_bold(15.0)));
            steps(
                ui,
                &[
                    "Tap Cast on a headset or Cast all in the toolbar.".into(),
                    "Streams are lens-corrected by default. Use Calibrate on a stream to fine-tune for a specific headset.".into(),
                ],
            );
        };
        if two_col {
            ui.columns(2, |cols| {
                col1(&mut cols[0]);
                col2(&mut cols[1]);
            });
        } else {
            col1(ui);
            col2(ui);
        }
        ui.label(RichText::new("Troubleshooting").font(theme::font_bold(15.0)));
        let platform = if cfg!(target_os = "linux") {
            "Linux: if a USB headset doesn't appear, install the USB rule (packaging/native/install.sh --udev, or the XR MultiCast .deb) and re-plug it. On a Raspberry Pi hub, use a powered USB hub for several headsets."
        } else if cfg!(target_os = "windows") {
            "Windows: direct USB needs a WinUSB-compatible ADB driver (Meta: Oculus ADB Drivers; others: Google USB Driver or the vendor's driver). Re-plug after installing."
        } else {
            "macOS: no driver needed. Use a USB-C data cable (some charge-only cables won't work)."
        };
        for (t, d) in [
            ("Platform. ", platform),
            (
                "Wi-Fi casting stops after the headset reboots. ",
                "Network debugging is reset on reboot — plug in via USB once and tap Enable Wi-Fi again.",
            ),
            (
                "Scan finds nothing. ",
                "The hub and headsets must be on the same network/subnet; guest networks and some enterprise Wi-Fi block device-to-device traffic (client isolation).",
            ),
            (
                "Another tool uses the headset. ",
                "XR MultiCast talks to headsets directly and also through a running adb server (SideQuest, Android Studio). If a headset stays offline, close the other tool or start with --no-adb-server.",
            ),
            (
                "Black video. ",
                "Wake the headset (press the power button) — the display must be on to be mirrored.",
            ),
        ] {
            widgets::item(ui, "•", 14.0, &[(t, Run::Bold), (d, Run::Muted)]);
        }
    }
}

/// The settings the user changed (so concurrent changes, e.g. `nextIndex` from a
/// verification, are not overwritten with stale values).
pub fn settings_patch(
    orig: &Settings,
    draft: &Settings,
) -> serde_json::Map<String, serde_json::Value> {
    let (a, b) = (orig.to_json(), draft.to_json());
    b.into_iter().filter(|(k, v)| a.get(k) != Some(v)).collect()
}

fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(text)
            .font(theme::font_bold(13.0))
            .color(theme::MUTED),
    );
}

fn swatches(ui: &mut egui::Ui, selected: &mut String) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        for c in HEADSET_COLORS {
            let (r, resp) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::click());
            let col = theme::parse_hex(c, Color32::GRAY);
            let active = selected.eq_ignore_ascii_case(c);
            if active {
                ui.painter()
                    .circle_stroke(r.center(), 18.0, Stroke::new(2.5, Color32::WHITE));
            } else if resp.hovered() {
                ui.painter()
                    .circle_stroke(r.center(), 18.0, Stroke::new(1.5, theme::LINE_HOVER));
            }
            ui.painter().circle_filled(r.center(), 14.0, col);
            resp.widget_info(|| {
                egui::WidgetInfo::selected(egui::Role::RadioButton, true, active, c)
            });
            if resp.on_hover_text(c).clicked() {
                *selected = c.to_string();
            }
        }
    });
}

fn video_block(ui: &mut egui::Ui, s: &mut Settings, kind: TransportKind) {
    let label = if kind == TransportKind::Usb {
        "USB"
    } else {
        "Wi-Fi"
    };
    let (q, codec, custom) = match kind {
        TransportKind::Usb => (&mut s.usb_quality, &mut s.usb_codec, &mut s.usb_custom),
        TransportKind::Wifi => (&mut s.wifi_quality, &mut s.wifi_codec, &mut s.wifi_custom),
    };
    egui::Frame::new()
        .fill(theme::BG_2)
        .stroke(Stroke::new(1.0, theme::LINE))
        .corner_radius(10)
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let half = ((ui.available_width() - 12.0) / 2.0).max(160.0);
            ui.horizontal_wrapped(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(half);
                    field_label(ui, &format!("Quality over {label}"));
                    let presets = quality_presets();
                    let current = presets
                        .iter()
                        .find(|(k, _)| k == q)
                        .map(|(_, p)| p.label.clone())
                        .unwrap_or_else(|| "Custom…".into());
                    egui::ComboBox::from_id_salt(("q", label))
                        .width(half)
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            for (k, p) in &presets {
                                ui.selectable_value(q, *k, &p.label);
                            }
                            ui.selectable_value(q, QualityKey::Custom, "Custom…");
                        });
                });
                ui.vertical(|ui| {
                    ui.set_width(half);
                    field_label(ui, &format!("Codec over {label}"));
                    let text = |c: CodecPreference| match c {
                        CodecPreference::Auto => "Auto (best available)".to_string(),
                        CodecPreference::Fixed(c) => c.label().to_string(),
                    };
                    egui::ComboBox::from_id_salt(("c", label))
                        .width(half)
                        .selected_text(text(*codec))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                codec,
                                CodecPreference::Auto,
                                text(CodecPreference::Auto),
                            );
                            for c in Codec::ALL {
                                ui.selectable_value(codec, CodecPreference::Fixed(c), c.label())
                                    .on_hover_text(c.description());
                            }
                        });
                });
            });
            if *q == QualityKey::Custom {
                custom_row(ui, custom, label);
            }
        });
}

fn custom_row(ui: &mut egui::Ui, c: &mut CustomQuality, label: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.vertical(|ui| {
            field_label(ui, "Max resolution");
            let text = MAX_SIZE_OPTIONS
                .iter()
                .find(|(v, _)| *v == c.max_size)
                .map(|(_, l)| l.to_string())
                .unwrap_or_else(|| format!("{} px", c.max_size));
            egui::ComboBox::from_id_salt(("size", label))
                .width(170.0)
                .selected_text(text)
                .show_ui(ui, |ui| {
                    for (v, l) in MAX_SIZE_OPTIONS {
                        ui.selectable_value(&mut c.max_size, v, l);
                    }
                });
        });
        ui.vertical(|ui| {
            field_label(ui, "Bitrate (Mbps)");
            ui.add_sized(
                [120.0, 44.0],
                egui::DragValue::new(&mut c.bit_rate_mbps)
                    .range(0.5..=200.0)
                    .speed(0.5),
            );
        });
        ui.vertical(|ui| {
            field_label(ui, "Max fps");
            egui::ComboBox::from_id_salt(("fps", label))
                .width(110.0)
                .selected_text(c.max_fps.to_string())
                .show_ui(ui, |ui| {
                    for v in FPS_OPTIONS {
                        ui.selectable_value(&mut c.max_fps, v, v.to_string());
                    }
                });
        });
    });
}

pub fn fix_title(f: &Fix) -> &'static str {
    match f {
        Fix::AdbError(_) => "Headset connection problem",
        Fix::ServerMissing => "scrcpy-server is missing",
        Fix::BatteryLow(_) => "Headset battery is low",
        Fix::NoDecoder(_) => "No video decoder",
        Fix::SoftwareDecoder(_) => "Video is decoded in software",
        Fix::Demo => "Demo mode",
    }
}

fn bullet(ui: &mut egui::Ui, bold: &str, text: &str) {
    widgets::item(ui, "•", 14.5, &[(bold, Run::Bold), (text, Run::Normal)]);
}

fn detail_box(ui: &mut egui::Ui, text: &str) {
    egui::Frame::new()
        .fill(theme::BG)
        .stroke(Stroke::new(1.0, theme::LINE))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(RichText::new(text).monospace().size(12.5))
                    .wrap()
                    .selectable(true),
            );
        });
}

fn fix_body(ui: &mut egui::Ui, f: &Fix) {
    match f {
        Fix::AdbError(detail) => {
            detail_box(ui, detail);
            bullet(
                ui,
                "\"server version doesn't match\" / adb server errors: ",
                "another tool (SideQuest, Meta Quest Developer Hub, Android Studio, scrcpy) runs its own adb server. XR MultiCast can work without it: close the tool, or start XR MultiCast with --no-adb-server.",
            );
            bullet(
                ui,
                "\"no permissions\" / access denied (Linux): ",
                "install the USB rule with packaging/native/install.sh --udev (or the .deb), then re-plug the headset.",
            );
            bullet(
                ui,
                "Windows: ",
                "direct USB needs a WinUSB-compatible ADB driver (Oculus ADB Drivers / Google USB Driver).",
            );
        }
        Fix::ServerMissing => {
            ui.add(egui::Label::new("The small screen-capture program that runs on the headset (scrcpy-server, Apache 2.0) should ship with the app but wasn't found, so streams can't start.").wrap());
            bullet(
                ui,
                "Installed app: ",
                "reinstall XR MultiCast, or put the scrcpy-server file next to the xr-multicast program.",
            );
            bullet(
                ui,
                "Running from source: ",
                "set XRMC_SCRCPY_SERVER=/path/to/scrcpy-server (run npm run setup on the main branch to download it) and restart.",
            );
        }
        Fix::BatteryLow(name) => {
            ui.add(egui::Label::new(format!("{name} is low on battery. Streaming keeps the display and video encoder busy and drains it faster.")).wrap());
            bullet(
                ui,
                "",
                "Plug the headset into a charger or a powered USB port. Casting over USB also charges it.",
            );
            bullet(
                ui,
                "",
                "Lower the quality preset in Settings to reduce the load.",
            );
        }
        Fix::NoDecoder(backend) => {
            ui.add(egui::Label::new("This computer can't decode the headsets' video yet, so streams connect but no picture is shown.").wrap());
            detail_box(ui, &format!("Decoder: {backend}"));
            bullet(
                ui,
                "Linux / Raspberry Pi OS: ",
                "install FFmpeg's libraries (sudo apt install ffmpeg), then restart XR MultiCast. Hardware decoding uses VA-API or V4L2 when available.",
            );
            bullet(
                ui,
                "Windows / macOS: ",
                "the decoder ships with the app; reinstall it.",
            );
        }
        Fix::SoftwareDecoder(backend) => {
            ui.add(egui::Label::new("Streams work, but every frame is decoded on the CPU. With many headsets (or on a small computer like a Raspberry Pi) this can limit the frame rate.").wrap());
            detail_box(ui, &format!("Decoder: {backend}"));
            bullet(
                ui,
                "",
                "Keep \"Match stream resolution to the size on screen\" on (Settings → Video): smaller tiles need far less decoding.",
            );
            bullet(ui, "", "Use H.264 or a lower quality preset over Wi-Fi.");
            bullet(
                ui,
                "Linux: ",
                "install your GPU's VA-API driver (e.g. mesa-va-drivers / intel-media-va-driver) for hardware decoding.",
            );
        }
        Fix::Demo => {
            ui.add(egui::Label::new("XR MultiCast is showing synthetic headsets (started with --demo). Nothing is sent to real devices and no settings are saved. Start it without --demo to use real headsets.").wrap());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_patch_has_only_changes() {
        let orig = Settings::default();
        let mut draft = orig.clone();
        draft.ten_bit = true;
        draft.usb_quality = QualityKey::Custom;
        let p = settings_patch(&orig, &draft);
        assert_eq!(p.len(), 2);
        assert_eq!(p["tenBit"], json!(true));
        assert_eq!(p["usbQuality"], json!("custom"));
        assert!(settings_patch(&orig, &orig).is_empty());
    }
}
