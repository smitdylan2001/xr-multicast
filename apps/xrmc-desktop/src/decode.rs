//! Per-stream decode workers.
//!
//! The hub's event loop routes each stream's packets into a bounded channel
//! (skipping to the next key frame when the worker falls behind). One thread
//! per stream decodes them, keeps only the latest frame for the UI, wakes the
//! UI, and now and then analyses a downscaled luma image to detect the panel
//! layout (side by side, stacked, 2×2).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use xrmc_core::layout::{self, GridShape};
use xrmc_core::{Codec, StreamState, StreamStatus, VideoPacket};
use xrmc_decode::{DecodeError, Decoder, Frame, PixelFormat};

use crate::model::needs_reset;

/// Packets buffered per stream before we start skipping to the next key frame.
pub const QUEUE: usize = 180;
/// Side of the luma image analysed for layout detection (like the Electron app).
pub const DETECT_SIZE: usize = 96;
/// No frame this long after the first packet: the codec can't be decoded here.
pub const NO_FRAME_TIMEOUT: Duration = Duration::from_secs(6);

/// Makes a decoder for a codec.
pub type DecoderFactory = Arc<dyn Fn(Codec) -> Result<Box<dyn Decoder>, DecodeError> + Send + Sync>;
/// Wakes the UI.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

pub fn decode_codec(c: Codec) -> xrmc_decode::Codec {
    match c {
        Codec::H264 => xrmc_decode::Codec::H264,
        Codec::H265 => xrmc_decode::Codec::H265,
        Codec::Av1 => xrmc_decode::Codec::Av1,
    }
}

/// The real decoders from `xrmc-decode`.
pub fn system_decoders(threads: u32) -> DecoderFactory {
    Arc::new(move |c| {
        xrmc_decode::create(
            decode_codec(c),
            xrmc_decode::DecoderOptions {
                prefer_hw: true,
                threads,
            },
        )
    })
}

/// A decoded frame and the session (decoder generation) it belongs to.
#[derive(Debug)]
pub struct DecodedFrame {
    pub frame: Frame,
    pub generation: u64,
}

/// What a worker shares with the UI.
#[derive(Debug, Default)]
pub struct StreamOutput {
    latest: Mutex<Option<DecodedFrame>>,
    grid: Mutex<Option<GridShape>>,
    pub frames: AtomicU64,
    pub bytes: AtomicU64,
    pub dropped: AtomicU64,
    pub bit_depth: AtomicU8,
    pub hardware: AtomicBool,
    backend: Mutex<String>,
}

impl StreamOutput {
    /// The newest frame since the last call (older ones were already dropped).
    pub fn take_frame(&self) -> Option<DecodedFrame> {
        self.latest.lock().ok()?.take()
    }

    pub fn grid(&self) -> Option<GridShape> {
        *self.grid.lock().ok()?
    }

    pub fn backend(&self) -> String {
        self.backend.lock().map(|b| b.clone()).unwrap_or_default()
    }

    fn put(&self, f: DecodedFrame) {
        if let Ok(mut l) = self.latest.lock() {
            *l = Some(f);
        }
    }
}

/// A decoder that could not be used; the hub falls back to another codec or shows the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeFailure {
    pub id: String,
    pub generation: u64,
    pub reason: String,
}

enum Msg {
    Packet {
        codec: Codec,
        generation: u64,
        packet: VideoPacket,
    },
    Stop,
}

/// Downscaled (DETECT_SIZE²) 0..1 luma of a frame's Y plane, box-filtered.
pub fn luma_grid(frame: &Frame, size: usize) -> Option<Vec<f32>> {
    let y = frame.planes.first()?;
    let (w, h) = (y.width as usize, y.height as usize);
    if w == 0 || h == 0 || size == 0 {
        return None;
    }
    let ten = frame.format == PixelFormat::I420P10;
    let bpp = if ten { 2 } else { 1 };
    if y.data.len() < (h - 1) * y.stride + w * bpp {
        return None;
    }
    let sample = |x: usize, row: usize| -> f32 {
        let i = row * y.stride + x * bpp;
        let v = if ten {
            u16::from_le_bytes([y.data[i], y.data[i + 1]]) as f32 / 4.0
        } else {
            y.data[i] as f32
        };
        if frame.color.full_range {
            v / 255.0
        } else {
            ((v - 16.0) / 219.0).clamp(0.0, 1.0)
        }
    };
    const SUB: usize = 3;
    let mut out = vec![0.0f32; size * size];
    for gy in 0..size {
        for gx in 0..size {
            let mut sum = 0.0;
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let px = ((gx * SUB + sx) * w / (size * SUB)).min(w - 1);
                    let py = ((gy * SUB + sy) * h / (size * SUB)).min(h - 1);
                    sum += sample(px, py);
                }
            }
            out[gy * size + gx] = sum / (SUB * SUB) as f32;
        }
    }
    Some(out)
}

/// Layout detection schedule and hysteresis (`maybeDetect`).
#[derive(Debug, Default)]
pub struct Detector {
    pub grid: Option<GridShape>,
    candidate: Option<GridShape>,
    since: Option<Instant>,
    next: Option<Instant>,
}

impl Detector {
    pub fn due(&self, now: Instant) -> bool {
        self.next.is_none_or(|n| now >= n)
    }

    /// Feed one detection result. Returns true when the accepted grid changed.
    pub fn feed(&mut self, now: Instant, g: Option<GridShape>) -> bool {
        let since = *self.since.get_or_insert(now);
        let mut changed = false;
        if let Some(g) = g {
            // Only switch once the same grid was seen twice in a row.
            if self.candidate == Some(g) && self.grid != Some(g) {
                self.grid = Some(g);
                changed = true;
            }
            self.candidate = Some(g);
        }
        let age = now.duration_since(since);
        self.next = Some(
            now + if self.grid.is_none() {
                Duration::from_millis(500)
            } else if age < Duration::from_secs(30) {
                Duration::from_secs(3)
            } else {
                Duration::from_secs(30)
            },
        );
        changed
    }

    /// A new session: re-check soon, keep the last grid (avoids flicker).
    pub fn restart(&mut self) {
        self.candidate = None;
        self.since = None;
        self.next = None;
    }
}

fn bit_depth(f: &Frame) -> u8 {
    if f.format == PixelFormat::I420P10 {
        10
    } else {
        8
    }
}

struct WorkerState {
    id: String,
    factory: DecoderFactory,
    output: Arc<StreamOutput>,
    wake: Wake,
    failures: tokio::sync::mpsc::UnboundedSender<DecodeFailure>,
    decoder: Option<(Codec, Box<dyn Decoder>)>,
    generation: u64,
    first_packet: Option<Instant>,
    got_frame: bool,
    errors: u32,
    failed: bool,
    detector: Detector,
}

impl WorkerState {
    fn fail(&mut self, reason: String) {
        if !self.failed {
            self.failed = true;
            tracing::warn!("[{}] decode failed: {reason}", self.id);
            let _ = self.failures.send(DecodeFailure {
                id: self.id.clone(),
                generation: self.generation,
                reason,
            });
        }
    }

    fn packet(&mut self, codec: Codec, generation: u64, p: VideoPacket) {
        if generation != self.generation {
            self.generation = generation;
            self.decoder = None;
            self.first_packet = None;
            self.got_frame = false;
            self.errors = 0;
            self.failed = false;
            self.detector.restart();
        }
        self.output
            .bytes
            .fetch_add(p.data.len() as u64, Ordering::Relaxed);
        if self.failed {
            return;
        }
        if self.decoder.as_ref().is_none_or(|(c, _)| *c != codec) {
            match (self.factory)(codec) {
                Ok(d) => {
                    if let Ok(mut b) = self.output.backend.lock() {
                        *b = d.backend().to_string();
                    }
                    self.output
                        .hardware
                        .store(d.is_hardware(), Ordering::Relaxed);
                    self.decoder = Some((codec, d));
                }
                Err(e) => {
                    self.fail(e.to_string());
                    return;
                }
            }
        }
        let now = Instant::now();
        if !p.config {
            self.first_packet.get_or_insert(now);
        }
        let Some((_, dec)) = self.decoder.as_mut() else {
            return;
        };
        match dec.push(&p.data, p.pts, p.key, p.config) {
            Ok(frames) => {
                self.errors = 0;
                if let Some(frame) = frames.into_iter().last() {
                    self.got_frame = true;
                    self.output.frames.fetch_add(1, Ordering::Relaxed);
                    self.output
                        .bit_depth
                        .store(bit_depth(&frame), Ordering::Relaxed);
                    if self.detector.due(now) {
                        let g = luma_grid(&frame, DETECT_SIZE).and_then(|l| {
                            layout::detect_grid(&l, DETECT_SIZE, DETECT_SIZE).map(|g| g.shape())
                        });
                        if self.detector.feed(now, g)
                            && let Ok(mut slot) = self.output.grid.lock()
                        {
                            *slot = self.detector.grid;
                        }
                    }
                    self.output.put(DecodedFrame {
                        frame,
                        generation: self.generation,
                    });
                    (self.wake)();
                }
            }
            Err(e) => {
                self.errors += 1;
                tracing::debug!("[{}] decoder error: {e}", self.id);
                if self.errors >= 3 {
                    self.fail(format!("decoder error: {e}"));
                }
            }
        }
        if !self.got_frame
            && self
                .first_packet
                .is_some_and(|t| now.duration_since(t) > NO_FRAME_TIMEOUT)
        {
            self.fail("no frames decoded".into());
        }
    }
}

fn run_worker(mut st: WorkerState, rx: Receiver<Msg>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Packet {
                codec,
                generation,
                packet,
            } => st.packet(codec, generation, packet),
            Msg::Stop => break,
        }
    }
}

struct Worker {
    tx: SyncSender<Msg>,
    output: Arc<StreamOutput>,
    join: Option<JoinHandle<()>>,
    generation: u64,
    need_key: bool,
    last_config: Option<VideoPacket>,
    state: Option<StreamState>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Unblock the thread even when the queue is full.
        let _ = self.tx.try_send(Msg::Stop);
        if let Some(j) = self.join.take() {
            // Don't block the event loop on a slow decoder; the thread ends on its own.
            drop(j);
        }
    }
}

/// Routes packets to per-stream workers and tracks their sessions.
pub struct Router {
    workers: HashMap<String, Worker>,
    outputs: Arc<Mutex<HashMap<String, Arc<StreamOutput>>>>,
    factory: DecoderFactory,
    wake: Wake,
    failures: tokio::sync::mpsc::UnboundedSender<DecodeFailure>,
    next_generation: u64,
    queue: usize,
}

impl Router {
    pub fn new(
        factory: DecoderFactory,
        wake: Wake,
        failures: tokio::sync::mpsc::UnboundedSender<DecodeFailure>,
    ) -> Router {
        Router {
            workers: HashMap::new(),
            outputs: Arc::default(),
            factory,
            wake,
            failures,
            next_generation: 1,
            queue: QUEUE,
        }
    }

    #[cfg(test)]
    fn with_queue(mut self, queue: usize) -> Router {
        self.queue = queue;
        self
    }

    /// Outputs by stream id, shared with the UI.
    pub fn outputs(&self) -> Arc<Mutex<HashMap<String, Arc<StreamOutput>>>> {
        self.outputs.clone()
    }

    fn spawn(&mut self, id: &str) -> &mut Worker {
        let (tx, rx) = mpsc::sync_channel(self.queue);
        let output = Arc::new(StreamOutput::default());
        let generation = self.next_generation;
        self.next_generation += 1;
        let st = WorkerState {
            id: id.to_string(),
            factory: self.factory.clone(),
            output: output.clone(),
            wake: self.wake.clone(),
            failures: self.failures.clone(),
            decoder: None,
            generation,
            first_packet: None,
            got_frame: false,
            errors: 0,
            failed: false,
            detector: Detector::default(),
        };
        let join = std::thread::Builder::new()
            .name(format!("decode-{id}"))
            .spawn(move || run_worker(st, rx))
            .ok();
        if let Ok(mut o) = self.outputs.lock() {
            o.insert(id.to_string(), output.clone());
        }
        self.workers.entry(id.to_string()).or_insert(Worker {
            tx,
            output,
            join,
            generation,
            need_key: false,
            last_config: None,
            state: None,
        })
    }

    /// A stream changed state. Returns the decoder generation of its current session.
    pub fn on_state(&mut self, st: &StreamState) -> u64 {
        if st.state == StreamStatus::Stopped {
            self.remove(&st.id);
            return 0;
        }
        if !self.workers.contains_key(&st.id) {
            self.spawn(&st.id);
        }
        let next = self.next_generation;
        let w = self.workers.get_mut(&st.id).expect("worker exists");
        if needs_reset(w.state.as_ref(), st) {
            w.generation = next;
            self.next_generation += 1;
            w.last_config = None;
            w.need_key = false;
        }
        w.state = Some(st.clone());
        w.generation
    }

    pub fn remove(&mut self, id: &str) {
        self.workers.remove(id);
        if let Ok(mut o) = self.outputs.lock() {
            o.remove(id);
        }
    }

    pub fn generation(&self, id: &str) -> Option<u64> {
        self.workers.get(id).map(|w| w.generation)
    }

    pub fn state(&self, id: &str) -> Option<&StreamState> {
        self.workers.get(id).and_then(|w| w.state.as_ref())
    }

    /// Packets were lost (the event channel lagged): wait for key frames.
    pub fn resync_all(&mut self) {
        for w in self.workers.values_mut() {
            w.need_key = true;
        }
    }

    /// Hand a packet to its stream's worker.
    pub fn route(&mut self, id: &str, packet: VideoPacket) {
        let Some(w) = self.workers.get_mut(id) else {
            return;
        };
        let codec = w
            .state
            .as_ref()
            .and_then(|s| s.codec)
            .unwrap_or(Codec::H264);
        if packet.config {
            w.last_config = Some(packet.clone());
        } else if w.need_key {
            if !packet.key {
                w.output.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // Resume at a key frame, with the codec configuration in front of it.
            if let Some(cfg) = w.last_config.clone()
                && w.tx
                    .try_send(Msg::Packet {
                        codec,
                        generation: w.generation,
                        packet: cfg,
                    })
                    .is_err()
            {
                w.output.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            w.need_key = false;
        }
        let msg = Msg::Packet {
            codec,
            generation: w.generation,
            packet,
        };
        match w.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                w.output.dropped.fetch_add(1, Ordering::Relaxed);
                w.need_key = true;
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use xrmc_decode::{ColorInfo, Plane};

    /// A "decoder" that turns each non-config packet into a frame whose size is in the payload.
    pub struct FakeDecoder;

    impl Decoder for FakeDecoder {
        fn push(
            &mut self,
            data: &[u8],
            pts: u64,
            _key: bool,
            config: bool,
        ) -> Result<Vec<Frame>, DecodeError> {
            if config {
                return Ok(vec![]);
            }
            if data == b"bad" {
                return Err(DecodeError::Decode("bad".into()));
            }
            let mut f = stereo_frame(64, 32);
            f.pts = pts;
            Ok(vec![f])
        }
        fn backend(&self) -> &str {
            "fake"
        }
    }

    /// Two bright eye discs side by side on black.
    pub fn stereo_frame(w: u32, h: u32) -> Frame {
        let (wu, hu) = (w as usize, h as usize);
        let mut y = vec![16u8; wu * hu];
        for row in 0..hu {
            for col in 0..wu {
                let half = wu / 2;
                let cx = if col < half {
                    half / 2
                } else {
                    half + half / 2
                };
                let dx = (col as f32 - cx as f32) / (half as f32 * 0.45);
                let dy = (row as f32 - hu as f32 / 2.0) / (hu as f32 * 0.45);
                if dx * dx + dy * dy <= 1.0 {
                    y[row * wu + col] = 200;
                }
            }
        }
        let (cw, ch) = (wu.div_ceil(2), hu.div_ceil(2));
        Frame {
            width: w,
            height: h,
            format: PixelFormat::I420,
            color: ColorInfo::default(),
            planes: vec![
                Plane {
                    data: y,
                    stride: wu,
                    width: w,
                    height: h,
                },
                Plane {
                    data: vec![128; cw * ch],
                    stride: cw,
                    width: cw as u32,
                    height: ch as u32,
                },
                Plane {
                    data: vec![128; cw * ch],
                    stride: cw,
                    width: cw as u32,
                    height: ch as u32,
                },
            ],
            pts: 0,
        }
    }

    fn pkt(key: bool, config: bool, data: &'static [u8]) -> VideoPacket {
        VideoPacket {
            config,
            key,
            pts: 0,
            data: Bytes::from_static(data),
        }
    }

    fn st(id: &str, s: StreamStatus, codec: Codec) -> StreamState {
        StreamState {
            id: id.into(),
            state: s,
            transport: None,
            adb_serial: None,
            width: None,
            height: None,
            device_name: None,
            quality: None,
            bit_rate: None,
            codec: Some(codec),
            ten_bit: false,
            encoder: None,
            single_eye: false,
            retries: 0,
            reason: None,
            size_long: None,
            auto_size: false,
            error: None,
        }
    }

    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(5) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn luma_grid_finds_two_eyes() {
        let f = stereo_frame(192, 96);
        let l = luma_grid(&f, DETECT_SIZE).unwrap();
        let g = layout::detect_grid(&l, DETECT_SIZE, DETECT_SIZE).unwrap();
        assert_eq!((g.cols, g.rows), (2, 1));
    }

    #[test]
    fn detector_needs_two_agreeing_results() {
        let mut d = Detector::default();
        let t = Instant::now();
        let g = Some(GridShape { cols: 2, rows: 1 });
        assert!(d.due(t));
        assert!(!d.feed(t, g));
        assert!(!d.due(t + Duration::from_millis(100)));
        assert!(d.due(t + Duration::from_millis(500)));
        assert!(d.feed(t + Duration::from_millis(500), g));
        assert_eq!(d.grid, g);
        // Once known: every 3 s, after 30 s every 30 s.
        assert!(!d.due(t + Duration::from_secs(3)));
        assert!(d.due(t + Duration::from_millis(3501)));
        d.feed(t + Duration::from_secs(31), g);
        assert!(!d.due(t + Duration::from_secs(40)));
        assert!(d.due(t + Duration::from_secs(61)));
    }

    #[test]
    fn workers_decode_detect_and_keep_the_latest_frame() {
        let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel();
        let woke = Arc::new(AtomicU64::new(0));
        let w2 = woke.clone();
        let mut r = Router::new(
            Arc::new(|_| Ok(Box::new(FakeDecoder) as Box<dyn Decoder>)),
            Arc::new(move || {
                w2.fetch_add(1, Ordering::Relaxed);
            }),
            ftx,
        );
        let g = r.on_state(&st("A", StreamStatus::Starting, Codec::H265));
        assert!(g > 0);
        let out = r.outputs().lock().unwrap().get("A").cloned().unwrap();
        r.route("A", pkt(false, true, b"cfg"));
        for _ in 0..3 {
            r.route("A", pkt(true, false, b"frame"));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(wait_for(|| out.frames.load(Ordering::Relaxed) == 3));
        assert!(woke.load(Ordering::Relaxed) >= 3);
        let f = out.take_frame().unwrap();
        assert_eq!((f.frame.width, f.generation), (64, g));
        assert!(out.take_frame().is_none(), "only the latest frame is kept");
        // Detection ran on the first frame and confirmed on a later one (500 ms later).
        std::thread::sleep(Duration::from_millis(550));
        r.route("A", pkt(false, false, b"frame"));
        assert!(wait_for(
            || out.grid() == Some(GridShape { cols: 2, rows: 1 })
        ));
        assert_eq!(out.backend(), "fake");

        // A new session gets a new generation.
        r.on_state(&st("A", StreamStatus::Live, Codec::H265));
        let g2 = r.on_state(&st("A", StreamStatus::Starting, Codec::H265));
        assert!(g2 > g);
        r.on_state(&st("A", StreamStatus::Stopped, Codec::H265));
        assert!(r.outputs().lock().unwrap().is_empty());
    }

    #[test]
    fn decoder_errors_and_missing_decoders_are_reported() {
        let (ftx, mut frx) = tokio::sync::mpsc::unbounded_channel();
        let mut r = Router::new(
            Arc::new(|c| match c {
                Codec::H264 => Ok(Box::new(FakeDecoder) as Box<dyn Decoder>),
                other => Err(DecodeError::Unsupported(decode_codec(other))),
            }),
            Arc::new(|| {}),
            ftx,
        );
        let g = r.on_state(&st("A", StreamStatus::Starting, Codec::H265));
        r.route("A", pkt(true, false, b"frame"));
        r.route("A", pkt(true, false, b"frame"));
        let f = wait_for_failure(&mut frx);
        assert_eq!(f.generation, g);
        assert!(f.reason.contains("no decoder"), "{}", f.reason);

        let g = r.on_state(&st("B", StreamStatus::Starting, Codec::H264));
        for _ in 0..3 {
            r.route("B", pkt(false, false, b"bad"));
        }
        let f = wait_for_failure(&mut frx);
        assert_eq!((f.id.as_str(), f.generation), ("B", g));
        assert!(f.reason.contains("decoder error"));
    }

    fn wait_for_failure(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<DecodeFailure>,
    ) -> DecodeFailure {
        let t = Instant::now();
        loop {
            if let Ok(f) = rx.try_recv() {
                return f;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "no failure reported");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// A decoder that blocks until released, to fill the queue.
    struct Blocked(Arc<Mutex<()>>);
    impl Decoder for Blocked {
        fn push(&mut self, _: &[u8], _: u64, _: bool, _: bool) -> Result<Vec<Frame>, DecodeError> {
            let _g = self.0.lock().unwrap();
            Ok(vec![stereo_frame(8, 8)])
        }
        fn backend(&self) -> &str {
            "blocked"
        }
    }

    #[test]
    fn overflow_skips_to_the_next_key_frame() {
        let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel();
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        let g2 = gate.clone();
        let mut r = Router::new(
            Arc::new(move |_| Ok(Box::new(Blocked(g2.clone())) as Box<dyn Decoder>)),
            Arc::new(|| {}),
            ftx,
        )
        .with_queue(4);
        r.on_state(&st("A", StreamStatus::Live, Codec::H264));
        let out = r.outputs().lock().unwrap().get("A").cloned().unwrap();
        r.route("A", pkt(false, true, b"cfg"));
        // Let the worker take the config and block in the decoder.
        std::thread::sleep(Duration::from_millis(100));
        for _ in 0..10 {
            r.route("A", pkt(false, false, b"p"));
        }
        let dropped = out.dropped.load(Ordering::Relaxed);
        assert!(dropped >= 5, "dropped {dropped}");
        // Still waiting for a key frame: P-frames are skipped without queueing.
        drop(held);
        // The config and the 4 queued packets.
        assert!(wait_for(|| out.frames.load(Ordering::Relaxed) == 5));
        r.route("A", pkt(false, false, b"p"));
        assert_eq!(out.dropped.load(Ordering::Relaxed), dropped + 1);
        let before = out.frames.load(Ordering::Relaxed);
        r.route("A", pkt(true, false, b"k"));
        // The config is re-sent in front of the key frame: 2 more frames from the fake.
        assert!(wait_for(|| out.frames.load(Ordering::Relaxed) == before + 2));
    }
}
