//! One live mirroring session: push scrcpy-server, start it, connect to its
//! video socket through the forward tunnel and demux the stream.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use xrmc_adb::{AdbDevice, AdbStream};

use crate::demux::{DEFAULT_MAX_PACKET_SIZE, DemuxEvent, Demuxer, MediaPacket};
use crate::options::{Scid, ServerOptions, VideoCodec, codec_id_name};
use crate::{Error, REMOTE_SERVER_PATH};

/// What to do with media packets when the event consumer falls behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backpressure {
    /// Drop packets while the event queue is full and resume at the next key
    /// frame, so latency never builds up. Config packets are never dropped.
    #[default]
    SkipToKeyFrame,
    /// Never drop: stop reading the socket until the consumer catches up
    /// (the device's encoder then stalls).
    Wait,
}

/// Session tuning. The defaults match the Electron app.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Capacity of the event channel (events, not bytes).
    pub event_capacity: usize,
    /// Behaviour when the event channel is full.
    pub backpressure: Backpressure,
    /// How long to keep retrying the video socket after starting the server.
    pub connect_timeout: Duration,
    /// Time limit for pushing the jar and starting the shell.
    pub push_timeout: Duration,
    /// Largest accepted media packet (protects against corrupt streams).
    pub max_packet_size: usize,
    /// Server log lines kept for error messages and [`SessionHandle::log_tail`].
    pub log_lines: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            event_capacity: 64,
            backpressure: Backpressure::SkipToKeyFrame,
            connect_timeout: Duration::from_secs(6),
            push_timeout: Duration::from_secs(30),
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            log_lines: 40,
        }
    }
}

/// Events from a running session, in stream order (log lines may interleave).
#[derive(Debug)]
pub enum SessionEvent {
    /// The device name from the stream's device meta.
    DeviceName(String),
    /// The stream codec (always the requested one; a mismatch ends the session).
    Codec(VideoCodec),
    /// A new capture session with this encoded frame size (sent at start and
    /// on every rotation / resize).
    VideoSize {
        /// Encoded width in pixels.
        width: u32,
        /// Encoded height in pixels.
        height: u32,
        /// The size changed because the client asked for it.
        client_resized: bool,
    },
    /// An encoded media packet.
    Packet(MediaPacket),
    /// A line of scrcpy-server output. Best effort: dropped when the event
    /// queue is full (see [`SessionHandle::log_tail`]).
    Log(String),
    /// The session is over: `None` after [`SessionHandle::stop`], otherwise
    /// the reason. Always the last event.
    Ended(Option<Error>),
}

/// Counters for a running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionStats {
    /// Bytes received on the video socket (after the dummy byte).
    pub bytes_received: u64,
    /// Media packets delivered as events.
    pub packets: u64,
    /// Media packets dropped because the consumer was too slow.
    pub packets_dropped: u64,
}

/// Starts sessions. See [`Session::start`].
#[derive(Debug, Clone, Copy)]
pub struct Session;

impl Session {
    /// Start mirroring with the default [`SessionConfig`].
    ///
    /// Pushes `jar` (the scrcpy-server v5.0.1 file) to [`REMOTE_SERVER_PATH`]
    /// (mode 0644), starts the server through a shell stream, then opens
    /// `localabstract:scrcpy_<scid>`, retrying until the server's dummy byte
    /// arrives (up to [`SessionConfig::connect_timeout`]). Returns once the
    /// video socket is connected; failures before that are returned
    /// directly, with the server's last error log line when it reported one.
    ///
    /// Dropping the returned future cancels the start and closes everything.
    pub async fn start<D: AdbDevice>(
        device: &D,
        jar: &[u8],
        options: ServerOptions,
    ) -> Result<(SessionHandle, mpsc::Receiver<SessionEvent>), Error> {
        Self::start_with(device, jar, options, SessionConfig::default()).await
    }

    /// [`Session::start`] with explicit tuning.
    pub async fn start_with<D: AdbDevice>(
        device: &D,
        jar: &[u8],
        options: ServerOptions,
        config: SessionConfig,
    ) -> Result<(SessionHandle, mpsc::Receiver<SessionEvent>), Error> {
        let (events, rx) = mpsc::channel(config.event_capacity.max(1));
        let (stop_tx, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            stop: stop_tx,
            log: Mutex::new(VecDeque::new()),
            log_cap: config.log_lines.max(1),
            bytes: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let scid = options.scid;

        tokio::time::timeout(
            config.push_timeout,
            device.push(jar, REMOTE_SERVER_PATH, 0o644),
        )
        .await
        .map_err(|_| Error::Timeout("pushing scrcpy-server"))??;
        let command = options.command();
        tracing::debug!(%scid, %command, "starting scrcpy-server");
        let shell = tokio::time::timeout(config.push_timeout, device.shell_stream(&command))
            .await
            .map_err(|_| Error::Timeout("starting scrcpy-server"))??;

        let (exited_tx, exited_rx) = watch::channel(false);
        let mut shell_task = AbortOnDrop(Some(tokio::spawn(read_server_log(
            shell,
            shared.clone(),
            events.clone(),
            shared.stop.subscribe(),
            exited_tx,
        ))));

        let connected = connect(
            device,
            &scid.service(),
            config.connect_timeout,
            exited_rx.clone(),
        )
        .await;
        let (video, rest) = match connected {
            Ok(v) => v,
            Err(failure) => {
                shared.stop.send_replace(true);
                shell_task.finish().await;
                return Err(match failure {
                    ConnectFailure::ServerExited => shared
                        .server_error("scrcpy-server exited before the video stream connected"),
                    ConnectFailure::Timeout(reason) => match shared.last_error_line() {
                        Some(line) => shared.server_error(&line),
                        None => Error::Connect(reason),
                    },
                });
            }
        };
        tracing::debug!(%scid, "video socket connected");

        let (finished_tx, finished_rx) = watch::channel(false);
        let run = Run {
            shared: shared.clone(),
            events,
            stop: shared.stop.subscribe(),
            exited: exited_rx,
            codec: options.codec,
            backpressure: config.backpressure,
            skipping: false,
        };
        let shell_task = shell_task.0.take();
        tokio::spawn(run.run(video, rest, config.max_packet_size, shell_task, finished_tx));
        Ok((
            SessionHandle {
                shared,
                finished: finished_rx,
                scid,
            },
            rx,
        ))
    }
}

/// Controls a running session. Dropping it stops the session.
#[derive(Debug)]
pub struct SessionHandle {
    shared: Arc<Shared>,
    finished: watch::Receiver<bool>,
    scid: Scid,
}

impl SessionHandle {
    /// The session's connection id.
    pub fn scid(&self) -> Scid {
        self.scid
    }

    /// Ask the session to stop without waiting for it.
    pub fn request_stop(&self) {
        self.shared.stop.send_replace(true);
    }

    /// Stop the session: closes the video stream and the server's shell
    /// stream (which ends the server), then emits [`SessionEvent::Ended`]`(None)`
    /// (unless it had already ended). Returns once the session is over.
    pub async fn stop(&self) {
        self.request_stop();
        self.wait().await;
    }

    /// Wait until the session is over (stopped or failed).
    pub async fn wait(&self) {
        let mut finished = self.finished.clone();
        // An error means the task is gone, which also means it is over.
        let _ = finished.wait_for(|done| *done).await;
    }

    /// Whether the session is over.
    pub fn is_finished(&self) -> bool {
        *self.finished.borrow() || self.finished.has_changed().is_err()
    }

    /// Traffic counters.
    pub fn stats(&self) -> SessionStats {
        SessionStats {
            bytes_received: self.shared.bytes.load(Ordering::Relaxed),
            packets: self.shared.packets.load(Ordering::Relaxed),
            packets_dropped: self.shared.dropped.load(Ordering::Relaxed),
        }
    }

    /// The most recent server log lines.
    pub fn log_tail(&self) -> Vec<String> {
        self.shared.log_tail()
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.request_stop();
    }
}

#[derive(Debug)]
struct Shared {
    stop: watch::Sender<bool>,
    log: Mutex<VecDeque<String>>,
    log_cap: usize,
    bytes: AtomicU64,
    packets: AtomicU64,
    dropped: AtomicU64,
}

impl Shared {
    fn log_tail(&self) -> Vec<String> {
        self.log
            .lock()
            .map(|l| l.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn push_log(&self, line: String) {
        if let Ok(mut log) = self.log.lock() {
            if log.len() >= self.log_cap {
                log.pop_front();
            }
            log.push_back(line);
        }
    }

    /// The last log line that looks like an error (as the Electron app's `lastError()`).
    fn last_error_line(&self) -> Option<String> {
        self.log_tail().into_iter().rev().find(|l| is_error_line(l))
    }

    /// [`Error::Server`] with the last error line, else the last line, else `fallback`.
    fn server_error(&self, fallback: &str) -> Error {
        let log = self.log_tail();
        let message = log
            .iter()
            .rev()
            .find(|l| is_error_line(l))
            .or(log.last())
            .cloned()
            .unwrap_or_else(|| fallback.to_owned());
        Error::Server { message, log }
    }
}

fn is_error_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("error") || lower.contains("exception")
}

/// Aborts the task when dropped (e.g. when `start` is cancelled).
struct AbortOnDrop(Option<JoinHandle<()>>);

impl AbortOnDrop {
    /// Wait briefly for the task to end by itself, then abort it.
    async fn finish(&mut self) {
        if let Some(task) = self.0.take() {
            join_or_abort(task).await;
        }
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

async fn join_or_abort(mut task: JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_secs(2), &mut task)
        .await
        .is_err()
    {
        task.abort();
    }
}

/// Reads the server's output into the log until it exits (sets `exited`) or
/// the session stops (closes the shell stream, which ends the server).
async fn read_server_log<S: AdbStream>(
    mut shell: S,
    shared: Arc<Shared>,
    events: mpsc::Sender<SessionEvent>,
    mut stop: watch::Receiver<bool>,
    exited: watch::Sender<bool>,
) {
    const MAX_LINE: usize = 16 * 1024;
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 4096];
    let emit = |raw: &[u8]| {
        let text = String::from_utf8_lossy(raw);
        let line = text.trim();
        if line.is_empty() {
            return;
        }
        tracing::debug!(target: "scrcpy_server", "{line}");
        shared.push_log(line.to_owned());
        let _ = events.try_send(SessionEvent::Log(line.to_owned()));
    };
    loop {
        tokio::select! {
            biased;
            _ = raised(&mut stop) => {
                let _ = tokio::time::timeout(Duration::from_secs(1), shell.shutdown()).await;
                return;
            }
            r = shell.read(&mut chunk) => match r {
                Ok(n) if n > 0 => {
                    pending.extend_from_slice(&chunk[..n]);
                    let mut start = 0;
                    while let Some(pos) = pending[start..].iter().position(|&b| b == b'\n') {
                        emit(&pending[start..start + pos]);
                        start += pos + 1;
                    }
                    pending.drain(..start);
                    if pending.len() > MAX_LINE {
                        emit(&pending);
                        pending.clear();
                    }
                }
                _ => {
                    emit(&pending);
                    exited.send_replace(true);
                    return;
                }
            }
        }
    }
}

/// Resolves once the flag is raised (or its sender is gone).
async fn raised(flag: &mut watch::Receiver<bool>) {
    let _ = flag.wait_for(|v| *v).await;
}

enum ConnectFailure {
    ServerExited,
    Timeout(String),
}

/// With the forward tunnel the device listens: retry opening the socket
/// until the server accepts us and sends its dummy byte.
async fn connect<D: AdbDevice>(
    device: &D,
    service: &str,
    timeout: Duration,
    mut exited: watch::Receiver<bool>,
) -> Result<(D::Stream, BytesMut), ConnectFailure> {
    let deadline = Instant::now() + timeout;
    let mut last_error = String::from("the server did not accept the connection");
    let mut attempt = 0u32;
    loop {
        let result = tokio::select! {
            biased;
            _ = raised(&mut exited) => return Err(ConnectFailure::ServerExited),
            r = tokio::time::timeout_at(deadline, connect_once(device, service)) => r,
        };
        match result {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) => {
                tracing::trace!(attempt, "video socket not ready: {e}");
                last_error = e.to_string();
            }
            Err(_) => return Err(ConnectFailure::Timeout(last_error)),
        }
        let delay = Duration::from_millis(if attempt < 10 { 100 } else { 250 });
        attempt = attempt.saturating_add(1);
        if Instant::now() + delay >= deadline {
            return Err(ConnectFailure::Timeout(last_error));
        }
        tokio::select! {
            biased;
            _ = raised(&mut exited) => return Err(ConnectFailure::ServerExited),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Open the video socket and wait for the dummy byte. Returns the stream and
/// whatever arrived after the dummy byte.
async fn connect_once<D: AdbDevice>(
    device: &D,
    service: &str,
) -> Result<(D::Stream, BytesMut), Error> {
    let mut stream = device.open(service).await?;
    let mut buf = BytesMut::with_capacity(64 * 1024);
    if stream.read_buf(&mut buf).await? == 0 {
        return Err(Error::Connect(
            "the stream closed before the server's dummy byte".to_owned(),
        ));
    }
    let _dummy = buf.split_to(1);
    Ok((stream, buf))
}

/// Why the stream loop ended.
enum Exit {
    Stopped,
    Failed(Error),
}

struct Run {
    shared: Arc<Shared>,
    events: mpsc::Sender<SessionEvent>,
    stop: watch::Receiver<bool>,
    exited: watch::Receiver<bool>,
    codec: VideoCodec,
    backpressure: Backpressure,
    skipping: bool,
}

impl Run {
    async fn run<S: AdbStream>(
        mut self,
        mut video: S,
        rest: BytesMut,
        max_packet_size: usize,
        shell_task: Option<JoinHandle<()>>,
        finished: watch::Sender<bool>,
    ) {
        let exit = self.stream(&mut video, rest, max_packet_size).await;
        // Close the video stream first, then the shell stream (ends the server).
        let _ = tokio::time::timeout(Duration::from_secs(1), video.shutdown()).await;
        drop(video);
        self.shared.stop.send_replace(true);
        if let Some(task) = shell_task {
            join_or_abort(task).await;
        }
        let error = match exit {
            Exit::Stopped => None,
            Exit::Failed(e) => {
                tracing::debug!("session ended: {e}");
                Some(e)
            }
        };
        // Over before the (possibly blocking) Ended event, so stop() never
        // waits on a consumer that is not reading.
        finished.send_replace(true);
        let _ = self.events.send(SessionEvent::Ended(error)).await;
    }

    async fn stream<S: AdbStream>(
        &mut self,
        video: &mut S,
        rest: BytesMut,
        max_packet_size: usize,
    ) -> Exit {
        let mut demux = Demuxer::with_max_packet_size(max_packet_size);
        self.shared
            .bytes
            .fetch_add(rest.len() as u64, Ordering::Relaxed);
        demux.buffer_mut().unsplit(rest);
        loop {
            loop {
                match demux.next_event() {
                    Ok(Some(ev)) => {
                        if let Err(exit) = self.dispatch(ev).await {
                            return exit;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => return Exit::Failed(e.into()),
                }
            }
            let want = demux.bytes_needed().clamp(64 * 1024, 4 << 20);
            demux.buffer_mut().reserve(want);
            let read = tokio::select! {
                biased;
                _ = raised(&mut self.stop) => return Exit::Stopped,
                _ = self.events.closed() => return Exit::Stopped,
                _ = raised(&mut self.exited) => {
                    return Exit::Failed(self.shared.server_error("scrcpy-server exited"));
                }
                r = video.read_buf(demux.buffer_mut()) => r,
            };
            match read {
                Ok(0) => return self.closed(None).await,
                Ok(n) => {
                    self.shared.bytes.fetch_add(n as u64, Ordering::Relaxed);
                }
                Err(e) => return self.closed(Some(e)).await,
            }
        }
    }

    /// The video stream ended on its own: give the server a moment to print
    /// why and exit, then prefer its error message.
    async fn closed(&mut self, io: Option<std::io::Error>) -> Exit {
        let grace = tokio::time::sleep(Duration::from_millis(500));
        tokio::select! {
            biased;
            _ = raised(&mut self.stop) => return Exit::Stopped,
            _ = raised(&mut self.exited) => {}
            _ = grace => {}
        }
        Exit::Failed(match (self.shared.last_error_line(), io) {
            (Some(line), _) => self.shared.server_error(&line),
            (None, Some(e)) => Error::Io(e),
            (None, None) => Error::Closed,
        })
    }

    async fn dispatch(&mut self, ev: DemuxEvent) -> Result<(), Exit> {
        match ev {
            DemuxEvent::DeviceName(name) => self.send(SessionEvent::DeviceName(name)).await,
            DemuxEvent::Codec(id) => match VideoCodec::from_id(id) {
                Some(c) if c == self.codec => self.send(SessionEvent::Codec(c)).await,
                _ => Err(Exit::Failed(Error::CodecMismatch {
                    requested: self.codec,
                    received: codec_id_name(id),
                })),
            },
            DemuxEvent::Session {
                width,
                height,
                client_resized,
            } => {
                self.send(SessionEvent::VideoSize {
                    width,
                    height,
                    client_resized,
                })
                .await
            }
            DemuxEvent::Packet(p) => self.deliver(p).await,
        }
    }

    async fn deliver(&mut self, p: MediaPacket) -> Result<(), Exit> {
        if self.backpressure == Backpressure::Wait || p.config {
            self.send(SessionEvent::Packet(p)).await?;
            self.shared.packets.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if self.skipping && !p.key {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        match self.events.try_send(SessionEvent::Packet(p)) {
            Ok(()) => {
                self.skipping = false;
                self.shared.packets.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if !self.skipping {
                    tracing::debug!("event queue full: dropping packets until the next key frame");
                }
                self.skipping = true;
                self.shared.dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => Err(Exit::Stopped),
        }
    }

    /// Send an event, giving up if the session is stopped meanwhile or nobody listens anymore.
    async fn send(&mut self, ev: SessionEvent) -> Result<(), Exit> {
        tokio::select! {
            biased;
            _ = raised(&mut self.stop) => Err(Exit::Stopped),
            r = self.events.send(ev) => r.map_err(|_| Exit::Stopped),
        }
    }
}
