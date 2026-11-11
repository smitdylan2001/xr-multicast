//! The headset list: cards with number, name, colour, brand, model, battery,
//! USB / Wi-Fi / verified chips and the main actions. On narrow or portrait
//! screens it opens as a drawer over the dashboard.

use eframe::egui::{
    self, Align, Align2, Color32, Layout, Margin, Rect, RichText, Sense, Stroke, StrokeKind,
    UiBuilder, pos2, vec2,
};
use xrmc_core::TransportPreference;

use super::dialogs::{Dialog, Fix};
use super::icons::{self, Icon};
use super::widgets::{self, Btn, CHIP_OFF, Kind};
use super::{App, Cmd};
use crate::model::{self, BatteryClass, Card, HeadsetCard, Primary};
use crate::theme;

impl App {
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui, drawer: bool) {
        let online = self
            .model
            .headsets
            .iter()
            .filter(|h| !h.pending && h.is_online())
            .count();
        egui::Frame::new()
            .inner_margin(Margin {
                left: 16,
                right: if drawer { 64 } else { 16 },
                top: 14,
                bottom: 6,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("HEADSETS")
                            .font(theme::font_bold(13.0))
                            .color(theme::MUTED),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if online > 0 {
                            ui.label(
                                RichText::new(format!("{online} online"))
                                    .font(theme::font(13.0))
                                    .color(theme::OK),
                            );
                        }
                    });
                });
            });
        if let Some(err) = self.model.adb_error.clone() {
            egui::Frame::new()
                .outer_margin(Margin::symmetric(12, 4))
                .inner_margin(Margin::symmetric(10, 8))
                .fill(Color32::from_rgb(0x3a, 0x15, 0x15))
                .stroke(Stroke::new(1.0, theme::DANGER_LINE))
                .corner_radius(8)
                .show(ui, |ui| {
                    let r = ui.add(
                        egui::Label::new(
                            RichText::new(format!("ADB problem: {err}  — how to fix"))
                                .color(Color32::from_rgb(0xff, 0xc9, 0xc9))
                                .small(),
                        )
                        .wrap()
                        .sense(Sense::click()),
                    );
                    if r.clicked() {
                        self.cmds
                            .push(Cmd::Open(Dialog::Fix(Fix::AdbError(err.clone()))));
                    }
                });
        }
        let foot_h = 68.0;
        let list_h = (ui.available_height() - foot_h).max(80.0);
        egui::ScrollArea::vertical()
            .id_salt("headset-list")
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin {
                        left: 12,
                        right: 12,
                        top: 0,
                        bottom: 12,
                    })
                    .show(ui, |ui| self.headset_list(ui));
            });
        let foot = Rect::from_min_max(
            pos2(ui.max_rect().min.x, ui.max_rect().max.y - foot_h),
            ui.max_rect().max,
        );
        ui.painter().line_segment(
            [foot.left_top(), foot.right_top()],
            Stroke::new(1.0, theme::LINE),
        );
        let btn_rect = foot.shrink(12.0);
        let r = Btn::new("How to connect a headset")
            .with_icon(Icon::Usb)
            .kind(Kind::Ghost)
            .show_at(ui, btn_rect, egui::Id::unique("guide-foot"));
        if r.clicked() {
            self.cmds.push(Cmd::Open(Dialog::Guide {
                brand: "meta".into(),
            }));
        }
    }

    fn headset_list(&mut self, ui: &mut egui::Ui) {
        ui.set_width(ui.available_width());
        if self.model.headsets.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
                icons::paint(
                    ui.painter(),
                    r,
                    Icon::Headset,
                    theme::MUTED.gamma_multiply(0.6),
                );
                ui.label(RichText::new("No headsets yet.").color(theme::MUTED));
                ui.label(RichText::new("Plug one in via USB.").color(theme::MUTED));
            });
            return;
        }
        let now = model::now_ms();
        let sections: Vec<(&str, Vec<Card>)> = model::sidebar_sections(&self.model.headsets)
            .into_iter()
            .map(|(title, list)| {
                (
                    title,
                    list.into_iter()
                        .map(|h| model::card_for(h, self.model.streams.get(&h.id), now))
                        .collect(),
                )
            })
            .collect();
        for (title, cards) in sections {
            ui.add_space(8.0);
            ui.label(
                RichText::new(title.to_uppercase())
                    .font(theme::font_bold(11.5))
                    .color(theme::MUTED),
            );
            ui.add_space(2.0);
            for c in cards {
                match c {
                    Card::Pending {
                        title,
                        subtitle,
                        brand_color,
                        hint,
                        spinner,
                        warn,
                    } => pending_card(ui, &title, &subtitle, &brand_color, &hint, spinner, warn),
                    Card::Headset(c) => self.headset_card(ui, &c),
                }
                ui.add_space(8.0);
            }
        }
    }

    fn headset_card(&mut self, ui: &mut egui::Ui, c: &HeadsetCard) {
        let color = theme::parse_hex(&c.color, theme::ACCENT);
        let brand_color = theme::parse_hex(&c.brand_color, theme::MUTED);
        let id = c.id.clone();
        let resp = egui::Frame::new()
            .fill(theme::PANEL)
            .stroke(Stroke::new(1.0, theme::LINE))
            .corner_radius(theme::RADIUS)
            .inner_margin(Margin {
                left: 16,
                right: 10,
                top: 10,
                bottom: 10,
            })
            .show(ui, |ui| {
                if c.offline {
                    ui.multiply_opacity(0.6);
                }
                ui.set_width(ui.available_width());
                // Row 1: dot, number, name / brand + model, battery, menu.
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let (dot, _) = ui.allocate_exact_size(vec2(14.0, 36.0), Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 8.0, color.gamma_multiply(0.25));
                    ui.painter().circle_filled(dot.center(), 5.0, color);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if c.menu {
                            let more = Btn::icon(Icon::More)
                                .kind(Kind::Ghost)
                                .tooltip("More")
                                .show(ui);
                            egui::Popup::menu(&more)
                                .width(250.0)
                                .show(|ui| self.card_menu(ui, c));
                        }
                        if let Some((b, class)) = &c.battery {
                            let col = match class {
                                BatteryClass::Low => theme::BAD,
                                BatteryClass::Mid => theme::WARN,
                                _ => theme::MUTED,
                            };
                            let icon = if b.charging {
                                Icon::Bolt
                            } else {
                                Icon::Battery(b.level)
                            };
                            let text = format!("{}%", b.level);
                            let w = widgets::text_width(ui, &text, theme::font(13.0)) + 22.0;
                            let sense = if *class == BatteryClass::Low {
                                Sense::click()
                            } else {
                                Sense::hover()
                            };
                            let (r, resp) = ui.allocate_exact_size(vec2(w, 36.0), sense);
                            icons::paint(
                                ui.painter(),
                                Rect::from_center_size(
                                    pos2(r.min.x + 8.0, r.center().y),
                                    vec2(16.0, 16.0),
                                ),
                                icon,
                                col,
                            );
                            ui.painter().text(
                                pos2(r.min.x + 20.0, r.center().y),
                                Align2::LEFT_CENTER,
                                &text,
                                theme::font(13.0),
                                col,
                            );
                            let tip = format!(
                                "Battery {}%{}",
                                b.level,
                                if b.charging { " (charging)" } else { "" }
                            );
                            if *class == BatteryClass::Low {
                                let resp = resp.on_hover_text(format!("{tip}: tap for details"));
                                if resp.clicked() {
                                    self.cmds.push(Cmd::Open(Dialog::Fix(Fix::BatteryLow(
                                        c.name.clone(),
                                    ))));
                                }
                            } else {
                                resp.on_hover_text(tip);
                            }
                        }
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.spacing_mut().interact_size.y = 0.0;
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                if let Some(i) = c.index {
                                    ui.label(
                                        RichText::new(format!("#{i}"))
                                            .font(theme::font_bold(15.0))
                                            .color(color),
                                    );
                                }
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&c.name)
                                            .font(theme::font_bold(15.0))
                                            .color(theme::TEXT),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&c.name);
                            });
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                let label = c.brand_name.to_uppercase();
                                let w = widgets::badge_width(ui, &label, 10.0);
                                let (r, _) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
                                widgets::badge(
                                    ui.painter(),
                                    r.left_center(),
                                    &label,
                                    10.0,
                                    brand_color,
                                    theme::on_color(brand_color),
                                );
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&c.model)
                                            .font(theme::font(13.0))
                                            .color(theme::MUTED),
                                    )
                                    .truncate(),
                                );
                            });
                        });
                    });
                });
                // Row 2: chips.
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                    widgets::chip(
                        ui,
                        Some(Icon::Usb),
                        "USB",
                        if c.usb { theme::CHIP_ON } else { CHIP_OFF },
                        if c.usb {
                            "Connected over USB"
                        } else {
                            "Not connected over USB"
                        },
                    );
                    widgets::chip(
                        ui,
                        Some(Icon::Wifi),
                        c.wifi.as_deref().unwrap_or("Wi-Fi"),
                        if c.wifi.is_some() {
                            theme::CHIP_ON
                        } else {
                            CHIP_OFF
                        },
                        &c.wifi_tooltip,
                    );
                    if c.trusted {
                        widgets::chip(
                            ui,
                            Some(Icon::Shield),
                            "Verified",
                            theme::CHIP_VERIFIED,
                            "Verified over USB",
                        );
                    } else {
                        widgets::chip(
                            ui,
                            Some(Icon::Alert),
                            "Unverified",
                            theme::CHIP_UNVERIFIED,
                            "Not verified yet",
                        );
                    }
                });
                // Row 3: actions.
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                    match &c.primary {
                        Primary::Offline(text) => {
                            ui.add_sized(
                                [0.0, 36.0],
                                egui::Label::new(RichText::new(text).small().color(theme::MUTED)),
                            );
                        }
                        Primary::Verify => {
                            if Btn::new("Verify")
                                .with_icon(Icon::Shield)
                                .primary()
                                .show(ui)
                                .clicked()
                            {
                                self.cmds.push(Cmd::Open(Dialog::verify(&self.model, &id)));
                            }
                        }
                        Primary::ConnectUsbToVerify {
                            can_cast,
                            streaming,
                        } => {
                            ui.add(
                                egui::Label::new(
                                    RichText::new("Connect via USB once to verify")
                                        .small()
                                        .color(theme::MUTED),
                                )
                                .wrap(),
                            );
                            if *streaming {
                                if Btn::new("Stop")
                                    .with_icon(Icon::Stop)
                                    .danger()
                                    .show(ui)
                                    .clicked()
                                {
                                    self.cmds.push(Cmd::Stop(id.clone()));
                                }
                            } else if *can_cast
                                && Btn::new("Cast").with_icon(Icon::Play).show(ui).clicked()
                            {
                                self.cmds.push(Cmd::Cast(id.clone(), None));
                            }
                        }
                        Primary::Cast => {
                            if Btn::new("Cast")
                                .with_icon(Icon::Play)
                                .primary()
                                .min_width(96.0)
                                .show(ui)
                                .clicked()
                            {
                                self.cmds.push(Cmd::Cast(id.clone(), None));
                            }
                        }
                        Primary::Stop => {
                            if Btn::new("Stop")
                                .with_icon(Icon::Stop)
                                .danger()
                                .min_width(96.0)
                                .show(ui)
                                .clicked()
                            {
                                self.cmds.push(Cmd::Stop(id.clone()));
                            }
                        }
                    }
                    if c.enable_wifi
                        && Btn::new("Enable Wi-Fi")
                            .with_icon(Icon::Wifi)
                            .kind(Kind::Ghost)
                            .tooltip("Enable casting over Wi-Fi")
                            .show(ui)
                            .clicked()
                    {
                        self.cmds.push(Cmd::EnableWifi(id.clone()));
                    }
                    if c.live {
                        let w = widgets::badge_width(ui, "LIVE", 11.0);
                        let (r, _) = ui.allocate_exact_size(vec2(w, 36.0), Sense::hover());
                        widgets::badge(
                            ui.painter(),
                            r.left_center(),
                            "LIVE",
                            11.0,
                            theme::BAD,
                            Color32::WHITE,
                        );
                    }
                });
            })
            .response;
        // Colour bar on the left edge, and an outline while streaming.
        let r = resp.rect;
        let bar = Rect::from_min_max(r.min, pos2(r.min.x + 5.0, r.max.y));
        ui.painter().rect_filled(
            bar,
            egui::CornerRadius {
                nw: theme::RADIUS,
                sw: theme::RADIUS,
                ne: 0,
                se: 0,
            },
            if c.offline {
                color.gamma_multiply(0.6)
            } else {
                color
            },
        );
        if c.streaming {
            ui.painter().rect_stroke(
                r,
                theme::RADIUS,
                Stroke::new(1.5, color),
                StrokeKind::Inside,
            );
        }
    }

    fn card_menu(&mut self, ui: &mut egui::Ui, c: &HeadsetCard) {
        let item = |ui: &mut egui::Ui, label: &str, danger: bool| {
            Btn::new(label)
                .kind(if danger {
                    Kind::GhostDanger
                } else {
                    Kind::Ghost
                })
                .min_width(236.0)
                .show(ui)
                .clicked()
        };
        let id = c.id.clone();
        if item(ui, "Rename / colour", false) {
            self.cmds.push(Cmd::Open(Dialog::edit(&self.model, &id)));
            ui.close();
        }
        if item(ui, "Video codecs", false) {
            self.cmds.push(Cmd::Open(Dialog::codecs(Some(id.clone()))));
            ui.close();
        }
        if !c.offline && item(ui, "Calibrate lens correction", false) {
            self.cmds.push(Cmd::Calibrate(id.clone()));
            ui.close();
        }
        if c.usb && c.wifi.is_some() && item(ui, "Cast via USB", false) {
            self.cmds
                .push(Cmd::Cast(id.clone(), Some(TransportPreference::Usb)));
            ui.close();
        }
        if c.wifi.is_some() && item(ui, "Cast via Wi-Fi", false) {
            self.cmds
                .push(Cmd::Cast(id.clone(), Some(TransportPreference::Wifi)));
            ui.close();
        }
        if c.usb && c.wifi.is_none() && item(ui, "Enable Wi-Fi casting", false) {
            self.cmds.push(Cmd::EnableWifi(id.clone()));
            ui.close();
        }
        if c.wifi.is_some() && item(ui, "Disconnect Wi-Fi", false) {
            self.cmds.push(Cmd::DisconnectWifi(id.clone()));
            ui.close();
        }
        if item(ui, "Forget headset", true) {
            self.cmds.push(Cmd::Open(Dialog::Forget { id }));
            ui.close();
        }
    }

    /// The headset list as a drawer over the dashboard (narrow / portrait screens).
    pub(super) fn sidebar_drawer(&mut self, ctx: &egui::Context) {
        let screen = ctx.content_rect();
        let w = (screen.width() * 0.88).min(360.0);
        egui::Area::new(egui::Id::unique("nav-drawer"))
            .order(egui::Order::Foreground)
            .fixed_pos(screen.min)
            .interactable(true)
            .show(ctx, |ui| {
                let backdrop = ui.allocate_rect(screen, Sense::click());
                ui.painter()
                    .rect_filled(screen, 0.0, Color32::from_black_alpha(140));
                if backdrop.clicked() {
                    self.nav_open = false;
                }
                let panel = Rect::from_min_size(screen.min, vec2(w, screen.height()));
                ui.interact(panel, egui::Id::unique("nav-panel"), Sense::click());
                ui.painter().rect_filled(panel, 0.0, theme::BG_2);
                ui.painter().line_segment(
                    [panel.right_top(), panel.right_bottom()],
                    Stroke::new(1.0, theme::LINE_2),
                );
                let close = Rect::from_min_size(
                    pos2(panel.max.x - 52.0, panel.min.y + 6.0),
                    vec2(44.0, 44.0),
                );
                ui.scope_builder(UiBuilder::new().max_rect(panel), |ui| {
                    ui.set_clip_rect(panel);
                    self.sidebar(ui, true);
                });
                if Btn::icon(Icon::X)
                    .kind(Kind::Ghost)
                    .tooltip("Close")
                    .show_at(ui, close, egui::Id::unique("nav-close"))
                    .clicked()
                {
                    self.nav_open = false;
                }
            });
    }
}

fn pending_card(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    brand_color: &str,
    hint: &str,
    spinner: bool,
    warn: bool,
) {
    let color = theme::parse_hex(brand_color, theme::MUTED);
    let resp = egui::Frame::new()
        .fill(theme::PANEL)
        .stroke(Stroke::new(1.0, theme::LINE_2))
        .corner_radius(theme::RADIUS)
        .inner_margin(Margin {
            left: 16,
            right: 10,
            top: 10,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(vec2(14.0, 36.0), Sense::hover());
                ui.painter()
                    .circle_filled(dot.center(), 8.0, color.gamma_multiply(0.25));
                ui.painter().circle_filled(dot.center(), 5.0, color);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.add(
                        egui::Label::new(RichText::new(title).font(theme::font_bold(15.0)))
                            .truncate(),
                    );
                    ui.label(
                        RichText::new(subtitle)
                            .font(theme::font(13.0))
                            .color(theme::MUTED),
                    );
                });
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                if spinner {
                    widgets::spinner(
                        ui.painter(),
                        r.center(),
                        7.0,
                        theme::MUTED,
                        ui.input(|i| i.time),
                    );
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(30));
                } else {
                    icons::paint(
                        ui.painter(),
                        r,
                        Icon::Alert,
                        if warn { theme::HINT_WARN } else { theme::MUTED },
                    );
                }
                ui.add(
                    egui::Label::new(RichText::new(hint).font(theme::font(13.0)).color(if warn {
                        theme::HINT_WARN
                    } else {
                        theme::MUTED
                    }))
                    .wrap(),
                );
            });
        })
        .response;
    let r = resp.rect;
    ui.painter().rect_filled(
        Rect::from_min_max(r.min, pos2(r.min.x + 5.0, r.max.y)),
        egui::CornerRadius {
            nw: theme::RADIUS,
            sw: theme::RADIUS,
            ne: 0,
            se: 0,
        },
        color,
    );
}
