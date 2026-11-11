//! The real egui path, headless: `VideoRenderer::paint` → egui-wgpu callbacks
//! (prepare / finish_prepare / paint) in egui's render pass, at a fractional
//! `pixels_per_point`, with a clip rect, over several frames.

use std::sync::Arc;

use egui::mutex::RwLock;
use xrmc_decode::{ColorInfo, Frame, PixelFormat, Plane};
use xrmc_render::{Layout, LensParams, TileParams, VideoRenderer, ViewMode};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const PPP: f32 = 1.5;
const SIZE_PT: [f32; 2] = [400.0, 300.0];
const SIZE_PX: [u32; 2] = [600, 450];

fn render_state() -> egui_wgpu::RenderState {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("no GPU adapter");
    let (device, queue) = pollster::block_on(
        adapter.request_device(&wgpu::DeviceDescriptor {
            required_limits: wgpu::Limits::downlevel_defaults()
                .using_resolution(adapter.limits())
                .using_alignment(adapter.limits()),
            ..Default::default()
        }),
    )
    .expect("no GPU device");
    let renderer = egui_wgpu::Renderer::new(&device, FORMAT, egui_wgpu::RendererOptions::default());
    egui_wgpu::RenderState {
        adapter,
        available_adapters: Vec::new(),
        instance,
        device,
        queue,
        target_format: FORMAT,
        renderer: Arc::new(RwLock::new(renderer)),
    }
}

/// Solid 4:2:0 frame (limited BT.709) with the left half `a` and the right half `b` (Y, U, V codes).
fn halves(w: u32, h: u32, a: [u8; 3], b: [u8; 3]) -> Frame {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let plane = |pw: u32, ph: u32, k: usize, half: u32| Plane {
        data: (0..ph)
            .flat_map(|_| (0..pw).map(move |x| if x < half { a[k] } else { b[k] }))
            .collect(),
        stride: pw as usize,
        width: pw,
        height: ph,
    };
    Frame {
        width: w,
        height: h,
        format: PixelFormat::I420,
        color: ColorInfo {
            bt709: true,
            full_range: false,
        },
        planes: vec![
            plane(w, h, 0, w / 2),
            plane(cw, ch, 1, cw / 2),
            plane(cw, ch, 2, cw / 2),
        ],
        pts: 0,
    }
}

// Limited-range BT.709 codes.
const RED: [u8; 3] = [63, 102, 240];
const BLUE: [u8; 3] = [32, 240, 118];
const GREEN: [u8; 3] = [173, 42, 26];

fn draw_frame(rs: &egui_wgpu::RenderState, ctx: &egui::Context, video: &VideoRenderer) -> Vec<u8> {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SIZE_PT.into())),
        ..Default::default()
    };
    input
        .viewports
        .get_mut(&egui::ViewportId::ROOT)
        .unwrap()
        .native_pixels_per_point = Some(PPP);
    let mut out = ctx.run_ui(input, |ui| {
        let painter = ui.painter();
        painter.rect_filled(ui.max_rect(), 0.0, egui::Color32::from_gray(40));
        let raw = TileParams {
            view: ViewMode::Raw,
            ..Default::default()
        };
        let flat = LensParams {
            k1: 0.0,
            k2: 0.0,
            zoom: 1.0,
            output_aspect: None,
            ..Default::default()
        };
        // A: raw, red | blue.
        video.paint(
            painter,
            egui::Rect::from_min_max(egui::pos2(10.0, 10.0), egui::pos2(190.0, 110.0)),
            "a",
            &raw,
        );
        // B: stereo pair of the same stream.
        let stereo = TileParams {
            view: ViewMode::Stereo,
            layout: Layout::Sbs,
            lens: flat,
            ..Default::default()
        };
        video.paint(
            painter,
            egui::Rect::from_min_max(egui::pos2(200.0, 10.0), egui::pos2(390.0, 140.0)),
            "a",
            &stereo,
        );
        // C: green, clipped to its left half.
        let clipped = painter.with_clip_rect(egui::Rect::from_min_max(
            egui::pos2(10.0, 150.0),
            egui::pos2(100.0, 290.0),
        ));
        video.paint(
            &clipped,
            egui::Rect::from_min_max(egui::pos2(10.0, 150.0), egui::pos2(190.0, 290.0)),
            "c",
            &raw,
        );
        // Unknown stream: draws nothing.
        video.paint(
            painter,
            egui::Rect::from_min_max(egui::pos2(200.0, 150.0), egui::pos2(390.0, 290.0)),
            "missing",
            &raw,
        );
    });
    let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
    assert_eq!(out.pixels_per_point, PPP);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: SIZE_PX,
        pixels_per_point: PPP,
    };

    let (device, queue) = (&rs.device, &rs.queue);
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d {
            width: SIZE_PX[0],
            height: SIZE_PX[1],
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let mut enc = device.create_command_encoder(&Default::default());
    let mut cmds = {
        let mut r = rs.renderer.write();
        for (id, deltas) in &out.textures_delta.set {
            for delta in deltas {
                r.update_texture(device, queue, *id, delta);
            }
        }
        out.textures_delta.clear();
        r.update_buffers(device, queue, &mut enc, &prims, &screen)
    };
    {
        let pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rs.renderer
            .read()
            .render(&mut pass.forget_lifetime(), &prims, &screen);
    }
    let padded = (SIZE_PX[0] * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: padded as u64 * SIZE_PX[1] as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    enc.copy_texture_to_buffer(
        target.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: SIZE_PX[0],
            height: SIZE_PX[1],
            depth_or_array_layers: 1,
        },
    );
    cmds.push(enc.finish());
    queue.submit(cmds);
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    let mapped = buf.slice(..).get_mapped_range().unwrap();
    let mut img = Vec::new();
    for y in 0..SIZE_PX[1] as usize {
        img.extend_from_slice(
            &mapped[y * padded as usize..y * padded as usize + SIZE_PX[0] as usize * 4],
        );
    }
    img
}

fn at(img: &[u8], x_pt: f32, y_pt: f32) -> [u8; 3] {
    let (x, y) = ((x_pt * PPP) as usize, (y_pt * PPP) as usize);
    let i = (y * SIZE_PX[0] as usize + x) * 4;
    [img[i], img[i + 1], img[i + 2]]
}

fn is(c: [u8; 3], want: [u8; 3]) -> bool {
    (0..3).all(|i| (c[i] as i32 - want[i] as i32).abs() <= 6)
}

#[test]
fn tiles_render_inside_egui_pass() {
    let rs = render_state();
    let video = VideoRenderer::install(&rs);
    let ctx = egui::Context::default();
    video.upload("a", &halves(128, 64, RED, BLUE));
    video.upload("c", &halves(64, 64, GREEN, GREEN));

    for frame in 0..3 {
        let img = draw_frame(&rs, &ctx, &video);
        if frame == 0 {
            let dir = std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".local/share/xrmc-tools/render-tests");
            std::fs::create_dir_all(&dir).unwrap();
            let file =
                std::io::BufWriter::new(std::fs::File::create(dir.join("egui-pass.png")).unwrap());
            let mut enc = png::Encoder::new(file, SIZE_PX[0], SIZE_PX[1]);
            enc.set_color(png::ColorType::Rgba);
            enc.write_header().unwrap().write_image_data(&img).unwrap();
        }
        let bg = [40, 40, 40];
        // A: raw 2:1 frame letterboxed into 180x100 pt → 180x90 pt, y 15..105.
        assert!(
            is(at(&img, 40.0, 60.0), [255, 0, 0]),
            "A left {:?}",
            at(&img, 40.0, 60.0)
        );
        assert!(
            is(at(&img, 160.0, 60.0), [0, 0, 255]),
            "A right {:?}",
            at(&img, 160.0, 60.0)
        );
        assert!(
            is(at(&img, 100.0, 12.0), bg),
            "A letterbox {:?}",
            at(&img, 100.0, 12.0)
        );
        // B: two square eyes (130x130 pt each would overflow) → 190x95 pt centred vertically.
        assert!(
            is(at(&img, 250.0, 75.0), [255, 0, 0]),
            "B left eye {:?}",
            at(&img, 250.0, 75.0)
        );
        assert!(
            is(at(&img, 340.0, 75.0), [0, 0, 255]),
            "B right eye {:?}",
            at(&img, 340.0, 75.0)
        );
        assert!(
            is(at(&img, 295.0, 15.0), bg),
            "B letterbox {:?}",
            at(&img, 295.0, 15.0)
        );
        // C: clipped at x = 100 pt.
        assert!(
            is(at(&img, 60.0, 220.0), [0, 255, 0]),
            "C visible {:?}",
            at(&img, 60.0, 220.0)
        );
        assert!(
            is(at(&img, 140.0, 220.0), bg),
            "C clipped {:?}",
            at(&img, 140.0, 220.0)
        );
        // Missing stream: background only.
        assert!(is(at(&img, 300.0, 220.0), bg));
        // New frames between UI frames are picked up.
        video.upload("c", &halves(64, 64, BLUE, BLUE));
        let img2 = draw_frame(&rs, &ctx, &video);
        assert!(
            is(at(&img2, 60.0, 220.0), [0, 0, 255]),
            "C updated {:?}",
            at(&img2, 60.0, 220.0)
        );
        video.upload("c", &halves(64, 64, GREEN, GREEN));
    }
}
