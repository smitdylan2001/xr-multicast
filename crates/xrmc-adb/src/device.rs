//! [`AdbDevice`] implementations: direct connections (USB/TCP), devices behind
//! an adb server, and the [`AnyDevice`] enum that unifies them.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;

use crate::connection::{AdbConnection, AdbSocket};
use crate::protocol::DeviceBanner;
use crate::server::AdbServer;
use crate::shell::{self, ShellOutput};
use crate::sync::{self, FileStat};
use crate::{AdbDevice, AdbError, Result};

/// How a [`DirectDevice`] is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Directly over USB (nusb).
    Usb,
    /// Directly over TCP (`adb tcpip` mode, emulator).
    Tcp,
    /// Some other transport (e.g. an in-memory pipe in tests).
    Other,
}

/// A device this process talks to directly over its own [`AdbConnection`].
#[derive(Debug, Clone)]
pub struct DirectDevice {
    conn: AdbConnection,
    kind: TransportKind,
}

impl DirectDevice {
    /// Wrap an established connection.
    pub fn new(conn: AdbConnection, kind: TransportKind) -> DirectDevice {
        DirectDevice { conn, kind }
    }

    /// The underlying connection.
    pub fn connection(&self) -> &AdbConnection {
        &self.conn
    }

    /// How the device is connected.
    pub fn kind(&self) -> TransportKind {
        self.kind
    }

    /// The device's `CNXN` banner (`ro.product.model`, `features`, ...).
    pub fn banner(&self) -> &DeviceBanner {
        self.conn.banner()
    }

    /// Device features from the banner.
    pub fn features(&self) -> &[String] {
        &self.conn.banner().features
    }

    /// Whether the device supports shell protocol v2 (exit codes, separate stderr).
    pub fn supports_shell_v2(&self) -> bool {
        self.conn.banner().has_feature("shell_v2")
    }

    /// Whether the connection has been lost or closed.
    pub fn is_closed(&self) -> bool {
        self.conn.is_closed()
    }

    /// Wait until the connection is lost or closed.
    pub async fn closed(&self) {
        self.conn.closed().await
    }

    /// Run a command and return stdout, stderr and the exit code (shell v2), or
    /// merged output without exit code on devices without `shell_v2`.
    pub async fn shell_output(&self, command: &str) -> Result<ShellOutput> {
        if self.supports_shell_v2() {
            let mut s = self.conn.open(&shell::v2_service(command)).await?;
            shell::collect_v2(&mut s).await
        } else {
            let mut s = self.conn.open(&shell::legacy_service(command)).await?;
            shell::collect_legacy(&mut s).await
        }
    }

    /// `getprop <name>`, trimmed (empty if unset).
    pub async fn getprop(&self, name: &str) -> Result<String> {
        getprop_from(
            self.shell_output(&format!("getprop {}", shell::quote(name)))
                .await?,
        )
    }

    /// `lstat` a remote path via the sync service.
    pub async fn stat(&self, remote_path: &str) -> Result<FileStat> {
        let mut s = self.conn.open("sync:").await?;
        let st = sync::stat(&mut s, remote_path).await?;
        quit_sync(&mut s).await;
        Ok(st)
    }

    /// Restart adbd on the device in TCP mode on `port` (`adb tcpip`). Returns
    /// the device's message. adbd restarts, so this connection drops right
    /// after (over USB the device re-enumerates); connect again over TCP.
    pub async fn tcpip(&self, port: u16) -> Result<String> {
        let s = self.conn.open(&format!("tcpip:{port}")).await?;
        read_reply_until_close(s).await
    }
}

impl AdbDevice for DirectDevice {
    type Stream = AdbSocket;

    fn serial(&self) -> &str {
        self.conn.serial()
    }

    async fn shell(&self, command: &str) -> Result<Vec<u8>> {
        Ok(self.shell_output(command).await?.stdout)
    }

    async fn shell_stream(&self, command: &str) -> Result<AdbSocket> {
        self.conn.open(&shell::legacy_service(command)).await
    }

    async fn push(&self, data: &[u8], remote_path: &str, mode: u32) -> Result<()> {
        let mut s = self.conn.open("sync:").await?;
        let max_write = s.max_payload();
        sync::push(&mut s, data, remote_path, mode, max_write).await
    }

    async fn open(&self, service: &str) -> Result<AdbSocket> {
        self.conn.open(service).await
    }
}

/// A device reached through a running adb server (`host:transport:<serial>`).
#[derive(Debug, Clone)]
pub struct ServerDevice {
    server: AdbServer,
    serial: String,
    features: Arc<OnceCell<Vec<String>>>,
}

impl ServerDevice {
    /// A device known to `server` as `serial` (not checked here; see
    /// [`crate::AdbHost::connect_server`]).
    pub fn new(server: AdbServer, serial: impl Into<String>) -> ServerDevice {
        ServerDevice {
            server,
            serial: serial.into(),
            features: Arc::new(OnceCell::new()),
        }
    }

    /// The adb server used.
    pub fn server(&self) -> &AdbServer {
        &self.server
    }

    /// Device features (`host-serial:<serial>:features`), cached after the first call.
    pub async fn features(&self) -> Result<&[String]> {
        let features = self
            .features
            .get_or_try_init(|| self.server.features(&self.serial))
            .await?;
        Ok(features)
    }

    /// Whether the device supports shell protocol v2.
    pub async fn supports_shell_v2(&self) -> Result<bool> {
        Ok(self.features().await?.iter().any(|f| f == "shell_v2"))
    }

    /// Run a command and return stdout, stderr and the exit code (shell v2), or
    /// merged output without exit code on devices without `shell_v2`.
    pub async fn shell_output(&self, command: &str) -> Result<ShellOutput> {
        if self.supports_shell_v2().await? {
            let mut s = self
                .server
                .open(&self.serial, &shell::v2_service(command))
                .await?;
            shell::collect_v2(&mut s).await
        } else {
            let mut s = self
                .server
                .open(&self.serial, &shell::legacy_service(command))
                .await?;
            shell::collect_legacy(&mut s).await
        }
    }

    /// `getprop <name>`, trimmed (empty if unset).
    pub async fn getprop(&self, name: &str) -> Result<String> {
        getprop_from(
            self.shell_output(&format!("getprop {}", shell::quote(name)))
                .await?,
        )
    }

    /// `lstat` a remote path via the sync service.
    pub async fn stat(&self, remote_path: &str) -> Result<FileStat> {
        let mut s = self.server.open(&self.serial, "sync:").await?;
        let st = sync::stat(&mut s, remote_path).await?;
        quit_sync(&mut s).await;
        Ok(st)
    }

    /// Restart adbd on the device in TCP mode on `port` (`adb tcpip`); the
    /// device's transport drops right after.
    pub async fn tcpip(&self, port: u16) -> Result<String> {
        let s = self
            .server
            .open(&self.serial, &format!("tcpip:{port}"))
            .await?;
        read_reply_until_close(s).await
    }
}

impl AdbDevice for ServerDevice {
    type Stream = TcpStream;

    fn serial(&self) -> &str {
        &self.serial
    }

    async fn shell(&self, command: &str) -> Result<Vec<u8>> {
        Ok(self.shell_output(command).await?.stdout)
    }

    async fn shell_stream(&self, command: &str) -> Result<TcpStream> {
        self.server
            .open(&self.serial, &shell::legacy_service(command))
            .await
    }

    async fn push(&self, data: &[u8], remote_path: &str, mode: u32) -> Result<()> {
        let mut s = self.server.open(&self.serial, "sync:").await?;
        sync::push(&mut s, data, remote_path, mode, 256 * 1024).await
    }

    async fn open(&self, service: &str) -> Result<TcpStream> {
        self.server.open(&self.serial, service).await
    }
}

/// Any kind of device; implements [`AdbDevice`] by delegating.
#[derive(Debug, Clone)]
pub enum AnyDevice {
    /// Our own connection over USB or TCP.
    Direct(DirectDevice),
    /// Through a running adb server.
    Server(ServerDevice),
}

impl From<DirectDevice> for AnyDevice {
    fn from(d: DirectDevice) -> Self {
        AnyDevice::Direct(d)
    }
}

impl From<ServerDevice> for AnyDevice {
    fn from(d: ServerDevice) -> Self {
        AnyDevice::Server(d)
    }
}

impl AnyDevice {
    /// Run a command and return stdout, stderr and (with shell v2) the exit code.
    pub async fn shell_output(&self, command: &str) -> Result<ShellOutput> {
        match self {
            AnyDevice::Direct(d) => d.shell_output(command).await,
            AnyDevice::Server(d) => d.shell_output(command).await,
        }
    }

    /// `getprop <name>`, trimmed.
    pub async fn getprop(&self, name: &str) -> Result<String> {
        match self {
            AnyDevice::Direct(d) => d.getprop(name).await,
            AnyDevice::Server(d) => d.getprop(name).await,
        }
    }

    /// `lstat` a remote path.
    pub async fn stat(&self, remote_path: &str) -> Result<FileStat> {
        match self {
            AnyDevice::Direct(d) => d.stat(remote_path).await,
            AnyDevice::Server(d) => d.stat(remote_path).await,
        }
    }

    /// Restart adbd in TCP mode (`adb tcpip <port>`); the connection drops afterwards.
    pub async fn tcpip(&self, port: u16) -> Result<String> {
        match self {
            AnyDevice::Direct(d) => d.tcpip(port).await,
            AnyDevice::Server(d) => d.tcpip(port).await,
        }
    }

    /// Whether this device is reached over the network rather than USB.
    pub fn is_network(&self) -> bool {
        match self {
            AnyDevice::Direct(d) => d.kind() == TransportKind::Tcp,
            AnyDevice::Server(d) => crate::server::is_network_serial(d.serial()),
        }
    }
}

impl AdbDevice for AnyDevice {
    type Stream = AnyStream;

    fn serial(&self) -> &str {
        match self {
            AnyDevice::Direct(d) => d.serial(),
            AnyDevice::Server(d) => d.serial(),
        }
    }

    async fn shell(&self, command: &str) -> Result<Vec<u8>> {
        match self {
            AnyDevice::Direct(d) => d.shell(command).await,
            AnyDevice::Server(d) => d.shell(command).await,
        }
    }

    async fn shell_stream(&self, command: &str) -> Result<AnyStream> {
        match self {
            AnyDevice::Direct(d) => d.shell_stream(command).await.map(AnyStream::Direct),
            AnyDevice::Server(d) => d.shell_stream(command).await.map(AnyStream::Server),
        }
    }

    async fn push(&self, data: &[u8], remote_path: &str, mode: u32) -> Result<()> {
        match self {
            AnyDevice::Direct(d) => d.push(data, remote_path, mode).await,
            AnyDevice::Server(d) => d.push(data, remote_path, mode).await,
        }
    }

    async fn open(&self, service: &str) -> Result<AnyStream> {
        match self {
            AnyDevice::Direct(d) => AdbDevice::open(d, service).await.map(AnyStream::Direct),
            AnyDevice::Server(d) => AdbDevice::open(d, service).await.map(AnyStream::Server),
        }
    }
}

/// Stream type of [`AnyDevice`].
#[derive(Debug)]
pub enum AnyStream {
    /// A stream on our own connection.
    Direct(AdbSocket),
    /// A socket to the adb server bound to a device service.
    Server(TcpStream),
}

impl AsyncRead for AnyStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            AnyStream::Direct(s) => Pin::new(s).poll_read(cx, buf),
            AnyStream::Server(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for AnyStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            AnyStream::Direct(s) => Pin::new(s).poll_write(cx, data),
            AnyStream::Server(s) => Pin::new(s).poll_write(cx, data),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            AnyStream::Direct(s) => Pin::new(s).poll_flush(cx),
            AnyStream::Server(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            AnyStream::Direct(s) => Pin::new(s).poll_shutdown(cx),
            AnyStream::Server(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

fn getprop_from(out: ShellOutput) -> Result<String> {
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_string())
}

async fn quit_sync<S: AsyncWrite + Unpin>(s: &mut S) {
    use tokio::io::AsyncWriteExt as _;
    let _ = s
        .write_all(&sync::request_with_value(sync::id::QUIT, 0))
        .await;
}

/// Read a short service reply until the device closes the stream (or the
/// connection drops, as it does after `tcpip:`).
async fn read_reply_until_close<S: AsyncRead + Unpin>(mut s: S) -> Result<String> {
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match s.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) if !out.is_empty() => break,
            Err(e) => return Err(AdbError::Io(e)),
        }
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}
