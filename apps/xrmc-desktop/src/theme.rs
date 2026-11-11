//! Colours, fonts and egui style, matching the Electron app's dark theme (`styles.css`).

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke,
    TextStyle, Visuals,
};

pub const BG: Color32 = Color32::from_rgb(0x0b, 0x0f, 0x17);
pub const BG_2: Color32 = Color32::from_rgb(0x11, 0x17, 0x25);
pub const PANEL: Color32 = Color32::from_rgb(0x15, 0x1c, 0x2c);
pub const PANEL_2: Color32 = Color32::from_rgb(0x1b, 0x23, 0x36);
pub const HOVER: Color32 = Color32::from_rgb(0x23, 0x2d, 0x45);
pub const LINE: Color32 = Color32::from_rgb(0x26, 0x30, 0x49);
pub const LINE_2: Color32 = Color32::from_rgb(0x33, 0x40, 0x5f);
pub const LINE_HOVER: Color32 = Color32::from_rgb(0x42, 0x50, 0x7a);
pub const TEXT: Color32 = Color32::from_rgb(0xe6, 0xeb, 0xf5);
pub const MUTED: Color32 = Color32::from_rgb(0x8b, 0x97, 0xb0);
pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x8c, 0xff);
pub const ACCENT_2: Color32 = Color32::from_rgb(0x3a, 0x73, 0xe6);
pub const OK: Color32 = Color32::from_rgb(0x22, 0xc5, 0x5e);
pub const WARN: Color32 = Color32::from_rgb(0xf5, 0x9e, 0x0b);
pub const BAD: Color32 = Color32::from_rgb(0xef, 0x44, 0x44);

// Chips (`.chip.on`, `.chip.verified`, `.chip.unverified`).
pub const CHIP_ON: (Color32, Color32, Color32) = (
    Color32::from_rgb(0xb5, 0xf5, 0xc9),
    Color32::from_rgb(0x23, 0x63, 0x3a),
    Color32::from_rgb(0x11, 0x33, 0x1f),
);
pub const CHIP_VERIFIED: (Color32, Color32, Color32) = (
    Color32::from_rgb(0xb9, 0xd3, 0xff),
    Color32::from_rgb(0x2a, 0x4a, 0x85),
    Color32::from_rgb(0x14, 0x24, 0x43),
);
pub const CHIP_UNVERIFIED: (Color32, Color32, Color32) = (
    Color32::from_rgb(0xff, 0xd7, 0x9a),
    Color32::from_rgb(0x7a, 0x55, 0x12),
    Color32::from_rgb(0x33, 0x25, 0x0b),
);
pub const DANGER_TEXT: Color32 = Color32::from_rgb(0xff, 0xb4, 0xb4);
pub const DANGER_LINE: Color32 = Color32::from_rgb(0x6b, 0x2a, 0x2a);
pub const DANGER_BG: Color32 = Color32::from_rgb(0x2a, 0x15, 0x15);
pub const DANGER_BG_HOVER: Color32 = Color32::from_rgb(0x3b, 0x1a, 0x1a);
pub const HINT_WARN: Color32 = Color32::from_rgb(0xff, 0xd7, 0x9a);

/// Minimum height of anything you can tap (logical points).
pub const TOUCH: f32 = 44.0;
pub const RADIUS: u8 = 10;

/// Font family with a bold face (falls back to the regular face when no bold system font exists).
pub fn bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

pub fn font_bold(size: f32) -> FontId {
    FontId::new(size, bold())
}

/// `#RRGGBB` → colour (`fallback` when malformed).
pub fn parse_hex(s: &str, fallback: Color32) -> Color32 {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return fallback;
    }
    match u32::from_str_radix(s, 16) {
        Ok(v) => Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8),
        Err(_) => fallback,
    }
}

/// `color-mix(in srgb, a p%, b)`.
pub fn mix(a: Color32, b: Color32, p: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 * p + y as f32 * (1.0 - p)).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Readable text colour on top of `bg`.
pub fn on_color(bg: Color32) -> Color32 {
    let l = 0.299 * bg.r() as f32 + 0.587 * bg.g() as f32 + 0.114 * bg.b() as f32;
    if l > 150.0 { BG } else { Color32::WHITE }
}

/// System fonts that look closer to the Electron app (Inter / Segoe UI / system-ui)
/// than egui's built-in font. Nothing is bundled; missing fonts fall back to egui's.
fn system_fonts() -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let regular: &[&str] = &[
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\segoeui.ttf",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
        // Android (static Roboto; the variable Roboto-Regular.ttf only gives the default weight).
        "/system/fonts/RobotoStatic-Regular.ttf",
        "/system/fonts/Roboto-Regular.ttf",
    ];
    let bold: &[&str] = &[
        "/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/google-noto/NotoSans-Bold.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        "C:\\Windows\\Fonts\\segoeuib.ttf",
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/Library/Fonts/Arial Bold.ttf",
        "/system/fonts/DroidSans-Bold.ttf",
    ];
    let load = |list: &[&str]| list.iter().find_map(|p| std::fs::read(p).ok());
    (load(regular), load(bold))
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let (regular, bold_face) = system_fonts();
    let prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut bold_list = prop.clone();
    if let Some(r) = regular {
        fonts
            .font_data
            .insert("sys-regular".into(), FontData::from_owned(r).into());
        if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
            list.insert(0, "sys-regular".into());
        }
        bold_list.insert(0, "sys-regular".into());
    }
    if let Some(b) = bold_face {
        fonts
            .font_data
            .insert("sys-bold".into(), FontData::from_owned(b).into());
        bold_list.insert(0, "sys-bold".into());
    }
    // Arrows and other symbols the main fonts may lack (e.g. "Menu → Devices").
    for p in [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\seguisym.ttf",
        "/system/fonts/NotoSansSymbols-Regular-Subsetted.ttf",
        "/System/Library/Fonts/Apple Symbols.ttf",
    ] {
        if let Ok(d) = std::fs::read(p) {
            fonts
                .font_data
                .insert("sys-symbols".into(), FontData::from_owned(d).into());
            if let Some(list) = fonts.families.get_mut(&FontFamily::Proportional) {
                list.push("sys-symbols".into());
            }
            bold_list.push("sys-symbols".into());
            break;
        }
    }
    fonts.families.insert(bold(), bold_list);
    ctx.set_fonts(fonts);
}

pub fn apply_style(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = BG_2;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, LINE_2);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = BG;
    v.faint_bg_color = PANEL;
    v.code_bg_color = BG;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.warn_fg_color = WARN;
    v.error_fg_color = BAD;
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.weak_bg_fill = PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.bg_fill = PANEL_2;
    w.inactive.weak_bg_fill = PANEL_2;
    w.inactive.bg_stroke = Stroke::new(1.0, LINE_2);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.bg_fill = HOVER;
    w.hovered.weak_bg_fill = HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0, LINE_HOVER);
    w.hovered.fg_stroke = Stroke::new(1.5, TEXT);
    w.active.bg_fill = ACCENT_2;
    w.active.weak_bg_fill = ACCENT_2;
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    w.open.bg_fill = HOVER;
    w.open.weak_bg_fill = HOVER;
    w.open.bg_stroke = Stroke::new(1.0, LINE_HOVER);
    for s in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        s.corner_radius = CornerRadius::same(8);
    }
    v.slider_trailing_fill = true;
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.set_theme(egui::Theme::Dark);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, font(20.0)),
            (TextStyle::Body, font(15.0)),
            (TextStyle::Button, font(15.0)),
            (TextStyle::Small, font(12.5)),
            (TextStyle::Monospace, FontId::monospace(13.0)),
        ]
        .into();
        let sp = &mut style.spacing;
        sp.interact_size = egui::vec2(44.0, TOUCH);
        sp.button_padding = egui::vec2(14.0, 8.0);
        sp.item_spacing = egui::vec2(8.0, 8.0);
        sp.slider_width = 220.0;
        sp.combo_height = 320.0;
        sp.icon_width = 22.0;
        sp.icon_width_inner = 12.0;
        sp.icon_spacing = 10.0;
        sp.menu_margin = Margin::same(8);
        sp.window_margin = Margin::same(18);
        sp.scroll.bar_width = 8.0;
        sp.scroll.floating = true;
        sp.tooltip_width = 360.0;
        style.interaction.tooltip_delay = 0.4;
    });
}
