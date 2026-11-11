//! Shell commands sent to headsets and parsers for their output (ported from
//! `src/main/device-manager.js`, `src/main/stream-manager.js` and `src/main/adb.js`).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::brands::Eye;
use crate::config::DEFAULT_ADB_TCP_PORT;
use crate::discovery::MdnsService;
use crate::view_size::FrameSize;

/// One shell round trip that identifies a device: seven `getprop` lines, the
/// XR-related `pm` features, and an `#end` marker so a truncated reply is visible.
pub const IDENTIFY_COMMAND: &str = "getprop ro.product.manufacturer; getprop ro.product.brand; getprop ro.product.model; \
getprop ro.product.device; getprop ro.product.name; getprop ro.serialno; getprop ro.build.version.release; \
pm list features 2>/dev/null | grep -i xr; echo '#end'";

/// Battery level and charging state.
pub const BATTERY_COMMAND: &str = "dumpsys battery";

/// IPv4 addresses of the device's interfaces, one per line.
pub const IP_COMMAND: &str = "ip -o -4 addr show 2>/dev/null";

/// Natural display size (`Physical size:` / `Override size:`).
pub const WM_SIZE_COMMAND: &str = "wm size";

/// Round trip used to prove a USB connection during verification.
pub const VERIFY_COMMAND: &str = "echo xrmc-verified";

/// What [`VERIFY_COMMAND`] prints.
pub const VERIFY_TOKEN: &str = "xrmc-verified";

/// The adb service that restarts adbd listening on TCP `port` (`adb tcpip <port>`).
pub fn tcpip_service(port: u16) -> String {
    format!("tcpip:{port}")
}

/// Device properties used for brand detection and labelling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub brand: String,
    pub model: String,
    pub device: String,
    pub product: String,
    pub android_version: String,
    /// `pm` features containing "xr", without the `feature:` prefix.
    pub features: Vec<String>,
}

/// Parsed output of [`IDENTIFY_COMMAND`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// `ro.serialno`: the hardware serial (empty if the device didn't report one).
    pub serialno: String,
    pub info: DeviceInfo,
}

/// Parse the output of [`IDENTIFY_COMMAND`].
pub fn parse_identify(out: &str) -> Identity {
    let lines: Vec<&str> = out.split('\n').map(str::trim).collect();
    let field = |i: usize| lines.get(i).copied().unwrap_or("").to_string();
    let features = lines
        .iter()
        .skip(7)
        .filter_map(|l| l.strip_prefix("feature:"))
        .map(str::to_string)
        .collect();
    Identity {
        serialno: field(5),
        info: DeviceInfo {
            manufacturer: field(0),
            brand: field(1),
            model: field(2),
            device: field(3),
            product: field(4),
            android_version: field(6),
            features,
        },
    }
}

/// Battery state shown on headset cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Battery {
    /// Percent, 0..100.
    pub level: u32,
    pub charging: bool,
}

static BATTERY_LEVEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*level:\s*(\d+)").expect("valid regex"));
static BATTERY_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*status:\s*(\d+)").expect("valid regex"));
static BATTERY_POWERED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(AC|USB|Wireless) powered:\s*true").expect("valid regex"));

/// Parse `dumpsys battery`. `None` when there is no level line.
pub fn parse_battery(out: &str) -> Option<Battery> {
    let level = BATTERY_LEVEL.captures(out)?.get(1)?.as_str().parse().ok()?;
    // BatteryManager.BATTERY_STATUS_CHARGING = 2, BATTERY_STATUS_FULL = 5
    let status = BATTERY_STATUS
        .captures(out)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str());
    let charging = matches!(status, Some("2" | "5")) || BATTERY_POWERED.is_match(out);
    Some(Battery { level, charging })
}

static IP_LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+:\s+(\S+)\s+inet\s+([\d.]+)/").expect("valid regex"));
static WIFI_IFACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)wlan|wifi").expect("valid regex"));

/// Parse [`IP_COMMAND`]: the Wi-Fi address if there is one, else the first non-loopback address.
pub fn parse_ip(out: &str) -> Option<String> {
    let entries: Vec<(&str, &str)> = out
        .lines()
        .filter_map(|l| IP_LINE.captures(l))
        .filter_map(|c| Some((c.get(1)?.as_str(), c.get(2)?.as_str())))
        .filter(|(iface, ip)| *iface != "lo" && !ip.starts_with("127."))
        .collect();
    entries
        .iter()
        .find(|(iface, _)| WIFI_IFACE.is_match(iface))
        .or_else(|| entries.first())
        .map(|(_, ip)| ip.to_string())
}

static WM_OVERRIDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Override size:\s*(\d+)x(\d+)").expect("valid regex"));
static WM_PHYSICAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Physical size:\s*(\d+)x(\d+)").expect("valid regex"));

/// Parse `wm size`: the override size when set, else the physical size.
pub fn parse_wm_size(out: &str) -> Option<FrameSize> {
    let c = WM_OVERRIDE
        .captures(out)
        .or_else(|| WM_PHYSICAL.captures(out))?;
    Some(FrameSize {
        w: c.get(1)?.as_str().parse().ok()?,
        h: c.get(2)?.as_str().parse().ok()?,
    })
}

/// The scrcpy `crop=` value that keeps one eye of a side-by-side panel, or
/// `None` when the panel isn't side-by-side stereo (narrower than 1.6:1).
pub fn eye_crop(panel: FrameSize, eye: Eye) -> Option<String> {
    let FrameSize { w, h } = panel;
    if f64::from(w) < f64::from(h) * 1.6 {
        return None;
    }
    let half = w / 2;
    let x = if eye == Eye::Right { half } else { 0 };
    Some(format!("{half}:{h}:{x}:0"))
}

/// One line of `adb devices -l`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportEntry {
    /// adb serial: USB serial, `ip:port`, or an mDNS instance name.
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, `authorizing`, `connecting`, ...
    pub state: String,
    /// A network (TCP / mDNS) transport rather than USB.
    pub network: bool,
    /// `product`, `model`, `device`, `transport_id`, `usb`, ...
    pub props: BTreeMap<String, String>,
}

impl TransportEntry {
    /// A transport with the given state and no properties.
    pub fn new(serial: impl Into<String>, state: impl Into<String>) -> Self {
        let serial = serial.into();
        TransportEntry {
            network: is_network_serial(&serial),
            serial,
            state: state.into(),
            props: BTreeMap::new(),
        }
    }

    /// Adds a property (builder style).
    pub fn with_prop(mut self, key: &str, value: &str) -> Self {
        self.props.insert(key.to_string(), value.to_string());
        self
    }

    pub fn prop(&self, key: &str) -> &str {
        self.props.get(key).map_or("", String::as_str)
    }
}

/// Is this adb serial a network transport (tcpip or mDNS) rather than USB?
pub fn is_network_serial(serial: &str) -> bool {
    let tcp = serial
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    tcp || serial.contains("._adb-tls-connect.") || serial.contains("._adb._tcp")
}

/// Parse `adb devices -l`. Emulators are skipped.
pub fn parse_devices_l(out: &str) -> Vec<TransportEntry> {
    let mut list = Vec::new();
    for raw in out.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        if serial.starts_with("emulator-") {
            continue;
        }
        let props = parts
            .filter_map(|p| p.split_once(':').filter(|(k, _)| !k.is_empty()))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        list.push(TransportEntry {
            serial: serial.to_string(),
            state: state.to_string(),
            network: is_network_serial(serial),
            props,
        });
    }
    list
}

static MDNS_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\S+)\s+(_adb[\w-]*\._tcp)\.?\s+([\d.]+):(\d+)").expect("valid regex")
});

/// Parse `adb mdns services` (when going through an adb server instead of our own mDNS browser).
pub fn parse_mdns_services(out: &str) -> Vec<MdnsService> {
    out.lines()
        .filter_map(|l| MDNS_LINE.captures(l))
        .filter_map(|c| {
            Some(MdnsService {
                name: c.get(1)?.as_str().to_string(),
                service: c.get(2)?.as_str().to_string(),
                ip: c.get(3)?.as_str().to_string(),
                port: c.get(4)?.as_str().parse().ok()?,
            })
        })
        .collect()
}

static HOST_PORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([\d.]+):(\d+)$").expect("valid regex"));

/// `ip:port` of a TCP adb serial such as `192.168.1.42:5555`.
pub fn host_of(adb_serial: &str) -> Option<(String, u16)> {
    let c = HOST_PORT.captures(adb_serial)?;
    Some((
        c.get(1)?.as_str().to_string(),
        c.get(2)?.as_str().parse().ok()?,
    ))
}

static ADDRESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_.\-]+)(?::(\d+))?$").expect("valid regex"));

/// Parse a user-entered address (`192.168.1.50` or `192.168.1.50:5555`); the port defaults to 5555.
pub fn parse_address(address: &str) -> Result<(String, u16), crate::CoreError> {
    let invalid = || crate::CoreError::InvalidAddress(address.to_string());
    let c = ADDRESS.captures(address.trim()).ok_or_else(invalid)?;
    let host = c.get(1).ok_or_else(invalid)?.as_str().to_string();
    let port = match c.get(2) {
        Some(p) => p.as_str().parse().map_err(|_| invalid())?,
        None => DEFAULT_ADB_TCP_PORT,
    };
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUEST3_IDENTIFY: &str =
        "Oculus\r\noculus\r\nQuest 3\r\neureka\r\neureka\r\n2G0YC5ZG8L0123\r\n14\r\n#end\r\n";
    const PFD_MR_IDENTIFY: &str = "Play For Dream\nYVR\nPFDM MR\nyvr_d3\nyvr_d3\nPFD3M0A1B2C3\n14\n\
feature:android.software.xr.input.controller\nfeature:com.yvr.xr.passthrough\n#end\n";

    #[test]
    fn identify_quest_3() {
        let id = parse_identify(QUEST3_IDENTIFY);
        assert_eq!(id.serialno, "2G0YC5ZG8L0123");
        assert_eq!(id.info.manufacturer, "Oculus");
        assert_eq!(id.info.model, "Quest 3");
        assert_eq!(id.info.device, "eureka");
        assert_eq!(id.info.android_version, "14");
        assert!(id.info.features.is_empty());
        let brand = crate::brands::detect_brand(&id.info);
        assert_eq!(brand, "meta");
        assert_eq!(crate::brands::friendly_model(brand, &id.info), "Quest 3");
    }

    #[test]
    fn identify_play_for_dream_mr() {
        let id = parse_identify(PFD_MR_IDENTIFY);
        assert_eq!(id.serialno, "PFD3M0A1B2C3");
        assert_eq!(id.info.brand, "YVR");
        assert_eq!(
            id.info.features,
            [
                "android.software.xr.input.controller",
                "com.yvr.xr.passthrough"
            ]
        );
        let brand = crate::brands::detect_brand(&id.info);
        assert_eq!(brand, "yvr");
        assert_eq!(
            crate::brands::detect_variant(brand, &id.info),
            Some("pfd-mr")
        );
        assert_eq!(crate::brands::friendly_model(brand, &id.info), "MR");
    }

    #[test]
    fn identify_tolerates_truncated_output() {
        let id = parse_identify("Oculus\noculus\n");
        assert_eq!(id.info.brand, "oculus");
        assert_eq!(id.info.model, "");
        assert_eq!(id.serialno, "");
        assert_eq!(parse_identify(""), Identity::default());
    }

    #[test]
    fn battery_charging_over_usb() {
        let out = "Current Battery Service state:\n  AC powered: false\n  USB powered: true\n  Wireless powered: false\n  \
Max charging current: 500000\n  status: 2\n  health: 2\n  present: true\n  level: 87\n  scale: 100\n  voltage: 4213\n";
        assert_eq!(
            parse_battery(out),
            Some(Battery {
                level: 87,
                charging: true
            })
        );
    }

    #[test]
    fn battery_discharging_and_full() {
        let out = "  AC powered: false\n  USB powered: false\n  Wireless powered: false\n  status: 3\n  level: 41\n";
        assert_eq!(
            parse_battery(out),
            Some(Battery {
                level: 41,
                charging: false
            })
        );
        let full = "  AC powered: false\n  USB powered: false\n  status: 5\n  level: 100\n";
        assert_eq!(
            parse_battery(full),
            Some(Battery {
                level: 100,
                charging: true
            })
        );
        // "Max charging current" etc. must not be mistaken for the level line.
        assert_eq!(parse_battery("  Charge counter: 2412000\n"), None);
        assert_eq!(parse_battery(""), None);
    }

    #[test]
    fn ip_prefers_wlan() {
        let out = "1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever\n\
12: rmnet0    inet 10.20.30.40/30 scope global rmnet0\\       valid_lft forever preferred_lft forever\n\
22: wlan0    inet 192.168.1.42/24 brd 192.168.1.255 scope global wlan0\\       valid_lft forever preferred_lft forever\n";
        assert_eq!(parse_ip(out).as_deref(), Some("192.168.1.42"));
        let no_wlan = "1: lo    inet 127.0.0.1/8 scope host lo\n5: eth0    inet 10.0.0.7/24 scope global eth0\n";
        assert_eq!(parse_ip(no_wlan).as_deref(), Some("10.0.0.7"));
        assert_eq!(parse_ip("1: lo    inet 127.0.0.1/8 scope host lo\n"), None);
        assert_eq!(parse_ip(""), None);
    }

    #[test]
    fn wm_size_prefers_override() {
        assert_eq!(
            parse_wm_size("Physical size: 2856x3072\n"),
            Some(FrameSize { w: 2856, h: 3072 })
        );
        assert_eq!(
            parse_wm_size("Physical size: 4128x2208\nOverride size: 3664x1920\n"),
            Some(FrameSize { w: 3664, h: 1920 })
        );
        assert_eq!(parse_wm_size("Error: no display\n"), None);
        assert_eq!(parse_wm_size("Physical size: 99999999999x2\n"), None);
    }

    #[test]
    fn eye_crop_only_for_side_by_side_panels() {
        assert_eq!(
            eye_crop(FrameSize { w: 4128, h: 2208 }, Eye::Left).as_deref(),
            Some("2064:2208:0:0")
        );
        assert_eq!(
            eye_crop(FrameSize { w: 4128, h: 2208 }, Eye::Right).as_deref(),
            Some("2064:2208:2064:0")
        );
        assert_eq!(eye_crop(FrameSize { w: 2856, h: 3072 }, Eye::Left), None);
    }

    #[test]
    fn devices_l() {
        let out = "List of devices attached\n\
2G0YC5ZG8L0123         device usb:1-4 product:eureka model:Quest_3 device:eureka transport_id:3\n\
192.168.1.42:5555      device product:eureka model:Quest_3 device:eureka transport_id:4\n\
PFD3M0A1B2C3           unauthorized usb:1-3 transport_id:5\n\
adb-2G0YC5ZG8L0123-AbCdEf._adb-tls-connect._tcp device product:eureka model:Quest_3 device:eureka transport_id:6\n\
emulator-5554          device product:sdk_gphone64 model:sdk device:emu64 transport_id:1\n\
* daemon started successfully\n\n";
        let list = parse_devices_l(out);
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].serial, "2G0YC5ZG8L0123");
        assert_eq!(list[0].state, "device");
        assert!(!list[0].network);
        assert_eq!(list[0].prop("model"), "Quest_3");
        assert_eq!(list[0].prop("usb"), "1-4");
        assert!(list[1].network);
        assert_eq!(list[2].state, "unauthorized");
        assert_eq!(list[2].prop("model"), "");
        assert!(list[3].network);
    }

    #[test]
    fn network_serials() {
        assert!(is_network_serial("192.168.1.42:5555"));
        assert!(is_network_serial("adb-XYZ._adb-tls-connect._tcp"));
        assert!(!is_network_serial("2G0YC5ZG8L0123"));
        assert!(!is_network_serial("weird:"));
    }

    #[test]
    fn mdns_services_output() {
        let out = "List of discovered mdns services\n\
adb-2G0YC5ZG8L0123-AbCdEf\t_adb-tls-connect._tcp\t192.168.1.42:37123\n\
adb-PFD3M0A1B2C3\t_adb._tcp.\t192.168.1.43:5555\n";
        let s = parse_mdns_services(out);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].service, "_adb-tls-connect._tcp");
        assert_eq!((s[0].ip.as_str(), s[0].port), ("192.168.1.42", 37123));
        assert_eq!(s[1].service, "_adb._tcp");
    }

    #[test]
    fn hosts_and_addresses() {
        assert_eq!(
            host_of("192.168.1.42:5555"),
            Some(("192.168.1.42".into(), 5555))
        );
        assert_eq!(host_of("2G0YC5ZG8L0123"), None);
        assert_eq!(host_of("adb-x._adb-tls-connect._tcp"), None);
        assert_eq!(
            parse_address(" 192.168.1.50 ").unwrap(),
            ("192.168.1.50".into(), 5555)
        );
        assert_eq!(
            parse_address("quest.local:5556").unwrap(),
            ("quest.local".into(), 5556)
        );
        let e = parse_address("http://x").unwrap_err().to_string();
        assert_eq!(
            e,
            "Invalid address \"http://x\". Use e.g. 192.168.1.50 or 192.168.1.50:5555"
        );
        assert!(parse_address("1.2.3.4:99999").is_err());
        assert!(parse_address("").is_err());
    }

    #[test]
    fn commands_match_the_electron_app() {
        assert!(
            IDENTIFY_COMMAND
                .starts_with("getprop ro.product.manufacturer; getprop ro.product.brand;")
        );
        assert!(
            IDENTIFY_COMMAND.ends_with("pm list features 2>/dev/null | grep -i xr; echo '#end'")
        );
        assert_eq!(IDENTIFY_COMMAND.matches("getprop").count(), 7);
        assert!(!IDENTIFY_COMMAND.contains("  "));
        assert_eq!(tcpip_service(5555), "tcpip:5555");
    }
}
