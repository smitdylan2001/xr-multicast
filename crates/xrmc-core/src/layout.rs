//! Panel layout detection and resolution (port of `src/renderer/js/layout-detect.js`).
//!
//! Standalone headsets mirror their panel image: one or more barrel-distorted
//! eye views, each a bright region surrounded by black (outside the
//! lens-visible area). Counting those regions along each axis tells us whether
//! the frame is a single view, two views side by side / stacked, or a 2×2 grid.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::js;

/// Frames darker than this (0..1) carry no layout information.
const MIN_MEAN: f64 = 0.03;
/// The centre dip must be below this fraction of the weaker peak.
const DIP_RATIO: f64 = 0.6;
/// Each peak must reach this fraction of the profile maximum.
const MIN_PEAK: f64 = 0.4;

/// A concrete arrangement of eye views in a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// Side by side.
    Sbs,
    /// Top / bottom.
    Tb,
    /// 2×2 grid.
    Quad,
    /// A single view. Unknown strings read as `Mono`.
    #[default]
    #[serde(other)]
    Mono,
}

impl Layout {
    pub const ALL: [Layout; 4] = [Layout::Sbs, Layout::Tb, Layout::Quad, Layout::Mono];

    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Sbs => "sbs",
            Layout::Tb => "tb",
            Layout::Quad => "quad",
            Layout::Mono => "mono",
        }
    }
}

/// A layout setting: automatic detection or a fixed [`Layout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutSetting {
    Sbs,
    Tb,
    Quad,
    Mono,
    /// Detect from the video. Unknown strings read as `Auto`.
    #[default]
    #[serde(other)]
    Auto,
}

impl LayoutSetting {
    /// The fixed layout, or `None` for automatic detection.
    pub fn fixed(self) -> Option<Layout> {
        match self {
            LayoutSetting::Auto => None,
            LayoutSetting::Sbs => Some(Layout::Sbs),
            LayoutSetting::Tb => Some(Layout::Tb),
            LayoutSetting::Quad => Some(Layout::Quad),
            LayoutSetting::Mono => Some(Layout::Mono),
        }
    }
}

impl From<Layout> for LayoutSetting {
    fn from(l: Layout) -> Self {
        match l {
            Layout::Sbs => LayoutSetting::Sbs,
            Layout::Tb => LayoutSetting::Tb,
            Layout::Quad => LayoutSetting::Quad,
            Layout::Mono => LayoutSetting::Mono,
        }
    }
}

/// A rotation setting: `"auto"` or clockwise degrees (normally 0/90/180/270).
///
/// Serialised like the Electron app: the string `"auto"` or a number. `null`
/// and `""` read as `Auto`; numeric strings read as degrees; any other string
/// reads as 0° (JavaScript's `Number("x") || 0`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Rotation {
    #[default]
    Auto,
    Degrees(f64),
}

impl Serialize for Rotation {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match *self {
            Rotation::Auto => s.serialize_str("auto"),
            Rotation::Degrees(d) if d.fract() == 0.0 && d.abs() < 1e15 => s.serialize_i64(d as i64),
            Rotation::Degrees(d) if d.is_finite() => s.serialize_f64(d),
            Rotation::Degrees(_) => s.serialize_i64(0),
        }
    }
}

impl<'de> Deserialize<'de> for Rotation {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(match v {
            serde_json::Value::Null => Rotation::Auto,
            serde_json::Value::Number(n) => Rotation::Degrees(n.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(s) if s == "auto" || s.is_empty() => Rotation::Auto,
            serde_json::Value::String(s) => {
                Rotation::Degrees(s.trim().parse::<f64>().unwrap_or(0.0))
            }
            serde_json::Value::Bool(b) => Rotation::Degrees(if b { 1.0 } else { 0.0 }),
            _ => Rotation::Degrees(0.0),
        })
    }
}

/// Number of views along each axis of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridShape {
    pub cols: u32,
    pub rows: u32,
}

/// Result of [`detect_grid`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    /// 1 or 2.
    pub cols: u32,
    /// 1 or 2.
    pub rows: u32,
    /// 0..1, how far the weakest axis decision was from the threshold.
    pub confidence: f64,
}

impl Grid {
    pub fn shape(&self) -> GridShape {
        GridShape {
            cols: self.cols,
            rows: self.rows,
        }
    }
}

/// Result of [`split_profile`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Split {
    pub split: bool,
    /// Centre dip divided by the weaker peak (1 when there is no usable signal).
    pub ratio: f64,
}

/// A luminance sample that can be read as 0..1.
pub trait LumaSample: Copy {
    fn luma(self) -> f64;
}

impl LumaSample for f32 {
    fn luma(self) -> f64 {
        f64::from(self)
    }
}

impl LumaSample for f64 {
    fn luma(self) -> f64 {
        self
    }
}

/// 8-bit samples (e.g. a decoded Y plane) are scaled to 0..1.
impl LumaSample for u8 {
    fn luma(self) -> f64 {
        f64::from(self) / 255.0
    }
}

fn smooth(p: &[f64], radius: usize) -> Vec<f64> {
    let n = p.len();
    (0..n)
        .map(|i| {
            let lo = i.saturating_sub(radius);
            let hi = (i + radius).min(n - 1);
            let s: f64 = p[lo..=hi].iter().sum();
            s / (hi - lo + 1) as f64
        })
        .collect()
}

/// Does a 1-D brightness profile show two bright lobes split by a dark gap near the centre?
pub fn split_profile(profile: &[f64]) -> Split {
    let none = Split {
        split: false,
        ratio: 1.0,
    };
    let n = profile.len();
    if n == 0 {
        return none;
    }
    let radius = js::round(n as f64 / 48.0).max(1.0) as usize;
    let p = smooth(profile, radius);
    let max = p.iter().fold(0.0_f64, |m, &v| m.max(v));
    if max <= 0.0 {
        return none;
    }
    let lo = (n as f64 * 0.3).floor() as usize;
    let hi = ((n as f64 * 0.7).ceil() as usize).min(n);
    let mut dip = lo.min(n - 1);
    for i in lo..hi {
        if p[i] < p[dip] {
            dip = i;
        }
    }
    let peak_a = p[..dip].iter().fold(0.0_f64, |m, &v| m.max(v));
    let peak_b = p[dip + 1..].iter().fold(0.0_f64, |m, &v| m.max(v));
    let reference = peak_a.min(peak_b);
    if reference < max * MIN_PEAK || reference <= 0.0 {
        return none;
    }
    let ratio = p[dip] / reference;
    Split {
        split: ratio < DIP_RATIO,
        ratio,
    }
}

/// Count the eye views in a row-major luminance image (0..1, or `u8`).
///
/// Returns `None` when the frame is too dark to judge or the buffer is smaller than `w × h`.
pub fn detect_grid<T: LumaSample>(luma: &[T], w: usize, h: usize) -> Option<Grid> {
    let count = w.checked_mul(h)?;
    if w == 0 || h == 0 || luma.len() < count {
        return None;
    }
    let mut col_p = vec![0.0_f64; w];
    let mut row_p = vec![0.0_f64; h];
    let mut total = 0.0_f64;
    for (y, row) in luma[..count].chunks_exact(w).enumerate() {
        for (x, &s) in row.iter().enumerate() {
            let v = s.luma();
            col_p[x] += v;
            row_p[y] += v;
            total += v;
        }
    }
    if total / (count as f64) < MIN_MEAN {
        return None;
    }
    for c in &mut col_p {
        *c /= h as f64;
    }
    for r in &mut row_p {
        *r /= w as f64;
    }
    let c = split_profile(&col_p);
    let r = split_profile(&row_p);
    let conf = |s: Split| ((s.ratio - DIP_RATIO).abs() / DIP_RATIO).min(1.0);
    Some(Grid {
        cols: if c.split { 2 } else { 1 },
        rows: if r.split { 2 } else { 1 },
        confidence: conf(c).min(conf(r)),
    })
}

/// The layout a detected grid corresponds to.
pub fn grid_to_layout(g: GridShape) -> Layout {
    match (g.cols == 2, g.rows == 2) {
        (true, true) => Layout::Quad,
        (true, false) => Layout::Sbs,
        (false, true) => Layout::Tb,
        (false, false) => Layout::Mono,
    }
}

/// Input of [`resolve_view`].
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ViewInput {
    pub layout: LayoutSetting,
    pub rotation: Rotation,
    /// Brand/model default used when `rotation` is `Auto`.
    pub auto_rotation: f64,
    /// Raw frame width.
    pub frame_w: u32,
    /// Raw frame height.
    pub frame_h: u32,
    /// Detected grid in the raw (unrotated) frame.
    pub grid: Option<GridShape>,
    /// The stream is already cropped to one eye.
    pub single_eye: bool,
}

/// Output of [`resolve_view`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedView {
    pub layout: Layout,
    /// Clockwise degrees: 0, 90, 180 or 270.
    pub rotation: u32,
    pub rotation_auto: bool,
    /// The layout came from the detected grid.
    pub detected: bool,
}

fn quarter_turns(degrees: f64) -> u32 {
    let r = js::round(degrees / 90.0);
    let r = if r.is_nan() { 0.0 } else { r };
    // JS: ((r % 4) + 4) % 4 — `%` is a truncating remainder in both languages.
    let q = ((r % 4.0) + 4.0) % 4.0;
    q as u32
}

/// Turn the (possibly automatic) layout/rotation settings into concrete values.
pub fn resolve_view(o: &ViewInput) -> ResolvedView {
    let rotation_auto = matches!(o.rotation, Rotation::Auto);
    let degrees = match o.rotation {
        Rotation::Auto => o.auto_rotation,
        Rotation::Degrees(d) => d,
    };
    let mut rot = quarter_turns(degrees) * 90;
    let (fw, fh) = (f64::from(o.frame_w), f64::from(o.frame_h));
    let g0 = if o.single_eye { None } else { o.grid };
    let mut g = g0.map(|g| {
        if !rot.is_multiple_of(180) {
            GridShape {
                cols: g.rows,
                rows: g.cols,
            }
        } else {
            g
        }
    });
    // Wide frames are side-by-side panels; a horizontal split there is just a dark band in the scene.
    let base_aspect = if !rot.is_multiple_of(180) {
        fh / fw
    } else {
        fw / fh
    };
    if let Some(gg) = g
        && base_aspect >= 1.6
    {
        g = Some(GridShape {
            cols: gg.cols,
            rows: 1,
        });
    }
    // Stereo views stacked vertically are almost certainly a sideways panel: turn them side by side.
    if rotation_auto
        && g.is_some_and(|g| g.cols == 1 && g.rows == 2)
        && matches!(o.layout, LayoutSetting::Auto | LayoutSetting::Sbs)
    {
        rot = (rot + 90) % 360;
        g = Some(GridShape { cols: 2, rows: 1 });
    }
    let (lw, lh) = if !rot.is_multiple_of(180) {
        (fh, fw)
    } else {
        (fw, fh)
    };
    if let Some(layout) = o.layout.fixed() {
        return ResolvedView {
            layout,
            rotation: rot,
            rotation_auto,
            detected: false,
        };
    }
    if o.single_eye {
        return ResolvedView {
            layout: Layout::Mono,
            rotation: rot,
            rotation_auto,
            detected: false,
        };
    }
    let mut layout = g.map(grid_to_layout);
    // Wide frames are side-by-side stereo even when the gap between the eyes is too faint to see.
    if matches!(layout, None | Some(Layout::Mono)) && lh > 0.0 && lw / lh >= 1.6 {
        layout = Some(Layout::Sbs);
    }
    ResolvedView {
        layout: layout.unwrap_or(Layout::Mono),
        rotation: rot,
        rotation_auto,
        detected: g.is_some(),
    }
}

#[cfg(test)]
mod tests {
    //! Port of `test/layout-detect.test.js` (layout parts; brand parts live in `brands`).
    use super::*;

    struct Frame {
        luma: Vec<f32>,
        w: usize,
        h: usize,
    }

    /// Synthetic panel: `cols`×`rows` bright elliptical eye views on black, with some texture inside.
    fn frame_with(cols: usize, rows: usize, w: usize, h: usize, level: f64, fill: f64) -> Frame {
        let mut luma = vec![0.0_f32; w * h];
        let cw = w as f64 / cols as f64;
        let ch = h as f64 / rows as f64;
        for y in 0..h {
            for x in 0..w {
                let (xf, yf) = (x as f64, y as f64);
                let cx = ((xf / cw).floor() + 0.5) * cw;
                let cy = ((yf / ch).floor() + 0.5) * ch;
                let dx = (xf - cx) / ((cw / 2.0) * fill);
                let dy = (yf - cy) / ((ch / 2.0) * fill);
                let inside = dx * dx + dy * dy <= 1.0;
                luma[y * w + x] = if inside {
                    (level * (0.6 + 0.4 * (xf * 0.7 + yf * 0.3).sin().abs())) as f32
                } else {
                    0.0
                };
            }
        }
        Frame { luma, w, h }
    }

    fn frame(cols: usize, rows: usize) -> Frame {
        frame_with(cols, rows, 96, 96, 0.7, 0.92)
    }

    fn grid(f: &Frame) -> Option<String> {
        detect_grid(&f.luma, f.w, f.h).map(|g| format!("{}x{}", g.cols, g.rows))
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn single_view() {
        assert_eq!(grid(&frame(1, 1)), some("1x1"));
    }

    #[test]
    fn side_by_side() {
        assert_eq!(grid(&frame(2, 1)), some("2x1"));
    }

    #[test]
    fn top_bottom() {
        assert_eq!(grid(&frame(1, 2)), some("1x2"));
    }

    #[test]
    fn grid_2x2() {
        assert_eq!(grid(&frame(2, 2)), some("2x2"));
    }

    #[test]
    fn non_square_sampling_of_a_2x2_grid() {
        assert_eq!(grid(&frame_with(2, 2, 64, 80, 0.7, 0.92)), some("2x2"));
    }

    #[test]
    fn dim_but_visible_content_still_detected() {
        assert_eq!(grid(&frame_with(2, 1, 96, 96, 0.12, 0.92)), some("2x1"));
    }

    #[test]
    fn black_and_near_black_frames_give_no_answer() {
        assert_eq!(detect_grid(&vec![0.0_f32; 96 * 96], 96, 96), None);
        assert_eq!(grid(&frame_with(2, 1, 96, 96, 0.02, 0.92)), None);
    }

    #[test]
    fn a_single_view_filling_the_frame_is_not_split() {
        let w = 64;
        let luma = vec![0.5_f32; w * w];
        assert_eq!(detect_grid(&luma, w, w).unwrap().cols, 1);
    }

    #[test]
    fn a_small_off_centre_view_is_not_mistaken_for_two() {
        let w = 96;
        let mut luma = vec![0.0_f32; w * w];
        for y in 30..60 {
            for x in 5..35 {
                luma[y * w + x] = 0.8;
            }
        }
        assert_eq!(grid(&Frame { luma, w, h: w }), some("1x1"));
    }

    #[test]
    fn bad_buffers_are_rejected_without_panicking() {
        assert_eq!(detect_grid::<f32>(&[], 0, 0), None);
        assert_eq!(detect_grid(&[0.5_f32; 10], 4, 4), None);
        assert_eq!(detect_grid(&[0.5_f32; 4], usize::MAX, 2), None);
        assert!(detect_grid(&[200_u8; 16], 4, 4).is_some());
        assert_eq!(
            split_profile(&[]),
            Split {
                split: false,
                ratio: 1.0
            }
        );
        assert!(!split_profile(&[1.0]).split);
    }

    fn input(
        layout: LayoutSetting,
        rotation: Rotation,
        auto_rotation: f64,
        w: u32,
        h: u32,
        grid: Option<(u32, u32)>,
    ) -> ViewInput {
        ViewInput {
            layout,
            rotation,
            auto_rotation,
            frame_w: w,
            frame_h: h,
            grid: grid.map(|(cols, rows)| GridShape { cols, rows }),
            single_eye: false,
        }
    }

    #[test]
    fn resolve_view_quest_like_wide_frame_is_side_by_side() {
        let base = |g| input(LayoutSetting::Auto, Rotation::Auto, 0.0, 1832, 960, g);
        assert_eq!(
            resolve_view(&base(Some((2, 1)))),
            ResolvedView {
                layout: Layout::Sbs,
                rotation: 0,
                rotation_auto: true,
                detected: true
            }
        );
        // Faint gap between the eyes: the aspect ratio still says stereo.
        assert_eq!(resolve_view(&base(Some((1, 1)))).layout, Layout::Sbs);
        assert_eq!(resolve_view(&base(None)).layout, Layout::Sbs);
        // A dark horizon band must not turn a wide stereo frame into a grid or rotate it.
        assert_eq!(resolve_view(&base(Some((2, 2)))).layout, Layout::Sbs);
        let r = resolve_view(&base(Some((1, 2))));
        assert_eq!((r.layout, r.rotation), (Layout::Sbs, 0));
    }

    #[test]
    fn resolve_view_pfd_mr_2x2_grid_with_90_default() {
        let r = resolve_view(&input(
            LayoutSetting::Auto,
            Rotation::Auto,
            90.0,
            1784,
            1920,
            Some((2, 2)),
        ));
        assert_eq!(r.layout, Layout::Quad);
        assert_eq!(r.rotation, 90);
    }

    #[test]
    fn resolve_view_stacked_stereo_views_get_turned_side_by_side() {
        // Raw frame has two views side by side; the 90° default stacks them, so auto adds another 90°.
        let r = resolve_view(&input(
            LayoutSetting::Auto,
            Rotation::Auto,
            90.0,
            1784,
            1920,
            Some((2, 1)),
        ));
        assert_eq!((r.layout, r.rotation), (Layout::Sbs, 180));
        let q = resolve_view(&input(
            LayoutSetting::Auto,
            Rotation::Auto,
            0.0,
            1000,
            1000,
            Some((1, 2)),
        ));
        assert_eq!((q.layout, q.rotation), (Layout::Sbs, 90));
    }

    #[test]
    fn resolve_view_explicit_settings_win() {
        let r = resolve_view(&input(
            LayoutSetting::Tb,
            Rotation::Degrees(270.0),
            90.0,
            1000,
            1000,
            Some((1, 2)),
        ));
        assert_eq!(
            (r.layout, r.rotation, r.rotation_auto),
            (Layout::Tb, 270, false)
        );
        let m = resolve_view(&input(
            LayoutSetting::Mono,
            Rotation::Auto,
            0.0,
            1080,
            2400,
            Some((1, 2)),
        ));
        assert_eq!((m.layout, m.rotation), (Layout::Mono, 0));
    }

    #[test]
    fn resolve_view_single_eye_streams_stay_mono() {
        let mut i = input(
            LayoutSetting::Auto,
            Rotation::Auto,
            0.0,
            916,
            960,
            Some((1, 2)),
        );
        i.single_eye = true;
        let r = resolve_view(&i);
        assert_eq!((r.layout, r.rotation), (Layout::Mono, 0));
    }

    #[test]
    fn rotation_settings_parse_like_javascript() {
        let p = |s: &str| serde_json::from_str::<Rotation>(s).unwrap();
        assert_eq!(p("\"auto\""), Rotation::Auto);
        assert_eq!(p("null"), Rotation::Auto);
        assert_eq!(p("\"\""), Rotation::Auto);
        assert_eq!(p("90"), Rotation::Degrees(90.0));
        assert_eq!(p("\"180\""), Rotation::Degrees(180.0));
        assert_eq!(
            serde_json::to_string(&Rotation::Degrees(270.0)).unwrap(),
            "270"
        );
        assert_eq!(serde_json::to_string(&Rotation::Auto).unwrap(), "\"auto\"");
        // Negative and odd angles normalise like the JS (Math.round ties up, truncating %).
        let r = |d: f64| {
            resolve_view(&input(
                LayoutSetting::Mono,
                Rotation::Degrees(d),
                0.0,
                10,
                10,
                None,
            ))
            .rotation
        };
        assert_eq!(r(-90.0), 270);
        assert_eq!(r(-45.0), 0);
        assert_eq!(r(45.0), 90);
        assert_eq!(r(450.0), 90);
    }

    #[test]
    fn layout_strings_parse_leniently() {
        assert_eq!(
            serde_json::from_str::<Layout>("\"quad\"").unwrap(),
            Layout::Quad
        );
        assert_eq!(
            serde_json::from_str::<Layout>("\"weird\"").unwrap(),
            Layout::Mono
        );
        assert_eq!(
            serde_json::from_str::<LayoutSetting>("\"weird\"").unwrap(),
            LayoutSetting::Auto
        );
        assert_eq!(grid_to_layout(GridShape { cols: 1, rows: 2 }), Layout::Tb);
    }
}
