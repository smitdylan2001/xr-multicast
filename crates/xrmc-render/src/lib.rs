//! wgpu compositor for XR MultiCast: draws every stream tile with the lens
//! correction shader inside egui's render pass (one GPU device, one pass).
//!
//! Port of `src/renderer/js/lens-renderer.js` (WebGL) to WGSL. Inputs are YUV
//! frames from `xrmc-decode` (converted to RGB in the shader).
//!
//! Usage from an eframe app:
//! ```ignore
//! let video = VideoRenderer::install(cc.wgpu_render_state.as_ref().unwrap());
//! video.upload("SERIAL", &frame);                     // whenever a new frame is decoded
//! video.paint(ui.painter(), tile_rect, "SERIAL", &params); // in the UI, per tile
//! ```
//!
//! Each stream owns one texture per YUV plane (R8 for I420, R8 + RG8 for NV12,
//! R16Unorm for 10-bit when the device has [`OPTIONAL_FEATURES`], otherwise the
//! 10-bit samples are converted to 8 bit on upload). Textures and bind groups are
//! only recreated when a stream's size or format changes. All tile draws of a
//! frame share one uniform buffer (dynamic offsets) that is written once per frame.
//!
//! Without egui (tests, tools): [`VideoRenderer::new_headless`] + [`VideoRenderer::snapshot`].

mod geometry;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use geometry::{Fit, FrameInfo, PxRect, TileUniform, rect_to_px, tile_uniform};
use xrmc_decode::PixelFormat;

pub use xrmc_decode::Frame;

/// Which eye to show in single-eye mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Eye {
    #[default]
    Left,
    Right,
}

/// How the panel image is split into eye views (already resolved; see `xrmc_core::layout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Two eyes side by side.
    #[default]
    Sbs,
    /// Two eyes stacked top/bottom.
    Tb,
    /// 2×2 grid; eyes are the top pair (or bottom pair when `bottom_row`).
    Quad { bottom_row: bool },
    /// One image.
    Mono,
}

/// What a tile shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// One eye, lens distortion removed (default).
    #[default]
    Corrected,
    /// Both eyes side by side, corrected.
    Stereo,
    /// The unprocessed panel image.
    Raw,
}

/// Lens correction and framing (same meaning and defaults as the Electron app's calibration).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LensParams {
    pub k1: f32,
    pub k2: f32,
    pub zoom: f32,
    /// Lens centre offset of the LEFT eye (-1..1); mirrored for the right eye in side-by-side layouts.
    pub center_x: f32,
    pub center_y: f32,
    /// Per-channel radial scale for colour-fringe correction.
    pub chroma_r: f32,
    pub chroma_b: f32,
    /// Output aspect (width / height); `None` = the eye's own aspect.
    pub output_aspect: Option<f32>,
}

impl Default for LensParams {
    fn default() -> Self {
        LensParams {
            k1: -0.2,
            k2: 0.03,
            zoom: 1.4,
            center_x: 0.0,
            center_y: 0.0,
            chroma_r: 1.0,
            chroma_b: 1.0,
            output_aspect: Some(16.0 / 9.0),
        }
    }
}

/// Everything needed to draw one tile.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TileParams {
    pub view: ViewMode,
    pub eye: Eye,
    pub layout: Layout,
    /// Clockwise quarter turns applied to the source: 0, 90, 180, 270.
    pub rotation: u32,
    pub flip_h: bool,
    pub flip_v: bool,
    pub lens: LensParams,
    /// Blur + dim the picture (e.g. while the stream changes resolution).
    pub blur: bool,
    /// Draw the calibration reference grid.
    pub grid: bool,
}

/// Device features worth requesting when the adapter has them (all optional):
/// `TEXTURE_FORMAT_16BIT_NORM` keeps 10-bit frames at full precision on the GPU
/// (otherwise they are converted to 8 bit on upload).
pub const OPTIONAL_FEATURES: wgpu::Features = wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;

/// Makes egui create its device with [`OPTIONAL_FEATURES`] where the adapter has them
/// (keeps egui's other device settings). Use on eframe's
/// `NativeOptions::wgpu_options.wgpu_setup` before `run_native`. No-op for an existing device.
pub fn request_optional_features(setup: &mut egui_wgpu::WgpuSetup) {
    if let egui_wgpu::WgpuSetup::CreateNew(create) = setup {
        let base = create.device_descriptor.clone();
        create.device_descriptor = Arc::new(move |adapter| {
            let mut desc = base(adapter);
            desc.required_features |= adapter.features() & OPTIONAL_FEATURES;
            desc
        });
    }
}

/// Upload / texture counters (monotonic since creation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderStats {
    /// Streams with textures.
    pub streams: usize,
    /// Frames uploaded.
    pub uploads: u64,
    /// Bytes handed to the GPU queue.
    pub upload_bytes: u64,
    /// Times a stream's textures were (re)created (first frame, size or format change).
    pub texture_creations: u64,
}

/// Why a frame was not uploaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadError {
    /// Planes missing, too small, or a stride shorter than a row.
    BadFrame(String),
    /// Larger than the device's maximum texture size.
    TooLarge { width: u32, height: u32, max: u32 },
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UploadError::BadFrame(why) => write!(f, "bad frame: {why}"),
            UploadError::TooLarge { width, height, max } => write!(
                f,
                "frame {width}x{height} exceeds the GPU texture limit {max}"
            ),
        }
    }
}

impl std::error::Error for UploadError {}

/// Shared handle to the GPU video textures and pipeline (cheap to clone).
#[derive(Clone)]
pub struct VideoRenderer {
    shared: Arc<Shared>,
}

const TILE_SIZE: u64 = std::mem::size_of::<TileUniform>() as u64;
const NO_SLOT: u32 = u32::MAX;
const OFFSCREEN_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// Identity of a stream's GPU textures (recreated when this changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TexKey {
    width: u32,
    height: u32,
    format: PixelFormat,
    r16: bool,
}

struct StreamGpu {
    key: TexKey,
    textures: Vec<wgpu::Texture>,
    bind_group: wgpu::BindGroup,
}

struct Stream {
    gpu: Option<Arc<StreamGpu>>,
    info: Option<FrameInfo>,
    /// Reused buffer for the 10-bit → 8-bit fallback conversion.
    scratch: Vec<u8>,
}

struct Shared {
    device: wgpu::Device,
    queue: wgpu::Queue,
    shader: wgpu::ShaderModule,
    tex_bgl: wgpu::BindGroupLayout,
    uni_bgl: wgpu::BindGroupLayout,
    pipeline_layout: wgpu::PipelineLayout,
    sampler: wgpu::Sampler,
    uniform_stride: u64,
    /// 10-bit frames may use R16Unorm textures (feature enabled and filterable).
    r16: AtomicBool,
    streams: Mutex<HashMap<String, Arc<Mutex<Stream>>>>,
    offscreen: Mutex<Option<wgpu::RenderPipeline>>,
    uploads: AtomicU64,
    upload_bytes: AtomicU64,
    creations: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn r16_supported(device: &wgpu::Device, adapter: Option<&wgpu::Adapter>) -> bool {
    let fmt = wgpu::TextureFormat::R16Unorm;
    device
        .features()
        .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM)
        && fmt
            .guaranteed_format_features(device.features())
            .flags
            .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
        && adapter.is_none_or(|a| {
            a.get_texture_format_features(fmt)
                .flags
                .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE)
        })
}

impl Shared {
    fn new(device: wgpu::Device, queue: wgpu::Queue, r16: bool) -> Shared {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("xrmc lens"),
            source: wgpu::ShaderSource::Wgsl(include_str!("lens.wgsl").into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("xrmc yuv planes"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                tex_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let uni_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("xrmc tile uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: wgpu::BufferSize::new(TILE_SIZE),
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("xrmc lens"),
            bind_group_layouts: &[Some(&tex_bgl), Some(&uni_bgl)],
            immediate_size: 0,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("xrmc yuv sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let align = device.limits().min_uniform_buffer_offset_alignment as u64;
        Shared {
            uniform_stride: TILE_SIZE.div_ceil(align) * align,
            device,
            queue,
            shader,
            tex_bgl,
            uni_bgl,
            pipeline_layout,
            sampler,
            r16: AtomicBool::new(r16),
            streams: Mutex::new(HashMap::new()),
            offscreen: Mutex::new(None),
            uploads: AtomicU64::new(0),
            upload_bytes: AtomicU64::new(0),
            creations: AtomicU64::new(0),
        }
    }

    fn pipeline(
        &self,
        format: wgpu::TextureFormat,
        samples: u32,
        depth: Option<wgpu::TextureFormat>,
    ) -> wgpu::RenderPipeline {
        self.device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("xrmc lens"),
                layout: Some(&self.pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &self.shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &self.shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: depth.map(|format| wgpu::DepthStencilState {
                    format,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState {
                    count: samples.max(1),
                    ..Default::default()
                },
                multiview_mask: None,
                cache: None,
            })
    }

    fn create_stream_gpu(&self, key: TexKey) -> StreamGpu {
        let (cw, ch) = (key.width.div_ceil(2), key.height.div_ceil(2));
        let tex = |label, w, h, format| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let plane = if key.r16 {
            wgpu::TextureFormat::R16Unorm
        } else {
            wgpu::TextureFormat::R8Unorm
        };
        let textures = match key.format {
            PixelFormat::Nv12 => vec![
                tex("xrmc y", key.width, key.height, plane),
                tex("xrmc uv", cw, ch, wgpu::TextureFormat::Rg8Unorm),
            ],
            PixelFormat::I420 | PixelFormat::I420P10 => vec![
                tex("xrmc y", key.width, key.height, plane),
                tex("xrmc u", cw, ch, plane),
                tex("xrmc v", cw, ch, plane),
            ],
        };
        let views: Vec<wgpu::TextureView> = textures
            .iter()
            .map(|t| t.create_view(&Default::default()))
            .collect();
        let v = |i: usize| wgpu::BindingResource::TextureView(&views[i.min(views.len() - 1)]);
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("xrmc yuv planes"),
            layout: &self.tex_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: v(0),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: v(1),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: v(2),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        StreamGpu {
            key,
            textures,
            bind_group,
        }
    }

    fn write_plane(
        &self,
        texture: &wgpu::Texture,
        data: &[u8],
        bytes_per_row: usize,
        width: u32,
        height: u32,
    ) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row as u32),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    fn lookup(&self, stream: &str) -> Option<(Arc<StreamGpu>, FrameInfo)> {
        let entry = lock(&self.streams).get(stream)?.clone();
        let s = lock(&entry);
        Some((s.gpu.clone()?, s.info?))
    }

    fn uniform_bind_group(&self, buffer: &wgpu::Buffer) -> wgpu::BindGroup {
        self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("xrmc tile uniforms"),
            layout: &self.uni_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer,
                    offset: 0,
                    size: wgpu::BufferSize::new(TILE_SIZE),
                }),
            }],
        })
    }

    /// Renders one tile into a fresh `width`×`height` RGBA texture and reads it back (top row first).
    fn render_offscreen(
        &self,
        gpu: &StreamGpu,
        uniform: Option<TileUniform>,
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>> {
        let max = self.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > max || height > max {
            return None;
        }
        let pipeline = lock(&self.offscreen)
            .get_or_insert_with(|| self.pipeline(OFFSCREEN_FORMAT, 1, None))
            .clone();
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("xrmc snapshot"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let ubuf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xrmc snapshot uniforms"),
            size: self.uniform_stride,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ubg = self.uniform_bind_group(&ubuf);
        if let Some(u) = &uniform {
            self.queue.write_buffer(&ubuf, 0, bytemuck::bytes_of(u));
        }
        let padded = (width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xrmc snapshot readback"),
            size: padded as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("xrmc snapshot"),
            });
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("xrmc snapshot"),
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
            if uniform.is_some() {
                pass.set_pipeline(&pipeline);
                pass.set_bind_group(0, &gpu.bind_group, &[]);
                pass.set_bind_group(1, &ubg, &[0]);
                pass.draw(0..6, 0..1);
            }
        }
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let index = self.queue.submit([enc.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        readback.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(index),
                timeout: None,
            })
            .ok()?;
        rx.recv().ok()?.ok()?;
        let mapped = readback.slice(..).get_mapped_range().ok()?;
        let row = (width * 4) as usize;
        let mut out = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let start = y * padded as usize;
            out.extend_from_slice(&mapped[start..start + row]);
        }
        drop(mapped);
        readback.unmap();
        Some(out)
    }
}

/// Bytes per sample and plane geometry checks for a frame.
fn validate(frame: &Frame) -> Result<(), UploadError> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0 || h == 0 {
        return Err(UploadError::BadFrame("empty frame".into()));
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let planes: &[(usize, usize)] = match frame.format {
        PixelFormat::I420 => &[(w, h), (cw, ch), (cw, ch)],
        PixelFormat::Nv12 => &[(w, h), (cw * 2, ch)],
        PixelFormat::I420P10 => &[(w * 2, h), (cw * 2, ch), (cw * 2, ch)],
    };
    if frame.planes.len() < planes.len() {
        return Err(UploadError::BadFrame(format!(
            "{:?} needs {} planes, got {}",
            frame.format,
            planes.len(),
            frame.planes.len()
        )));
    }
    for (i, (&(row, rows), p)) in planes.iter().zip(&frame.planes).enumerate() {
        if p.stride < row {
            return Err(UploadError::BadFrame(format!(
                "plane {i}: stride {} < row {row}",
                p.stride
            )));
        }
        let need = p.stride * (rows - 1) + row;
        if p.data.len() < need {
            return Err(UploadError::BadFrame(format!(
                "plane {i}: {} bytes < {need}",
                p.data.len()
            )));
        }
        if p.stride > u32::MAX as usize {
            return Err(UploadError::BadFrame(format!(
                "plane {i}: stride too large"
            )));
        }
    }
    Ok(())
}

/// 10-bit little-endian samples → 8 bit (tightly packed into `out`).
fn convert_10_to_8(data: &[u8], stride: usize, width: usize, height: usize, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(width * height);
    for y in 0..height {
        let row = &data[y * stride..y * stride + width * 2];
        out.extend(
            row.as_chunks::<2>()
                .0
                .iter()
                .map(|&s| ((u16::from_le_bytes(s) as u32 + 2) >> 2).min(255) as u8),
        );
    }
}

impl VideoRenderer {
    /// Create the pipeline and register it with egui's wgpu renderer.
    ///
    /// Assumes egui's default render pass (no MSAA, no depth buffer); use
    /// [`VideoRenderer::install_with`] otherwise.
    pub fn install(render_state: &egui_wgpu::RenderState) -> VideoRenderer {
        Self::install_with(render_state, 1, None)
    }

    /// Like [`VideoRenderer::install`] for an egui render pass with MSAA and/or a depth buffer
    /// (eframe's `NativeOptions::multisampling` / `depth_buffer`).
    pub fn install_with(
        render_state: &egui_wgpu::RenderState,
        msaa_samples: u32,
        depth_format: Option<wgpu::TextureFormat>,
    ) -> VideoRenderer {
        let r16 = r16_supported(&render_state.device, Some(&render_state.adapter));
        let shared = Arc::new(Shared::new(
            render_state.device.clone(),
            render_state.queue.clone(),
            r16,
        ));
        let pipeline = shared.pipeline(render_state.target_format, msaa_samples, depth_format);
        let ring = UniformRing::new(&shared, 32);
        render_state
            .renderer
            .write()
            .callback_resources
            .insert(EguiResources {
                pipeline,
                ring,
                uni_bgl: shared.uni_bgl.clone(),
                linearise: render_state.target_format.is_srgb(),
            });
        VideoRenderer { shared }
    }

    /// A renderer without egui (tests, tools): only [`VideoRenderer::snapshot`] /
    /// [`VideoRenderer::analyze_luma`] draw. Pass the adapter when available so 10-bit
    /// texture support is checked precisely.
    pub fn new_headless(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        adapter: Option<&wgpu::Adapter>,
    ) -> VideoRenderer {
        let r16 = r16_supported(device, adapter);
        VideoRenderer {
            shared: Arc::new(Shared::new(device.clone(), queue.clone(), r16)),
        }
    }

    /// Upload the latest decoded frame of a stream (replaces the previous one).
    /// Invalid frames are ignored; see [`VideoRenderer::try_upload`].
    pub fn upload(&self, stream: &str, frame: &Frame) {
        let _ = self.try_upload(stream, frame);
    }

    /// Upload the latest decoded frame of a stream, reporting why it was rejected.
    /// The copy is queued and reaches the GPU with the next submission (egui's frame).
    pub fn try_upload(&self, stream: &str, frame: &Frame) -> Result<(), UploadError> {
        validate(frame)?;
        let sh = &self.shared;
        let max = sh.device.limits().max_texture_dimension_2d;
        if frame.width > max || frame.height > max {
            return Err(UploadError::TooLarge {
                width: frame.width,
                height: frame.height,
                max,
            });
        }
        let entry = {
            let mut map = lock(&sh.streams);
            match map.get(stream) {
                Some(e) => e.clone(),
                None => map
                    .entry(stream.to_owned())
                    .or_insert_with(|| {
                        Arc::new(Mutex::new(Stream {
                            gpu: None,
                            info: None,
                            scratch: Vec::new(),
                        }))
                    })
                    .clone(),
            }
        };
        let mut s = lock(&entry);
        let r16 = frame.format == PixelFormat::I420P10 && sh.r16.load(Ordering::Relaxed);
        let key = TexKey {
            width: frame.width,
            height: frame.height,
            format: frame.format,
            r16,
        };
        let gpu = match &s.gpu {
            Some(g) if g.key == key => g.clone(),
            _ => {
                let g = Arc::new(sh.create_stream_gpu(key));
                sh.creations.fetch_add(1, Ordering::Relaxed);
                s.gpu = Some(g.clone());
                g
            }
        };
        s.info = Some(FrameInfo {
            width: frame.width,
            height: frame.height,
            color: frame.color,
            format: frame.format,
            r16,
        });

        let (w, h) = (frame.width, frame.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut bytes = 0u64;
        for (i, tex) in gpu.textures.iter().enumerate() {
            let p = &frame.planes[i];
            let (pw, ph) = if i == 0 { (w, h) } else { (cw, ch) };
            if frame.format == PixelFormat::I420P10 && !r16 {
                let mut scratch = std::mem::take(&mut s.scratch);
                convert_10_to_8(&p.data, p.stride, pw as usize, ph as usize, &mut scratch);
                sh.write_plane(tex, &scratch, pw as usize, pw, ph);
                bytes += scratch.len() as u64;
                s.scratch = scratch;
            } else {
                sh.write_plane(tex, &p.data, p.stride, pw, ph);
                bytes += (p.stride * ph as usize) as u64;
            }
        }
        sh.uploads.fetch_add(1, Ordering::Relaxed);
        sh.upload_bytes.fetch_add(bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Forget a stream's textures.
    pub fn remove(&self, stream: &str) {
        lock(&self.shared.streams).remove(stream);
    }

    /// Size of the last uploaded frame of a stream.
    pub fn frame_size(&self, stream: &str) -> Option<(u32, u32)> {
        let entry = lock(&self.shared.streams).get(stream)?.clone();
        let info = lock(&entry).info?;
        Some((info.width, info.height))
    }

    /// Draw a stream into `rect` (letterboxed; nothing is drawn if no frame was uploaded yet).
    pub fn paint(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        stream: &str,
        params: &TileParams,
    ) {
        let Some((gpu, info)) = self.shared.lookup(stream) else {
            return;
        };
        if !rect.is_positive() || !painter.clip_rect().intersects(rect) {
            return;
        }
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            TileCallback {
                gpu,
                info,
                params: *params,
                rect,
                slot: AtomicU32::new(NO_SLOT),
            },
        ));
    }

    /// Render a stream offscreen at `width`×`height` exactly as a tile of that size
    /// (letterboxed on black, grid/blur as in `params`) and read it back as RGBA8,
    /// top row first. Blocks until the GPU is done; `None` without a frame.
    pub fn snapshot(
        &self,
        stream: &str,
        params: &TileParams,
        width: u32,
        height: u32,
    ) -> Option<Vec<u8>> {
        let (gpu, info) = self.shared.lookup(stream)?;
        let bx = PxRect {
            x: 0,
            y: 0,
            w: width as i32,
            h: height as i32,
        };
        let u = tile_uniform(&info, params, bx, [width, height], false, Fit::Letterbox);
        self.shared.render_offscreen(&gpu, u, width, height)
    }

    /// The logical frame (rotation and flips applied, no correction) stretched to
    /// `size`×`size` as luma 0..1, row-major: input for `xrmc_core::layout::detect_grid`.
    /// Blocks until the GPU is done (call rarely).
    pub fn analyze_luma(
        &self,
        stream: &str,
        rotation: u32,
        flip_h: bool,
        flip_v: bool,
        size: u32,
    ) -> Option<Vec<f32>> {
        let (gpu, info) = self.shared.lookup(stream)?;
        let p = TileParams {
            view: ViewMode::Raw,
            rotation,
            flip_h,
            flip_v,
            ..Default::default()
        };
        let bx = PxRect {
            x: 0,
            y: 0,
            w: size as i32,
            h: size as i32,
        };
        let u = tile_uniform(&info, &p, bx, [size, size], false, Fit::Stretch);
        let px = self.shared.render_offscreen(&gpu, u, size, size)?;
        Some(
            px.as_chunks::<4>()
                .0
                .iter()
                .map(|c| (0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32) / 255.0)
                .collect(),
        )
    }

    /// Upload counters.
    pub fn stats(&self) -> RenderStats {
        let sh = &self.shared;
        RenderStats {
            streams: lock(&sh.streams).len(),
            uploads: sh.uploads.load(Ordering::Relaxed),
            upload_bytes: sh.upload_bytes.load(Ordering::Relaxed),
            texture_creations: sh.creations.load(Ordering::Relaxed),
        }
    }

    /// Whether 10-bit frames are kept at 16-bit precision on the GPU
    /// (else converted to 8 bit on upload; see [`OPTIONAL_FEATURES`]).
    pub fn has_16bit_textures(&self) -> bool {
        self.shared.r16.load(Ordering::Relaxed)
    }

    /// Disable 16-bit textures (forces the 8-bit fallback; for tests and low-end GPUs).
    /// Takes effect at each stream's next upload.
    pub fn disable_16bit_textures(&self) {
        self.shared.r16.store(false, Ordering::Relaxed);
    }
}

/// Per-frame uniforms of all tile draws, in one buffer bound with dynamic offsets.
struct UniformRing {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    capacity: u32,
    stride: u64,
    staging: Vec<u8>,
    count: u32,
}

impl UniformRing {
    fn new(shared: &Shared, capacity: u32) -> UniformRing {
        let buffer = Self::create_buffer(&shared.device, shared.uniform_stride, capacity);
        let bind_group = shared.uniform_bind_group(&buffer);
        UniformRing {
            buffer,
            bind_group,
            capacity,
            stride: shared.uniform_stride,
            staging: Vec::new(),
            count: 0,
        }
    }

    fn create_buffer(device: &wgpu::Device, stride: u64, capacity: u32) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("xrmc tile uniforms"),
            size: stride * capacity as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn push(&mut self, u: &TileUniform) -> u32 {
        let off = self.count as usize * self.stride as usize;
        self.staging.resize(off + self.stride as usize, 0);
        self.staging[off..off + TILE_SIZE as usize].copy_from_slice(bytemuck::bytes_of(u));
        self.count += 1;
        self.count - 1
    }

    /// Writes this frame's uniforms (once) and starts the next frame.
    fn flush(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        uni_bgl: &wgpu::BindGroupLayout,
    ) {
        if self.count == 0 {
            return;
        }
        if self.count > self.capacity {
            self.capacity = self.count.next_power_of_two();
            self.buffer = Self::create_buffer(device, self.stride, self.capacity);
            self.bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("xrmc tile uniforms"),
                layout: uni_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &self.buffer,
                        offset: 0,
                        size: wgpu::BufferSize::new(TILE_SIZE),
                    }),
                }],
            });
        }
        queue.write_buffer(&self.buffer, 0, &self.staging);
        self.staging.clear();
        self.count = 0;
    }
}

/// Lives in egui-wgpu's `callback_resources`.
struct EguiResources {
    pipeline: wgpu::RenderPipeline,
    ring: UniformRing,
    uni_bgl: wgpu::BindGroupLayout,
    /// The target is sRGB: the shader outputs linear values.
    linearise: bool,
}

/// One tile draw inside egui's render pass.
struct TileCallback {
    gpu: Arc<StreamGpu>,
    info: FrameInfo,
    params: TileParams,
    rect: egui::Rect,
    slot: AtomicU32,
}

impl egui_wgpu::CallbackTrait for TileCallback {
    fn prepare(
        &self,
        _device: &wgpu::Device,
        _queue: &wgpu::Queue,
        screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(res) = resources.get_mut::<EguiResources>() {
            let bx = rect_to_px(self.rect, screen.pixels_per_point);
            if let Some(u) = tile_uniform(
                &self.info,
                &self.params,
                bx,
                screen.size_in_pixels,
                res.linearise,
                Fit::Letterbox,
            ) {
                self.slot.store(res.ring.push(&u), Ordering::Relaxed);
            }
        }
        Vec::new()
    }

    fn finish_prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        if let Some(res) = resources.get_mut::<EguiResources>() {
            let EguiResources { ring, uni_bgl, .. } = res;
            ring.flush(device, queue, uni_bgl);
        }
        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let slot = self.slot.load(Ordering::Relaxed);
        let Some(res) = resources.get::<EguiResources>() else {
            return;
        };
        if slot == NO_SLOT {
            return;
        }
        // Tile geometry is in full-framebuffer coordinates; egui keeps the scissor on the clip rect.
        let [w, h] = info.screen_size_px;
        pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
        pass.set_pipeline(&res.pipeline);
        pass.set_bind_group(0, &self.gpu.bind_group, &[]);
        pass.set_bind_group(
            1,
            &res.ring.bind_group,
            &[(slot as u64 * res.ring.stride) as u32],
        );
        pass.draw(0..6, 0..1);
    }
}
