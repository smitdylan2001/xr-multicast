//! Stream lifecycle and policy (port of `src/main/stream-manager.js`).
//!
//! Each running stream has a driver task that owns its state machine:
//!
//! ```text
//!   start ─► starting ─► live ──(resize)──► resizing ─► starting ─► live …
//!               │          │
//!               │          └─(ended)─► reconnecting ─(1.5 s × n)─► starting …
//!               └─(no video)─► 10-bit → 8-bit → H.264 fallbacks
//!   stop ─► stopped        too many failures ─► error
//! ```
//!
//! The scrcpy session itself is behind [`StreamBackend`] / [`SessionControl`]
//! so the state machine can be tested with a fake. The decisions are pure
//! functions in [`policy`].

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::CoreError;
use crate::brands::Eye;
use crate::codec_policy::{
    self, CodecChoice, CodecFailures, DecoderSupport, DecoderSupportSet, EncoderInfo,
    TEN_BIT_CODEC_OPTION, quality_for, use_ten_bit,
};
use crate::config::{Codec, CodecPreference, QualityPreset, TransportKind, TransportPreference};
use crate::device_info::eye_crop;
use crate::device_manager::{DeviceDirectory, Transport};
use crate::events::{CoreEvent, EventSender, Notice, NoticeLevel, VideoPacket};
use crate::store::SharedStore;
use crate::view_size::{FrameSize, ViewRequest};

pub use policy::ViewSizes;

/// Something the session reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// The device name from the scrcpy device meta.
    Device(String),
    /// The encoder (re)configured: the encoded frame size.
    Session {
        width: u32,
        height: u32,
    },
    Packet(VideoPacket),
    /// A line of scrcpy-server output.
    Log(String),
    /// Why the session is about to end.
    Error(String),
    /// The session ended (closing the channel means the same).
    End,
}

/// What a session should stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    pub codec: Codec,
    /// `video_encoder=`; `None` lets the headset choose.
    pub encoder: Option<String>,
    /// Long-side limit (`max_size=`); 0 = native.
    pub max_size: u32,
    pub bit_rate: u32,
    pub max_fps: u32,
    /// `crop=w:h:x:y`.
    pub crop: Option<String>,
    /// Extra `video_codec_options` entries (e.g. `profile=2` for 10-bit).
    pub codec_options: Vec<String>,
}

/// Handle to a running session.
pub trait SessionControl: Send + Sync + 'static {
    /// Ask the session to stop. It must then end its event stream (send
    /// [`SessionEvent::End`] or drop the sender). Must be idempotent.
    fn stop(&self);
}

/// A started session and its events.
pub struct SessionStart<S> {
    pub session: S,
    pub events: mpsc::Receiver<SessionEvent>,
}

/// Starts scrcpy sessions on headsets (implemented with `xrmc-scrcpy` by the app).
pub trait StreamBackend: Send + Sync + 'static {
    type Session: SessionControl;

    /// The scrcpy-server binary is available.
    fn server_available(&self) -> bool;

    /// The headset's video encoders (scrcpy `list_encoders=true`).
    fn list_encoders(
        &self,
        adb_serial: &str,
    ) -> impl Future<Output = Result<Vec<EncoderInfo>, CoreError>> + Send;

    /// The natural display size (`wm size`), what scrcpy captures before scaling.
    fn panel_size(
        &self,
        adb_serial: &str,
    ) -> impl Future<Output = Result<Option<FrameSize>, CoreError>> + Send;

    /// Push the server, open the tunnel and start streaming. The returned
    /// future may be dropped (cancelled) when the user stops the stream.
    fn start_session(
        &self,
        adb_serial: &str,
        config: SessionConfig,
    ) -> impl Future<Output = Result<SessionStart<Self::Session>, CoreError>> + Send;
}

/// Lifecycle state of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamStatus {
    /// Created, transport/codec not chosen yet.
    Idle,
    Starting,
    Live,
    /// Restarting at a new resolution.
    Resizing,
    /// Restarting after the stream ended unexpectedly.
    Reconnecting,
    Error,
    Stopped,
}

impl StreamStatus {
    /// Starting, live, resizing or reconnecting.
    pub fn is_running(self) -> bool {
        matches!(
            self,
            StreamStatus::Starting
                | StreamStatus::Live
                | StreamStatus::Resizing
                | StreamStatus::Reconnecting
        )
    }
}

/// A stream's state for UIs (the Electron `stateOf` object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamState {
    /// Headset hardware serial.
    pub id: String,
    pub state: StreamStatus,
    pub transport: Option<TransportKind>,
    pub adb_serial: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub device_name: Option<String>,
    /// Human-readable quality, e.g. `Auto 960px · 2.8 Mbps (max: High …)`.
    pub quality: Option<String>,
    pub bit_rate: Option<u32>,
    pub codec: Option<Codec>,
    pub ten_bit: bool,
    pub encoder: Option<String>,
    pub single_eye: bool,
    pub retries: u32,
    /// `resize` while restarting at a new resolution.
    pub reason: Option<String>,
    /// Encoded long side the stream was started with.
    pub size_long: Option<u32>,
    pub auto_size: bool,
    /// Set in the `error` and `reconnecting` states.
    pub error: Option<String>,
}

/// Options for [`StreamManager::start`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StartOptions {
    pub transport: Option<TransportPreference>,
    /// Restart a running stream.
    pub restart: bool,
    /// Force a codec.
    pub codec: Option<Codec>,
    /// The viewer could not decode the stream: fall back (10-bit → 8-bit, else → H.264).
    pub decode_failed: bool,
}

/// Result of one [`StreamManager::start_all`] start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartResult {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Encoders for the UI (`{ live, encoders }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodersReply {
    /// Queried from the connected headset now (vs. last known).
    pub live: bool,
    pub encoders: Option<Vec<EncoderInfo>>,
}

/// Timings and limits (defaults match the Electron app).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamManagerConfig {
    pub max_retries: u32,
    /// Retry n waits n × this.
    pub retry_base_delay: Duration,
    /// Wait before a codec / bit-depth fallback restart.
    pub fallback_delay: Duration,
    pub resize_debounce: Duration,
    /// Don't re-evaluate size right after a (re)start: the first frames settle the layout.
    pub resize_settle: Duration,
    pub down_delay: Duration,
    pub hidden_delay: Duration,
    /// How long to wait for a session to end after asking it to stop.
    pub stop_timeout: Duration,
}

impl Default for StreamManagerConfig {
    fn default() -> Self {
        StreamManagerConfig {
            max_retries: 5,
            retry_base_delay: Duration::from_millis(1500),
            fallback_delay: Duration::from_millis(300),
            resize_debounce: Duration::from_millis(1000),
            resize_settle: Duration::from_millis(2500),
            down_delay: Duration::from_millis(5000),
            hidden_delay: Duration::from_millis(15000),
            stop_timeout: Duration::from_secs(5),
        }
    }
}

/// Pure decisions of the stream manager.
pub mod policy {
    use std::collections::{BTreeMap, HashMap};

    use super::StreamStatus;
    use crate::config::{Codec, QualityPreset};
    use crate::view_size::{
        FrameSize, ResizeDecision, ResizeQuery, ViewRequest, decide_resize, needed_long_side,
        pick_step, scaled_bitrate,
    };

    /// What each viewer reported for each stream.
    #[derive(Debug, Clone, Default, PartialEq)]
    pub struct ViewSizes {
        by_stream: HashMap<String, BTreeMap<String, ViewRequest>>,
    }

    impl ViewSizes {
        /// A viewer's report: stream id → requirement, or `None` when it no longer shows the stream.
        /// Returns the affected stream ids.
        pub fn set(
            &mut self,
            viewer: &str,
            sizes: impl IntoIterator<Item = (String, Option<ViewRequest>)>,
        ) -> Vec<String> {
            let mut affected = Vec::new();
            for (id, req) in sizes {
                let m = self.by_stream.entry(id.clone()).or_default();
                match req {
                    Some(r) => {
                        m.insert(viewer.to_string(), r);
                    }
                    None => {
                        m.remove(viewer);
                    }
                }
                if m.is_empty() {
                    self.by_stream.remove(&id);
                }
                affected.push(id);
            }
            affected
        }

        /// Forget everything a viewer reported (it disconnected). Returns the affected stream ids.
        pub fn clear_viewer(&mut self, viewer: &str) -> Vec<String> {
            let mut affected = Vec::new();
            self.by_stream.retain(|id, m| {
                if m.remove(viewer).is_some() {
                    affected.push(id.clone());
                }
                !m.is_empty()
            });
            affected
        }

        pub fn requests(&self, stream: &str) -> Vec<ViewRequest> {
            self.by_stream
                .get(stream)
                .map(|m| m.values().cloned().collect())
                .unwrap_or_default()
        }

        pub fn viewer_count(&self, stream: &str) -> usize {
            self.by_stream.get(stream).map_or(0, BTreeMap::len)
        }
    }

    /// `w:h` of a `crop=w:h:x:y` value.
    pub fn parse_crop(crop: &str) -> Option<FrameSize> {
        let mut it = crop.split(':');
        Some(FrameSize {
            w: it.next()?.trim().parse().ok()?,
            h: it.next()?.trim().parse().ok()?,
        })
    }

    /// Frame size in natural orientation for a stream: the crop if any, else the
    /// panel, else the encoded size (proportional, which is all that's needed).
    pub fn frame_size_for(
        crop: Option<&str>,
        panel: Option<FrameSize>,
        encoded: Option<FrameSize>,
    ) -> Option<FrameSize> {
        if let Some(c) = crop {
            return parse_crop(c);
        }
        panel.or(encoded.filter(|e| e.w > 0 && e.h > 0))
    }

    /// Target encoded size from all viewers' requirements.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Target {
        pub size: u32,
        pub native_long: u32,
        /// No viewer shows the stream.
        pub hidden_only: bool,
    }

    /// `None` = no information (use the preset).
    pub fn target_size(
        reqs: &[ViewRequest],
        preset: &QualityPreset,
        frame: Option<FrameSize>,
    ) -> Option<Target> {
        let frame = frame?;
        if reqs.is_empty() {
            return None;
        }
        let native_long = frame.long_side();
        let hidden_only = reqs.iter().all(|r| !r.visible);
        let need = reqs
            .iter()
            .map(|r| needed_long_side(frame, r))
            .fold(0.0_f64, f64::max);
        Some(Target {
            size: pick_step(need, preset.max_size, native_long),
            native_long,
            hidden_only,
        })
    }

    /// Encoder size and bitrate a (re)start uses.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct LaunchSize {
        /// scrcpy `max_size` (0 = native).
        pub max_size: u32,
        pub bit_rate: u32,
        /// Encoded long side (0 = unknown).
        pub size_long: u32,
    }

    /// Match the encoded size to how the stream is shown (falls back to the preset).
    pub fn launch_size(
        preset: &QualityPreset,
        auto_size: bool,
        frame: Option<FrameSize>,
        reqs: &[ViewRequest],
    ) -> LaunchSize {
        if !auto_size {
            return LaunchSize {
                max_size: preset.max_size,
                bit_rate: preset.bit_rate,
                size_long: preset.max_size,
            };
        }
        match target_size(reqs, preset, frame) {
            Some(t) => LaunchSize {
                max_size: if t.size >= t.native_long { 0 } else { t.size },
                bit_rate: scaled_bitrate(
                    preset.bit_rate,
                    t.size,
                    if preset.max_size > 0 {
                        preset.max_size
                    } else {
                        t.native_long
                    },
                ),
                size_long: t.size,
            },
            None => LaunchSize {
                max_size: preset.max_size,
                bit_rate: preset.bit_rate,
                size_long: if preset.max_size > 0 {
                    preset.max_size
                } else {
                    frame.map_or(0, |f| f.long_side())
                },
            },
        }
    }

    /// The `quality` string of a stream state.
    pub fn quality_label(
        auto_size: bool,
        max_size: u32,
        bit_rate: u32,
        preset: &QualityPreset,
    ) -> String {
        if !auto_size {
            return preset.label.clone();
        }
        let size = if max_size > 0 {
            format!("{max_size}px")
        } else {
            "native".to_string()
        };
        let mbps = crate::js::to_fixed(f64::from(bit_rate) / 1e6, 1);
        format!("Auto {size} · {mbps} Mbps (max: {})", preset.label)
    }

    /// Inputs of [`evaluate_resize`]. Times in ms on one monotonic clock.
    #[derive(Debug, Clone, Copy)]
    pub struct ResizeContext<'a> {
        pub auto_size: bool,
        pub user_stopped: bool,
        pub resizing: bool,
        pub state: StreamStatus,
        pub got_media: bool,
        pub live_since: Option<u64>,
        pub now: u64,
        /// Current encoded long side (0/None = native).
        pub size_long: Option<u32>,
        pub down_since: Option<u64>,
        pub preset: &'a QualityPreset,
        pub frame: Option<FrameSize>,
        pub reqs: &'a [ViewRequest],
        pub debounce_ms: u64,
        pub settle_ms: u64,
        pub down_delay_ms: u64,
        pub hidden_delay_ms: u64,
    }

    /// What to do after evaluating a stream's size.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ResizeStep {
        /// Nothing (resizing off, stopped, already resizing, or no information).
        Ignore,
        /// Not live/settled yet: evaluate again after this many ms.
        Reschedule(u64),
        /// Size is fine: clear any pending downgrade.
        Keep,
        /// A downgrade is pending: remember since when, and evaluate again after this many ms.
        Wait(u64),
        /// Restart at this long side.
        Resize(u32),
    }

    /// Restart a live stream at a new resolution, but only when it actually needs one (`evaluateResize`).
    pub fn evaluate_resize(c: &ResizeContext<'_>) -> ResizeStep {
        if !c.auto_size || c.user_stopped || c.resizing {
            return ResizeStep::Ignore;
        }
        if c.state != StreamStatus::Live || !c.got_media {
            return ResizeStep::Reschedule(c.debounce_ms);
        }
        let settled = c.now.saturating_sub(c.live_since.unwrap_or(0));
        if settled < c.settle_ms {
            return ResizeStep::Reschedule(c.settle_ms - settled);
        }
        let Some(t) = target_size(c.reqs, c.preset, c.frame) else {
            return ResizeStep::Ignore;
        };
        let current = c.size_long.filter(|&s| s > 0).unwrap_or(t.native_long);
        let d = decide_resize(&ResizeQuery {
            current,
            target: t.size,
            native_long: t.native_long,
            now: c.now,
            down_since: c.down_since,
            hidden_only: t.hidden_only,
            down_delay: c.down_delay_ms,
            hidden_delay: c.hidden_delay_ms,
        });
        match d {
            ResizeDecision::None => ResizeStep::Keep,
            ResizeDecision::Wait { wait_ms } => ResizeStep::Wait(wait_ms + 50),
            ResizeDecision::Resize => ResizeStep::Resize(t.size),
        }
    }

    /// State of a stream when its session ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct EndContext {
        pub user_stopped: bool,
        pub replaced: bool,
        pub resizing: bool,
        /// The entry is still the stream's current one.
        pub is_current: bool,
        /// The headset is still connected.
        pub online: bool,
        pub ten_bit: bool,
        pub got_media: bool,
        pub codec: Option<Codec>,
        pub auto_retry: bool,
        pub retries: u32,
        pub max_retries: u32,
    }

    /// What to do when a session ended.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum EndAction {
        /// The user stopped it; emit `stopped` unless the stream was replaced.
        Stopped { emit: bool },
        /// Planned restart at a new resolution: relaunch right away.
        Relaunch,
        /// 10-bit refused by the encoder: retry the same codec in 8-bit.
        TenBitFallback,
        /// A non-H.264 stream died before producing video: fall back to H.264.
        CodecFallback,
        /// Retry (attempt number, 1-based) after `attempt × retry delay`.
        Retry { attempt: u32 },
        /// Give up: `error`.
        Fail,
    }

    /// The end-of-session decision (the `end` handler in stream-manager.js).
    pub fn decide_on_end(c: &EndContext) -> EndAction {
        if c.user_stopped {
            return EndAction::Stopped { emit: !c.replaced };
        }
        if c.resizing && c.is_current {
            return EndAction::Relaunch;
        }
        if c.online && c.ten_bit && !c.got_media {
            return EndAction::TenBitFallback;
        }
        if c.online && c.codec.is_some_and(|k| k != Codec::H264) && !c.got_media {
            return EndAction::CodecFallback;
        }
        if c.auto_retry && c.online && c.retries < c.max_retries {
            return EndAction::Retry {
                attempt: c.retries + 1,
            };
        }
        EndAction::Fail
    }
}

use policy::{EndAction, EndContext, LaunchSize, ResizeContext, ResizeStep};

#[derive(Debug, Clone)]
struct EntryData {
    serial: String,
    preference: TransportPreference,
    codec_override: Option<Codec>,
    retries: u32,
    user_stopped: bool,
    replaced: bool,
    state: StreamStatus,
    transport: Option<TransportKind>,
    adb_serial: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    device_name: Option<String>,
    quality: Option<String>,
    bit_rate: Option<u32>,
    codec: Option<Codec>,
    ten_bit: bool,
    encoder: Option<String>,
    crop: Option<String>,
    reason: Option<String>,
    size_long: Option<u32>,
    auto_size: bool,
    error: Option<String>,
    force_eight_bit: bool,
    noted: bool,
    got_media: bool,
    live_since: Option<u64>,
    down_since: Option<u64>,
    resizing: bool,
}

impl EntryData {
    fn new(serial: &str, preference: TransportPreference, codec_override: Option<Codec>) -> Self {
        EntryData {
            serial: serial.to_string(),
            preference,
            codec_override,
            retries: 0,
            user_stopped: false,
            replaced: false,
            state: StreamStatus::Idle,
            transport: None,
            adb_serial: None,
            width: None,
            height: None,
            device_name: None,
            quality: None,
            bit_rate: None,
            codec: None,
            ten_bit: false,
            encoder: None,
            crop: None,
            reason: None,
            size_long: None,
            auto_size: false,
            error: None,
            force_eight_bit: false,
            noted: false,
            got_media: false,
            live_since: None,
            down_since: None,
            resizing: false,
        }
    }

    fn state(&self) -> StreamState {
        StreamState {
            id: self.serial.clone(),
            state: self.state,
            transport: self.transport,
            adb_serial: self.adb_serial.clone(),
            width: self.width.filter(|&w| w > 0),
            height: self.height.filter(|&h| h > 0),
            device_name: self.device_name.clone(),
            quality: self.quality.clone(),
            bit_rate: self.bit_rate.filter(|&b| b > 0),
            codec: self.codec,
            ten_bit: self.ten_bit,
            encoder: self.encoder.clone(),
            single_eye: self.crop.is_some(),
            retries: self.retries,
            reason: self.reason.clone(),
            size_long: self.size_long.filter(|&s| s > 0),
            auto_size: self.auto_size,
            error: if matches!(self.state, StreamStatus::Error | StreamStatus::Reconnecting) {
                self.error.clone()
            } else {
                None
            },
        }
    }

    fn encoded(&self) -> Option<FrameSize> {
        Some(FrameSize {
            w: self.width?,
            h: self.height?,
        })
    }
}

enum Command {
    Stop {
        replaced: bool,
        done: oneshot::Sender<()>,
    },
    ScheduleResize(Duration),
}

struct EntryHandle {
    id: u64,
    serial: String,
    data: Mutex<EntryData>,
    cmd: mpsc::UnboundedSender<Command>,
}

impl EntryHandle {
    /// Lock order: the entries map, then an entry's data, then the caches. Never
    /// take the map (e.g. `is_current`) while holding an entry's data.
    fn data(&self) -> MutexGuard<'_, EntryData> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

struct Prepared {
    transport: Transport,
    preset: QualityPreset,
    size: LaunchSize,
    choice: CodecChoice,
    ten_bit: bool,
    crop: Option<String>,
    auto_size: bool,
}

enum Prep {
    Ready(Box<Prepared>),
    /// Finish with this error.
    Finish(String),
    /// Stopped or replaced meanwhile.
    Abort,
}

enum Phase<S> {
    /// Waiting to (re)launch.
    Idle {
        relaunch_at: Instant,
    },
    /// Choosing transport, size and codec.
    Preparing(BoxFuture<Prep>),
    /// Starting the session.
    Starting(BoxFuture<Result<SessionStart<S>, CoreError>>),
    Running {
        session: S,
        events: mpsc::Receiver<SessionEvent>,
    },
    Done,
}

enum PhaseEvent<S> {
    RelaunchDue,
    Prepared(Prep),
    Started(Result<SessionStart<S>, CoreError>),
    Session(SessionEvent),
    SessionEnded,
}

async fn phase_event<S>(phase: &mut Phase<S>) -> PhaseEvent<S> {
    match phase {
        Phase::Idle { relaunch_at } => {
            tokio::time::sleep_until(*relaunch_at).await;
            PhaseEvent::RelaunchDue
        }
        Phase::Preparing(f) => PhaseEvent::Prepared(f.as_mut().await),
        Phase::Starting(f) => PhaseEvent::Started(f.as_mut().await),
        Phase::Running { events, .. } => match events.recv().await {
            Some(SessionEvent::End) | None => PhaseEvent::SessionEnded,
            Some(e) => PhaseEvent::Session(e),
        },
        Phase::Done => std::future::pending().await,
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

enum DriverEvent<S> {
    Command(Option<Command>),
    StopTimeout,
    ResizeDue,
    Phase(PhaseEvent<S>),
}

struct SmInner<B, D> {
    backend: B,
    devices: D,
    store: SharedStore,
    events: EventSender,
    config: StreamManagerConfig,
    epoch: Instant,
    next_id: AtomicU64,
    entries: Mutex<BTreeMap<String, Arc<EntryHandle>>>,
    decoders: Mutex<DecoderSupportSet>,
    failures: Mutex<CodecFailures>,
    encoder_cache: Mutex<HashMap<String, Vec<EncoderInfo>>>,
    panel_sizes: Mutex<HashMap<String, FrameSize>>,
    view_sizes: Mutex<ViewSizes>,
    /// Serialises `start` calls so two starts can't both create an entry.
    start_lock: tokio::sync::Mutex<()>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs the streams of all headsets. Cheap to clone (shared handle).
pub struct StreamManager<B: StreamBackend, D: DeviceDirectory> {
    inner: Arc<SmInner<B, D>>,
}

impl<B: StreamBackend, D: DeviceDirectory> Clone for StreamManager<B, D> {
    fn clone(&self) -> Self {
        StreamManager {
            inner: self.inner.clone(),
        }
    }
}

impl<B: StreamBackend, D: DeviceDirectory> StreamManager<B, D> {
    pub fn new(
        backend: B,
        devices: D,
        store: SharedStore,
        events: EventSender,
        config: StreamManagerConfig,
    ) -> Self {
        StreamManager {
            inner: Arc::new(SmInner {
                backend,
                devices,
                store,
                events,
                config,
                epoch: Instant::now(),
                next_id: AtomicU64::new(1),
                entries: Mutex::new(BTreeMap::new()),
                decoders: Mutex::new(DecoderSupportSet::new()),
                failures: Mutex::new(CodecFailures::new()),
                encoder_cache: Mutex::new(HashMap::new()),
                panel_sizes: Mutex::new(HashMap::new()),
                view_sizes: Mutex::new(ViewSizes::default()),
                start_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }

    pub fn backend(&self) -> &B {
        &self.inner.backend
    }

    pub fn server_available(&self) -> bool {
        self.inner.backend.server_available()
    }

    fn now_ms(&self) -> u64 {
        self.inner.epoch.elapsed().as_millis() as u64
    }

    fn notice(&self, level: NoticeLevel, message: impl Into<String>) {
        let _ = self
            .inner
            .events
            .send(CoreEvent::Notice(Notice::new(level, message)));
    }

    // ------------------------------------------------------ decoder support

    /// What the hub's own renderer can decode (`None` keeps the previous report).
    pub fn set_decoder_support(&self, support: Option<DecoderSupport>) {
        lock(&self.inner.decoders).set_local(support);
    }

    /// A remote viewer's decoders (`None` when it leaves). Streams must use a codec
    /// every connected viewer can play; incompatible ones are restarted.
    pub fn set_viewer_support(&self, client_id: &str, support: Option<DecoderSupport>) {
        let changed = lock(&self.inner.decoders).set_viewer(client_id, support);
        if changed {
            self.restart_incompatible_streams();
        }
    }

    /// The merged decoder support.
    pub fn decoder_support(&self) -> DecoderSupport {
        lock(&self.inner.decoders).merged().clone()
    }

    fn restart_incompatible_streams(&self) {
        let support = self.decoder_support();
        let handles: Vec<Arc<EntryHandle>> = lock(&self.inner.entries).values().cloned().collect();
        for h in handles {
            let (codec, preference, ok) = {
                let d = h.data();
                (
                    d.codec,
                    d.preference,
                    !d.user_stopped && d.state.is_running(),
                )
            };
            let Some(codec) = codec else { continue };
            if !ok || support.get(codec.as_str()).is_some_and(|c| c.any) {
                continue;
            }
            self.notice(
                NoticeLevel::Info,
                "Switching a stream to a codec every viewer can play.",
            );
            let this = self.clone();
            let serial = h.serial.clone();
            tokio::spawn(async move {
                let opts = StartOptions {
                    restart: true,
                    transport: Some(preference),
                    ..Default::default()
                };
                if let Err(e) = this.start(&serial, opts).await {
                    tracing::debug!("restart {serial}: {e}");
                }
            });
        }
    }

    // ------------------------------------------------------------- encoders

    async fn get_encoders(
        &self,
        serial: &str,
        adb_serial: &str,
        refresh: bool,
    ) -> Result<Vec<EncoderInfo>, CoreError> {
        if !refresh && let Some(list) = lock(&self.inner.encoder_cache).get(serial) {
            return Ok(list.clone());
        }
        let list = self.inner.backend.list_encoders(adb_serial).await?;
        lock(&self.inner.encoder_cache).insert(serial.to_string(), list.clone());
        self.inner
            .store
            .write(|s| s.modify_existing_headset(serial, |r| r.encoders = Some(list.clone())));
        Ok(list)
    }

    /// For the UI: encoders of a headset (live query if connected, else last known).
    pub async fn encoders_for(&self, serial: &str) -> Result<EncodersReply, CoreError> {
        if let Some(t) = self
            .inner
            .devices
            .pick_transport(serial, TransportPreference::Auto)
            && self.server_available()
        {
            let list = self.get_encoders(serial, &t.adb_serial, false).await?;
            return Ok(EncodersReply {
                live: true,
                encoders: Some(list),
            });
        }
        let stored = self.inner.store.headset(serial).and_then(|r| r.encoders);
        let encoders = stored.or_else(|| lock(&self.inner.encoder_cache).get(serial).cloned());
        Ok(EncodersReply {
            live: false,
            encoders,
        })
    }

    async fn panel_size(&self, adb_serial: &str) -> Option<FrameSize> {
        if let Some(s) = lock(&self.inner.panel_sizes).get(adb_serial) {
            return Some(*s);
        }
        let size = self
            .inner
            .backend
            .panel_size(adb_serial)
            .await
            .ok()
            .flatten()?;
        lock(&self.inner.panel_sizes).insert(adb_serial.to_string(), size);
        Some(size)
    }

    async fn resolve_codec(
        &self,
        serial: &str,
        codec_override: Option<Codec>,
        t: &Transport,
    ) -> CodecChoice {
        let pref = codec_override
            .map(CodecPreference::Fixed)
            .unwrap_or_else(|| {
                self.inner
                    .store
                    .read(|s| s.settings().codec_preference(t.kind))
            });
        let failed = lock(&self.inner.failures).failed_codecs(serial);
        // Can't enumerate (very old firmware?) — only H.264 is a safe bet.
        let encoders = self.get_encoders(serial, &t.adb_serial, false).await.ok();
        let decoders = self.decoder_support();
        codec_policy::resolve_codec(pref, encoders.as_deref(), &decoders, &failed)
    }

    // ------------------------------------------------- resolution matching

    /// A viewer reports how it shows each stream (`None` = no longer shown).
    pub fn set_view_sizes(
        &self,
        viewer_id: &str,
        sizes: impl IntoIterator<Item = (String, Option<ViewRequest>)>,
    ) {
        let affected = lock(&self.inner.view_sizes).set(viewer_id, sizes);
        self.schedule_resizes(&affected);
    }

    /// Forget everything a viewer reported (it disconnected).
    pub fn clear_viewer(&self, viewer_id: &str) {
        let affected = lock(&self.inner.view_sizes).clear_viewer(viewer_id);
        self.schedule_resizes(&affected);
    }

    fn schedule_resizes(&self, ids: &[String]) {
        let entries = lock(&self.inner.entries);
        for id in ids {
            if let Some(h) = entries.get(id) {
                let _ = h
                    .cmd
                    .send(Command::ScheduleResize(self.inner.config.resize_debounce));
            }
        }
    }

    fn view_requests(&self, serial: &str) -> Vec<ViewRequest> {
        lock(&self.inner.view_sizes).requests(serial)
    }

    // ------------------------------------------------------------ streams

    /// Start (or restart) casting a headset.
    pub async fn start(&self, serial: &str, opts: StartOptions) -> Result<(), CoreError> {
        if !self.server_available() {
            return Err(CoreError::ServerMissing);
        }
        let _guard = self.inner.start_lock.lock().await;
        let hs = self
            .inner
            .devices
            .headset(serial)
            .filter(|h| h.is_online())
            .ok_or(CoreError::NotConnected)?;
        if !hs.can_cast {
            return Err(CoreError::NotVerified {
                name: hs.name.clone(),
            });
        }
        let mut codec = opts.codec;
        let existing = lock(&self.inner.entries).get(serial).cloned();
        if let Some(e) = &existing {
            let (running, transport, prev_codec, ten_bit) = {
                let d = e.data();
                (
                    !d.user_stopped && d.state.is_running(),
                    d.transport,
                    d.codec,
                    d.ten_bit,
                )
            };
            if opts.decode_failed
                && let Some(c) = prev_codec
            {
                if ten_bit {
                    lock(&self.inner.failures).mark_ten_bit_failed(serial, c);
                    self.notice(
                        NoticeLevel::Warn,
                        format!(
                            "{}: 10-bit {} could not be decoded — switching to 8-bit.",
                            hs.name,
                            c.label()
                        ),
                    );
                    codec = codec.or(Some(c));
                } else if c != Codec::H264 {
                    lock(&self.inner.failures).mark_codec_failed(serial, c);
                    self.notice(
                        NoticeLevel::Warn,
                        format!(
                            "{}: this computer could not decode {} — switching to H.264.",
                            hs.name,
                            c.label()
                        ),
                    );
                    codec = Some(Codec::H264);
                }
            }
            let same_transport = opts
                .transport
                .is_none_or(|p| transport.map(TransportPreference::from) == Some(p));
            if running && !opts.restart && same_transport {
                return Ok(());
            }
            self.stop_entry(serial, true).await;
        }
        let preference = opts.transport.unwrap_or(hs.transport);
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = Arc::new(EntryHandle {
            id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
            serial: serial.to_string(),
            data: Mutex::new(EntryData::new(serial, preference, codec)),
            cmd: tx,
        });
        lock(&self.inner.entries).insert(serial.to_string(), handle.clone());
        tokio::spawn(self.clone().drive(handle, rx));
        Ok(())
    }

    /// Stop casting a headset; resolves once the stream has stopped.
    pub async fn stop(&self, serial: &str) {
        self.stop_entry(serial, false).await;
    }

    async fn stop_entry(&self, serial: &str, replaced: bool) {
        let Some(h) = lock(&self.inner.entries).get(serial).cloned() else {
            return;
        };
        let (done, wait) = oneshot::channel();
        if h.cmd.send(Command::Stop { replaced, done }).is_err() {
            // The driver is gone; just drop the entry.
            self.remove_if_current(&h);
            return;
        }
        let limit = self.inner.config.stop_timeout + Duration::from_secs(1);
        let _ = tokio::time::timeout(limit, wait).await;
    }

    /// Start every castable headset that isn't streaming.
    pub async fn start_all(&self) -> Vec<StartResult> {
        let mut out = Vec::new();
        for h in self.inner.devices.headsets() {
            if !h.can_cast || lock(&self.inner.entries).contains_key(&h.id) {
                continue;
            }
            out.push(match self.start(&h.id, StartOptions::default()).await {
                Ok(()) => StartResult {
                    id: h.id,
                    ok: true,
                    error: None,
                },
                Err(e) => StartResult {
                    id: h.id,
                    ok: false,
                    error: Some(e.to_string()),
                },
            });
        }
        out
    }

    /// Stop all streams.
    pub async fn stop_all(&self) {
        let serials: Vec<String> = lock(&self.inner.entries).keys().cloned().collect();
        let mut tasks = tokio::task::JoinSet::new();
        for s in serials {
            let this = self.clone();
            tasks.spawn(async move { this.stop(&s).await });
        }
        while tasks.join_next().await.is_some() {}
    }

    /// Current state of every stream.
    pub fn states(&self) -> Vec<StreamState> {
        lock(&self.inner.entries)
            .values()
            .map(|h| h.data().state())
            .collect()
    }

    /// Current state of one stream.
    pub fn state_of(&self, serial: &str) -> Option<StreamState> {
        lock(&self.inner.entries)
            .get(serial)
            .map(|h| h.data().state())
    }

    // ------------------------------------------------------------- driver

    fn is_current(&self, h: &EntryHandle) -> bool {
        lock(&self.inner.entries)
            .get(&h.serial)
            .is_some_and(|e| e.id == h.id)
    }

    fn remove_if_current(&self, h: &EntryHandle) {
        let mut entries = lock(&self.inner.entries);
        if entries.get(&h.serial).is_some_and(|e| e.id == h.id) {
            entries.remove(&h.serial);
        }
    }

    fn set_state(&self, h: &EntryHandle, state: StreamStatus) {
        let snapshot = {
            let mut d = h.data();
            d.state = state;
            d.state()
        };
        let _ = self.inner.events.send(CoreEvent::StreamState(snapshot));
    }

    fn finish(&self, h: &EntryHandle, error: String) {
        h.data().error = Some(error);
        self.remove_if_current(h);
        self.set_state(h, StreamStatus::Error);
    }

    fn prepare(&self, h: &Arc<EntryHandle>) -> BoxFuture<Prep> {
        let this = self.clone();
        let h = h.clone();
        Box::pin(async move { this.prepare_launch(&h).await })
    }

    async fn prepare_launch(&self, h: &EntryHandle) -> Prep {
        let serial = h.serial.clone();
        let (preference, codec_override, force_eight_bit, encoded) = {
            let d = h.data();
            (
                d.preference,
                d.codec_override,
                d.force_eight_bit,
                d.encoded(),
            )
        };
        let devices = &self.inner.devices;
        let Some(t) = devices
            .pick_transport(&serial, preference)
            .or_else(|| devices.pick_transport(&serial, TransportPreference::Auto))
        else {
            return Prep::Finish("Headset is no longer connected.".into());
        };
        let settings = self.inner.store.settings();
        let preset = quality_for(&settings, t.kind);
        let rec = self.inner.store.headset(&serial);
        let crop = if settings.single_eye_stream {
            let eye = match rec
                .as_ref()
                .and_then(|r| r.calibration_value("eye"))
                .and_then(|v| v.as_str())
            {
                Some("right") => Eye::Right,
                _ => Eye::Left,
            };
            self.panel_size(&t.adb_serial)
                .await
                .and_then(|p| eye_crop(p, eye))
        } else {
            None
        };
        let auto_size = settings.auto_resolution;
        let size = if auto_size {
            let panel = self.panel_size(&t.adb_serial).await;
            let frame = policy::frame_size_for(crop.as_deref(), panel, encoded);
            policy::launch_size(&preset, true, frame, &self.view_requests(&serial))
        } else {
            policy::launch_size(&preset, false, None, &[])
        };
        let choice = self.resolve_codec(&serial, codec_override, &t).await;
        if let Some(note) = &choice.note {
            let first = !std::mem::replace(&mut h.data().noted, true);
            if first {
                let name = rec
                    .as_ref()
                    .and_then(|r| r.name.clone())
                    .unwrap_or_else(|| serial.clone());
                self.notice(NoticeLevel::Warn, format!("{name}: {note}"));
            }
        }
        let stopped = h.data().user_stopped;
        if stopped || !self.is_current(h) {
            return Prep::Abort;
        }
        let failed = lock(&self.inner.failures).ten_bit_failed(&serial, choice.codec);
        let ten_bit = use_ten_bit(
            settings.ten_bit,
            force_eight_bit,
            choice.codec,
            failed,
            &self.decoder_support(),
        );
        Prep::Ready(Box::new(Prepared {
            transport: t,
            preset,
            size,
            choice,
            ten_bit,
            crop,
            auto_size,
        }))
    }

    /// Apply a prepared launch to the entry and start the session.
    fn launch(
        &self,
        h: &EntryHandle,
        p: Prepared,
    ) -> BoxFuture<Result<SessionStart<B::Session>, CoreError>> {
        let config = SessionConfig {
            codec: p.choice.codec,
            encoder: p.choice.encoder.clone(),
            max_size: p.size.max_size,
            bit_rate: p.size.bit_rate,
            max_fps: p.preset.max_fps,
            crop: p.crop.clone(),
            codec_options: if p.ten_bit {
                vec![TEN_BIT_CODEC_OPTION.to_string()]
            } else {
                Vec::new()
            },
        };
        let state = {
            let mut d = h.data();
            d.auto_size = p.auto_size;
            d.size_long = Some(p.size.size_long);
            d.ten_bit = p.ten_bit;
            d.transport = Some(p.transport.kind);
            d.adb_serial = Some(p.transport.adb_serial.clone());
            d.quality = Some(policy::quality_label(
                p.auto_size,
                p.size.max_size,
                p.size.bit_rate,
                &p.preset,
            ));
            d.bit_rate = Some(p.size.bit_rate);
            d.resizing = false;
            d.codec = Some(p.choice.codec);
            d.encoder = p.choice.encoder.clone();
            d.crop = p.crop.clone();
            d.error = None;
            d.got_media = false;
            if d.retries > 0 {
                StreamStatus::Reconnecting
            } else {
                StreamStatus::Starting
            }
        };
        self.set_state(h, state);
        let this = self.clone();
        let adb_serial = p.transport.adb_serial;
        Box::pin(async move { this.inner.backend.start_session(&adb_serial, config).await })
    }

    /// The session ended: decide what's next. `None` = the driver is done.
    fn on_end(&self, h: &Arc<EntryHandle>) -> Option<Phase<B::Session>> {
        let online = self
            .inner
            .devices
            .headset(&h.serial)
            .is_some_and(|hs| hs.is_online());
        let auto_retry = self.inner.store.read(|s| s.settings().auto_retry_streams);
        let is_current = self.is_current(h);
        let ctx = {
            let d = h.data();
            EndContext {
                user_stopped: d.user_stopped,
                replaced: d.replaced,
                resizing: d.resizing,
                is_current,
                online,
                ten_bit: d.ten_bit,
                got_media: d.got_media,
                codec: d.codec,
                auto_retry,
                retries: d.retries,
                max_retries: self.inner.config.max_retries,
            }
        };
        let cfg = self.inner.config;
        let name = || {
            self.inner
                .devices
                .headset(&h.serial)
                .map_or_else(|| h.serial.clone(), |hs| hs.name)
        };
        let reason = || {
            h.data()
                .error
                .clone()
                .unwrap_or_else(|| "encoder error".to_string())
        };
        match policy::decide_on_end(&ctx) {
            EndAction::Stopped { emit } => {
                self.remove_if_current(h);
                if emit {
                    self.set_state(h, StreamStatus::Stopped);
                }
                None
            }
            EndAction::Relaunch => Some(Phase::Preparing(self.prepare(h))),
            EndAction::TenBitFallback => {
                let codec = ctx.codec.unwrap_or(Codec::H264);
                lock(&self.inner.failures).mark_ten_bit_failed(&h.serial, codec);
                self.notice(
                    NoticeLevel::Warn,
                    format!(
                        "{}: 10-bit {} did not start ({}) — using 8-bit.",
                        name(),
                        codec.label(),
                        reason()
                    ),
                );
                {
                    let mut d = h.data();
                    d.force_eight_bit = true;
                    d.codec_override = Some(codec);
                }
                Some(Phase::Idle {
                    relaunch_at: Instant::now() + cfg.fallback_delay,
                })
            }
            EndAction::CodecFallback => {
                let codec = ctx.codec.unwrap_or(Codec::H264);
                lock(&self.inner.failures).mark_codec_failed(&h.serial, codec);
                self.notice(
                    NoticeLevel::Warn,
                    format!(
                        "{}: {} did not start ({}) — falling back to H.264.",
                        name(),
                        codec.label(),
                        reason()
                    ),
                );
                h.data().codec_override = Some(Codec::H264);
                Some(Phase::Idle {
                    relaunch_at: Instant::now() + cfg.fallback_delay,
                })
            }
            EndAction::Retry { attempt } => {
                h.data().retries = attempt;
                self.set_state(h, StreamStatus::Reconnecting);
                Some(Phase::Idle {
                    relaunch_at: Instant::now() + cfg.retry_base_delay * attempt,
                })
            }
            EndAction::Fail => {
                let error = h
                    .data()
                    .error
                    .clone()
                    .unwrap_or_else(|| "Stream ended.".to_string());
                self.finish(h, error);
                None
            }
        }
    }

    fn evaluate_resize(
        &self,
        h: &EntryHandle,
        phase: &Phase<B::Session>,
        resize_at: &mut Option<Instant>,
    ) {
        let cfg = self.inner.config;
        let now = self.now_ms();
        let settings = self.inner.store.settings();
        let reqs = self.view_requests(&h.serial);
        let mut d = h.data();
        let preset = quality_for(&settings, d.transport.unwrap_or(TransportKind::Wifi));
        let panel = d
            .adb_serial
            .as_ref()
            .and_then(|s| lock(&self.inner.panel_sizes).get(s).copied());
        let frame = policy::frame_size_for(d.crop.as_deref(), panel, d.encoded());
        let ms = |dur: Duration| dur.as_millis() as u64;
        let step = policy::evaluate_resize(&ResizeContext {
            auto_size: d.auto_size,
            user_stopped: d.user_stopped,
            resizing: d.resizing,
            state: d.state,
            got_media: d.got_media,
            live_since: d.live_since,
            now,
            size_long: d.size_long,
            down_since: d.down_since,
            preset: &preset,
            frame,
            reqs: &reqs,
            debounce_ms: ms(cfg.resize_debounce),
            settle_ms: ms(cfg.resize_settle),
            down_delay_ms: ms(cfg.down_delay),
            hidden_delay_ms: ms(cfg.hidden_delay),
        });
        match step {
            ResizeStep::Ignore => {}
            ResizeStep::Reschedule(after) => {
                *resize_at = Some(Instant::now() + Duration::from_millis(after))
            }
            ResizeStep::Keep => d.down_since = None,
            ResizeStep::Wait(after) => {
                d.down_since.get_or_insert(now);
                *resize_at = Some(Instant::now() + Duration::from_millis(after));
            }
            ResizeStep::Resize(size) => {
                d.down_since = None;
                if let Phase::Running { session, .. } = phase {
                    tracing::debug!("{}: resizing {:?} → {size}", h.serial, d.size_long);
                    d.resizing = true;
                    d.reason = Some("resize".into());
                    drop(d);
                    self.set_state(h, StreamStatus::Resizing);
                    session.stop();
                }
            }
        }
    }

    fn on_session_event(
        &self,
        h: &EntryHandle,
        event: SessionEvent,
        resize_at: &mut Option<Instant>,
    ) {
        match event {
            SessionEvent::Device(name) => h.data().device_name = Some(name),
            SessionEvent::Session { width, height } => {
                let auto_size = {
                    let mut d = h.data();
                    d.width = Some(width);
                    d.height = Some(height);
                    d.live_since = Some(self.now_ms());
                    d.auto_size
                };
                self.set_state(h, StreamStatus::Live);
                if auto_size {
                    *resize_at = Some(Instant::now() + self.inner.config.resize_settle);
                }
            }
            SessionEvent::Packet(packet) => {
                {
                    let mut d = h.data();
                    if !d.got_media && !packet.config {
                        d.got_media = true;
                        d.retries = 0;
                        d.reason = None;
                    }
                }
                let _ = self.inner.events.send(CoreEvent::Packet {
                    id: h.serial.clone(),
                    packet,
                });
            }
            SessionEvent::Log(line) => {
                let _ = self.inner.events.send(CoreEvent::StreamLog {
                    id: h.serial.clone(),
                    line,
                });
            }
            SessionEvent::Error(e) => h.data().error = Some(e),
            SessionEvent::End => {}
        }
    }

    async fn drive(self, h: Arc<EntryHandle>, mut cmd_rx: mpsc::UnboundedReceiver<Command>) {
        let mut phase: Phase<B::Session> = Phase::Preparing(self.prepare(&h));
        let mut resize_at: Option<Instant> = None;
        let mut stop_deadline: Option<Instant> = None;
        let mut stop_waiters: Vec<oneshot::Sender<()>> = Vec::new();
        loop {
            let event = tokio::select! {
                biased;
                cmd = cmd_rx.recv() => DriverEvent::Command(cmd),
                _ = sleep_until_opt(stop_deadline) => DriverEvent::StopTimeout,
                _ = sleep_until_opt(resize_at) => DriverEvent::ResizeDue,
                e = phase_event(&mut phase) => DriverEvent::Phase(e),
            };
            let next = match event {
                DriverEvent::Command(None) => {
                    // Every handle is gone: shut down.
                    if let Phase::Running { session, .. } = &phase {
                        session.stop();
                    }
                    None
                }
                DriverEvent::Command(Some(Command::ScheduleResize(after))) => {
                    resize_at = Some(Instant::now() + after);
                    continue;
                }
                DriverEvent::Command(Some(Command::Stop { replaced, done })) => {
                    resize_at = None;
                    stop_waiters.push(done);
                    {
                        let mut d = h.data();
                        d.user_stopped = true;
                        d.replaced = replaced;
                    }
                    if let Phase::Running { session, .. } = &phase {
                        session.stop();
                        stop_deadline
                            .get_or_insert(Instant::now() + self.inner.config.stop_timeout);
                        continue;
                    }
                    // Not streaming (waiting, preparing or starting): cancel whatever is in flight.
                    drop(std::mem::replace(&mut phase, Phase::Done));
                    self.on_end(&h)
                }
                DriverEvent::StopTimeout => {
                    tracing::warn!("{}: session did not stop in time", h.serial);
                    drop(std::mem::replace(&mut phase, Phase::Done));
                    self.on_end(&h)
                }
                DriverEvent::ResizeDue => {
                    resize_at = None;
                    self.evaluate_resize(&h, &phase, &mut resize_at);
                    continue;
                }
                DriverEvent::Phase(PhaseEvent::RelaunchDue) => {
                    // Read the flag first: never hold an entry lock while taking the map lock.
                    let stopped = h.data().user_stopped;
                    if stopped || !self.is_current(&h) {
                        None
                    } else {
                        Some(Phase::Preparing(self.prepare(&h)))
                    }
                }
                DriverEvent::Phase(PhaseEvent::Prepared(prep)) => match prep {
                    Prep::Ready(p) => Some(Phase::Starting(self.launch(&h, *p))),
                    Prep::Finish(error) => {
                        self.finish(&h, error);
                        None
                    }
                    Prep::Abort => None,
                },
                DriverEvent::Phase(PhaseEvent::Started(Ok(SessionStart { session, events }))) => {
                    if self.is_current(&h) {
                        Some(Phase::Running { session, events })
                    } else {
                        // Replaced while starting: never leave an orphaned session running.
                        session.stop();
                        None
                    }
                }
                DriverEvent::Phase(PhaseEvent::Started(Err(e))) => {
                    {
                        let mut d = h.data();
                        if d.error.is_none() {
                            d.error = Some(e.to_string());
                        }
                    }
                    drop(std::mem::replace(&mut phase, Phase::Done));
                    self.on_end(&h)
                }
                DriverEvent::Phase(PhaseEvent::Session(e)) => {
                    self.on_session_event(&h, e, &mut resize_at);
                    continue;
                }
                DriverEvent::Phase(PhaseEvent::SessionEnded) => {
                    drop(std::mem::replace(&mut phase, Phase::Done));
                    stop_deadline = None;
                    self.on_end(&h)
                }
            };
            match next {
                Some(p) => phase = p,
                None => break,
            }
        }
        for w in stop_waiters {
            let _ = w.send(());
        }
        // Answer stops that raced with the end of the stream.
        cmd_rx.close();
        while let Ok(cmd) = cmd_rx.try_recv() {
            if let Command::Stop { done, .. } = cmd {
                let _ = done.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests;
