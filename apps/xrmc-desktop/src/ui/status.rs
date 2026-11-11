//! Status bar (green = fine, amber = works with a caveat, red = broken; amber
//! and red items explain the fix when tapped), toasts and the start-up error screen.

use eframe::egui::{self, Align, Align2, Layout, Margin, Rect, RichText, Stroke, pos2, vec2};
use xrmc_core::NoticeLevel;

use super::dialogs::{Dialog, Fix};
use super::icons::{self, Icon};
use super::widgets::{self, Level};
use super::{App, Cmd};
use crate::model::EnvInfo;
use crate::theme;

/// One status bar entry.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusItem {
    pub icon: Icon,
    pub text: String,
    pub level: Level,
    pub fix: Option<Fix>,
}

/// The status items for the environment (pure, unit-tested).
pub fn status_items(
    env: &EnvInfo,
    adb_error: Option<&str>,
    gpu: &str,
    narrow: bool,
) -> Vec<StatusItem> {
    let mut out = Vec::new();
    if env.demo {
        out.push(StatusItem {
            icon: Icon::Info,
            text: "Demo mode".into(),
            level: Level::Warn,
            fix: Some(Fix::Demo),
        });
    }
    out.push(match adb_error {
        Some(e) => StatusItem {
            icon: Icon::Alert,
            text: "Headset connection problem".into(),
            level: Level::Bad,
            fix: Some(Fix::AdbError(e.to_string())),
        },
        None => StatusItem {
            icon: Icon::Check,
            text: if narrow {
                "Headsets: OK".into()
            } else {
                format!("Headsets: {}", env.adb_mode)
            },
            level: Level::Good,
            fix: None,
        },
    });
    out.push(if env.server_available {
        StatusItem {
            icon: Icon::Check,
            text: format!("scrcpy-server {}", env.scrcpy_version),
            level: Level::Good,
            fix: None,
        }
    } else {
        StatusItem {
            icon: Icon::Alert,
            text: "scrcpy-server missing".into(),
            level: Level::Bad,
            fix: Some(Fix::ServerMissing),
        }
    });
    let any = env.decoder_support.values().any(|(a, _)| *a);
    let hw = env.decoder_support.values().any(|(a, h)| *a && *h);
    out.push(if !any {
        StatusItem {
            icon: Icon::Alert,
            text: "No video decoder".into(),
            level: Level::Bad,
            fix: Some(Fix::NoDecoder(env.decoder.clone())),
        }
    } else if !hw {
        StatusItem {
            icon: Icon::Alert,
            text: "Software video decoding".into(),
            level: Level::Warn,
            fix: Some(Fix::SoftwareDecoder(env.decoder.clone())),
        }
    } else {
        StatusItem {
            icon: Icon::Check,
            text: "Hardware video decoding".into(),
            level: Level::Good,
            fix: None,
        }
    });
    if !narrow {
        out.push(StatusItem {
            icon: Icon::Grid,
            text: format!("GPU: {gpu}"),
            level: Level::Neutral,
            fix: None,
        });
    }
    out.push(StatusItem {
        icon: Icon::Wifi,
        text: if env.local_ips.is_empty() {
            "No network".into()
        } else {
            format!("This hub: {}", env.local_ips.join(", "))
        },
        level: Level::Neutral,
        fix: None,
    });
    out
}

impl App {
    pub(super) fn statusbar(&mut self, ui: &mut egui::Ui) {
        let width = ui.ctx().content_rect().width();
        egui::Panel::bottom("status")
            .exact_size(40.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_2)
                    .stroke(Stroke::new(1.0, theme::LINE))
                    .inner_margin(Margin::symmetric(12, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 16.0;
                    let items = status_items(
                        &self.model.env,
                        self.model.adb_error.as_deref(),
                        &self.gpu,
                        width < 1100.0,
                    );
                    for it in items {
                        if ui.available_width() < 120.0 && it.fix.is_none() {
                            continue;
                        }
                        let r = widgets::status_item(ui, it.icon, &it.text, it.level, 36.0);
                        if r.clicked()
                            && let Some(f) = it.fix
                        {
                            self.cmds.push(Cmd::Open(Dialog::Fix(f)));
                        }
                    }
                    if width >= 1500.0 {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new("Tip: double-tap a stream to focus it, press 1–9 to jump to a headset, Esc to return.")
                                        .font(theme::font(12.5))
                                        .color(theme::MUTED),
                                )
                                .truncate(),
                            );
                        });
                    }
                });
            });
    }

    pub(super) fn toasts(&mut self, ctx: &egui::Context) {
        if self.model.toasts.is_empty() {
            return;
        }
        let screen = ctx.content_rect();
        let w = (screen.width() - 24.0).min(440.0);
        let toasts = self.model.toasts.clone();
        egui::Area::new(egui::Id::unique("toasts"))
            .order(egui::Order::Tooltip)
            .anchor(Align2::RIGHT_BOTTOM, vec2(-12.0, -52.0))
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_width(w);
                ui.spacing_mut().item_spacing.y = 8.0;
                for t in toasts {
                    let (col, icon) = match t.level {
                        NoticeLevel::Ok => (theme::OK, Icon::Check),
                        NoticeLevel::Warn => (theme::WARN, Icon::Alert),
                        NoticeLevel::Error => (theme::BAD, Icon::Alert),
                        NoticeLevel::Info => (theme::ACCENT, Icon::Info),
                    };
                    let resp = egui::Frame::new()
                        .fill(theme::PANEL_2)
                        .stroke(Stroke::new(1.0, theme::LINE_2))
                        .corner_radius(10)
                        .inner_margin(Margin {
                            left: 16,
                            right: 12,
                            top: 10,
                            bottom: 10,
                        })
                        .show(ui, |ui| {
                            ui.set_width(w - 28.0);
                            ui.horizontal(|ui| {
                                let (r, _) =
                                    ui.allocate_exact_size(vec2(18.0, 18.0), egui::Sense::hover());
                                icons::paint(ui.painter(), r, icon, col);
                                ui.add(
                                    egui::Label::new(RichText::new(&t.message).color(theme::TEXT))
                                        .wrap(),
                                );
                            });
                        })
                        .response;
                    let r = resp.rect;
                    ui.painter().rect_filled(
                        Rect::from_min_max(r.min, pos2(r.min.x + 4.0, r.max.y)),
                        egui::CornerRadius {
                            nw: 10,
                            sw: 10,
                            ne: 0,
                            se: 0,
                        },
                        col,
                    );
                    if ui
                        .interact(
                            r,
                            egui::Id::unique(("toast", &t.message)),
                            egui::Sense::click(),
                        )
                        .clicked()
                    {
                        self.model.toasts.retain(|x| x.message != t.message);
                    }
                }
            });
    }

    pub(super) fn fatal_screen(&mut self, ui: &mut egui::Ui) {
        let msg = self.model.env.fatal.clone().unwrap_or_default();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.3);
                    let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), egui::Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Alert, theme::BAD);
                    ui.label(
                        RichText::new("XR MultiCast could not start").font(theme::font_bold(22.0)),
                    );
                    ui.add(egui::Label::new(RichText::new(msg).color(theme::MUTED)).wrap());
                    ui.add_space(12.0);
                    if widgets::Btn::new("Quit").show(ui).clicked() {
                        self.cmds.push(Cmd::Quit);
                    }
                });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> EnvInfo {
        EnvInfo {
            version: "0.1.0".into(),
            scrcpy_version: "3.3.1".into(),
            server_available: true,
            adb_mode: "adb server + direct USB/TCP".into(),
            decoder: "FFmpeg".into(),
            decoder_support: [("h264".to_string(), (true, true))].into(),
            local_ips: vec!["192.168.1.10".into()],
            ..Default::default()
        }
    }

    #[test]
    fn healthy_setup_is_green() {
        let items = status_items(&env(), None, "Vulkan · GPU", false);
        assert!(
            items
                .iter()
                .all(|i| matches!(i.level, Level::Good | Level::Neutral))
        );
        assert!(items.iter().any(|i| i.text == "This hub: 192.168.1.10"));
        assert!(items.iter().all(|i| i.fix.is_none()));
    }

    #[test]
    fn problems_are_red_or_amber_and_explain_the_fix() {
        let mut e = env();
        e.server_available = false;
        e.decoder_support = [("h264".to_string(), (true, false))].into();
        let items = status_items(&e, Some("cannot connect to daemon"), "GL", true);
        let red: Vec<_> = items.iter().filter(|i| i.level == Level::Bad).collect();
        assert_eq!(red.len(), 2);
        assert!(red.iter().all(|i| i.fix.is_some()));
        let amber = items.iter().find(|i| i.level == Level::Warn).unwrap();
        assert_eq!(amber.text, "Software video decoding");
        assert!(matches!(amber.fix, Some(Fix::SoftwareDecoder(_))));
        e.decoder_support.clear();
        let items = status_items(&e, None, "GL", true);
        assert!(
            items
                .iter()
                .any(|i| i.text == "No video decoder" && i.level == Level::Bad)
        );
        e.demo = true;
        assert_eq!(status_items(&e, None, "GL", true)[0].fix, Some(Fix::Demo));
    }
}
