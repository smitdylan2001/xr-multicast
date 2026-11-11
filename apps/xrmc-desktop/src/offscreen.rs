//! Runs the app without a window, rendering egui (and the video tiles) into an
//! offscreen texture at any size: deterministic screenshots of portrait
//! touchscreens, small SBC screens and large monitors from any machine.
//! Used with the hidden `--offscreen WxH --screenshot PATH --exit-after SECS` flags.

use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, mutex::RwLock};
use eframe::{egui_wgpu, wgpu};

use crate::ui::{App, AppOptions, save_png};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn render_state() -> Result<egui_wgpu::RenderState, String> {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .map_err(|e| format!("no GPU adapter: {e}"))?;
    let features = adapter.features() & xrmc_render::OPTIONAL_FEATURES;
    let (device, queue) = pollster::block_on(
        adapter.request_device(&wgpu::DeviceDescriptor {
            required_features: features,
            required_limits: wgpu::Limits {
                max_texture_dimension_2d: 8192,
                ..wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits())
            }
            .using_alignment(adapter.limits()),
            ..Default::default()
        }),
    )
    .map_err(|e| format!("no GPU device: {e}"))?;
    let renderer = egui_wgpu::Renderer::new(&device, FORMAT, egui_wgpu::RendererOptions::default());
    Ok(egui_wgpu::RenderState {
        adapter,
        available_adapters: Vec::new(),
        instance,
        device,
        queue,
        target_format: FORMAT,
        renderer: Arc::new(RwLock::new(renderer)),
    })
}

fn draw(
    rs: &egui_wgpu::RenderState,
    mut out: egui::FullOutput,
    ctx: &egui::Context,
    size: [u32; 2],
) -> Vec<u8> {
    let prims = ctx.tessellate(out.shapes, out.pixels_per_point);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: size,
        pixels_per_point: out.pixels_per_point,
    };
    let (device, queue) = (&rs.device, &rs.queue);
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen"),
        size: wgpu::Extent3d {
            width: size[0],
            height: size[1],
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
        r.update_buffers(device, queue, &mut enc, &prims, &screen)
    };
    {
        let bg = egui::Rgba::from(crate::theme::BG);
        let pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: bg.r() as f64,
                        g: bg.g() as f64,
                        b: bg.b() as f64,
                        a: 1.0,
                    }),
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
    {
        let mut r = rs.renderer.write();
        for id in &out.textures_delta.free {
            r.free_texture(id);
        }
    }
    out.textures_delta.clear();
    let padded = (size[0] * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: padded as u64 * size[1] as u64,
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
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
    );
    cmds.push(enc.finish());
    queue.submit(cmds);
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    let _ = device.poll(wgpu::PollType::wait_indefinitely());
    let Ok(mapped) = buf.slice(..).get_mapped_range() else {
        return vec![0; (size[0] * size[1] * 4) as usize];
    };
    let mut img = Vec::with_capacity((size[0] * size[1] * 4) as usize);
    for y in 0..size[1] as usize {
        let row = y * padded as usize;
        img.extend_from_slice(&mapped[row..row + size[0] as usize * 4]);
    }
    img
}

/// Run the app offscreen at `size` pixels with `ppp` device pixels per point.
pub fn run(opts: AppOptions, size: [u32; 2], ppp: f32) -> Result<(), String> {
    let rs = render_state()?;
    let ctx = egui::Context::default();
    let screenshot = opts.screenshot.clone();
    let exit = opts.exit_after.unwrap_or(5.0);
    let shot_at = opts.screenshot_after.unwrap_or((exit - 1.0).max(0.5));
    let mut app = App::create(&ctx, Some(&rs), opts);
    let start = Instant::now();
    let mut shot_taken = false;
    loop {
        let t = start.elapsed().as_secs_f32();
        let eff = ppp * ctx.zoom_factor();
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0] as f32 / eff, size[1] as f32 / eff),
            )),
            time: Some(t as f64),
            ..Default::default()
        };
        if let Some(v) = input.viewports.get_mut(&egui::ViewportId::ROOT) {
            v.native_pixels_per_point = Some(ppp);
            v.inner_rect = input.screen_rect;
            v.focused = Some(true);
        }
        let out = ctx.run_ui(input, |ui| app.frame(ui));
        let take = !shot_taken && t >= shot_at;
        let img = draw(&rs, out, &ctx, size);
        if take {
            shot_taken = true;
            if let Some(p) = &screenshot {
                save_png(p, size[0], size[1], &img).map_err(|e| e.to_string())?;
                tracing::info!(
                    "screenshot saved to {} ({}×{})",
                    p.display(),
                    size[0],
                    size[1]
                );
            }
        }
        if t >= exit {
            break;
        }
        std::thread::sleep(Duration::from_millis(33));
    }
    app.shutdown();
    Ok(())
}
