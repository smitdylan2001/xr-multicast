//! The lens calibration drawer (right side; a bottom sheet on portrait screens).

use eframe::egui::{self, Align, Layout, Margin, RichText, Stroke, vec2};
use xrmc_core::brands::{Correction, Eye};
use xrmc_core::layout::{LayoutSetting, Rotation};

use super::icons::Icon;
use super::widgets::{self, Btn, Kind};
use super::{App, Cmd};
use crate::model::{self, CALIB_SLIDERS};
use crate::theme;

const LAYOUTS: [LayoutSetting; 5] = [
    LayoutSetting::Auto,
    LayoutSetting::Sbs,
    LayoutSetting::Tb,
    LayoutSetting::Quad,
    LayoutSetting::Mono,
];

impl App {
    pub(super) fn calibration_panel(&mut self, ui: &mut egui::Ui) {
        let frame = egui::Frame::new()
            .fill(theme::BG_2)
            .stroke(Stroke::new(1.0, theme::LINE));
        if self.narrow {
            let h = (ui.ctx().content_rect().height() * 0.45).max(260.0);
            egui::Panel::bottom("calibration")
                .exact_size(h)
                .frame(frame)
                .show(ui, |ui| self.calibration(ui));
        } else {
            egui::Panel::right("calibration")
                .exact_size(370.0)
                .frame(frame)
                .show(ui, |ui| self.calibration(ui));
        }
    }

    fn calibration(&mut self, ui: &mut egui::Ui) {
        let Some(calib) = self.model.calib.clone() else {
            return;
        };
        let h = self.model.headset(&calib.id).cloned();
        let brand = model::brand_of(h.as_ref());
        let color = theme::parse_hex(
            h.as_ref().map(|h| h.color.as_str()).unwrap_or(&brand.color),
            theme::ACCENT,
        );
        let trusted = h.as_ref().is_some_and(|h| h.trusted);
        // What "auto" currently resolves to.
        let auto = self.model.tile(&calib.id).filter(|t| t.has_frame).map(|t| {
            let single = self
                .model
                .streams
                .get(&calib.id)
                .is_some_and(|s| s.single_eye);
            let mut p = calib.params.clone();
            p.layout = LayoutSetting::Auto;
            let lay = model::resolve_view(&p, t, single);
            let mut p = calib.params.clone();
            p.rotation = Rotation::Auto;
            let rot = model::resolve_view(&p, t, single);
            (lay.layout, rot.rotation)
        });

        // Header with the headset colour on top.
        let top = ui.max_rect();
        ui.painter().rect_filled(
            egui::Rect::from_min_size(top.min, vec2(top.width(), 4.0)),
            0.0,
            color,
        );
        egui::Frame::new()
            .inner_margin(Margin {
                left: 16,
                right: 8,
                top: 10,
                bottom: 6,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(
                            RichText::new("Lens calibration")
                                .small()
                                .color(theme::MUTED),
                        );
                        ui.label(
                            RichText::new(model::label_of(h.as_ref())).font(theme::font_bold(16.0)),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if Btn::icon(Icon::X)
                            .kind(Kind::Ghost)
                            .tooltip("Close")
                            .show(ui)
                            .clicked()
                        {
                            self.cmds.push(Cmd::CloseCalibration { save: false });
                        }
                    });
                });
            });
        let foot_h = 64.0;
        let body_h = (ui.available_height() - foot_h).max(60.0);
        let mut p = calib.params.clone();
        let mut grid = calib.grid;
        egui::ScrollArea::vertical()
            .max_height(body_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 6))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 8.0;
                        widgets::small_muted(
                            ui,
                            format!(
                                "Look at something with straight lines in the headset (a door frame, a window). Adjust until those lines are straight in the preview. Starting values come from the {} profile.",
                                brand.family
                            ),
                        );
                        ui.checkbox(&mut grid, "Show reference grid");
                        self.calibration_fields(ui, &mut p, auto);
                    });
            });
        if let Some(c) = &mut self.model.calib {
            // Zoom, framing, layout and rotation change how many source pixels the tile needs.
            let resize = c.params.zoom != p.zoom
                || c.params.output_aspect != p.output_aspect
                || c.params.layout != p.layout
                || c.params.rotation != p.rotation
                || c.params.quad_row != p.quad_row;
            if resize {
                self.view_delay = std::time::Duration::from_millis(600);
            }
            c.params = p;
            c.grid = grid;
        }
        // Footer.
        let r = ui.max_rect();
        let foot = egui::Rect::from_min_max(egui::pos2(r.min.x, r.max.y - foot_h), r.max);
        ui.painter().line_segment(
            [foot.left_top(), foot.right_top()],
            Stroke::new(1.0, theme::LINE),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(foot.shrink2(vec2(12.0, 10.0)))
                .layout(Layout::right_to_left(Align::Center)),
        );
        let save = Btn::new("Save")
            .primary()
            .enabled(trusted)
            .tooltip(if trusted {
                ""
            } else {
                "Verify the headset to save calibration"
            })
            .show(&mut child);
        if save.clicked() {
            self.cmds.push(Cmd::CloseCalibration { save: true });
        }
        if Btn::new("Cancel").show(&mut child).clicked() {
            self.cmds.push(Cmd::CloseCalibration { save: false });
        }
        child.with_layout(Layout::left_to_right(Align::Center), |ui| {
            if Btn::new("Reset to preset")
                .kind(Kind::Ghost)
                .tooltip("Back to the brand's defaults")
                .show(ui)
                .clicked()
            {
                self.cmds.push(Cmd::ResetCalibration);
            }
        });
    }

    fn calibration_fields(
        &mut self,
        ui: &mut egui::Ui,
        p: &mut Correction,
        auto: Option<(xrmc_core::layout::Layout, u32)>,
    ) {
        let w = ui.available_width();
        let half = ((w - 8.0) / 2.0).max(120.0);
        let layout_label = |l: LayoutSetting| match (l, auto) {
            (LayoutSetting::Auto, Some((lay, _))) => {
                format!("Auto ({})", model::layout_label(lay.into()))
            }
            (l, _) => model::layout_label(l).to_string(),
        };
        let rotation_label = |r: Rotation| match (r, auto) {
            (Rotation::Auto, Some((_, deg))) => format!("Auto ({deg}°)"),
            (Rotation::Auto, None) => "Auto".into(),
            (Rotation::Degrees(d), _) => format!("{d:.0}°"),
        };
        let field = |ui: &mut egui::Ui, label: &str| {
            ui.label(
                RichText::new(label)
                    .font(theme::font_bold(13.0))
                    .color(theme::MUTED),
            );
        };
        field(ui, "Panel layout");
        egui::ComboBox::from_id_salt("c-layout")
            .width(w)
            .selected_text(layout_label(p.layout))
            .show_ui(ui, |ui| {
                for l in LAYOUTS {
                    ui.selectable_value(&mut p.layout, l, layout_label(l));
                }
            });
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(half);
                field(ui, "Eye");
                let opts = [
                    (Eye::Left, "Left", "Left eye"),
                    (Eye::Right, "Right", "Right eye"),
                ];
                if let Some(e) =
                    widgets::segmented(ui, egui::Id::unique("c-eye"), &opts, p.eye, 44.0)
                {
                    p.eye = e;
                }
            });
            ui.vertical(|ui| {
                ui.set_width(half);
                field(ui, "Rotation");
                egui::ComboBox::from_id_salt("c-rotation")
                    .width(half)
                    .selected_text(rotation_label(p.rotation))
                    .show_ui(ui, |ui| {
                        for r in [
                            Rotation::Auto,
                            Rotation::Degrees(0.0),
                            Rotation::Degrees(90.0),
                            Rotation::Degrees(180.0),
                            Rotation::Degrees(270.0),
                        ] {
                            ui.selectable_value(&mut p.rotation, r, rotation_label(r));
                        }
                    });
            });
        });
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(half);
                field(ui, "Output framing");
                let opts = [
                    ("16:9", "16:9"),
                    ("4:3", "4:3"),
                    ("1:1", "Square"),
                    ("eye", "Full eye"),
                ];
                let current = opts
                    .iter()
                    .find(|(v, _)| *v == p.output_aspect)
                    .map(|(_, l)| l.to_string())
                    .unwrap_or_else(|| p.output_aspect.clone());
                egui::ComboBox::from_id_salt("c-aspect")
                    .width(half)
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        for (v, l) in opts {
                            ui.selectable_value(&mut p.output_aspect, v.to_string(), l);
                        }
                    });
            });
            ui.vertical(|ui| {
                ui.set_width(half);
                field(ui, "Eye row (2×2 grid)");
                let rows = [
                    ("top", "Top", "Top pair"),
                    ("bottom", "Bottom", "Bottom pair"),
                ];
                if let Some(r) = widgets::segmented(
                    ui,
                    egui::Id::unique("c-row"),
                    &rows,
                    p.quad_row.as_str(),
                    44.0,
                ) {
                    p.quad_row = r.to_string();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Orientation")
                    .font(theme::font_bold(13.0))
                    .color(theme::MUTED),
            );
            ui.label(
                RichText::new("EXPERIMENTAL")
                    .font(theme::font_bold(10.0))
                    .color(theme::WARN),
            );
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut p.flip_h, "Flip horizontally");
            ui.checkbox(&mut p.flip_v, "Flip vertically");
        });
        for s in &CALIB_SLIDERS {
            let decimals = if s.step < 0.001 { 4 } else { 3 };
            let Some(v) = model::slider_value(p, s.key) else {
                continue;
            };
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(s.label).font(theme::font_bold(14.0)));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{:.*}", decimals, *v))
                            .monospace()
                            .color(theme::MUTED),
                    );
                });
            });
            ui.spacing_mut().slider_width = ui.available_width();
            ui.add(
                egui::Slider::new(v, s.min..=s.max)
                    .step_by(s.step)
                    .show_value(false),
            );
            if !s.hint.is_empty() {
                widgets::small_muted(ui, s.hint);
            }
        }
    }
}
