//! Stream resolution matched to the size a stream is actually shown at
//! (port of `src/shared/view-size.js`).
//!
//! Viewers (the hub window and remote tablets) report, per stream, how each
//! tile is displayed. From that and the headset's panel size we compute the
//! encoded resolution needed for ~1 source pixel per screen pixel, then round
//! UP to a fixed step so small resizes don't cause restarts.

use serde::{Deserialize, Serialize};

use crate::layout::Layout;

/// Long-side resolution steps; the panel's native size is always an extra final step.
pub const RES_STEPS: [u32; 8] = [640, 960, 1280, 1600, 1920, 2560, 3200, 3840];

/// Minimum bitrate [`scaled_bitrate`] goes down to.
pub const DEFAULT_BITRATE_FLOOR: u32 = 1_500_000;

/// Frame size in pixels (natural orientation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameSize {
    pub w: u32,
    pub h: u32,
}

impl FrameSize {
    pub fn long_side(&self) -> u32 {
        self.w.max(self.h)
    }
}

/// How a viewer shows a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    /// Both eyes side by side, lens distortion removed.
    Stereo,
    /// The unprocessed panel image.
    Raw,
    /// Single eye, lens distortion removed. Unknown strings read as `Corrected`.
    #[default]
    #[serde(other)]
    Corrected,
}

/// One viewer's requirement for one stream (`{visible, tileW, tileH, mode, zoom, outputAspect, layout, rotation}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ViewRequest {
    pub visible: bool,
    /// Tile width in device pixels.
    pub tile_w: f64,
    /// Tile height in device pixels.
    pub tile_h: f64,
    pub mode: ViewMode,
    pub zoom: f64,
    /// `W:H` or `eye` (use the eye's own aspect).
    pub output_aspect: Option<String>,
    pub layout: Layout,
    /// Clockwise degrees.
    pub rotation: f64,
}

impl Default for ViewRequest {
    fn default() -> Self {
        ViewRequest {
            visible: true,
            tile_w: 0.0,
            tile_h: 0.0,
            mode: ViewMode::Corrected,
            zoom: 1.0,
            output_aspect: None,
            layout: Layout::Mono,
            rotation: 0.0,
        }
    }
}

impl ViewRequest {
    /// Clamp untrusted input the way the Electron hub does for remote viewers
    /// (`checkViewSizes` in main.js).
    pub fn sanitized(&self) -> ViewRequest {
        let num = |v: f64, max: f64| {
            if v.is_finite() {
                v.clamp(0.0, max)
            } else {
                0.0
            }
        };
        let aspect_ok = |s: &str| s == "eye" || is_aspect(s, 3);
        let zoom = num(self.zoom, 10.0);
        ViewRequest {
            visible: self.visible,
            tile_w: num(self.tile_w, 16384.0),
            tile_h: num(self.tile_h, 16384.0),
            mode: self.mode,
            zoom: if zoom == 0.0 { 1.0 } else { zoom },
            output_aspect: Some(
                self.output_aspect
                    .as_deref()
                    .filter(|s| aspect_ok(s))
                    .unwrap_or("eye")
                    .to_string(),
            ),
            layout: self.layout,
            rotation: if [0.0, 90.0, 180.0, 270.0].contains(&self.rotation) {
                self.rotation
            } else {
                0.0
            },
        }
    }
}

/// `\d+(\.\d+)?`, with at most `max_int_digits` integer digits (0 = unlimited).
fn is_decimal(s: &str, max_int_digits: usize) -> bool {
    let (int, frac) = match s.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (s, None),
    };
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    digits(int) && (max_int_digits == 0 || int.len() <= max_int_digits) && frac.is_none_or(digits)
}

fn is_aspect(s: &str, max_int_digits: usize) -> bool {
    s.split_once(':')
        .is_some_and(|(a, b)| is_decimal(a, max_int_digits) && is_decimal(b, max_int_digits))
}

/// `"16:9"` → 16/9; anything else → `fallback`.
fn parse_aspect(v: Option<&str>, fallback: f64) -> f64 {
    match v {
        Some(s) if is_aspect(s, 0) => {
            let (a, b) = s.split_once(':').unwrap_or_default();
            match (a.parse::<f64>(), b.parse::<f64>()) {
                (Ok(a), Ok(b)) => a / b,
                _ => fallback,
            }
        }
        _ => fallback,
    }
}

/// Height of a box with aspect `a` fitted inside w×h.
fn fit_height(a: f64, w: f64, h: f64) -> f64 {
    h.min(w / a)
}

/// Fraction of the frame one eye takes up, as (width, height).
fn eye_rect(layout: Layout) -> (f64, f64) {
    match layout {
        Layout::Sbs => (0.5, 1.0),
        Layout::Tb => (1.0, 0.5),
        Layout::Quad => (0.5, 0.5),
        Layout::Mono => (1.0, 1.0),
    }
}

/// Encoded long-side pixels needed to show one stream in one tile at ~1:1 (0 when nothing is visible).
pub fn needed_long_side(frame: FrameSize, req: &ViewRequest) -> f64 {
    if !req.visible || frame.w == 0 || frame.h == 0 {
        return 0.0;
    }
    let tile_w = if req.tile_w.is_nan() {
        0.0
    } else {
        req.tile_w.max(0.0)
    };
    let tile_h = if req.tile_h.is_nan() {
        0.0
    } else {
        req.tile_h.max(0.0)
    };
    if tile_w == 0.0 || tile_h == 0.0 {
        return 0.0;
    }
    let (fw, fh) = (f64::from(frame.w), f64::from(frame.h));
    let long = fw.max(fh);
    let rotation = if req.rotation.is_finite() {
        req.rotation
    } else {
        0.0
    };
    let turns = crate::js::round(rotation / 90.0);
    let sideways = (((turns % 4.0) + 4.0) % 4.0) % 2.0 == 1.0;
    let (lw, lh) = if sideways { (fh, fw) } else { (fw, fh) };

    if req.mode == ViewMode::Raw {
        return long * (tile_w / lw).min(tile_h / lh);
    }

    let (ew, eh) = eye_rect(req.layout);
    let eye_aspect = (lw * ew) / (lh * eh);
    let out_aspect = parse_aspect(req.output_aspect.as_deref(), eye_aspect);
    let zoom = if req.zoom.is_nan() || req.zoom == 0.0 {
        1.0
    } else {
        req.zoom
    }
    .max(0.1);
    // Stereo draws two eyes side by side, each at most square (see lens-renderer.js).
    let stereo = req.mode == ViewMode::Stereo && req.layout != Layout::Mono;
    let vp_h = if stereo {
        fit_height(out_aspect.min(1.0) * 2.0, tile_w, tile_h)
    } else {
        fit_height(out_aspect, tile_w, tile_h)
    };
    // The corrected view shows eyeHeight/zoom of the eye across the viewport height (1:1 at the lens centre).
    let needed_eye_h = vp_h * zoom;
    let needed_lh = needed_eye_h / eh;
    long * (needed_lh / lh)
}

/// The step ladder for a panel: [`RES_STEPS`] below the native size, then the native size.
fn steps_for(native_long: u32) -> Vec<u32> {
    let mut steps: Vec<u32> = RES_STEPS
        .iter()
        .copied()
        .filter(|&s| native_long == 0 || s < native_long)
        .collect();
    if native_long > 0 {
        steps.push(native_long);
    }
    steps
}

/// Smallest step at or above `need`, capped by the quality preset and the panel's native size.
///
/// `cap` is the preset's max long side and `native_long` the panel's long side
/// (0 = none / unknown). A result equal to `native_long` means "native" (scrcpy `max_size=0`).
pub fn pick_step(need: f64, cap: u32, native_long: u32) -> u32 {
    let native = if native_long > 0 {
        f64::from(native_long)
    } else {
        f64::INFINITY
    };
    let ceiling = if cap > 0 {
        f64::from(cap)
    } else {
        f64::INFINITY
    }
    .min(native);
    let steps = steps_for(native_long);
    let last = steps
        .last()
        .copied()
        .unwrap_or(RES_STEPS[RES_STEPS.len() - 1]);
    let mut chosen = f64::from(
        steps
            .iter()
            .copied()
            .find(|&s| f64::from(s) >= need)
            .unwrap_or(last),
    );
    if chosen > ceiling {
        chosen = ceiling;
    }
    chosen.max(f64::from(RES_STEPS[0]).min(ceiling)) as u32
}

/// Index on the step ladder (for "how many steps apart" decisions).
pub fn step_index(size: u32, native_long: u32) -> usize {
    let steps = steps_for(native_long);
    steps
        .iter()
        .position(|&s| s >= size)
        .unwrap_or(steps.len().saturating_sub(1))
}

/// Bitrate for a reduced resolution: scales with pixel count^0.75 relative to
/// the preset's cap, with a floor of 1.5 Mbps so small tiles still look clean.
pub fn scaled_bitrate(base: u32, size: u32, cap_long: u32) -> u32 {
    scaled_bitrate_with_floor(base, size, cap_long, DEFAULT_BITRATE_FLOOR)
}

/// [`scaled_bitrate`] with an explicit floor.
pub fn scaled_bitrate_with_floor(base: u32, size: u32, cap_long: u32, floor: u32) -> u32 {
    if cap_long == 0 || size == 0 || size >= cap_long {
        return base;
    }
    let b = f64::from(base);
    let scaled = b * (f64::from(size) / f64::from(cap_long)).powf(1.5);
    crate::js::round(b.min(f64::from(floor).min(b).max(scaled))) as u32
}

/// Input of [`decide_resize`]. Times are milliseconds on any monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeQuery {
    /// Current encoded long side.
    pub current: u32,
    /// Wanted encoded long side.
    pub target: u32,
    pub native_long: u32,
    pub now: u64,
    /// When the stream first became eligible for a downgrade.
    pub down_since: Option<u64>,
    /// No viewer shows the stream at the moment.
    pub hidden_only: bool,
    pub down_delay: u64,
    pub hidden_delay: u64,
}

impl Default for ResizeQuery {
    fn default() -> Self {
        ResizeQuery {
            current: 0,
            target: 0,
            native_long: 0,
            now: 0,
            down_since: None,
            hidden_only: false,
            down_delay: 5000,
            hidden_delay: 15000,
        }
    }
}

/// What to do with a running stream (`{action: 'none'|'resize'|'wait', waitMs?}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum ResizeDecision {
    None,
    Resize,
    #[serde(rename_all = "camelCase")]
    Wait {
        wait_ms: u64,
    },
}

/// Whether a running stream should be restarted at a new size.
///
/// Upgrades apply immediately; downgrades only when at least 2 steps lower and
/// persistent for `down_delay` ms (`hidden_delay` when no viewer shows the stream).
pub fn decide_resize(q: &ResizeQuery) -> ResizeDecision {
    let ci = step_index(q.current, q.native_long);
    let ti = step_index(q.target, q.native_long);
    if ti > ci {
        return ResizeDecision::Resize;
    }
    if ci - ti < 2 {
        return ResizeDecision::None;
    }
    let delay = if q.hidden_only {
        q.hidden_delay
    } else {
        q.down_delay
    };
    let Some(since) = q.down_since else {
        return ResizeDecision::Wait { wait_ms: delay };
    };
    let left = since as i128 + delay as i128 - q.now as i128;
    if left <= 0 {
        ResizeDecision::Resize
    } else {
        ResizeDecision::Wait {
            wait_ms: left as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of `test/view-size.test.js`.
    use super::*;

    const QUEST3: FrameSize = FrameSize { w: 4128, h: 2208 }; // side-by-side stereo panel
    const PFD: FrameSize = FrameSize { w: 2856, h: 3072 }; // 2×2 grid, shown rotated 90°

    fn req(
        tile_w: f64,
        tile_h: f64,
        mode: ViewMode,
        zoom: f64,
        aspect: Option<&str>,
        layout: Layout,
        rotation: f64,
    ) -> ViewRequest {
        ViewRequest {
            visible: true,
            tile_w,
            tile_h,
            mode,
            zoom,
            output_aspect: aspect.map(String::from),
            layout,
            rotation,
        }
    }

    #[test]
    fn corrected_single_eye_in_a_620x349_tile_needs_about_960px_on_a_quest_3() {
        let need = needed_long_side(
            QUEST3,
            &req(
                620.0,
                349.0,
                ViewMode::Corrected,
                1.45,
                Some("16:9"),
                Layout::Sbs,
                0.0,
            ),
        );
        assert!(need > 900.0 && need < 960.0, "{need}");
        assert_eq!(pick_step(need, 2560, 4128), 960);
    }

    #[test]
    fn pfd_mr_quad_layout_rotated_90_needs_a_higher_step() {
        // Each eye is a quarter of the frame.
        let need = needed_long_side(
            PFD,
            &req(
                620.0,
                349.0,
                ViewMode::Corrected,
                1.4,
                Some("16:9"),
                Layout::Quad,
                90.0,
            ),
        );
        assert!(need > 1000.0 && need < 1100.0, "{need}");
        assert_eq!(pick_step(need, 2560, 3072), 1280);
    }

    #[test]
    fn raw_view_fits_the_whole_frame_into_the_tile() {
        let r = |w, h, rot| {
            needed_long_side(
                QUEST3,
                &req(w, h, ViewMode::Raw, 1.0, None, Layout::Sbs, rot),
            )
            .round()
        };
        assert_eq!(r(1000.0, 1000.0, 0.0), 1000.0);
        // Rotated: the long side becomes vertical, so the tile height limits it.
        assert_eq!(r(1000.0, 500.0, 90.0), 500.0);
    }

    #[test]
    fn stereo_view_needs_fewer_pixels_per_eye_than_single_eye_corrected_in_the_same_tile() {
        let stereo = needed_long_side(
            QUEST3,
            &req(
                1200.0,
                400.0,
                ViewMode::Stereo,
                1.4,
                Some("16:9"),
                Layout::Sbs,
                0.0,
            ),
        );
        let single = needed_long_side(
            QUEST3,
            &req(
                1200.0,
                400.0,
                ViewMode::Corrected,
                1.4,
                Some("16:9"),
                Layout::Sbs,
                0.0,
            ),
        );
        assert!(stereo <= single);
    }

    #[test]
    fn hidden_or_empty_tiles_need_nothing() {
        let hidden = ViewRequest {
            visible: false,
            tile_w: 800.0,
            tile_h: 600.0,
            mode: ViewMode::Raw,
            ..Default::default()
        };
        assert_eq!(needed_long_side(QUEST3, &hidden), 0.0);
        assert_eq!(
            needed_long_side(
                QUEST3,
                &req(0.0, 600.0, ViewMode::Raw, 1.0, None, Layout::Mono, 0.0)
            ),
            0.0
        );
        assert_eq!(pick_step(0.0, 1920, 4128), RES_STEPS[0]);
    }

    #[test]
    fn between_two_steps_the_higher_one_is_chosen_capped_by_preset_and_native_size() {
        assert_eq!(pick_step(961.0, 2560, 4128), 1280);
        assert_eq!(pick_step(1280.0, 2560, 4128), 1280);
        assert_eq!(pick_step(3000.0, 1920, 4128), 1920);
        assert_eq!(pick_step(3000.0, 0, 3072), 3072, "native is the last step");
        assert_eq!(pick_step(5000.0, 0, 4128), 4128);
    }

    #[test]
    fn step_indexes_include_the_native_size() {
        assert_eq!(step_index(640, 3072), 0);
        assert_eq!(
            step_index(3072, 3072),
            RES_STEPS.iter().filter(|&&s| s < 3072).count()
        );
    }

    #[test]
    fn bitrate_scales_down_with_resolution_but_keeps_a_floor() {
        assert_eq!(scaled_bitrate(8_000_000, 1920, 1920), 8_000_000);
        let half = scaled_bitrate(8_000_000, 960, 1920);
        assert!(half > 2_500_000 && half < 3_000_000, "{half}");
        assert_eq!(scaled_bitrate(8_000_000, 640, 2560), 1_500_000);
        assert_eq!(
            scaled_bitrate(1_000_000, 640, 2560),
            1_000_000,
            "never above the preset"
        );
    }

    #[test]
    fn resize_decisions_up_immediately_down_only_after_2_steps_and_a_delay() {
        let n = 4128;
        let q = |current, target, now, down_since, hidden_only| {
            decide_resize(&ResizeQuery {
                current,
                target,
                native_long: n,
                now,
                down_since,
                hidden_only,
                ..Default::default()
            })
        };
        assert_eq!(q(960, 1280, 0, None, false), ResizeDecision::Resize);
        assert_eq!(
            q(1280, 960, 0, None, false),
            ResizeDecision::None,
            "one step lower is kept"
        );
        assert_eq!(
            q(1920, 960, 0, None, false),
            ResizeDecision::Wait { wait_ms: 5000 }
        );
        assert_eq!(
            q(1920, 960, 3000, Some(0), false),
            ResizeDecision::Wait { wait_ms: 2000 }
        );
        assert_eq!(q(1920, 960, 5000, Some(0), false), ResizeDecision::Resize);
        assert_eq!(
            q(1920, 640, 6000, Some(0), true),
            ResizeDecision::Wait { wait_ms: 9000 }
        );
        assert_eq!(q(1280, 1280, 0, None, false), ResizeDecision::None);
    }

    // ---- additional Rust-side checks

    #[test]
    fn decisions_serialise_like_the_javascript_objects() {
        assert_eq!(
            serde_json::to_value(ResizeDecision::Wait { wait_ms: 5 }).unwrap(),
            serde_json::json!({"action": "wait", "waitMs": 5})
        );
        assert_eq!(
            serde_json::to_value(ResizeDecision::None).unwrap(),
            serde_json::json!({"action": "none"})
        );
    }

    #[test]
    fn view_requests_parse_from_viewer_json() {
        let r: ViewRequest =
            serde_json::from_str(r#"{"visible":false,"mode":"corrected"}"#).unwrap();
        assert!(!r.visible);
        assert_eq!(r.zoom, 1.0);
        let r: ViewRequest = serde_json::from_str(
            r#"{"tileW":620,"tileH":349,"mode":"stereo","zoom":1.45,"outputAspect":"16:9","layout":"sbs","rotation":0}"#,
        )
        .unwrap();
        assert_eq!(
            (r.tile_w, r.mode, r.layout),
            (620.0, ViewMode::Stereo, Layout::Sbs)
        );
    }

    #[test]
    fn sanitizing_clamps_like_the_hub() {
        let r = ViewRequest {
            tile_w: 1e9,
            tile_h: -5.0,
            zoom: 0.0,
            output_aspect: Some("16:9:1".into()),
            rotation: 45.0,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(
            (r.tile_w, r.tile_h, r.zoom, r.rotation),
            (16384.0, 0.0, 1.0, 0.0)
        );
        assert_eq!(r.output_aspect.as_deref(), Some("eye"));
        let ok = ViewRequest {
            output_aspect: Some("4:3".into()),
            zoom: 50.0,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(ok.output_aspect.as_deref(), Some("4:3"));
        assert_eq!(ok.zoom, 10.0);
    }

    #[test]
    fn odd_inputs_do_not_panic() {
        let weird = req(
            f64::NAN,
            f64::INFINITY,
            ViewMode::Corrected,
            f64::NAN,
            Some("1:0"),
            Layout::Quad,
            f64::NAN,
        );
        let _ = needed_long_side(QUEST3, &weird);
        assert_eq!(
            needed_long_side(
                FrameSize { w: 0, h: 10 },
                &req(10.0, 10.0, ViewMode::Raw, 1.0, None, Layout::Mono, 0.0)
            ),
            0.0
        );
        assert_eq!(pick_step(f64::NAN, 0, 0), 3840);
        assert_eq!(pick_step(100.0, 0, 500), 500);
        assert_eq!(step_index(10_000, 0), RES_STEPS.len() - 1);
        assert_eq!(scaled_bitrate(8_000_000, 0, 1920), 8_000_000);
    }
}
