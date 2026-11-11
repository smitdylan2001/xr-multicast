//! Touch-friendly widgets in the Electron app's style: buttons with icons,
//! segmented controls, chips, badges, spinners.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, Mesh, Painter, Pos2, Rect, Response, Sense, Shape, Stroke,
    StrokeKind, Ui, Vec2, pos2, vec2,
};

use super::icons::{self, Icon};
use crate::theme::{self, TOUCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Normal,
    Primary,
    Danger,
    Ghost,
    GhostDanger,
}

/// A button: optional icon, optional label.
#[derive(Debug, Clone)]
pub struct Btn<'a> {
    pub icon: Option<Icon>,
    pub label: &'a str,
    pub kind: Kind,
    pub height: f32,
    pub min_width: f32,
    pub enabled: bool,
    pub tooltip: Option<&'a str>,
    pub busy: bool,
    pub font_size: f32,
}

impl<'a> Btn<'a> {
    pub fn new(label: &'a str) -> Self {
        Btn {
            icon: None,
            label,
            kind: Kind::Normal,
            height: TOUCH,
            min_width: TOUCH,
            enabled: true,
            tooltip: None,
            busy: false,
            font_size: 15.0,
        }
    }
    pub fn icon(icon: Icon) -> Self {
        Btn {
            icon: Some(icon),
            ..Btn::new("")
        }
    }
    pub fn with_icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn kind(mut self, k: Kind) -> Self {
        self.kind = k;
        self
    }
    pub fn primary(self) -> Self {
        self.kind(Kind::Primary)
    }
    pub fn danger(self) -> Self {
        self.kind(Kind::Danger)
    }
    pub fn ghost(self) -> Self {
        self.kind(Kind::Ghost)
    }
    pub fn height(mut self, h: f32) -> Self {
        self.height = h;
        self.min_width = self.min_width.min(h);
        self
    }
    pub fn min_width(mut self, w: f32) -> Self {
        self.min_width = w;
        self
    }
    pub fn enabled(mut self, e: bool) -> Self {
        self.enabled = e;
        self
    }
    pub fn tooltip(mut self, t: &'a str) -> Self {
        self.tooltip = Some(t);
        self
    }
    pub fn busy(mut self, b: bool) -> Self {
        self.busy = b;
        self
    }
    pub fn font(mut self, size: f32) -> Self {
        self.font_size = size;
        self
    }

    fn colors(&self, hovered: bool, pressed: bool) -> (Color32, Color32, Color32) {
        use theme::*;
        let (bg, line, fg) = match self.kind {
            Kind::Normal if hovered => (HOVER, LINE_HOVER, TEXT),
            Kind::Normal => (PANEL_2, LINE_2, TEXT),
            Kind::Primary if hovered => (ACCENT_2, ACCENT_2, Color32::WHITE),
            Kind::Primary => (ACCENT, ACCENT, Color32::WHITE),
            Kind::Danger if hovered => (DANGER_BG_HOVER, DANGER_LINE, DANGER_TEXT),
            Kind::Danger => (DANGER_BG, DANGER_LINE, DANGER_TEXT),
            Kind::Ghost if hovered => (Color32::from_white_alpha(16), Color32::TRANSPARENT, TEXT),
            Kind::Ghost => (Color32::TRANSPARENT, Color32::TRANSPARENT, TEXT),
            Kind::GhostDanger if hovered => (
                Color32::from_white_alpha(16),
                Color32::TRANSPARENT,
                Color32::from_rgb(0xff, 0x8a, 0x8a),
            ),
            Kind::GhostDanger => (
                Color32::TRANSPARENT,
                Color32::TRANSPARENT,
                Color32::from_rgb(0xff, 0x8a, 0x8a),
            ),
        };
        if pressed {
            (theme::mix(bg, Color32::BLACK, 0.85), line, fg)
        } else {
            (bg, line, fg)
        }
    }

    /// Width this button needs.
    pub fn width(&self, ui: &Ui) -> f32 {
        let icon = if self.icon.is_some() {
            self.height * 0.42
        } else {
            0.0
        };
        let text = if self.label.is_empty() {
            0.0
        } else {
            text_width(ui, self.label, theme::font_bold(self.font_size))
        };
        let gap = if icon > 0.0 && text > 0.0 { 8.0 } else { 0.0 };
        let pad = if text > 0.0 { 16.0 } else { 0.0 };
        (icon + gap + text + 2.0 * pad).max(self.min_width)
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let size = vec2(self.width(ui), self.height);
        let sense = if self.enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let (rect, resp) = ui.allocate_exact_size(size, sense);
        self.paint_at(ui, rect, resp)
    }

    pub fn show_at(self, ui: &mut Ui, rect: Rect, id: egui::Id) -> Response {
        let sense = if self.enabled {
            Sense::click()
        } else {
            Sense::hover()
        };
        let resp = ui.interact(rect, id, sense);
        self.paint_at(ui, rect, resp)
    }

    fn paint_at(self, ui: &mut Ui, rect: Rect, resp: Response) -> Response {
        let label = if self.label.is_empty() {
            self.tooltip.unwrap_or("")
        } else {
            self.label
        };
        let enabled = self.enabled;
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
        if ui.is_rect_visible(rect) {
            let hovered = self.enabled && resp.hovered();
            let pressed = self.enabled && resp.is_pointer_button_down_on();
            let (bg, line, mut fg) = self.colors(hovered, pressed);
            if !self.enabled {
                fg = fg.gamma_multiply(0.45);
            }
            let p = ui.painter();
            let bg = if self.enabled {
                bg
            } else {
                bg.gamma_multiply(0.6)
            };
            p.rect(rect, 8.0, bg, Stroke::new(1.0, line), StrokeKind::Inside);
            let icon_side = (self.height * 0.42).round();
            let text_w = if self.label.is_empty() {
                0.0
            } else {
                text_width(ui, self.label, theme::font_bold(self.font_size))
            };
            let gap = if self.icon.is_some() && text_w > 0.0 {
                8.0
            } else {
                0.0
            };
            let content = if self.icon.is_some() { icon_side } else { 0.0 } + gap + text_w;
            let mut x = rect.center().x - content / 2.0;
            if let Some(icon) = self.icon {
                let r = Rect::from_min_size(
                    pos2(x, rect.center().y - icon_side / 2.0),
                    vec2(icon_side, icon_side),
                );
                if self.busy {
                    spinner(p, r.center(), icon_side / 2.0, fg, ui.input(|i| i.time));
                    ui.ctx().request_repaint();
                } else {
                    icons::paint(p, r, icon, fg);
                }
                x += icon_side + gap;
            }
            if text_w > 0.0 {
                p.text(
                    pos2(x, rect.center().y),
                    Align2::LEFT_CENTER,
                    self.label,
                    theme::font_bold(self.font_size),
                    fg,
                );
            }
        }
        match self.tooltip {
            Some(t) if !t.is_empty() => resp.on_hover_text(t),
            _ => resp,
        }
    }
}

pub fn text_width(ui: &Ui, text: &str, font: egui::FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_string(), font, Color32::WHITE)
        .size()
        .x
}

/// Segmented control (`.seg`). Returns the newly picked value.
pub fn segmented<T: Copy + PartialEq>(
    ui: &mut Ui,
    id: egui::Id,
    options: &[(T, &str, &str)],
    current: T,
    height: f32,
) -> Option<T> {
    let font = theme::font_bold(13.0);
    let widths: Vec<f32> = options
        .iter()
        .map(|(_, l, _)| (text_width(ui, l, font.clone()) + 24.0).max(height))
        .collect();
    let total: f32 = widths.iter().sum();
    let (rect, _) = ui.allocate_exact_size(vec2(total, height), Sense::hover());
    let mut picked = None;
    let mut x = rect.min.x;
    let p = ui.painter().clone();
    p.rect(
        rect,
        8.0,
        Color32::TRANSPARENT,
        Stroke::new(1.0, theme::LINE_2),
        StrokeKind::Inside,
    );
    for (i, ((value, label, tip), w)) in options.iter().zip(&widths).enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.min.y), vec2(*w, height));
        let resp = ui.interact(r, id.with(i), Sense::click());
        resp.widget_info(|| {
            egui::WidgetInfo::selected(egui::Role::RadioButton, true, *value == current, *label)
        });
        let active = *value == current;
        let radius = CornerRadius {
            nw: if i == 0 { 8 } else { 0 },
            sw: if i == 0 { 8 } else { 0 },
            ne: if i + 1 == options.len() { 8 } else { 0 },
            se: if i + 1 == options.len() { 8 } else { 0 },
        };
        if active {
            p.rect_filled(r.shrink(1.0), radius, theme::ACCENT);
        } else if resp.hovered() {
            p.rect_filled(r.shrink(1.0), radius, Color32::from_white_alpha(14));
        }
        if i > 0 {
            p.line_segment(
                [pos2(r.min.x, r.min.y + 1.0), pos2(r.min.x, r.max.y - 1.0)],
                Stroke::new(1.0, theme::LINE_2),
            );
        }
        p.text(
            r.center(),
            Align2::CENTER_CENTER,
            *label,
            font.clone(),
            if active { Color32::WHITE } else { theme::MUTED },
        );
        let resp = if tip.is_empty() {
            resp
        } else {
            resp.on_hover_text(*tip)
        };
        if resp.clicked() && !active {
            picked = Some(*value);
        }
        x += w;
    }
    picked
}

/// Chip colours: text, border, background.
pub type ChipStyle = (Color32, Color32, Color32);
pub const CHIP_OFF: ChipStyle = (theme::MUTED, theme::LINE_2, Color32::TRANSPARENT);

/// A rounded chip (`.chip`) with an optional icon.
pub fn chip(
    ui: &mut Ui,
    icon: Option<Icon>,
    text: &str,
    style: ChipStyle,
    tooltip: &str,
) -> Response {
    let font = theme::font_bold(12.0);
    let h = 26.0;
    let icon_w = if icon.is_some() { 14.0 + 5.0 } else { 0.0 };
    let w = text_width(ui, text, font.clone()) + icon_w + 20.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter();
    p.rect(
        rect,
        13.0,
        style.2,
        Stroke::new(1.0, style.1),
        StrokeKind::Inside,
    );
    let mut x = rect.min.x + 10.0;
    if let Some(i) = icon {
        icons::paint(
            p,
            Rect::from_center_size(pos2(x + 7.0, rect.center().y), vec2(14.0, 14.0)),
            i,
            style.0,
        );
        x += icon_w;
    }
    p.text(
        pos2(x, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        font,
        style.0,
    );
    if tooltip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tooltip)
    }
}

/// A filled badge (brand pill, LIVE badge): returns its rect. `pos` is the left-centre.
pub fn badge(p: &Painter, pos: Pos2, text: &str, size: f32, bg: Color32, fg: Color32) -> Rect {
    let galley = p.layout_no_wrap(text.to_string(), theme::font_bold(size), fg);
    let pad = vec2(size * 0.6, size * 0.3);
    let rect = Rect::from_min_size(
        pos2(pos.x, pos.y - galley.size().y / 2.0 - pad.y),
        galley.size() + pad * 2.0,
    );
    p.rect_filled(rect, 4.0, bg);
    p.galley(rect.min + pad, galley, fg);
    rect
}

/// Width of [`badge`] for layout.
pub fn badge_width(ui: &Ui, text: &str, size: f32) -> f32 {
    text_width(ui, text, theme::font_bold(size)) + size * 1.2
}

pub fn spinner(p: &Painter, center: Pos2, radius: f32, color: Color32, time: f64) {
    let start = (time * 4.0) as f32;
    let n = 24;
    let pts: Vec<Pos2> = (0..=n)
        .map(|i| {
            let a = start + i as f32 / n as f32 * std::f32::consts::PI * 1.5;
            center + radius * Vec2::angled(a)
        })
        .collect();
    p.circle_stroke(
        center,
        radius,
        Stroke::new(radius * 0.22, color.gamma_multiply(0.2)),
    );
    p.add(Shape::line(pts, Stroke::new(radius * 0.22, color)));
}

/// Horizontal gradient fill.
pub fn gradient_rect(p: &Painter, rect: Rect, left: Color32, right: Color32) {
    let mut mesh = Mesh::default();
    let i = mesh.vertices.len() as u32;
    mesh.colored_vertex(rect.left_top(), left);
    mesh.colored_vertex(rect.right_top(), right);
    mesh.colored_vertex(rect.right_bottom(), right);
    mesh.colored_vertex(rect.left_bottom(), left);
    mesh.add_triangle(i, i + 1, i + 2);
    mesh.add_triangle(i, i + 2, i + 3);
    p.add(Shape::mesh(mesh));
}

/// Text truncated with an ellipsis to fit `max_w`.
pub fn fit_text(p: &Painter, text: &str, font: egui::FontId, max_w: f32) -> String {
    let w = |s: &str| {
        p.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
    };
    if w(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars[..mid].iter().collect::<String>() + "…";
        if w(&s) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    chars[..lo].iter().collect::<String>() + "…"
}

/// A large toggle row for touch: checkbox + bold title + muted description.
pub fn check_row(ui: &mut Ui, value: &mut bool, title: &str, desc: &str) -> Response {
    let resp = ui
        .horizontal(|ui| {
            let r = ui.checkbox(value, "");
            ui.vertical(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(title).strong()).wrap());
                if !desc.is_empty() {
                    ui.add(
                        egui::Label::new(egui::RichText::new(desc).small().color(theme::MUTED))
                            .wrap(),
                    );
                }
            });
            r
        })
        .inner;
    ui.add_space(2.0);
    resp
}

/// Section heading (`h4` in the dialogs).
pub fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .font(theme::font_bold(12.0))
            .color(theme::MUTED),
    );
    ui.add_space(2.0);
}

pub fn muted(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(egui::Label::new(egui::RichText::new(text.into()).color(theme::MUTED)).wrap())
}

pub fn small_muted(ui: &mut Ui, text: impl Into<String>) -> Response {
    ui.add(egui::Label::new(egui::RichText::new(text.into()).small().color(theme::MUTED)).wrap())
}

/// A colour-coded status item (green fine / amber caveat / red broken). Amber and
/// red ones are buttons that explain the fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Good,
    Warn,
    Bad,
    Neutral,
}

pub fn level_color(l: Level) -> Color32 {
    match l {
        Level::Good => theme::OK,
        Level::Warn => theme::WARN,
        Level::Bad => theme::BAD,
        Level::Neutral => theme::MUTED,
    }
}

pub fn status_item(ui: &mut Ui, icon: Icon, text: &str, level: Level, height: f32) -> Response {
    let font = theme::font(13.0);
    let clickable = matches!(level, Level::Warn | Level::Bad);
    let w = text_width(ui, text, font.clone()) + 18.0 + 6.0 + if clickable { 12.0 } else { 0.0 };
    let sense = if clickable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, height), sense);
    let color = level_color(level);
    let p = ui.painter();
    if clickable && resp.hovered() {
        p.rect_filled(rect, 6.0, Color32::from_white_alpha(18));
    }
    let x0 = rect.min.x + if clickable { 6.0 } else { 0.0 };
    icons::paint(
        p,
        Rect::from_center_size(pos2(x0 + 8.0, rect.center().y), vec2(16.0, 16.0)),
        icon,
        color,
    );
    let text_pos = pos2(x0 + 22.0, rect.center().y);
    let tr = p.text(text_pos, Align2::LEFT_CENTER, text, font, color);
    if clickable {
        // Dotted underline: "click for details".
        let y = tr.max.y + 1.0;
        let mut x = tr.min.x;
        while x < tr.max.x {
            p.line_segment(
                [pos2(x, y), pos2((x + 2.0).min(tr.max.x), y)],
                Stroke::new(1.0, color),
            );
            x += 4.0;
        }
        resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, text));
        resp.on_hover_text("Tap for details and how to fix it")
    } else {
        resp
    }
}

/// Styles for [`rich`] text runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run {
    Normal,
    Bold,
    Muted,
    Accent,
}

/// Wrapped text made of differently styled runs (one label, so it wraps like a paragraph).
pub fn rich(ui: &mut Ui, size: f32, runs: &[(&str, Run)]) -> Response {
    let mut job = egui::text::LayoutJob::default();
    for (text, run) in runs {
        let (font, color) = match run {
            Run::Normal => (theme::font(size), theme::TEXT),
            Run::Bold => (theme::font_bold(size), theme::TEXT),
            Run::Muted => (theme::font(size), theme::MUTED),
            Run::Accent => (theme::font_bold(size), theme::ACCENT),
        };
        job.append(
            text,
            0.0,
            egui::TextFormat {
                font_id: font,
                color,
                ..Default::default()
            },
        );
    }
    ui.add(egui::Label::new(job).wrap())
}

/// A numbered or bulleted item: the marker, then wrapped text aligned next to it.
pub fn item(ui: &mut Ui, marker: &str, size: f32, runs: &[(&str, Run)]) {
    ui.horizontal_top(|ui| {
        ui.spacing_mut().interact_size.y = 0.0;
        ui.spacing_mut().item_spacing.x = 6.0;
        let w = text_width(ui, "00.", theme::font_bold(size));
        ui.add_sized(
            [w, size * 1.3],
            egui::Label::new(
                egui::RichText::new(marker)
                    .font(theme::font_bold(size))
                    .color(theme::ACCENT),
            ),
        );
        ui.vertical(|ui| {
            rich(ui, size, runs);
        });
    });
}
