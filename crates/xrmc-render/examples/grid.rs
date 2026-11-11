//! N animated test-pattern tiles drawn by `xrmc-render` inside an eframe window.
//!
//! ```text
//! cargo run --release -p xrmc-render --example grid -- 9
//! options: --size 1280x720   source frame size
//!          --fps 60          source frame rate (uploads per stream per second)
//!          --seconds S       quit after S seconds
//!          --screenshot P    save a PNG of the window (egui screenshot) after ~2 s
//!          --snapshot P      save an offscreen `VideoRenderer::snapshot` of tile 1
//!          --format F        i420 (default), nv12, p10 (10-bit planar) or p010
//!                            (10-bit Y + interleaved UV, MSB-aligned; hardware decoders)
//! ```
//!
//! Each stream gets a new frame `fps` times per second, uploaded from the UI thread
//! (frames are pre-generated so the measurement shows the renderer, not the generator).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use xrmc_decode::{PixelFormat, Plane};
use xrmc_render::{Eye, Frame, Layout, LensParams, TileParams, VideoRenderer, ViewMode};

const PHASES: u32 = 16;

struct Args {
    tiles: usize,
    size: (u32, u32),
    fps: f32,
    seconds: Option<f32>,
    screenshot: Option<PathBuf>,
    snapshot: Option<PathBuf>,
    format: PixelFormat,
}

fn parse_args() -> Args {
    let mut a = Args {
        tiles: 9,
        size: (1280, 720),
        fps: 60.0,
        seconds: None,
        screenshot: None,
        snapshot: None,
        format: PixelFormat::I420,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || {
            it.next()
                .unwrap_or_else(|| panic!("missing value for {arg}"))
        };
        match arg.as_str() {
            "--size" => {
                let v = val();
                let (w, h) = v.split_once('x').expect("--size WxH");
                a.size = (w.parse().expect("width"), h.parse().expect("height"));
            }
            "--fps" => a.fps = val().parse().expect("--fps F"),
            "--seconds" => a.seconds = Some(val().parse().expect("--seconds S")),
            "--screenshot" => a.screenshot = Some(val().into()),
            "--snapshot" => a.snapshot = Some(val().into()),
            "--format" => {
                a.format = match val().as_str() {
                    "i420" => PixelFormat::I420,
                    "nv12" => PixelFormat::Nv12,
                    "p10" => PixelFormat::I420P10,
                    "p010" => PixelFormat::P010,
                    f => panic!("unknown format {f}"),
                }
            }
            n => a.tiles = n.parse().unwrap_or_else(|_| panic!("unknown argument {n}")),
        }
    }
    a.tiles = a.tiles.clamp(1, 64);
    a
}

#[derive(Clone, Copy, PartialEq)]
enum ModeOverride {
    Mixed,
    Corrected,
    Stereo,
    Raw,
}

/// A different look per tile so every shader path is on screen.
fn preset(i: usize) -> (TileParams, &'static str) {
    let lens = LensParams::default();
    let base = TileParams {
        lens,
        ..Default::default()
    };
    match i % 8 {
        0 => (base, "corrected"),
        1 => (
            TileParams {
                view: ViewMode::Stereo,
                ..base
            },
            "stereo",
        ),
        2 => (
            TileParams {
                view: ViewMode::Raw,
                ..base
            },
            "raw",
        ),
        3 => (TileParams { grid: true, ..base }, "corrected + grid"),
        4 => (
            TileParams {
                eye: Eye::Right,
                lens: LensParams {
                    chroma_r: 1.02,
                    chroma_b: 0.98,
                    ..lens
                },
                ..base
            },
            "right eye, chroma",
        ),
        5 => (
            TileParams {
                view: ViewMode::Raw,
                rotation: 90,
                ..base
            },
            "raw, 90°",
        ),
        6 => (
            TileParams {
                layout: Layout::Mono,
                flip_h: true,
                ..base
            },
            "mono, flipped",
        ),
        _ => (TileParams { blur: true, ..base }, "blurred"),
    }
}

struct App {
    video: VideoRenderer,
    frames: Arc<Vec<Frame>>,
    args: Args,
    phase: Vec<u32>,
    start: Instant,
    mode: ModeOverride,
    grid: bool,
    blur: bool,
    lens: LensParams,
    meter: (Instant, u64, xrmc_render::RenderStats),
    line: String,
    shot_requested: bool,
    adapter: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, args: Args) -> App {
        let rs = cc
            .wgpu_render_state
            .as_ref()
            .expect("wgpu renderer required");
        let video = VideoRenderer::install(rs);
        let info = rs.adapter.get_info();
        let adapter = format!(
            "{:?} · {} · 16-bit textures: {}",
            info.backend,
            info.name,
            video.has_16bit_textures()
        );
        eprintln!("adapter: {adapter}, target {:?}", rs.target_format);
        let t0 = Instant::now();
        let (w, h) = args.size;
        let frames: Vec<Frame> = (0..PHASES)
            .map(|p| {
                convert(
                    xrmc_decode::test_pattern(w, h, p * 64 / PHASES),
                    args.format,
                )
            })
            .collect();
        eprintln!(
            "generated {PHASES} {w}x{h} {:?} frames in {:?}",
            args.format,
            t0.elapsed()
        );
        App {
            video,
            frames: Arc::new(frames),
            phase: vec![u32::MAX; args.tiles],
            args,
            start: Instant::now(),
            mode: ModeOverride::Mixed,
            grid: false,
            blur: false,
            lens: LensParams::default(),
            meter: (Instant::now(), 0, Default::default()),
            line: String::new(),
            shot_requested: false,
            adapter,
        }
    }

    fn upload_new_frames(&mut self) {
        let t = self.start.elapsed().as_secs_f32();
        for i in 0..self.args.tiles {
            // Tiles are offset in phase so they don't all look alike.
            let p = ((t * self.args.fps) as u64 + i as u64 * 3) as u32 % PHASES;
            if p != self.phase[i] {
                self.phase[i] = p;
                self.video
                    .upload(&format!("tile{i}"), &self.frames[p as usize]);
            }
        }
    }

    fn update_meter(&mut self) {
        self.meter.1 += 1;
        let dt = self.meter.0.elapsed().as_secs_f32();
        if dt >= 2.0 {
            let s = self.video.stats();
            let uploads = (s.uploads - self.meter.2.uploads) as f32 / dt;
            let mb = (s.upload_bytes - self.meter.2.upload_bytes) as f32 / dt / 1e6;
            self.line = format!(
                "{:.0} fps · {uploads:.0} uploads/s · {mb:.0} MB/s · {} tiles",
                self.meter.1 as f32 / dt,
                self.args.tiles
            );
            eprintln!("stats: {}", self.line);
            self.meter = (Instant::now(), 0, s);
        }
    }

    fn params(&self, i: usize) -> (TileParams, String) {
        let (mut p, name) = preset(i);
        match self.mode {
            ModeOverride::Mixed => {}
            ModeOverride::Corrected => p.view = ViewMode::Corrected,
            ModeOverride::Stereo => p.view = ViewMode::Stereo,
            ModeOverride::Raw => p.view = ViewMode::Raw,
        }
        p.lens = LensParams {
            chroma_r: p.lens.chroma_r,
            chroma_b: p.lens.chroma_b,
            ..self.lens
        };
        p.grid |= self.grid;
        p.blur |= self.blur;
        (p, format!("{} · {name}", i + 1))
    }

    fn automation(&mut self, ctx: &egui::Context) {
        let t = self.start.elapsed();
        if t > Duration::from_secs(2) && !self.shot_requested {
            self.shot_requested = true;
            if let Some(path) = self.args.screenshot.clone() {
                ctx.request_screenshot(move |img| {
                    let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                    save_png(&path, img.size[0] as u32, img.size[1] as u32, &rgba);
                });
            }
            if let Some(path) = &self.args.snapshot {
                let (p, _) = self.params(0);
                if let Some(rgba) = self.video.snapshot("tile0", &p, 960, 540) {
                    save_png(path, 960, 540, &rgba);
                }
            }
        }
        if let Some(s) = self.args.seconds
            && t.as_secs_f32() >= s
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// The I420 test pattern in another layout.
fn convert(f: Frame, format: PixelFormat) -> Frame {
    let plane = |data: Vec<u8>, bps: usize, p: &Plane| Plane {
        data,
        stride: p.width as usize * bps,
        width: p.width,
        height: p.height,
    };
    let planes = match format {
        PixelFormat::I420 => return f,
        PixelFormat::Nv12 => {
            let (u, v) = (&f.planes[1], &f.planes[2]);
            let uv = u
                .data
                .iter()
                .zip(&v.data)
                .flat_map(|(&a, &b)| [a, b])
                .collect();
            vec![f.planes[0].clone(), plane(uv, 2, u)]
        }
        PixelFormat::P010 => {
            // 8-bit code → 10-bit (<< 2) → MSB-aligned (<< 6).
            let msb = |v: u8| (u16::from(v) << 8).to_le_bytes();
            let (u, v) = (&f.planes[1], &f.planes[2]);
            let y = f.planes[0].data.iter().flat_map(|&s| msb(s)).collect();
            let uv = u
                .data
                .iter()
                .zip(&v.data)
                .flat_map(|(&a, &b)| [msb(a), msb(b)].concat())
                .collect();
            vec![plane(y, 2, &f.planes[0]), plane(uv, 4, u)]
        }
        PixelFormat::I420P10 => f
            .planes
            .iter()
            .map(|p| {
                plane(
                    p.data
                        .iter()
                        .flat_map(|&v| ((v as u16) << 2).to_le_bytes())
                        .collect(),
                    2,
                    p,
                )
            })
            .collect(),
    };
    Frame {
        format,
        planes,
        ..f
    }
}

fn save_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) {
    let file = std::io::BufWriter::new(std::fs::File::create(path).expect("create png"));
    let mut enc = png::Encoder::new(file, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.write_header()
        .and_then(|mut wr| wr.write_image_data(rgba))
        .expect("write png");
    eprintln!("saved {}", path.display());
}

/// Splits `area` into `n` equal tiles of `aspect`, picking the column count with the largest tiles.
fn grid_layout(area: egui::Rect, n: usize, aspect: f32, gap: f32) -> Vec<egui::Rect> {
    let mut best = (1, 0.0f32);
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let w = (area.width() - gap * (cols as f32 - 1.0)) / cols as f32;
        let h = (area.height() - gap * (rows as f32 - 1.0)) / rows as f32;
        let tile_w = w.min(h * aspect);
        if tile_w > best.1 {
            best = (cols, tile_w);
        }
    }
    let (cols, tw) = best;
    let th = tw / aspect;
    (0..n)
        .map(|i| {
            egui::Rect::from_min_size(
                area.min
                    + egui::vec2(
                        (i % cols) as f32 * (tw + gap),
                        (i / cols) as f32 * (th + gap),
                    ),
                egui::vec2(tw, th),
            )
        })
        .collect()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.upload_new_frames();
        self.update_meter();
        self.automation(&ctx);

        egui::Panel::top("controls").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("xrmc-render grid").strong());
                ui.label(&self.line);
                ui.separator();
                ui.label(RichText::new(&self.adapter).small().weak());
            });
            ui.horizontal_wrapped(|ui| {
                egui::ComboBox::from_label("view")
                    .selected_text(match self.mode {
                        ModeOverride::Mixed => "mixed",
                        ModeOverride::Corrected => "corrected",
                        ModeOverride::Stereo => "stereo",
                        ModeOverride::Raw => "raw",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.mode, ModeOverride::Mixed, "mixed");
                        ui.selectable_value(&mut self.mode, ModeOverride::Corrected, "corrected");
                        ui.selectable_value(&mut self.mode, ModeOverride::Stereo, "stereo");
                        ui.selectable_value(&mut self.mode, ModeOverride::Raw, "raw");
                    });
                ui.checkbox(&mut self.grid, "grid");
                ui.checkbox(&mut self.blur, "blur");
                ui.add(egui::Slider::new(&mut self.lens.k1, -0.6..=0.3).text("k1"));
                ui.add(egui::Slider::new(&mut self.lens.k2, -0.2..=0.2).text("k2"));
                ui.add(egui::Slider::new(&mut self.lens.zoom, 0.5..=3.0).text("zoom"));
                ui.add(egui::Slider::new(&mut self.lens.center_x, -0.5..=0.5).text("centre x"));
            });
        });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_gray(12))
                    .inner_margin(6.0),
            )
            .show(ui, |ui| {
                let area = ui.available_rect_before_wrap();
                let rects = grid_layout(area, self.args.tiles, 16.0 / 9.0, 6.0);
                for (i, rect) in rects.into_iter().enumerate() {
                    let (p, label) = self.params(i);
                    ui.painter().rect_filled(rect, 4.0, Color32::from_gray(24));
                    self.video
                        .paint(ui.painter(), rect, &format!("tile{i}"), &p);
                    let painter = ui.painter().with_clip_rect(rect);
                    let galley = painter.layout_no_wrap(
                        label,
                        egui::FontId::proportional(13.0),
                        Color32::WHITE,
                    );
                    let pill = egui::Rect::from_min_size(
                        rect.min + egui::vec2(6.0, 6.0),
                        galley.size() + egui::vec2(12.0, 6.0),
                    );
                    painter.rect_filled(pill, 5.0, Color32::from_black_alpha(170));
                    painter.galley(pill.min + egui::vec2(6.0, 3.0), galley, Color32::WHITE);
                }
            });
        ctx.request_repaint();
    }
}

fn main() -> eframe::Result {
    let args = parse_args();
    if let Some(s) = args.seconds {
        // A hidden Wayland window gets no frame callbacks; make scripted runs end anyway.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f32(s + 5.0));
            eprintln!("watchdog: exiting");
            std::process::exit(0);
        });
    }
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("xrmc-render grid")
            .with_inner_size([1600.0, 900.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    if std::env::var_os("XRMC_NO_16BIT").is_none() {
        xrmc_render::request_optional_features(&mut options.wgpu_options.wgpu_setup);
    }
    eframe::run_native(
        "xrmc-render grid",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, args)))),
    )
}
