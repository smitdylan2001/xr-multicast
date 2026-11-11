//! The egui application: top bar, headset sidebar, stream grid, calibration
//! drawer, status bar, dialogs and toasts.

pub mod calibration;
pub mod dialogs;
pub mod icons;
pub mod sidebar;
pub mod status;
pub mod tiles;
pub mod topbar;
pub mod widgets;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, Rect};
use serde_json::{Map, Value, json};
use xrmc_core::brands::Eye;
use xrmc_core::layout::Rotation;
use xrmc_core::view_size::{ViewMode, ViewRequest};
use xrmc_core::{NoticeLevel, TransportPreference};
use xrmc_render::VideoRenderer;

use crate::bridge::{Action, Bridge, Mode, Reply, UiEvent};
use crate::model::{self, Model};
use crate::theme;
use crate::view_report::{DEFAULT_DELAY, ViewReporter};
use dialogs::{Dialog, Fix};

/// Command-line options that shape the UI.
#[derive(Debug, Clone)]
pub struct AppOptions {
    pub mode: Mode,
    pub fullscreen: bool,
    pub kiosk: bool,
    pub ui_scale: Option<f32>,
    pub screenshot: Option<PathBuf>,
    pub screenshot_after: Option<f32>,
    pub exit_after: Option<f32>,
    /// Open a dialog at start (for screenshots): settings, verify, edit, calibrate, guide, add-ip, focus, fix.
    pub show: Option<String>,
    pub decoder_threads: u32,
    /// Rendered by [`crate::offscreen`]: screenshots and exit are handled there.
    pub offscreen: bool,
    /// Cast each castable headset once when it comes online.
    pub cast_on_start: bool,
}

/// Something the user did; applied after the frame is laid out.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Cast(String, Option<TransportPreference>),
    Stop(String),
    CastAll,
    StopAll,
    Scan,
    EnableWifi(String),
    DisconnectWifi(String),
    Open(Dialog),
    Calibrate(String),
    CloseCalibration { save: bool },
    ResetCalibration,
    SetViewMode(String, ViewMode),
    SetEye(String, Eye),
    Rotate(String),
    Snapshot(String),
    TileFullscreen(String),
    Retry(String),
    CloseTile(String),
    ToggleFocus(String),
    SetGridColumns(String),
    ToggleFullscreen,
    ToggleNav,
    Quit,
}

pub struct App {
    pub(crate) opts: AppOptions,
    pub(crate) bridge: Bridge,
    pub(crate) model: Model,
    pub(crate) video: Option<VideoRenderer>,
    pub(crate) gpu: String,
    pub(crate) reporter: ViewReporter,
    pub(crate) dialog: Option<Dialog>,
    pub(crate) cmds: Vec<Cmd>,
    /// Sidebar drawer open (narrow screens).
    pub(crate) nav_open: bool,
    /// One stream fullscreen: the focus to return to.
    pub(crate) app_full: Option<Option<String>>,
    pub(crate) window_full: bool,
    pub(crate) ui_scale: f32,
    pub(crate) narrow: bool,
    pub(crate) scan_busy: bool,
    pub(crate) video_rects: HashMap<String, Rect>,
    /// Streams with textures in the renderer.
    uploaded: HashSet<String>,
    snapshots: Vec<std::sync::mpsc::Receiver<Result<PathBuf, String>>>,
    pub(crate) view_delay: Duration,
    start: Instant,
    shot_requested: bool,
    pending_casts: Vec<(String, Option<TransportPreference>, bool)>,
    demo_autocast: bool,
    auto_cast_done: HashSet<String>,
    quit_ok: bool,
    show_done: bool,
}

/// UI scale stored in the settings file (unknown to the Electron app, which keeps it).
pub const UI_SCALE_KEY: &str = "desktopUiScale";

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, opts: AppOptions) -> App {
        App::create(&cc.egui_ctx, cc.wgpu_render_state.as_ref(), opts)
    }

    /// The app on an egui context and (optional) wgpu render state; also used offscreen.
    pub fn create(
        ctx: &egui::Context,
        render_state: Option<&eframe::egui_wgpu::RenderState>,
        opts: AppOptions,
    ) -> App {
        let ctx = ctx.clone();
        theme::install_fonts(&ctx);
        theme::apply_style(&ctx);
        let (video, gpu) = match render_state {
            Some(rs) => {
                let info = rs.adapter.get_info();
                (
                    Some(VideoRenderer::install(rs)),
                    format!("{:?} · {}", info.backend, info.name),
                )
            }
            None => (None, "no GPU renderer".into()),
        };
        tracing::info!("GPU: {gpu}");
        let wake_ctx = ctx.clone();
        let bridge = Bridge::start(
            opts.mode.clone(),
            std::sync::Arc::new(move || wake_ctx.request_repaint()),
            opts.decoder_threads,
        );
        let demo = matches!(opts.mode, Mode::Demo { .. });
        let fullscreen = opts.fullscreen || opts.kiosk;
        App {
            ui_scale: opts.ui_scale.unwrap_or(1.0),
            opts,
            bridge,
            model: Model::default(),
            video,
            gpu,
            reporter: ViewReporter::new(),
            dialog: None,
            cmds: Vec::new(),
            nav_open: false,
            app_full: None,
            window_full: fullscreen,
            narrow: false,
            scan_busy: false,
            video_rects: HashMap::new(),
            uploaded: HashSet::new(),
            snapshots: Vec::new(),
            view_delay: DEFAULT_DELAY,
            start: Instant::now(),
            shot_requested: false,
            pending_casts: Vec::new(),
            demo_autocast: demo,
            auto_cast_done: HashSet::new(),
            quit_ok: false,
            show_done: false,
        }
    }

    pub(crate) fn send(&mut self, action: Action) -> u64 {
        self.bridge.send(action)
    }

    pub(crate) fn toast(&mut self, level: NoticeLevel, msg: impl Into<String>) {
        self.model.toast(level, msg, Instant::now());
    }

    // ------------------------------------------------------------- events

    fn handle_events(&mut self) {
        for ev in self.bridge.poll() {
            match ev {
                UiEvent::Devices(s) => self.model.on_devices(s),
                UiEvent::Stream { state, generation } => {
                    self.model.on_stream_state(state, generation)
                }
                UiEvent::Notice(n) => self.toast(n.level, n.message),
                UiEvent::Settings(s) => {
                    self.model.settings = *s;
                    if self.opts.ui_scale.is_none()
                        && let Some(v) = self
                            .model
                            .settings
                            .extra
                            .get(UI_SCALE_KEY)
                            .and_then(Value::as_f64)
                    {
                        self.ui_scale = (v as f32).clamp(0.5, 3.0);
                    }
                }
                UiEvent::Env(e) => {
                    let first = self.model.env.version.is_empty();
                    if first && !e.server_available && !e.demo && e.fatal.is_none() {
                        self.toast(
                            NoticeLevel::Error,
                            "scrcpy-server is missing. Tap it in the status bar for how to fix it.",
                        );
                    }
                    self.model.env = *e;
                }
                UiEvent::DecodeFatal {
                    id,
                    generation,
                    message,
                } => self.model.on_decode_fatal(&id, generation, message),
                UiEvent::Done {
                    ticket,
                    action,
                    result,
                } => self.on_done(ticket, *action, result),
            }
        }
    }

    fn on_done(&mut self, ticket: u64, action: Action, result: Result<Reply, String>) {
        if let Some(d) = &mut self.dialog
            && d.busy_ticket() == Some(ticket)
        {
            d.set_busy(None);
            match &result {
                Ok(_) => {
                    if let Dialog::Settings { .. } = d {
                        self.toast(NoticeLevel::Ok, "Settings saved.");
                    }
                    self.dialog = None;
                }
                Err(e) => {
                    let e = e.clone();
                    self.toast(NoticeLevel::Error, e);
                    return;
                }
            }
        }
        if let Some(Dialog::Edit {
            enc_ticket,
            encoders,
            ..
        }) = &mut self.dialog
            && *enc_ticket == Some(ticket)
        {
            *encoders = Some(match &result {
                Ok(Reply::Encoders(r)) => Ok(r.clone()),
                Ok(_) => Err("No answer.".into()),
                Err(e) => Err(e.clone()),
            });
            return;
        }
        match (&action, result) {
            (Action::Cast { id, .. }, Err(e)) => self.model.on_cast_failed(id, e),
            (Action::Scan, r) => {
                self.scan_busy = false;
                match r {
                    Ok(Reply::Scan(s)) => {
                        let (msg, level) = model::scan_summary(&s);
                        self.toast(level, msg);
                    }
                    Err(e) => self.toast(NoticeLevel::Error, e),
                    _ => {}
                }
            }
            (Action::Stop { id }, _) => {
                let id = id.clone();
                self.model.remove_tile(&id);
            }
            (Action::StopAll, _) => {
                let ids: Vec<String> = self.model.tiles.iter().map(|t| t.id.clone()).collect();
                for id in ids {
                    self.model.remove_tile(&id);
                }
            }
            (_, Ok(Reply::Settings(s))) => self.model.settings = *s,
            (_, Ok(Reply::Message(level, m))) => self.toast(level, m),
            (Action::UpdateHeadset { .. }, Err(e)) | (_, Err(e)) => {
                self.toast(NoticeLevel::Error, e)
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------- frames

    /// Upload each stream's newest decoded frame and refresh its stats.
    fn pull_frames(&mut self) {
        let now = Instant::now();
        let focus = self.model.focus.clone();
        for t in &mut self.model.tiles {
            let Some(out) = self.bridge.output(&t.id) else {
                continue;
            };
            if let Some(f) = out.take_frame() {
                let hidden = focus.as_ref().is_some_and(|id| id != &t.id);
                if !hidden {
                    if let Some(v) = &self.video
                        && let Err(e) = v.try_upload(&t.id, &f.frame)
                    {
                        tracing::warn!("[{}] frame not shown: {e}", t.id);
                    }
                    t.on_frame(
                        f.generation,
                        f.frame.width,
                        f.frame.height,
                        out.bit_depth.load(std::sync::atomic::Ordering::Relaxed),
                    );
                    self.uploaded.insert(t.id.clone());
                }
            }
            if let Some(g) = out.grid() {
                t.grid = Some(g);
            }
            t.sample_stats(
                now,
                out.frames.load(std::sync::atomic::Ordering::Relaxed),
                out.bytes.load(std::sync::atomic::Ordering::Relaxed),
            );
        }
        // Free the textures of streams that no longer have a tile.
        let gone: Vec<String> = self
            .uploaded
            .iter()
            .filter(|id| self.model.tile(id).is_none())
            .cloned()
            .collect();
        for id in gone {
            if let Some(v) = &self.video {
                v.remove(&id);
            }
            self.uploaded.remove(&id);
            self.video_rects.remove(&id);
        }
    }

    // ----------------------------------------------------------- commands

    fn begin_cast(&mut self, id: &str, transport: Option<TransportPreference>, restart: bool) {
        self.model.begin_cast(id);
        // Report the new tile's size before starting so the first launch already has the right resolution.
        self.pending_casts
            .push((id.to_string(), transport, restart));
    }

    fn update_headset(&mut self, id: &str, patch: Map<String, Value>) {
        if self.model.headset(id).is_some_and(|h| h.trusted) {
            self.send(Action::UpdateHeadset {
                id: id.to_string(),
                patch,
            });
        }
    }

    fn apply_cmds(&mut self, ctx: &egui::Context) {
        let cmds = std::mem::take(&mut self.cmds);
        for c in cmds {
            match c {
                Cmd::Cast(id, t) => {
                    if self.narrow {
                        self.nav_open = false;
                    }
                    self.begin_cast(&id, t, t.is_some());
                }
                Cmd::Retry(id) => self.begin_cast(&id, None, false),
                Cmd::Stop(id) => {
                    self.send(Action::Stop { id });
                }
                Cmd::CloseTile(id) => {
                    self.model.remove_tile(&id);
                    self.send(Action::Stop { id });
                }
                Cmd::CastAll => {
                    let ids = self.model.castable_not_streaming();
                    if ids.is_empty() {
                        let any = self.model.headsets.iter().any(|h| h.is_online());
                        self.toast(
                            NoticeLevel::Warn,
                            if any {
                                "No verified headsets to cast. Verify them over USB first."
                            } else {
                                "No headsets connected."
                            },
                        );
                    }
                    for id in ids {
                        self.begin_cast(&id, None, false);
                    }
                }
                Cmd::StopAll => {
                    self.send(Action::StopAll);
                }
                Cmd::Scan => {
                    if !self.scan_busy {
                        self.scan_busy = true;
                        self.toast(
                            NoticeLevel::Info,
                            "Scanning the local network for headsets…",
                        );
                        self.send(Action::Scan);
                    }
                }
                Cmd::EnableWifi(id) => {
                    self.toast(
                        NoticeLevel::Info,
                        "Enabling Wi-Fi casting… keep the cable plugged in for a few seconds.",
                    );
                    self.send(Action::EnableWifi { id });
                }
                Cmd::DisconnectWifi(id) => {
                    self.send(Action::DisconnectWifi { id });
                }
                Cmd::Open(d) => {
                    if self.narrow && matches!(d, Dialog::Verify { .. } | Dialog::Edit { .. }) {
                        self.nav_open = false;
                    }
                    self.open_dialog(d);
                }
                Cmd::Calibrate(id) => {
                    if self.narrow {
                        self.nav_open = false;
                    }
                    if self.model.tile(&id).is_none() {
                        self.begin_cast(&id, None, false);
                    }
                    self.model.open_calibration(&id);
                }
                Cmd::CloseCalibration { save } => {
                    if let Some(c) = self.model.close_calibration(save) {
                        let single_eye =
                            self.model.streams.get(&c.id).is_some_and(|s| s.single_eye);
                        let mut patch = Map::new();
                        patch.insert("calibration".into(), model::correction_json(&c.params));
                        self.update_headset(&c.id, patch);
                        self.toast(NoticeLevel::Ok, "Calibration saved for this headset.");
                        // In single-eye streaming the headset crops, so another eye needs a restart.
                        if single_eye && c.params.eye != c.original.eye {
                            self.begin_cast(&c.id, None, true);
                        }
                    }
                }
                Cmd::ResetCalibration => {
                    if let Some(c) = &self.model.calib {
                        let h = self.model.headset(&c.id).cloned();
                        let eye = c.params.eye;
                        let mut p = model::preset_for(h.as_ref());
                        p.eye = eye;
                        if let Some(c) = &mut self.model.calib {
                            c.params = p;
                        }
                    }
                }
                Cmd::SetViewMode(id, mode) => {
                    if let Some(t) = self.model.tile_mut(&id) {
                        t.view_mode = mode;
                    }
                    let mut patch = Map::new();
                    patch.insert("viewMode".into(), json!(model::view_mode_str(mode)));
                    self.update_headset(&id, patch);
                }
                Cmd::SetEye(id, eye) => {
                    if let Some(c) = self.model.calib.as_mut().filter(|c| c.id == id) {
                        c.params.eye = eye;
                    } else {
                        let h = self.model.headset(&id).cloned();
                        if let Some(h) = h.filter(|h| h.trusted) {
                            let mut cal = model::correction_for(Some(&h));
                            cal.eye = eye;
                            let mut patch = Map::new();
                            patch.insert("calibration".into(), model::correction_json(&cal));
                            self.update_headset(&id, patch);
                            if self.model.streams.get(&id).is_some_and(|s| s.single_eye) {
                                self.begin_cast(&id, None, true);
                            }
                        }
                    }
                    if let Some(t) = self.model.tile_mut(&id) {
                        t.eye_override = Some(eye);
                    }
                }
                Cmd::Rotate(id) => self.rotate(&id),
                Cmd::Snapshot(id) => self.snapshot(ctx, &id),
                Cmd::TileFullscreen(id) => {
                    if self.app_full.is_some() {
                        self.exit_app_full(ctx);
                    } else {
                        self.app_full = Some(self.model.focus.clone());
                        self.model.set_focus(Some(&id));
                        if !self.window_full {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                        }
                    }
                }
                Cmd::ToggleFocus(id) => {
                    let next = if self.model.focus.as_deref() == Some(id.as_str()) {
                        None
                    } else {
                        Some(id)
                    };
                    self.model.set_focus(next.as_deref());
                }
                Cmd::SetGridColumns(v) => {
                    self.model.settings.grid_columns = v.clone();
                    let mut patch = Map::new();
                    patch.insert("gridColumns".into(), json!(v));
                    self.send(Action::UpdateSettings { patch });
                }
                Cmd::ToggleFullscreen => {
                    self.window_full = !self.window_full;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.window_full));
                }
                Cmd::ToggleNav => self.nav_open = !self.nav_open,
                Cmd::Quit => {
                    self.quit_ok = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn open_dialog(&mut self, d: Dialog) {
        if let Dialog::Edit { id, .. } = &d {
            let ticket = self.send(Action::Encoders { id: id.clone() });
            let mut d = d;
            if let Dialog::Edit { enc_ticket, .. } = &mut d {
                *enc_ticket = Some(ticket);
            }
            self.dialog = Some(d);
            return;
        }
        self.dialog = Some(d);
    }

    fn exit_app_full(&mut self, ctx: &egui::Context) {
        if let Some(prev) = self.app_full.take() {
            self.model.set_focus(prev.as_deref());
            if !self.window_full {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
        }
    }

    /// Rotate a further 90° clockwise; turns automatic rotation into an explicit angle.
    fn rotate(&mut self, id: &str) {
        let Some(t) = self.model.tile(id) else { return };
        let r = model::tile_params(&self.model.inputs(t));
        let next = f64::from((r.view.rotation + 90) % 360);
        if let Some(c) = self.model.calib.as_mut().filter(|c| c.id == id) {
            c.params.rotation = Rotation::Degrees(next);
            return;
        }
        if let Some(t) = self.model.tile_mut(id) {
            t.rotation_override = Some(next);
        }
        if let Some(h) = self.model.headset(id).cloned().filter(|h| h.trusted) {
            let mut cal = model::correction_for(Some(&h));
            cal.rotation = Rotation::Degrees(next);
            let mut patch = Map::new();
            patch.insert("calibration".into(), model::correction_json(&cal));
            self.update_headset(id, patch);
        }
    }

    /// Save the stream as a PNG, rendered offscreen at 2× the tile size (640–2560 px wide), without overlays.
    fn snapshot(&mut self, ctx: &egui::Context, id: &str) {
        let (Some(video), Some(rect)) = (self.video.clone(), self.video_rects.get(id).copied())
        else {
            self.toast(NoticeLevel::Warn, "No video yet.");
            return;
        };
        let Some(t) = self.model.tile(id).filter(|t| t.has_frame) else {
            self.toast(NoticeLevel::Warn, "No video yet.");
            return;
        };
        let mut params = model::tile_params(&self.model.inputs(t)).params;
        params.grid = false;
        params.blur = false;
        let ppp = ctx.pixels_per_point();
        let scale = (2560.0 / rect.width()).min((640.0 / rect.width()).max(2.0 * ppp));
        let (w, h) = (
            (rect.width() * scale).round().max(1.0) as u32,
            (rect.height() * scale).round().max(1.0) as u32,
        );
        let name = self
            .model
            .headset(id)
            .map(|h| h.name.clone())
            .unwrap_or_else(|| id.to_string());
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == ' ' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let dir = snapshot_dir();
        let path = dir.join(format!("{safe}-{}.png", chrono_stamp()));
        let id = id.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = ctx.clone();
        std::thread::spawn(move || {
            let res = match video.snapshot(&id, &params, w, h) {
                Some(px) => std::fs::create_dir_all(&dir)
                    .and_then(|()| save_png(&path, w, h, &px))
                    .map(|()| path)
                    .map_err(|e| e.to_string()),
                None => Err("No video yet.".to_string()),
            };
            let _ = tx.send(res);
            wake.request_repaint();
        });
        self.snapshots.push(rx);
    }

    fn poll_snapshots(&mut self) {
        let mut done = Vec::new();
        self.snapshots.retain(|rx| match rx.try_recv() {
            Ok(r) => {
                done.push(r);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
        for r in done {
            match r {
                Ok(p) => self.toast(NoticeLevel::Ok, format!("Snapshot saved: {}", p.display())),
                Err(e) => self.toast(
                    NoticeLevel::Error,
                    format!("Could not save the snapshot: {e}"),
                ),
            }
        }
    }

    // --------------------------------------------------------- view sizes

    fn report_view_sizes(&mut self, ctx: &egui::Context, force: bool) {
        let ppp = ctx.pixels_per_point();
        let mut current: BTreeMap<String, ViewRequest> = BTreeMap::new();
        for t in &self.model.tiles {
            let r = model::tile_params(&self.model.inputs(t));
            let req = match self.video_rects.get(&t.id) {
                Some(rect)
                    if self.model.focus.as_ref().is_none_or(|f| f == &t.id)
                        && rect.width() > 1.0 =>
                {
                    model::view_request(
                        true,
                        (rect.width() * ppp, rect.height() * ppp),
                        t.view_mode,
                        &r,
                    )
                }
                _ => model::view_request(false, (0.0, 0.0), t.view_mode, &r),
            };
            current.insert(t.id.clone(), req);
        }
        let now = Instant::now();
        let diff = if force {
            Some(self.reporter.flush(&current))
        } else {
            self.reporter.update(now, &current, self.view_delay)
        };
        if let Some(d) = diff {
            self.bridge.set_view_sizes(d);
        }
        if let Some(due) = self.reporter.due() {
            ctx.request_repaint_after(due.saturating_duration_since(now));
        }
    }

    fn flush_casts(&mut self, ctx: &egui::Context) {
        if self.pending_casts.is_empty() {
            return;
        }
        self.report_view_sizes(ctx, true);
        for (id, transport, restart) in std::mem::take(&mut self.pending_casts) {
            self.send(Action::Cast {
                id,
                transport,
                restart,
                decode_failed: false,
            });
        }
    }

    // ---------------------------------------------------------- automation

    fn automation(&mut self, ctx: &egui::Context) {
        let t = self.start.elapsed().as_secs_f32();
        if !self.show_done && t > 2.0 {
            self.show_done = true;
            if let Some(what) = self.opts.show.clone() {
                self.show_for_screenshot(&what);
            }
        }
        if self.opts.offscreen {
            return;
        }
        if let Some(path) = self.opts.screenshot.clone()
            && !self.shot_requested
        {
            let at = self
                .opts
                .screenshot_after
                .or(self.opts.exit_after.map(|e| (e - 1.0).max(0.5)))
                .unwrap_or(3.0);
            if t >= at {
                self.shot_requested = true;
                ctx.request_screenshot(move |img| {
                    std::thread::spawn(move || {
                        let [w, h] = img.size;
                        let rgba: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                        match save_png(&path, w as u32, h as u32, &rgba) {
                            Ok(()) => {
                                tracing::info!("screenshot saved to {} ({w}×{h})", path.display())
                            }
                            Err(e) => tracing::error!("screenshot failed: {e}"),
                        }
                    });
                });
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        if let Some(exit) = self.opts.exit_after {
            if t >= exit {
                self.quit_ok = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
    }

    fn show_for_screenshot(&mut self, what: &str) {
        let first_tile = self.model.tiles.first().map(|t| t.id.clone());
        let first_trusted = self
            .model
            .headsets
            .iter()
            .find(|h| h.trusted && h.is_online())
            .map(|h| h.id.clone());
        match what {
            "settings" => self
                .cmds
                .push(Cmd::Open(Dialog::settings(&self.model, self.ui_scale))),
            "guide" => self.cmds.push(Cmd::Open(Dialog::Guide {
                brand: "meta".into(),
            })),
            "add-ip" => self.cmds.push(Cmd::Open(Dialog::add_ip())),
            "verify" => {
                if let Some(h) = self
                    .model
                    .headsets
                    .iter()
                    .find(|h| !h.trusted && !h.pending)
                {
                    let d = Dialog::verify(&self.model, &h.id.clone());
                    self.cmds.push(Cmd::Open(d));
                }
            }
            "edit" => {
                if let Some(id) = first_trusted {
                    let d = Dialog::edit(&self.model, &id);
                    self.cmds.push(Cmd::Open(d));
                }
            }
            "calibrate" => {
                if let Some(id) = first_tile.or(first_trusted) {
                    self.cmds.push(Cmd::Calibrate(id));
                }
            }
            "focus" => {
                if let Some(id) = first_tile {
                    self.cmds.push(Cmd::ToggleFocus(id));
                }
            }
            "nav" => self.nav_open = true,
            "snapshot" => {
                if let Some(id) = first_tile {
                    self.cmds.push(Cmd::Snapshot(id));
                }
            }
            "fix" => self
                .cmds
                .push(Cmd::Open(Dialog::Fix(Fix::BatteryLow("Demo booth".into())))),
            "status" => self.cmds.push(Cmd::Open(Dialog::Fix(Fix::NoDecoder(
                "none (not implemented yet)".into(),
            )))),
            other => tracing::warn!("--show {other}: unknown"),
        }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.text_edit_focused() || self.dialog.is_some() {
            return;
        }
        let (esc, digit) = ctx.input(|i| {
            let digit = [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .iter()
            .position(|k| i.key_pressed(*k))
            .map(|p| p as u32 + 1);
            (i.key_pressed(Key::Escape), digit)
        });
        if ctx.input(|i| i.key_pressed(Key::F11)) && !self.opts.kiosk {
            self.cmds.push(Cmd::ToggleFullscreen);
        }
        if esc {
            if self.app_full.is_some() {
                self.exit_app_full(ctx);
            } else if self.model.calib.is_some() {
                self.cmds.push(Cmd::CloseCalibration { save: false });
            } else if self.nav_open {
                self.nav_open = false;
            } else {
                self.model.set_focus(None);
            }
        }
        if let Some(d) = digit {
            self.model.focus_index(d);
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn on_exit(&mut self) {
        self.shutdown();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::from(theme::BG).to_array()
    }
}

impl App {
    /// Stop all streams and save the settings.
    pub fn shutdown(&mut self) {
        tracing::info!("shutting down: stopping streams and saving settings");
        self.bridge.shutdown();
    }

    /// One UI frame.
    pub fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if (ctx.zoom_factor() - self.ui_scale).abs() > 0.001 {
            ctx.set_zoom_factor(self.ui_scale);
        }
        // Kiosk: closing needs a confirmation (no accidental quits on a touchscreen).
        if ctx.input(|i| i.viewport().close_requested()) && self.opts.kiosk && !self.quit_ok {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.dialog = Some(Dialog::Quit);
        }
        self.handle_events();
        self.poll_snapshots();
        self.pull_frames();
        self.keyboard(&ctx);

        let screen = ctx.content_rect();
        self.narrow = screen.width() < 900.0 || screen.height() > screen.width() * 1.05;
        if !self.narrow {
            self.nav_open = false;
        }
        let full = self.app_full.is_some();

        if self.model.env.fatal.is_some() {
            self.fatal_screen(ui);
            self.dialogs(&ctx);
            self.apply_cmds(&ctx);
            self.automation(&ctx);
            return;
        }

        if !full {
            self.topbar(ui);
            self.statusbar(ui);
            if !self.narrow {
                egui::Panel::left("sidebar")
                    .exact_size(if screen.width() < 1280.0 {
                        290.0
                    } else {
                        320.0
                    })
                    .resizable(false)
                    .frame(
                        egui::Frame::new()
                            .fill(theme::BG_2)
                            .stroke(egui::Stroke::new(1.0, theme::LINE)),
                    )
                    .show(ui, |ui| self.sidebar(ui, false));
            }
            if self.model.calib.is_some() {
                self.calibration_panel(ui);
            }
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(if full {
                egui::Color32::BLACK
            } else {
                theme::BG
            }))
            .show(ui, |ui| self.stage(ui));

        if self.narrow && self.nav_open && !full {
            self.sidebar_drawer(&ctx);
        }
        self.dialogs(&ctx);
        self.toasts(&ctx);

        if self.demo_autocast
            && !self.model.headsets.is_empty()
            && self.start.elapsed() > Duration::from_millis(300)
        {
            self.demo_autocast = false;
            self.cmds.push(Cmd::CastAll);
        }
        if self.opts.cast_on_start {
            for id in self.model.castable_not_streaming() {
                if self.auto_cast_done.insert(id.clone()) {
                    tracing::info!("casting {id} (--cast-on-start)");
                    self.cmds.push(Cmd::Cast(id, None));
                }
            }
        }
        self.apply_cmds(&ctx);
        self.flush_casts(&ctx);
        self.report_view_sizes(&ctx, false);
        self.automation(&ctx);

        if let Some(next) = self.model.expire_toasts(Instant::now()) {
            ctx.request_repaint_after(next.saturating_duration_since(Instant::now()));
        }
    }

    /// The view model (for tests and tools).
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// The UI scale currently in effect (zoom on top of the display's scaling).
    pub fn ui_scale(&self) -> f32 {
        self.ui_scale
    }
}

// ------------------------------------------------------------------ files

fn snapshot_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let pictures = home.join("Pictures");
    if pictures.is_dir() {
        pictures.join("XR MultiCast")
    } else {
        home.join("XR MultiCast snapshots")
    }
}

fn chrono_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}-{:02}-{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

/// Days since 1970-01-01 → (year, month, day) (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

pub fn save_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().map_err(std::io::Error::other)?;
    writer
        .write_image_data(rgba)
        .map_err(std::io::Error::other)?;
    writer.finish().map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_403), (2025, 11, 11));
    }
}
