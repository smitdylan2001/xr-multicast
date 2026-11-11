//! Headless GPU tests: synthetic frames rendered offscreen, pixels checked.
//!
//! Runs on the default wgpu backend (Vulkan here); `WGPU_BACKEND=gl cargo test` exercises
//! the GLES path. PNGs go to `~/.local/share/xrmc-tools/render-tests/` (or `$XRMC_RENDER_TEST_OUT`).

use std::path::PathBuf;
use std::sync::OnceLock;

use xrmc_decode::{ColorInfo, Frame, PixelFormat, Plane};
use xrmc_render::{Eye, Layout, LensParams, TileParams, VideoRenderer, ViewMode};

struct Gpu {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

fn gpu() -> &'static Gpu {
    static GPU: OnceLock<Gpu> = OnceLock::new();
    GPU.get_or_init(|| {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .expect("no GPU adapter");
        // Downlevel limits: what a Raspberry Pi 5 class GPU guarantees.
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("xrmc-render tests"),
                required_features: adapter.features() & xrmc_render::OPTIONAL_FEATURES,
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits())
                    .using_alignment(adapter.limits()),
                ..Default::default()
            }),
        )
        .expect("no GPU device");
        eprintln!("test GPU: {:?}", adapter.get_info());
        Gpu {
            adapter,
            device,
            queue,
        }
    })
}

fn renderer() -> VideoRenderer {
    let g = gpu();
    VideoRenderer::new_headless(&g.device, &g.queue, Some(&g.adapter))
}

fn out_dir() -> PathBuf {
    let dir = std::env::var_os("XRMC_RENDER_TEST_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".local/share/xrmc-tools/render-tests")
        });
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn save_png(name: &str, w: u32, h: u32, rgba: &[u8]) {
    let path = out_dir().join(name);
    let file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
}

// ---------- synthetic frames ----------

type Rgb = [f32; 3];
const RED: Rgb = [1.0, 0.0, 0.0];
const GREEN: Rgb = [0.0, 1.0, 0.0];
const BLUE: Rgb = [0.0, 0.0, 1.0];
const WHITE: Rgb = [1.0, 1.0, 1.0];
const BLACK: Rgb = [0.0, 0.0, 0.0];
const YELLOW: Rgb = [1.0, 1.0, 0.0];
const CYAN: Rgb = [0.0, 1.0, 1.0];
const MAGENTA: Rgb = [1.0, 0.0, 1.0];

/// RGB (0..1) → (Y 0..1, Cb -0.5..0.5, Cr -0.5..0.5).
fn rgb_to_ycc(c: Rgb, bt709: bool) -> [f64; 3] {
    let (kr, kb) = if bt709 {
        (0.2126, 0.0722)
    } else {
        (0.299, 0.114)
    };
    let (r, g, b) = (c[0] as f64, c[1] as f64, c[2] as f64);
    let y = kr * r + (1.0 - kr - kb) * g + kb * b;
    [
        y,
        (b - y) / (2.0 * (1.0 - kb)),
        (r - y) / (2.0 * (1.0 - kr)),
    ]
}

fn code(v: f64, luma: bool, full: bool, bits: u32) -> u16 {
    let m = (1u32 << (bits - 8)) as f64;
    let max = ((1u32 << bits) - 1) as f64;
    let c = match (luma, full) {
        (true, false) => 16.0 * m + 219.0 * m * v,
        (false, false) => 128.0 * m + 224.0 * m * v,
        (true, true) => max * v,
        (false, true) => 128.0 * m + max * v,
    };
    c.round().clamp(0.0, max) as u16
}

/// Builds a frame of any supported format from an RGB function; `pad` extra bytes per row
/// (filled with garbage) exercise stride handling.
fn make_frame(
    w: u32,
    h: u32,
    format: PixelFormat,
    color: ColorInfo,
    pad: usize,
    f: impl Fn(u32, u32) -> Rgb,
) -> Frame {
    let bits = if format == PixelFormat::I420P10 {
        10
    } else {
        8
    };
    let bps = if bits == 10 { 2 } else { 1 };
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let ycc: Vec<[f64; 3]> = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| rgb_to_ycc(f(x, y), color.bt709))
        .collect();
    let chroma = |cx: u32, cy: u32, k: usize| {
        let mut s = 0.0;
        let mut n = 0.0;
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let (x, y) = (cx * 2 + dx, cy * 2 + dy);
            if x < w && y < h {
                s += ycc[(y * w + x) as usize][k];
                n += 1.0;
            }
        }
        s / n
    };
    let put = |data: &mut Vec<u8>, v: u16| {
        if bps == 2 {
            data.extend_from_slice(&v.to_le_bytes());
        } else {
            data.push(v as u8);
        }
    };
    let mut yp = Vec::new();
    for y in 0..h {
        for x in 0..w {
            put(
                &mut yp,
                code(ycc[(y * w + x) as usize][0], true, color.full_range, bits),
            );
        }
        yp.extend(std::iter::repeat_n(0xA5, pad));
    }
    let plane = |data: Vec<u8>, stride: usize, pw: u32, ph: u32| Plane {
        data,
        stride,
        width: pw,
        height: ph,
    };
    let mut planes = vec![plane(yp, w as usize * bps + pad, w, h)];
    if format == PixelFormat::Nv12 {
        let mut uv = Vec::new();
        for cy in 0..ch {
            for cx in 0..cw {
                uv.push(code(chroma(cx, cy, 1), false, color.full_range, 8) as u8);
                uv.push(code(chroma(cx, cy, 2), false, color.full_range, 8) as u8);
            }
            uv.extend(std::iter::repeat_n(0x5A, pad));
        }
        planes.push(plane(uv, cw as usize * 2 + pad, cw, ch));
    } else {
        for k in [1, 2] {
            let mut p = Vec::new();
            for cy in 0..ch {
                for cx in 0..cw {
                    put(
                        &mut p,
                        code(chroma(cx, cy, k), false, color.full_range, bits),
                    );
                }
                p.extend(std::iter::repeat_n(0x5A, pad));
            }
            planes.push(plane(p, cw as usize * bps + pad, cw, ch));
        }
    }
    Frame {
        width: w,
        height: h,
        format,
        color,
        planes,
        pts: 0,
    }
}

fn quadrants(w: u32, h: u32, tl: Rgb, tr: Rgb, bl: Rgb, br: Rgb) -> impl Fn(u32, u32) -> Rgb {
    move |x, y| match (x < w / 2, y < h / 2) {
        (true, true) => tl,
        (false, true) => tr,
        (true, false) => bl,
        (false, false) => br,
    }
}

const LIMITED_709: ColorInfo = ColorInfo {
    bt709: true,
    full_range: false,
};

fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]]
}

fn assert_color(img: &[u8], w: u32, x: u32, y: u32, want: Rgb, tol: i32, what: &str) {
    let got = px(img, w, x, y);
    let want8 = want.map(|c| (c * 255.0).round() as i32);
    let ok = (0..3).all(|i| (got[i] as i32 - want8[i]).abs() <= tol);
    assert!(
        ok,
        "{what}: pixel ({x},{y}) = {got:?}, want {want8:?} ±{tol}"
    );
}

/// Lens off: the corrected view shows the eye region exactly, stretched to the viewport.
fn flat_lens() -> LensParams {
    LensParams {
        k1: 0.0,
        k2: 0.0,
        zoom: 1.0,
        output_aspect: None,
        ..Default::default()
    }
}

// ---------- YUV → RGB ----------

const BANDS: [Rgb; 8] = [WHITE, BLACK, RED, GREEN, BLUE, YELLOW, CYAN, MAGENTA];

fn check_bands(r: &VideoRenderer, frame: &Frame, tol: i32, what: &str) {
    r.try_upload("s", frame).unwrap();
    let (w, h) = (frame.width, frame.height);
    let raw = TileParams {
        view: ViewMode::Raw,
        ..Default::default()
    };
    let img = r.snapshot("s", &raw, w, h).unwrap();
    let band_w = w / 8;
    for (i, c) in BANDS.iter().enumerate() {
        assert_color(
            &img,
            w,
            i as u32 * band_w + band_w / 2,
            h / 2,
            *c,
            tol,
            &format!("{what} band {i}"),
        );
    }
}

fn band_frame(format: PixelFormat, color: ColorInfo, pad: usize) -> Frame {
    make_frame(128, 16, format, color, pad, |x, _| BANDS[(x / 16) as usize])
}

#[test]
fn yuv_i420_limited_709() {
    let r = renderer();
    check_bands(
        &r,
        &band_frame(PixelFormat::I420, LIMITED_709, 0),
        3,
        "I420 709 limited",
    );
}

#[test]
fn yuv_i420_full_601_and_limited_601() {
    let r = renderer();
    check_bands(
        &r,
        &band_frame(
            PixelFormat::I420,
            ColorInfo {
                bt709: false,
                full_range: true,
            },
            0,
        ),
        3,
        "I420 601 full",
    );
    check_bands(
        &r,
        &band_frame(
            PixelFormat::I420,
            ColorInfo {
                bt709: false,
                full_range: false,
            },
            0,
        ),
        3,
        "I420 601 limited",
    );
}

#[test]
fn yuv_nv12() {
    let r = renderer();
    check_bands(
        &r,
        &band_frame(PixelFormat::Nv12, LIMITED_709, 0),
        3,
        "NV12",
    );
    check_bands(
        &r,
        &band_frame(
            PixelFormat::Nv12,
            ColorInfo {
                bt709: true,
                full_range: true,
            },
            0,
        ),
        3,
        "NV12 full",
    );
}

#[test]
fn yuv_i420p10_16bit_and_fallback() {
    let r = renderer();
    eprintln!("16-bit textures: {}", r.has_16bit_textures());
    check_bands(
        &r,
        &band_frame(PixelFormat::I420P10, LIMITED_709, 0),
        2,
        "P10",
    );
    check_bands(
        &r,
        &band_frame(
            PixelFormat::I420P10,
            ColorInfo {
                bt709: false,
                full_range: true,
            },
            6,
        ),
        2,
        "P10 full + pad",
    );
    r.disable_16bit_textures();
    assert!(!r.has_16bit_textures());
    check_bands(
        &r,
        &band_frame(PixelFormat::I420P10, LIMITED_709, 0),
        3,
        "P10 → 8-bit fallback",
    );
    check_bands(
        &r,
        &band_frame(PixelFormat::I420P10, LIMITED_709, 10),
        3,
        "P10 → 8-bit fallback + pad",
    );
}

#[test]
fn ten_bit_precision_survives_on_16bit_textures() {
    let r = renderer();
    if !r.has_16bit_textures() {
        eprintln!("skipped: no 16-bit textures on this adapter");
        return;
    }
    // A gentle luma ramp: 10-bit precision means neighbouring 10-bit codes stay distinct
    // before quantisation; check monotonic, smooth output instead of 4-code steps.
    let mut f = make_frame(
        256,
        4,
        PixelFormat::I420P10,
        ColorInfo {
            bt709: true,
            full_range: true,
        },
        0,
        |_, _| BLACK,
    );
    for y in 0..4 {
        for x in 0..256usize {
            let v = (300 + x) as u16;
            f.planes[0].data[(y * 256 + x) * 2..(y * 256 + x) * 2 + 2]
                .copy_from_slice(&v.to_le_bytes());
        }
    }
    for p in &mut f.planes[1..] {
        for s in p.data.as_chunks_mut::<2>().0.iter_mut() {
            s.copy_from_slice(&512u16.to_le_bytes());
        }
    }
    r.try_upload("s", &f).unwrap();
    let img = r
        .snapshot(
            "s",
            &TileParams {
                view: ViewMode::Raw,
                ..Default::default()
            },
            256,
            4,
        )
        .unwrap();
    for x in 0..256 {
        let want = (300.0 + x as f32) / 1023.0 * 255.0;
        let got = px(&img, 256, x, 1)[1] as f32;
        assert!((got - want).abs() <= 1.0, "x {x}: {got} vs {want}");
    }
}

#[test]
fn stride_padding_and_odd_sizes() {
    let r = renderer();
    for format in [PixelFormat::I420, PixelFormat::Nv12, PixelFormat::I420P10] {
        // Odd stride (13 bytes of padding) and an odd frame size.
        let f = make_frame(131, 17, format, LIMITED_709, 13, |x, _| {
            BANDS[((x / 16) as usize).min(7)]
        });
        r.try_upload("odd", &f).unwrap();
        let img = r
            .snapshot(
                "odd",
                &TileParams {
                    view: ViewMode::Raw,
                    ..Default::default()
                },
                131,
                17,
            )
            .unwrap();
        for i in 0..8u32 {
            assert_color(
                &img,
                131,
                i * 16 + 8,
                8,
                BANDS[i as usize],
                3,
                &format!("{format:?} padded band {i}"),
            );
        }
    }
}

#[test]
fn rejects_bad_frames_and_recreates_on_resize() {
    let r = renderer();
    let mut bad = make_frame(64, 32, PixelFormat::I420, LIMITED_709, 0, |_, _| RED);
    bad.planes[1].data.truncate(10);
    assert!(r.try_upload("s", &bad).is_err());
    assert!(
        r.try_upload(
            "s",
            &Frame {
                planes: vec![],
                ..bad.clone()
            }
        )
        .is_err()
    );
    assert_eq!(r.frame_size("s"), None);

    r.upload(
        "s",
        &make_frame(64, 32, PixelFormat::I420, LIMITED_709, 0, |_, _| RED),
    );
    r.upload(
        "s",
        &make_frame(64, 32, PixelFormat::I420, LIMITED_709, 0, |_, _| GREEN),
    );
    assert_eq!(r.frame_size("s"), Some((64, 32)));
    assert_eq!(r.stats().texture_creations, 1);
    r.upload(
        "s",
        &make_frame(32, 48, PixelFormat::Nv12, LIMITED_709, 0, |_, _| BLUE),
    );
    assert_eq!(r.frame_size("s"), Some((32, 48)));
    assert_eq!(r.stats().texture_creations, 2);
    let img = r
        .snapshot(
            "s",
            &TileParams {
                view: ViewMode::Raw,
                ..Default::default()
            },
            32,
            48,
        )
        .unwrap();
    assert_color(&img, 32, 16, 24, BLUE, 3, "after resize");
    assert_eq!(r.stats().uploads, 3);

    r.remove("s");
    assert_eq!(r.frame_size("s"), None);
    assert!(r.snapshot("s", &TileParams::default(), 8, 8).is_none());
}

// ---------- layouts, rotation, flips, modes ----------

/// Renders single eyes with the lens off and checks the colour at several points.
fn eye_colour(r: &VideoRenderer, layout: Layout, eye: Eye) -> Vec<u8> {
    let p = TileParams {
        layout,
        eye,
        lens: flat_lens(),
        ..Default::default()
    };
    r.snapshot("q", &p, 64, 64).unwrap()
}

#[test]
fn layouts_pick_the_right_eye() {
    let r = renderer();
    let (w, h) = (128, 64);
    r.try_upload(
        "q",
        &make_frame(
            w,
            h,
            PixelFormat::I420,
            LIMITED_709,
            0,
            quadrants(w, h, RED, GREEN, BLUE, YELLOW),
        ),
    )
    .unwrap();
    let cases = [
        (Layout::Quad { bottom_row: false }, Eye::Left, RED),
        (Layout::Quad { bottom_row: false }, Eye::Right, GREEN),
        (Layout::Quad { bottom_row: true }, Eye::Left, BLUE),
        (Layout::Quad { bottom_row: true }, Eye::Right, YELLOW),
    ];
    for (layout, eye, want) in cases {
        let img = eye_colour(&r, layout, eye);
        // Eye is 64x32 → letterboxed 64x32 in the middle of the 64x64 tile.
        for (x, y) in [(4, 20), (60, 20), (32, 32), (4, 44), (60, 44)] {
            assert_color(&img, 64, x, y, want, 4, &format!("{layout:?} {eye:?}"));
        }
        assert_color(&img, 64, 32, 4, BLACK, 0, &format!("{layout:?} letterbox"));
    }

    r.try_upload(
        "q",
        &make_frame(w, h, PixelFormat::I420, LIMITED_709, 0, |x, _| {
            if x < w / 2 { RED } else { BLUE }
        }),
    )
    .unwrap();
    for (eye, want) in [(Eye::Left, RED), (Eye::Right, BLUE)] {
        let img = eye_colour(&r, Layout::Sbs, eye);
        for (x, y) in [(2, 2), (61, 61), (32, 32)] {
            assert_color(&img, 64, x, y, want, 4, &format!("sbs {eye:?}"));
        }
    }

    r.try_upload(
        "q",
        &make_frame(64, 128, PixelFormat::I420, LIMITED_709, 0, |_, y| {
            if y < 64 { RED } else { BLUE }
        }),
    )
    .unwrap();
    for (eye, want) in [(Eye::Left, RED), (Eye::Right, BLUE)] {
        let img = eye_colour(&r, Layout::Tb, eye);
        for (x, y) in [(2, 2), (61, 61), (32, 32)] {
            assert_color(&img, 64, x, y, want, 4, &format!("tb {eye:?}"));
        }
    }

    // Mono: the whole frame is the eye (both eyes identical).
    r.try_upload(
        "q",
        &make_frame(
            64,
            64,
            PixelFormat::I420,
            LIMITED_709,
            0,
            quadrants(64, 64, RED, GREEN, BLUE, YELLOW),
        ),
    )
    .unwrap();
    for eye in [Eye::Left, Eye::Right] {
        let img = eye_colour(&r, Layout::Mono, eye);
        assert_color(&img, 64, 10, 10, RED, 4, "mono TL");
        assert_color(&img, 64, 54, 10, GREEN, 4, "mono TR");
        assert_color(&img, 64, 10, 54, BLUE, 4, "mono BL");
        assert_color(&img, 64, 54, 54, YELLOW, 4, "mono BR");
    }
}

#[test]
fn rotation_and_flips_map_quadrants() {
    let r = renderer();
    let (w, h) = (96, 48);
    r.try_upload(
        "q",
        &make_frame(
            w,
            h,
            PixelFormat::I420,
            LIMITED_709,
            0,
            quadrants(w, h, RED, GREEN, BLUE, WHITE),
        ),
    )
    .unwrap();
    // (rotation, flip_h, flip_v) → expected (TL, TR, BL, BR) of the displayed image.
    let cases = [
        (0, false, false, [RED, GREEN, BLUE, WHITE]),
        (90, false, false, [BLUE, RED, WHITE, GREEN]),
        (180, false, false, [WHITE, BLUE, GREEN, RED]),
        (270, false, false, [GREEN, WHITE, RED, BLUE]),
        (0, true, false, [GREEN, RED, WHITE, BLUE]),
        (0, false, true, [BLUE, WHITE, RED, GREEN]),
        (0, true, true, [WHITE, BLUE, GREEN, RED]),
        (90, true, false, [RED, BLUE, GREEN, WHITE]),
    ];
    let mut montage = Vec::new();
    for (rotation, flip_h, flip_v, want) in cases {
        let p = TileParams {
            view: ViewMode::Raw,
            rotation,
            flip_h,
            flip_v,
            ..Default::default()
        };
        let img = r.snapshot("q", &p, 120, 120).unwrap();
        let what = format!("rot {rotation} flip {flip_h}/{flip_v}");
        let (vx, vy, vw, vh) = if rotation % 180 == 90 {
            (30, 0, 60, 120)
        } else {
            (0, 30, 120, 60)
        };
        let at = |fx: u32, fy: u32| (vx + vw * fx / 4, vy + vh * fy / 4);
        for (i, (fx, fy)) in [(1, 1), (3, 1), (1, 3), (3, 3)].into_iter().enumerate() {
            let (x, y) = at(fx, fy);
            assert_color(&img, 120, x, y, want[i], 4, &what);
        }
        // Letterbox stays black.
        let (x, y) = if rotation % 180 == 90 {
            (10, 60)
        } else {
            (60, 10)
        };
        assert_color(&img, 120, x, y, BLACK, 0, &format!("{what} letterbox"));
        montage.push(img);
    }
    // 8 renders side by side.
    let mut m = vec![0u8; 120 * 8 * 120 * 4];
    for (i, img) in montage.iter().enumerate() {
        for y in 0..120 {
            let dst = (y * 960 + i * 120) * 4;
            m[dst..dst + 480].copy_from_slice(&img[y * 480..y * 480 + 480]);
        }
    }
    save_png("rotation-flip-montage.png", 960, 120, &m);
}

#[test]
fn stereo_shows_both_eyes_and_raw_the_whole_frame() {
    let r = renderer();
    let (w, h) = (128, 64);
    r.try_upload(
        "s",
        &make_frame(w, h, PixelFormat::I420, LIMITED_709, 0, |x, _| {
            if x < w / 2 { RED } else { BLUE }
        }),
    )
    .unwrap();
    let p = TileParams {
        view: ViewMode::Stereo,
        layout: Layout::Sbs,
        lens: flat_lens(),
        ..Default::default()
    };
    let img = r.snapshot("s", &p, 200, 100).unwrap();
    save_png("stereo-flat.png", 200, 100, &img);
    // Eye aspect 1 → two 100x100 squares.
    assert_color(&img, 200, 50, 50, RED, 4, "stereo left");
    assert_color(&img, 200, 2, 2, RED, 4, "stereo left corner");
    assert_color(&img, 200, 150, 50, BLUE, 4, "stereo right");
    assert_color(&img, 200, 197, 97, BLUE, 4, "stereo right corner");

    // Wide output aspect is clamped to square eyes: letterbox above and below.
    let p = TileParams {
        lens: LensParams {
            output_aspect: Some(16.0 / 9.0),
            ..flat_lens()
        },
        ..p
    };
    let img = r.snapshot("s", &p, 200, 200).unwrap();
    assert_color(&img, 200, 50, 5, BLACK, 0, "stereo letterbox");
    assert_color(&img, 200, 50, 100, RED, 4, "stereo wide left");
    assert_color(&img, 200, 150, 100, BLUE, 4, "stereo wide right");

    // Mono layout has no pair: stereo falls back to a single eye.
    let p = TileParams {
        layout: Layout::Mono,
        ..p
    };
    let img = r.snapshot("s", &p, 160, 90).unwrap();
    assert_color(&img, 160, 40, 45, RED, 4, "stereo mono left half");
    assert_color(&img, 160, 120, 45, BLUE, 4, "stereo mono right half");

    // Raw shows the whole frame (both halves), letterboxed to its own aspect.
    let raw = TileParams {
        view: ViewMode::Raw,
        ..Default::default()
    };
    let img = r.snapshot("s", &raw, 200, 200).unwrap();
    assert_color(&img, 200, 40, 100, RED, 4, "raw left");
    assert_color(&img, 200, 160, 100, BLUE, 4, "raw right");
    assert_color(&img, 200, 100, 20, BLACK, 0, "raw letterbox");
}

#[test]
fn blur_dims_and_softens_grid_draws() {
    let r = renderer();
    let (w, h) = (256, 128);
    r.try_upload(
        "s",
        &make_frame(w, h, PixelFormat::I420, LIMITED_709, 0, |x, y| {
            if (x / 3 + y / 3) % 2 == 0 {
                WHITE
            } else {
                BLACK
            }
        }),
    )
    .unwrap();
    let base = TileParams {
        layout: Layout::Sbs,
        lens: LensParams::default(),
        ..Default::default()
    };
    let sharp = r.snapshot("s", &base, 320, 180).unwrap();
    let blurred = r
        .snapshot("s", &TileParams { blur: true, ..base }, 320, 180)
        .unwrap();
    save_png("blur-sharp.png", 320, 180, &sharp);
    save_png("blur-blurred.png", 320, 180, &blurred);
    let mean = |img: &[u8]| {
        img.as_chunks::<4>()
            .0
            .iter()
            .map(|c| c[1] as f64)
            .sum::<f64>()
            / (img.len() / 4) as f64
    };
    let ratio = mean(&blurred) / mean(&sharp);
    assert!(
        (0.45..0.65).contains(&ratio),
        "blur dims to ~0.55, got {ratio}"
    );
    // Softer: less edge energy relative to brightness.
    let edges = |img: &[u8]| {
        let mut e = 0.0;
        for row in img.as_chunks::<{ 320 * 4 }>().0.iter() {
            for x in 0..319 {
                e += (row[x * 4 + 5] as f64 - row[x * 4 + 1] as f64).abs();
            }
        }
        e / mean(img)
    };
    let soft = edges(&blurred) / edges(&sharp);
    assert!(soft < 0.7, "blurred edge energy ratio {soft}");
    assert_ne!(sharp, blurred);

    // Calibration grid: green lines on a black frame, axis through the centre.
    r.try_upload(
        "k",
        &make_frame(128, 64, PixelFormat::I420, LIMITED_709, 0, |_, _| BLACK),
    )
    .unwrap();
    let off = r.snapshot("k", &base, 320, 180).unwrap();
    let on = r
        .snapshot("k", &TileParams { grid: true, ..base }, 320, 180)
        .unwrap();
    save_png("grid-overlay.png", 320, 180, &on);
    let green = |img: &[u8]| {
        img.as_chunks::<4>()
            .0
            .iter()
            .filter(|c| c[1] > 100 && c[0] < 100)
            .count()
    };
    assert_eq!(green(&off), 0);
    assert!(green(&on) > 500, "grid pixels: {}", green(&on));
    // The axes cross the centre of the viewport.
    assert!(px(&on, 320, 160, 40)[1] > 120, "vertical axis");
    assert!(px(&on, 320, 40, 90)[1] > 120, "horizontal axis");
    // No grid in raw mode (like the JS).
    let raw = r
        .snapshot(
            "k",
            &TileParams {
                grid: true,
                view: ViewMode::Raw,
                ..base
            },
            320,
            180,
        )
        .unwrap();
    assert_eq!(green(&raw), 0);
}

#[test]
fn analyze_luma_sees_the_logical_frame() {
    let r = renderer();
    r.try_upload(
        "a",
        &make_frame(
            64,
            32,
            PixelFormat::I420,
            LIMITED_709,
            0,
            quadrants(64, 32, WHITE, BLACK, BLACK, BLACK),
        ),
    )
    .unwrap();
    let l = r.analyze_luma("a", 0, false, false, 16).unwrap();
    assert_eq!(l.len(), 256);
    assert!(l[16 + 2] > 0.95 && l[16 + 13] < 0.05 && l[14 * 16 + 2] < 0.05);
    // Rotated 90° clockwise: the white top-left quadrant moves to the top right.
    let l = r.analyze_luma("a", 90, false, false, 16).unwrap();
    assert!(l[16 + 13] > 0.95 && l[16 + 2] < 0.05);
}

// ---------- lens correction ----------

const K1: f64 = -0.22;
const K2: f64 = 0.04;

/// Solves s · (1 + k1 s² + k2 s⁴) = d for s ≥ 0 (monotonic for these k).
fn undistort_radius(d: f64) -> f64 {
    let mut s = d;
    for _ in 0..30 {
        let s2 = s * s;
        let f = s * (1.0 + K1 * s2 + K2 * s2 * s2) - d;
        let df = 1.0 + 3.0 * K1 * s2 + 5.0 * K2 * s2 * s2;
        s -= f / df;
    }
    s
}

/// The scene (in lens-space q units): white grid lines every 0.25, 0.012 thick, on dark grey.
fn scene(q: [f64; 2]) -> f64 {
    let near = |v: f64| ((v * 4.0) - (v * 4.0).round()).abs() / 4.0 < 0.006;
    if near(q[0]) || near(q[1]) { 1.0 } else { 0.1 }
}

/// Side-by-side panel: each eye shows the scene barrel pre-distorted with K1/K2 around
/// a lens centre at `cx` (left eye) / `-cx` (right eye), like a headset compositor would.
fn distorted_sbs(eye_w: u32, eye_h: u32, cx: f64) -> Frame {
    let w = eye_w * 2;
    let aspect = eye_w as f64 / eye_h as f64;
    let ss = 3;
    make_frame(w, eye_h, PixelFormat::I420, LIMITED_709, 0, move |x, y| {
        let right = x >= eye_w;
        let center = if right { -cx } else { cx };
        let ex0 = (x % eye_w) as f64;
        let mut acc = 0.0;
        for sy in 0..ss {
            for sx in 0..ss {
                let ex = (ex0 + (sx as f64 + 0.5) / ss as f64) / eye_w as f64;
                let ey = 1.0 - (y as f64 + (sy as f64 + 0.5) / ss as f64) / eye_h as f64; // y up
                let n = [ex * 2.0 - 1.0 - center, ey * 2.0 - 1.0];
                let d = [n[0] * aspect, n[1]];
                let dl = (d[0] * d[0] + d[1] * d[1]).sqrt();
                let q = if dl < 1e-9 {
                    d
                } else {
                    let s = undistort_radius(dl);
                    [d[0] / dl * s, d[1] / dl * s]
                };
                acc += scene(q);
            }
        }
        let v = (acc / (ss * ss) as f64) as f32;
        [v, v, v]
    })
}

/// Rows (sub-pixel centroid of brightness) of the horizontal line near `row` at each column.
fn line_rows(
    img: &[u8],
    w: u32,
    row: u32,
    window: u32,
    cols: impl Iterator<Item = u32>,
) -> Vec<(u32, f64)> {
    cols.filter_map(|x| {
        let mut sum = 0.0;
        let mut wsum = 0.0;
        for y in row.saturating_sub(window)..row + window {
            let v = (px(img, w, x, y)[1] as f64 - 60.0).max(0.0);
            sum += v * y as f64;
            wsum += v;
        }
        (wsum > 50.0).then(|| (x, sum / wsum))
    })
    .collect()
}

#[test]
fn lens_correction_straightens_grid_lines() {
    let r = renderer();
    let (ew, eh) = (480, 480);
    let panel = distorted_sbs(ew, eh, 0.08);
    r.try_upload("lens", &panel).unwrap();
    let panel_rgba = r
        .snapshot(
            "lens",
            &TileParams {
                view: ViewMode::Raw,
                ..Default::default()
            },
            ew * 2,
            eh,
        )
        .unwrap();
    save_png("lens-panel-distorted.png", ew * 2, eh, &panel_rgba);

    let zoom = 1.25f32;
    let lens = LensParams {
        k1: K1 as f32,
        k2: K2 as f32,
        zoom,
        center_x: 0.08,
        output_aspect: None,
        ..Default::default()
    };
    let (ow, oh) = (480u32, 480u32);
    // Scene line q.y = 0.5 appears at p.y = 0.5 · zoom → row (1 - p.y)/2 · H.
    let expected_row = ((1.0 - 0.5 * zoom as f64) / 2.0 * oh as f64) as u32;
    // Columns away from the scene's vertical lines (q.x multiples of 0.25).
    let clear_col = |x: u32, width: u32| {
        let qx = ((x as f64 + 0.5) / width as f64 * 2.0 - 1.0) / zoom as f64;
        ((qx * 4.0) - (qx * 4.0).round()).abs() / 4.0 > 0.02
    };
    for eye in [Eye::Left, Eye::Right] {
        let p = TileParams {
            eye,
            layout: Layout::Sbs,
            lens,
            ..Default::default()
        };
        let img = r.snapshot("lens", &p, ow, oh).unwrap();
        save_png(
            &format!("lens-corrected-{eye:?}.png").to_lowercase(),
            ow,
            oh,
            &img,
        );
        let rows = line_rows(
            &img,
            ow,
            expected_row,
            12,
            (20..ow - 20).step_by(2).filter(|&x| clear_col(x, ow)),
        );
        assert!(rows.len() > 80, "line found in {} columns", rows.len());
        let min = rows.iter().map(|r| r.1).fold(f64::MAX, f64::min);
        let max = rows.iter().map(|r| r.1).fold(f64::MIN, f64::max);
        eprintln!("{eye:?}: line rows {min:.2}..{max:.2} (expected ~{expected_row})");
        assert!(
            max - min <= 2.0,
            "{eye:?}: corrected line bends by {:.2} px",
            max - min
        );
        assert!(
            (min - expected_row as f64).abs() <= 2.0 && (max - expected_row as f64).abs() <= 2.0
        );
        // Within ±1 px of the mean row across the width.
        let mean = rows.iter().map(|r| r.1).sum::<f64>() / rows.len() as f64;
        assert!(
            rows.iter().all(|r| (r.1 - mean).abs() <= 1.0),
            "{eye:?}: not within ±1 px of row {mean:.2}"
        );
    }

    // Sanity: without correction the same line is visibly curved.
    let p = TileParams {
        layout: Layout::Sbs,
        lens: LensParams {
            k1: 0.0,
            k2: 0.0,
            ..lens
        },
        ..Default::default()
    };
    let img = r.snapshot("lens", &p, ow, oh).unwrap();
    save_png("lens-uncorrected-left.png", ow, oh, &img);
    // The pre-distorted q.y = 0.5 line: ~row 98 near the centre, ~row 115 near the edge.
    let centre_row = line_rows(&img, ow, 106, 20, [ow / 2 + 30].into_iter());
    let edge_row = line_rows(&img, ow, 106, 20, [40].into_iter());
    eprintln!("uncorrected: centre {centre_row:?}, edge {edge_row:?}");
    let bend = (centre_row[0].1 - edge_row[0].1).abs();
    assert!(bend > 8.0, "uncorrected bend only {bend:.2} px");

    // Stereo: both eyes corrected side by side, same line row in each half.
    let p = TileParams {
        view: ViewMode::Stereo,
        layout: Layout::Sbs,
        lens,
        ..Default::default()
    };
    let img = r.snapshot("lens", &p, 2 * ow, oh).unwrap();
    save_png("lens-corrected-stereo.png", 2 * ow, oh, &img);
    let rows = line_rows(
        &img,
        2 * ow,
        expected_row,
        12,
        (20..2 * ow - 20)
            .step_by(2)
            .filter(|&x| x.abs_diff(ow) > 20 && clear_col(x % ow, ow)),
    );
    let mean = rows.iter().map(|r| r.1).sum::<f64>() / rows.len() as f64;
    assert!(
        rows.iter().all(|r| (r.1 - mean).abs() <= 1.0),
        "stereo rows not straight"
    );
}
