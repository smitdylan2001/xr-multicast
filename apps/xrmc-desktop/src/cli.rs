//! Command-line options and the window / renderer setup.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use eframe::egui;

use crate::bridge::{HubConfig, Mode, find_scrcpy_server};
use crate::ui::{App, AppOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Renderer {
    /// Vulkan, Metal or DirectX 12, falling back to OpenGL.
    Auto,
    Vulkan,
    Gl,
}

#[derive(Debug, Parser)]
#[command(
    name = "xr-multicast",
    version,
    about = "XR MultiCast: cast many standalone VR headsets into one lens-corrected, clearly labelled dashboard."
)]
pub struct Cli {
    /// Show N synthetic headsets instead of real ones (testing and screenshots).
    #[arg(long, value_name = "N")]
    pub demo: Option<usize>,
    /// Start fullscreen.
    #[arg(long)]
    pub fullscreen: bool,
    /// Touchscreen hub: fullscreen, no window decorations, quitting needs a confirmation.
    #[arg(long)]
    pub kiosk: bool,
    /// Interface size on top of the display's scaling (e.g. 1.25). Default: the saved setting.
    #[arg(long, value_name = "X")]
    pub ui_scale: Option<f32>,
    /// Don't use a running adb server; talk to headsets over USB and TCP directly.
    #[arg(long)]
    pub no_adb_server: bool,
    /// Don't open USB devices directly (leave USB to an adb server).
    #[arg(long)]
    pub no_usb: bool,
    /// Connect to a headset at this address on start (e.g. 192.168.1.50:5555). Repeatable.
    #[arg(long, value_name = "ADDR")]
    pub connect: Vec<String>,
    /// GPU backend.
    #[arg(long, value_enum, default_value_t = Renderer::Auto)]
    pub renderer: Renderer,
    /// Settings file (default: the XR MultiCast app's `xr-multicast.json`).
    #[arg(long, value_name = "PATH")]
    pub store: Option<PathBuf>,
    /// ADB key file (default: `adbkey` next to the settings).
    #[arg(long, value_name = "PATH")]
    pub adb_key: Option<PathBuf>,
    /// scrcpy-server file (default: next to the program, $XRMC_SCRCPY_SERVER, or the repository's vendor/).
    #[arg(long, value_name = "PATH")]
    pub scrcpy_server: Option<PathBuf>,
    /// Initial window size, e.g. 1280x800.
    #[arg(long, value_name = "WxH")]
    pub size: Option<String>,
    /// Decoder threads per stream (0 = automatic).
    #[arg(long, default_value_t = 2)]
    pub decoder_threads: u32,
    /// Save a PNG of the window (testing).
    #[arg(long, value_name = "PATH", hide = true)]
    pub screenshot: Option<PathBuf>,
    /// Seconds after start to take the screenshot (default: 1 s before --exit-after).
    #[arg(long, value_name = "SECS", hide = true)]
    pub screenshot_after: Option<f32>,
    /// Quit after this many seconds (testing).
    #[arg(long, value_name = "SECS", hide = true)]
    pub exit_after: Option<f32>,
    /// Render without a window at this size in pixels (testing, screenshots): e.g. 800x1280.
    #[arg(long, value_name = "WxH", hide = true)]
    pub offscreen: Option<String>,
    /// Display scale for --offscreen (device pixels per point).
    #[arg(long, value_name = "X", default_value_t = 1.0, hide = true)]
    pub ppp: f32,
    /// Cast every castable headset once as soon as it is online (testing, unattended hubs).
    #[arg(long, hide = true)]
    pub cast_on_start: bool,
    /// Open a dialog at start (testing): settings, verify, edit, calibrate, guide, add-ip, focus, nav, fix.
    #[arg(long, value_name = "WHAT", hide = true)]
    pub show: Option<String>,
}

pub fn parse_size(s: &str) -> Option<[f32; 2]> {
    let (w, h) = s.split_once(['x', 'X'])?;
    Some([w.trim().parse().ok()?, h.trim().parse().ok()?])
}

impl Cli {
    pub fn mode(&self) -> Result<Mode, String> {
        if let Some(n) = self.demo {
            return Ok(Mode::Demo {
                headsets: n.clamp(1, 32),
            });
        }
        let mut paths = xrmc_hub::HubPaths::default_for_user()
            .ok_or("No config directory (set HOME / XDG_CONFIG_HOME / APPDATA).")?;
        if let Some(s) = &self.store {
            paths.store = s.clone();
        }
        if let Some(k) = &self.adb_key {
            paths.adb_key = k.clone();
        }
        paths.scrcpy_server = self.scrcpy_server.clone().or_else(find_scrcpy_server);
        Ok(Mode::Hub(Box::new(HubConfig {
            paths,
            use_adb_server: !self.no_adb_server,
            direct_usb: !self.no_usb,
            connect: self.connect.clone(),
        })))
    }

    pub fn app_options(&self) -> Result<AppOptions, String> {
        Ok(AppOptions {
            mode: self.mode()?,
            fullscreen: self.fullscreen,
            kiosk: self.kiosk,
            ui_scale: self.ui_scale.map(|s| s.clamp(0.5, 3.0)),
            screenshot: self.screenshot.clone(),
            screenshot_after: self.screenshot_after,
            exit_after: self.exit_after,
            show: self.show.clone(),
            decoder_threads: self.decoder_threads,
            offscreen: self.offscreen.is_some(),
            cast_on_start: self.cast_on_start,
        })
    }

    pub fn native_options(&self) -> eframe::NativeOptions {
        let size = self
            .size
            .as_deref()
            .and_then(parse_size)
            .unwrap_or([1600.0, 940.0]);
        let full = self.fullscreen || self.kiosk;
        let mut viewport = egui::ViewportBuilder::default()
            .with_title("XR MultiCast")
            .with_app_id("xr-multicast")
            .with_inner_size(size)
            .with_min_inner_size([400.0, 320.0])
            .with_fullscreen(full);
        if self.kiosk {
            viewport = viewport.with_decorations(false);
        }
        let mut options = eframe::NativeOptions {
            viewport,
            renderer: eframe::Renderer::Wgpu,
            // Less GPU memory and latency: no MSAA, no depth buffer.
            multisampling: 0,
            depth_buffer: 0,
            ..Default::default()
        };
        // Full-precision 10-bit textures where the GPU has them.
        xrmc_render::request_optional_features(&mut options.wgpu_options.wgpu_setup);
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(c) = &mut options.wgpu_options.wgpu_setup {
            match self.renderer {
                Renderer::Auto => {}
                Renderer::Vulkan => c.instance_descriptor.backends = eframe::wgpu::Backends::VULKAN,
                Renderer::Gl => c.instance_descriptor.backends = eframe::wgpu::Backends::GL,
            }
        }
        options
    }
}

/// Parse arguments and run the app (blocks until the window closes).
pub fn run() -> Result<(), String> {
    let cli = Cli::parse();
    init_logging();
    let opts = cli.app_options()?;
    if let Some(size) = &cli.offscreen {
        let size = parse_size(size).ok_or("--offscreen WxH")?;
        return crate::offscreen::run(opts, [size[0] as u32, size[1] as u32], cli.ppp);
    }
    let native = cli.native_options();
    if let Some(limit) = opts.exit_after {
        // A hidden Wayland window gets no frame callbacks; make scripted runs end anyway.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f32(limit + 5.0));
            tracing::warn!("watchdog: the window did not close in time (hidden?), exiting");
            std::process::exit(0);
        });
    }
    eframe::run_native(
        "XR MultiCast",
        native,
        Box::new(move |cc| Ok(Box::new(App::new(cc, opts)))),
    )
    .map_err(|e| e.to_string())
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "info,wgpu_core=warn,wgpu_hal=warn,naga=warn,eframe=warn,egui_wgpu=warn",
        )
    });
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_parse() {
        let c = Cli::try_parse_from([
            "xr-multicast",
            "--demo",
            "9",
            "--kiosk",
            "--ui-scale",
            "1.5",
            "--connect",
            "127.0.0.1:5567",
            "--connect",
            "10.0.0.2",
            "--renderer",
            "gl",
            "--no-adb-server",
        ])
        .unwrap();
        assert_eq!(c.demo, Some(9));
        assert!(c.kiosk && c.no_adb_server);
        assert_eq!(c.connect.len(), 2);
        assert_eq!(c.renderer, Renderer::Gl);
        let o = c.app_options().unwrap();
        assert!(matches!(o.mode, Mode::Demo { headsets: 9 }));
        assert_eq!(o.ui_scale, Some(1.5));
        assert_eq!(parse_size("800x1280"), Some([800.0, 1280.0]));
        assert_eq!(parse_size("nope"), None);
    }

    #[test]
    fn hub_mode_uses_the_electron_store() {
        let c = Cli::try_parse_from(["xr-multicast", "--no-adb-server", "--connect", "1.2.3.4"])
            .unwrap();
        let Mode::Hub(cfg) = c.mode().unwrap() else {
            panic!()
        };
        assert!(cfg.paths.store.ends_with("XR MultiCast/xr-multicast.json"));
        assert!(cfg.paths.adb_key.ends_with("XR MultiCast/adbkey"));
        assert!(!cfg.use_adb_server && cfg.direct_usb);
        assert_eq!(cfg.connect, ["1.2.3.4"]);
    }
}
