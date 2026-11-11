//! App settings, with the same keys and defaults as `DEFAULT_SETTINGS` in
//! `src/main/config.js`, so the Electron store file can be shared.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::config::{CodecPreference, DEFAULT_ADB_TCP_PORT, QualityKey, TransportKind};
use crate::view_size::ViewMode;

/// Custom encoder parameters (`usbCustom` / `wifiCustom`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CustomQuality {
    /// Long-side limit in pixels; 0 = full resolution.
    pub max_size: u32,
    pub bit_rate_mbps: f64,
    pub max_fps: u32,
}

/// Missing fields fall back to what `qualityFor` uses for them (0 px = full res, 8 Mbps, 60 fps).
impl Default for CustomQuality {
    fn default() -> Self {
        CustomQuality {
            max_size: 0,
            bit_rate_mbps: 8.0,
            max_fps: 60,
        }
    }
}

/// Grid column setting: `"auto"` or a number of columns (stored as a string by the UI).
fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected a string or number, got {other}"
        ))),
    }
}

/// All app settings.
///
/// Unknown keys (from newer app versions or the Electron app) are kept in
/// `extra` and written back unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Only headsets verified over USB may cast.
    pub require_usb_verification: bool,
    pub auto_reconnect_wifi: bool,
    pub auto_cast_on_connect: bool,
    pub auto_retry_streams: bool,
    pub usb_quality: QualityKey,
    pub wifi_quality: QualityKey,
    pub usb_codec: CodecPreference,
    pub wifi_codec: CodecPreference,
    /// Experimental 10-bit H.265/AV1.
    pub ten_bit: bool,
    /// Match each stream's encoded resolution to the size it is shown at
    /// (restarts the stream when it must change).
    pub auto_resolution: bool,
    pub usb_custom: CustomQuality,
    pub wifi_custom: CustomQuality,
    pub single_eye_stream: bool,
    pub default_view_mode: ViewMode,
    /// adb TCP port probed by network scans.
    pub scan_port: u16,
    #[serde(deserialize_with = "string_or_number")]
    pub grid_columns: String,
    pub remote_enabled: bool,
    pub remote_port: u16,
    pub remote_allow_control: bool,
    /// Next headset number handed out by verification (`settings.nextIndex || 1`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_index: Option<u32>,
    /// When the user accepted Google's terms for downloading adb (ms since the epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adb_terms_accepted_at: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            require_usb_verification: true,
            auto_reconnect_wifi: true,
            auto_cast_on_connect: false,
            auto_retry_streams: true,
            usb_quality: QualityKey::High,
            wifi_quality: QualityKey::Balanced,
            usb_codec: CodecPreference::Auto,
            wifi_codec: CodecPreference::Auto,
            ten_bit: false,
            auto_resolution: true,
            usb_custom: CustomQuality {
                max_size: 2560,
                bit_rate_mbps: 20.0,
                max_fps: 60,
            },
            wifi_custom: CustomQuality {
                max_size: 1920,
                bit_rate_mbps: 10.0,
                max_fps: 60,
            },
            single_eye_stream: false,
            default_view_mode: ViewMode::Corrected,
            scan_port: DEFAULT_ADB_TCP_PORT,
            grid_columns: "auto".to_string(),
            remote_enabled: false,
            remote_port: 8787,
            remote_allow_control: true,
            next_index: None,
            adb_terms_accepted_at: None,
            extra: Map::new(),
        }
    }
}

impl Settings {
    /// Settings from a stored JSON object, starting from the defaults (`{...DEFAULT_SETTINGS, ...stored}`).
    ///
    /// Values of the wrong type are ignored (the default stays) instead of
    /// failing the whole file; their keys are returned.
    pub fn from_json(stored: &Map<String, Value>) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let rejected = s.apply_patch(stored);
        (s, rejected)
    }

    /// Merge a partial settings object (`Object.assign(settings, patch)`).
    ///
    /// Each key is applied on its own; keys whose value doesn't fit are skipped
    /// and returned. `null` resets optional values.
    pub fn apply_patch(&mut self, patch: &Map<String, Value>) -> Vec<String> {
        let (merged, rejected) = lenient_merge(self, patch);
        *self = merged;
        rejected
    }

    /// The settings as a JSON object (camelCase keys, extras included).
    pub fn to_json(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    pub fn quality_key(&self, kind: TransportKind) -> QualityKey {
        match kind {
            TransportKind::Usb => self.usb_quality,
            TransportKind::Wifi => self.wifi_quality,
        }
    }

    pub fn custom_quality(&self, kind: TransportKind) -> CustomQuality {
        match kind {
            TransportKind::Usb => self.usb_custom,
            TransportKind::Wifi => self.wifi_custom,
        }
    }

    pub fn codec_preference(&self, kind: TransportKind) -> CodecPreference {
        match kind {
            TransportKind::Usb => self.usb_codec,
            TransportKind::Wifi => self.wifi_codec,
        }
    }

    /// Port for network scans (`Number(settings.scanPort) || 5555`).
    pub fn effective_scan_port(&self) -> u16 {
        if self.scan_port == 0 {
            DEFAULT_ADB_TCP_PORT
        } else {
            self.scan_port
        }
    }
}

/// Apply `patch` over `base` key by key, keeping the result deserialisable as
/// `T`. `null` removes a key (so it falls back to its default). Returns the
/// merged value and the keys that were rejected.
pub(crate) fn lenient_merge<T>(base: &T, patch: &Map<String, Value>) -> (T, Vec<String>)
where
    T: Serialize + DeserializeOwned + Clone,
{
    let mut current = match serde_json::to_value(base) {
        Ok(Value::Object(m)) => m,
        _ => return (base.clone(), patch.keys().cloned().collect()),
    };
    let mut accepted = None;
    let mut rejected = Vec::new();
    for (k, v) in patch {
        let mut trial = current.clone();
        if v.is_null() {
            trial.remove(k);
        } else {
            trial.insert(k.clone(), v.clone());
        }
        match serde_json::from_value::<T>(Value::Object(trial.clone())) {
            Ok(t) => {
                current = trial;
                accepted = Some(t);
            }
            Err(_) => rejected.push(k.clone()),
        }
    }
    (accepted.unwrap_or_else(|| base.clone()), rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn defaults_match_the_electron_app() {
        let v = Value::Object(Settings::default().to_json());
        let expected = json!({
            "requireUsbVerification": true,
            "autoReconnectWifi": true,
            "autoCastOnConnect": false,
            "autoRetryStreams": true,
            "usbQuality": "high",
            "wifiQuality": "balanced",
            "usbCodec": "auto",
            "wifiCodec": "auto",
            "tenBit": false,
            "autoResolution": true,
            "usbCustom": { "maxSize": 2560, "bitRateMbps": 20.0, "maxFps": 60 },
            "wifiCustom": { "maxSize": 1920, "bitRateMbps": 10.0, "maxFps": 60 },
            "singleEyeStream": false,
            "defaultViewMode": "corrected",
            "scanPort": 5555,
            "gridColumns": "auto",
            "remoteEnabled": false,
            "remotePort": 8787,
            "remoteAllowControl": true
        });
        assert_eq!(v, expected);
    }

    #[test]
    fn stored_values_override_defaults_and_unknown_keys_survive() {
        let (s, rejected) = Settings::from_json(&obj(json!({
            "usbQuality": "custom",
            "wifiCodec": "h265",
            "nextIndex": 4,
            "gridColumns": "3",
            "someFutureSetting": { "a": 1 },
            "adbTermsAcceptedAt": 1731312000000u64
        })));
        assert!(rejected.is_empty());
        assert_eq!(s.usb_quality, QualityKey::Custom);
        assert_eq!(
            s.wifi_codec,
            CodecPreference::Fixed(crate::config::Codec::H265)
        );
        assert_eq!(s.next_index, Some(4));
        assert_eq!(s.grid_columns, "3");
        assert!(s.auto_retry_streams, "untouched defaults stay");
        let out = s.to_json();
        assert_eq!(out["someFutureSetting"], json!({ "a": 1 }));
        assert_eq!(out["adbTermsAcceptedAt"], json!(1731312000000u64));
    }

    #[test]
    fn wrongly_typed_values_are_ignored_not_fatal() {
        let (s, rejected) = Settings::from_json(&obj(json!({
            "scanPort": "abc",
            "tenBit": "yes",
            "remotePort": 99999,
            "autoCastOnConnect": true,
            "gridColumns": 2
        })));
        let mut rejected = rejected;
        rejected.sort();
        assert_eq!(rejected, ["remotePort", "scanPort", "tenBit"]);
        assert_eq!(s.scan_port, 5555);
        assert!(!s.ten_bit);
        assert!(s.auto_cast_on_connect);
        assert_eq!(s.grid_columns, "2");
    }

    #[test]
    fn patches_merge_partially() {
        let mut s = Settings::default();
        let rejected = s.apply_patch(&obj(
            json!({ "wifiCustom": { "maxSize": 1600 }, "nextIndex": 2 }),
        ));
        assert!(rejected.is_empty());
        // A partial custom object fills the rest with qualityFor's fallbacks.
        assert_eq!(
            s.wifi_custom,
            CustomQuality {
                max_size: 1600,
                bit_rate_mbps: 8.0,
                max_fps: 60
            }
        );
        s.apply_patch(&obj(json!({ "nextIndex": null })));
        assert_eq!(s.next_index, None);
        assert_eq!(s.effective_scan_port(), 5555);
        s.scan_port = 0;
        assert_eq!(s.effective_scan_port(), 5555);
    }
}
