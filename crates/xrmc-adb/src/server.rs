//! Client for a running adb server (`127.0.0.1:5037`, "smart socket" protocol).
//!
//! Requests are sent as a 4-digit hex length followed by the request text;
//! the server answers `OKAY` or `FAIL` + hex length + message. After
//! `host:transport:<serial>` the socket is bound to one device and the next
//! request opens a device service; from then on the socket carries that
//! service's raw bytes.
//!
//! This lets the core coexist with Android Studio, scrcpy or a user's own adb
//! on desktop, where the adb server owns the USB interfaces.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::{AdbError, Result};

/// Default adb server port.
pub const DEFAULT_SERVER_PORT: u16 = 5037;

/// Address and timeouts for reaching an adb server.
#[derive(Debug, Clone)]
pub struct AdbServer {
    addr: SocketAddr,
    connect_timeout: Duration,
}

impl Default for AdbServer {
    fn default() -> Self {
        AdbServer::new(SocketAddr::from(([127, 0, 0, 1], DEFAULT_SERVER_PORT)))
    }
}

/// One line of `host:devices-l`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDeviceEntry {
    /// adb serial (`1WMHH8...`, `192.168.1.50:5555`, `emulator-5554`, mDNS names).
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, `authorizing`, `connecting`, `recovery`, ...
    pub state: String,
    /// `product`, `model`, `device`, `transport_id`, `usb` ...
    pub props: BTreeMap<String, String>,
}

impl ServerDeviceEntry {
    /// Whether the device is online and authorised.
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }

    /// `model:` property (underscores instead of spaces, as adb reports it).
    pub fn model(&self) -> Option<&str> {
        self.props.get("model").map(String::as_str)
    }

    /// adb's transport id.
    pub fn transport_id(&self) -> Option<u64> {
        self.props.get("transport_id").and_then(|s| s.parse().ok())
    }

    /// Whether this is a network transport (`ip:port` or an mDNS name) rather than USB.
    pub fn is_network(&self) -> bool {
        is_network_serial(&self.serial)
    }
}

/// Is this adb serial a network transport (tcpip or mDNS) rather than USB?
pub fn is_network_serial(serial: &str) -> bool {
    let port_suffix = serial
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    port_suffix || serial.contains("._adb-tls-connect.") || serial.contains("._adb._tcp")
}

/// Parse the text of `host:devices-l` (or `host:devices`).
pub fn parse_devices(text: &str) -> Vec<ServerDeviceEntry> {
    let mut list = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        let mut props = BTreeMap::new();
        let mut state = state.to_string();
        for part in parts {
            match part.split_once(':') {
                Some((k, v)) if !k.is_empty() => {
                    props.insert(k.to_string(), v.to_string());
                }
                // States with spaces, e.g. "no permissions (...)": keep the first word only.
                _ if props.is_empty() && state == "no" => state = format!("no {part}"),
                _ => {}
            }
        }
        list.push(ServerDeviceEntry {
            serial: serial.to_string(),
            state,
            props,
        });
    }
    list
}

impl AdbServer {
    /// A server at `addr`.
    pub fn new(addr: SocketAddr) -> AdbServer {
        AdbServer {
            addr,
            connect_timeout: Duration::from_secs(3),
        }
    }

    /// Change the TCP connect timeout (default 3 s).
    pub fn with_connect_timeout(mut self, timeout: Duration) -> AdbServer {
        self.connect_timeout = timeout;
        self
    }

    /// The server address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Open a fresh socket to the server.
    pub async fn connect(&self) -> Result<TcpStream> {
        match tokio::time::timeout(self.connect_timeout, TcpStream::connect(self.addr)).await {
            Ok(Ok(stream)) => {
                stream.set_nodelay(true)?;
                Ok(stream)
            }
            Ok(Err(e)) => Err(AdbError::Io(std::io::Error::new(
                e.kind(),
                format!("adb server not reachable at {}: {e}", self.addr),
            ))),
            Err(_) => Err(AdbError::Timeout),
        }
    }

    /// Whether a server is listening.
    pub async fn is_running(&self) -> bool {
        self.version().await.is_ok()
    }

    /// `host:version`: the server's internal protocol version (e.g. 41).
    pub async fn version(&self) -> Result<u32> {
        let text = self.query("host:version").await?;
        u32::from_str_radix(text.trim(), 16)
            .map_err(|_| AdbError::Protocol(format!("bad version reply {text:?}")))
    }

    /// `host:devices-l`: all transports the server knows (any state).
    pub async fn devices(&self) -> Result<Vec<ServerDeviceEntry>> {
        Ok(parse_devices(&self.query("host:devices-l").await?))
    }

    /// `host:connect:<addr>`: ask the server to connect to a device in tcpip mode.
    /// Returns the server's message (`connected to …` / `already connected to …`).
    pub async fn connect_device(&self, addr: &str) -> Result<String> {
        let msg = self.query(&format!("host:connect:{addr}")).await?;
        if msg.starts_with("connected to") || msg.starts_with("already connected to") {
            Ok(msg)
        } else {
            Err(AdbError::Other(if msg.is_empty() {
                format!("could not connect to {addr}")
            } else {
                msg
            }))
        }
    }

    /// `host:disconnect:<addr>`: drop a network transport from the server.
    pub async fn disconnect_device(&self, addr: &str) -> Result<String> {
        self.query(&format!("host:disconnect:{addr}")).await
    }

    /// `host-serial:<serial>:features`: the device's feature list.
    pub async fn features(&self, serial: &str) -> Result<Vec<String>> {
        let text = self
            .query(&format!("host-serial:{serial}:features"))
            .await?;
        Ok(text
            .trim()
            .split(',')
            .filter(|f| !f.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Send a host request whose reply is `OKAY` + a length-prefixed string.
    pub async fn query(&self, request: &str) -> Result<String> {
        let mut stream = self.connect().await?;
        send_request(&mut stream, request).await?;
        read_status(&mut stream, request).await?;
        read_length_prefixed(&mut stream).await
    }

    /// Open `service` on the device `serial` and return the raw stream
    /// (`host:transport:<serial>` followed by the service request).
    pub async fn open(&self, serial: &str, service: &str) -> Result<TcpStream> {
        let mut stream = self.connect().await?;
        let transport = format!("host:transport:{serial}");
        send_request(&mut stream, &transport).await?;
        read_status(&mut stream, &transport).await?;
        send_request(&mut stream, service).await?;
        match read_status(&mut stream, service).await {
            Ok(()) => Ok(stream),
            Err(AdbError::Other(msg)) => Err(AdbError::Refused(format!("{service}: {msg}"))),
            Err(e) => Err(e),
        }
    }
}

/// Write one smart-socket request (`%04x` length + text).
pub async fn send_request(stream: &mut TcpStream, request: &str) -> Result<()> {
    if request.len() > 0xffff {
        return Err(AdbError::Other("adb server request too long".into()));
    }
    let mut buf = format!("{:04x}", request.len()).into_bytes();
    buf.extend_from_slice(request.as_bytes());
    stream.write_all(&buf).await?;
    Ok(())
}

/// Read `OKAY` or `FAIL <hex len><message>`; a `FAIL` becomes
/// [`AdbError::Unauthorized`] for unauthorised devices and `AdbError::Other` otherwise.
pub async fn read_status(stream: &mut TcpStream, request: &str) -> Result<()> {
    let mut status = [0u8; 4];
    read_exact(stream, &mut status).await?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let msg = read_length_prefixed(stream).await?;
            if msg.contains("unauthorized") {
                Err(AdbError::Unauthorized)
            } else {
                Err(AdbError::Other(format!("adb server: {msg} ({request})")))
            }
        }
        other => Err(AdbError::Protocol(format!(
            "unexpected adb server reply {:?} to {request}",
            String::from_utf8_lossy(other)
        ))),
    }
}

async fn read_length_prefixed(stream: &mut TcpStream) -> Result<String> {
    let mut len = [0u8; 4];
    read_exact(stream, &mut len).await?;
    let len = std::str::from_utf8(&len)
        .ok()
        .and_then(|s| usize::from_str_radix(s, 16).ok())
        .ok_or_else(|| AdbError::Protocol("bad length in adb server reply".into()))?;
    let mut buf = vec![0u8; len];
    read_exact(stream, &mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> Result<()> {
    match stream.read_exact(buf).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(AdbError::Disconnected),
        Err(e) => Err(AdbError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_l() {
        let text = "List of devices attached\n\
            1WMHH000000000         device usb:1-2 product:hollywood model:Quest_3 device:eureka transport_id:3\n\
            192.168.1.50:5555      unauthorized transport_id:4\n\
            emulator-5554          device product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 device:emu64xa transport_id:1\n\
            0123456789ABCDEF       no permissions (missing udev rules? user is in the plugdev group); see [http://developer.android.com/tools/device.html] usb:1-1 transport_id:5\n";
        let list = parse_devices(text);
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].serial, "1WMHH000000000");
        assert!(list[0].is_ready());
        assert_eq!(list[0].model(), Some("Quest_3"));
        assert_eq!(list[0].transport_id(), Some(3));
        assert!(!list[0].is_network());
        assert_eq!(list[1].state, "unauthorized");
        assert!(list[1].is_network());
        assert_eq!(
            list[2].props.get("device").map(String::as_str),
            Some("emu64xa")
        );
        assert!(list[3].state.starts_with("no permissions"));
        assert!(!list[3].is_ready());
    }

    #[test]
    fn network_serials() {
        assert!(is_network_serial("192.168.1.5:5555"));
        assert!(is_network_serial("adb-1WMHH._adb-tls-connect._tcp"));
        assert!(!is_network_serial("1WMHH000000000"));
        assert!(!is_network_serial("emulator-5554"));
    }

    #[tokio::test]
    async fn smart_socket_against_fake_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut len = [0u8; 4];
                s.read_exact(&mut len).await.unwrap();
                let len = usize::from_str_radix(std::str::from_utf8(&len).unwrap(), 16).unwrap();
                let mut req = vec![0u8; len];
                s.read_exact(&mut req).await.unwrap();
                match std::str::from_utf8(&req).unwrap() {
                    "host:version" => s.write_all(b"OKAY00040029").await.unwrap(),
                    "host:devices-l" => {
                        let body =
                            "emulator-5554 device product:p model:m device:d transport_id:1\n";
                        s.write_all(format!("OKAY{:04x}{body}", body.len()).as_bytes())
                            .await
                            .unwrap();
                    }
                    "host:connect:127.0.0.1:1" => {
                        let body = "failed to connect to '127.0.0.1:1': Connection refused";
                        s.write_all(format!("OKAY{:04x}{body}", body.len()).as_bytes())
                            .await
                            .unwrap();
                    }
                    other => panic!("unexpected request {other}"),
                }
            }
        });
        let adb = AdbServer::new(addr);
        assert_eq!(adb.version().await.unwrap(), 0x29);
        let devices = adb.devices().await.unwrap();
        assert_eq!(devices[0].serial, "emulator-5554");
        let err = adb.connect_device("127.0.0.1:1").await.unwrap_err();
        assert!(err.to_string().contains("failed to connect"));
        server.await.unwrap();
    }
}
