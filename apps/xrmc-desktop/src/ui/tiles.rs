//! The stream grid: one tile per stream with a clear label (number, name,
//! colour, brand, model, connection, LIVE), the lens-corrected video, its
//! status, stats and controls. Also the empty state.

use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Layout, Margin, Pos2, Rect, RichText, Sense,
    Stroke, StrokeKind, UiBuilder, pos2, vec2,
};
use xrmc_core::brands::Eye;
use xrmc_core::view_size::ViewMode;
use xrmc_core::{HeadsetSnapshot, StreamState, StreamStatus, TransportKind};

use super::dialogs::Dialog;
use super::icons::{self, Icon};
use super::widgets::{self, Btn, Kind};
use super::{App, Cmd};
use crate::model::{self, Resolved, Tile, TileStatus, VIEW_MODES};
use crate::theme;

const GAP: f32 = 12.0;
const HEAD_H: f32 = 58.0;
const FOOT_H: f32 = 58.0;

/// Everything needed to draw one tile (copied out of the model).
struct TileView {
    id: String,
    headset: Option<HeadsetSnapshot>,
    state: Option<StreamState>,
    tile: Tile,
    resolved: Resolved,
    status: Option<TileStatus>,
    stats: String,
    stats_tip: String,
}

impl App {
    pub(super) fn stage(&mut self, ui: &mut egui::Ui) {
        let full = self.app_full.is_some();
        if self.model.tiles.is_empty() {
            self.video_rects.clear();
            self.empty_state(ui);
            return;
        }
        let ids: Vec<String> = self
            .model
            .visible_tiles()
            .iter()
            .map(|t| t.id.clone())
            .collect();
        let views: Vec<TileView> = ids
            .iter()
            .filter_map(|id| {
                let t = self.model.tile(id)?;
                let inputs = self.model.inputs(t);
                let resolved = model::tile_params(&inputs);
                let mut stats_tip = model::stats_tooltip(inputs.state, &resolved.view);
                if let Some(out) = self.bridge.output(id) {
                    let backend = out.backend();
                    if !backend.is_empty() && !stats_tip.is_empty() {
                        let hw = out.hardware.load(std::sync::atomic::Ordering::Relaxed);
                        stats_tip.push_str(&format!(
                            "\nDecoder: {backend} ({})\nTap for the video codecs: headset encoders vs. this receiver's decoders",
                            if hw { "hardware" } else { "software" }
                        ));
                    }
                }
                Some(TileView {
                    id: id.clone(),
                    headset: inputs.headset.cloned(),
                    state: inputs.state.cloned(),
                    status: model::tile_status(inputs.state, t),
                    stats: model::stats_line(inputs.state, t, &resolved.view),
                    stats_tip,
                    tile: t.clone(),
                    resolved,
                })
            })
            .collect();
        self.video_rects.clear();
        let pad = if full { 0.0 } else { GAP };
        let area = ui.available_rect_before_wrap().shrink(pad);
        let n = views.len();
        let cols = model::grid_columns(
            &self.model.settings.grid_columns,
            n,
            self.model.focus.is_some(),
            (area.width(), area.height()),
            HEAD_H + FOOT_H,
        );
        let (rects, content_h) = model::grid_rects(area.width(), area.height(), n, cols, GAP);
        if content_h > area.height() + 1.0 {
            let mut child = ui.new_child(UiBuilder::new().max_rect(area));
            egui::ScrollArea::vertical()
                .id_salt("grid")
                .auto_shrink([false, false])
                .show(&mut child, |ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(vec2(area.width(), content_h), Sense::hover());
                    for (v, r) in views.into_iter().zip(&rects) {
                        let tr = Rect::from_min_size(rect.min + vec2(r[0], r[1]), vec2(r[2], r[3]));
                        self.draw_tile(ui, tr, v);
                    }
                });
        } else {
            for (v, r) in views.into_iter().zip(&rects) {
                let tr = Rect::from_min_size(area.min + vec2(r[0], r[1]), vec2(r[2], r[3]));
                self.draw_tile(ui, tr, v);
            }
        }
    }

    fn draw_tile(&mut self, ui: &mut egui::Ui, rect: Rect, v: TileView) {
        let full = self.app_full.is_some();
        let h = v.headset.as_ref();
        let brand = model::brand_of(h);
        let color = theme::parse_hex(
            h.map(|h| h.color.as_str()).unwrap_or(brand.color.as_str()),
            theme::ACCENT,
        );
        let live = v
            .state
            .as_ref()
            .is_some_and(|s| s.state == StreamStatus::Live)
            && v.tile.has_video;
        let errored = v
            .state
            .as_ref()
            .is_some_and(|s| s.state == StreamStatus::Error);
        let compact = !full && model::is_compact_tile(rect.width(), rect.height());
        let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        let id = egui::Id::unique(("tile", &v.id));

        // Frame.
        if !full {
            painter.rect_filled(rect, 12.0, theme::PANEL);
        }
        let body = if full {
            rect
        } else if compact {
            rect.shrink(2.0)
        } else {
            Rect::from_min_max(
                pos2(rect.min.x + 2.0, rect.min.y + HEAD_H),
                pos2(rect.max.x - 2.0, rect.max.y - FOOT_H),
            )
        };

        if !full && !compact {
            self.tile_header(ui, &painter, rect, &v, color, live);
        }

        // Video.
        let body_painter = painter.with_clip_rect(body.intersect(ui.clip_rect()));
        body_painter.rect_filled(body, if compact { 10.0 } else { 0.0 }, Color32::BLACK);
        if v.tile.has_frame {
            if let Some(video) = &self.video {
                video.paint(&body_painter, body, &v.id, &v.resolved.params);
            }
        } else if self.video.is_none() {
            // Shown through the status below.
        }
        self.video_rects.insert(v.id.clone(), body);
        let body_resp = ui.interact(body, id.with("body"), Sense::click());
        if body_resp.double_clicked() {
            if full {
                self.cmds.push(Cmd::TileFullscreen(v.id.clone()));
            } else {
                self.cmds.push(Cmd::ToggleFocus(v.id.clone()));
            }
        }

        // Label over the video: always visible, so every picture is identifiable.
        let label = model::label_of(h);
        let sub = model::brand_model(&brand.name, h.map(|h| h.model.as_str()).unwrap_or(""));
        // Tiny tiles show LIVE as a red dot to leave room for the name.
        let live_dot = compact && body.width() < 240.0;
        let live_w = match (compact && live, live_dot) {
            (false, _) => 0.0,
            (true, true) => 22.0,
            (true, false) => widgets::badge_width(ui, "LIVE", 11.0) + 12.0,
        };
        video_label(
            &body_painter,
            body,
            &label,
            &sub,
            color,
            full || self.model.focus.is_some(),
            live_w,
        );
        if compact && live {
            if live_dot {
                body_painter.circle_filled(
                    pos2(body.max.x - 16.0, body.min.y + 22.0),
                    6.0,
                    theme::BAD,
                );
            } else {
                widgets::badge(
                    &body_painter,
                    pos2(body.max.x - live_w, body.min.y + 22.0),
                    "LIVE",
                    11.0,
                    theme::BAD,
                    Color32::WHITE,
                );
            }
        }

        // Status (starting, reconnecting, changing resolution, errors).
        let status = if self.video.is_none() {
            Some(TileStatus {
                title: "Cannot display video".into(),
                detail: "The GPU renderer is not available.".into(),
                spinner: false,
                actions: false,
                resizing: false,
            })
        } else {
            v.status.clone()
        };
        if let Some(st) = &status {
            self.tile_status(ui, &body_painter, body, &v.id, st);
        }

        // Compact tiles: tools and stop at the bottom right of the video.
        let tools_w = if compact { 2.0 * 44.0 + 6.0 + 8.0 } else { 0.0 };
        if compact {
            let y = body.max.y - 52.0;
            let stop = Rect::from_min_size(pos2(body.max.x - 52.0, y), vec2(44.0, 44.0));
            let more = Rect::from_min_size(pos2(body.max.x - 102.0, y), vec2(44.0, 44.0));
            body_painter.circle_filled(more.center(), 20.0, Color32::from_black_alpha(170));
            body_painter.circle_filled(stop.center(), 20.0, Color32::from_black_alpha(170));
            body_painter.circle_stroke(stop.center(), 20.0, Stroke::new(1.0, theme::DANGER_LINE));
            if Btn::icon(Icon::Stop)
                .kind(Kind::GhostDanger)
                .tooltip("Stop casting")
                .show_at(ui, stop, egui::Id::unique(("tile-stop", &v.id)))
                .clicked()
            {
                self.cmds.push(Cmd::Stop(v.id.clone()));
            }
            let resp = Btn::icon(Icon::More)
                .kind(Kind::Ghost)
                .tooltip("View and tools")
                .show_at(ui, more, egui::Id::unique(("tile-more", &v.id)));
            egui::Popup::menu(&resp)
                .width(260.0)
                .show(|ui| self.tile_menu(ui, &v, true));
        }

        // Stats overlay (compact tiles, fullscreen, or when the footer is too narrow).
        let footer_fits_stats = !compact && !full && rect.width() >= 860.0;
        if !v.stats.is_empty()
            && !footer_fits_stats
            && status.is_none()
            && body.width() - tools_w > 90.0
            && (!compact || body.width() >= 300.0)
        {
            let font = theme::font(12.0);
            let text = widgets::fit_text(
                &body_painter,
                &v.stats,
                font.clone(),
                body.width() - 28.0 - tools_w,
            );
            let galley =
                body_painter.layout_no_wrap(text, font, Color32::from_rgb(0xc9, 0xd2, 0xe6));
            let r = Rect::from_min_size(
                pos2(body.min.x + 8.0, body.max.y - galley.size().y - 14.0),
                galley.size() + vec2(12.0, 6.0),
            );
            body_painter.rect_filled(r, 6.0, Color32::from_black_alpha(150));
            body_painter.galley(r.min + vec2(6.0, 3.0), galley, Color32::WHITE);
            let tip_resp = ui.interact(r, id.with("stats"), Sense::click());
            let tip_resp = if v.stats_tip.is_empty() {
                tip_resp
            } else {
                tip_resp.on_hover_text(&v.stats_tip)
            };
            if tip_resp.clicked() {
                self.cmds
                    .push(Cmd::Open(Dialog::codecs(Some(v.id.clone()))));
            }
        }

        if full {
            let r =
                Rect::from_min_size(pos2(body.max.x - 56.0, body.min.y + 12.0), vec2(44.0, 44.0));
            painter.rect_filled(r, 8.0, Color32::from_black_alpha(140));
            if Btn::icon(Icon::Shrink)
                .kind(Kind::Ghost)
                .tooltip("Leave fullscreen (Esc)")
                .show_at(ui, r, id.with("exit-full"))
                .clicked()
            {
                self.cmds.push(Cmd::TileFullscreen(v.id.clone()));
            }
        } else if !compact {
            let foot = Rect::from_min_max(pos2(rect.min.x, rect.max.y - FOOT_H), rect.max);
            self.tile_footer(ui, &painter, foot, &v, footer_fits_stats);
        }

        // Border on top of everything.
        if !full {
            let border = if live {
                color
            } else if errored {
                theme::DANGER_LINE
            } else {
                theme::mix(color, theme::LINE, 0.55)
            };
            painter.rect_stroke(rect, 12.0, Stroke::new(2.0, border), StrokeKind::Inside);
        }
    }

    fn tile_header(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        rect: Rect,
        v: &TileView,
        color: Color32,
        live: bool,
    ) {
        let h = v.headset.as_ref();
        let brand = model::brand_of(h);
        let head = Rect::from_min_size(rect.min, vec2(rect.width(), HEAD_H));
        let tint = theme::mix(color, theme::PANEL, 0.28);
        painter.rect_filled(
            Rect::from_min_size(head.min, vec2(14.0, head.height())),
            CornerRadius {
                nw: 12,
                sw: 0,
                ne: 0,
                se: 0,
            },
            tint,
        );
        widgets::gradient_rect(
            painter,
            Rect::from_min_max(
                pos2(head.min.x + 12.0, head.min.y + 1.0),
                pos2(head.min.x + head.width() * 0.7, head.max.y),
            ),
            tint,
            theme::PANEL,
        );
        painter.line_segment(
            [head.left_bottom(), head.right_bottom()],
            Stroke::new(1.0, theme::LINE),
        );

        // Number badge.
        let idx = Rect::from_min_size(
            pos2(head.min.x + 12.0, head.center().y - 19.0),
            vec2(38.0, 38.0),
        );
        let num = h.and_then(|h| h.index).map(|i| format!("#{i}"));
        let num_w = num.as_ref().map_or(0.0, |n| {
            widgets::text_width(ui, n, theme::font_bold(16.0)) + 14.0
        });
        let idx = Rect::from_min_size(idx.min, vec2(num_w.max(38.0), 38.0));
        painter.rect_filled(idx, 8.0, color);
        match &num {
            Some(n) => {
                painter.text(
                    idx.center(),
                    Align2::CENTER_CENTER,
                    n,
                    theme::font_bold(16.0),
                    theme::on_color(color),
                );
            }
            // Not verified yet, so no number.
            None => icons::paint(
                painter,
                idx.shrink(7.0),
                Icon::Headset,
                theme::on_color(color),
            ),
        }

        // Right side: LIVE, connection, brand.
        let mut x = head.max.x - 10.0;
        let cy = head.center().y;
        if live {
            let w = widgets::badge_width(ui, "LIVE", 11.0);
            widgets::badge(
                painter,
                pos2(x - w, cy),
                "LIVE",
                11.0,
                theme::BAD,
                Color32::WHITE,
            );
            x -= w + 10.0;
        }
        let narrow_head = rect.width() < 520.0;
        if let Some(t) = v.state.as_ref().and_then(|s| s.transport) {
            let text = match t {
                TransportKind::Usb => "USB".to_string(),
                TransportKind::Wifi if narrow_head => "Wi-Fi".to_string(),
                TransportKind::Wifi => match h.and_then(|h| h.ip.as_ref()) {
                    Some(ip) => format!("Wi-Fi {ip}"),
                    None => "Wi-Fi".into(),
                },
            };
            let w = widgets::text_width(ui, &text, theme::font(13.0));
            painter.text(
                pos2(x - w, cy),
                Align2::LEFT_CENTER,
                &text,
                theme::font(13.0),
                theme::MUTED,
            );
            let icon = if t == TransportKind::Usb {
                Icon::Usb
            } else {
                Icon::Wifi
            };
            icons::paint(
                painter,
                Rect::from_center_size(pos2(x - w - 11.0, cy), vec2(15.0, 15.0)),
                icon,
                theme::MUTED,
            );
            x -= w + 30.0;
        }
        if !narrow_head {
            let label = brand.name.to_uppercase();
            let w = widgets::badge_width(ui, &label, 11.0);
            if x - w > idx.max.x + 120.0 {
                let bc = theme::parse_hex(&brand.color, theme::MUTED);
                widgets::badge(
                    painter,
                    pos2(x - w, cy),
                    &label,
                    11.0,
                    bc,
                    theme::on_color(bc),
                );
                x -= w + 12.0;
            }
        }

        // Name and details.
        let tx = idx.max.x + 12.0;
        let max_w = (x - tx).max(20.0);
        let name = h.map(|h| h.name.clone()).unwrap_or_else(|| v.id.clone());
        let name = widgets::fit_text(painter, &name, theme::font_bold(17.0), max_w);
        painter.text(
            pos2(tx, cy - 9.0),
            Align2::LEFT_CENTER,
            name,
            theme::font_bold(17.0),
            theme::TEXT,
        );
        let sub = match h {
            Some(h) => {
                let mut s = format!("{} · {}", brand.family, h.model);
                if let Some(b) = &h.battery {
                    s.push_str(&format!(
                        " · {}%{}",
                        b.level,
                        if b.charging { " charging" } else { "" }
                    ));
                }
                s
            }
            None => String::new(),
        };
        let sub = widgets::fit_text(painter, &sub, theme::font(12.5), max_w);
        painter.text(
            pos2(tx, cy + 11.0),
            Align2::LEFT_CENTER,
            sub,
            theme::font(12.5),
            theme::MUTED,
        );
    }

    fn tile_status(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        body: Rect,
        id: &str,
        st: &TileStatus,
    ) {
        if !st.resizing {
            painter.rect_filled(body, 0.0, Color32::from_black_alpha(205));
        }
        let wrap = (body.width() - 40.0).clamp(80.0, 520.0);
        let title = painter.layout(st.title.clone(), theme::font_bold(16.0), theme::TEXT, wrap);
        let detail = (!st.detail.is_empty())
            .then(|| painter.layout(st.detail.clone(), theme::font(13.0), theme::MUTED, wrap));
        let spin = if st.spinner { 34.0 } else { 0.0 };
        let actions_h = if st.actions { 56.0 } else { 0.0 };
        let total =
            spin + title.size().y + detail.as_ref().map_or(0.0, |d| d.size().y + 6.0) + actions_h;
        let mut y = body.center().y - total / 2.0;
        if st.spinner {
            widgets::spinner(
                painter,
                pos2(body.center().x, y + 13.0),
                12.0,
                theme::TEXT,
                ui.input(|i| i.time),
            );
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(33));
            y += spin;
        }
        let shadow = |p: Pos2, g: &std::sync::Arc<egui::Galley>| {
            if st.resizing {
                for d in [
                    vec2(0.0, 1.0),
                    vec2(1.0, 1.0),
                    vec2(-1.0, 1.0),
                    vec2(0.0, 2.0),
                ] {
                    painter.galley_with_override_text_color(
                        p + d,
                        g.clone(),
                        Color32::from_black_alpha(200),
                    );
                }
            }
        };
        let tp = pos2(body.center().x - title.size().x / 2.0, y);
        shadow(tp, &title);
        y += title.size().y + 6.0;
        painter.galley(tp, title, theme::TEXT);
        if let Some(d) = detail {
            let dp = pos2(body.center().x - d.size().x / 2.0, y);
            shadow(dp, &d);
            y += d.size().y;
            painter.galley(
                dp,
                d,
                if st.resizing {
                    theme::TEXT
                } else {
                    theme::MUTED
                },
            );
        }
        if st.actions {
            let w = 110.0;
            let r1 = Rect::from_min_size(pos2(body.center().x - w - 4.0, y + 12.0), vec2(w, 44.0));
            let r2 = Rect::from_min_size(pos2(body.center().x + 4.0, y + 12.0), vec2(w, 44.0));
            if Btn::new("Retry")
                .primary()
                .show_at(ui, r1, egui::Id::unique(("retry", id)))
                .clicked()
            {
                self.cmds.push(Cmd::Retry(id.to_string()));
            }
            if Btn::new("Close")
                .show_at(ui, r2, egui::Id::unique(("close", id)))
                .clicked()
            {
                self.cmds.push(Cmd::CloseTile(id.to_string()));
            }
        }
    }

    fn tile_footer(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        foot: Rect,
        v: &TileView,
        with_stats: bool,
    ) {
        painter.rect_filled(
            foot.shrink2(vec2(2.0, 0.0)),
            CornerRadius {
                nw: 0,
                ne: 0,
                sw: 10,
                se: 10,
            },
            theme::BG_2,
        );
        painter.line_segment(
            [foot.left_top(), foot.right_top()],
            Stroke::new(1.0, theme::LINE),
        );
        let inner = foot.shrink2(vec2(10.0, 7.0));
        let id = v.id.clone();
        let mode = v.tile.view_mode;
        let eye = match v.resolved.params.eye {
            xrmc_render::Eye::Left => Eye::Left,
            xrmc_render::Eye::Right => Eye::Right,
        };
        let seg_w: f32 = VIEW_MODES
            .iter()
            .map(|(_, l, _)| (widgets::text_width(ui, l, theme::font_bold(13.0)) + 24.0).max(44.0))
            .sum();
        let eye_w = if mode == ViewMode::Corrected {
            88.0 + 8.0
        } else {
            0.0
        };
        let tools_w = 5.0 * 44.0 + 4.0 * 4.0;
        let roomy = inner.width() >= seg_w + 8.0 + eye_w + tools_w + 12.0;
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(inner)
                .layout(Layout::left_to_right(Align::Center)),
        );
        child.spacing_mut().item_spacing.x = 8.0;
        if roomy {
            let opts: Vec<(ViewMode, &str, &str)> = VIEW_MODES.to_vec();
            if let Some(m) = widgets::segmented(
                &mut child,
                egui::Id::unique(("view", &id)),
                &opts,
                mode,
                44.0,
            ) {
                self.cmds.push(Cmd::SetViewMode(id.clone(), m));
            }
        } else {
            let label = VIEW_MODES
                .iter()
                .find(|m| m.0 == mode)
                .map(|m| m.1)
                .unwrap_or("View");
            let resp = Btn::new(label)
                .with_icon(Icon::Eye)
                .tooltip("View mode")
                .show(&mut child);
            egui::Popup::menu(&resp).width(260.0).show(|ui| {
                for (m, l, tip) in VIEW_MODES {
                    if ui
                        .add_sized([250.0, 44.0], egui::Button::selectable(m == mode, l))
                        .on_hover_text(tip)
                        .clicked()
                    {
                        self.cmds.push(Cmd::SetViewMode(id.clone(), m));
                        ui.close();
                    }
                }
            });
        }
        if mode == ViewMode::Corrected && (roomy || inner.width() >= 330.0) {
            let opts = [(Eye::Left, "L", "Left eye"), (Eye::Right, "R", "Right eye")];
            if let Some(e) =
                widgets::segmented(&mut child, egui::Id::unique(("eye", &id)), &opts, eye, 44.0)
            {
                self.cmds.push(Cmd::SetEye(id.clone(), e));
            }
        }
        child.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if Btn::icon(Icon::Stop)
                .kind(Kind::GhostDanger)
                .tooltip("Stop casting")
                .show(ui)
                .clicked()
            {
                self.cmds.push(Cmd::Stop(id.clone()));
            }
            if roomy {
                if Btn::icon(Icon::Expand)
                    .kind(Kind::Ghost)
                    .tooltip("Fullscreen")
                    .show(ui)
                    .clicked()
                {
                    self.cmds.push(Cmd::TileFullscreen(id.clone()));
                }
                if Btn::icon(Icon::Camera)
                    .kind(Kind::Ghost)
                    .tooltip("Save a snapshot (PNG)")
                    .show(ui)
                    .clicked()
                {
                    self.cmds.push(Cmd::Snapshot(id.clone()));
                }
                if Btn::icon(Icon::Sliders)
                    .kind(Kind::Ghost)
                    .tooltip("Calibrate lens correction")
                    .show(ui)
                    .clicked()
                {
                    self.cmds.push(Cmd::Calibrate(id.clone()));
                }
                if Btn::icon(Icon::Rotate)
                    .kind(Kind::Ghost)
                    .tooltip("Rotate 90° clockwise")
                    .show(ui)
                    .clicked()
                {
                    self.cmds.push(Cmd::Rotate(id.clone()));
                }
            } else {
                let resp = Btn::icon(Icon::More)
                    .kind(Kind::Ghost)
                    .tooltip("More tools")
                    .show(ui);
                egui::Popup::menu(&resp)
                    .width(260.0)
                    .show(|ui| self.tile_menu(ui, v, false));
            }
            if with_stats && !v.stats.is_empty() {
                let w = ui.available_width();
                let font = theme::font(12.5);
                let text = widgets::fit_text(ui.painter(), &v.stats, font.clone(), w - 8.0);
                let r = ui.add(
                    egui::Label::new(RichText::new(text).font(font).color(theme::MUTED))
                        .truncate()
                        .sense(Sense::click()),
                );
                let r = if v.stats_tip.is_empty() {
                    r
                } else {
                    r.on_hover_text(&v.stats_tip)
                };
                if r.clicked() {
                    self.cmds
                        .push(Cmd::Open(Dialog::codecs(Some(v.id.clone()))));
                }
            }
        });
    }

    /// Tools for a tile in a popup (narrow / compact tiles).
    fn tile_menu(&mut self, ui: &mut egui::Ui, v: &TileView, with_view: bool) {
        let id = v.id.clone();
        if with_view {
            ui.label(
                RichText::new("VIEW")
                    .font(theme::font_bold(11.5))
                    .color(theme::MUTED),
            );
            for (m, l, tip) in VIEW_MODES {
                if ui
                    .add_sized(
                        [250.0, 44.0],
                        egui::Button::selectable(m == v.tile.view_mode, l),
                    )
                    .on_hover_text(tip)
                    .clicked()
                {
                    self.cmds.push(Cmd::SetViewMode(id.clone(), m));
                    ui.close();
                }
            }
            if v.tile.view_mode == ViewMode::Corrected {
                let eye = v.resolved.correction.eye;
                ui.horizontal(|ui| {
                    for (e, l) in [(Eye::Left, "Left eye"), (Eye::Right, "Right eye")] {
                        if ui
                            .add_sized([121.0, 44.0], egui::Button::selectable(eye == e, l))
                            .clicked()
                        {
                            self.cmds.push(Cmd::SetEye(id.clone(), e));
                            ui.close();
                        }
                    }
                });
            }
            ui.separator();
        }
        let item = |ui: &mut egui::Ui, icon: Icon, label: &str| {
            Btn::new(label)
                .with_icon(icon)
                .kind(Kind::Ghost)
                .min_width(250.0)
                .show(ui)
                .clicked()
        };
        if item(ui, Icon::Rotate, "Rotate 90° clockwise") {
            self.cmds.push(Cmd::Rotate(id.clone()));
            ui.close();
        }
        if item(ui, Icon::Sliders, "Calibrate lens correction") {
            self.cmds.push(Cmd::Calibrate(id.clone()));
            ui.close();
        }
        if item(ui, Icon::Info, "Video codecs") {
            self.cmds.push(Cmd::Open(Dialog::codecs(Some(id.clone()))));
            ui.close();
        }
        if item(ui, Icon::Camera, "Save a snapshot (PNG)") {
            self.cmds.push(Cmd::Snapshot(id.clone()));
            ui.close();
        }
        if item(ui, Icon::Expand, "Fullscreen") {
            self.cmds.push(Cmd::TileFullscreen(id.clone()));
            ui.close();
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        let area = ui.available_rect_before_wrap();
        let w = (area.width() - 48.0).min(640.0);
        let mut child = ui.new_child(UiBuilder::new().max_rect(area.shrink(24.0)));
        egui::ScrollArea::vertical().id_salt("empty").show(&mut child, |ui| {
            ui.add_space(((area.height() - 460.0) / 2.0).max(0.0));
            ui.vertical_centered(|ui| {
                egui::Frame::new()
                    .fill(theme::PANEL)
                    .stroke(Stroke::new(1.0, theme::LINE))
                    .corner_radius(16)
                    .inner_margin(Margin::symmetric(30, 26))
                    .show(ui, |ui| {
                        ui.set_width(w - 60.0);
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            ui.label(
                                RichText::new(model::empty_title(&self.model.headsets))
                                    .font(theme::font_bold(23.0))
                                    .color(theme::TEXT),
                            );
                            ui.add_space(4.0);
                            ui.add(egui::Label::new(
                                "Headsets are added over USB first so you can verify exactly which devices are allowed to cast. After that they can cast over Wi-Fi.",
                            ).wrap());
                            ui.add_space(8.0);
                            let steps = [
                                ("Enable developer mode / USB debugging", " on the headset (see the connection guide for your brand)."),
                                ("Plug it in", " with a USB-C data cable, put it on and accept Allow USB debugging."),
                                ("Tap Verify", " on the headset in the headset list. Tick Enable Wi-Fi casting to unplug afterwards."),
                                ("Tap Cast", " (or Cast all) to see the headset's view here, lens-corrected."),
                            ];
                            for (i, (b, rest)) in steps.iter().enumerate() {
                                widgets::item(ui, &format!("{}.", i + 1), 15.0, &[(b, widgets::Run::Bold), (rest, widgets::Run::Normal)]);
                                ui.add_space(4.0);
                            }
                            ui.add_space(10.0);
                            ui.horizontal_wrapped(|ui| {
                                if Btn::new("Connection guide").with_icon(Icon::Help).primary().show(ui).clicked() {
                                    self.cmds.push(Cmd::Open(Dialog::Guide { brand: "meta".into() }));
                                }
                                if self.narrow
                                    && Btn::new("Headsets").with_icon(Icon::Menu).show(ui).clicked()
                                {
                                    self.cmds.push(Cmd::ToggleNav);
                                }
                                if !self.model.castable_not_streaming().is_empty()
                                    && Btn::new("Cast all").with_icon(Icon::Play).show(ui).clicked()
                                {
                                    self.cmds.push(Cmd::CastAll);
                                }
                            });
                            ui.add_space(10.0);
                            widgets::small_muted(
                                ui,
                                "Supported: Meta Quest · PICO · Play For Dream / YVR · HTC VIVE Focus / XR Elite · Android XR",
                            );
                        });
                    });
            });
        });
    }
}

/// The name label over the video (`.video-label`).
fn video_label(
    p: &egui::Painter,
    body: Rect,
    label: &str,
    sub: &str,
    color: Color32,
    big: bool,
    reserve_right: f32,
) {
    let (fs, ss) = if big { (22.0, 14.0) } else { (15.0, 11.5) };
    let max_w = body.width() - 36.0 - reserve_right;
    if max_w < 40.0 {
        return;
    }
    let label = widgets::fit_text(p, label, theme::font_bold(fs), max_w);
    let sub = widgets::fit_text(p, sub, theme::font(ss), max_w);
    let g1 = p.layout_no_wrap(label, theme::font_bold(fs), Color32::WHITE);
    let g2 = p.layout_no_wrap(sub, theme::font(ss), Color32::from_rgb(0xc9, 0xd2, 0xe6));
    let w = g1.size().x.max(g2.size().x) + 24.0;
    let h = g1.size().y + g2.size().y + 12.0;
    let r = Rect::from_min_size(body.min + vec2(10.0, 10.0), vec2(w, h));
    p.rect_filled(r, 8.0, Color32::from_black_alpha(170));
    p.rect_filled(
        Rect::from_min_size(r.min, vec2(4.0, h)),
        CornerRadius {
            nw: 8,
            sw: 8,
            ne: 0,
            se: 0,
        },
        color,
    );
    let y2 = r.min.y + 6.0 + g1.size().y;
    p.galley(pos2(r.min.x + 14.0, r.min.y + 6.0), g1, Color32::WHITE);
    p.galley(
        pos2(r.min.x + 14.0, y2),
        g2,
        Color32::from_rgb(0xc9, 0xd2, 0xe6),
    );
}
