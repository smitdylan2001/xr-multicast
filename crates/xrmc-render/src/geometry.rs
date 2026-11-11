//! Tile geometry and shader uniforms (the CPU half of `lens-renderer.js`).

use xrmc_decode::{ColorInfo, PixelFormat};

use crate::{Eye, Layout, TileParams, ViewMode};

/// A rectangle in framebuffer pixels (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PxRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// JavaScript `Math.round` (halves round up).
fn js_round(v: f64) -> i32 {
    (v + 0.5).floor() as i32
}

/// Largest box with `aspect` centred inside `b` (same rounding as the JS `fit()`).
pub(crate) fn fit(b: PxRect, aspect: f64) -> PxRect {
    let mut w = b.w;
    let mut h = js_round(b.w as f64 / aspect);
    if h > b.h {
        h = b.h;
        w = js_round(b.h as f64 * aspect);
    }
    PxRect {
        x: b.x + js_round((b.w - w) as f64 / 2.0),
        y: b.y + js_round((b.h - h) as f64 / 2.0),
        w,
        h,
    }
}

/// egui rect (points) → framebuffer pixels, rounding each edge like the JS `toFb()`.
pub(crate) fn rect_to_px(r: egui::Rect, ppp: f32) -> PxRect {
    let x = js_round((r.min.x * ppp) as f64);
    let y = js_round((r.min.y * ppp) as f64);
    PxRect {
        x,
        y,
        w: js_round((r.max.x * ppp) as f64) - x,
        h: js_round((r.max.y * ppp) as f64) - y,
    }
}

/// Eye region (x, y, w, h) in logical-frame UV for a layout.
pub(crate) fn eye_rect(layout: Layout, eye: Eye) -> [f32; 4] {
    let right = eye == Eye::Right;
    match layout {
        Layout::Sbs => {
            if right {
                [0.5, 0.0, 0.5, 1.0]
            } else {
                [0.0, 0.0, 0.5, 1.0]
            }
        }
        Layout::Tb => {
            if right {
                [0.0, 0.5, 1.0, 0.5]
            } else {
                [0.0, 0.0, 1.0, 0.5]
            }
        }
        Layout::Quad { bottom_row } => {
            let qy = if bottom_row { 0.5 } else { 0.0 };
            if right {
                [0.5, qy, 0.5, 0.5]
            } else {
                [0.0, qy, 0.5, 0.5]
            }
        }
        Layout::Mono => [0.0, 0.0, 1.0, 1.0],
    }
}

/// Clockwise quarter turns (0..3) from a rotation in degrees.
pub(crate) fn quarter_turns(rotation: u32) -> u32 {
    ((rotation as f64 / 90.0).round() as u64 % 4) as u32
}

/// What the shader needs to know about a stream's textures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub color: ColorInfo,
    pub format: PixelFormat,
    /// 10-bit samples live in R16Unorm textures (else they were converted to 8 bit).
    pub r16: bool,
}

impl FrameInfo {
    fn bits(&self) -> u32 {
        if self.format == PixelFormat::I420P10 && self.r16 {
            10
        } else {
            8
        }
    }
}

/// Uniforms of one tile draw; layout matches `struct Tile` in `lens.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct TileUniform {
    pub quad: [f32; 4],
    pub vp0: [f32; 4],
    pub vp1: [f32; 4],
    pub eye0: [f32; 4],
    pub eye1: [f32; 4],
    pub center: [f32; 4],
    pub lens: [f32; 4],
    pub chroma: [f32; 4],
    pub look: [f32; 4],
    pub mode: [u32; 4],
    pub yuv_r: [f32; 4],
    pub yuv_g: [f32; 4],
    pub yuv_b: [f32; 4],
    pub tex: [f32; 4],
}

/// YUV → RGB matrix rows applied to `(y, u, v, 1)` where each sample is `code / (2^bits - 1)`.
pub(crate) fn yuv_matrix(color: ColorInfo, bits: u32) -> [[f32; 4]; 3] {
    let (kr, kb) = if color.bt709 {
        (0.2126f64, 0.0722f64)
    } else {
        (0.299, 0.114)
    };
    let kg = 1.0 - kr - kb;
    let max = ((1u32 << bits) - 1) as f64;
    let m = (1u32 << (bits - 8)) as f64;
    let (ay, by, ac, bc) = if color.full_range {
        (1.0, 0.0, 1.0, -128.0 * m / max)
    } else {
        (
            max / (219.0 * m),
            -16.0 / 219.0,
            max / (224.0 * m),
            -128.0 / 224.0,
        )
    };
    let cr_r = 2.0 * (1.0 - kr);
    let cb_b = 2.0 * (1.0 - kb);
    let cb_g = -2.0 * kb * (1.0 - kb) / kg;
    let cr_g = -2.0 * kr * (1.0 - kr) / kg;
    let row = |cb: f64, cr: f64| {
        [
            ay as f32,
            (cb * ac) as f32,
            (cr * ac) as f32,
            (by + (cb + cr) * bc) as f32,
        ]
    };
    [row(0.0, cr_r), row(cb_g, cr_g), row(cb_b, 0.0)]
}

fn finite_or(v: f32, fallback: f32) -> f32 {
    // Mirrors `Number(x) || fallback` in the JS (0 and NaN fall back).
    if v.is_finite() && v != 0.0 {
        v
    } else {
        fallback
    }
}

/// How a tile is laid out in the target.
pub(crate) enum Fit {
    /// Letterbox inside the box (the normal tile drawing).
    Letterbox,
    /// Fill the box exactly (frame analysis: logical frame stretched to a square).
    Stretch,
}

/// Computes the uniforms for drawing `frame` with `p` into `bx` of a `target`-sized framebuffer.
/// `None` when nothing would be visible.
pub(crate) fn tile_uniform(
    frame: &FrameInfo,
    p: &TileParams,
    bx: PxRect,
    target: [u32; 2],
    linearise: bool,
    fit_mode: Fit,
) -> Option<TileUniform> {
    if frame.width == 0 || frame.height == 0 || bx.w <= 0 || bx.h <= 0 {
        return None;
    }
    let rot = quarter_turns(p.rotation);
    let (lw, lh) = if rot % 2 == 1 {
        (frame.height as f64, frame.width as f64)
    } else {
        (frame.width as f64, frame.height as f64)
    };
    let cw = frame.width.div_ceil(2) as f32;
    let ch = frame.height.div_ceil(2) as f32;
    let [yuv_r, yuv_g, yuv_b] = yuv_matrix(frame.color, frame.bits());
    let mut u = TileUniform {
        quad: [0.0; 4],
        vp0: [0.0; 4],
        vp1: [0.0; 4],
        eye0: [0.0, 0.0, 1.0, 1.0],
        eye1: [0.0, 0.0, 1.0, 1.0],
        center: [0.0; 4],
        lens: [0.0, 0.0, 1.0, 1.0],
        chroma: [1.0, 1.0, 1.0, (lw / lh) as f32],
        look: [
            f32::from(u8::from(p.flip_h)),
            f32::from(u8::from(p.flip_v)),
            f32::from(u8::from(p.grid)),
            f32::from(u8::from(p.blur)),
        ],
        mode: [0, rot, 0, u32::from(frame.format == PixelFormat::Nv12)],
        yuv_r,
        yuv_g,
        yuv_b,
        tex: [
            if frame.bits() == 10 {
                65535.0 / 1023.0
            } else {
                1.0
            },
            frame.width as f32 / (2.0 * cw),
            frame.height as f32 / (2.0 * ch),
            f32::from(u8::from(linearise)),
        ],
    };

    let vp_of = |r: PxRect| [r.x as f32, r.y as f32, r.w as f32, r.h as f32];
    let quad = match (p.view, fit_mode) {
        (_, Fit::Stretch) => {
            u.vp0 = vp_of(bx);
            bx
        }
        (ViewMode::Raw, Fit::Letterbox) => {
            let vp = fit(bx, lw / lh);
            u.vp0 = vp_of(vp);
            vp
        }
        (view, Fit::Letterbox) => {
            let left = eye_rect(p.layout, Eye::Left);
            let eye_w = lw * left[2] as f64;
            let eye_h = lh * left[3] as f64;
            let eye_aspect = eye_w / eye_h;
            let out_aspect = match p.lens.output_aspect {
                Some(a) if a.is_finite() && a > 0.0 => a as f64,
                _ => eye_aspect,
            };
            let side_by_side = matches!(p.layout, Layout::Sbs | Layout::Quad { .. });
            let cx = finite_or(p.lens.center_x, 0.0);
            let cy = finite_or(p.lens.center_y, 0.0);
            // Lens centres sit symmetrically around the nose, so mirror X for the right eye.
            let center_of = |eye: Eye| {
                [
                    if side_by_side && eye == Eye::Right {
                        -cx
                    } else {
                        cx
                    },
                    cy,
                ]
            };
            u.mode[0] = 1;
            u.lens = [
                finite_or(p.lens.k1, 0.0),
                finite_or(p.lens.k2, 0.0),
                finite_or(p.lens.zoom, 1.0).max(0.1),
                eye_aspect as f32,
            ];
            u.chroma = [
                finite_or(p.lens.chroma_r, 1.0),
                1.0,
                finite_or(p.lens.chroma_b, 1.0),
                out_aspect as f32,
            ];

            if view == ViewMode::Stereo && p.layout != Layout::Mono {
                // Keep each eye at most square so the pair doesn't become a thin letterbox.
                let oa = out_aspect.min(1.0);
                u.chroma[3] = oa as f32;
                let vp = fit(bx, oa * 2.0);
                let half = vp.w / 2;
                u.vp0 = vp_of(PxRect { w: half, ..vp });
                u.vp1 = vp_of(PxRect {
                    x: vp.x + half,
                    w: vp.w - half,
                    ..vp
                });
                u.eye0 = left;
                u.eye1 = eye_rect(p.layout, Eye::Right);
                let (c0, c1) = (center_of(Eye::Left), center_of(Eye::Right));
                u.center = [c0[0], c0[1], c1[0], c1[1]];
                u.mode[2] = 1;
                vp
            } else {
                let vp = fit(bx, out_aspect);
                u.vp0 = vp_of(vp);
                u.eye0 = eye_rect(p.layout, p.eye);
                let c = center_of(p.eye);
                u.center = [c[0], c[1], c[0], c[1]];
                vp
            }
        }
    };
    if quad.w <= 0 || quad.h <= 0 {
        return None;
    }
    let (tw, th) = (target[0].max(1) as f32, target[1].max(1) as f32);
    u.quad = [
        quad.x as f32 / tw * 2.0 - 1.0,
        1.0 - quad.y as f32 / th * 2.0,
        (quad.x + quad.w) as f32 / tw * 2.0 - 1.0,
        1.0 - (quad.y + quad.h) as f32 / th * 2.0,
    ];
    Some(u)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LensParams;

    fn info(w: u32, h: u32) -> FrameInfo {
        FrameInfo {
            width: w,
            height: h,
            color: ColorInfo::default(),
            format: PixelFormat::I420,
            r16: false,
        }
    }

    #[test]
    fn fit_letterboxes_like_js() {
        let b = PxRect {
            x: 10,
            y: 20,
            w: 400,
            h: 400,
        };
        assert_eq!(
            fit(b, 16.0 / 9.0),
            PxRect {
                x: 10,
                y: 108,
                w: 400,
                h: 225
            }
        );
        assert_eq!(
            fit(b, 0.5),
            PxRect {
                x: 110,
                y: 20,
                w: 200,
                h: 400
            }
        );
    }

    #[test]
    fn uniform_layout_matches_wgsl() {
        assert_eq!(std::mem::size_of::<TileUniform>(), 14 * 16);
    }

    #[test]
    fn rotation_swaps_logical_size() {
        let p = TileParams {
            view: ViewMode::Raw,
            rotation: 90,
            ..Default::default()
        };
        let u = tile_uniform(
            &info(200, 100),
            &p,
            PxRect {
                x: 0,
                y: 0,
                w: 300,
                h: 300,
            },
            [300, 300],
            false,
            Fit::Letterbox,
        )
        .unwrap();
        assert_eq!(u.mode[1], 1);
        assert_eq!(u.vp0, [75.0, 0.0, 150.0, 300.0]);
        assert_eq!(quarter_turns(270), 3);
        assert_eq!(quarter_turns(360), 0);
    }

    #[test]
    fn stereo_eyes_are_at_most_square_and_mirrored() {
        let p = TileParams {
            view: ViewMode::Stereo,
            lens: LensParams {
                center_x: 0.1,
                output_aspect: Some(16.0 / 9.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let u = tile_uniform(
            &info(2000, 1000),
            &p,
            PxRect {
                x: 0,
                y: 0,
                w: 800,
                h: 300,
            },
            [800, 300],
            false,
            Fit::Letterbox,
        )
        .unwrap();
        assert_eq!(u.chroma[3], 1.0);
        assert_eq!(u.vp0, [100.0, 0.0, 300.0, 300.0]);
        assert_eq!(u.vp1, [400.0, 0.0, 300.0, 300.0]);
        assert_eq!(u.center, [0.1, 0.0, -0.1, 0.0]);
        assert_eq!(u.mode[2], 1);
    }

    #[test]
    fn tb_does_not_mirror_centre() {
        let p = TileParams {
            eye: Eye::Right,
            layout: Layout::Tb,
            lens: LensParams {
                center_x: 0.1,
                ..Default::default()
            },
            ..Default::default()
        };
        let u = tile_uniform(
            &info(1000, 2000),
            &p,
            PxRect {
                x: 0,
                y: 0,
                w: 160,
                h: 90,
            },
            [160, 90],
            false,
            Fit::Letterbox,
        )
        .unwrap();
        assert_eq!(u.center[0], 0.1);
        assert_eq!(u.eye0, [0.0, 0.5, 1.0, 0.5]);
    }

    #[test]
    fn bt709_limited_white_and_black() {
        let m = yuv_matrix(
            ColorInfo {
                bt709: true,
                full_range: false,
            },
            8,
        );
        let apply = |y: f32, u: f32, v: f32| m.map(|r| r[0] * y + r[1] * u + r[2] * v + r[3]);
        for c in apply(235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0) {
            assert!((c - 1.0).abs() < 1e-4);
        }
        for c in apply(16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0) {
            assert!(c.abs() < 1e-4);
        }
    }
}
