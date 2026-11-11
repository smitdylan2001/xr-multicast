use std::sync::atomic::AtomicBool;

use tokio::sync::broadcast;

use super::policy::*;
use super::*;
use crate::codec_policy::DecodeCaps;
use crate::config::QualityKey;
use crate::device_manager::HeadsetSnapshot;
use crate::layout::Layout;
use crate::view_size::ViewMode;

// --------------------------------------------------------------------------
// Port of test/stream-resize.test.js (the policy, without timers).

const QUEST3: FrameSize = FrameSize { w: 4128, h: 2208 };

fn tile(w: f64, h: f64) -> ViewRequest {
    ViewRequest {
        visible: true,
        tile_w: w,
        tile_h: h,
        mode: ViewMode::Corrected,
        zoom: 1.45,
        output_aspect: Some("16:9".into()),
        layout: Layout::Sbs,
        rotation: 0.0,
    }
}

/// Mirrors the JS test's `setup()`: a live Quest 3 stream on USB with a panel size known.
struct Resize {
    sizes: ViewSizes,
    preset: QualityPreset,
    size_long: u32,
    state: StreamStatus,
    live_since: Option<u64>,
    auto_size: bool,
    down_since: Option<u64>,
    now: u64,
    resized: Vec<u32>,
}

impl Resize {
    fn setup(size_long: u32, quality: QualityKey) -> Self {
        Resize {
            sizes: ViewSizes::default(),
            preset: quality.preset().unwrap(),
            size_long,
            state: StreamStatus::Live,
            live_since: Some(0),
            auto_size: true,
            down_since: None,
            now: 1_000_000,
            resized: Vec::new(),
        }
    }

    fn set_view_sizes(&mut self, viewer: &str, id: &str, req: Option<ViewRequest>) {
        self.sizes.set(viewer, [(id.to_string(), req)]);
    }

    /// `evaluateResize('S1')`, applying the step like the stream manager does.
    fn evaluate(&mut self) {
        let reqs = self.sizes.requests("S1");
        let step = evaluate_resize(&ResizeContext {
            auto_size: self.auto_size,
            user_stopped: false,
            resizing: false,
            state: self.state,
            got_media: true,
            live_since: self.live_since,
            now: self.now,
            size_long: Some(self.size_long),
            down_since: self.down_since,
            preset: &self.preset,
            frame: frame_size_for(None, Some(QUEST3), None),
            reqs: &reqs,
            debounce_ms: 1000,
            settle_ms: 2500,
            down_delay_ms: 5000,
            hidden_delay_ms: 15000,
        });
        match step {
            ResizeStep::Keep => self.down_since = None,
            ResizeStep::Wait(_) => {
                self.down_since.get_or_insert(self.now);
            }
            ResizeStep::Resize(size) => {
                self.down_since = None;
                self.resized.push(size);
            }
            ResizeStep::Ignore | ResizeStep::Reschedule(_) => {}
        }
    }
}

#[test]
fn a_bigger_tile_upgrades_the_stream_immediately() {
    let mut r = Resize::setup(960, QualityKey::High);
    r.set_view_sizes("local", "S1", Some(tile(1800.0, 1000.0)));
    r.evaluate();
    assert_eq!(r.resized, [2560]);
}

#[test]
fn a_slightly_different_tile_size_does_not_restart_the_stream() {
    let mut r = Resize::setup(960, QualityKey::High);
    r.set_view_sizes("local", "S1", Some(tile(600.0, 338.0)));
    r.evaluate();
    assert!(r.resized.is_empty());
}

#[test]
fn shrinking_two_or_more_steps_waits_before_downgrading() {
    let mut r = Resize::setup(2560, QualityKey::High);
    r.set_view_sizes("local", "S1", Some(tile(600.0, 338.0)));
    r.evaluate();
    assert!(r.resized.is_empty(), "not immediately");
    assert!(r.down_since.is_some());
    r.down_since = r.down_since.map(|d| d - 6000); // pretend 6 s passed
    r.evaluate();
    assert_eq!(r.resized, [960]);
}

#[test]
fn the_largest_viewer_wins() {
    // Hub window hidden, tablet shows it big.
    let mut r = Resize::setup(960, QualityKey::High);
    r.set_view_sizes(
        "local",
        "S1",
        Some(ViewRequest {
            visible: false,
            ..Default::default()
        }),
    );
    r.set_view_sizes("remote:ipad", "S1", Some(tile(1600.0, 900.0)));
    r.evaluate();
    assert_eq!(r.resized, [2560]);
    // The tablet leaves: only the hidden hub tile remains.
    r.sizes.clear_viewer("remote:ipad");
    assert_eq!(r.sizes.viewer_count("S1"), 1);
}

#[test]
fn never_above_the_quality_preset() {
    let mut r = Resize::setup(960, QualityKey::Balanced); // 1920 cap
    r.set_view_sizes("local", "S1", Some(tile(3000.0, 1700.0)));
    r.evaluate();
    assert_eq!(r.resized, [1920]);
}

#[test]
fn no_resize_while_the_stream_is_not_live_yet_or_just_restarted() {
    let mut r = Resize::setup(960, QualityKey::High);
    r.set_view_sizes("local", "S1", Some(tile(1800.0, 1000.0)));
    r.state = StreamStatus::Starting;
    r.evaluate();
    r.state = StreamStatus::Live;
    r.live_since = Some(r.now);
    r.evaluate();
    assert!(r.resized.is_empty());
}

#[test]
fn turning_the_setting_off_disables_resizing() {
    let mut r = Resize::setup(960, QualityKey::High);
    r.auto_size = false;
    r.set_view_sizes("local", "S1", Some(tile(1800.0, 1000.0)));
    r.evaluate();
    assert!(r.resized.is_empty());
}

// ---- more policy checks

#[test]
fn reschedule_delays() {
    let preset = QualityKey::High.preset().unwrap();
    let reqs = [tile(1800.0, 1000.0)];
    let ctx = |state, got_media, live_since, now| ResizeContext {
        auto_size: true,
        user_stopped: false,
        resizing: false,
        state,
        got_media,
        live_since,
        now,
        size_long: Some(960),
        down_since: None,
        preset: &preset,
        frame: Some(QUEST3),
        reqs: &reqs,
        debounce_ms: 1000,
        settle_ms: 2500,
        down_delay_ms: 5000,
        hidden_delay_ms: 15000,
    };
    assert_eq!(
        evaluate_resize(&ctx(StreamStatus::Starting, false, None, 0)),
        ResizeStep::Reschedule(1000)
    );
    assert_eq!(
        evaluate_resize(&ctx(StreamStatus::Live, false, Some(0), 9000)),
        ResizeStep::Reschedule(1000)
    );
    assert_eq!(
        evaluate_resize(&ctx(StreamStatus::Live, true, Some(1000), 2000)),
        ResizeStep::Reschedule(1500)
    );
    assert_eq!(
        evaluate_resize(&ctx(StreamStatus::Live, true, Some(0), 9000)),
        ResizeStep::Resize(2560)
    );
    let mut c = ctx(StreamStatus::Live, true, Some(0), 9000);
    c.resizing = true;
    assert_eq!(evaluate_resize(&c), ResizeStep::Ignore);
    let mut c = ctx(StreamStatus::Live, true, Some(0), 9000);
    c.reqs = &[];
    assert_eq!(
        evaluate_resize(&c),
        ResizeStep::Ignore,
        "no viewer info: keep the preset"
    );
    let mut c = ctx(StreamStatus::Live, true, Some(0), 9000);
    c.size_long = Some(2560);
    c.reqs = &[];
    assert_eq!(evaluate_resize(&c), ResizeStep::Ignore);
}

#[test]
fn launch_sizes() {
    let high = QualityKey::High.preset().unwrap();
    let max = QualityKey::Max.preset().unwrap();
    // No viewer info: the preset.
    assert_eq!(
        launch_size(&high, true, Some(QUEST3), &[]),
        LaunchSize {
            max_size: 2560,
            bit_rate: 16_000_000,
            size_long: 2560
        }
    );
    assert_eq!(
        launch_size(&max, true, Some(QUEST3), &[]),
        LaunchSize {
            max_size: 0,
            bit_rate: 24_000_000,
            size_long: 4128
        }
    );
    assert_eq!(launch_size(&max, true, None, &[]).size_long, 0);
    // Resolution off: the preset, whatever the viewers say.
    assert_eq!(
        launch_size(&high, false, Some(QUEST3), &[tile(100.0, 100.0)]).max_size,
        2560
    );
    // A small tile: a smaller size and bitrate.
    let s = launch_size(&high, true, Some(QUEST3), &[tile(620.0, 349.0)]);
    assert_eq!(s.max_size, 960);
    assert_eq!(s.size_long, 960);
    assert_eq!(
        s.bit_rate,
        crate::view_size::scaled_bitrate(16_000_000, 960, 2560)
    );
    // A huge tile on the native preset: native (max_size 0).
    let s = launch_size(&max, true, Some(QUEST3), &[tile(8000.0, 4000.0)]);
    assert_eq!((s.max_size, s.size_long, s.bit_rate), (0, 4128, 24_000_000));
    assert_eq!(
        quality_label(true, 960, 3_674_000, &high),
        "Auto 960px · 3.7 Mbps (max: High (2560px · 16 Mbps · 60 fps))"
    );
    assert_eq!(
        quality_label(true, 0, 24_000_000, &max),
        "Auto native · 24.0 Mbps (max: Native (full res · 24 Mbps · 72 fps))"
    );
    assert_eq!(quality_label(false, 0, 1, &high), high.label);
}

#[test]
fn frame_sizes() {
    assert_eq!(
        frame_size_for(Some("2064:2208:0:0"), Some(QUEST3), None),
        Some(FrameSize { w: 2064, h: 2208 })
    );
    assert_eq!(frame_size_for(Some("garbage"), Some(QUEST3), None), None);
    assert_eq!(
        frame_size_for(None, None, Some(FrameSize { w: 1832, h: 960 })),
        Some(FrameSize { w: 1832, h: 960 })
    );
    assert_eq!(
        frame_size_for(None, None, Some(FrameSize { w: 0, h: 960 })),
        None
    );
}

#[test]
fn end_decisions() {
    let base = EndContext {
        user_stopped: false,
        replaced: false,
        resizing: false,
        is_current: true,
        online: true,
        ten_bit: false,
        got_media: true,
        codec: Some(Codec::H264),
        auto_retry: true,
        retries: 0,
        max_retries: 5,
    };
    assert_eq!(
        decide_on_end(&EndContext {
            user_stopped: true,
            ..base
        }),
        EndAction::Stopped { emit: true }
    );
    assert_eq!(
        decide_on_end(&EndContext {
            user_stopped: true,
            replaced: true,
            ..base
        }),
        EndAction::Stopped { emit: false }
    );
    assert_eq!(
        decide_on_end(&EndContext {
            resizing: true,
            ..base
        }),
        EndAction::Relaunch
    );
    let no_media = EndContext {
        got_media: false,
        ..base
    };
    assert_eq!(
        decide_on_end(&EndContext {
            ten_bit: true,
            codec: Some(Codec::H265),
            ..no_media
        }),
        EndAction::TenBitFallback
    );
    assert_eq!(
        decide_on_end(&EndContext {
            codec: Some(Codec::Av1),
            ..no_media
        }),
        EndAction::CodecFallback
    );
    assert_eq!(
        decide_on_end(&no_media),
        EndAction::Retry { attempt: 1 },
        "H.264 without media just retries"
    );
    assert_eq!(
        decide_on_end(&EndContext { retries: 4, ..base }),
        EndAction::Retry { attempt: 5 }
    );
    assert_eq!(
        decide_on_end(&EndContext { retries: 5, ..base }),
        EndAction::Fail
    );
    assert_eq!(
        decide_on_end(&EndContext {
            auto_retry: false,
            ..base
        }),
        EndAction::Fail
    );
    assert_eq!(
        decide_on_end(&EndContext {
            online: false,
            codec: Some(Codec::H265),
            ..no_media
        }),
        EndAction::Fail
    );
}

// --------------------------------------------------------------------------
// The state machine, with a fake scrcpy backend and fake devices.

#[derive(Default)]
struct BackendState {
    server_missing: bool,
    encoders: Option<Vec<EncoderInfo>>,
    panel: Option<FrameSize>,
    fail_start: Option<String>,
    starts: Vec<(String, SessionConfig)>,
    sessions: Vec<mpsc::Sender<SessionEvent>>,
    stops: usize,
}

#[derive(Clone, Default)]
struct FakeBackend(Arc<Mutex<BackendState>>);

impl FakeBackend {
    fn lock(&self) -> MutexGuard<'_, BackendState> {
        self.0.lock().unwrap()
    }
}

struct FakeSession {
    tx: mpsc::Sender<SessionEvent>,
    stopped: AtomicBool,
    backend: FakeBackend,
}

impl SessionControl for FakeSession {
    fn stop(&self) {
        if !self.stopped.swap(true, Ordering::SeqCst) {
            self.backend.lock().stops += 1;
            let _ = self.tx.try_send(SessionEvent::End);
        }
    }
}

impl StreamBackend for FakeBackend {
    type Session = FakeSession;

    fn server_available(&self) -> bool {
        !self.lock().server_missing
    }

    async fn list_encoders(&self, _adb_serial: &str) -> Result<Vec<EncoderInfo>, CoreError> {
        self.lock()
            .encoders
            .clone()
            .ok_or_else(|| CoreError::Other("No encoders reported".into()))
    }

    async fn panel_size(&self, _adb_serial: &str) -> Result<Option<FrameSize>, CoreError> {
        Ok(self.lock().panel)
    }

    async fn start_session(
        &self,
        adb_serial: &str,
        config: SessionConfig,
    ) -> Result<SessionStart<FakeSession>, CoreError> {
        let mut st = self.lock();
        st.starts.push((adb_serial.to_string(), config));
        if let Some(e) = &st.fail_start {
            return Err(CoreError::Other(e.clone()));
        }
        let (tx, rx) = mpsc::channel(64);
        st.sessions.push(tx.clone());
        Ok(SessionStart {
            session: FakeSession {
                tx,
                stopped: AtomicBool::new(false),
                backend: self.clone(),
            },
            events: rx,
        })
    }
}

type DeviceList = Vec<(HeadsetSnapshot, Vec<Transport>)>;

#[derive(Clone, Default)]
struct FakeDevices(Arc<Mutex<DeviceList>>);

impl FakeDevices {
    fn add(&self, id: &str, name: &str, can_cast: bool, transports: &[(&str, TransportKind)]) {
        let online = !transports.is_empty();
        let ts: Vec<Transport> = transports
            .iter()
            .map(|(s, k)| Transport {
                adb_serial: s.to_string(),
                kind: *k,
            })
            .collect();
        let hs = HeadsetSnapshot {
            id: id.into(),
            serial: Some(id.into()),
            adb_serial: None,
            pending: false,
            network: None,
            trusted: can_cast,
            index: Some(1),
            name: name.into(),
            color: "#F97316".into(),
            brand: "meta".into(),
            model: "Quest 3".into(),
            model_raw: Some("Quest 3".into()),
            variant: None,
            manufacturer: Some("Oculus".into()),
            android_version: Some("14".into()),
            usb: ts
                .iter()
                .find(|t| t.kind == TransportKind::Usb)
                .map(|t| t.adb_serial.clone()),
            wifi: ts
                .iter()
                .find(|t| t.kind == TransportKind::Wifi)
                .map(|t| t.adb_serial.clone()),
            ip: None,
            battery: None,
            status: if online { "online" } else { "offline" }.into(),
            can_cast: can_cast && online,
            calibration: None,
            transport: TransportPreference::Auto,
            view_mode: None,
            verified_at: None,
            last_seen: None,
            error: None,
        };
        let mut all = self.0.lock().unwrap();
        all.retain(|(h, _)| h.id != id);
        all.push((hs, ts));
    }
}

impl DeviceDirectory for FakeDevices {
    fn headset(&self, id: &str) -> Option<HeadsetSnapshot> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(h, _)| h.id == id)
            .map(|(h, _)| h.clone())
    }

    fn headsets(&self) -> Vec<HeadsetSnapshot> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(h, _)| h.clone())
            .collect()
    }

    fn pick_transport(&self, id: &str, preference: TransportPreference) -> Option<Transport> {
        let all = self.0.lock().unwrap();
        let ts = &all.iter().find(|(h, _)| h.id == id)?.1;
        let of = |k| ts.iter().find(|t| t.kind == k).cloned();
        match preference {
            TransportPreference::Usb => of(TransportKind::Usb),
            TransportPreference::Wifi => of(TransportKind::Wifi),
            TransportPreference::Auto => of(TransportKind::Usb).or_else(|| of(TransportKind::Wifi)),
        }
    }
}

fn quest_encoders() -> Vec<EncoderInfo> {
    vec![
        EncoderInfo {
            codec: "h264".into(),
            name: "c2.qti.avc.encoder".into(),
            hw: true,
            alias: false,
        },
        EncoderInfo {
            codec: "h265".into(),
            name: "c2.qti.hevc.encoder".into(),
            hw: true,
            alias: false,
        },
        EncoderInfo {
            codec: "av1".into(),
            name: "c2.android.av1.encoder".into(),
            hw: false,
            alias: false,
        },
    ]
}

fn all_hw(ten_bit: bool) -> DecoderSupport {
    let mut keys = vec!["h264", "h265", "av1"];
    if ten_bit {
        keys.extend(["h265_10", "av1_10"]);
    }
    keys.into_iter()
        .map(|k| {
            (
                k.to_string(),
                DecodeCaps {
                    any: true,
                    hw: true,
                },
            )
        })
        .collect()
}

struct Harness {
    sm: StreamManager<FakeBackend, FakeDevices>,
    backend: FakeBackend,
    devices: FakeDevices,
    store: SharedStore,
    rx: broadcast::Receiver<CoreEvent>,
}

const ID: &str = "2G0YC5ZG8L0123";

fn harness() -> Harness {
    let backend = FakeBackend::default();
    backend.lock().encoders = Some(quest_encoders());
    backend.lock().panel = Some(QUEST3);
    let devices = FakeDevices::default();
    devices.add(ID, "Meta Quest 3 #1", true, &[("USB1", TransportKind::Usb)]);
    let store = SharedStore::in_memory();
    store.write(|s| s.modify_headset(ID, |r| r.name = Some("Meta Quest 3 #1".into())));
    let (tx, rx) = crate::events::channel(1024);
    let sm = StreamManager::new(
        backend.clone(),
        devices.clone(),
        store.clone(),
        tx,
        StreamManagerConfig::default(),
    );
    Harness {
        sm,
        backend,
        devices,
        store,
        rx,
    }
}

impl Harness {
    /// Let spawned tasks run without advancing the clock.
    async fn settle(&self) {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    /// Advance the (paused) clock until `cond` holds.
    async fn wait_for(&self, what: &str, cond: impl Fn(&Self) -> bool) {
        for _ in 0..2000 {
            if cond(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn wait_starts(&self, n: usize) {
        self.wait_for(&format!("{n} session starts"), |h| {
            h.backend.lock().starts.len() >= n
        })
        .await;
        self.settle().await;
    }

    fn config(&self, i: usize) -> SessionConfig {
        self.backend.lock().starts[i].1.clone()
    }

    fn session(&self, i: usize) -> mpsc::Sender<SessionEvent> {
        self.backend.lock().sessions[i].clone()
    }

    async fn send(&self, i: usize, e: SessionEvent) {
        self.session(i).send(e).await.unwrap();
        self.settle().await;
    }

    async fn go_live(&self, i: usize) {
        self.send(i, SessionEvent::Device("Quest 3".into())).await;
        self.send(
            i,
            SessionEvent::Session {
                width: 1832,
                height: 960,
            },
        )
        .await;
        self.send(i, packet(true)).await;
        self.send(i, packet(false)).await;
    }

    /// Drain events: (stream states, notices, packets).
    fn drain(&mut self) -> (Vec<StreamState>, Vec<String>, usize) {
        let (mut states, mut notices, mut packets) = (Vec::new(), Vec::new(), 0);
        while let Ok(e) = self.rx.try_recv() {
            match e {
                CoreEvent::StreamState(s) => states.push(s),
                CoreEvent::Notice(n) => notices.push(n.message),
                CoreEvent::Packet { .. } => packets += 1,
                _ => {}
            }
        }
        (states, notices, packets)
    }

    fn statuses(&mut self) -> Vec<StreamStatus> {
        self.drain().0.into_iter().map(|s| s.state).collect()
    }
}

fn packet(config: bool) -> SessionEvent {
    SessionEvent::Packet(VideoPacket {
        config,
        key: !config,
        pts: 0,
        data: bytes::Bytes::from_static(b"\0\0\0\x01"),
    })
}

#[tokio::test(start_paused = true)]
async fn start_goes_live_and_forwards_packets() {
    let mut h = harness();
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    let (adb_serial, cfg) = h.backend.lock().starts[0].clone();
    assert_eq!(adb_serial, "USB1");
    // No decoder report yet: H.264 on the hardware encoder, USB preset "high".
    assert_eq!(
        cfg,
        SessionConfig {
            codec: Codec::H264,
            encoder: Some("c2.qti.avc.encoder".into()),
            max_size: 2560,
            bit_rate: 16_000_000,
            max_fps: 60,
            crop: None,
            codec_options: vec![],
        }
    );
    h.go_live(0).await;
    let (states, notices, packets) = h.drain();
    assert_eq!(
        states.iter().map(|s| s.state).collect::<Vec<_>>(),
        [StreamStatus::Starting, StreamStatus::Live]
    );
    assert!(notices.is_empty());
    assert_eq!(packets, 2);
    let s = h.sm.state_of(ID).unwrap();
    assert_eq!(s.transport, Some(TransportKind::Usb));
    assert_eq!((s.width, s.height), (Some(1832), Some(960)));
    assert_eq!(s.device_name.as_deref(), Some("Quest 3"));
    assert_eq!(s.size_long, Some(2560));
    assert!(s.auto_size);
    assert_eq!(
        s.quality.as_deref(),
        Some("Auto 2560px · 16.0 Mbps (max: High (2560px · 16 Mbps · 60 fps))")
    );
    assert_eq!(h.sm.states().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn start_errors() {
    let h = harness();
    assert!(matches!(
        h.sm.start("nope", StartOptions::default()).await,
        Err(CoreError::NotConnected)
    ));
    h.devices.add(
        "UNV",
        "Meta Quest 3",
        false,
        &[("USB2", TransportKind::Usb)],
    );
    let e =
        h.sm.start("UNV", StartOptions::default())
            .await
            .unwrap_err();
    assert_eq!(
        e.to_string(),
        "Verify \"Meta Quest 3\" over USB before casting (Settings → require USB verification)."
    );
    h.devices.add("OFF", "Off", true, &[]);
    assert!(matches!(
        h.sm.start("OFF", StartOptions::default()).await,
        Err(CoreError::NotConnected)
    ));
    h.backend.lock().server_missing = true;
    assert!(matches!(
        h.sm.start(ID, StartOptions::default()).await,
        Err(CoreError::ServerMissing)
    ));
}

#[tokio::test(start_paused = true)]
async fn stop_ends_the_session_and_reports_stopped() {
    let mut h = harness();
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.go_live(0).await;
    h.drain();
    h.sm.stop(ID).await;
    assert_eq!(h.statuses(), [StreamStatus::Stopped]);
    assert_eq!(h.backend.lock().stops, 1);
    assert!(h.sm.states().is_empty());
    h.sm.stop(ID).await; // no-op
}

#[tokio::test(start_paused = true)]
async fn stop_while_starting_or_waiting_cancels() {
    let mut h = harness();
    h.backend.lock().fail_start = Some("adb push failed".into());
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    // The failed start waits 1.5 s before retrying: stop during the wait.
    assert_eq!(
        h.statuses(),
        [StreamStatus::Starting, StreamStatus::Reconnecting]
    );
    assert_eq!(
        h.sm.state_of(ID).unwrap().error.as_deref(),
        Some("adb push failed")
    );
    h.sm.stop(ID).await;
    assert_eq!(h.statuses(), [StreamStatus::Stopped]);
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(h.backend.lock().starts.len(), 1, "no retry after stop");
}

#[tokio::test(start_paused = true)]
async fn starting_twice_is_a_no_op_and_restart_replaces_silently() {
    let mut h = harness();
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.go_live(0).await;
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.sm.start(
        ID,
        StartOptions {
            transport: Some(TransportPreference::Usb),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    h.settle().await;
    assert_eq!(h.backend.lock().starts.len(), 1);
    h.drain();
    h.sm.start(
        ID,
        StartOptions {
            restart: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    h.wait_starts(2).await;
    assert_eq!(
        h.statuses(),
        [StreamStatus::Starting],
        "the replaced stream does not report 'stopped'"
    );
    // "auto" differs from the running transport kind ("usb"): restarts, like the Electron app.
    h.go_live(1).await;
    h.sm.start(
        ID,
        StartOptions {
            transport: Some(TransportPreference::Auto),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    h.wait_starts(3).await;
}

#[tokio::test(start_paused = true)]
async fn codec_falls_back_to_h264_when_the_encoder_produces_nothing() {
    let mut h = harness();
    h.sm.set_decoder_support(Some(all_hw(false)));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    assert_eq!(h.config(0).codec, Codec::H265);
    assert_eq!(h.config(0).encoder.as_deref(), Some("c2.qti.hevc.encoder"));
    h.send(
        0,
        SessionEvent::Error("The headset could not configure its video encoder.".into()),
    )
    .await;
    h.send(0, SessionEvent::End).await;
    h.wait_starts(2).await;
    assert_eq!(h.config(1).codec, Codec::H264);
    let (_, notices, _) = h.drain();
    assert_eq!(
        notices,
        [
            "Meta Quest 3 #1: H.265 (HEVC) did not start (The headset could not configure its video encoder.) — falling back to H.264."
        ]
    );
    // Remembered for this run: a fresh start skips H.265.
    h.go_live(1).await;
    h.sm.start(
        ID,
        StartOptions {
            restart: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    h.wait_starts(3).await;
    assert_eq!(h.config(2).codec, Codec::H264);
}

#[tokio::test(start_paused = true)]
async fn ten_bit_falls_back_to_eight_bit_first() {
    let mut h = harness();
    h.store.write(|s| s.modify_settings(|st| st.ten_bit = true));
    h.sm.set_decoder_support(Some(all_hw(true)));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    assert_eq!(h.config(0).codec, Codec::H265);
    assert_eq!(h.config(0).codec_options, ["profile=2"]);
    assert!(h.sm.state_of(ID).unwrap().ten_bit);
    h.send(0, SessionEvent::End).await;
    h.wait_starts(2).await;
    assert_eq!(h.config(1).codec, Codec::H265);
    assert!(h.config(1).codec_options.is_empty());
    assert!(!h.sm.state_of(ID).unwrap().ten_bit);
    let (_, notices, _) = h.drain();
    assert_eq!(
        notices,
        ["Meta Quest 3 #1: 10-bit H.265 (HEVC) did not start (encoder error) — using 8-bit."]
    );
    // 8-bit H.265 fails too: H.264.
    h.send(1, SessionEvent::End).await;
    h.wait_starts(3).await;
    assert_eq!(h.config(2).codec, Codec::H264);
}

#[tokio::test(start_paused = true)]
async fn ended_streams_retry_with_backoff_then_error() {
    let mut h = harness();
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    let t0 = Instant::now();
    for i in 0..=5 {
        h.wait_starts(i + 1).await;
        if i == 5 {
            h.send(i, SessionEvent::Error("Server exited (code 1)".into()))
                .await;
        }
        h.send(i, SessionEvent::End).await;
    }
    h.settle().await;
    // 1.5 + 3 + 4.5 + 6 + 7.5 s of back-off.
    assert!(
        t0.elapsed() >= Duration::from_millis(22_500),
        "{:?}",
        t0.elapsed()
    );
    let (states, _, _) = h.drain();
    let last = states.last().unwrap();
    assert_eq!(last.state, StreamStatus::Error);
    assert_eq!(last.error.as_deref(), Some("Server exited (code 1)"));
    let reconnecting: Vec<u32> = states
        .iter()
        .filter(|s| s.state == StreamStatus::Reconnecting)
        .map(|s| s.retries)
        .collect();
    assert_eq!(
        reconnecting,
        [1, 1, 2, 2, 3, 3, 4, 4, 5, 5],
        "end handler + relaunch each report it"
    );
    assert!(h.sm.states().is_empty());
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(h.backend.lock().starts.len(), 6);
}

#[tokio::test(start_paused = true)]
async fn media_resets_the_retry_counter() {
    let mut h = harness();
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.send(0, SessionEvent::End).await;
    h.wait_starts(2).await;
    assert_eq!(h.sm.state_of(ID).unwrap().retries, 1);
    h.go_live(1).await;
    assert_eq!(h.sm.state_of(ID).unwrap().retries, 0);
    h.drain();
    h.send(1, SessionEvent::End).await;
    let s = h.drain().0;
    assert_eq!(s[0].state, StreamStatus::Reconnecting);
    assert_eq!(s[0].retries, 1);
}

#[tokio::test(start_paused = true)]
async fn no_retry_when_disabled_or_offline() {
    let mut h = harness();
    h.store
        .write(|s| s.modify_settings(|st| st.auto_retry_streams = false));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.go_live(0).await;
    h.drain();
    h.send(0, SessionEvent::End).await;
    let s = h.drain().0;
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].state, StreamStatus::Error);
    assert_eq!(s[0].error.as_deref(), Some("Stream ended."));

    // Headset unplugged mid-stream: no fallback or retry either.
    h.store
        .write(|s| s.modify_settings(|st| st.auto_retry_streams = true));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(2).await;
    h.devices.add(ID, "Meta Quest 3 #1", true, &[]);
    h.drain();
    h.send(1, SessionEvent::End).await;
    assert_eq!(h.statuses(), [StreamStatus::Error]);
}

#[tokio::test(start_paused = true)]
async fn lost_transport_finishes_on_relaunch() {
    let mut h = harness();
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.go_live(0).await;
    h.send(0, SessionEvent::End).await;
    // Still listed online (e.g. the device list hasn't refreshed) but no transport at relaunch time.
    h.devices.0.lock().unwrap()[0].1.clear();
    tokio::time::sleep(Duration::from_secs(2)).await;
    h.settle().await;
    let s = h.drain().0;
    let last = s.last().unwrap();
    assert_eq!(last.state, StreamStatus::Error);
    assert_eq!(
        last.error.as_deref(),
        Some("Headset is no longer connected.")
    );
}

#[tokio::test(start_paused = true)]
async fn view_sizes_resize_a_live_stream() {
    let mut h = harness();
    h.sm.set_view_sizes("local", [(ID.to_string(), Some(tile(620.0, 349.0)))]);
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    let c0 = h.config(0);
    assert_eq!(c0.max_size, 960);
    assert_eq!(
        c0.bit_rate,
        crate::view_size::scaled_bitrate(16_000_000, 960, 2560)
    );
    h.go_live(0).await;
    // The tile grows: upgrade once the stream has settled.
    h.sm.set_view_sizes("local", [(ID.to_string(), Some(tile(1800.0, 1000.0)))]);
    h.wait_starts(2).await;
    let c1 = h.config(1);
    assert_eq!((c1.max_size, c1.bit_rate), (2560, 16_000_000));
    let (states, _, _) = h.drain();
    let seq: Vec<_> = states.iter().map(|s| s.state).collect();
    assert_eq!(
        seq,
        [
            StreamStatus::Starting,
            StreamStatus::Live,
            StreamStatus::Resizing,
            StreamStatus::Starting
        ]
    );
    assert_eq!(states[2].reason.as_deref(), Some("resize"));
    assert_eq!(states[3].size_long, Some(2560));
    // Shrinking a lot waits 5 s before downgrading.
    h.go_live(1).await;
    let live_at = Instant::now();
    h.sm.set_view_sizes("local", [(ID.to_string(), Some(tile(600.0, 338.0)))]);
    h.wait_starts(3).await;
    assert!(
        live_at.elapsed() >= Duration::from_secs(5),
        "{:?}",
        live_at.elapsed()
    );
    assert_eq!(h.config(2).max_size, 960);
}

#[tokio::test(start_paused = true)]
async fn hidden_and_cleared_viewers() {
    let h = harness();
    h.sm.set_view_sizes("local", [(ID.to_string(), Some(tile(1800.0, 1000.0)))]);
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    assert_eq!(h.config(0).max_size, 2560);
    h.go_live(0).await;
    // Hidden everywhere: downgrade only after the longer hidden delay.
    h.sm.set_view_sizes(
        "local",
        [(
            ID.to_string(),
            Some(ViewRequest {
                visible: false,
                ..Default::default()
            }),
        )],
    );
    let t = Instant::now();
    h.wait_starts(2).await;
    assert!(t.elapsed() >= Duration::from_secs(15), "{:?}", t.elapsed());
    assert_eq!(h.config(1).max_size, 640);
    // Viewer leaves entirely: no information, nothing changes.
    h.go_live(1).await;
    h.sm.clear_viewer("local");
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(h.backend.lock().starts.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn resolution_matching_off_uses_the_preset() {
    let h = harness();
    h.store
        .write(|s| s.modify_settings(|st| st.auto_resolution = false));
    h.sm.set_view_sizes("local", [(ID.to_string(), Some(tile(300.0, 200.0)))]);
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    assert_eq!(h.config(0).max_size, 2560);
    let s = h.sm.state_of(ID).unwrap();
    assert!(!s.auto_size);
    assert_eq!(
        s.quality.as_deref(),
        Some("High (2560px · 16 Mbps · 60 fps)")
    );
}

#[tokio::test(start_paused = true)]
async fn decode_failure_reported_by_a_viewer_switches_codec() {
    let mut h = harness();
    h.sm.set_decoder_support(Some(all_hw(false)));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.go_live(0).await;
    h.drain();
    h.sm.start(
        ID,
        StartOptions {
            restart: true,
            decode_failed: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    h.wait_starts(2).await;
    assert_eq!(h.config(1).codec, Codec::H264);
    let (_, notices, _) = h.drain();
    assert_eq!(
        notices,
        ["Meta Quest 3 #1: this computer could not decode H.265 (HEVC) — switching to H.264."]
    );
}

#[tokio::test(start_paused = true)]
async fn ten_bit_decode_failure_switches_to_eight_bit() {
    let mut h = harness();
    h.store.write(|s| s.modify_settings(|st| st.ten_bit = true));
    h.sm.set_decoder_support(Some(all_hw(true)));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.go_live(0).await;
    h.drain();
    h.sm.start(
        ID,
        StartOptions {
            restart: true,
            decode_failed: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    h.wait_starts(2).await;
    assert_eq!(h.config(1).codec, Codec::H265);
    assert!(h.config(1).codec_options.is_empty());
    assert_eq!(
        h.drain().1,
        ["Meta Quest 3 #1: 10-bit H.265 (HEVC) could not be decoded — switching to 8-bit."]
    );
}

#[tokio::test(start_paused = true)]
async fn a_viewer_without_the_codec_restarts_the_stream() {
    let mut h = harness();
    h.sm.set_decoder_support(Some(all_hw(false)));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.go_live(0).await;
    assert_eq!(h.config(0).codec, Codec::H265);
    h.drain();
    let tablet: DecoderSupport = [(
        "h264".to_string(),
        DecodeCaps {
            any: true,
            hw: true,
        },
    )]
    .into_iter()
    .collect();
    h.sm.set_viewer_support("ipad", Some(tablet.clone()));
    h.wait_starts(2).await;
    assert_eq!(h.config(1).codec, Codec::H264);
    assert!(
        h.drain()
            .1
            .contains(&"Switching a stream to a codec every viewer can play.".to_string())
    );
    // Same report again: nothing to do.
    h.go_live(1).await;
    h.sm.set_viewer_support("ipad", Some(tablet));
    h.settle().await;
    assert_eq!(h.backend.lock().starts.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn single_eye_streams_crop_the_panel() {
    let h = harness();
    h.store.write(|s| {
        s.modify_settings(|st| st.single_eye_stream = true);
        s.upsert_headset(
            ID,
            serde_json::json!({ "calibration": { "eye": "right" } })
                .as_object()
                .unwrap(),
        );
    });
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    assert_eq!(h.config(0).crop.as_deref(), Some("2064:2208:2064:0"));
    assert!(h.sm.state_of(ID).unwrap().single_eye);
}

#[tokio::test(start_paused = true)]
async fn explicit_codec_notes_are_shown_once() {
    let mut h = harness();
    h.store
        .write(|s| s.modify_settings(|st| st.usb_codec = CodecPreference::Fixed(Codec::Av1)));
    h.sm.set_decoder_support(Some(all_hw(false)));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    assert_eq!(h.config(0).codec, Codec::Av1);
    assert_eq!(
        h.config(0).encoder.as_deref(),
        Some("c2.android.av1.encoder")
    );
    h.go_live(0).await;
    h.send(0, SessionEvent::End).await; // had media: plain retry, same entry
    h.wait_starts(2).await;
    let notices = h.drain().1;
    assert_eq!(
        notices,
        ["Meta Quest 3 #1: AV1 on this headset is a software encoder and may be slow."]
    );
}

#[tokio::test(start_paused = true)]
async fn encoders_are_cached_and_stored() {
    let h = harness();
    let r = h.sm.encoders_for(ID).await.unwrap();
    assert!(r.live);
    assert_eq!(r.encoders.unwrap().len(), 3);
    assert_eq!(h.store.headset(ID).unwrap().encoders.unwrap().len(), 3);
    h.backend.lock().encoders = None;
    assert!(h.sm.encoders_for(ID).await.unwrap().live, "cached");
    h.devices.add(ID, "Meta Quest 3 #1", true, &[]);
    let r = h.sm.encoders_for(ID).await.unwrap();
    assert!(!r.live);
    assert_eq!(r.encoders.unwrap()[1].codec, "h265");
}

#[tokio::test(start_paused = true)]
async fn encoder_listing_failure_means_h264_without_encoder_name() {
    let h = harness();
    h.backend.lock().encoders = None;
    h.sm.set_decoder_support(Some(all_hw(false)));
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    assert_eq!(h.config(0).codec, Codec::H264);
    assert_eq!(h.config(0).encoder, None);
}

#[tokio::test(start_paused = true)]
async fn start_all_and_stop_all() {
    let mut h = harness();
    h.devices
        .add("B", "Second", true, &[("USB2", TransportKind::Usb)]);
    h.devices
        .add("C", "Unverified", false, &[("USB3", TransportKind::Usb)]);
    let results = h.sm.start_all().await;
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.ok));
    h.wait_starts(2).await;
    h.go_live(0).await;
    h.go_live(1).await;
    assert_eq!(h.sm.start_all().await.len(), 0, "already streaming");
    h.drain();
    h.sm.stop_all().await;
    assert_eq!(h.statuses(), [StreamStatus::Stopped, StreamStatus::Stopped]);
    assert!(h.sm.states().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stream_state_json_matches_electron_keys() {
    let h = harness();
    h.sm.start(ID, StartOptions::default()).await.unwrap();
    h.wait_starts(1).await;
    h.go_live(0).await;
    let v = serde_json::to_value(h.sm.state_of(ID).unwrap()).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "adbSerial",
            "autoSize",
            "bitRate",
            "codec",
            "deviceName",
            "encoder",
            "error",
            "height",
            "id",
            "quality",
            "reason",
            "retries",
            "singleEye",
            "sizeLong",
            "state",
            "tenBit",
            "transport",
            "width"
        ]
    );
    assert_eq!(v["state"], serde_json::json!("live"));
    assert_eq!(v["codec"], serde_json::json!("h264"));
    assert_eq!(v["transport"], serde_json::json!("usb"));
    assert_eq!(v["error"], serde_json::Value::Null);
    let opts: StartOptions = serde_json::from_str(
        r#"{"restart":true,"decodeFailed":true,"transport":"wifi","codec":"h265"}"#,
    )
    .unwrap();
    assert_eq!(
        opts,
        StartOptions {
            restart: true,
            decode_failed: true,
            transport: Some(TransportPreference::Wifi),
            codec: Some(Codec::H265)
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_session_that_never_stops_is_abandoned_after_the_timeout() {
    struct Stubborn;
    impl SessionControl for Stubborn {
        fn stop(&self) {}
    }
    #[derive(Clone)]
    struct StubbornBackend(Arc<Mutex<Vec<mpsc::Sender<SessionEvent>>>>);
    impl StreamBackend for StubbornBackend {
        type Session = Stubborn;
        fn server_available(&self) -> bool {
            true
        }
        async fn list_encoders(&self, _: &str) -> Result<Vec<EncoderInfo>, CoreError> {
            Ok(vec![])
        }
        async fn panel_size(&self, _: &str) -> Result<Option<FrameSize>, CoreError> {
            Ok(None)
        }
        async fn start_session(
            &self,
            _: &str,
            _: SessionConfig,
        ) -> Result<SessionStart<Stubborn>, CoreError> {
            let (tx, rx) = mpsc::channel(4);
            self.0.lock().unwrap().push(tx);
            Ok(SessionStart {
                session: Stubborn,
                events: rx,
            })
        }
    }
    let devices = FakeDevices::default();
    devices.add(ID, "Q", true, &[("USB1", TransportKind::Usb)]);
    let (tx, mut rx) = crate::events::channel(64);
    let backend = StubbornBackend(Arc::default());
    let sm = StreamManager::new(
        backend.clone(),
        devices,
        SharedStore::in_memory(),
        tx,
        StreamManagerConfig::default(),
    );
    sm.start(ID, StartOptions::default()).await.unwrap();
    for _ in 0..100 {
        if !backend.0.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let t = Instant::now();
    sm.stop(ID).await;
    assert!(t.elapsed() >= Duration::from_secs(5));
    let mut last = None;
    while let Ok(e) = rx.try_recv() {
        if let CoreEvent::StreamState(s) = e {
            last = Some(s.state);
        }
    }
    assert_eq!(last, Some(StreamStatus::Stopped));
}
