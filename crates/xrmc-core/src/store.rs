//! JSON store for verified ("trusted") headsets and app settings (port of
//! `src/main/store.js`).
//!
//! The file format is the Electron app's `xr-multicast.json`:
//! `{ "headsets": { "<serial>": { ... } }, "settings": { ... }, ... }`, so users
//! can switch between the apps without losing verified headsets. Unknown
//! fields, at every level, are preserved; writes are atomic (temp file + rename).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Notify;

use crate::codec_policy::EncoderInfo;
use crate::config::TransportPreference;
use crate::settings::{Settings, lenient_merge};

/// File name of the store, shared with the Electron app.
pub const STORE_FILE_NAME: &str = "xr-multicast.json";
/// Electron's per-user data folder name (the app's product name).
pub const APP_DIR_NAME: &str = "XR MultiCast";
/// Folder and file of the app's previous name ("XR View").
pub const LEGACY_APP_DIR_NAME: &str = "XR View";
pub const LEGACY_STORE_FILE_NAME: &str = "xrview.json";

/// A verified headset as stored on disk.
///
/// Field names match the Electron store. Every field but `serial` is optional;
/// unknown fields are kept in `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadsetRecord {
    /// Hardware serial (`ro.serialno`); also the key in `headsets`.
    #[serde(default)]
    pub serial: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The #number shown in the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    /// Brand id (see [`crate::brands`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    /// Friendly model name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `ro.product.model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    /// ms since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<u64>,
    /// ms since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
    /// Last known Wi-Fi address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Wi-Fi casting enabled (`false` after the user disconnected Wi-Fi: don't auto-reconnect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wireless: Option<bool>,
    /// Lens calibration saved from the UI (free-form; see [`crate::brands::Correction`] for the known keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<Map<String, Value>>,
    /// Preferred casting transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TransportPreference>,
    /// Preferred view mode for this headset's tile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_mode: Option<String>,
    /// Last known encoder list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoders: Option<Vec<EncoderInfo>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl HeadsetRecord {
    pub fn new(serial: impl Into<String>) -> Self {
        HeadsetRecord {
            serial: serial.into(),
            ..Default::default()
        }
    }

    /// A record from stored JSON; wrongly typed fields are dropped instead of failing.
    pub fn from_json(serial: &str, stored: &Map<String, Value>) -> (HeadsetRecord, Vec<String>) {
        let (mut rec, rejected) = lenient_merge(&HeadsetRecord::new(serial), stored);
        rec.serial = serial.to_string();
        (rec, rejected)
    }

    /// Calibration value by key, e.g. `eye`.
    pub fn calibration_value(&self, key: &str) -> Option<&Value> {
        self.calibration.as_ref()?.get(key)
    }
}

/// Everything in the store file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreData {
    pub headsets: BTreeMap<String, HeadsetRecord>,
    pub settings: Settings,
    /// Other top-level fields (e.g. the remote-access pairing data `remote`).
    pub extra: Map<String, Value>,
}

impl StoreData {
    /// Parse a store file leniently: missing or malformed parts fall back to defaults.
    pub fn from_json(v: &Value) -> StoreData {
        let mut data = StoreData::default();
        let Some(root) = v.as_object() else {
            return data;
        };
        if let Some(Value::Object(hs)) = root.get("headsets") {
            for (serial, rec) in hs {
                let Some(obj) = rec.as_object() else {
                    tracing::warn!("store: ignoring malformed headset entry {serial:?}");
                    continue;
                };
                let (rec, rejected) = HeadsetRecord::from_json(serial, obj);
                if !rejected.is_empty() {
                    tracing::warn!("store: headset {serial}: ignoring invalid fields {rejected:?}");
                }
                data.headsets.insert(serial.clone(), rec);
            }
        }
        if let Some(Value::Object(s)) = root.get("settings") {
            let (settings, rejected) = Settings::from_json(s);
            if !rejected.is_empty() {
                tracing::warn!("store: ignoring invalid settings {rejected:?}");
            }
            data.settings = settings;
        }
        for (k, v) in root {
            if k != "headsets" && k != "settings" {
                data.extra.insert(k.clone(), v.clone());
            }
        }
        data
    }

    pub fn to_json(&self) -> Value {
        let mut root = Map::new();
        let headsets: Map<String, Value> = self
            .headsets
            .iter()
            .map(|(k, r)| (k.clone(), serde_json::to_value(r).unwrap_or(Value::Null)))
            .collect();
        root.insert("headsets".into(), Value::Object(headsets));
        root.insert("settings".into(), Value::Object(self.settings.to_json()));
        for (k, v) in &self.extra {
            root.insert(k.clone(), v.clone());
        }
        Value::Object(root)
    }
}

/// The settings/headset store, backed by a JSON file (or memory only).
#[derive(Debug)]
pub struct Store {
    path: Option<PathBuf>,
    data: StoreData,
    dirty: bool,
}

impl Store {
    /// Open the store at `path`. A missing file means first run; an unreadable
    /// or corrupt file is copied to `<file>.invalid` and the store starts empty.
    pub fn open(path: impl Into<PathBuf>) -> Store {
        let path = path.into();
        let data = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(v) => StoreData::from_json(&v),
                Err(e) => {
                    let backup = sibling(&path, ".invalid");
                    tracing::warn!(
                        "store: {} is not valid JSON ({e}); keeping a copy at {}",
                        path.display(),
                        backup.display()
                    );
                    let _ = std::fs::write(&backup, &bytes);
                    StoreData::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreData::default(),
            Err(e) => {
                tracing::warn!("store: cannot read {}: {e}", path.display());
                StoreData::default()
            }
        };
        Store {
            path: Some(path),
            data,
            dirty: false,
        }
    }

    /// Like [`Store::open`], but first copies `legacy` to `path` when only the legacy file exists
    /// (the Electron app does this for the previous app name).
    pub fn open_with_migration(path: impl Into<PathBuf>, legacy: Option<&Path>) -> Store {
        let path = path.into();
        if let Some(legacy) = legacy
            && !path.exists()
            && legacy.exists()
        {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::copy(legacy, &path) {
                tracing::warn!("store: could not migrate {}: {e}", legacy.display());
            }
        }
        Store::open(path)
    }

    /// A store that is never written to disk (tests, demos).
    pub fn in_memory(data: StoreData) -> Store {
        Store {
            path: None,
            data,
            dirty: false,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn data(&self) -> &StoreData {
        &self.data
    }

    /// Has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn settings(&self) -> &Settings {
        &self.data.settings
    }

    /// Merge a partial settings object; returns the keys that were rejected.
    pub fn update_settings(&mut self, patch: &Map<String, Value>) -> Vec<String> {
        let rejected = self.data.settings.apply_patch(patch);
        self.dirty = true;
        rejected
    }

    /// Change settings in place.
    pub fn modify_settings<R>(&mut self, f: impl FnOnce(&mut Settings) -> R) -> R {
        self.dirty = true;
        f(&mut self.data.settings)
    }

    pub fn headset(&self, serial: &str) -> Option<&HeadsetRecord> {
        self.data.headsets.get(serial)
    }

    pub fn headsets(&self) -> impl Iterator<Item = &HeadsetRecord> {
        self.data.headsets.values()
    }

    /// Merge `patch` (camelCase JSON fields) into a headset record, creating it if needed
    /// (`{ ...current, ...patch, serial }`). `null` clears a field.
    pub fn upsert_headset(&mut self, serial: &str, patch: &Map<String, Value>) -> &HeadsetRecord {
        let current = self
            .data
            .headsets
            .get(serial)
            .cloned()
            .unwrap_or_else(|| HeadsetRecord::new(serial));
        let (mut rec, rejected) = lenient_merge(&current, patch);
        if !rejected.is_empty() {
            tracing::warn!("store: headset {serial}: ignoring invalid fields {rejected:?}");
        }
        rec.serial = serial.to_string();
        self.dirty = true;
        self.data.headsets.insert(serial.to_string(), rec);
        &self.data.headsets[serial]
    }

    /// Change a headset record in place, creating it if needed.
    pub fn modify_headset<R>(
        &mut self,
        serial: &str,
        f: impl FnOnce(&mut HeadsetRecord) -> R,
    ) -> R {
        self.dirty = true;
        let rec = self
            .data
            .headsets
            .entry(serial.to_string())
            .or_insert_with(|| HeadsetRecord::new(serial));
        let r = f(rec);
        rec.serial = serial.to_string();
        r
    }

    /// Change an existing headset record; does nothing (returns `None`) for unknown serials.
    pub fn modify_existing_headset<R>(
        &mut self,
        serial: &str,
        f: impl FnOnce(&mut HeadsetRecord) -> R,
    ) -> Option<R> {
        let rec = self.data.headsets.get_mut(serial)?;
        self.dirty = true;
        Some(f(rec))
    }

    pub fn remove_headset(&mut self, serial: &str) -> Option<HeadsetRecord> {
        self.dirty = true;
        self.data.headsets.remove(serial)
    }

    /// Top-level extra field, e.g. `remote`.
    pub fn extra(&self, key: &str) -> Option<&Value> {
        self.data.extra.get(key)
    }

    pub fn set_extra(&mut self, key: &str, value: Value) {
        self.dirty = true;
        self.data.extra.insert(key.to_string(), value);
    }

    /// Write the store to disk now (atomically). No-op for in-memory stores.
    pub fn flush(&mut self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            self.dirty = false;
            return Ok(());
        };
        let json =
            serde_json::to_string_pretty(&self.data.to_json()).map_err(std::io::Error::other)?;
        write_atomic(path, json.as_bytes())?;
        self.dirty = false;
        Ok(())
    }
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Write `bytes` to `<path>.tmp`, sync it, then rename it over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = sibling(path, ".tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Electron's `app.getPath('appData')` for this platform (`None` where there is none, e.g. Android).
pub fn electron_app_data_dir() -> Option<PathBuf> {
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(target_os = "android") {
        None
    } else if cfg!(windows) {
        env("APPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library").join("Application Support"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
    }
}

/// Where the Electron app keeps its store (`<appData>/XR MultiCast/xr-multicast.json`).
pub fn electron_store_path() -> Option<PathBuf> {
    electron_app_data_dir().map(|d| d.join(APP_DIR_NAME).join(STORE_FILE_NAME))
}

/// The store file of the app's previous name (`<appData>/XR View/xrview.json`).
pub fn legacy_store_path() -> Option<PathBuf> {
    electron_app_data_dir().map(|d| d.join(LEGACY_APP_DIR_NAME).join(LEGACY_STORE_FILE_NAME))
}

struct SharedInner {
    store: Mutex<Store>,
    changed: Notify,
}

/// A [`Store`] shared between the managers, with debounced saving.
///
/// Mutations through [`SharedStore::write`] mark the store dirty and wake the
/// autosave task ([`SharedStore::autosave`]), which flushes after a short
/// quiet period (the Electron app waits 200 ms).
#[derive(Clone)]
pub struct SharedStore {
    inner: Arc<SharedInner>,
}

impl std::fmt::Debug for SharedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedStore").finish_non_exhaustive()
    }
}

impl SharedStore {
    pub fn new(store: Store) -> Self {
        SharedStore {
            inner: Arc::new(SharedInner {
                store: Mutex::new(store),
                changed: Notify::new(),
            }),
        }
    }

    /// An in-memory store with default contents.
    pub fn in_memory() -> Self {
        Self::new(Store::in_memory(StoreData::default()))
    }

    fn lock(&self) -> MutexGuard<'_, Store> {
        // A panic while holding the lock can't leave the store half-written: recover the data.
        self.inner.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Read from the store.
    pub fn read<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        f(&self.lock())
    }

    /// Change the store; schedules a save.
    pub fn write<R>(&self, f: impl FnOnce(&mut Store) -> R) -> R {
        let r = f(&mut self.lock());
        self.inner.changed.notify_one();
        r
    }

    pub fn settings(&self) -> Settings {
        self.read(|s| s.settings().clone())
    }

    pub fn headset(&self, serial: &str) -> Option<HeadsetRecord> {
        self.read(|s| s.headset(serial).cloned())
    }

    pub fn headsets(&self) -> Vec<HeadsetRecord> {
        self.read(|s| s.headsets().cloned().collect())
    }

    /// Write pending changes now.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut s = self.lock();
        if s.is_dirty() { s.flush() } else { Ok(()) }
    }

    /// Save changes `debounce` after they stop coming in. Runs until the task is dropped/aborted;
    /// call [`SharedStore::flush`] on shutdown.
    pub async fn autosave(self, debounce: Duration) {
        loop {
            self.inner.changed.notified().await;
            // Coalesce bursts (e.g. calibration sliders).
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(debounce) => break,
                    _ = self.inner.changed.notified() => continue,
                }
            }
            if let Err(e) = self.flush() {
                tracing::warn!("store: save failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A store file as written by the Electron app (v1.0).
    const ELECTRON_FILE: &str = r##"{
  "headsets": {
    "2G0YC5ZG8L0123": {
      "serial": "2G0YC5ZG8L0123",
      "name": "Meta Quest 3 #1",
      "color": "#F97316",
      "index": 1,
      "brand": "meta",
      "model": "Quest 3",
      "modelRaw": "Quest 3",
      "manufacturer": "Oculus",
      "verifiedAt": 1731312000000,
      "lastSeen": 1731315600000,
      "ip": "192.168.1.42",
      "port": 5555,
      "wireless": true,
      "calibration": { "k1": -0.22, "eye": "right", "rotation": "auto", "custom": null },
      "transport": "auto",
      "encoders": [{ "codec": "h265", "name": "c2.qti.hevc.encoder", "hw": true, "alias": false }],
      "futureField": [1, 2, 3]
    },
    "PFD3M0A1B2C3": {
      "serial": "PFD3M0A1B2C3",
      "name": "Lab PFD",
      "index": 2,
      "ip": null,
      "port": "not a number"
    }
  },
  "settings": {
    "requireUsbVerification": true,
    "usbQuality": "high",
    "wifiQuality": "custom",
    "wifiCustom": { "maxSize": 1600, "bitRateMbps": 12.5, "maxFps": 72 },
    "nextIndex": 3,
    "adbTermsAcceptedAt": 1731311000000
  },
  "remote": { "pin": "123456", "clients": {} }
}"##;

    #[test]
    fn reads_an_electron_store_file() {
        let data = StoreData::from_json(&serde_json::from_str(ELECTRON_FILE).unwrap());
        let q = &data.headsets["2G0YC5ZG8L0123"];
        assert_eq!(q.name.as_deref(), Some("Meta Quest 3 #1"));
        assert_eq!(q.index, Some(1));
        assert_eq!(q.port, Some(5555));
        assert_eq!(q.wireless, Some(true));
        assert_eq!(q.transport, Some(TransportPreference::Auto));
        assert_eq!(q.calibration_value("eye"), Some(&json!("right")));
        assert_eq!(q.encoders.as_ref().unwrap()[0].name, "c2.qti.hevc.encoder");
        assert_eq!(q.extra["futureField"], json!([1, 2, 3]));
        let p = &data.headsets["PFD3M0A1B2C3"];
        assert_eq!(p.ip, None);
        assert_eq!(p.port, None, "invalid fields are dropped, not fatal");
        assert_eq!(data.settings.next_index, Some(3));
        assert_eq!(data.settings.wifi_custom.max_size, 1600);
        assert!(
            data.settings.auto_retry_streams,
            "missing settings use the defaults"
        );
        assert_eq!(data.extra["remote"]["pin"], json!("123456"));
    }

    #[test]
    fn round_trips_unknown_fields() {
        let original: Value = serde_json::from_str(ELECTRON_FILE).unwrap();
        let data = StoreData::from_json(&original);
        let out = data.to_json();
        assert_eq!(out["remote"], original["remote"]);
        assert_eq!(
            out["headsets"]["2G0YC5ZG8L0123"]["futureField"],
            json!([1, 2, 3])
        );
        assert_eq!(
            out["headsets"]["2G0YC5ZG8L0123"]["calibration"],
            original["headsets"]["2G0YC5ZG8L0123"]["calibration"]
        );
        assert_eq!(
            out["headsets"]["2G0YC5ZG8L0123"]["verifiedAt"],
            json!(1731312000000u64)
        );
        assert_eq!(
            out["settings"]["adbTermsAcceptedAt"],
            json!(1731311000000u64)
        );
        assert_eq!(
            out["settings"]["remotePort"],
            json!(8787),
            "defaults are written like the Electron store"
        );
        // Reading our own output gives the same data.
        assert_eq!(StoreData::from_json(&out), data);
    }

    #[test]
    fn open_flush_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(STORE_FILE_NAME);
        let mut s = Store::open(&path);
        assert!(s.headsets().next().is_none());
        assert!(s.settings().require_usb_verification);
        s.upsert_headset(
            "S1",
            json!({ "name": "One", "index": 1, "ip": "10.0.0.2" })
                .as_object()
                .unwrap(),
        );
        s.upsert_headset(
            "S1",
            json!({ "ip": null, "lastSeen": 5 }).as_object().unwrap(),
        );
        s.update_settings(json!({ "nextIndex": 2 }).as_object().unwrap());
        assert!(s.is_dirty());
        s.flush().unwrap();
        assert!(!s.is_dirty());
        assert!(!sibling(&path, ".tmp").exists());

        let s2 = Store::open(&path);
        let r = s2.headset("S1").unwrap();
        assert_eq!(r.name.as_deref(), Some("One"));
        assert_eq!(r.ip, None);
        assert_eq!(r.last_seen, Some(5));
        assert_eq!(s2.settings().next_index, Some(2));
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["headsets"]["S1"]["serial"], json!("S1"));
        assert!(raw["headsets"]["S1"].get("ip").is_none());
    }

    #[test]
    fn corrupt_files_are_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE_FILE_NAME);
        std::fs::write(&path, "{ not json").unwrap();
        let s = Store::open(&path);
        assert!(s.headsets().next().is_none());
        assert_eq!(
            std::fs::read_to_string(sibling(&path, ".invalid")).unwrap(),
            "{ not json"
        );
        // Non-object roots are tolerated too.
        assert_eq!(StoreData::from_json(&json!([1, 2])), StoreData::default());
        assert_eq!(
            StoreData::from_json(&json!({ "headsets": { "X": 5 }, "settings": 3 }))
                .headsets
                .len(),
            0
        );
    }

    #[test]
    fn legacy_store_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("XR View").join(LEGACY_STORE_FILE_NAME);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy,
            r#"{"headsets":{"A":{"serial":"A","name":"Old"}},"settings":{}}"#,
        )
        .unwrap();
        let path = dir.path().join(APP_DIR_NAME).join(STORE_FILE_NAME);
        let s = Store::open_with_migration(&path, Some(&legacy));
        assert_eq!(s.headset("A").unwrap().name.as_deref(), Some("Old"));
        assert!(path.exists());
    }

    #[test]
    fn typed_modifications() {
        let mut s = Store::in_memory(StoreData::default());
        s.modify_headset("S2", |r| r.name = Some("Two".into()));
        assert_eq!(s.headset("S2").unwrap().serial, "S2");
        assert_eq!(s.modify_existing_headset("nope", |_| ()), None);
        assert!(s.remove_headset("S2").is_some());
        s.set_extra("remote", json!({}));
        assert_eq!(s.extra("remote"), Some(&json!({})));
        s.flush().unwrap();
    }

    #[test]
    fn electron_paths() {
        if let Some(p) = electron_store_path() {
            assert!(p.ends_with(Path::new(APP_DIR_NAME).join(STORE_FILE_NAME)));
        }
        if let Some(p) = legacy_store_path() {
            assert!(p.ends_with(Path::new("XR View").join("xrview.json")));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn autosave_debounces() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE_FILE_NAME);
        let shared = SharedStore::new(Store::open(&path));
        let task = tokio::spawn(shared.clone().autosave(Duration::from_millis(200)));
        shared.write(|s| s.modify_settings(|st| st.ten_bit = true));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!path.exists(), "not yet: still inside the debounce window");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(path.exists());
        assert!(Store::open(&path).settings().ten_bit);
        task.abort();
        shared.write(|s| s.modify_settings(|st| st.ten_bit = false));
        shared.flush().unwrap();
        assert!(!Store::open(&path).settings().ten_bit);
    }
}
