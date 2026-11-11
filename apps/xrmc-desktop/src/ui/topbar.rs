//! The top bar: logo, Cast all / Stop all, Scan network, Add by IP, grid, guide, settings, fullscreen.

use eframe::egui::{self, Align, Layout, Sense, Stroke, vec2};

use super::dialogs::Dialog;
use super::icons::{self, Icon};
use super::widgets::{Btn, Kind};
use super::{App, Cmd};
use crate::theme;

const GRID_OPTIONS: [(&str, &str); 5] = [
    ("auto", "Auto grid"),
    ("1", "1 column"),
    ("2", "2 columns"),
    ("3", "3 columns"),
    ("4", "4 columns"),
];

impl App {
    pub(super) fn topbar(&mut self, ui: &mut egui::Ui) {
        let width = ui.ctx().content_rect().width();
        // Breakpoints: full labels, icon-only secondary buttons, overflow menu.
        let labels = width >= 1280.0;
        let overflow = width < 960.0;
        egui::Panel::top("topbar")
            .exact_size(60.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG_2)
                    .stroke(Stroke::new(1.0, theme::LINE))
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if self.narrow {
                        let r = Btn::icon(Icon::Menu).tooltip("Headsets").show(ui);
                        if r.clicked() {
                            self.cmds.push(Cmd::ToggleNav);
                        }
                    }
                    let (rect, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                    icons::logo(ui.painter(), rect, theme::ACCENT);
                    if width >= 1000.0 || (overflow && width >= 560.0) {
                        ui.label(
                            egui::RichText::new("XR MultiCast")
                                .font(theme::font_bold(17.0))
                                .color(theme::TEXT),
                        );
                    }
                    if self.model.env.demo && width >= 1100.0 {
                        widgets_demo_tag(ui);
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        self.topbar_right(ui, labels, overflow);
                    });
                });
            });
    }

    fn topbar_right(&mut self, ui: &mut egui::Ui, labels: bool, overflow: bool) {
        // Right to left.
        let full_icon = if self.window_full {
            Icon::Shrink
        } else {
            Icon::Expand
        };
        if overflow {
            let more = Btn::icon(Icon::More).tooltip("More").show(ui);
            egui::Popup::menu(&more)
                .width(240.0)
                .show(|ui| self.topbar_menu(ui));
        } else {
            if Btn::icon(full_icon)
                .tooltip(if self.window_full {
                    "Leave fullscreen"
                } else {
                    "Fullscreen"
                })
                .show(ui)
                .clicked()
            {
                self.cmds.push(Cmd::ToggleFullscreen);
            }
            if Btn::icon(Icon::Gear).tooltip("Settings").show(ui).clicked() {
                self.cmds
                    .push(Cmd::Open(Dialog::settings(&self.model, self.ui_scale)));
            }
            if Btn::icon(Icon::Help)
                .tooltip("Connection guide")
                .show(ui)
                .clicked()
            {
                self.cmds.push(Cmd::Open(Dialog::Guide {
                    brand: "meta".into(),
                }));
            }
            self.grid_combo(ui, labels);
            divider(ui);
            let add = if labels {
                Btn::new("Add by IP")
            } else {
                Btn::new("")
            };
            if add
                .with_icon(Icon::Plus)
                .tooltip("Connect to a headset by IP address")
                .show(ui)
                .clicked()
            {
                self.cmds.push(Cmd::Open(Dialog::add_ip()));
            }
            let scan = if labels {
                Btn::new("Scan network")
            } else {
                Btn::new("")
            };
            if scan
                .with_icon(Icon::Radar)
                .busy(self.scan_busy)
                .tooltip("Search the local network for headsets")
                .show(ui)
                .clicked()
            {
                self.cmds.push(Cmd::Scan);
            }
            divider(ui);
        }
        let compact = ui.ctx().content_rect().width() < 560.0;
        let stop = if compact {
            Btn::new("")
        } else {
            Btn::new("Stop all")
        };
        if stop
            .with_icon(Icon::Stop)
            .tooltip("Stop every stream")
            .show(ui)
            .clicked()
        {
            self.cmds.push(Cmd::StopAll);
        }
        let cast = if compact {
            Btn::new("")
        } else {
            Btn::new("Cast all")
        };
        if cast
            .with_icon(Icon::Play)
            .kind(Kind::Primary)
            .tooltip("Start casting every connected, verified headset")
            .show(ui)
            .clicked()
        {
            self.cmds.push(Cmd::CastAll);
        }
    }

    fn topbar_menu(&mut self, ui: &mut egui::Ui) {
        let item = |ui: &mut egui::Ui, icon: Icon, label: &str| {
            Btn::new(label)
                .with_icon(icon)
                .kind(Kind::Ghost)
                .min_width(220.0)
                .show(ui)
                .clicked()
        };
        if item(ui, Icon::Radar, "Scan network") {
            self.cmds.push(Cmd::Scan);
            ui.close();
        }
        if item(ui, Icon::Plus, "Add by IP") {
            self.cmds.push(Cmd::Open(Dialog::add_ip()));
            ui.close();
        }
        ui.separator();
        for (v, label) in GRID_OPTIONS {
            let sel = self.model.settings.grid_columns == v;
            if ui
                .add_sized([220.0, 40.0], egui::Button::selectable(sel, label))
                .clicked()
            {
                self.cmds.push(Cmd::SetGridColumns(v.into()));
                ui.close();
            }
        }
        ui.separator();
        if item(ui, Icon::Help, "Connection guide") {
            self.cmds.push(Cmd::Open(Dialog::Guide {
                brand: "meta".into(),
            }));
            ui.close();
        }
        if item(ui, Icon::Gear, "Settings") {
            self.cmds
                .push(Cmd::Open(Dialog::settings(&self.model, self.ui_scale)));
            ui.close();
        }
        let label = if self.window_full {
            "Leave fullscreen"
        } else {
            "Fullscreen"
        };
        if item(ui, Icon::Expand, label) {
            self.cmds.push(Cmd::ToggleFullscreen);
            ui.close();
        }
    }

    fn grid_combo(&mut self, ui: &mut egui::Ui, labels: bool) {
        let current = GRID_OPTIONS
            .iter()
            .find(|(v, _)| *v == self.model.settings.grid_columns)
            .map(|(_, l)| *l)
            .unwrap_or("Auto grid");
        let text = if labels {
            current.to_string()
        } else {
            current.split(' ').next().unwrap_or("").to_string()
        };
        let resp = Btn::new(&text)
            .with_icon(Icon::Grid)
            .tooltip("Grid layout")
            .show(ui);
        egui::Popup::menu(&resp).width(200.0).show(|ui| {
            for (v, label) in GRID_OPTIONS {
                let sel = self.model.settings.grid_columns == v;
                if ui
                    .add_sized([190.0, 40.0], egui::Button::selectable(sel, label))
                    .clicked()
                {
                    self.cmds.push(Cmd::SetGridColumns(v.into()));
                    ui.close();
                }
            }
        });
    }
}

fn divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, 26.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::LINE);
}

fn widgets_demo_tag(ui: &mut egui::Ui) {
    let p = ui.painter().clone();
    let w = super::widgets::badge_width(ui, "DEMO", 11.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::hover());
    super::widgets::badge(
        &p,
        rect.left_center(),
        "DEMO",
        11.0,
        theme::mix(theme::WARN, theme::BG_2, 0.25),
        theme::WARN,
    );
    resp.on_hover_text("Demo mode: synthetic headsets, nothing is sent to real devices.");
}
