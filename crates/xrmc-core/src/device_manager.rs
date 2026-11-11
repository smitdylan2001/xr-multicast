//! Device lifecycle: polling adb transports, identifying headsets, USB
//! verification, Wi-Fi enablement and auto-reconnect (port of
//! `src/main/device-manager.js`).
//!
//! A headset is keyed by its hardware serial (`ro.serialno`) and can be
//! reachable over several adb transports at once (USB and Wi-Fi). Verification
//! must happen over USB: that proves physical access, and the verified record
//! (name, colour, number, calibration, last Wi-Fi address...) is stored with the
//! same fields as the Electron app.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::io::AsyncReadExt;
use tokio::task::{JoinHandle, JoinSet};
use xrmc_adb::{AdbDevice, AdbError};

use crate::brands::{self, brand_or_fallback, detect_brand, detect_variant, friendly_model};
use crate::config::{DEFAULT_ADB_TCP_PORT, HEADSET_COLORS, TransportKind, TransportPreference};
use crate::device_info::{
    self, BATTERY_COMMAND, Battery, DeviceInfo, IDENTIFY_COMMAND, IP_COMMAND, TransportEntry,
    VERIFY_COMMAND, VERIFY_TOKEN, host_of, parse_battery, parse_identify, parse_ip,
};
use crate::discovery::{self, MdnsService, ScanOptions};
use crate::events::{CoreEvent, EventSender, Notice, NoticeLevel};
use crate::store::{HeadsetRecord, SharedStore, Store};
use crate::{CoreError, js};

/// Host-level adb operations (the `adb` command-line verbs that aren't per device).
///
/// Implemented on top of `xrmc-adb` (direct USB/TCP or through an adb server) by
/// the app, and by fakes in tests.
pub trait AdbBackend: Send + Sync + 'static {
    type Device: AdbDevice + 'static;

    /// All transports, like `adb devices -l` (emulators may be omitted).
    fn transports(&self) -> impl Future<Output = xrmc_adb::Result<Vec<TransportEntry>>> + Send;

    /// A handle for running commands on one transport.
    fn device(
        &self,
        adb_serial: &str,
    ) -> impl Future<Output = xrmc_adb::Result<Self::Device>> + Send;

    /// Connect to a TCP transport (`adb connect host:port`).
    fn connect(
        &self,
        address: &str,
        timeout: Duration,
    ) -> impl Future<Output = xrmc_adb::Result<()>> + Send;

    /// Drop a TCP transport (`adb disconnect host:port`).
    fn disconnect(&self, address: &str) -> impl Future<Output = xrmc_adb::Result<()>> + Send;
}

/// LAN access used by scans and auto-reconnect (replaceable in tests).
pub trait Network: Send + Sync + 'static {
    /// Is `host:port` accepting TCP connections?
    fn probe(&self, host: &str, port: u16, timeout: Duration) -> impl Future<Output = bool> + Send;
    /// Hosts on the local subnet(s) with `port` open.
    fn scan(&self, port: u16) -> impl Future<Output = Vec<Ipv4Addr>> + Send;
    /// adb services advertised over mDNS.
    fn mdns(&self) -> impl Future<Output = Vec<MdnsService>> + Send;
}

/// The real network: TCP probes, subnet scan and mDNS from [`crate::discovery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanNetwork {
    pub scan: ScanOptions,
    /// How long to listen for mDNS answers.
    pub mdns_window: Duration,
}

impl Default for LanNetwork {
    fn default() -> Self {
        LanNetwork {
            scan: ScanOptions::default(),
            mdns_window: Duration::from_millis(1500),
        }
    }
}

impl Network for LanNetwork {
    async fn probe(&self, host: &str, port: u16, timeout: Duration) -> bool {
        discovery::probe(host, port, timeout).await
    }

    async fn scan(&self, port: u16) -> Vec<Ipv4Addr> {
        discovery::scan_for_adb(port, self.scan).await
    }

    async fn mdns(&self) -> Vec<MdnsService> {
        discovery::discover_adb_services(self.mdns_window)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!("{e}");
                Vec::new()
            })
    }
}

/// One adb transport chosen for a headset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transport {
    pub adb_serial: String,
    pub kind: TransportKind,
}

/// What the stream manager needs to know about devices.
pub trait DeviceDirectory: Send + Sync + 'static {
    /// The snapshot entry of a headset (by hardware serial).
    fn headset(&self, id: &str) -> Option<HeadsetSnapshot>;
    /// All headsets (not pending transports).
    fn headsets(&self) -> Vec<HeadsetSnapshot>;
    /// The adb transport to use for a headset.
    fn pick_transport(&self, id: &str, preference: TransportPreference) -> Option<Transport>;
}

/// A headset (or a not-yet-identified transport) as shown in the UI.
///
/// Matches the Electron snapshot objects. Regular entries carry the headset
/// fields; pending ones (`pending: true`) describe a transport that is
/// unauthorized, offline, being identified or failed to identify.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadsetSnapshot {
    /// Hardware serial, or `pending:<adb serial>`.
    pub id: String,
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adb_serial: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    pub trusted: bool,
    pub index: Option<u32>,
    pub name: String,
    pub color: String,
    pub brand: String,
    pub model: String,
    pub model_raw: Option<String>,
    pub variant: Option<String>,
    pub manufacturer: Option<String>,
    pub android_version: Option<String>,
    /// USB adb serial when connected over USB.
    pub usb: Option<String>,
    /// Wi-Fi adb serial when connected over Wi-Fi.
    pub wifi: Option<String>,
    pub ip: Option<String>,
    pub battery: Option<Battery>,
    /// `online` | `offline` for headsets; the adb state (`unauthorized`, `identifying`, `error`, ...) for pending ones.
    pub status: String,
    pub can_cast: bool,
    pub calibration: Option<Map<String, Value>>,
    pub transport: TransportPreference,
    pub view_mode: Option<String>,
    pub verified_at: Option<u64>,
    pub last_seen: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl HeadsetSnapshot {
    pub fn is_online(&self) -> bool {
        self.status == "online"
    }
}

/// The device list sent to UIs (`{ headsets, adbError }`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSnapshot {
    pub headsets: Vec<HeadsetSnapshot>,
    pub adb_error: Option<String>,
}

/// Options for [`DeviceManager::verify`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VerifyOptions {
    pub name: Option<String>,
    pub color: Option<String>,
    /// Also switch the headset to Wi-Fi casting.
    pub enable_wireless: bool,
}

/// Result of [`DeviceManager::scan_network`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanResult {
    /// Hosts that had the adb port open.
    pub probed: usize,
    /// New transports.
    pub connected: usize,
    /// New transports of headsets that aren't verified.
    pub unverified: usize,
    /// Network transports waiting for "Allow debugging" on the headset.
    pub pending_auth: usize,
}

/// Timings (defaults match the Electron app).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceManagerConfig {
    pub poll_interval: Duration,
    pub info_refresh_interval: Duration,
    pub reconnect_interval: Duration,
    pub identify_timeout: Duration,
    pub shell_timeout: Duration,
    pub connect_timeout: Duration,
    pub reconnect_probe_timeout: Duration,
    /// Wait after `tcpip` before the first connect attempt.
    pub wireless_first_delay: Duration,
    pub wireless_retry_delay: Duration,
    pub wireless_attempts: u32,
}

impl Default for DeviceManagerConfig {
    fn default() -> Self {
        DeviceManagerConfig {
            poll_interval: Duration::from_secs(2),
            info_refresh_interval: Duration::from_secs(30),
            reconnect_interval: Duration::from_secs(15),
            identify_timeout: Duration::from_secs(12),
            shell_timeout: Duration::from_secs(6),
            connect_timeout: Duration::from_secs(5),
            reconnect_probe_timeout: Duration::from_millis(700),
            wireless_first_delay: Duration::from_millis(2000),
            wireless_retry_delay: Duration::from_millis(1200),
            wireless_attempts: 6,
        }
    }
}

#[derive(Debug, Clone)]
struct LiveHeadset {
    info: DeviceInfo,
    brand: String,
    model: String,
    /// adb serial → is a network transport, in connection order.
    transports: Vec<(String, bool)>,
    battery: Option<Battery>,
    ip: Option<String>,
}

impl LiveHeadset {
    fn usb_serial(&self) -> Option<&str> {
        self.transports
            .iter()
            .find(|(_, net)| !net)
            .map(|(s, _)| s.as_str())
    }

    fn wifi_serial(&self) -> Option<&str> {
        self.transports
            .iter()
            .find(|(_, net)| *net)
            .map(|(s, _)| s.as_str())
    }
}

#[derive(Debug, Clone)]
struct PendingTransport {
    state: String,
    network: bool,
    props: BTreeMap<String, String>,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    /// hardware serial → live headset
    headsets: HashMap<String, LiveHeadset>,
    /// adb serial → hardware serial (identified transports)
    transports: HashMap<String, String>,
    /// adb serial → unauthorized / offline / identifying / failed transports
    pending: BTreeMap<String, PendingTransport>,
    identifying: HashSet<String>,
    adb_error: Option<String>,
    last_snapshot: Option<DeviceSnapshot>,
}

fn pick(hs: &LiveHeadset, preference: TransportPreference) -> Option<Transport> {
    let usb = || {
        hs.usb_serial().map(|s| Transport {
            adb_serial: s.to_string(),
            kind: TransportKind::Usb,
        })
    };
    let wifi = || {
        hs.wifi_serial().map(|s| Transport {
            adb_serial: s.to_string(),
            kind: TransportKind::Wifi,
        })
    };
    match preference {
        TransportPreference::Usb => usb(),
        TransportPreference::Wifi => wifi(),
        TransportPreference::Auto => usb().or_else(wifi),
    }
}

/// Build the UI snapshot from live state and the store.
fn build_snapshot(st: &State, store: &Store) -> DeviceSnapshot {
    let settings = store.settings();
    let ids: BTreeSet<&str> = st
        .headsets
        .keys()
        .map(String::as_str)
        .chain(store.headsets().map(|r| r.serial.as_str()))
        .collect();
    let mut out = Vec::new();
    for id in ids {
        let live = st.headsets.get(id);
        let rec = store.headset(id);
        let brand_id = live
            .map(|l| l.brand.clone())
            .or_else(|| rec.and_then(|r| r.brand.clone()))
            .unwrap_or_else(|| brands::FALLBACK_BRAND.to_string());
        let brand = brand_or_fallback(&brand_id);
        let usb = live.and_then(|l| l.usb_serial()).map(String::from);
        let wifi = live.and_then(|l| l.wifi_serial()).map(String::from);
        let online = usb.is_some() || wifi.is_some();
        let model = live
            .map(|l| l.model.clone())
            .filter(|m| !m.is_empty())
            .or_else(|| rec.and_then(|r| r.model.clone()))
            .unwrap_or_else(|| "Unknown".to_string());
        let variant_info = match live {
            Some(l) => l.info.clone(),
            None => DeviceInfo {
                model: rec.and_then(|r| r.model_raw.clone()).unwrap_or_default(),
                ..Default::default()
            },
        };
        out.push(HeadsetSnapshot {
            id: id.to_string(),
            serial: Some(id.to_string()),
            adb_serial: None,
            pending: false,
            network: None,
            trusted: rec.is_some(),
            index: rec.and_then(|r| r.index),
            name: rec
                .and_then(|r| r.name.clone())
                .unwrap_or_else(|| format!("{} {model}", brand.name)),
            color: rec
                .and_then(|r| r.color.clone())
                .unwrap_or_else(|| brand.color.clone()),
            brand: brand_id.clone(),
            model,
            model_raw: live
                .map(|l| l.info.model.clone())
                .or_else(|| rec.and_then(|r| r.model_raw.clone())),
            variant: detect_variant(&brand_id, &variant_info).map(String::from),
            manufacturer: live
                .map(|l| l.info.manufacturer.clone())
                .or_else(|| rec.and_then(|r| r.manufacturer.clone())),
            android_version: live.map(|l| l.info.android_version.clone()),
            usb,
            wifi,
            ip: live
                .and_then(|l| l.ip.clone())
                .or_else(|| rec.and_then(|r| r.ip.clone())),
            battery: live.and_then(|l| l.battery),
            status: if online { "online" } else { "offline" }.to_string(),
            can_cast: online && (rec.is_some() || !settings.require_usb_verification),
            calibration: rec.and_then(|r| r.calibration.clone()),
            transport: rec.and_then(|r| r.transport).unwrap_or_default(),
            view_mode: rec.and_then(|r| r.view_mode.clone()),
            verified_at: rec.and_then(|r| r.verified_at),
            last_seen: rec.and_then(|r| r.last_seen),
            error: None,
        });
    }
    for (adb_serial, p) in &st.pending {
        if st.transports.contains_key(adb_serial) {
            continue;
        }
        let prop = |k: &str| p.props.get(k).cloned().unwrap_or_default();
        let guess = detect_brand(&DeviceInfo {
            model: prop("model"),
            device: prop("device"),
            product: prop("product"),
            ..Default::default()
        });
        let model = prop("model");
        out.push(HeadsetSnapshot {
            id: format!("pending:{adb_serial}"),
            serial: None,
            adb_serial: Some(adb_serial.clone()),
            pending: true,
            network: Some(p.network),
            trusted: false,
            index: None,
            name: if model.is_empty() {
                adb_serial.clone()
            } else {
                model.clone()
            }
            .replace('_', " "),
            color: brand_or_fallback(guess).color.clone(),
            brand: guess.to_string(),
            model: model.replace('_', " "),
            model_raw: None,
            variant: None,
            manufacturer: None,
            android_version: None,
            usb: None,
            wifi: None,
            ip: None,
            battery: None,
            status: p.state.clone(),
            can_cast: false,
            calibration: None,
            transport: TransportPreference::Auto,
            view_mode: None,
            verified_at: None,
            last_seen: None,
            error: p.error.clone(),
        });
    }
    let rank = |h: &HeadsetSnapshot| -> u8 {
        if h.pending {
            0
        } else if h.is_online() {
            if h.trusted { 2 } else { 1 }
        } else {
            3
        }
    };
    out.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.index.unwrap_or(999).cmp(&b.index.unwrap_or(999)))
            .then_with(|| js::locale_cmp(&a.name, &b.name))
    });
    DeviceSnapshot {
        headsets: out,
        adb_error: st.adb_error.clone(),
    }
}

/// Fields a user may change on a verified headset (`update` in device-manager.js).
pub const EDITABLE_HEADSET_FIELDS: [&str; 5] =
    ["name", "color", "calibration", "transport", "viewMode"];

/// Validate a headset patch from an untrusted remote viewer (`remotePatch` in main.js).
pub fn remote_headset_patch(patch: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(Value::String(name)) = patch.get("name") {
        let name: String = name.trim().chars().take(40).collect();
        if !name.is_empty() {
            out.insert("name".into(), Value::String(name));
        }
    }
    if let Some(Value::String(c)) = patch.get("color")
        && c.len() == 7
        && c.starts_with('#')
        && c[1..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        out.insert("color".into(), Value::String(c.clone()));
    }
    if let Some(Value::String(t)) = patch.get("transport")
        && ["auto", "usb", "wifi"].contains(&t.as_str())
    {
        out.insert("transport".into(), Value::String(t.clone()));
    }
    if let Some(Value::String(m)) = patch.get("viewMode")
        && (1..=20).contains(&m.len())
        && m.bytes().all(|b| b.is_ascii_lowercase())
    {
        out.insert("viewMode".into(), Value::String(m.clone()));
    }
    if let Some(Value::Object(cal)) = patch.get("calibration") {
        let mut clean = Map::new();
        for (k, v) in cal.iter().take(40) {
            let key_ok = (1..=32).contains(&k.len())
                && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            let val_ok = match v {
                Value::Number(_) | Value::Bool(_) | Value::Null => true,
                Value::String(s) => s.chars().count() <= 40,
                _ => false,
            };
            if key_ok && val_ok {
                clean.insert(k.clone(), v.clone());
            }
        }
        out.insert("calibration".into(), Value::Object(clean));
    }
    out
}

/// Background loops started by [`DeviceManager::start`]; aborted on drop.
#[derive(Debug)]
pub struct BackgroundTasks(Vec<JoinHandle<()>>);

impl BackgroundTasks {
    pub fn stop(self) {}
}

impl Drop for BackgroundTasks {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

struct Inner<H, N> {
    host: H,
    network: N,
    store: SharedStore,
    events: EventSender,
    config: DeviceManagerConfig,
    state: Mutex<State>,
    poll_lock: tokio::sync::Mutex<()>,
}

/// Tracks connected headsets. Cheap to clone (shared handle).
pub struct DeviceManager<H: AdbBackend, N: Network = LanNetwork> {
    inner: Arc<Inner<H, N>>,
}

impl<H: AdbBackend, N: Network> Clone for DeviceManager<H, N> {
    fn clone(&self) -> Self {
        DeviceManager {
            inner: self.inner.clone(),
        }
    }
}

fn timed_out(_: tokio::time::error::Elapsed) -> CoreError {
    CoreError::Adb(AdbError::Timeout)
}

impl<H: AdbBackend, N: Network> DeviceManager<H, N> {
    pub fn new(
        host: H,
        network: N,
        store: SharedStore,
        events: EventSender,
        config: DeviceManagerConfig,
    ) -> Self {
        DeviceManager {
            inner: Arc::new(Inner {
                host,
                network,
                store,
                events,
                config,
                state: Mutex::new(State::default()),
                poll_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }

    pub fn host(&self) -> &H {
        &self.inner.host
    }

    pub fn store(&self) -> &SharedStore {
        &self.inner.store
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        let _ = self
            .inner
            .events
            .send(CoreEvent::Notice(Notice::new(level, message)));
    }

    /// Start polling, info refresh and auto-reconnect in the background.
    pub fn start(&self) -> BackgroundTasks {
        let cfg = self.inner.config;
        let (a, b, c) = (self.clone(), self.clone(), self.clone());
        BackgroundTasks(vec![
            spawn_every(cfg.poll_interval, move || {
                let dm = a.clone();
                async move { dm.poll().await }
            }),
            spawn_every(cfg.info_refresh_interval, move || {
                let dm = b.clone();
                async move { dm.refresh_all_info().await }
            }),
            spawn_every(cfg.reconnect_interval, move || {
                let dm = c.clone();
                async move { dm.auto_reconnect().await }
            }),
        ])
    }

    // ---------------------------------------------------------------- polling

    /// Refresh the transport list once: identify new devices, track pending
    /// ones and detach those that disappeared.
    pub async fn poll(&self) {
        let _guard = self.inner.poll_lock.lock().await;
        let result = self.inner.host.transports().await;
        let mut notices = Vec::new();
        let mut to_identify = Vec::new();
        {
            let mut st = self.state();
            match result {
                Ok(list) => {
                    st.adb_error = None;
                    let mut seen = HashSet::new();
                    for d in list {
                        seen.insert(d.serial.clone());
                        if d.state == "device" {
                            st.pending.remove(&d.serial);
                            if !st.transports.contains_key(&d.serial)
                                && !st.identifying.contains(&d.serial)
                            {
                                st.identifying.insert(d.serial.clone());
                                st.pending.insert(
                                    d.serial.clone(),
                                    PendingTransport {
                                        state: "identifying".into(),
                                        network: d.network,
                                        props: d.props.clone(),
                                        error: None,
                                    },
                                );
                                to_identify.push(d);
                            }
                        } else {
                            if st.transports.contains_key(&d.serial) {
                                self.detach_transport(&mut st, &d.serial, &mut notices);
                            }
                            if !st.pending.contains_key(&d.serial) && d.state == "unauthorized" {
                                let who = if d.prop("model").is_empty() {
                                    d.serial.as_str()
                                } else {
                                    d.prop("model")
                                };
                                notices.push(Notice::new(
                                    NoticeLevel::Warn,
                                    format!("Headset {who} is waiting for permission — put it on and accept \"Allow USB debugging\"."),
                                ));
                            }
                            st.pending.insert(
                                d.serial.clone(),
                                PendingTransport {
                                    state: d.state.clone(),
                                    network: d.network,
                                    props: d.props.clone(),
                                    error: None,
                                },
                            );
                        }
                    }
                    let gone: Vec<String> = st
                        .transports
                        .keys()
                        .filter(|s| !seen.contains(*s))
                        .cloned()
                        .collect();
                    for s in gone {
                        self.detach_transport(&mut st, &s, &mut notices);
                    }
                    let identifying = st.identifying.clone();
                    st.pending
                        .retain(|s, _| seen.contains(s) || identifying.contains(s));
                }
                Err(e) => st.adb_error = Some(e.to_string()),
            }
        }
        for n in notices {
            let _ = self.inner.events.send(CoreEvent::Notice(n));
        }
        for d in to_identify {
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.identify(&d).await {
                    let mut st = this.state();
                    st.pending.insert(
                        d.serial.clone(),
                        PendingTransport {
                            state: "error".into(),
                            network: d.network,
                            props: d.props.clone(),
                            error: Some(e.to_string()),
                        },
                    );
                }
                this.state().identifying.remove(&d.serial);
                this.emit_change(false);
            });
        }
        self.emit_change(false);
    }

    async fn shell(
        &self,
        adb_serial: &str,
        command: &str,
        timeout: Duration,
    ) -> Result<String, CoreError> {
        let dev = self.inner.host.device(adb_serial).await?;
        let out = tokio::time::timeout(timeout, dev.shell(command))
            .await
            .map_err(timed_out)??;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    async fn identify(&self, d: &TransportEntry) -> Result<(), CoreError> {
        let out = self
            .shell(
                &d.serial,
                IDENTIFY_COMMAND,
                self.inner.config.identify_timeout,
            )
            .await?;
        let id = parse_identify(&out);
        let serial = if id.serialno.is_empty() {
            d.serial.clone()
        } else {
            id.serialno.clone()
        };
        let brand_id = detect_brand(&id.info).to_string();
        let model = friendly_model(&brand_id, &id.info);
        let host = host_of(&d.serial);
        let is_new = {
            let mut st = self.state();
            let is_new = !st.headsets.contains_key(&serial);
            let hs = st
                .headsets
                .entry(serial.clone())
                .or_insert_with(|| LiveHeadset {
                    info: DeviceInfo::default(),
                    brand: String::new(),
                    model: String::new(),
                    transports: Vec::new(),
                    battery: None,
                    ip: None,
                });
            hs.info = id.info.clone();
            hs.brand = brand_id.clone();
            hs.model = model.clone();
            match hs.transports.iter_mut().find(|(s, _)| *s == d.serial) {
                Some(t) => t.1 = d.network,
                None => hs.transports.push((d.serial.clone(), d.network)),
            }
            if let Some((ip, _)) = &host {
                hs.ip = Some(ip.clone());
            }
            st.transports.insert(d.serial.clone(), serial.clone());
            st.pending.remove(&d.serial);
            is_new
        };

        let rec = self.inner.store.headset(&serial);
        let label = rec
            .as_ref()
            .and_then(|r| r.name.clone())
            .unwrap_or_else(|| format!("{} {model}", brand_or_fallback(&brand_id).name));
        let via = if d.network { "Wi-Fi" } else { "USB" };
        if rec.is_some() {
            self.inner.store.write(|s| {
                s.modify_headset(&serial, |r| {
                    r.brand = Some(brand_id.clone());
                    r.model = Some(model.clone());
                    r.last_seen = Some(js::now_ms());
                    if let Some((ip, port)) = &host {
                        r.ip = Some(ip.clone());
                        r.port = Some(*port);
                    }
                })
            });
            if is_new || d.network {
                self.notice(NoticeLevel::Ok, format!("{label} connected via {via}."));
            }
            if self.inner.store.read(|s| s.settings().auto_cast_on_connect) {
                let _ = self
                    .inner
                    .events
                    .send(CoreEvent::AutoCast { id: serial.clone() });
            }
        } else if !d.network {
            self.notice(
                NoticeLevel::Info,
                format!("New headset on USB: {label}. Click \"Verify\" to add it."),
            );
        } else {
            self.notice(
                NoticeLevel::Warn,
                format!("Unverified headset found on the network: {label}. Connect it via USB once to verify it."),
            );
        }

        if let Err(e) = self.refresh_info(&serial).await {
            tracing::debug!("refresh info for {serial}: {e}");
        }
        self.emit_change(false);
        Ok(())
    }

    fn detach_transport(&self, st: &mut State, adb_serial: &str, notices: &mut Vec<Notice>) {
        let Some(serial) = st.transports.remove(adb_serial) else {
            return;
        };
        let Some(hs) = st.headsets.get_mut(&serial) else {
            return;
        };
        hs.transports.retain(|(s, _)| s != adb_serial);
        if hs.transports.is_empty() {
            let model = hs.model.clone();
            st.headsets.remove(&serial);
            let name = self
                .inner
                .store
                .headset(&serial)
                .and_then(|r| r.name)
                .unwrap_or(model);
            notices.push(Notice::new(
                NoticeLevel::Info,
                format!("{name} disconnected."),
            ));
        }
    }

    // ------------------------------------------------------------ device info

    fn usb_serial(&self, serial: &str) -> Option<String> {
        self.state()
            .headsets
            .get(serial)
            .and_then(|h| h.usb_serial())
            .map(String::from)
    }

    fn wifi_serial(&self, serial: &str) -> Option<String> {
        self.state()
            .headsets
            .get(serial)
            .and_then(|h| h.wifi_serial())
            .map(String::from)
    }

    /// Pick the adb transport for a headset. `Auto` prefers USB (lower latency, no Wi-Fi load).
    pub fn pick_transport(
        &self,
        serial: &str,
        preference: TransportPreference,
    ) -> Option<Transport> {
        self.state()
            .headsets
            .get(serial)
            .and_then(|h| pick(h, preference))
    }

    async fn fetch_ip(&self, adb_serial: &str) -> Result<Option<String>, CoreError> {
        Ok(parse_ip(
            &self
                .shell(adb_serial, IP_COMMAND, self.inner.config.shell_timeout)
                .await?,
        ))
    }

    /// Refresh battery and Wi-Fi address of a connected headset.
    pub async fn refresh_info(&self, serial: &str) -> Result<(), CoreError> {
        let Some(t) = self.pick_transport(serial, TransportPreference::Auto) else {
            return Ok(());
        };
        let out = self
            .shell(
                &t.adb_serial,
                BATTERY_COMMAND,
                self.inner.config.shell_timeout,
            )
            .await?;
        if let Some(b) = parse_battery(&out)
            && let Some(hs) = self.state().headsets.get_mut(serial)
        {
            hs.battery = Some(b);
        }
        if host_of(&t.adb_serial).is_none()
            && let Ok(Some(ip)) = self.fetch_ip(&t.adb_serial).await
        {
            if let Some(hs) = self.state().headsets.get_mut(serial) {
                hs.ip = Some(ip.clone());
            }
            self.inner
                .store
                .write(|s| s.modify_existing_headset(serial, |r| r.ip = Some(ip)));
        }
        Ok(())
    }

    /// Refresh every connected headset's info, then publish the snapshot.
    pub async fn refresh_all_info(&self) {
        let serials: Vec<String> = self.state().headsets.keys().cloned().collect();
        let mut tasks = JoinSet::new();
        for s in serials {
            let this = self.clone();
            tasks.spawn(async move {
                let _ = this.refresh_info(&s).await;
            });
        }
        while tasks.join_next().await.is_some() {}
        self.emit_change(false);
    }

    // ---------------------------------------------------------- user actions

    /// Verify (trust) a headset over USB and store it.
    pub async fn verify(
        &self,
        serial: &str,
        opts: VerifyOptions,
    ) -> Result<HeadsetRecord, CoreError> {
        let (usb, brand_id, model, info, ip) = {
            let st = self.state();
            let hs = st.headsets.get(serial).ok_or(CoreError::NotConnected)?;
            let usb = hs.usb_serial().ok_or(CoreError::UsbRequired)?.to_string();
            (
                usb,
                hs.brand.clone(),
                hs.model.clone(),
                hs.info.clone(),
                hs.ip.clone(),
            )
        };
        let echo = self
            .shell(&usb, VERIFY_COMMAND, self.inner.config.shell_timeout)
            .await?;
        if !echo.contains(VERIFY_TOKEN) {
            return Err(CoreError::NoResponse);
        }
        let now = js::now_ms();
        let rec = self.inner.store.write(|s| {
            let index = s.settings().next_index.filter(|&i| i > 0).unwrap_or(1);
            s.modify_settings(|st| st.next_index = Some(index + 1));
            let used: HashSet<String> = s.headsets().filter_map(|h| h.color.clone()).collect();
            let auto_color = HEADSET_COLORS
                .iter()
                .find(|c| !used.contains(**c))
                .copied()
                .unwrap_or(HEADSET_COLORS[index as usize % HEADSET_COLORS.len()]);
            let name = opts
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(String::from)
                .unwrap_or_else(|| {
                    format!("{} {model} #{index}", brand_or_fallback(&brand_id).name)
                });
            s.modify_headset(serial, |r| {
                r.name = Some(name);
                r.color = Some(
                    opts.color
                        .clone()
                        .filter(|c| !c.is_empty())
                        .unwrap_or_else(|| auto_color.to_string()),
                );
                r.index = Some(index);
                r.brand = Some(brand_id.clone());
                r.model = Some(model.clone());
                r.model_raw = Some(info.model.clone());
                r.manufacturer = Some(info.manufacturer.clone());
                r.verified_at = Some(now);
                r.last_seen = Some(now);
                r.ip = ip.clone();
                r.port = Some(DEFAULT_ADB_TCP_PORT);
                r.clone()
            })
        });
        self.notice(
            NoticeLevel::Ok,
            format!("Verified {}.", rec.name.as_deref().unwrap_or(serial)),
        );
        self.emit_change(true);
        if opts.enable_wireless
            && let Err(e) = self.enable_wireless(serial).await
        {
            self.notice(
                NoticeLevel::Warn,
                format!("Verified, but Wi-Fi casting could not be enabled: {e}"),
            );
        }
        Ok(self.inner.store.headset(serial).unwrap_or(rec))
    }

    /// Switch a USB-connected headset to adb over TCP and connect to it.
    pub async fn enable_wireless(&self, serial: &str) -> Result<(String, u16), CoreError> {
        let usb = self
            .usb_serial(serial)
            .ok_or_else(|| CoreError::Other("Connect the headset via USB first.".into()))?;
        let known_ip = self.state().headsets.get(serial).and_then(|h| h.ip.clone());
        let ip = self
            .fetch_ip(&usb)
            .await
            .ok()
            .flatten()
            .or(known_ip)
            .ok_or_else(|| {
                CoreError::Other("The headset does not seem to be connected to Wi-Fi.".into())
            })?;
        let port = DEFAULT_ADB_TCP_PORT;
        let cfg = self.inner.config;
        let dev = self.inner.host.device(&usb).await?;
        let mut stream = tokio::time::timeout(
            Duration::from_secs(10),
            dev.open(&device_info::tcpip_service(port)),
        )
        .await
        .map_err(timed_out)??;
        // adbd answers "restarting in TCP mode port: 5555" and drops the USB connection.
        let mut reply = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut reply)).await;
        tracing::debug!("tcpip: {}", String::from_utf8_lossy(&reply).trim());
        drop(stream);

        let address = format!("{ip}:{port}");
        let mut last_err = None;
        for attempt in 0..cfg.wireless_attempts.max(1) {
            tokio::time::sleep(if attempt == 0 {
                cfg.wireless_first_delay
            } else {
                cfg.wireless_retry_delay
            })
            .await;
            match self
                .inner
                .host
                .connect(&address, Duration::from_secs(8))
                .await
            {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(e) = last_err {
            return Err(CoreError::Other(format!(
                "Could not reach {ip}:{port} — make sure this computer and the headset are on the same network. ({e})"
            )));
        }
        self.inner.store.write(|s| {
            s.modify_existing_headset(serial, |r| {
                r.ip = Some(ip.clone());
                r.port = Some(port);
                r.wireless = Some(true);
            })
        });
        self.notice(
            NoticeLevel::Ok,
            format!("Wi-Fi casting enabled at {ip}. You can unplug the USB cable."),
        );
        self.poll().await;
        Ok((ip, port))
    }

    /// Drop the Wi-Fi transport and stop auto-reconnecting it.
    pub async fn disconnect_wireless(&self, serial: &str) {
        if let Some(wifi) = self.wifi_serial(serial) {
            let _ = self.inner.host.disconnect(&wifi).await;
        }
        self.inner
            .store
            .write(|s| s.modify_existing_headset(serial, |r| r.wireless = Some(false)));
        self.poll().await;
    }

    /// Connect to a user-entered address (`192.168.1.50[:5555]`).
    pub async fn connect_address(&self, address: &str) -> Result<(), CoreError> {
        let (host, port) = device_info::parse_address(address)?;
        self.inner
            .host
            .connect(&format!("{host}:{port}"), Duration::from_secs(8))
            .await?;
        self.poll().await;
        Ok(())
    }

    /// Remove a verified headset (and drop its Wi-Fi transport).
    pub async fn forget(&self, serial: &str) {
        if let Some(wifi) = self.wifi_serial(serial) {
            let _ = self.inner.host.disconnect(&wifi).await;
        }
        self.inner.store.write(|s| s.remove_headset(serial));
        self.poll().await;
        self.emit_change(true);
    }

    /// Change the user-editable fields of a verified headset (see [`EDITABLE_HEADSET_FIELDS`]).
    pub fn update(
        &self,
        serial: &str,
        patch: &Map<String, Value>,
    ) -> Result<HeadsetRecord, CoreError> {
        let allowed: Map<String, Value> = patch
            .iter()
            .filter(|(k, _)| EDITABLE_HEADSET_FIELDS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let rec = self.inner.store.write(|s| {
            s.headset(serial)?;
            Some(s.upsert_headset(serial, &allowed).clone())
        });
        let rec = rec.ok_or(CoreError::NotTrusted)?;
        self.emit_change(true);
        Ok(rec)
    }

    fn connected_wifi_ips(&self) -> HashSet<String> {
        let st = self.state();
        st.transports
            .keys()
            .chain(st.pending.keys())
            .filter_map(|s| host_of(s))
            .map(|(ip, _)| ip)
            .collect()
    }

    /// Reconnect verified headsets whose last Wi-Fi address answers on the adb port.
    pub async fn auto_reconnect(&self) {
        if !self.inner.store.read(|s| s.settings().auto_reconnect_wifi) {
            return;
        }
        let connected = self.connected_wifi_ips();
        let targets: Vec<(String, u16)> = self
            .inner
            .store
            .headsets()
            .into_iter()
            .filter(|r| r.wireless != Some(false) && self.wifi_serial(&r.serial).is_none())
            .filter_map(|r| Some((r.ip.clone()?, r.port.unwrap_or(DEFAULT_ADB_TCP_PORT))))
            .filter(|(ip, _)| !ip.is_empty() && !connected.contains(ip))
            .collect();
        if targets.is_empty() {
            return;
        }
        let mut tasks = JoinSet::new();
        for (ip, port) in targets {
            let this = self.clone();
            tasks.spawn(async move {
                let cfg = this.inner.config;
                if this
                    .inner
                    .network
                    .probe(&ip, port, cfg.reconnect_probe_timeout)
                    .await
                {
                    let _ = this
                        .inner
                        .host
                        .connect(&format!("{ip}:{port}"), cfg.connect_timeout)
                        .await;
                }
            });
        }
        while tasks.join_next().await.is_some() {}
        self.poll().await;
    }

    /// Find headsets on the LAN: saved addresses, mDNS-advertised adb, and a subnet scan of the adb port.
    pub async fn scan_network(&self) -> ScanResult {
        let port = self
            .inner
            .store
            .read(|s| s.settings().effective_scan_port());
        let before: HashSet<String> = self.state().transports.keys().cloned().collect();
        let connected = self.connected_wifi_ips();
        let mut tried = HashSet::new();
        let mut attempts = JoinSet::new();
        let mut try_connect = |addr: String| {
            if tried.insert(addr.clone()) {
                let this = self.clone();
                attempts.spawn(async move {
                    let _ = this
                        .inner
                        .host
                        .connect(&addr, this.inner.config.connect_timeout)
                        .await;
                });
            }
        };
        for r in self.inner.store.headsets() {
            if let Some(ip) = r.ip.filter(|ip| !ip.is_empty() && !connected.contains(ip)) {
                try_connect(format!("{ip}:{}", r.port.unwrap_or(DEFAULT_ADB_TCP_PORT)));
            }
        }
        let (services, open) =
            tokio::join!(self.inner.network.mdns(), self.inner.network.scan(port));
        for svc in services {
            if !connected.contains(&svc.ip) {
                try_connect(format!("{}:{}", svc.ip, svc.port));
            }
        }
        for ip in &open {
            let ip = ip.to_string();
            if !connected.contains(&ip) {
                try_connect(format!("{ip}:{port}"));
            }
        }
        while attempts.join_next().await.is_some() {}
        self.poll().await;
        // Give identification a moment to finish for fresh transports.
        for _ in 0..20 {
            if self.state().identifying.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let st = self.state();
        let fresh: Vec<&String> = st
            .transports
            .keys()
            .filter(|s| !before.contains(*s))
            .collect();
        let unverified = self.inner.store.read(|store| {
            fresh
                .iter()
                .filter(|s| store.headset(&st.transports[**s]).is_none())
                .count()
        });
        let pending_auth = st
            .pending
            .values()
            .filter(|p| p.network && p.state == "unauthorized")
            .count();
        ScanResult {
            probed: open.len(),
            connected: fresh.len(),
            unverified,
            pending_auth,
        }
    }

    // -------------------------------------------------------------- snapshot

    /// The device list for UIs.
    pub fn snapshot(&self) -> DeviceSnapshot {
        let st = self.state();
        self.inner.store.read(|store| build_snapshot(&st, store))
    }

    /// Publish the snapshot if it changed (or always, with `force`).
    pub fn emit_change(&self, force: bool) {
        let snap = {
            let mut st = self.state();
            let snap = self.inner.store.read(|store| build_snapshot(&st, store));
            if !force && st.last_snapshot.as_ref() == Some(&snap) {
                return;
            }
            st.last_snapshot = Some(snap.clone());
            snap
        };
        let _ = self.inner.events.send(CoreEvent::DevicesChanged(snap));
    }

    #[cfg(test)]
    fn identifying_count(&self) -> usize {
        self.state().identifying.len()
    }
}

impl<H: AdbBackend, N: Network> DeviceDirectory for DeviceManager<H, N> {
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
        DeviceManager::pick_transport(self, id, preference)
    }
}

/// Run `job` now and then every `period` (skipping missed ticks) until aborted.
fn spawn_every<F, Fut>(period: Duration, job: F) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(period);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            iv.tick().await;
            job().await;
        }
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::broadcast;

    pub(crate) const QUEST3_IDENTIFY: &str =
        "Oculus\noculus\nQuest 3\neureka\neureka\n2G0YC5ZG8L0123\n14\n#end\n";
    const PFD_IDENTIFY: &str = "Play For Dream\nYVR\nPFDM MR\nyvr_d3\nyvr_d3\nPFD3M0A1B2C3\n14\nfeature:android.software.xr.input.controller\n#end\n";
    const BATTERY: &str = "  AC powered: false\n  USB powered: true\n  status: 2\n  level: 77\n";
    const IP: &str = "22: wlan0    inet 192.168.1.42/24 brd 192.168.1.255 scope global wlan0\n";

    #[derive(Default)]
    pub(crate) struct HostState {
        transports: Vec<TransportEntry>,
        /// adb serial → command → output
        shells: HashMap<String, HashMap<String, String>>,
        /// Addresses that `connect` succeeds for, with the identify output of the device behind them.
        reachable: HashMap<String, String>,
        connects: Vec<String>,
        disconnects: Vec<String>,
        opened: Vec<(String, String)>,
    }

    #[derive(Clone, Default)]
    pub(crate) struct FakeHost(Arc<Mutex<HostState>>);

    #[derive(Clone)]
    pub(crate) struct FakeDevice {
        serial: String,
        host: FakeHost,
    }

    impl FakeHost {
        fn lock(&self) -> MutexGuard<'_, HostState> {
            self.0.lock().unwrap()
        }

        /// Plug in a device on `adb_serial` answering the standard commands.
        pub(crate) fn plug(&self, adb_serial: &str, identify: &str) {
            let mut st = self.lock();
            st.transports.retain(|t| t.serial != adb_serial);
            st.transports
                .push(TransportEntry::new(adb_serial, "device"));
            let shells = st.shells.entry(adb_serial.into()).or_default();
            shells.insert(IDENTIFY_COMMAND.into(), identify.into());
            shells.insert(BATTERY_COMMAND.into(), BATTERY.into());
            shells.insert(IP_COMMAND.into(), IP.into());
            shells.insert(VERIFY_COMMAND.into(), "xrmc-verified\n".into());
        }

        pub(crate) fn unplug(&self, adb_serial: &str) {
            self.lock().transports.retain(|t| t.serial != adb_serial);
        }

        fn add_pending(&self, adb_serial: &str, state: &str, model: &str) {
            self.lock()
                .transports
                .push(TransportEntry::new(adb_serial, state).with_prop("model", model));
        }
    }

    impl AdbDevice for FakeDevice {
        type Stream = tokio::io::DuplexStream;

        fn serial(&self) -> &str {
            &self.serial
        }

        async fn shell(&self, command: &str) -> xrmc_adb::Result<Vec<u8>> {
            let st = self.host.lock();
            st.shells
                .get(&self.serial)
                .and_then(|m| m.get(command))
                .map(|s| s.clone().into_bytes())
                .ok_or_else(|| AdbError::Other(format!("unexpected command {command}")))
        }

        async fn shell_stream(&self, _command: &str) -> xrmc_adb::Result<Self::Stream> {
            Err(AdbError::Other("not supported".into()))
        }

        async fn push(&self, _data: &[u8], _remote_path: &str, _mode: u32) -> xrmc_adb::Result<()> {
            Ok(())
        }

        async fn open(&self, service: &str) -> xrmc_adb::Result<Self::Stream> {
            self.host
                .lock()
                .opened
                .push((self.serial.clone(), service.to_string()));
            let (a, mut b) = tokio::io::duplex(256);
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = b.write_all(b"restarting in TCP mode port: 5555\n").await;
            });
            Ok(a)
        }
    }

    impl AdbBackend for FakeHost {
        type Device = FakeDevice;

        async fn transports(&self) -> xrmc_adb::Result<Vec<TransportEntry>> {
            Ok(self.lock().transports.clone())
        }

        async fn device(&self, adb_serial: &str) -> xrmc_adb::Result<FakeDevice> {
            Ok(FakeDevice {
                serial: adb_serial.to_string(),
                host: self.clone(),
            })
        }

        async fn connect(&self, address: &str, _timeout: Duration) -> xrmc_adb::Result<()> {
            let identify = {
                let mut st = self.lock();
                st.connects.push(address.to_string());
                st.reachable.get(address).cloned()
            };
            match identify {
                Some(id) => {
                    self.plug(address, &id);
                    Ok(())
                }
                None => Err(AdbError::Other(format!("failed to connect to {address}"))),
            }
        }

        async fn disconnect(&self, address: &str) -> xrmc_adb::Result<()> {
            let mut st = self.lock();
            st.disconnects.push(address.to_string());
            st.transports.retain(|t| t.serial != address);
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct FakeNetwork {
        open: Arc<Mutex<Vec<Ipv4Addr>>>,
        probes: Arc<Mutex<Vec<String>>>,
    }

    impl Network for FakeNetwork {
        async fn probe(&self, host: &str, port: u16, _timeout: Duration) -> bool {
            self.probes.lock().unwrap().push(format!("{host}:{port}"));
            self.open
                .lock()
                .unwrap()
                .iter()
                .any(|ip| ip.to_string() == host)
        }

        async fn scan(&self, _port: u16) -> Vec<Ipv4Addr> {
            self.open.lock().unwrap().clone()
        }

        async fn mdns(&self) -> Vec<MdnsService> {
            Vec::new()
        }
    }

    type Dm = DeviceManager<FakeHost, FakeNetwork>;

    fn manager() -> (Dm, FakeHost, FakeNetwork, broadcast::Receiver<CoreEvent>) {
        let host = FakeHost::default();
        let net = FakeNetwork::default();
        let (tx, rx) = crate::events::channel(256);
        let dm = DeviceManager::new(
            host.clone(),
            net.clone(),
            SharedStore::in_memory(),
            tx,
            DeviceManagerConfig::default(),
        );
        (dm, host, net, rx)
    }

    async fn poll_and_identify(dm: &Dm) {
        dm.poll().await;
        for _ in 0..200 {
            if dm.identifying_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn notices(rx: &mut broadcast::Receiver<CoreEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if let CoreEvent::Notice(n) = e {
                out.push(n.message);
            }
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_usb_headset_is_identified_but_cannot_cast_until_verified() {
        let (dm, host, _, mut rx) = manager();
        host.plug("2G0YC5ZG8L0123", QUEST3_IDENTIFY);
        poll_and_identify(&dm).await;
        let snap = dm.snapshot();
        assert_eq!(snap.headsets.len(), 1);
        let h = &snap.headsets[0];
        assert_eq!(
            (h.id.as_str(), h.brand.as_str(), h.model.as_str()),
            ("2G0YC5ZG8L0123", "meta", "Quest 3")
        );
        assert_eq!(h.name, "Meta Quest 3");
        assert_eq!(h.usb.as_deref(), Some("2G0YC5ZG8L0123"));
        assert_eq!(h.status, "online");
        assert!(!h.trusted && !h.can_cast);
        assert_eq!(
            h.battery,
            Some(Battery {
                level: 77,
                charging: true
            })
        );
        assert_eq!(h.ip.as_deref(), Some("192.168.1.42"));
        assert_eq!(h.color, "#1877F2");
        let n = notices(&mut rx);
        assert!(
            n.iter()
                .any(|m| m == "New headset on USB: Meta Quest 3. Click \"Verify\" to add it."),
            "{n:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn verification_stores_the_electron_fields() {
        let (dm, host, _, mut rx) = manager();
        host.plug("2G0YC5ZG8L0123", QUEST3_IDENTIFY);
        poll_and_identify(&dm).await;
        let rec = dm
            .verify("2G0YC5ZG8L0123", VerifyOptions::default())
            .await
            .unwrap();
        assert_eq!(rec.name.as_deref(), Some("Meta Quest 3 #1"));
        assert_eq!(rec.color.as_deref(), Some(HEADSET_COLORS[0]));
        assert_eq!(rec.index, Some(1));
        assert_eq!(rec.brand.as_deref(), Some("meta"));
        assert_eq!(rec.model_raw.as_deref(), Some("Quest 3"));
        assert_eq!(rec.manufacturer.as_deref(), Some("Oculus"));
        assert_eq!(rec.ip.as_deref(), Some("192.168.1.42"));
        assert_eq!(rec.port, Some(5555));
        assert!(rec.verified_at.is_some() && rec.last_seen.is_some());
        assert_eq!(dm.store().settings().next_index, Some(2));
        let h = dm.headset("2G0YC5ZG8L0123").unwrap();
        assert!(h.trusted && h.can_cast);
        assert_eq!(h.index, Some(1));
        assert!(notices(&mut rx).contains(&"Verified Meta Quest 3 #1.".to_string()));

        // A second headset gets the next number and colour, and a custom name.
        host.plug("PFD3M0A1B2C3", PFD_IDENTIFY);
        poll_and_identify(&dm).await;
        let rec2 = dm
            .verify(
                "PFD3M0A1B2C3",
                VerifyOptions {
                    name: Some("  Lab PFD ".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(rec2.name.as_deref(), Some("Lab PFD"));
        assert_eq!(rec2.color.as_deref(), Some(HEADSET_COLORS[1]));
        assert_eq!(rec2.index, Some(2));
        let snap = dm.snapshot();
        assert_eq!(
            snap.headsets.iter().map(|h| h.index).collect::<Vec<_>>(),
            [Some(1), Some(2)]
        );
        let pfd = dm.headset("PFD3M0A1B2C3").unwrap();
        assert_eq!(pfd.variant.as_deref(), Some("pfd-mr"));
        assert_eq!(pfd.model, "MR");
    }

    #[tokio::test(start_paused = true)]
    async fn verification_needs_usb() {
        let (dm, host, _, _rx) = manager();
        assert!(matches!(
            dm.verify("nope", VerifyOptions::default()).await,
            Err(CoreError::NotConnected)
        ));
        host.lock()
            .reachable
            .insert("192.168.1.42:5555".into(), QUEST3_IDENTIFY.into());
        dm.connect_address("192.168.1.42").await.unwrap();
        poll_and_identify(&dm).await;
        let h = dm.headset("2G0YC5ZG8L0123").unwrap();
        assert_eq!(h.wifi.as_deref(), Some("192.168.1.42:5555"));
        let err = dm
            .verify("2G0YC5ZG8L0123", VerifyOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::UsbRequired));
        assert_eq!(
            err.to_string(),
            "Verification requires a USB connection. Plug the headset in with a USB data cable."
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unauthorized_transports_are_pending_with_a_notice() {
        let (dm, host, _, mut rx) = manager();
        host.add_pending("PFD3M0A1B2C3", "unauthorized", "PFDM_MR");
        dm.poll().await;
        let snap = dm.snapshot();
        let p = &snap.headsets[0];
        assert!(p.pending);
        assert_eq!(p.id, "pending:PFD3M0A1B2C3");
        assert_eq!(p.status, "unauthorized");
        assert_eq!(p.name, "PFDM MR");
        assert_eq!(p.brand, "yvr");
        assert!(!p.can_cast);
        let n = notices(&mut rx);
        assert_eq!(
            n,
            [
                "Headset PFDM_MR is waiting for permission — put it on and accept \"Allow USB debugging\"."
            ]
        );
        dm.poll().await;
        assert!(notices(&mut rx).is_empty(), "the notice is shown once");
        let json = serde_json::to_value(p).unwrap();
        assert_eq!(json["adbSerial"], json!("PFD3M0A1B2C3"));
        assert_eq!(json["pending"], json!(true));
    }

    #[tokio::test(start_paused = true)]
    async fn unplugging_detaches_and_offline_verified_headsets_stay_listed() {
        let (dm, host, _, mut rx) = manager();
        host.plug("2G0YC5ZG8L0123", QUEST3_IDENTIFY);
        poll_and_identify(&dm).await;
        dm.verify("2G0YC5ZG8L0123", VerifyOptions::default())
            .await
            .unwrap();
        notices(&mut rx);
        host.unplug("2G0YC5ZG8L0123");
        dm.poll().await;
        let h = dm.headset("2G0YC5ZG8L0123").unwrap();
        assert_eq!(h.status, "offline");
        assert!(!h.can_cast && h.trusted);
        assert_eq!(h.battery, None);
        assert_eq!(
            h.ip.as_deref(),
            Some("192.168.1.42"),
            "last known address from the store"
        );
        assert_eq!(notices(&mut rx), ["Meta Quest 3 #1 disconnected."]);
        assert!(
            dm.pick_transport("2G0YC5ZG8L0123", TransportPreference::Auto)
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn enable_wireless_switches_to_tcp_and_connects() {
        let (dm, host, _, mut rx) = manager();
        host.plug("2G0YC5ZG8L0123", QUEST3_IDENTIFY);
        host.lock()
            .reachable
            .insert("192.168.1.42:5555".into(), QUEST3_IDENTIFY.into());
        poll_and_identify(&dm).await;
        dm.verify(
            "2G0YC5ZG8L0123",
            VerifyOptions {
                enable_wireless: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        poll_and_identify(&dm).await;
        assert_eq!(
            host.lock().opened,
            [("2G0YC5ZG8L0123".to_string(), "tcpip:5555".to_string())]
        );
        assert_eq!(host.lock().connects, ["192.168.1.42:5555"]);
        let rec = dm.store().headset("2G0YC5ZG8L0123").unwrap();
        assert_eq!(rec.wireless, Some(true));
        let h = dm.headset("2G0YC5ZG8L0123").unwrap();
        assert_eq!(h.usb.as_deref(), Some("2G0YC5ZG8L0123"));
        assert_eq!(h.wifi.as_deref(), Some("192.168.1.42:5555"));
        assert_eq!(
            dm.pick_transport("2G0YC5ZG8L0123", TransportPreference::Auto)
                .unwrap()
                .kind,
            TransportKind::Usb
        );
        assert_eq!(
            dm.pick_transport("2G0YC5ZG8L0123", TransportPreference::Wifi)
                .unwrap()
                .adb_serial,
            "192.168.1.42:5555"
        );
        let n = notices(&mut rx);
        assert!(
            n.contains(
                &"Wi-Fi casting enabled at 192.168.1.42. You can unplug the USB cable.".to_string()
            ),
            "{n:?}"
        );
        assert!(
            n.contains(&"Meta Quest 3 #1 connected via Wi-Fi.".to_string()),
            "{n:?}"
        );

        dm.disconnect_wireless("2G0YC5ZG8L0123").await;
        assert_eq!(
            dm.store().headset("2G0YC5ZG8L0123").unwrap().wireless,
            Some(false)
        );
        assert!(dm.headset("2G0YC5ZG8L0123").unwrap().wifi.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn enable_wireless_reports_unreachable_headsets() {
        let (dm, host, _, _rx) = manager();
        host.plug("2G0YC5ZG8L0123", QUEST3_IDENTIFY);
        poll_and_identify(&dm).await;
        let err = dm
            .enable_wireless("2G0YC5ZG8L0123")
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Could not reach 192.168.1.42:5555 — make sure"),
            "{err}"
        );
        assert_eq!(host.lock().connects.len(), 6);
    }

    #[tokio::test(start_paused = true)]
    async fn auto_reconnect_probes_saved_addresses() {
        let (dm, host, net, _rx) = manager();
        dm.store().write(|s| {
            s.upsert_headset(
                "2G0YC5ZG8L0123",
                json!({ "name": "Q", "ip": "192.168.1.42", "port": 5555 })
                    .as_object()
                    .unwrap(),
            );
            s.upsert_headset(
                "OFF",
                json!({ "name": "Off", "ip": "192.168.1.50", "wireless": false })
                    .as_object()
                    .unwrap(),
            );
            s.upsert_headset(
                "AWAY",
                json!({ "name": "Away", "ip": "192.168.1.60" })
                    .as_object()
                    .unwrap(),
            );
        });
        host.lock()
            .reachable
            .insert("192.168.1.42:5555".into(), QUEST3_IDENTIFY.into());
        net.open
            .lock()
            .unwrap()
            .push(Ipv4Addr::new(192, 168, 1, 42));
        dm.auto_reconnect().await;
        poll_and_identify(&dm).await;
        let mut probes = net.probes.lock().unwrap().clone();
        probes.sort();
        assert_eq!(
            probes,
            ["192.168.1.42:5555", "192.168.1.60:5555"],
            "wireless: false is skipped"
        );
        assert_eq!(
            host.lock().connects,
            ["192.168.1.42:5555"],
            "only reachable hosts are connected"
        );
        let h = dm.headset("2G0YC5ZG8L0123").unwrap();
        assert!(h.can_cast);
        assert_eq!(h.wifi.as_deref(), Some("192.168.1.42:5555"));
        // Already connected: no new probe.
        net.probes.lock().unwrap().clear();
        dm.auto_reconnect().await;
        assert_eq!(net.probes.lock().unwrap().clone(), ["192.168.1.60:5555"]);
    }

    #[tokio::test(start_paused = true)]
    async fn scan_connects_to_open_hosts() {
        let (dm, host, net, _rx) = manager();
        host.lock()
            .reachable
            .insert("192.168.1.42:5555".into(), QUEST3_IDENTIFY.into());
        net.open.lock().unwrap().extend([
            Ipv4Addr::new(192, 168, 1, 42),
            Ipv4Addr::new(192, 168, 1, 77),
        ]);
        let r = dm.scan_network().await;
        assert_eq!(
            r,
            ScanResult {
                probed: 2,
                connected: 1,
                unverified: 1,
                pending_auth: 0
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn update_only_touches_editable_fields_of_verified_headsets() {
        let (dm, _, _, _rx) = manager();
        assert!(matches!(
            dm.update("X", &Map::new()),
            Err(CoreError::NotTrusted)
        ));
        dm.store().write(|s| {
            s.upsert_headset("X", json!({ "name": "X", "index": 3 }).as_object().unwrap())
                .clone()
        });
        let rec = dm
            .update("X", json!({ "name": "New", "index": 9, "calibration": { "k1": -0.3 }, "transport": "wifi" }).as_object().unwrap())
            .unwrap();
        assert_eq!(rec.name.as_deref(), Some("New"));
        assert_eq!(rec.index, Some(3));
        assert_eq!(rec.transport, Some(TransportPreference::Wifi));
        assert_eq!(rec.calibration_value("k1"), Some(&json!(-0.3)));
        dm.forget("X").await;
        assert!(dm.store().headset("X").is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn auto_cast_event_for_verified_headsets() {
        let (dm, host, _, mut rx) = manager();
        dm.store().write(|s| {
            s.upsert_headset(
                "2G0YC5ZG8L0123",
                json!({ "name": "Q" }).as_object().unwrap(),
            );
            s.modify_settings(|st| st.auto_cast_on_connect = true);
        });
        host.plug("2G0YC5ZG8L0123", QUEST3_IDENTIFY);
        poll_and_identify(&dm).await;
        let mut auto = false;
        while let Ok(e) = rx.try_recv() {
            auto |= matches!(e, CoreEvent::AutoCast { ref id } if id == "2G0YC5ZG8L0123");
        }
        assert!(auto);
        assert!(
            dm.store()
                .headset("2G0YC5ZG8L0123")
                .unwrap()
                .last_seen
                .is_some()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn identify_failures_show_as_pending_errors_and_adb_errors_are_reported() {
        let (dm, host, _, _rx) = manager();
        host.lock()
            .transports
            .push(TransportEntry::new("BROKEN", "device"));
        poll_and_identify(&dm).await;
        let snap = dm.snapshot();
        assert_eq!(snap.headsets[0].status, "error");
        assert!(
            snap.headsets[0]
                .error
                .as_deref()
                .unwrap()
                .contains("unexpected command")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn snapshots_are_only_published_when_they_change() {
        let (dm, host, _, mut rx) = manager();
        host.plug("2G0YC5ZG8L0123", QUEST3_IDENTIFY);
        poll_and_identify(&dm).await;
        while rx.try_recv().is_ok() {}
        dm.poll().await;
        assert!(rx.try_recv().is_err(), "nothing changed");
        dm.emit_change(true);
        assert!(matches!(rx.try_recv(), Ok(CoreEvent::DevicesChanged(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn background_tasks_poll_until_dropped() {
        let (dm, host, _, _rx) = manager();
        let tasks = dm.start();
        host.plug("2G0YC5ZG8L0123", QUEST3_IDENTIFY);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(dm.headset("2G0YC5ZG8L0123").is_some());
        drop(tasks);
    }

    #[test]
    fn remote_patches_are_validated() {
        let p = remote_headset_patch(
            json!({
                "name": "  A very long name that keeps going and going and going  ",
                "color": "#12abZZ",
                "transport": "usb",
                "viewMode": "Raw!",
                "calibration": { "k1": -0.2, "bad key": 1, "obj": {}, "eye": "left" },
                "index": 5
            })
            .as_object()
            .unwrap(),
        );
        assert_eq!(p["name"].as_str().unwrap().chars().count(), 40);
        assert!(p.get("color").is_none());
        assert_eq!(p["transport"], json!("usb"));
        assert!(p.get("viewMode").is_none());
        assert_eq!(p["calibration"], json!({ "k1": -0.2, "eye": "left" }));
        assert!(p.get("index").is_none());
    }

    #[test]
    fn snapshot_sorting() {
        let mut store = Store::in_memory(Default::default());
        store.upsert_headset(
            "B",
            json!({ "name": "Bravo", "index": 2 }).as_object().unwrap(),
        );
        store.upsert_headset(
            "A",
            json!({ "name": "alpha", "index": 1 }).as_object().unwrap(),
        );
        store.upsert_headset(
            "C",
            json!({ "name": "Charlie", "index": 1 })
                .as_object()
                .unwrap(),
        );
        let mut st = State::default();
        st.headsets.insert(
            "B".into(),
            LiveHeadset {
                info: DeviceInfo::default(),
                brand: "meta".into(),
                model: "Quest 3".into(),
                transports: vec![("B".into(), false)],
                battery: None,
                ip: None,
            },
        );
        st.pending.insert(
            "P".into(),
            PendingTransport {
                state: "offline".into(),
                network: false,
                props: BTreeMap::new(),
                error: None,
            },
        );
        let snap = build_snapshot(&st, &store);
        let ids: Vec<_> = snap.headsets.iter().map(|h| h.id.as_str()).collect();
        // pending, online trusted, then offline by index and name
        assert_eq!(ids, ["pending:P", "B", "A", "C"]);
        assert_eq!(snap.headsets[3].brand, "android");
        assert_eq!(snap.headsets[2].status, "offline");
    }
}
