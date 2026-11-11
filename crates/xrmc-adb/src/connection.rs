//! One ADB connection to a device, multiplexing many streams over a transport.
//!
//! [`AdbConnection::connect`] performs the `CNXN`/`AUTH` handshake and then
//! spawns two tokio tasks: a reader that dispatches incoming messages to
//! streams, and a writer that sends queued messages in order. Streams
//! ([`AdbSocket`]) implement [`tokio::io::AsyncRead`] and
//! [`tokio::io::AsyncWrite`].
//!
//! Flow control is adb's classic scheme: each side may have one unacknowledged
//! `WRTE` per stream and must wait for `OKAY` before sending the next.
//! Incoming data is acknowledged right away while the stream's receive buffer
//! is below a window; once it is full the `OKAY` is deferred until the
//! application reads, which stalls only that stream on the device.
//!
//! The connection stays up while any [`AdbConnection`] clone or [`AdbSocket`]
//! exists; when the last one is dropped the tasks stop and the transport is
//! closed.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot, watch};

use crate::auth::{AdbKey, default_key_name};
use crate::protocol::{
    A_AUTH, A_CLSE, A_CNXN, A_OKAY, A_OPEN, A_STLS, A_VERSION, A_VERSION_MIN,
    A_VERSION_SKIP_CHECKSUM, A_WRTE, AUTH_RSAPUBLICKEY, AUTH_SIGNATURE, AUTH_TOKEN, DeviceBanner,
    MAX_PAYLOAD, MAX_PAYLOAD_V1, Message, command_name, host_banner,
};
use crate::transport::{MessageReader, MessageWriter};
use crate::{AdbError, Result};

/// Minimum receive window per stream (bytes buffered before `OKAY`s are deferred).
const MIN_RECV_WINDOW: usize = 256 * 1024;

/// Progress of a connection handshake, reported through [`ConnectOptions::on_stage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeStage {
    /// `CNXN` sent, waiting for the device.
    Connecting,
    /// The device asked for authentication; our signature was sent.
    Authenticating,
    /// Our public key was sent: the headset shows "Allow USB debugging" and
    /// we wait for the user (adb reports this as `unauthorized`).
    AwaitingUserApproval,
    /// The device accepted the connection.
    Connected,
}

/// Callback receiving [`HandshakeStage`] updates.
pub type StageCallback = Arc<dyn Fn(HandshakeStage) + Send + Sync>;

/// Options for connecting to a device.
#[derive(Clone)]
pub struct ConnectOptions {
    /// Largest payload we accept / announce (clamped to 1 MiB). The device's
    /// value from its `CNXN` is used if smaller.
    pub max_payload: u32,
    /// Timeout for each handshake step before the user is asked to authorise
    /// this host (`CNXN` reply, `AUTH` exchange).
    pub handshake_timeout: Duration,
    /// After sending our public key, how long to wait for the user to accept
    /// "Allow USB debugging" on the headset before failing with
    /// [`AdbError::Unauthorized`]. `Duration::ZERO` sends the key (so the
    /// prompt appears) and fails right away.
    pub auth_timeout: Duration,
    /// Whether to offer the public key when the signature is rejected.
    pub send_public_key: bool,
    /// Comment appended to the public key (shown in the prompt), default `user@host`.
    pub key_name: String,
    /// Timeout for opening a stream (`OPEN` → `OKAY`/`CLSE`).
    pub open_timeout: Duration,
    /// Optional handshake progress callback (e.g. to show "unauthorized"
    /// while the user has not accepted the prompt yet).
    pub on_stage: Option<StageCallback>,
}

impl std::fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("max_payload", &self.max_payload)
            .field("handshake_timeout", &self.handshake_timeout)
            .field("auth_timeout", &self.auth_timeout)
            .field("send_public_key", &self.send_public_key)
            .field("key_name", &self.key_name)
            .field("open_timeout", &self.open_timeout)
            .field("on_stage", &self.on_stage.is_some())
            .finish()
    }
}

impl ConnectOptions {
    fn stage(&self, stage: HandshakeStage) {
        if let Some(cb) = &self.on_stage {
            cb(stage);
        }
    }
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            max_payload: MAX_PAYLOAD,
            handshake_timeout: Duration::from_secs(10),
            auth_timeout: Duration::from_secs(60),
            send_public_key: true,
            key_name: default_key_name(),
            open_timeout: Duration::from_secs(30),
            on_stage: None,
        }
    }
}

/// What the device told us in its `CNXN` message.
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    /// Transport-level serial (USB serial, `ip:port`, ...).
    pub serial: String,
    /// Negotiated protocol version (`min(ours, device's)`).
    pub version: u32,
    /// Negotiated maximum payload per message.
    pub max_payload: usize,
    /// The device's identity banner (`ro.product.model`, `features=`, ...).
    pub banner: DeviceBanner,
}

impl ConnectionInfo {
    /// Whether outgoing messages need the legacy checksum.
    fn checksums(&self) -> bool {
        self.version < A_VERSION_SKIP_CHECKSUM
    }
}

/// A live, authenticated connection to one device.
#[derive(Clone)]
pub struct AdbConnection {
    shared: Arc<Shared>,
    guard: Arc<Guard>,
}

impl std::fmt::Debug for AdbConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdbConnection")
            .field("serial", &self.shared.info.serial)
            .field(
                "version",
                &format_args!("0x{:08x}", self.shared.info.version),
            )
            .field("max_payload", &self.shared.info.max_payload)
            .finish_non_exhaustive()
    }
}

struct Shared {
    info: ConnectionInfo,
    recv_window: usize,
    open_timeout: Duration,
    out: mpsc::UnboundedSender<Message>,
    stop: watch::Sender<bool>,
    state: Mutex<ConnState>,
}

struct ConnState {
    next_id: u32,
    opening: HashMap<u32, PendingOpen>,
    streams: HashMap<u32, Arc<StreamShared>>,
    dead: Option<String>,
}

struct PendingOpen {
    service: String,
    reply: oneshot::Sender<Result<AdbSocket>>,
    guard: Arc<Guard>,
}

/// Dropping the last guard (held by connection handles and sockets) shuts the connection down.
struct Guard {
    shared: Arc<Shared>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.shared.fail("connection closed by host".to_string());
    }
}

struct StreamShared {
    local_id: u32,
    remote_id: u32,
    state: Mutex<StreamState>,
}

#[derive(Default)]
struct StreamState {
    recv: VecDeque<Bytes>,
    recv_len: usize,
    /// We still owe the device an `OKAY` for a `WRTE` (deferred: buffer full).
    ack_owed: bool,
    /// Our last `WRTE` is still waiting for the device's `OKAY`.
    write_pending: bool,
    /// The device closed the stream.
    remote_closed: bool,
    /// We sent `CLSE` (or no longer need to).
    clse_sent: bool,
    /// The connection died.
    error: Option<String>,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Shared {
    fn send(&self, msg: Message) {
        // Fails only when the writer has stopped, i.e. the connection is dead.
        let _ = self.out.send(msg);
    }

    fn is_dead(&self) -> bool {
        lock(&self.state).dead.is_some()
    }

    /// Mark the connection dead: fail pending opens and all streams, stop the tasks.
    fn fail(&self, reason: String) {
        let (opening, streams) = {
            let mut st = lock(&self.state);
            if st.dead.is_some() {
                return;
            }
            tracing::debug!(serial = %self.info.serial, "adb connection closed: {reason}");
            st.dead = Some(reason.clone());
            (
                std::mem::take(&mut st.opening),
                std::mem::take(&mut st.streams),
            )
        };
        for (_, pending) in opening {
            let _ = pending.reply.send(Err(AdbError::Disconnected));
        }
        for (_, stream) in streams {
            let mut s = lock(&stream.state);
            if s.error.is_none() {
                s.error = Some(reason.clone());
            }
            wake(&mut s.read_waker);
            wake(&mut s.write_waker);
        }
        self.stop.send_replace(true);
    }

    fn remove_stream(&self, local_id: u32) {
        lock(&self.state).streams.remove(&local_id);
    }

    /// Dispatch one incoming message (reader task).
    fn handle(self: &Arc<Self>, msg: Message) -> Result<()> {
        match msg.command {
            A_OKAY => {
                let (local, remote) = (msg.arg1, msg.arg0);
                let mut st = lock(&self.state);
                if let Some(pending) = st.opening.remove(&local) {
                    let stream = Arc::new(StreamShared {
                        local_id: local,
                        remote_id: remote,
                        state: Mutex::new(StreamState::default()),
                    });
                    st.streams.insert(local, stream.clone());
                    drop(st);
                    let socket = AdbSocket {
                        shared: self.clone(),
                        stream,
                        _guard: pending.guard,
                    };
                    // If the opener gave up meanwhile, dropping the socket closes the stream.
                    if let Err(unwanted) = pending.reply.send(Ok(socket)) {
                        drop(unwanted);
                    }
                } else if let Some(stream) = st.streams.get(&local).cloned() {
                    drop(st);
                    let mut s = lock(&stream.state);
                    s.write_pending = false;
                    wake(&mut s.write_waker);
                } else {
                    drop(st);
                    // A stream we no longer track (e.g. its OPEN timed out): close it on the device.
                    if remote != 0 {
                        self.send(Message::new(A_CLSE, local, remote, Bytes::new()));
                    }
                }
            }
            A_WRTE => {
                let (local, remote) = (msg.arg1, msg.arg0);
                let stream = lock(&self.state).streams.get(&local).cloned();
                if let Some(stream) = stream {
                    let mut s = lock(&stream.state);
                    if s.clse_sent || s.remote_closed {
                        return Ok(());
                    }
                    s.recv_len += msg.payload.len();
                    if !msg.payload.is_empty() {
                        s.recv.push_back(msg.payload);
                    }
                    if s.recv_len <= self.recv_window {
                        self.send(Message::new(A_OKAY, local, remote, Bytes::new()));
                    } else {
                        s.ack_owed = true;
                    }
                    wake(&mut s.read_waker);
                }
            }
            A_CLSE => {
                let (local, remote) = (msg.arg1, msg.arg0);
                let mut st = lock(&self.state);
                if let Some(pending) = st.opening.remove(&local) {
                    drop(st);
                    let _ = pending.reply.send(Err(AdbError::Refused(pending.service)));
                } else if let Some(stream) = st.streams.remove(&local) {
                    drop(st);
                    let mut s = lock(&stream.state);
                    s.remote_closed = true;
                    if !s.clse_sent {
                        s.clse_sent = true;
                        if remote != 0 {
                            self.send(Message::new(A_CLSE, local, remote, Bytes::new()));
                        }
                    }
                    wake(&mut s.read_waker);
                    wake(&mut s.write_waker);
                }
            }
            A_OPEN => {
                // Device-initiated streams (`adb reverse`) are not supported.
                self.send(Message::new(A_CLSE, 0, msg.arg0, Bytes::new()));
            }
            A_CNXN | A_AUTH | A_STLS => {
                return Err(AdbError::Protocol(format!(
                    "device restarted the handshake ({}); adbd probably restarted",
                    command_name(msg.command)
                )));
            }
            other => {
                tracing::debug!("ignoring unexpected adb message {}", command_name(other));
            }
        }
        Ok(())
    }
}

fn wake(waker: &mut Option<Waker>) {
    if let Some(w) = waker.take() {
        w.wake();
    }
}

impl AdbConnection {
    /// Perform the handshake over the given transport halves and start the
    /// connection's background tasks. `key` is used to answer `AUTH`; with
    /// `None`, devices that require authentication fail with [`AdbError::Unauthorized`].
    pub async fn connect<R, W>(
        mut reader: R,
        mut writer: W,
        key: Option<&AdbKey>,
        options: &ConnectOptions,
        serial: impl Into<String>,
    ) -> Result<AdbConnection>
    where
        R: MessageReader,
        W: MessageWriter,
    {
        let serial = serial.into();
        let (version, max_payload, banner) =
            handshake(&mut reader, &mut writer, key, options).await?;
        let info = ConnectionInfo {
            serial,
            version,
            max_payload,
            banner,
        };
        tracing::debug!(serial = %info.serial, version = format_args!("0x{version:08x}"), max_payload, banner = %info.banner.raw, "adb connected");
        Ok(AdbConnection::start(reader, writer, info, options))
    }

    /// Connect over any bidirectional byte stream (TCP socket, in-memory pipe, ...).
    pub async fn connect_stream<S>(
        stream: S,
        key: Option<&AdbKey>,
        options: &ConnectOptions,
        serial: impl Into<String>,
    ) -> Result<AdbConnection>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (r, w) = crate::transport::split_stream(stream);
        AdbConnection::connect(r, w, key, options, serial).await
    }

    /// Connect to an adbd listening on TCP (`ip:port`).
    pub async fn connect_tcp(
        addr: &str,
        key: Option<&AdbKey>,
        options: &ConnectOptions,
        timeout: Duration,
    ) -> Result<AdbConnection> {
        let (r, w) = crate::transport::connect_tcp(addr, timeout).await?;
        AdbConnection::connect(r, w, key, options, addr).await
    }

    /// Connect over an opened USB ADB interface.
    pub async fn connect_usb(
        transport: crate::usb::UsbTransport,
        key: Option<&AdbKey>,
        options: &ConnectOptions,
    ) -> Result<AdbConnection> {
        let serial = transport.serial().to_string();
        let (r, w) = transport.into_split();
        AdbConnection::connect(r, w, key, options, serial).await
    }

    fn start<R: MessageReader, W: MessageWriter>(
        reader: R,
        writer: W,
        info: ConnectionInfo,
        options: &ConnectOptions,
    ) -> AdbConnection {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (stop_tx, _) = watch::channel(false);
        let recv_window = info.max_payload.max(MIN_RECV_WINDOW);
        let checksums = info.checksums();
        let shared = Arc::new(Shared {
            info,
            recv_window,
            open_timeout: options.open_timeout,
            out: out_tx,
            stop: stop_tx,
            state: Mutex::new(ConnState {
                next_id: 1,
                opening: HashMap::new(),
                streams: HashMap::new(),
                dead: None,
            }),
        });
        tokio::spawn(reader_task(reader, shared.clone(), checksums));
        tokio::spawn(writer_task(writer, out_rx, shared.clone(), checksums));
        let guard = Arc::new(Guard {
            shared: shared.clone(),
        });
        AdbConnection { shared, guard }
    }

    /// Connection details from the handshake.
    pub fn info(&self) -> &ConnectionInfo {
        &self.shared.info
    }

    /// Transport-level serial.
    pub fn serial(&self) -> &str {
        &self.shared.info.serial
    }

    /// The device's `CNXN` banner (model, features, ...).
    pub fn banner(&self) -> &DeviceBanner {
        &self.shared.info.banner
    }

    /// Negotiated maximum payload per message.
    pub fn max_payload(&self) -> usize {
        self.shared.info.max_payload
    }

    /// Whether the connection has been closed or lost.
    pub fn is_closed(&self) -> bool {
        self.shared.is_dead()
    }

    /// Why the connection ended, if it did.
    pub fn close_reason(&self) -> Option<String> {
        lock(&self.shared.state).dead.clone()
    }

    /// Wait until the connection is closed or lost (e.g. the cable was unplugged).
    pub async fn closed(&self) {
        let mut rx = self.shared.stop.subscribe();
        let _ = rx.wait_for(|stopped| *stopped).await;
    }

    /// Close the connection now; all streams fail with a disconnection error.
    pub fn close(&self) {
        self.shared.fail("connection closed by host".to_string());
    }

    /// Open a stream to a device service, e.g. `shell:ls`, `sync:`,
    /// `localabstract:scrcpy_1234abcd`, `tcp:27183`.
    pub async fn open(&self, service: &str) -> Result<AdbSocket> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let local_id = {
            let mut st = lock(&self.shared.state);
            if st.dead.is_some() {
                return Err(AdbError::Disconnected);
            }
            let id = st.next_id;
            st.next_id = st.next_id.checked_add(1).unwrap_or(1);
            st.opening.insert(
                id,
                PendingOpen {
                    service: service.to_string(),
                    reply: reply_tx,
                    guard: self.guard.clone(),
                },
            );
            id
        };
        // Forget the pending open if this future is dropped or times out; a late
        // OKAY for the id is then answered with CLSE by the reader.
        let _cancel = CancelOpen {
            shared: &self.shared,
            local_id,
        };
        let mut payload = Vec::with_capacity(service.len() + 1);
        payload.extend_from_slice(service.as_bytes());
        payload.push(0);
        self.shared.send(Message::new(A_OPEN, local_id, 0, payload));

        match tokio::time::timeout(self.shared.open_timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AdbError::Disconnected),
            Err(_) => Err(AdbError::Timeout),
        }
    }
}

struct CancelOpen<'a> {
    shared: &'a Shared,
    local_id: u32,
}

impl Drop for CancelOpen<'_> {
    fn drop(&mut self) {
        let pending = lock(&self.shared.state).opening.remove(&self.local_id);
        // Dropped outside the lock: it holds a guard whose drop may lock the state.
        drop(pending);
    }
}

async fn handshake<R: MessageReader, W: MessageWriter>(
    reader: &mut R,
    writer: &mut W,
    key: Option<&AdbKey>,
    options: &ConnectOptions,
) -> Result<(u32, usize, DeviceBanner)> {
    let our_max = options.max_payload.clamp(MAX_PAYLOAD_V1, MAX_PAYLOAD);
    // The device's version is unknown yet, so include the checksum like adb does.
    writer
        .write_message(
            &Message::new(A_CNXN, A_VERSION, our_max, host_banner().into_bytes()),
            true,
        )
        .await?;
    writer.flush().await?;
    options.stage(HandshakeStage::Connecting);

    let mut signed = false;
    let mut auth_deadline: Option<tokio::time::Instant> = None;
    loop {
        let wait = match auth_deadline {
            Some(deadline) => deadline.saturating_duration_since(tokio::time::Instant::now()),
            None => options.handshake_timeout,
        };
        let msg = match tokio::time::timeout(wait, reader.read_message(false)).await {
            Ok(msg) => msg?,
            Err(_) if auth_deadline.is_some() => return Err(AdbError::Unauthorized),
            Err(_) => return Err(AdbError::Timeout),
        };
        match msg.command {
            A_CNXN => {
                let version = msg.arg0.clamp(A_VERSION_MIN, A_VERSION);
                let device_max = if msg.arg1 == 0 {
                    MAX_PAYLOAD_V1
                } else {
                    msg.arg1
                };
                let max_payload = device_max.min(our_max) as usize;
                options.stage(HandshakeStage::Connected);
                return Ok((version, max_payload, DeviceBanner::parse(&msg.payload)));
            }
            A_STLS => {
                return Err(AdbError::Protocol(
                    "the device requires TLS (Android 11+ wireless debugging pairing), which is not supported; \
                     connect over USB or enable `adb tcpip` mode"
                        .into(),
                ));
            }
            A_AUTH if msg.arg0 == AUTH_TOKEN => {
                let Some(key) = key else {
                    return Err(AdbError::Unauthorized);
                };
                if !signed {
                    let signature = key.sign_token(&msg.payload)?;
                    writer
                        .write_message(&Message::new(A_AUTH, AUTH_SIGNATURE, 0, signature), true)
                        .await?;
                    writer.flush().await?;
                    signed = true;
                    options.stage(HandshakeStage::Authenticating);
                } else if auth_deadline.is_none() {
                    // The signature was rejected: offer our public key so the user can allow it.
                    if !options.send_public_key {
                        return Err(AdbError::Unauthorized);
                    }
                    writer
                        .write_message(
                            &Message::new(
                                A_AUTH,
                                AUTH_RSAPUBLICKEY,
                                0,
                                key.auth_public_key_payload(&options.key_name),
                            ),
                            true,
                        )
                        .await?;
                    writer.flush().await?;
                    tracing::info!(
                        "waiting for the user to accept \"Allow USB debugging\" on the device"
                    );
                    options.stage(HandshakeStage::AwaitingUserApproval);
                    if options.auth_timeout.is_zero() {
                        return Err(AdbError::Unauthorized);
                    }
                    auth_deadline = Some(tokio::time::Instant::now() + options.auth_timeout);
                }
                // A further TOKEN while waiting for the user: keep waiting.
            }
            other => {
                // Stale traffic from a previous session (e.g. on USB); ignore it.
                tracing::debug!("ignoring {} during handshake", command_name(other));
            }
        }
    }
}

async fn reader_task<R: MessageReader>(mut reader: R, shared: Arc<Shared>, verify_checksum: bool) {
    let mut stop = shared.stop.subscribe();
    loop {
        let msg = tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => break,
            msg = reader.read_message(verify_checksum) => msg,
        };
        let result = msg.and_then(|msg| shared.handle(msg));
        if let Err(e) = result {
            let reason = match e {
                AdbError::Disconnected => "device disconnected".to_string(),
                other => other.to_string(),
            };
            shared.fail(reason);
            break;
        }
    }
}

async fn writer_task<W: MessageWriter>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Message>,
    shared: Arc<Shared>,
    checksums: bool,
) {
    let mut stop = shared.stop.subscribe();
    loop {
        let msg = tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => break,
            msg = rx.recv() => msg,
        };
        let Some(msg) = msg else { break };
        let mut result = writer.write_message(&msg, checksums).await;
        // Batch whatever is already queued, then flush once.
        while result.is_ok() {
            match rx.try_recv() {
                Ok(next) => result = writer.write_message(&next, checksums).await,
                Err(_) => break,
            }
        }
        if result.is_ok() {
            result = writer.flush().await;
        }
        if let Err(e) = result {
            shared.fail(format!("write failed: {e}"));
            break;
        }
    }
}

/// One stream on an [`AdbConnection`]: a bidirectional byte pipe to a device service.
///
/// Shutting down (`AsyncWriteExt::shutdown`) or dropping it sends `CLSE`;
/// ADB has no half-close, so this ends both directions. Reads return EOF
/// once the device closes the stream and buffered data has been consumed.
pub struct AdbSocket {
    shared: Arc<Shared>,
    stream: Arc<StreamShared>,
    _guard: Arc<Guard>,
}

impl std::fmt::Debug for AdbSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdbSocket")
            .field("local_id", &self.stream.local_id)
            .field("remote_id", &self.stream.remote_id)
            .finish()
    }
}

impl AdbSocket {
    /// Our stream id.
    pub fn local_id(&self) -> u32 {
        self.stream.local_id
    }

    /// The device's stream id.
    pub fn remote_id(&self) -> u32 {
        self.stream.remote_id
    }

    /// Maximum bytes per `WRTE` on this connection.
    pub fn max_payload(&self) -> usize {
        self.shared.info.max_payload
    }

    /// Whether the device has closed this stream.
    pub fn is_remote_closed(&self) -> bool {
        lock(&self.stream.state).remote_closed
    }

    fn send_clse(&self) {
        self.shared.send(Message::new(
            A_CLSE,
            self.stream.local_id,
            self.stream.remote_id,
            Bytes::new(),
        ));
    }
}

fn aborted(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        format!("adb connection lost: {reason}"),
    )
}

impl AsyncRead for AdbSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut s = lock(&this.stream.state);
        if !s.recv.is_empty() {
            while buf.remaining() > 0 {
                let Some(front) = s.recv.front_mut() else {
                    break;
                };
                let n = front.len().min(buf.remaining());
                buf.put_slice(&front[..n]);
                front.advance(n);
                if front.is_empty() {
                    s.recv.pop_front();
                }
                s.recv_len -= n;
            }
            if s.ack_owed && s.recv_len <= this.shared.recv_window {
                s.ack_owed = false;
                if !s.remote_closed && !s.clse_sent && s.error.is_none() {
                    this.shared.send(Message::new(
                        A_OKAY,
                        this.stream.local_id,
                        this.stream.remote_id,
                        Bytes::new(),
                    ));
                }
            }
            return Poll::Ready(Ok(()));
        }
        if s.remote_closed || s.clse_sent {
            return Poll::Ready(Ok(()));
        }
        if let Some(reason) = &s.error {
            return Poll::Ready(Err(aborted(reason)));
        }
        s.read_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for AdbSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut s = lock(&this.stream.state);
        if let Some(reason) = &s.error {
            return Poll::Ready(Err(aborted(reason)));
        }
        if s.remote_closed || s.clse_sent {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "adb stream closed",
            )));
        }
        if data.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if s.write_pending {
            s.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = data.len().min(this.shared.info.max_payload);
        s.write_pending = true;
        this.shared.send(Message::new(
            A_WRTE,
            this.stream.local_id,
            this.stream.remote_id,
            Bytes::copy_from_slice(&data[..n]),
        ));
        Poll::Ready(Ok(n))
    }

    /// Completes once the device has acknowledged everything written so far.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut s = lock(&this.stream.state);
        if let Some(reason) = &s.error {
            return Poll::Ready(Err(aborted(reason)));
        }
        if !s.write_pending || s.remote_closed || s.clse_sent {
            return Poll::Ready(Ok(()));
        }
        s.write_waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Waits for outstanding data to be acknowledged, then sends `CLSE`.
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut s = lock(&this.stream.state);
        if s.write_pending && !s.remote_closed && !s.clse_sent && s.error.is_none() {
            s.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let need_clse = !s.clse_sent && !s.remote_closed && s.error.is_none();
        s.clse_sent = true;
        wake(&mut s.read_waker);
        drop(s);
        if need_clse {
            this.send_clse();
        }
        this.shared.remove_stream(this.stream.local_id);
        Poll::Ready(Ok(()))
    }
}

impl Drop for AdbSocket {
    fn drop(&mut self) {
        let need_clse = {
            let mut s = lock(&self.stream.state);
            let need = !s.clse_sent && !s.remote_closed && s.error.is_none();
            s.clse_sent = true;
            need
        };
        if need_clse {
            self.send_clse();
        }
        self.shared.remove_stream(self.stream.local_id);
    }
}
