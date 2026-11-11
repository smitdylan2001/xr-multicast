//! The UI's view model: what the core reported (devices, streams, settings) plus
//! per-tile UI state, and the pure functions that turn it into what is shown
//! (headset cards, tile status, tile render parameters, stats, grid layout).
//!
//! Nothing here touches egui or the hub, so it is unit-tested directly.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use xrmc_core::brands::{self, Brand, Correction, Eye};
use xrmc_core::device_info::Battery;
use xrmc_core::layout::{GridShape, Layout, LayoutSetting, ResolvedView, Rotation, ViewInput};
use xrmc_core::view_size::{ViewMode, ViewRequest};
use xrmc_core::{
    Codec, DeviceSnapshot, HeadsetSnapshot, NoticeLevel, Settings, StreamState, StreamStatus,
};
use xrmc_render::{LensParams, TileParams};

// ------------------------------------------------------------------ helpers

pub fn brand_of(h: Option<&HeadsetSnapshot>) -> &'static Brand {
    brands::brand_or_fallback(h.map_or(brands::FALLBACK_BRAND, |h| h.brand.as_str()))
}

/// The brand/model starting correction (`presetFor`).
pub fn preset_for(h: Option<&HeadsetSnapshot>) -> Correction {
    match h {
        Some(h) => brands::preset_correction(&h.brand, h.variant.as_deref()),
        None => brands::preset_correction(brands::FALLBACK_BRAND, None),
    }
}

/// Saved calibration over the preset (`correctionFor`). Values of the wrong type are ignored.
pub fn correction_for(h: Option<&HeadsetSnapshot>) -> Correction {
    let preset = preset_for(h);
    match h.and_then(|h| h.calibration.as_ref()) {
        Some(cal) => merge_correction(&preset, cal),
        None => preset,
    }
}

/// Apply a stored calibration object over `base`, key by key.
pub fn merge_correction(base: &Correction, cal: &Map<String, Value>) -> Correction {
    let mut cal = cal.clone();
    // Calibrations saved before 'auto' rotation existed stored the old default of 0°.
    let zero = |v: &Value| match v {
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.trim().parse::<f64>().ok() == Some(0.0),
        _ => false,
    };
    if !cal.contains_key("flipH") && cal.get("rotation").is_some_and(zero) {
        cal.remove("rotation");
    }
    let Ok(Value::Object(mut current)) = serde_json::to_value(base) else {
        return base.clone();
    };
    let mut out = base.clone();
    for (k, v) in cal {
        if v.is_null() {
            continue;
        }
        let mut trial = current.clone();
        trial.insert(k, v);
        if let Ok(c) = serde_json::from_value::<Correction>(Value::Object(trial.clone())) {
            current = trial;
            out = c;
        }
    }
    out
}

/// `PICO 4 Ultra` rather than `PICO PICO 4 Ultra`, `HTC VIVE Focus Vision` rather than `HTC VIVE VIVE Focus Vision`.
pub fn brand_model(brand: &str, model: &str) -> String {
    let (b, m) = (brand.to_lowercase(), model.to_lowercase());
    if model.is_empty() {
        return brand.to_string();
    }
    if m.starts_with(&b) {
        return model.to_string();
    }
    if let Some((head, last)) = brand.rsplit_once(' ')
        && m.starts_with(&last.to_lowercase())
    {
        return format!("{head} {model}");
    }
    format!("{brand} {model}")
}

/// `#3 Station 3` (`labelOf`).
pub fn label_of(h: Option<&HeadsetSnapshot>) -> String {
    match h {
        Some(h) => match h.index {
            Some(i) => format!("#{i} {}", h.name),
            None => h.name.clone(),
        },
        None => "Headset".into(),
    }
}

pub fn view_mode_from_str(s: Option<&str>) -> Option<ViewMode> {
    match s? {
        "corrected" => Some(ViewMode::Corrected),
        "stereo" => Some(ViewMode::Stereo),
        "raw" => Some(ViewMode::Raw),
        _ => None,
    }
}

pub fn view_mode_str(m: ViewMode) -> &'static str {
    match m {
        ViewMode::Corrected => "corrected",
        ViewMode::Stereo => "stereo",
        ViewMode::Raw => "raw",
    }
}

pub const VIEW_MODES: [(ViewMode, &str, &str); 3] = [
    (
        ViewMode::Corrected,
        "Corrected",
        "Single eye, lens distortion removed (recommended)",
    ),
    (
        ViewMode::Stereo,
        "Both eyes",
        "Both eyes side by side, lens distortion removed",
    ),
    (
        ViewMode::Raw,
        "Raw",
        "Unprocessed panel image (fisheye, both eyes)",
    ),
];

pub fn layout_label(l: LayoutSetting) -> &'static str {
    match l {
        LayoutSetting::Auto => "Auto detect",
        LayoutSetting::Sbs => "Side by side",
        LayoutSetting::Tb => "Top / bottom",
        LayoutSetting::Quad => "2×2 grid",
        LayoutSetting::Mono => "Single image",
    }
}

pub fn codec_short(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "H.264",
        Codec::H265 => "H.265",
        Codec::Av1 => "AV1",
    }
}

pub fn time_ago(ts_ms: u64, now_ms: u64) -> String {
    let s = now_ms.saturating_sub(ts_ms) / 1000;
    if s < 60 {
        "just now".into()
    } else if s < 3600 {
        format!("{} min ago", (s as f64 / 60.0).round())
    } else if s < 86400 {
        format!("{} h ago", (s as f64 / 3600.0).round())
    } else {
        format!("{} d ago", (s as f64 / 86400.0).round())
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------- headset card

/// The main button (or note) on a headset card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Primary {
    Offline(String),
    Verify,
    /// Not verified and not on USB (may still cast when verification isn't required).
    ConnectUsbToVerify {
        can_cast: bool,
        streaming: bool,
    },
    Cast,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryClass {
    Normal,
    Mid,
    /// ≤ 15 % and not charging: shown red and clickable (explains the fix).
    Low,
    LowCharging,
}

/// What a sidebar card shows (`headsetCard`).
#[derive(Debug, Clone, PartialEq)]
pub enum Card {
    Pending {
        title: String,
        subtitle: String,
        brand_color: String,
        hint: String,
        spinner: bool,
        warn: bool,
    },
    Headset(HeadsetCard),
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeadsetCard {
    pub id: String,
    pub index: Option<u32>,
    pub name: String,
    pub color: String,
    pub brand_name: String,
    pub brand_color: String,
    pub model: String,
    pub battery: Option<(Battery, BatteryClass)>,
    pub usb: bool,
    pub wifi: Option<String>,
    pub wifi_tooltip: String,
    pub trusted: bool,
    pub offline: bool,
    pub streaming: bool,
    pub live: bool,
    pub primary: Primary,
    pub enable_wifi: bool,
    pub menu: bool,
}

pub fn battery_class(b: &Battery) -> BatteryClass {
    match b.level {
        0..=15 if b.charging => BatteryClass::LowCharging,
        0..=15 => BatteryClass::Low,
        16..=35 => BatteryClass::Mid,
        _ => BatteryClass::Normal,
    }
}

pub fn card_for(h: &HeadsetSnapshot, st: Option<&StreamState>, now_ms: u64) -> Card {
    let b = brand_of(Some(h));
    if h.pending {
        let (hint, spinner, warn) = match h.status.as_str() {
            "unauthorized" => (
                "Put the headset on and accept Allow USB debugging (tick \"Always allow\")."
                    .to_string(),
                false,
                true,
            ),
            "identifying" => ("Identifying…".to_string(), true, false),
            "error" => (
                h.error
                    .clone()
                    .unwrap_or_else(|| "Could not read device info.".into()),
                false,
                true,
            ),
            other => (
                format!("Device is {other}. Try re-plugging the cable or toggling USB debugging."),
                false,
                true,
            ),
        };
        let name = if h.name.is_empty() {
            "New device".to_string()
        } else {
            h.name.clone()
        };
        return Card::Pending {
            title: name,
            subtitle: format!(
                "{} · {}",
                b.family,
                if h.network == Some(true) {
                    "Wi-Fi"
                } else {
                    "USB"
                }
            ),
            brand_color: b.color.clone(),
            hint,
            spinner,
            warn,
        };
    }
    let streaming = st.is_some_and(|s| s.state.is_running());
    let offline = h.status == "offline";
    let primary = if offline {
        Primary::Offline(match h.last_seen {
            Some(t) => format!("Offline · seen {}", time_ago(t, now_ms)),
            None => "Offline".into(),
        })
    } else if !h.trusted && h.usb.is_some() {
        Primary::Verify
    } else if !h.trusted {
        Primary::ConnectUsbToVerify {
            can_cast: h.can_cast,
            streaming,
        }
    } else if streaming {
        Primary::Stop
    } else {
        Primary::Cast
    };
    let wifi = h
        .wifi
        .as_ref()
        .map(|_| h.ip.clone().unwrap_or_else(|| "Wi-Fi".into()));
    let wifi_tooltip = match (&h.wifi, &h.ip) {
        (Some(w), _) => format!("Connected over Wi-Fi ({w})"),
        (None, Some(ip)) => format!("Last known IP {ip}"),
        (None, None) => "No Wi-Fi connection".into(),
    };
    Card::Headset(HeadsetCard {
        id: h.id.clone(),
        index: h.index,
        name: h.name.clone(),
        color: h.color.clone(),
        brand_name: b.name.clone(),
        brand_color: b.color.clone(),
        model: h.model.clone(),
        battery: h.battery.as_ref().map(|b| (*b, battery_class(b))),
        usb: h.usb.is_some(),
        wifi,
        wifi_tooltip,
        trusted: h.trusted,
        offline,
        streaming,
        live: st.is_some_and(|s| s.state == StreamStatus::Live),
        primary,
        enable_wifi: h.trusted && h.usb.is_some() && h.wifi.is_none(),
        menu: h.trusted,
    })
}

/// Sidebar sections in order: needs attention, connected, saved · offline.
pub fn sidebar_sections(
    headsets: &[HeadsetSnapshot],
) -> Vec<(&'static str, Vec<&HeadsetSnapshot>)> {
    let pending: Vec<_> = headsets.iter().filter(|h| h.pending).collect();
    let online: Vec<_> = headsets
        .iter()
        .filter(|h| !h.pending && h.status == "online")
        .collect();
    let offline: Vec<_> = headsets
        .iter()
        .filter(|h| !h.pending && h.status == "offline")
        .collect();
    [
        ("Needs attention", pending),
        ("Connected", online),
        ("Saved · offline", offline),
    ]
    .into_iter()
    .filter(|(_, v)| !v.is_empty())
    .collect()
}

pub fn empty_title(headsets: &[HeadsetSnapshot]) -> String {
    if headsets.is_empty() {
        return "Connect your first headset".into();
    }
    let castable = headsets.iter().filter(|h| h.can_cast).count();
    if castable > 0 {
        format!(
            "{castable} headset{} ready — tap Cast or Cast all",
            if castable > 1 { "s" } else { "" }
        )
    } else {
        "Verify a headset to start casting".into()
    }
}

// --------------------------------------------------------------------- tiles

/// UI state of one stream tile.
#[derive(Debug, Clone)]
pub struct Tile {
    pub id: String,
    pub view_mode: ViewMode,
    pub eye_override: Option<Eye>,
    pub rotation_override: Option<f64>,
    /// A frame of the current session was shown.
    pub has_video: bool,
    /// Any frame was ever uploaded (the renderer still holds it).
    pub has_frame: bool,
    /// Restarting at a new resolution: show the last frame blurred.
    pub resizing: bool,
    /// Frames of this decoder generation or newer belong to the current session.
    pub generation: u64,
    /// The video can't be shown at all (no decoder for the codec).
    pub fatal: Option<String>,
    /// Starting failed before the core reported a state.
    pub local_error: Option<String>,
    pub frame_size: Option<(u32, u32)>,
    pub bit_depth: u8,
    pub grid: Option<GridShape>,
    pub fps: f32,
    pub mbps: f32,
    stat_at: Option<Instant>,
    stat_frames: u64,
    stat_bytes: u64,
}

impl Tile {
    pub fn new(id: &str, view_mode: ViewMode) -> Tile {
        Tile {
            id: id.to_string(),
            view_mode,
            eye_override: None,
            rotation_override: None,
            has_video: false,
            has_frame: false,
            resizing: false,
            generation: 0,
            fatal: None,
            local_error: None,
            frame_size: None,
            bit_depth: 8,
            grid: None,
            fps: 0.0,
            mbps: 0.0,
            stat_at: None,
            stat_frames: 0,
            stat_bytes: 0,
        }
    }

    /// A new session starts: wait for its first frame (the old one stays visible underneath).
    pub fn reset_video(&mut self) {
        self.has_video = false;
        self.fatal = None;
    }

    /// A frame of decoder generation `generation` was uploaded.
    pub fn on_frame(&mut self, generation: u64, w: u32, h: u32, bit_depth: u8) {
        self.has_frame = true;
        self.frame_size = Some((w, h));
        self.bit_depth = bit_depth;
        if generation >= self.generation && !self.has_video {
            self.has_video = true;
            // First picture at the new resolution: drop the blur.
            self.resizing = false;
        }
    }

    /// Update fps / Mbps from the decoder's running counters (once a second). Returns true when updated.
    pub fn sample_stats(&mut self, now: Instant, frames: u64, bytes: u64) -> bool {
        match self.stat_at {
            None => {
                self.stat_at = Some(now);
                self.stat_frames = frames;
                self.stat_bytes = bytes;
                false
            }
            Some(at) => {
                let dt = now.duration_since(at).as_secs_f32();
                if dt < 1.0 {
                    return false;
                }
                self.fps = frames.saturating_sub(self.stat_frames) as f32 / dt;
                self.mbps = bytes.saturating_sub(self.stat_bytes) as f32 * 8.0 / dt / 1e6;
                self.stat_at = Some(now);
                self.stat_frames = frames;
                self.stat_bytes = bytes;
                true
            }
        }
    }
}

/// Does a new state start a new session whose video must be decoded from scratch?
pub fn needs_reset(prev: Option<&StreamState>, st: &StreamState) -> bool {
    if !matches!(
        st.state,
        StreamStatus::Starting | StreamStatus::Reconnecting
    ) {
        return false;
    }
    match prev {
        Some(p) => {
            matches!(
                p.state,
                StreamStatus::Live | StreamStatus::Resizing | StreamStatus::Error
            ) || (st.codec.is_some() && p.codec != st.codec)
                || (st.ten_bit != p.ten_bit)
        }
        None => false,
    }
}

/// What a tile shows over (or instead of) its video.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileStatus {
    pub title: String,
    pub detail: String,
    pub spinner: bool,
    /// Retry / Close buttons.
    pub actions: bool,
    /// Drawn over the blurred last frame instead of a dark backdrop.
    pub resizing: bool,
}

fn status(title: impl Into<String>, detail: impl Into<String>, spinner: bool) -> TileStatus {
    TileStatus {
        title: title.into(),
        detail: detail.into(),
        spinner,
        actions: false,
        resizing: false,
    }
}

fn via(st: &StreamState) -> String {
    match st.transport {
        Some(xrmc_core::TransportKind::Usb) => "via USB".into(),
        Some(xrmc_core::TransportKind::Wifi) => "via Wi-Fi".into(),
        None => String::new(),
    }
}

/// The tile's status message (`Tile.refresh` in the Electron app).
pub fn tile_status(st: Option<&StreamState>, tile: &Tile) -> Option<TileStatus> {
    if let Some(f) = &tile.fatal {
        return Some(status("Cannot display video", f.clone(), false));
    }
    let Some(st) = st else {
        return Some(match &tile.local_error {
            Some(e) => TileStatus {
                actions: true,
                ..status("Stream stopped", e.clone(), false)
            },
            None => status("Starting stream…", "", true),
        });
    };
    let resizing = tile.resizing && tile.has_frame && st.state != StreamStatus::Error;
    if resizing {
        return Some(TileStatus {
            resizing: true,
            ..status(
                "Changing resolution",
                st.size_long.map(|s| format!("{s} px")).unwrap_or_default(),
                true,
            )
        });
    }
    match st.state {
        StreamStatus::Idle | StreamStatus::Starting => {
            Some(status("Starting stream…", via(st), true))
        }
        StreamStatus::Reconnecting => Some(status(
            format!("Reconnecting… (attempt {})", st.retries.max(1)),
            st.error.clone().unwrap_or_default(),
            true,
        )),
        StreamStatus::Live | StreamStatus::Resizing if !tile.has_video => Some(status(
            "Waiting for video…",
            "Wake the headset if its display is off.",
            true,
        )),
        StreamStatus::Error => Some(TileStatus {
            actions: true,
            ..status(
                "Stream stopped",
                st.error.clone().unwrap_or_else(|| "Unknown error".into()),
                false,
            )
        }),
        _ => None,
    }
}

/// Inputs for [`tile_params`].
#[derive(Debug, Clone, Copy)]
pub struct TileInputs<'a> {
    pub headset: Option<&'a HeadsetSnapshot>,
    pub state: Option<&'a StreamState>,
    pub tile: &'a Tile,
    /// Calibration being edited for this tile (overrides the saved one).
    pub calib: Option<&'a Calib>,
}

/// Concrete render parameters and the resolved layout of a tile.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub params: TileParams,
    pub view: ResolvedView,
    pub correction: Correction,
    /// Layout or rotation is automatic, so the video should be analysed.
    pub wants_detection: bool,
}

/// The correction in effect for a tile (calibration draft, overrides, saved, preset).
pub fn effective_correction(i: &TileInputs<'_>) -> Correction {
    if let Some(c) = i.calib {
        return c.params.clone();
    }
    let mut p = correction_for(i.headset);
    if let Some(e) = i.tile.eye_override {
        p.eye = e;
    }
    if let Some(r) = i.tile.rotation_override {
        p.rotation = Rotation::Degrees(r);
    }
    p
}

pub fn resolve_view(p: &Correction, tile: &Tile, single_eye: bool) -> ResolvedView {
    let (fw, fh) = tile.frame_size.unwrap_or((0, 0));
    xrmc_core::layout::resolve_view(&ViewInput {
        layout: p.layout,
        rotation: p.rotation,
        auto_rotation: p.auto_rotation,
        frame_w: fw,
        frame_h: fh,
        grid: tile.grid,
        single_eye,
    })
}

pub fn parse_output_aspect(s: &str) -> Option<f32> {
    let (a, b) = s.split_once(':')?;
    let (a, b) = (a.trim().parse::<f32>().ok()?, b.trim().parse::<f32>().ok()?);
    (a > 0.0 && b > 0.0).then_some(a / b)
}

pub fn render_view_mode(m: ViewMode) -> xrmc_render::ViewMode {
    match m {
        ViewMode::Corrected => xrmc_render::ViewMode::Corrected,
        ViewMode::Stereo => xrmc_render::ViewMode::Stereo,
        ViewMode::Raw => xrmc_render::ViewMode::Raw,
    }
}

pub fn tile_params(i: &TileInputs<'_>) -> Resolved {
    let p = effective_correction(i);
    let single_eye = i.state.is_some_and(|s| s.single_eye);
    let v = resolve_view(&p, i.tile, single_eye);
    let layout = match v.layout {
        Layout::Sbs => xrmc_render::Layout::Sbs,
        Layout::Tb => xrmc_render::Layout::Tb,
        Layout::Quad => xrmc_render::Layout::Quad {
            bottom_row: p.quad_row == "bottom",
        },
        Layout::Mono => xrmc_render::Layout::Mono,
    };
    let params = TileParams {
        view: render_view_mode(i.tile.view_mode),
        eye: match p.eye {
            Eye::Left => xrmc_render::Eye::Left,
            Eye::Right => xrmc_render::Eye::Right,
        },
        layout,
        rotation: v.rotation,
        flip_h: p.flip_h,
        flip_v: p.flip_v,
        lens: LensParams {
            k1: p.k1 as f32,
            k2: p.k2 as f32,
            zoom: p.zoom as f32,
            center_x: p.center_x as f32,
            center_y: p.center_y as f32,
            chroma_r: p.chroma_r as f32,
            chroma_b: p.chroma_b as f32,
            output_aspect: parse_output_aspect(&p.output_aspect),
        },
        blur: i.tile.resizing,
        grid: i.calib.is_some_and(|c| c.grid),
    };
    Resolved {
        wants_detection: !single_eye
            && (p.layout == LayoutSetting::Auto || p.rotation == Rotation::Auto),
        params,
        view: v,
        correction: p,
    }
}

/// `H.265 · 1920×1024 · 2×1 auto rotated 90° · 60 fps · 8.1 Mbps` (empty until live with video).
pub fn stats_line(st: Option<&StreamState>, tile: &Tile, view: &ResolvedView) -> String {
    let Some(st) = st.filter(|s| s.state == StreamStatus::Live) else {
        return String::new();
    };
    if !tile.has_video {
        return String::new();
    }
    let mut parts = Vec::new();
    if let Some(c) = st.codec {
        let depth = if tile.bit_depth > 8 {
            format!(" {}-bit", tile.bit_depth)
        } else {
            String::new()
        };
        parts.push(format!("{}{depth}", codec_short(c)));
    }
    if let Some((w, h)) = tile.frame_size {
        parts.push(format!("{w}×{h}"));
        let grid = match view.layout {
            Layout::Sbs => "2×1",
            Layout::Tb => "1×2",
            Layout::Quad => "2×2",
            Layout::Mono if st.single_eye => "one eye",
            Layout::Mono => "1×1",
        };
        parts.push(format!(
            "{grid}{}{}",
            if view.detected { " auto" } else { "" },
            if view.rotation != 0 {
                format!(" rotated {}°", view.rotation)
            } else {
                String::new()
            }
        ));
    }
    parts.push(format!("{:.0} fps", tile.fps));
    parts.push(format!("{:.1} Mbps", tile.mbps));
    parts.join(" · ")
}

/// Tooltip of the stats line: quality, encoder and layout.
pub fn stats_tooltip(st: Option<&StreamState>, view: &ResolvedView) -> String {
    let Some(st) = st else { return String::new() };
    let layout = match view.layout {
        Layout::Sbs => "Side by side",
        Layout::Tb => "Top / bottom",
        Layout::Quad => "2×2 grid",
        Layout::Mono => "Single image",
    };
    [
        st.quality.clone(),
        st.encoder.as_ref().map(|e| format!("Encoder: {e}")),
        Some(format!(
            "Panel layout: {layout}{}, rotation {}°{}",
            if view.detected { " (detected)" } else { "" },
            view.rotation,
            if view.rotation_auto { " (auto)" } else { "" }
        )),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n")
}

// ----------------------------------------------------------------- grid

/// Columns for `n` visible tiles (`layoutGrid`); `auto` picks the column count
/// that gives the largest video for the stage's shape.
pub fn grid_columns(
    setting: &str,
    n: usize,
    focused: bool,
    stage: (f32, f32),
    chrome_h: f32,
) -> usize {
    if focused || n <= 1 {
        return 1;
    }
    if let Ok(c) = setting.parse::<usize>()
        && c > 0
    {
        return c.min(n);
    }
    // The Electron rule (1 / 2 / 3 / 4 columns) for landscape screens…
    let classic = if n <= 4 {
        2
    } else if n <= 9 {
        3
    } else {
        4
    };
    let (w, h) = stage;
    if w <= 0.0 || h <= 0.0 {
        return classic;
    }
    // …but pick what shows the most video on portrait / unusual screens.
    let aspect = 16.0 / 9.0;
    let gap = 12.0;
    let score = |cols: usize| {
        let rows = n.div_ceil(cols);
        let tw = (w - gap * (cols as f32 - 1.0)) / cols as f32;
        let th = (h - gap * (rows as f32 - 1.0)) / rows as f32;
        // Small tiles drop their header and footer (labels move onto the video).
        let chrome = if is_compact_tile(tw, th) {
            0.0
        } else {
            chrome_h
        };
        let th = (th - chrome).max(0.0);
        tw.min(th * aspect)
    };
    let best = (1..=n.min(6))
        .max_by(|a, b| score(*a).total_cmp(&score(*b)))
        .unwrap_or(classic);
    if score(classic) >= score(best) * 0.97 {
        classic
    } else {
        best
    }
}

/// Small tiles show only the video, with the label and controls on top of it.
pub fn is_compact_tile(w: f32, h: f32) -> bool {
    h < 300.0 || w < 360.0
}

/// Minimum tile height before the grid scrolls (logical points).
pub const MIN_TILE_H: f32 = 140.0;

/// Tile rectangles (x, y, w, h) relative to the grid's top-left, plus the total content height.
pub fn grid_rects(
    width: f32,
    height: f32,
    n: usize,
    cols: usize,
    gap: f32,
) -> (Vec<[f32; 4]>, f32) {
    if n == 0 || cols == 0 {
        return (Vec::new(), 0.0);
    }
    let rows = n.div_ceil(cols);
    let tw = ((width - gap * (cols as f32 - 1.0)) / cols as f32).max(1.0);
    let fit_h = (height - gap * (rows as f32 - 1.0)) / rows as f32;
    let th = fit_h.max(MIN_TILE_H.min(height)).max(1.0);
    let rects = (0..n)
        .map(|i| {
            let (c, r) = (i % cols, i / cols);
            [c as f32 * (tw + gap), r as f32 * (th + gap), tw, th]
        })
        .collect();
    (rects, rows as f32 * th + (rows as f32 - 1.0) * gap)
}

/// What a tile needs from the stream, for resolution matching (`viewRequirement`).
pub fn view_request(
    visible: bool,
    video_px: (f32, f32),
    mode: ViewMode,
    r: &Resolved,
) -> ViewRequest {
    if !visible {
        return ViewRequest {
            visible: false,
            mode,
            ..Default::default()
        };
    }
    // Round to 8 px so sub-pixel layout jitter doesn't spam the hub.
    let px = |v: f32| ((v / 8.0).round() * 8.0) as f64;
    ViewRequest {
        visible: true,
        tile_w: px(video_px.0),
        tile_h: px(video_px.1),
        mode,
        zoom: if r.correction.zoom > 0.0 {
            r.correction.zoom
        } else {
            1.0
        },
        output_aspect: Some(if r.correction.output_aspect.is_empty() {
            "eye".into()
        } else {
            r.correction.output_aspect.clone()
        }),
        layout: r.view.layout,
        rotation: f64::from(r.view.rotation),
    }
}

// ------------------------------------------------------------- calibration

/// The calibration drawer's draft.
#[derive(Debug, Clone, PartialEq)]
pub struct Calib {
    pub id: String,
    pub params: Correction,
    pub grid: bool,
    pub original: Correction,
}

pub struct SliderSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub hint: &'static str,
}

pub const CALIB_SLIDERS: [SliderSpec; 7] = [
    SliderSpec {
        key: "k1",
        label: "Barrel correction (k1)",
        min: -0.6,
        max: 0.3,
        step: 0.005,
        hint: "Main fisheye removal. More negative = straighter edges.",
    },
    SliderSpec {
        key: "k2",
        label: "Edge correction (k2)",
        min: -0.3,
        max: 0.3,
        step: 0.005,
        hint: "Fine-tunes the outer edge curvature.",
    },
    SliderSpec {
        key: "zoom",
        label: "Zoom",
        min: 0.6,
        max: 2.5,
        step: 0.01,
        hint: "Crop into the centre of the view.",
    },
    SliderSpec {
        key: "centerX",
        label: "Lens centre X",
        min: -0.35,
        max: 0.35,
        step: 0.005,
        hint: "Shift toward the nose / temple. Mirrored for the right eye.",
    },
    SliderSpec {
        key: "centerY",
        label: "Lens centre Y",
        min: -0.35,
        max: 0.35,
        step: 0.005,
        hint: "",
    },
    SliderSpec {
        key: "chromaR",
        label: "Red fringe",
        min: 0.97,
        max: 1.03,
        step: 0.0005,
        hint: "Removes red/cyan colour fringing at the edges.",
    },
    SliderSpec {
        key: "chromaB",
        label: "Blue fringe",
        min: 0.97,
        max: 1.03,
        step: 0.0005,
        hint: "Removes blue/yellow colour fringing at the edges.",
    },
];

pub fn slider_value<'a>(c: &'a mut Correction, key: &str) -> Option<&'a mut f64> {
    Some(match key {
        "k1" => &mut c.k1,
        "k2" => &mut c.k2,
        "zoom" => &mut c.zoom,
        "centerX" => &mut c.center_x,
        "centerY" => &mut c.center_y,
        "chromaR" => &mut c.chroma_r,
        "chromaB" => &mut c.chroma_b,
        _ => return None,
    })
}

pub fn correction_json(c: &Correction) -> Value {
    serde_json::to_value(c).unwrap_or(Value::Null)
}

// ------------------------------------------------------------------ toasts

#[derive(Debug, Clone)]
pub struct Toast {
    pub level: NoticeLevel,
    pub message: String,
    pub until: Instant,
}

/// Status of the app's environment, from the hub bridge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvInfo {
    pub demo: bool,
    pub version: String,
    pub scrcpy_version: String,
    pub server_available: bool,
    pub server_path: Option<String>,
    /// e.g. `adb server + direct USB/TCP`.
    pub adb_mode: String,
    pub decoder: String,
    pub decoder_support: BTreeMap<String, (bool, bool)>,
    pub local_ips: Vec<String>,
    pub store_path: Option<String>,
    /// The hub could not start (shown instead of the dashboard).
    pub fatal: Option<String>,
}

// -------------------------------------------------------------------- model

/// Everything the UI shows.
#[derive(Debug, Default)]
pub struct Model {
    pub headsets: Vec<HeadsetSnapshot>,
    pub adb_error: Option<String>,
    pub streams: HashMap<String, StreamState>,
    pub tiles: Vec<Tile>,
    pub settings: Settings,
    pub env: EnvInfo,
    pub focus: Option<String>,
    pub calib: Option<Calib>,
    pub toasts: Vec<Toast>,
}

impl Model {
    pub fn headset(&self, id: &str) -> Option<&HeadsetSnapshot> {
        self.headsets.iter().find(|h| h.id == id)
    }

    pub fn tile(&self, id: &str) -> Option<&Tile> {
        self.tiles.iter().find(|t| t.id == id)
    }

    pub fn tile_mut(&mut self, id: &str) -> Option<&mut Tile> {
        self.tiles.iter_mut().find(|t| t.id == id)
    }

    pub fn on_devices(&mut self, snap: DeviceSnapshot) {
        self.headsets = snap.headsets;
        self.adb_error = snap.adb_error;
        self.sort_tiles();
    }

    fn sort_tiles(&mut self) {
        let key = |id: &str| {
            let h = self.headsets.iter().find(|h| h.id == id);
            (
                h.and_then(|h| h.index).unwrap_or(999),
                h.map(|h| h.name.to_lowercase()).unwrap_or_default(),
            )
        };
        let mut keyed: Vec<_> = self.tiles.drain(..).map(|t| (key(&t.id), t)).collect();
        keyed.sort_by(|a, b| a.0.cmp(&b.0));
        self.tiles = keyed.into_iter().map(|(_, t)| t).collect();
    }

    pub fn ensure_tile(&mut self, id: &str) -> &mut Tile {
        if self.tile(id).is_none() {
            let mode = self
                .headset(id)
                .and_then(|h| view_mode_from_str(h.view_mode.as_deref()))
                .unwrap_or(self.settings.default_view_mode);
            self.tiles.push(Tile::new(id, mode));
            self.sort_tiles();
        }
        self.tile_mut(id).expect("tile was just added")
    }

    pub fn remove_tile(&mut self, id: &str) {
        if self.calib.as_ref().is_some_and(|c| c.id == id) {
            self.calib = None;
        }
        self.tiles.retain(|t| t.id != id);
        self.streams.remove(id);
        if self.focus.as_deref() == Some(id) {
            self.focus = None;
        }
    }

    /// The user starts a stream: show its tile right away (`startStream`).
    pub fn begin_cast(&mut self, id: &str) {
        self.streams.remove(id);
        let t = self.ensure_tile(id);
        t.local_error = None;
        t.reset_video();
    }

    /// A stream changed state (`onStreamState`). `generation` is the decoder
    /// generation the bridge assigned to the session.
    pub fn on_stream_state(&mut self, st: StreamState, generation: u64) {
        if st.state == StreamStatus::Stopped {
            self.remove_tile(&st.id);
            return;
        }
        let prev = self.streams.get(&st.id).cloned();
        let reset = needs_reset(prev.as_ref(), &st);
        let t = self.ensure_tile(&st.id);
        t.local_error = None;
        match st.state {
            StreamStatus::Resizing => t.resizing = true,
            StreamStatus::Error => t.resizing = false,
            StreamStatus::Reconnecting if st.reason.as_deref() != Some("resize") => {
                t.resizing = false
            }
            _ => {}
        }
        if reset {
            t.reset_video();
        }
        t.generation = t.generation.max(generation);
        self.streams.insert(st.id.clone(), st);
    }

    /// Starting failed before the core reported anything.
    pub fn on_cast_failed(&mut self, id: &str, error: String) {
        if !self.streams.get(id).is_some_and(|s| s.state.is_running())
            && let Some(t) = self.tile_mut(id)
        {
            t.local_error = Some(error);
        }
    }

    pub fn on_decode_fatal(&mut self, id: &str, generation: u64, message: String) {
        if let Some(t) = self.tile_mut(id)
            && generation >= t.generation
        {
            t.fatal = Some(message);
        }
    }

    pub fn set_focus(&mut self, id: Option<&str>) {
        self.focus = id.filter(|id| self.tile(id).is_some()).map(String::from);
    }

    /// Keys 1–9 toggle focus on the headset with that number.
    pub fn focus_index(&mut self, index: u32) {
        let Some(id) = self
            .headsets
            .iter()
            .find(|h| h.index == Some(index))
            .map(|h| h.id.clone())
        else {
            return;
        };
        if self.tile(&id).is_none() {
            return;
        }
        if self.focus.as_deref() == Some(id.as_str()) {
            self.focus = None;
        } else {
            self.focus = Some(id);
        }
    }

    pub fn visible_tiles(&self) -> Vec<&Tile> {
        match &self.focus {
            Some(f) => self.tiles.iter().filter(|t| &t.id == f).collect(),
            None => self.tiles.iter().collect(),
        }
    }

    pub fn inputs<'a>(&'a self, tile: &'a Tile) -> TileInputs<'a> {
        TileInputs {
            headset: self.headset(&tile.id),
            state: self.streams.get(&tile.id),
            tile,
            calib: self.calib.as_ref().filter(|c| c.id == tile.id),
        }
    }

    pub fn open_calibration(&mut self, id: &str) {
        let Some(h) = self.headset(id).cloned() else {
            return;
        };
        let mut params = correction_for(Some(&h));
        if let Some(t) = self.tile_mut(id) {
            if t.view_mode == ViewMode::Raw {
                t.view_mode = ViewMode::Corrected;
            }
            if let Some(e) = t.eye_override {
                params.eye = e;
            }
            if let Some(r) = t.rotation_override {
                params.rotation = Rotation::Degrees(r);
            }
        }
        self.calib = Some(Calib {
            id: id.to_string(),
            original: params.clone(),
            params,
            grid: true,
        });
        self.set_focus(Some(id));
    }

    /// Close the drawer. Returns the draft when it should be saved.
    pub fn close_calibration(&mut self, save: bool) -> Option<Calib> {
        let c = self.calib.take()?;
        if let Some(t) = self.tile_mut(&c.id) {
            t.eye_override = Some(c.params.eye);
            if save {
                t.rotation_override = None;
            }
        }
        save.then_some(c)
    }

    pub fn toast(&mut self, level: NoticeLevel, message: impl Into<String>, now: Instant) {
        let message = message.into();
        let ms = match level {
            NoticeLevel::Error => 8000,
            NoticeLevel::Warn => 6000,
            _ => 5000,
        };
        // The same message again just extends the existing toast.
        if let Some(t) = self.toasts.iter_mut().find(|t| t.message == message) {
            t.until = now + Duration::from_millis(ms);
            return;
        }
        self.toasts.push(Toast {
            level,
            message,
            until: now + Duration::from_millis(ms),
        });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    pub fn expire_toasts(&mut self, now: Instant) -> Option<Instant> {
        self.toasts.retain(|t| t.until > now);
        self.toasts.iter().map(|t| t.until).min()
    }

    pub fn castable_not_streaming(&self) -> Vec<String> {
        self.headsets
            .iter()
            .filter(|h| !h.pending && h.can_cast && h.is_online())
            .filter(|h| {
                !self
                    .streams
                    .get(&h.id)
                    .is_some_and(|s| s.state.is_running())
                    && self.tile(&h.id).is_none()
            })
            .map(|h| h.id.clone())
            .collect()
    }
}

/// Toast text for a network scan (`doScan`).
pub fn scan_summary(r: &xrmc_core::device_manager::ScanResult) -> (String, NoticeLevel) {
    let plural = |n: usize, s: &str| format!("{n} {s}{}", if n == 1 { "" } else { "s" });
    let mut parts = vec![format!(
        "Found {} with ADB open",
        plural(r.probed, "device")
    )];
    if r.connected > 0 {
        parts.push(plural(r.connected, "new connection"));
    }
    if r.unverified > 0 {
        parts.push(format!(
            "{} unverified (connect via USB to verify)",
            r.unverified
        ));
    }
    if r.pending_auth > 0 {
        parts.push(format!(
            "{} waiting for permission in the headset",
            r.pending_auth
        ));
    }
    (
        parts.join(" · "),
        if r.connected > 0 {
            NoticeLevel::Ok
        } else {
            NoticeLevel::Info
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use xrmc_core::TransportPreference;

    pub fn headset(id: &str, index: u32) -> HeadsetSnapshot {
        HeadsetSnapshot {
            id: id.into(),
            serial: Some(id.into()),
            adb_serial: None,
            pending: false,
            network: None,
            trusted: true,
            index: Some(index),
            name: format!("Station {index}"),
            color: "#F97316".into(),
            brand: "meta".into(),
            model: "Quest 3".into(),
            model_raw: None,
            variant: None,
            manufacturer: None,
            android_version: None,
            usb: Some(id.into()),
            wifi: None,
            ip: Some("192.168.1.50".into()),
            battery: Some(Battery {
                level: 80,
                charging: false,
            }),
            status: "online".into(),
            can_cast: true,
            calibration: None,
            transport: TransportPreference::Auto,
            view_mode: None,
            verified_at: None,
            last_seen: None,
            error: None,
        }
    }

    pub fn state(id: &str, s: StreamStatus) -> StreamState {
        StreamState {
            id: id.into(),
            state: s,
            transport: Some(xrmc_core::TransportKind::Usb),
            adb_serial: None,
            width: None,
            height: None,
            device_name: None,
            quality: Some("Auto 1280px".into()),
            bit_rate: None,
            codec: Some(Codec::H265),
            ten_bit: false,
            encoder: Some("c2.qti.hevc.encoder".into()),
            single_eye: false,
            retries: 0,
            reason: None,
            size_long: Some(1280),
            auto_size: true,
            error: None,
        }
    }

    #[test]
    fn cards_follow_the_electron_rules() {
        let h = headset("A", 1);
        let Card::Headset(c) = card_for(&h, None, 0) else {
            panic!()
        };
        assert_eq!(c.primary, Primary::Cast);
        assert_eq!(c.brand_name, "Meta");
        assert!(c.usb && c.wifi.is_none() && c.enable_wifi && c.menu);
        assert_eq!(c.battery.unwrap().1, BatteryClass::Normal);

        let live = state("A", StreamStatus::Live);
        let Card::Headset(c) = card_for(&h, Some(&live), 0) else {
            panic!()
        };
        assert_eq!(c.primary, Primary::Stop);
        assert!(c.live && c.streaming);

        let mut u = headset("B", 2);
        u.trusted = false;
        u.index = None;
        let Card::Headset(c) = card_for(&u, None, 0) else {
            panic!()
        };
        assert_eq!(c.primary, Primary::Verify);
        assert!(!c.menu && !c.enable_wifi);

        u.usb = None;
        u.wifi = Some("192.168.1.51:5555".into());
        u.ip = Some("192.168.1.51".into());
        let Card::Headset(c) = card_for(&u, None, 0) else {
            panic!()
        };
        assert_eq!(
            c.primary,
            Primary::ConnectUsbToVerify {
                can_cast: true,
                streaming: false
            }
        );
        assert_eq!(c.wifi.as_deref(), Some("192.168.1.51"));

        let mut off = headset("C", 3);
        off.status = "offline".into();
        off.last_seen = Some(1_000);
        off.battery = Some(Battery {
            level: 10,
            charging: false,
        });
        let Card::Headset(c) = card_for(&off, None, 1_000 + 5 * 60_000) else {
            panic!()
        };
        assert_eq!(
            c.primary,
            Primary::Offline("Offline · seen 5 min ago".into())
        );
        assert_eq!(c.battery.unwrap().1, BatteryClass::Low);
    }

    #[test]
    fn pending_cards_explain_what_to_do() {
        let mut p = headset("pending:x", 0);
        p.pending = true;
        p.status = "unauthorized".into();
        p.name = String::new();
        p.brand = "android".into();
        let Card::Pending {
            title,
            hint,
            warn,
            subtitle,
            ..
        } = card_for(&p, None, 0)
        else {
            panic!()
        };
        assert_eq!(title, "New device");
        assert!(hint.contains("Allow USB debugging"));
        assert!(warn);
        assert_eq!(subtitle, "Other Android device · USB");
        p.status = "identifying".into();
        let Card::Pending { spinner, .. } = card_for(&p, None, 0) else {
            panic!()
        };
        assert!(spinner);
    }

    #[test]
    fn sections_and_empty_state() {
        let mut list = vec![headset("A", 1), headset("B", 2)];
        list[1].status = "offline".into();
        let s = sidebar_sections(&list);
        assert_eq!(
            s.iter().map(|x| x.0).collect::<Vec<_>>(),
            ["Connected", "Saved · offline"]
        );
        assert_eq!(empty_title(&[]), "Connect your first headset");
        assert_eq!(
            empty_title(&list),
            "2 headsets ready — tap Cast or Cast all"
        );
        list.iter_mut().for_each(|h| h.can_cast = false);
        assert_eq!(empty_title(&list), "Verify a headset to start casting");
    }

    #[test]
    fn tile_status_text_follows_the_state() {
        let mut t = Tile::new("A", ViewMode::Corrected);
        assert_eq!(tile_status(None, &t).unwrap().title, "Starting stream…");
        let s = state("A", StreamStatus::Starting);
        let st = tile_status(Some(&s), &t).unwrap();
        assert_eq!(
            (st.title.as_str(), st.detail.as_str(), st.spinner),
            ("Starting stream…", "via USB", true)
        );
        let live = state("A", StreamStatus::Live);
        assert_eq!(
            tile_status(Some(&live), &t).unwrap().title,
            "Waiting for video…"
        );
        t.on_frame(0, 1920, 1024, 8);
        assert!(tile_status(Some(&live), &t).is_none());

        let mut r = state("A", StreamStatus::Reconnecting);
        r.retries = 2;
        r.error = Some("connection reset".into());
        let st = tile_status(Some(&r), &t).unwrap();
        assert_eq!(st.title, "Reconnecting… (attempt 2)");
        assert_eq!(st.detail, "connection reset");

        let mut e = state("A", StreamStatus::Error);
        e.error = Some("boom".into());
        let st = tile_status(Some(&e), &t).unwrap();
        assert!(st.actions && st.title == "Stream stopped" && st.detail == "boom");

        t.fatal = Some("no decoder available for H265".into());
        assert_eq!(
            tile_status(Some(&live), &t).unwrap().title,
            "Cannot display video"
        );

        let mut t2 = Tile::new("B", ViewMode::Corrected);
        t2.local_error = Some("Headset is not connected.".into());
        assert!(tile_status(None, &t2).unwrap().actions);
    }

    #[test]
    fn resizing_shows_the_blurred_frame_until_the_new_session_has_video() {
        let mut m = Model {
            headsets: vec![headset("A", 1)],
            ..Default::default()
        };
        m.on_stream_state(state("A", StreamStatus::Starting), 1);
        m.on_stream_state(state("A", StreamStatus::Live), 1);
        m.tile_mut("A").unwrap().on_frame(1, 1920, 1024, 8);
        assert!(m.tile("A").unwrap().has_video);

        let mut rz = state("A", StreamStatus::Resizing);
        rz.reason = Some("resize".into());
        rz.size_long = Some(960);
        m.on_stream_state(rz.clone(), 1);
        let st = tile_status(m.streams.get("A"), m.tile("A").unwrap()).unwrap();
        assert!(st.resizing);
        assert_eq!(
            (st.title.as_str(), st.detail.as_str()),
            ("Changing resolution", "960 px")
        );
        let r = tile_params(&m.inputs(m.tile("A").unwrap()));
        assert!(r.params.blur);

        let mut starting = state("A", StreamStatus::Starting);
        starting.reason = Some("resize".into());
        m.on_stream_state(starting, 2);
        let t = m.tile("A").unwrap();
        assert!(
            !t.has_video && t.resizing,
            "still blurred while the new session starts"
        );
        // A late frame of the old session must not end the blur.
        m.tile_mut("A").unwrap().on_frame(1, 1920, 1024, 8);
        assert!(m.tile("A").unwrap().resizing);
        m.on_stream_state(state("A", StreamStatus::Live), 2);
        m.tile_mut("A").unwrap().on_frame(2, 960, 512, 8);
        let t = m.tile("A").unwrap();
        assert!(t.has_video && !t.resizing);
        assert_eq!(t.frame_size, Some((960, 512)));

        m.on_stream_state(state("A", StreamStatus::Stopped), 2);
        assert!(m.tile("A").is_none());
    }

    #[test]
    fn reset_rules() {
        let live = state("A", StreamStatus::Live);
        let starting = state("A", StreamStatus::Starting);
        assert!(needs_reset(Some(&live), &starting));
        assert!(!needs_reset(None, &starting));
        assert!(!needs_reset(Some(&starting), &starting));
        let mut h264 = starting.clone();
        h264.codec = Some(Codec::H264);
        assert!(needs_reset(Some(&starting), &h264));
        assert!(!needs_reset(Some(&starting), &live));
    }

    #[test]
    fn tile_params_resolve_presets_calibration_and_detection() {
        let mut h = headset("A", 1);
        let mut t = Tile::new("A", ViewMode::Corrected);
        t.frame_size = Some((1920, 1024));
        let m = Model {
            headsets: vec![h.clone()],
            ..Default::default()
        };
        let r = tile_params(&m.inputs(&t));
        // Meta preset: wide frame → side by side, auto rotation 0.
        assert_eq!(r.params.layout, xrmc_render::Layout::Sbs);
        assert_eq!(r.params.rotation, 0);
        assert!((r.params.lens.k1 - -0.22).abs() < 1e-6);
        assert_eq!(r.params.lens.output_aspect, Some(16.0 / 9.0));
        assert!(r.wants_detection);
        assert!(!r.view.detected);

        // A detected vertical stack in a portrait frame turns sideways.
        t.frame_size = Some((1024, 1920));
        t.grid = Some(GridShape { cols: 1, rows: 2 });
        let r = tile_params(&m.inputs(&t));
        assert_eq!(r.params.layout, xrmc_render::Layout::Sbs);
        assert_eq!(r.params.rotation, 90);
        assert!(r.view.detected);

        // Saved calibration overrides the preset; bad values are ignored.
        h.calibration = Some(
            json!({ "k1": -0.3, "zoom": "x", "eye": "right", "layout": "quad", "quadRow": "bottom", "outputAspect": "eye", "flipH": true, "rotation": 180 })
                .as_object()
                .cloned()
                .unwrap(),
        );
        let m = Model {
            headsets: vec![h.clone()],
            ..Default::default()
        };
        let r = tile_params(&m.inputs(&t));
        assert!((r.params.lens.k1 - -0.3).abs() < 1e-6);
        assert!((r.params.lens.zoom - 1.45).abs() < 1e-6);
        assert_eq!(r.params.eye, xrmc_render::Eye::Right);
        assert_eq!(
            r.params.layout,
            xrmc_render::Layout::Quad { bottom_row: true }
        );
        assert_eq!(r.params.lens.output_aspect, None);
        assert_eq!(r.params.rotation, 180);
        assert!(r.params.flip_h);
        assert!(!r.wants_detection);

        // Old calibrations stored rotation 0 without flipH: that means "auto".
        h.calibration = Some(json!({ "rotation": 0 }).as_object().cloned().unwrap());
        assert_eq!(correction_for(Some(&h)).rotation, Rotation::Auto);

        // Overrides from the tile (eye / rotate button).
        h.calibration = None;
        let m = Model {
            headsets: vec![h],
            ..Default::default()
        };
        t.eye_override = Some(Eye::Right);
        t.rotation_override = Some(270.0);
        t.grid = None;
        t.frame_size = Some((1920, 1024));
        let r = tile_params(&m.inputs(&t));
        assert_eq!(r.params.eye, xrmc_render::Eye::Right);
        assert_eq!(r.params.rotation, 270);
    }

    #[test]
    fn calibration_draft_overrides_and_saves() {
        let mut m = Model {
            headsets: vec![headset("A", 1)],
            ..Default::default()
        };
        m.ensure_tile("A").view_mode = ViewMode::Raw;
        m.open_calibration("A");
        assert_eq!(m.tile("A").unwrap().view_mode, ViewMode::Corrected);
        assert_eq!(m.focus.as_deref(), Some("A"));
        m.calib.as_mut().unwrap().params.k1 = -0.4;
        let r = tile_params(&m.inputs(m.tile("A").unwrap()));
        assert!((r.params.lens.k1 - -0.4).abs() < 1e-6);
        assert!(r.params.grid);
        let saved = m.close_calibration(true).unwrap();
        assert!((saved.params.k1 - -0.4).abs() < 1e-9);
        assert_eq!(correction_json(&saved.params)["k1"], json!(-0.4));
        assert!(m.calib.is_none());
    }

    #[test]
    fn stats_line_matches_the_electron_format() {
        let mut t = Tile::new("A", ViewMode::Corrected);
        let st = state("A", StreamStatus::Live);
        let r = resolve_view(&preset_for(None), &t, false);
        assert_eq!(stats_line(Some(&st), &t, &r), "");
        t.on_frame(0, 1920, 1024, 10);
        t.fps = 59.6;
        t.mbps = 8.04;
        t.grid = Some(GridShape { cols: 2, rows: 1 });
        let h = headset("A", 1);
        let r = resolve_view(&correction_for(Some(&h)), &t, false);
        assert_eq!(
            stats_line(Some(&st), &t, &r),
            "H.265 10-bit · 1920×1024 · 2×1 auto · 60 fps · 8.0 Mbps"
        );
        assert!(stats_tooltip(Some(&st), &r).contains("Encoder: c2.qti.hevc.encoder"));
    }

    #[test]
    fn stats_sampling() {
        let mut t = Tile::new("A", ViewMode::Corrected);
        let t0 = Instant::now();
        assert!(!t.sample_stats(t0, 0, 0));
        assert!(!t.sample_stats(t0 + Duration::from_millis(500), 10, 1000));
        assert!(t.sample_stats(t0 + Duration::from_secs(2), 120, 2_000_000));
        assert!((t.fps - 60.0).abs() < 0.01);
        assert!((t.mbps - 8.0).abs() < 0.01);
    }

    #[test]
    fn grid_columns_and_rects() {
        // Landscape: the Electron rule.
        let land = (1600.0, 900.0);
        assert_eq!(grid_columns("auto", 1, false, land, 90.0), 1);
        assert_eq!(grid_columns("auto", 4, false, land, 90.0), 2);
        assert_eq!(grid_columns("auto", 9, false, land, 90.0), 3);
        assert_eq!(grid_columns("auto", 12, false, land, 90.0), 4);
        assert_eq!(grid_columns("3", 9, true, land, 90.0), 1);
        assert_eq!(grid_columns("3", 2, false, land, 90.0), 2);
        // Portrait touchscreen: stacked when that shows more video.
        assert_eq!(grid_columns("auto", 2, false, (790.0, 1150.0), 90.0), 1);
        assert_eq!(grid_columns("auto", 9, false, (790.0, 1150.0), 90.0), 2);

        let (r, h) = grid_rects(1000.0, 600.0, 4, 2, 10.0);
        assert_eq!(r.len(), 4);
        assert_eq!(r[3], [505.0, 305.0, 495.0, 295.0]);
        assert_eq!(h, 600.0);
        // Too many rows: tiles keep a minimum height and the grid scrolls.
        let (r, h) = grid_rects(1000.0, 300.0, 12, 4, 10.0);
        assert_eq!(r[11][3], MIN_TILE_H);
        assert!(h > 300.0);
    }

    #[test]
    fn view_requests_round_to_8px() {
        let m = Model {
            headsets: vec![headset("A", 1)],
            ..Default::default()
        };
        let mut t = Tile::new("A", ViewMode::Corrected);
        t.frame_size = Some((1920, 1024));
        let r = tile_params(&m.inputs(&t));
        let q = view_request(true, (641.3, 359.0), ViewMode::Corrected, &r);
        assert_eq!((q.tile_w, q.tile_h), (640.0, 360.0));
        assert_eq!(q.output_aspect.as_deref(), Some("16:9"));
        assert_eq!(q.layout, Layout::Sbs);
        assert!((q.zoom - 1.45).abs() < 1e-9);
        let hidden = view_request(false, (641.3, 359.0), ViewMode::Raw, &r);
        assert!(!hidden.visible);
        assert_eq!(hidden.mode, ViewMode::Raw);
    }

    #[test]
    fn focus_keys_and_tile_order() {
        let mut m = Model {
            headsets: vec![headset("B", 2), headset("A", 1)],
            ..Default::default()
        };
        m.begin_cast("B");
        m.begin_cast("A");
        assert_eq!(
            m.tiles.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["A", "B"]
        );
        m.focus_index(2);
        assert_eq!(m.focus.as_deref(), Some("B"));
        assert_eq!(m.visible_tiles().len(), 1);
        m.focus_index(2);
        assert!(m.focus.is_none());
        m.focus_index(7);
        assert!(m.focus.is_none());
        assert!(m.castable_not_streaming().is_empty());
        m.remove_tile("A");
        assert_eq!(m.castable_not_streaming(), ["A"]);
    }

    #[test]
    fn toasts_dedupe_and_expire() {
        let mut m = Model::default();
        let t0 = Instant::now();
        m.toast(NoticeLevel::Info, "hello", t0);
        m.toast(NoticeLevel::Info, "hello", t0 + Duration::from_secs(1));
        assert_eq!(m.toasts.len(), 1);
        assert!(m.expire_toasts(t0 + Duration::from_secs(5)).is_some());
        assert!(m.expire_toasts(t0 + Duration::from_secs(7)).is_none());
        assert!(m.toasts.is_empty());
    }

    #[test]
    fn brand_and_model_without_repetition() {
        assert_eq!(brand_model("PICO", "PICO 4 Ultra"), "PICO 4 Ultra");
        assert_eq!(
            brand_model("HTC VIVE", "VIVE Focus Vision"),
            "HTC VIVE Focus Vision"
        );
        assert_eq!(brand_model("Meta", "Quest 3"), "Meta Quest 3");
        assert_eq!(brand_model("Android", ""), "Android");
    }

    #[test]
    fn scan_summaries() {
        let r = xrmc_core::device_manager::ScanResult {
            probed: 1,
            connected: 2,
            unverified: 1,
            pending_auth: 0,
        };
        let (s, l) = scan_summary(&r);
        assert_eq!(
            s,
            "Found 1 device with ADB open · 2 new connections · 1 unverified (connect via USB to verify)"
        );
        assert_eq!(l, NoticeLevel::Ok);
    }
}
