//! Demo mode (`--demo N`): N fake headsets that cast synthetic video through the
//! real core [`StreamManager`] (state machine, codec policy, resolution
//! matching with restarts) and the real decode workers. Nothing touches the
//! network, USB or the user's settings file.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use serde_json::{Map, Value};
use tokio::sync::{broadcast, mpsc, watch};
use xrmc_core::codec_policy::{DecodeCaps, DecoderSupport, EncoderInfo};
use xrmc_core::config::HEADSET_COLORS;
use xrmc_core::device_info::Battery;
use xrmc_core::device_manager::{DeviceDirectory, EDITABLE_HEADSET_FIELDS, ScanResult, Transport};
use xrmc_core::events::{self, EventSender};
use xrmc_core::store::StoreData;
use xrmc_core::stream_manager::{
    SessionConfig, SessionControl, SessionEvent, SessionStart, StreamBackend, StreamManagerConfig,
};
use xrmc_core::view_size::FrameSize;
use xrmc_core::{
    CoreError, CoreEvent, DeviceSnapshot, HeadsetSnapshot, Notice, NoticeLevel, SharedStore, Store,
    StreamManager, StreamState, TransportKind, TransportPreference, VideoPacket,
};
use xrmc_decode::{DecodeError, Decoder, Frame};

use crate::bridge::{Action, Backend, BoxFut, Reply, stream_action, update_settings};
use crate::decode::DecoderFactory;
use crate::model::{EnvInfo, now_ms};
use crate::view_report::ViewDiff;

/// Frames per second of the synthetic streams.
pub const DEMO_FPS: u32 = 30;

#[derive(Debug, Clone)]
struct Fixture {
    id: String,
    brand: &'static str,
    model: &'static str,
    variant: Option<&'static str>,
    panel: FrameSize,
    usb: bool,
    wifi: bool,
    ip: String,
    battery: Battery,
    /// adb state of a not-yet-identified transport (`unauthorized`).
    pending: Option<&'static str>,
}

const MODELS: [(&str, &str, Option<&str>, u32, u32); 8] = [
    ("meta", "Quest 3", None, 4128, 2208),
    ("pico", "PICO 4 Ultra", None, 4320, 2160),
    ("meta", "Quest 3S", None, 3664, 1920),
    ("htc", "VIVE Focus Vision", None, 4896, 2448),
    ("yvr", "MR", Some("pfd-mr"), 3552, 7680),
    ("androidxr", "Galaxy XR", None, 7104, 3840),
    ("meta", "Quest Pro", None, 3600, 1920),
    ("pico", "PICO 4", None, 4320, 2160),
];

const NAMES: [&str; 16] = [
    "Station 1",
    "Station 2",
    "Trainee Alex",
    "Trainee Sam",
    "Demo booth",
    "Instructor",
    "Station 7",
    "Station 8",
    "Station 9",
    "Station 10",
    "Station 11",
    "Station 12",
    "Station 13",
    "Station 14",
    "Station 15",
    "Station 16",
];

fn fixtures(n: usize) -> Vec<Fixture> {
    let mut out: Vec<Fixture> = (0..n)
        .map(|i| {
            let (brand, model, variant, w, h) = MODELS[i % MODELS.len()];
            Fixture {
                id: format!("DEMO{:04}", i + 1),
                brand,
                model,
                variant,
                panel: FrameSize { w, h },
                usb: i % 3 == 0,
                wifi: i % 3 != 0,
                ip: format!("192.168.1.{}", 101 + i),
                battery: Battery {
                    level: [86, 64, 31, 100, 12, 77, 53, 95][i % 8],
                    charging: i % 4 == 3,
                },
                pending: None,
            }
        })
        .collect();
    if n >= 2 {
        out.push(Fixture {
            id: "DEMO-NEW".into(),
            brand: "meta",
            model: "Quest 3S",
            variant: None,
            panel: FrameSize { w: 3664, h: 1920 },
            usb: true,
            wifi: false,
            ip: "192.168.1.99".into(),
            battery: Battery {
                level: 58,
                charging: true,
            },
            pending: None,
        });
        out.push(Fixture {
            id: "DEMO-AUTH".into(),
            brand: "pico",
            model: "A9210",
            variant: None,
            panel: FrameSize { w: 4320, h: 2160 },
            usb: true,
            wifi: false,
            ip: String::new(),
            battery: Battery {
                level: 0,
                charging: false,
            },
            pending: Some("unauthorized"),
        });
    }
    out
}

// --------------------------------------------------------------- directory

struct DirInner {
    fixtures: Mutex<Vec<Fixture>>,
    store: SharedStore,
}

/// The demo's device list ([`DeviceDirectory`] for the core's stream manager).
#[derive(Clone)]
pub struct DemoDirectory {
    inner: Arc<DirInner>,
}

impl DemoDirectory {
    fn fixtures(&self) -> Vec<Fixture> {
        self.inner
            .fixtures
            .lock()
            .map(|f| f.clone())
            .unwrap_or_default()
    }

    fn modify(&self, id: &str, f: impl FnOnce(&mut Fixture)) {
        if let Ok(mut list) = self.inner.fixtures.lock()
            && let Some(x) = list.iter_mut().find(|x| x.id == id)
        {
            f(x);
        }
    }

    pub fn snapshot(&self) -> DeviceSnapshot {
        let settings = self.inner.store.settings();
        let mut headsets: Vec<HeadsetSnapshot> = self
            .fixtures()
            .into_iter()
            .map(|f| {
                let rec = self.inner.store.headset(&f.id);
                let brand = xrmc_core::brands::brand_or_fallback(f.brand);
                let trusted = rec.is_some();
                HeadsetSnapshot {
                    id: if f.pending.is_some() {
                        format!("pending:{}", f.id)
                    } else {
                        f.id.clone()
                    },
                    serial: Some(f.id.clone()),
                    adb_serial: None,
                    pending: f.pending.is_some(),
                    network: f.pending.map(|_| false),
                    trusted,
                    index: rec.as_ref().and_then(|r| r.index),
                    name: rec
                        .as_ref()
                        .and_then(|r| r.name.clone())
                        .unwrap_or_else(|| {
                            if f.pending.is_some() {
                                f.model.to_string()
                            } else {
                                format!("{} {}", brand.name, f.model)
                            }
                        }),
                    color: rec
                        .as_ref()
                        .and_then(|r| r.color.clone())
                        .unwrap_or_else(|| brand.color.clone()),
                    brand: f.brand.into(),
                    model: f.model.into(),
                    model_raw: Some(f.model.into()),
                    variant: f.variant.map(String::from),
                    manufacturer: Some(brand.name.clone()),
                    android_version: Some("14".into()),
                    usb: f.usb.then(|| f.id.clone()),
                    wifi: f.wifi.then(|| format!("{}:5555", f.ip)),
                    ip: (!f.ip.is_empty()).then(|| f.ip.clone()),
                    battery: f.pending.is_none().then_some(f.battery),
                    status: f.pending.unwrap_or("online").to_string(),
                    can_cast: f.pending.is_none()
                        && (trusted || !settings.require_usb_verification),
                    calibration: rec.as_ref().and_then(|r| r.calibration.clone()),
                    transport: rec.as_ref().and_then(|r| r.transport).unwrap_or_default(),
                    view_mode: rec.as_ref().and_then(|r| r.view_mode.clone()),
                    verified_at: rec.as_ref().and_then(|r| r.verified_at),
                    last_seen: Some(now_ms()),
                    error: None,
                }
            })
            .collect();
        let rank = |h: &HeadsetSnapshot| {
            if h.pending {
                0
            } else if h.trusted {
                2
            } else {
                1
            }
        };
        headsets.sort_by_key(|h| (rank(h), h.index.unwrap_or(999)));
        DeviceSnapshot {
            headsets,
            adb_error: None,
        }
    }

    fn fixture_for_adb(&self, adb_serial: &str) -> Option<Fixture> {
        self.fixtures()
            .into_iter()
            .find(|f| f.id == adb_serial || format!("{}:5555", f.ip) == adb_serial)
    }
}

impl DeviceDirectory for DemoDirectory {
    fn headset(&self, id: &str) -> Option<HeadsetSnapshot> {
        self.snapshot()
            .headsets
            .into_iter()
            .find(|h| !h.pending && h.id == id)
    }

    fn headsets(&self) -> Vec<HeadsetSnapshot> {
        self.snapshot()
            .headsets
            .into_iter()
            .filter(|h| !h.pending)
            .collect()
    }

    fn pick_transport(&self, id: &str, preference: TransportPreference) -> Option<Transport> {
        let f = self
            .fixtures()
            .into_iter()
            .find(|f| f.id == id && f.pending.is_none())?;
        let usb = || {
            f.usb.then(|| Transport {
                adb_serial: f.id.clone(),
                kind: TransportKind::Usb,
            })
        };
        let wifi = || {
            f.wifi.then(|| Transport {
                adb_serial: format!("{}:5555", f.ip),
                kind: TransportKind::Wifi,
            })
        };
        match preference {
            TransportPreference::Usb => usb(),
            TransportPreference::Wifi => wifi(),
            TransportPreference::Auto => usb().or_else(wifi),
        }
    }
}

// ----------------------------------------------------------------- streams

/// Stops a demo session.
pub struct DemoSession(watch::Sender<bool>);

impl SessionControl for DemoSession {
    fn stop(&self) {
        let _ = self.0.send(true);
    }
}

/// scrcpy-like output size: crop, then scale so the long side fits `max_size` (multiples of 8).
pub fn session_size(panel: FrameSize, config: &SessionConfig) -> (u32, u32) {
    let (mut w, mut h) = (panel.w, panel.h);
    if let Some(c) = config
        .crop
        .as_deref()
        .and_then(xrmc_core::stream_manager::policy::parse_crop)
    {
        (w, h) = (c.w, c.h);
    }
    let long = w.max(h);
    if config.max_size > 0 && long > config.max_size {
        let s = config.max_size as f64 / long as f64;
        w = (w as f64 * s).round() as u32;
        h = (h as f64 * s).round() as u32;
    }
    ((w / 8 * 8).max(8), (h / 8 * 8).max(8))
}

/// Synthetic packet payload: magic, width, height, sequence, style, padded to `len`
/// bytes so the bitrate (and the work of moving packets around) is realistic.
fn packet_data(w: u32, h: u32, seq: u32, style: u32, len: usize) -> Bytes {
    let mut v = Vec::with_capacity(len.max(20));
    v.extend_from_slice(b"XRMC");
    for x in [w, h, seq, style] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.resize(len.max(20), 0);
    Bytes::from(v)
}

fn parse_packet(d: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if d.len() < 20 || &d[..4] != b"XRMC" {
        return None;
    }
    let n = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
    Some((n(4), n(8), n(12), n(16)))
}

#[derive(Clone)]
pub struct DemoStreams {
    dir: DemoDirectory,
}

impl StreamBackend for DemoStreams {
    type Session = DemoSession;

    fn server_available(&self) -> bool {
        true
    }

    async fn list_encoders(&self, _adb_serial: &str) -> Result<Vec<EncoderInfo>, CoreError> {
        Ok(vec![
            EncoderInfo {
                codec: "h264".into(),
                name: "c2.qti.avc.encoder".into(),
                hw: true,
                alias: false,
            },
            EncoderInfo {
                codec: "h265".into(),
                name: "c2.qti.hevc.encoder".into(),
                hw: true,
                alias: false,
            },
        ])
    }

    async fn panel_size(&self, adb_serial: &str) -> Result<Option<FrameSize>, CoreError> {
        Ok(self.dir.fixture_for_adb(adb_serial).map(|f| f.panel))
    }

    async fn start_session(
        &self,
        adb_serial: &str,
        config: SessionConfig,
    ) -> Result<SessionStart<DemoSession>, CoreError> {
        let f = self
            .dir
            .fixture_for_adb(adb_serial)
            .ok_or(CoreError::NotConnected)?;
        let style = self
            .dir
            .fixtures()
            .iter()
            .position(|x| x.id == f.id)
            .unwrap_or(0) as u32;
        let (w, h) = session_size(f.panel, &config);
        let frame_bytes = (config.bit_rate / 8 / DEMO_FPS) as usize;
        let (tx, events) = mpsc::channel(64);
        let (stop_tx, mut stop) = watch::channel(false);
        tokio::spawn(async move {
            // Connecting and starting the encoder takes a moment on a real headset.
            // `XRMC_DEMO_CONNECT_MS` slows this down (e.g. to look at "Changing resolution").
            let base = std::env::var("XRMC_DEMO_CONNECT_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(250);
            tokio::time::sleep(Duration::from_millis(base + 40 * style as u64)).await;
            let _ = tx.send(SessionEvent::Device(f.model.to_string())).await;
            let _ = tx
                .send(SessionEvent::Session {
                    width: w,
                    height: h,
                })
                .await;
            let mut iv = tokio::time::interval(Duration::from_secs(1) / DEMO_FPS);
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut seq = 0u32;
            let config_packet = SessionEvent::Packet(VideoPacket {
                config: true,
                key: false,
                pts: 0,
                data: packet_data(w, h, 0, style, 40),
            });
            if tx.send(config_packet).await.is_err() {
                return;
            }
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = iv.tick() => {
                        let key = seq.is_multiple_of(DEMO_FPS * 2);
                        let p = VideoPacket {
                            config: false,
                            key,
                            pts: seq as u64 * 1_000_000 / DEMO_FPS as u64,
                            data: packet_data(w, h, seq, style, if key { frame_bytes * 4 } else { frame_bytes }),
                        };
                        seq = seq.wrapping_add(1);
                        if tx.send(SessionEvent::Packet(p)).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(SessionEvent::End).await;
        });
        Ok(SessionStart {
            session: DemoSession(stop_tx),
            events,
        })
    }
}

// ----------------------------------------------------------------- decoder

/// Turns demo packets into synthetic stereo panel frames (two lens-shaped eye
/// views of `xrmc_decode::test_pattern`, tinted per headset, with a moving marker).
#[derive(Default)]
pub struct DemoDecoder {
    base: Option<((u32, u32, u32), Frame)>,
}

const TINTS: [(i32, i32); 8] = [
    (-20, 40),
    (30, -25),
    (-35, -10),
    (20, 30),
    (40, 10),
    (-10, -35),
    (10, 45),
    (-30, 25),
];

/// The static part of a demo frame.
pub fn demo_base(w: u32, h: u32, style: u32) -> Frame {
    let mut f = xrmc_decode::test_pattern(w, h, style * 7);
    let (wu, hu) = (w as usize, h as usize);
    let eyes = if wu as f32 >= hu as f32 * 1.6 {
        (2, 1)
    } else if hu as f32 >= wu as f32 * 1.6 {
        (1, 2)
    } else {
        (1, 1)
    };
    let (ew, eh) = (wu as f32 / eyes.0 as f32, hu as f32 / eyes.1 as f32);
    let inside = |x: f32, y: f32| {
        let cx = ((x / ew).floor() + 0.5) * ew;
        let cy = ((y / eh).floor() + 0.5) * eh;
        let dx = (x - cx) / (ew * 0.47);
        let dy = (y - cy) / (eh * 0.47);
        dx * dx + dy * dy <= 1.0
    };
    let ys = f.planes[0].stride;
    for row in 0..hu {
        for col in 0..wu {
            if !inside(col as f32, row as f32) {
                f.planes[0].data[row * ys + col] = 16;
            }
        }
    }
    let (du, dv) = TINTS[style as usize % TINTS.len()];
    for p in 1..3 {
        let plane = &mut f.planes[p];
        let (pw, ph, stride) = (plane.width as usize, plane.height as usize, plane.stride);
        for row in 0..ph {
            for col in 0..pw {
                let on = inside(col as f32 * 2.0, row as f32 * 2.0);
                let d = if p == 1 { du } else { dv };
                plane.data[row * stride + col] = if on { (128 + d) as u8 } else { 128 };
            }
        }
    }
    f
}

impl Decoder for DemoDecoder {
    fn push(
        &mut self,
        data: &[u8],
        pts: u64,
        _key: bool,
        config: bool,
    ) -> Result<Vec<Frame>, DecodeError> {
        let (w, h, seq, style) =
            parse_packet(data).ok_or_else(|| DecodeError::Decode("not a demo packet".into()))?;
        if config {
            return Ok(Vec::new());
        }
        if w == 0 || h == 0 || w > 8192 || h > 8192 {
            return Err(DecodeError::Decode("bad size".into()));
        }
        let key = (w, h, style);
        if self.base.as_ref().is_none_or(|(k, _)| *k != key) {
            self.base = Some((key, demo_base(w, h, style)));
        }
        let mut f = self.base.as_ref().map(|(_, f)| f.clone()).expect("base");
        f.pts = pts;
        // A bright marker sweeping across each eye so the stream visibly moves.
        let (wu, hu) = (w as usize, h as usize);
        let stride = f.planes[0].stride;
        let size = (hu / 14).max(2);
        let eyes = if wu as f32 >= hu as f32 * 1.6 { 2 } else { 1 };
        let ew = wu / eyes;
        let t = (seq % 120) as f32 / 120.0;
        let x0 = (ew as f32 * (0.3 + 0.4 * t)) as usize;
        let y0 = hu / 2 - size / 2;
        for e in 0..eyes {
            for row in y0..(y0 + size).min(hu) {
                let start = row * stride + e * ew + x0;
                let end = (start + size).min(row * stride + wu);
                f.planes[0].data[start..end].fill(235);
            }
        }
        Ok(vec![f])
    }

    fn backend(&self) -> &str {
        "demo (synthetic frames)"
    }
}

pub fn demo_decoders() -> DecoderFactory {
    Arc::new(|_| Ok(Box::new(DemoDecoder::default()) as Box<dyn Decoder>))
}

// ----------------------------------------------------------------- backend

pub struct DemoBackend {
    dir: DemoDirectory,
    streams: StreamManager<DemoStreams, DemoDirectory>,
    store: SharedStore,
    events: EventSender,
}

impl DemoBackend {
    pub fn new(n: usize) -> DemoBackend {
        let store = SharedStore::new(Store::in_memory(StoreData::default()));
        let list = fixtures(n);
        store.write(|s| {
            for (i, f) in list
                .iter()
                .filter(|f| f.pending.is_none() && f.id != "DEMO-NEW")
                .enumerate()
            {
                s.modify_headset(&f.id, |r| {
                    r.name = Some(NAMES[i % NAMES.len()].to_string());
                    r.color = Some(HEADSET_COLORS[i % HEADSET_COLORS.len()].to_string());
                    r.index = Some(i as u32 + 1);
                    r.brand = Some(f.brand.to_string());
                    r.model = Some(f.model.to_string());
                    r.verified_at = Some(now_ms());
                    r.ip = Some(f.ip.clone());
                });
            }
            s.modify_settings(|st| st.next_index = Some(n as u32 + 1));
        });
        let (events, _) = events::channel(1024);
        let dir = DemoDirectory {
            inner: Arc::new(DirInner {
                fixtures: Mutex::new(list),
                store: store.clone(),
            }),
        };
        let streams = StreamManager::new(
            DemoStreams { dir: dir.clone() },
            dir.clone(),
            store.clone(),
            events.clone(),
            StreamManagerConfig::default(),
        );
        let support: DecoderSupport = ["h264", "h265"]
            .into_iter()
            .map(|k| {
                (
                    k.to_string(),
                    DecodeCaps {
                        any: true,
                        hw: true,
                    },
                )
            })
            .collect();
        streams.set_decoder_support(Some(support));
        DemoBackend {
            dir,
            streams,
            store,
            events,
        }
    }

    fn emit(&self) {
        let _ = self
            .events
            .send(CoreEvent::DevicesChanged(self.dir.snapshot()));
    }

    fn notice(&self, level: NoticeLevel, msg: impl Into<String>) {
        let _ = self.events.send(CoreEvent::Notice(Notice::new(level, msg)));
    }
}

impl Backend for DemoBackend {
    fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }
    fn snapshot(&self) -> DeviceSnapshot {
        self.dir.snapshot()
    }
    fn states(&self) -> Vec<StreamState> {
        self.streams.states()
    }
    fn store(&self) -> SharedStore {
        self.store.clone()
    }
    fn env(&self) -> EnvInfo {
        EnvInfo {
            demo: true,
            version: env!("CARGO_PKG_VERSION").into(),
            scrcpy_version: xrmc_scrcpy::SCRCPY_VERSION.into(),
            server_available: true,
            server_path: None,
            adb_mode: "demo (no devices)".into(),
            decoder: "demo (synthetic frames)".into(),
            decoder_support: [("h264", (true, true)), ("h265", (true, true))]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            decoder_details: ["h264", "h265", "av1", "h265_10", "av1_10"]
                .into_iter()
                .map(|k| {
                    let on = k == "h264" || k == "h265";
                    xrmc_decode::CodecDetail {
                        key: k.into(),
                        caps: xrmc_decode::DecodeCaps { any: on, hw: on },
                        decoder: if on { "demo".into() } else { String::new() },
                        hw_api: on.then(|| "demo".into()),
                    }
                })
                .collect(),
            local_ips: crate::bridge::local_ips(),
            store_path: None,
            fatal: None,
        }
    }
    fn set_view_sizes(&self, diff: ViewDiff) {
        self.streams.set_view_sizes("local", diff);
    }
    fn set_decoder_support(&self, s: DecoderSupport) {
        self.streams.set_decoder_support(Some(s));
    }
    fn emit_devices(&self) {
        self.emit();
    }

    fn run(self: Arc<Self>, action: Action) -> BoxFut<Result<Reply, String>> {
        Box::pin(async move {
            if let Some(r) = stream_action(&self.streams, &action).await {
                return r;
            }
            match action {
                Action::Scan => {
                    tokio::time::sleep(Duration::from_millis(1200)).await;
                    let n = self.dir.fixtures().iter().filter(|f| f.wifi).count();
                    Ok(Reply::Scan(ScanResult {
                        probed: n,
                        ..Default::default()
                    }))
                }
                Action::Connect { address } => Err(format!(
                    "Demo mode: {address} is not reachable (no real headsets are used)."
                )),
                Action::Verify {
                    id,
                    name,
                    color,
                    enable_wireless,
                } => {
                    let f = self
                        .dir
                        .fixtures()
                        .into_iter()
                        .find(|f| f.id == id)
                        .ok_or("Headset is not connected.")?;
                    let label = self.store.write(|s| {
                        let index = s.settings().next_index.unwrap_or(1);
                        s.modify_settings(|st| st.next_index = Some(index + 1));
                        let label = if name.trim().is_empty() {
                            format!("{} {} #{index}", f.brand, f.model)
                        } else {
                            name.trim().to_string()
                        };
                        s.modify_headset(&id, |r| {
                            r.name = Some(label.clone());
                            r.color = Some(color);
                            r.index = Some(index);
                            r.brand = Some(f.brand.into());
                            r.model = Some(f.model.into());
                            r.verified_at = Some(now_ms());
                        });
                        label
                    });
                    self.notice(NoticeLevel::Ok, format!("Verified {label}."));
                    if enable_wireless {
                        self.dir.modify(&id, |f| f.wifi = true);
                        self.notice(
                            NoticeLevel::Ok,
                            format!(
                                "Wi-Fi casting enabled at {}. You can unplug the USB cable.",
                                f.ip
                            ),
                        );
                    }
                    self.emit();
                    Ok(Reply::None)
                }
                Action::EnableWifi { id } => {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    self.dir.modify(&id, |f| f.wifi = true);
                    self.emit();
                    Ok(Reply::Message(
                        NoticeLevel::Ok,
                        "Wi-Fi casting enabled. You can unplug the USB cable.".into(),
                    ))
                }
                Action::DisconnectWifi { id } => {
                    self.dir.modify(&id, |f| f.wifi = false);
                    self.emit();
                    Ok(Reply::Message(
                        NoticeLevel::Ok,
                        "Wi-Fi connection closed.".into(),
                    ))
                }
                Action::Forget { id } => {
                    self.streams.stop(&id).await;
                    let rec = self.store.write(|s| s.remove_headset(&id));
                    self.emit();
                    Ok(Reply::Message(
                        NoticeLevel::Ok,
                        format!("{} forgotten.", rec.and_then(|r| r.name).unwrap_or(id)),
                    ))
                }
                Action::UpdateHeadset { id, patch } => {
                    let allowed: Map<String, Value> = patch
                        .into_iter()
                        .filter(|(k, _)| EDITABLE_HEADSET_FIELDS.contains(&k.as_str()))
                        .collect();
                    let ok = self.store.write(|s| {
                        s.headset(&id)?;
                        Some(s.upsert_headset(&id, &allowed).clone())
                    });
                    if ok.is_none() {
                        return Err(CoreError::NotTrusted.to_string());
                    }
                    self.emit();
                    Ok(Reply::None)
                }
                Action::UpdateSettings { patch } => {
                    let r = update_settings(&self.store, &patch);
                    self.emit();
                    r
                }
                _ => Ok(Reply::None),
            }
        })
    }

    fn start(&self) -> Box<dyn Send> {
        // Battery levels drift a little, like real headsets.
        let dir = self.dir.clone();
        let events = self.events.clone();
        let task = tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(30));
            iv.tick().await;
            loop {
                iv.tick().await;
                if let Ok(mut list) = dir.inner.fixtures.lock() {
                    for f in list.iter_mut().filter(|f| f.pending.is_none()) {
                        f.battery.level = if f.battery.charging {
                            (f.battery.level + 1).min(100)
                        } else {
                            f.battery.level.saturating_sub(1).max(5)
                        };
                    }
                }
                let _ = events.send(CoreEvent::DevicesChanged(dir.snapshot()));
            }
        });
        Box::new(Abort(task))
    }

    fn stop_all(&self) -> BoxFut<()> {
        let sm = self.streams.clone();
        Box::pin(async move { sm.stop_all().await })
    }
}

struct Abort(tokio::task::JoinHandle<()>);

impl Drop for Abort {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Panel sizes of the demo headsets, by id (for tests).
pub fn demo_panels(n: usize) -> HashMap<String, FrameSize> {
    fixtures(n).into_iter().map(|f| (f.id, f.panel)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use xrmc_core::layout;

    #[test]
    fn demo_frames_look_like_stereo_panels() {
        let mut d = DemoDecoder::default();
        assert!(
            d.push(&packet_data(320, 160, 0, 1, 20), 0, false, true)
                .unwrap()
                .is_empty()
        );
        let f = d
            .push(&packet_data(320, 160, 5, 1, 1000), 0, true, false)
            .unwrap()
            .remove(0);
        assert_eq!((f.width, f.height), (320, 160));
        let l = crate::decode::luma_grid(&f, crate::decode::DETECT_SIZE).unwrap();
        let g = layout::detect_grid(&l, 96, 96).unwrap();
        assert_eq!((g.cols, g.rows), (2, 1));
        assert!(d.push(b"nope", 0, false, false).is_err());
    }

    #[test]
    fn session_sizes_follow_scrcpy_rules() {
        let panel = FrameSize { w: 4128, h: 2208 };
        let mut cfg = SessionConfig {
            codec: xrmc_core::Codec::H265,
            encoder: None,
            max_size: 1920,
            bit_rate: 8_000_000,
            max_fps: 60,
            crop: None,
            codec_options: vec![],
        };
        assert_eq!(session_size(panel, &cfg), (1920, 1024));
        cfg.max_size = 0;
        assert_eq!(session_size(panel, &cfg), (4128, 2208));
        cfg.crop = Some("2064:2208:0:0".into());
        cfg.max_size = 1280;
        assert_eq!(session_size(panel, &cfg), (1192, 1280));
    }

    #[test]
    fn directory_lists_verified_unverified_and_pending() {
        let b = DemoBackend::new(3);
        let snap = b.snapshot();
        assert_eq!(snap.headsets.len(), 5);
        assert!(snap.headsets[0].pending);
        let trusted: Vec<_> = snap.headsets.iter().filter(|h| h.trusted).collect();
        assert_eq!(trusted.len(), 3);
        assert_eq!(trusted[0].index, Some(1));
        assert!(trusted.iter().all(|h| h.can_cast));
        let new = snap.headsets.iter().find(|h| h.id == "DEMO-NEW").unwrap();
        assert!(!new.trusted && !new.can_cast && new.usb.is_some());
        let t = b
            .dir
            .pick_transport("DEMO0002", TransportPreference::Auto)
            .unwrap();
        assert_eq!(t.kind, TransportKind::Wifi);
        assert!(
            b.dir
                .pick_transport("DEMO0002", TransportPreference::Usb)
                .is_none()
        );
    }
}
