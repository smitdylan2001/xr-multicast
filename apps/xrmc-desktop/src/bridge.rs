//! The bridge between the UI thread and the hub.
//!
//! A tokio runtime on background threads owns the [`Hub`] (or the demo backend).
//! The UI sends [`Action`]s and receives [`UiEvent`]s; it never blocks on the hub.
//! The event loop routes video packets to per-stream decode workers
//! ([`crate::decode`]) and forwards everything else to the UI.

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::sync::{broadcast, mpsc};
use xrmc_core::codec_policy::{DecodeCaps, DecoderSupport};
use xrmc_core::device_manager::{DeviceDirectory, ScanResult, VerifyOptions};
use xrmc_core::stream_manager::{EncodersReply, StartOptions, StartResult, StreamBackend};
use xrmc_core::{
    Codec, CoreEvent, DeviceSnapshot, Notice, NoticeLevel, Settings, SharedStore, StreamManager,
    StreamState, TransportPreference,
};
use xrmc_hub::{Hub, HubOptions, HubPaths};

use crate::decode::{DecodeFailure, DecoderFactory, Router, StreamOutput, Wake};
use crate::model::EnvInfo;
use crate::view_report::ViewDiff;

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Something the user asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Cast {
        id: String,
        transport: Option<TransportPreference>,
        restart: bool,
        /// This computer could not decode the stream: fall back (10-bit → 8-bit, else → H.264).
        decode_failed: bool,
    },
    Stop {
        id: String,
    },
    CastAll,
    StopAll,
    Scan,
    Connect {
        address: String,
    },
    Verify {
        id: String,
        name: String,
        color: String,
        enable_wireless: bool,
    },
    EnableWifi {
        id: String,
    },
    DisconnectWifi {
        id: String,
    },
    Forget {
        id: String,
    },
    UpdateHeadset {
        id: String,
        patch: Map<String, Value>,
    },
    UpdateSettings {
        patch: Map<String, Value>,
    },
    Encoders {
        id: String,
    },
}

/// Result of an [`Action`].
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    None,
    Message(NoticeLevel, String),
    Scan(ScanResult),
    StartAll(Vec<StartResult>),
    Encoders(EncodersReply),
    Settings(Box<Settings>),
}

/// What the UI receives.
#[derive(Debug, Clone)]
pub enum UiEvent {
    Devices(DeviceSnapshot),
    /// A stream state with the decoder generation of its session.
    Stream {
        state: StreamState,
        generation: u64,
    },
    Notice(Notice),
    Settings(Box<Settings>),
    Env(Box<EnvInfo>),
    /// An action finished.
    Done {
        ticket: u64,
        action: Box<Action>,
        result: Result<Reply, String>,
    },
    /// The stream's video can't be decoded on this computer at all.
    DecodeFatal {
        id: String,
        generation: u64,
        message: String,
    },
}

/// Converts `xrmc-decode` capabilities into the core's decoder-support map.
pub fn decoder_support(c: &xrmc_decode::Capabilities) -> DecoderSupport {
    let caps = |d: xrmc_decode::DecodeCaps| DecodeCaps {
        any: d.any,
        hw: d.hw,
    };
    [
        ("h264", c.h264),
        ("h265", c.h265),
        ("av1", c.av1),
        ("h265_10", c.h265_10),
        ("av1_10", c.av1_10),
    ]
    .into_iter()
    .map(|(k, d)| (k.to_string(), caps(d)))
    .collect()
}

/// One-line summary of what this computer decodes (`decoderSummary`).
pub fn decoder_summary(s: &DecoderSupport) -> String {
    [("h264", "H.264"), ("h265", "H.265"), ("av1", "AV1")]
        .iter()
        .map(|(k, name)| match s.get(*k) {
            Some(d) if d.any => format!("{name} ({})", if d.hw { "hardware" } else { "software" }),
            _ => format!("{name} not supported"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The local IP address used for outgoing traffic (no packets are sent).
pub fn local_ips() -> Vec<String> {
    let probe = || -> std::io::Result<String> {
        let s = std::net::UdpSocket::bind("0.0.0.0:0")?;
        s.connect("192.0.2.1:9")?;
        Ok(s.local_addr()?.ip().to_string())
    };
    probe().into_iter().filter(|ip| ip != "0.0.0.0").collect()
}

// --------------------------------------------------------------- backends

/// The hub (real devices) or the demo; both are driven the same way.
pub trait Backend: Send + Sync + 'static {
    fn subscribe(&self) -> broadcast::Receiver<CoreEvent>;
    fn snapshot(&self) -> DeviceSnapshot;
    fn states(&self) -> Vec<StreamState>;
    fn store(&self) -> SharedStore;
    fn env(&self) -> EnvInfo;
    fn set_view_sizes(&self, diff: ViewDiff);
    fn set_decoder_support(&self, s: DecoderSupport);
    /// Publish the device list again (e.g. after settings changed `canCast`).
    fn emit_devices(&self);
    fn run(self: Arc<Self>, action: Action) -> BoxFut<Result<Reply, String>>;
    /// Start background work; the returned value keeps it alive.
    fn start(&self) -> Box<dyn Send>;
    fn stop_all(&self) -> BoxFut<()>;
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Stream actions shared by both backends.
pub(crate) async fn stream_action<B: StreamBackend, D: DeviceDirectory>(
    sm: &StreamManager<B, D>,
    action: &Action,
) -> Option<Result<Reply, String>> {
    Some(match action {
        Action::Cast {
            id,
            transport,
            restart,
            decode_failed,
        } => sm
            .start(
                id,
                StartOptions {
                    transport: *transport,
                    restart: *restart,
                    decode_failed: *decode_failed,
                    ..Default::default()
                },
            )
            .await
            .map(|()| Reply::None)
            .map_err(err),
        Action::Stop { id } => {
            sm.stop(id).await;
            Ok(Reply::None)
        }
        Action::CastAll => Ok(Reply::StartAll(sm.start_all().await)),
        Action::StopAll => {
            sm.stop_all().await;
            Ok(Reply::None)
        }
        Action::Encoders { id } => sm.encoders_for(id).await.map(Reply::Encoders).map_err(err),
        _ => return None,
    })
}

pub(crate) fn update_settings(
    store: &SharedStore,
    patch: &Map<String, Value>,
) -> Result<Reply, String> {
    let rejected = store.write(|s| s.update_settings(patch));
    let _ = store.flush();
    if !rejected.is_empty() {
        return Err(format!("Invalid settings: {}", rejected.join(", ")));
    }
    Ok(Reply::Settings(Box::new(store.settings())))
}

/// [`Backend`] over a real [`Hub`].
pub struct HubBackend {
    hub: Hub,
    env: EnvInfo,
    connect: Vec<String>,
}

impl HubBackend {
    pub fn open(cfg: &HubConfig) -> Result<HubBackend, String> {
        let mut opts = HubOptions::new(cfg.paths.clone());
        opts.registry.use_server = cfg.use_adb_server;
        opts.registry.direct_usb = cfg.direct_usb;
        let hub = Hub::open(opts).map_err(|e| format!("Could not start the hub: {e}"))?;
        let support = decoder_support(&xrmc_decode::capabilities());
        let caps = xrmc_decode::capabilities();
        let env = EnvInfo {
            demo: false,
            version: env!("CARGO_PKG_VERSION").into(),
            scrcpy_version: xrmc_scrcpy::SCRCPY_VERSION.into(),
            server_available: hub.streams.server_available(),
            server_path: cfg
                .paths
                .scrcpy_server
                .as_ref()
                .map(|p| p.display().to_string()),
            adb_mode: match (cfg.use_adb_server, cfg.direct_usb) {
                (true, true) => "adb server + direct USB/TCP".into(),
                (true, false) => "adb server + direct TCP".into(),
                (false, true) => "direct USB/TCP (no adb server)".into(),
                (false, false) => "direct TCP only".into(),
            },
            decoder: caps.backend.clone(),
            decoder_support: support
                .iter()
                .map(|(k, d)| (k.clone(), (d.any, d.hw)))
                .collect(),
            local_ips: local_ips(),
            store_path: Some(cfg.paths.store.display().to_string()),
            fatal: None,
        };
        hub.streams.set_decoder_support(Some(support));
        Ok(HubBackend {
            hub,
            env,
            connect: cfg.connect.clone(),
        })
    }
}

impl Backend for HubBackend {
    fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.hub.subscribe()
    }
    fn snapshot(&self) -> DeviceSnapshot {
        self.hub.devices.snapshot()
    }
    fn states(&self) -> Vec<StreamState> {
        self.hub.streams.states()
    }
    fn store(&self) -> SharedStore {
        self.hub.store.clone()
    }
    fn env(&self) -> EnvInfo {
        self.env.clone()
    }
    fn set_view_sizes(&self, diff: ViewDiff) {
        self.hub.streams.set_view_sizes("local", diff);
    }
    fn set_decoder_support(&self, s: DecoderSupport) {
        self.hub.streams.set_decoder_support(Some(s));
    }
    fn emit_devices(&self) {
        self.hub.devices.emit_change(true);
    }

    fn run(self: Arc<Self>, action: Action) -> BoxFut<Result<Reply, String>> {
        Box::pin(async move {
            if let Some(r) = stream_action(&self.hub.streams, &action).await {
                return r;
            }
            let dm = &self.hub.devices;
            match action {
                Action::Scan => Ok(Reply::Scan(dm.scan_network().await)),
                Action::Connect { address } => dm
                    .connect_address(&address)
                    .await
                    .map(|()| Reply::Message(NoticeLevel::Ok, format!("Connected to {address}")))
                    .map_err(err),
                Action::Verify {
                    id,
                    name,
                    color,
                    enable_wireless,
                } => dm
                    .verify(
                        &id,
                        VerifyOptions {
                            name: Some(name),
                            color: Some(color),
                            enable_wireless,
                        },
                    )
                    .await
                    .map(|_| Reply::None)
                    .map_err(err),
                Action::EnableWifi { id } => dm
                    .enable_wireless(&id)
                    .await
                    .map(|_| Reply::None)
                    .map_err(err),
                Action::DisconnectWifi { id } => {
                    dm.disconnect_wireless(&id).await;
                    Ok(Reply::Message(
                        NoticeLevel::Ok,
                        "Wi-Fi connection closed.".into(),
                    ))
                }
                Action::Forget { id } => {
                    let name = dm
                        .snapshot()
                        .headsets
                        .into_iter()
                        .find(|h| h.id == id)
                        .map(|h| h.name);
                    self.hub.streams.stop(&id).await;
                    dm.forget(&id).await;
                    Ok(Reply::Message(
                        NoticeLevel::Ok,
                        format!("{} forgotten.", name.unwrap_or(id)),
                    ))
                }
                Action::UpdateHeadset { id, patch } => {
                    dm.update(&id, &patch).map(|_| Reply::None).map_err(err)
                }
                Action::UpdateSettings { patch } => {
                    let r = update_settings(&self.hub.store, &patch);
                    dm.emit_change(true);
                    r
                }
                _ => Ok(Reply::None),
            }
        })
    }

    fn start(&self) -> Box<dyn Send> {
        let tasks = self.hub.start();
        let autosave = tokio::spawn(self.hub.store.clone().autosave(Duration::from_millis(200)));
        for address in &self.connect {
            let dm = self.hub.devices.clone();
            let address = address.clone();
            tokio::spawn(async move {
                if let Err(e) = dm.connect_address(&address).await {
                    tracing::warn!("--connect {address}: {e}");
                }
            });
        }
        Box::new((tasks, AbortOnDrop(autosave)))
    }

    fn stop_all(&self) -> BoxFut<()> {
        let sm = self.hub.streams.clone();
        let store = self.hub.store.clone();
        Box::pin(async move {
            sm.stop_all().await;
            let _ = store.flush();
        })
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ------------------------------------------------------------------ bridge

/// Settings for a real hub.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub paths: HubPaths,
    pub use_adb_server: bool,
    pub direct_usb: bool,
    pub connect: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum Mode {
    Hub(Box<HubConfig>),
    Demo { headsets: usize },
}

/// Where to find scrcpy-server: next to the executable, `XRMC_SCRCPY_SERVER`, then the repository's `vendor/`.
pub fn find_scrcpy_server() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidates.push(dir.join("scrcpy-server"));
    }
    if let Some(p) = std::env::var_os("XRMC_SCRCPY_SERVER") {
        candidates.push(PathBuf::from(p));
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    candidates.push(manifest.join("../../vendor/scrcpy-server"));
    candidates.push(manifest.join("../../../XR View tool/vendor/scrcpy-server"));
    candidates.push(PathBuf::from("vendor/scrcpy-server"));
    candidates.into_iter().find(|p| p.is_file())
}

struct Request {
    ticket: u64,
    action: Action,
}

enum Control {
    Action(Request),
    ViewSizes(ViewDiff),
}

/// UI-side handle to the hub running on background threads.
pub struct Bridge {
    tx: mpsc::UnboundedSender<Control>,
    rx: std_mpsc::Receiver<UiEvent>,
    outputs: Arc<Mutex<std::collections::HashMap<String, Arc<StreamOutput>>>>,
    backend: Option<Arc<dyn Backend>>,
    runtime: Option<tokio::runtime::Runtime>,
    next_ticket: u64,
}

impl Bridge {
    /// Start the hub (or the demo) on a background runtime. `wake` is called
    /// whenever the UI has something new to show.
    pub fn start(mode: Mode, wake: Wake, decoder_threads: u32) -> Bridge {
        let (ui_tx, rx) = std_mpsc::channel();
        let (tx, ctl_rx) = mpsc::unbounded_channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("xrmc-hub")
            .enable_all()
            .build()
            .expect("tokio runtime");
        let (backend, factory): (Result<Arc<dyn Backend>, String>, DecoderFactory) = match &mode {
            Mode::Hub(cfg) => {
                let _g = runtime.enter();
                (
                    HubBackend::open(cfg).map(|b| Arc::new(b) as Arc<dyn Backend>),
                    crate::decode::system_decoders(decoder_threads),
                )
            }
            Mode::Demo { headsets } => {
                let _g = runtime.enter();
                (
                    Ok(Arc::new(crate::demo::DemoBackend::new(*headsets)) as Arc<dyn Backend>),
                    crate::demo::demo_decoders(),
                )
            }
        };
        let (ftx, frx) = mpsc::unbounded_channel();
        let router = Router::new(factory, wake.clone(), ftx);
        let outputs = router.outputs();
        let send = {
            let wake = wake.clone();
            move |ev: UiEvent| {
                let _ = ui_tx.send(ev);
                wake();
            }
        };
        let backend = match backend {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("{e}");
                send(UiEvent::Env(Box::new(EnvInfo {
                    fatal: Some(e),
                    version: env!("CARGO_PKG_VERSION").into(),
                    ..Default::default()
                })));
                return Bridge {
                    tx,
                    rx,
                    outputs,
                    backend: None,
                    runtime: Some(runtime),
                    next_ticket: 1,
                };
            }
        };
        send(UiEvent::Env(Box::new(backend.env())));
        send(UiEvent::Settings(Box::new(backend.store().settings())));
        let b = backend.clone();
        let send2 = send.clone();
        runtime.spawn(async move {
            let events = b.subscribe();
            let keep = b.start();
            event_loop(b, events, ctl_rx, frx, router, send2).await;
            drop(keep);
        });
        Bridge {
            tx,
            rx,
            outputs,
            backend: Some(backend),
            runtime: Some(runtime),
            next_ticket: 1,
        }
    }

    /// Run an action; its result comes back as [`UiEvent::Done`] with the returned ticket.
    pub fn send(&mut self, action: Action) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        let _ = self.tx.send(Control::Action(Request { ticket, action }));
        ticket
    }

    pub fn set_view_sizes(&self, diff: ViewDiff) {
        if !diff.is_empty() {
            let _ = self.tx.send(Control::ViewSizes(diff));
        }
    }

    pub fn poll(&self) -> Vec<UiEvent> {
        self.rx.try_iter().collect()
    }

    pub fn output(&self, id: &str) -> Option<Arc<StreamOutput>> {
        self.outputs.lock().ok()?.get(id).cloned()
    }

    /// Stop all streams and save the store (waits up to a few seconds).
    pub fn shutdown(&mut self) {
        if let (Some(rt), Some(b)) = (&self.runtime, &self.backend) {
            let fut = b.stop_all();
            rt.block_on(async move {
                let _ = tokio::time::timeout(Duration::from_secs(4), fut).await;
            });
            let _ = b.store().flush();
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_millis(500));
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

async fn event_loop(
    backend: Arc<dyn Backend>,
    mut events: broadcast::Receiver<CoreEvent>,
    mut ctl: mpsc::UnboundedReceiver<Control>,
    mut failures: mpsc::UnboundedReceiver<DecodeFailure>,
    mut router: Router,
    send: impl Fn(UiEvent) + Clone + Send + 'static,
) {
    // Initial state (events before subscribing are not replayed).
    send(UiEvent::Devices(backend.snapshot()));
    for st in backend.states() {
        let generation = router.on_state(&st);
        send(UiEvent::Stream {
            state: st,
            generation,
        });
    }
    let mut fallback_requested: HashSet<(String, Codec, bool)> = HashSet::new();
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(CoreEvent::Packet { id, packet }) => router.route(&id, packet),
                Ok(CoreEvent::StreamState(st)) => {
                    let generation = router.on_state(&st);
                    send(UiEvent::Stream { state: st, generation });
                }
                Ok(CoreEvent::DevicesChanged(s)) => send(UiEvent::Devices(s)),
                Ok(CoreEvent::Notice(n)) => send(UiEvent::Notice(n)),
                Ok(CoreEvent::StreamLog { id, line }) => tracing::debug!("[{id}] {line}"),
                Ok(CoreEvent::AutoCast { id }) => {
                    let b = backend.clone();
                    let send = send.clone();
                    tokio::spawn(async move {
                        let action = Action::Cast { id, transport: None, restart: false, decode_failed: false };
                        let result = b.run(action.clone()).await;
                        if let Err(e) = &result {
                            send(UiEvent::Notice(Notice::new(NoticeLevel::Error, e.clone())));
                        }
                    });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("event loop lagged by {n} events; waiting for key frames");
                    router.resync_all();
                    send(UiEvent::Devices(backend.snapshot()));
                    for st in backend.states() {
                        let generation = router.on_state(&st);
                        send(UiEvent::Stream { state: st, generation });
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            c = ctl.recv() => match c {
                Some(Control::ViewSizes(diff)) => backend.set_view_sizes(diff),
                Some(Control::Action(Request { ticket, action })) => {
                    let b = backend.clone();
                    let send = send.clone();
                    tokio::spawn(async move {
                        let result = b.run(action.clone()).await;
                        send(UiEvent::Done { ticket, action: Box::new(action), result });
                    });
                }
                None => break,
            },
            f = failures.recv() => {
                let Some(f) = f else { continue };
                if router.generation(&f.id) != Some(f.generation) {
                    continue;
                }
                let (codec, ten_bit) = router
                    .state(&f.id)
                    .map(|s| (s.codec.unwrap_or(Codec::H264), s.ten_bit))
                    .unwrap_or((Codec::H264, false));
                let key = (f.id.clone(), codec, ten_bit);
                if (codec != Codec::H264 || ten_bit) && fallback_requested.insert(key) {
                    // 10-bit → 8-bit of the same codec, else → H.264 (the core decides).
                    let b = backend.clone();
                    let send = send.clone();
                    tokio::spawn(async move {
                        let action = Action::Cast { id: f.id, transport: None, restart: true, decode_failed: true };
                        if let Err(e) = b.run(action).await {
                            send(UiEvent::Notice(Notice::new(NoticeLevel::Error, e)));
                        }
                    });
                } else {
                    send(UiEvent::DecodeFatal {
                        id: f.id,
                        generation: f.generation,
                        message: f.reason,
                    });
                }
            }
        }
    }
}
