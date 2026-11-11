//! Small host-side manager API: one key + options, many devices.

use std::sync::Arc;
use std::time::Duration;

use crate::auth::AdbKey;
use crate::connection::{AdbConnection, ConnectOptions};
#[cfg(not(target_os = "android"))]
use crate::device::AnyDevice;
use crate::device::{DirectDevice, ServerDevice, TransportKind};
use crate::server::{AdbServer, ServerDeviceEntry};
use crate::usb::UsbDeviceInfo;
use crate::{AdbError, Result};

/// Default port of adbd in `adb tcpip` mode.
pub const DEFAULT_TCP_PORT: u16 = 5555;

/// Entry point for the core: holds the host key, connection options and the
/// adb server address, and creates [`DirectDevice`]s / [`ServerDevice`]s.
#[derive(Debug, Clone)]
pub struct AdbHost {
    key: Arc<AdbKey>,
    options: ConnectOptions,
    server: AdbServer,
}

impl AdbHost {
    /// Use `key` with default options and the default adb server (`127.0.0.1:5037`).
    pub fn new(key: AdbKey) -> AdbHost {
        AdbHost::with_options(key, ConnectOptions::default())
    }

    /// Use `key` with the given options.
    pub fn with_options(key: AdbKey, options: ConnectOptions) -> AdbHost {
        AdbHost {
            key: Arc::new(key),
            options,
            server: AdbServer::default(),
        }
    }

    /// Load the key at `key_path` or generate and save one there.
    ///
    /// Generating an RSA-2048 key is CPU-heavy (up to a few seconds in debug
    /// builds); call this from `spawn_blocking` in async code.
    pub fn load_or_generate(key_path: impl AsRef<std::path::Path>) -> Result<AdbHost> {
        Ok(AdbHost::new(AdbKey::load_or_generate(key_path)?))
    }

    /// The host key.
    pub fn key(&self) -> &AdbKey {
        &self.key
    }

    /// Connection options.
    pub fn options(&self) -> &ConnectOptions {
        &self.options
    }

    /// Mutable connection options.
    pub fn options_mut(&mut self) -> &mut ConnectOptions {
        &mut self.options
    }

    /// The adb server used for server-based devices.
    pub fn server(&self) -> &AdbServer {
        &self.server
    }

    /// Use a different adb server.
    pub fn set_server(&mut self, server: AdbServer) {
        self.server = server;
    }

    /// List USB devices exposing an ADB interface (not on Android).
    pub async fn list_usb(&self) -> Result<Vec<UsbDeviceInfo>> {
        crate::usb::list_adb_devices().await
    }

    /// Connect directly over USB. Fails with `ResourceBusy` (see
    /// [`crate::usb::is_busy`]) when an adb server holds the device.
    #[cfg(not(target_os = "android"))]
    pub async fn connect_usb(&self, info: &UsbDeviceInfo) -> Result<DirectDevice> {
        let transport = crate::usb::UsbTransport::open(info).await?;
        let conn = AdbConnection::connect_usb(transport, Some(&self.key), &self.options).await?;
        Ok(DirectDevice::new(conn, TransportKind::Usb))
    }

    /// Connect over USB directly, or through the adb server if it holds the
    /// device (the usual situation on desktops with adb installed).
    #[cfg(not(target_os = "android"))]
    pub async fn connect_usb_or_server(&self, info: &UsbDeviceInfo) -> Result<AnyDevice> {
        match self.connect_usb(info).await {
            Ok(device) => Ok(device.into()),
            Err(e) if crate::usb::is_busy(&e) => {
                tracing::info!(
                    "USB device {} is held by the adb server; using the server",
                    info.adb_serial()
                );
                match self.connect_server(&info.adb_serial()).await {
                    Ok(device) => Ok(device.into()),
                    Err(AdbError::Unauthorized) => Err(AdbError::Unauthorized),
                    Err(server_err) => Err(AdbError::Other(format!(
                        "{e}; via the adb server: {server_err}"
                    ))),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Connect over a USB device file descriptor (Android `UsbDeviceConnection.getFileDescriptor()`,
    /// duplicated; see [`crate::usb::UsbTransport::from_fd`]).
    #[cfg(any(target_os = "android", target_os = "linux"))]
    pub async fn connect_usb_fd(
        &self,
        fd: std::os::fd::OwnedFd,
        serial: Option<String>,
    ) -> Result<DirectDevice> {
        let transport = crate::usb::UsbTransport::from_fd(fd, serial).await?;
        let conn = AdbConnection::connect_usb(transport, Some(&self.key), &self.options).await?;
        Ok(DirectDevice::new(conn, TransportKind::Usb))
    }

    /// Connect directly to adbd over TCP. `addr` is `host` or `host:port`
    /// (default port 5555). `timeout` bounds the TCP connect.
    pub async fn connect_tcp(&self, addr: &str, timeout: Duration) -> Result<DirectDevice> {
        let addr = normalize_address(addr, DEFAULT_TCP_PORT)?;
        let conn =
            AdbConnection::connect_tcp(&addr, Some(&self.key), &self.options, timeout).await?;
        Ok(DirectDevice::new(conn, TransportKind::Tcp))
    }

    /// Devices known to the adb server.
    pub async fn server_devices(&self) -> Result<Vec<ServerDeviceEntry>> {
        self.server.devices().await
    }

    /// A device through the adb server; checks that it is online and authorised.
    pub async fn connect_server(&self, serial: &str) -> Result<ServerDevice> {
        let devices = self.server.devices().await?;
        match devices.iter().find(|d| d.serial == serial) {
            Some(d) if d.is_ready() => Ok(ServerDevice::new(self.server.clone(), serial)),
            Some(d) if d.state == "unauthorized" || d.state == "authorizing" => {
                Err(AdbError::Unauthorized)
            }
            Some(d) => Err(AdbError::Other(format!("device {serial} is {}", d.state))),
            None => Err(AdbError::Other(format!(
                "device {serial} is not known to the adb server"
            ))),
        }
    }
}

/// Turn `host` or `host:port` (or `[v6]:port`) into `host:port`.
pub fn normalize_address(addr: &str, default_port: u16) -> Result<String> {
    let addr = addr.trim();
    if addr.is_empty() || addr.contains(char::is_whitespace) {
        return Err(AdbError::Other(format!(
            "invalid address {addr:?}; use e.g. 192.168.1.50 or 192.168.1.50:5555"
        )));
    }
    if let Some(rest) = addr.strip_prefix('[') {
        // [v6] or [v6]:port
        return match rest.split_once(']') {
            Some((_, "")) => Ok(format!("{addr}:{default_port}")),
            Some((_, port)) if port.strip_prefix(':').is_some_and(valid_port) => {
                Ok(addr.to_string())
            }
            _ => Err(AdbError::Other(format!("invalid address {addr:?}"))),
        };
    }
    match addr.matches(':').count() {
        0 => Ok(format!("{addr}:{default_port}")),
        1 => match addr.split_once(':') {
            Some((host, port)) if !host.is_empty() && valid_port(port) => Ok(addr.to_string()),
            _ => Err(AdbError::Other(format!("invalid address {addr:?}"))),
        },
        _ => Ok(format!("[{addr}]:{default_port}")),
    }
}

fn valid_port(port: &str) -> bool {
    port.parse::<u16>().is_ok_and(|p| p != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(
            normalize_address("192.168.1.50", 5555).unwrap(),
            "192.168.1.50:5555"
        );
        assert_eq!(
            normalize_address("192.168.1.50:5037", 5555).unwrap(),
            "192.168.1.50:5037"
        );
        assert_eq!(
            normalize_address(" quest.local ", 5555).unwrap(),
            "quest.local:5555"
        );
        assert_eq!(
            normalize_address("fe80::1", 5555).unwrap(),
            "[fe80::1]:5555"
        );
        assert_eq!(
            normalize_address("[fe80::1]:7000", 5555).unwrap(),
            "[fe80::1]:7000"
        );
        assert_eq!(
            normalize_address("[fe80::1]", 5555).unwrap(),
            "[fe80::1]:5555"
        );
        assert!(normalize_address("", 5555).is_err());
        assert!(normalize_address("a b", 5555).is_err());
        assert!(normalize_address("host:0", 5555).is_err());
        assert!(normalize_address("host:99999", 5555).is_err());
        assert!(normalize_address(":5555", 5555).is_err());
    }
}
