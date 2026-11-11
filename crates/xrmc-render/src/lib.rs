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
//! STATUS: interface defined; implementation in progress.

use std::sync::Arc;

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
        LensParams { k1: -0.2, k2: 0.03, zoom: 1.4, center_x: 0.0, center_y: 0.0, chroma_r: 1.0, chroma_b: 1.0, output_aspect: Some(16.0 / 9.0) }
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

/// Shared handle to the GPU video textures and pipeline (cheap to clone).
#[derive(Clone)]
pub struct VideoRenderer {
    _inner: Arc<()>,
}

impl VideoRenderer {
    /// Create the pipeline and register it with egui's wgpu renderer.
    pub fn install(_render_state: &egui_wgpu::RenderState) -> VideoRenderer {
        VideoRenderer { _inner: Arc::new(()) }
    }

    /// Upload the latest decoded frame of a stream (replaces the previous one).
    pub fn upload(&self, _stream: &str, _frame: &Frame) {}

    /// Forget a stream's textures.
    pub fn remove(&self, _stream: &str) {}

    /// Size of the last uploaded frame of a stream.
    pub fn frame_size(&self, _stream: &str) -> Option<(u32, u32)> {
        None
    }

    /// Draw a stream into `rect` (letterboxed; nothing is drawn if no frame was uploaded yet).
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect, _stream: &str, _params: &TileParams) {
        painter.rect_filled(rect, 0.0, egui::Color32::from_gray(20));
    }
}
