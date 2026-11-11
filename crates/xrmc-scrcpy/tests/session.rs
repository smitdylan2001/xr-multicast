//! Session tests against an in-memory fake device.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use xrmc_adb::{AdbDevice, AdbError};
use xrmc_scrcpy::{
    Backpressure, DemuxError, Error, REMOTE_LIST_SERVER_PATH, REMOTE_SERVER_PATH, Scid,
    ServerOptions, Session, SessionConfig, SessionEvent, VideoCodec, list_encoders,
    list_encoders_command,
};

const JAR: &[u8] = b"PK\x03\x04 fake scrcpy-server v5.0.1";
const H264: u32 = 0x6832_3634;
const INFO: &str = "[server] INFO: Device: [Oculus] oculus Quest 3 (Android 14)";

// ------------------------------------------------------------ stream bytes

fn device_meta(name: &str) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[..name.len()].copy_from_slice(name.as_bytes());
    b
}

fn session(w: u32, h: u32) -> Vec<u8> {
    [0x8000_0000u32, w, h]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect()
}

fn media(data: &[u8], config: bool, key: bool, pts: u64) -> Vec<u8> {
    let mut v = if config { 1u64 << 62 } else { pts };
    if key {
        v |= 1 << 61;
    }
    let mut out = v.to_be_bytes().to_vec();
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out
}

const SPS: [u8; 15] = [
    0, 0, 0, 1, 0x67, 0x64, 0x00, 0x33, 0xaa, 0, 0, 0, 1, 0x68, 0xce,
];

fn header(codec: u32) -> Vec<u8> {
    [
        device_meta("Quest 3"),
        codec.to_be_bytes().to_vec(),
        session(3664, 1920),
    ]
    .concat()
}

/// The stream from test/stream-parser.test.js.
fn standard_stream() -> Vec<u8> {
    [
        header(H264),
        media(&SPS, true, false, 0),
        media(&[7; 5000], false, true, 16666),
        media(&[3; 300], false, false, 33333),
        session(1920, 1006),
        media(&[3; 300], false, false, 50000),
    ]
    .concat()
}

// ------------------------------------------------------------- fake device

/// How the fake scrcpy-server behaves.
#[derive(Default, Clone)]
struct Script {
    /// Opens refused before the server listens.
    refuse_opens: usize,
    /// Never listen.
    refuse_forever: bool,
    /// After the refusals, opens that connect but close before the dummy byte.
    close_before_dummy: usize,
    /// Server output at start (written in small chunks).
    log: Vec<&'static str>,
    /// The server exits right after its log (crash before connecting).
    exit_after_log: bool,
    /// Video bytes after the dummy byte; the server waits for `gate` between phases.
    phases: Vec<Vec<u8>>,
    /// After the last phase: print this line, then exit (closing shell and video).
    crash_after: Option<&'static str>,
    /// After the last phase: close the video socket but keep running.
    close_video_after: bool,
    /// Output of the one-shot `shell` command.
    shell_output: &'static str,
}

#[derive(Default)]
struct Record {
    pushes: Mutex<Vec<(String, Vec<u8>, u32)>>,
    shell_streams: Mutex<Vec<String>>,
    shells: Mutex<Vec<String>>,
    opens: Mutex<Vec<String>>,
    video_closed: AtomicBool,
    shell_closed: AtomicBool,
    /// Releases the next video phase.
    gate: Notify,
    crash: Notify,
}

struct FakeDevice {
    script: Script,
    record: Arc<Record>,
    refusals: AtomicUsize,
    early_closes: AtomicUsize,
}

impl FakeDevice {
    fn new(script: Script) -> Self {
        Self {
            refusals: AtomicUsize::new(script.refuse_opens),
            early_closes: AtomicUsize::new(script.close_before_dummy),
            script,
            record: Arc::default(),
        }
    }

    fn opens(&self) -> Vec<String> {
        self.record.opens.lock().unwrap().clone()
    }

    /// Wait (in virtual time) until the server side saw both streams close.
    async fn wait_closed(&self) {
        for _ in 0..1000 {
            if self.record.video_closed.load(Ordering::SeqCst)
                && self.record.shell_closed.load(Ordering::SeqCst)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!(
            "streams not closed: video {} shell {}",
            self.record.video_closed.load(Ordering::SeqCst),
            self.record.shell_closed.load(Ordering::SeqCst)
        );
    }
}

/// Decrement the counter if it is positive.
fn take_one(counter: &AtomicUsize) -> bool {
    let mut cur = counter.load(Ordering::SeqCst);
    while cur > 0 {
        match counter.compare_exchange(cur, cur - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => cur = now,
        }
    }
    false
}

/// Read until the peer closes its end.
async fn read_to_close(s: &mut DuplexStream) {
    let mut buf = [0u8; 256];
    while let Ok(n) = s.read(&mut buf).await {
        if n == 0 {
            break;
        }
    }
}

impl AdbDevice for FakeDevice {
    type Stream = DuplexStream;

    fn serial(&self) -> &str {
        "fake-quest"
    }

    async fn shell(&self, command: &str) -> xrmc_adb::Result<Vec<u8>> {
        self.record.shells.lock().unwrap().push(command.to_owned());
        Ok(self.script.shell_output.as_bytes().to_vec())
    }

    async fn shell_stream(&self, command: &str) -> xrmc_adb::Result<DuplexStream> {
        self.record
            .shell_streams
            .lock()
            .unwrap()
            .push(command.to_owned());
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let script = self.script.clone();
        let rec = self.record.clone();
        tokio::spawn(async move {
            let text: String = script.log.iter().map(|l| format!("{l}\r\n")).collect();
            for chunk in text.as_bytes().chunks(5) {
                let _ = server.write_all(chunk).await;
            }
            if script.exit_after_log {
                return;
            }
            tokio::select! {
                _ = read_to_close(&mut server) => rec.shell_closed.store(true, Ordering::SeqCst),
                _ = rec.crash.notified() => {
                    let line = format!("{}\n", script.crash_after.unwrap_or_default());
                    let _ = server.write_all(line.as_bytes()).await;
                }
            }
        });
        Ok(client)
    }

    async fn push(&self, data: &[u8], remote_path: &str, mode: u32) -> xrmc_adb::Result<()> {
        self.record
            .pushes
            .lock()
            .unwrap()
            .push((remote_path.to_owned(), data.to_vec(), mode));
        Ok(())
    }

    async fn open(&self, service: &str) -> xrmc_adb::Result<DuplexStream> {
        self.record.opens.lock().unwrap().push(service.to_owned());
        if self.script.refuse_forever || take_one(&self.refusals) {
            return Err(AdbError::Refused(format!("{service}: Connection refused")));
        }
        let (client, mut server) = tokio::io::duplex(4096);
        if take_one(&self.early_closes) {
            return Ok(client); // `server` dropped: EOF before the dummy byte
        }
        let script = self.script.clone();
        let rec = self.record.clone();
        tokio::spawn(async move {
            let _ = server.write_all(&[0]).await; // dummy byte
            for (i, phase) in script.phases.iter().enumerate() {
                if i > 0 {
                    rec.gate.notified().await;
                }
                if server.write_all(phase).await.is_err() {
                    rec.video_closed.store(true, Ordering::SeqCst);
                    return;
                }
            }
            if script.crash_after.is_some() {
                rec.crash.notify_one();
                rec.video_closed.store(true, Ordering::SeqCst);
                return;
            }
            if script.close_video_after {
                rec.video_closed.store(true, Ordering::SeqCst);
                return;
            }
            read_to_close(&mut server).await;
            rec.video_closed.store(true, Ordering::SeqCst);
        });
        Ok(client)
    }
}

// ------------------------------------------------------------------ helpers

fn options() -> ServerOptions {
    ServerOptions {
        scid: Scid::new(0x0a1b2c3d),
        ..ServerOptions::default()
    }
}

/// Next non-log event (with a generous virtual-time limit).
async fn next(rx: &mut mpsc::Receiver<SessionEvent>) -> SessionEvent {
    loop {
        match tokio::time::timeout(Duration::from_secs(30), rx.recv()).await {
            Ok(Some(SessionEvent::Log(_))) => continue,
            Ok(Some(ev)) => return ev,
            Ok(None) => panic!("event channel closed"),
            Err(_) => panic!("no event"),
        }
    }
}

async fn ended(rx: &mut mpsc::Receiver<SessionEvent>) -> Option<Error> {
    loop {
        if let SessionEvent::Ended(e) = next(rx).await {
            return e;
        }
    }
}

fn packet(ev: SessionEvent) -> (bool, bool, u64, usize) {
    match ev {
        SessionEvent::Packet(p) => (p.config, p.key, p.pts, p.data.len()),
        other => panic!("expected a packet, got {other:?}"),
    }
}

// -------------------------------------------------------------------- tests

#[tokio::test(start_paused = true)]
async fn retries_until_the_server_listens_then_streams_events() {
    let dev = FakeDevice::new(Script {
        refuse_opens: 3,
        close_before_dummy: 1,
        log: vec![INFO],
        phases: vec![standard_stream()],
        ..Script::default()
    });
    let opts = options();
    let (handle, mut rx) = Session::start(&dev, JAR, opts.clone()).await.unwrap();
    assert_eq!(handle.scid(), Scid::new(0x0a1b2c3d));

    assert_eq!(
        *dev.record.pushes.lock().unwrap(),
        [(REMOTE_SERVER_PATH.to_owned(), JAR.to_vec(), 0o644)]
    );
    let commands = dev.record.shell_streams.lock().unwrap().clone();
    assert_eq!(commands, [opts.command()]);
    assert!(commands[0].starts_with(
        "CLASSPATH=/data/local/tmp/xrmc-scrcpy-server.jar app_process / com.genymobile.scrcpy.Server 5.0.1 scid=0a1b2c3d "
    ));
    assert_eq!(
        dev.opens(),
        vec!["localabstract:scrcpy_0a1b2c3d".to_owned(); 5]
    );

    // Log lines arrive as events too (and are kept in the tail).
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&first, SessionEvent::Log(l) if l == INFO),
        "{first:?}"
    );
    assert_eq!(handle.log_tail(), [INFO]);

    assert!(matches!(next(&mut rx).await, SessionEvent::DeviceName(n) if n == "Quest 3"));
    assert!(matches!(
        next(&mut rx).await,
        SessionEvent::Codec(VideoCodec::H264)
    ));
    assert!(matches!(
        next(&mut rx).await,
        SessionEvent::VideoSize {
            width: 3664,
            height: 1920,
            client_resized: false
        }
    ));
    let SessionEvent::Packet(config) = next(&mut rx).await else {
        panic!()
    };
    assert!(config.config && !config.key);
    assert_eq!(&config.data[..], SPS);
    assert_eq!(packet(next(&mut rx).await), (false, true, 16666, 5000));
    assert_eq!(packet(next(&mut rx).await), (false, false, 33333, 300));
    assert!(matches!(
        next(&mut rx).await,
        SessionEvent::VideoSize {
            width: 1920,
            height: 1006,
            ..
        }
    ));
    assert_eq!(packet(next(&mut rx).await), (false, false, 50000, 300));

    let stats = handle.stats();
    assert_eq!(stats.bytes_received, standard_stream().len() as u64);
    assert_eq!((stats.packets, stats.packets_dropped), (4, 0));
    assert!(!handle.is_finished());

    handle.stop().await;
    assert!(handle.is_finished());
    assert!(ended(&mut rx).await.is_none());
    assert!(rx.recv().await.is_none(), "Ended is the last event");
    dev.wait_closed().await;
}

#[tokio::test(start_paused = true)]
async fn server_exit_before_connect_reports_its_last_error_line() {
    let error = "[server] ERROR: Encoder error: c2.qti.hevc.encoder could not be configured";
    let dev = FakeDevice::new(Script {
        refuse_forever: true,
        log: vec![INFO, error, "[server] INFO: Cleaning up"],
        exit_after_log: true,
        ..Script::default()
    });
    let started = Instant::now();
    let Err(e) = Session::start(&dev, JAR, options()).await else {
        panic!("should fail")
    };
    let Error::Server { message, log } = &e else {
        panic!("{e:?}")
    };
    assert_eq!(message, error);
    assert_eq!(e.to_string(), error);
    assert_eq!(log.len(), 3);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "gave up early: {:?}",
        started.elapsed()
    );
}

#[tokio::test(start_paused = true)]
async fn server_exit_without_error_line_reports_its_last_line() {
    let dev = FakeDevice::new(Script {
        refuse_forever: true,
        log: vec![INFO, "Aborted"],
        exit_after_log: true,
        ..Script::default()
    });
    let Err(Error::Server { message, .. }) = Session::start(&dev, JAR, options()).await else {
        panic!()
    };
    assert_eq!(message, "Aborted");
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_the_connect_timeout() {
    let dev = FakeDevice::new(Script {
        refuse_forever: true,
        log: vec![INFO],
        ..Script::default()
    });
    let started = Instant::now();
    let Err(e) = Session::start(&dev, JAR, options()).await else {
        panic!("should fail")
    };
    assert!(
        matches!(&e, Error::Connect(reason) if reason.contains("Connection refused")),
        "{e:?}"
    );
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(5700) && took <= Duration::from_secs(6),
        "{took:?}"
    );
    // 100 ms backoff 10 times, then 250 ms.
    assert!(
        (25..=35).contains(&dev.opens().len()),
        "{}",
        dev.opens().len()
    );
    // The server is told to quit.
    for _ in 0..100 {
        if dev.record.shell_closed.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("shell stream not closed");
}

#[tokio::test(start_paused = true)]
async fn connect_timeout_prefers_a_logged_server_error() {
    let error = "[server] ERROR: Could not open video stream";
    let dev = FakeDevice::new(Script {
        refuse_forever: true,
        log: vec![INFO, error],
        ..Script::default()
    });
    let config = SessionConfig {
        connect_timeout: Duration::from_secs(1),
        ..SessionConfig::default()
    };
    let Err(Error::Server { message, .. }) =
        Session::start_with(&dev, JAR, options(), config).await
    else {
        panic!()
    };
    assert_eq!(message, error);
}

#[tokio::test(start_paused = true)]
async fn server_crash_while_streaming_ends_with_its_error() {
    let error = "[server] ERROR: Exception on thread Thread[video]: MediaCodec died";
    let dev = FakeDevice::new(Script {
        log: vec![INFO],
        phases: vec![standard_stream()],
        crash_after: Some(error),
        ..Script::default()
    });
    let (handle, mut rx) = Session::start(&dev, JAR, options()).await.unwrap();
    match ended(&mut rx).await {
        Some(Error::Server { message, log }) => {
            assert_eq!(message, error);
            assert_eq!(log, [INFO, error]);
        }
        other => panic!("{other:?}"),
    }
    handle.wait().await;
    assert!(handle.is_finished());
}

#[tokio::test(start_paused = true)]
async fn video_close_without_server_error_is_reported_and_stops_the_server() {
    let dev = FakeDevice::new(Script {
        log: vec![INFO],
        phases: vec![standard_stream()],
        close_video_after: true,
        ..Script::default()
    });
    let (_handle, mut rx) = Session::start(&dev, JAR, options()).await.unwrap();
    assert!(matches!(ended(&mut rx).await, Some(Error::Closed)));
    dev.wait_closed().await;
}

#[tokio::test(start_paused = true)]
async fn codec_mismatch_ends_the_session() {
    let dev = FakeDevice::new(Script {
        phases: vec![standard_stream()],
        ..Script::default()
    });
    let opts = ServerOptions {
        codec: VideoCodec::H265,
        ..options()
    };
    assert!(dev.record.shell_streams.lock().unwrap().is_empty());
    let (_handle, mut rx) = Session::start(&dev, JAR, opts).await.unwrap();
    assert!(dev.record.shell_streams.lock().unwrap()[0].contains(" video_codec=h265 "));
    assert!(matches!(next(&mut rx).await, SessionEvent::DeviceName(_)));
    match ended(&mut rx).await {
        Some(
            e @ Error::CodecMismatch {
                requested: VideoCodec::H265,
                ..
            },
        ) => {
            assert_eq!(e.to_string(), "Requested h265 but the headset sent h264");
        }
        other => panic!("{other:?}"),
    }
    dev.wait_closed().await;
}

#[tokio::test(start_paused = true)]
async fn encoder_failure_codec_id_ends_the_session() {
    let dev = FakeDevice::new(Script {
        phases: vec![[device_meta("Quest 3"), 1u32.to_be_bytes().to_vec()].concat()],
        ..Script::default()
    });
    let (_handle, mut rx) = Session::start(&dev, JAR, options()).await.unwrap();
    assert!(matches!(
        ended(&mut rx).await,
        Some(Error::Demux(DemuxError::EncoderFailed))
    ));
}

#[tokio::test(start_paused = true)]
async fn slow_consumer_drops_packets_until_the_next_key_frame() {
    // Phase 1: config, key, 20 deltas, key, 5 deltas. Phase 2: key, delta.
    let mut phase1 = [
        header(H264),
        media(&SPS, true, false, 0),
        media(&[1; 100], false, true, 0),
    ]
    .concat();
    for i in 1..=20 {
        phase1.extend(media(&[2; 50], false, false, i));
    }
    phase1.extend(media(&[1; 100], false, true, 21));
    for i in 22..=26 {
        phase1.extend(media(&[2; 50], false, false, i));
    }
    let phase2 = [
        media(&[1; 100], false, true, 100),
        media(&[2; 50], false, false, 101),
    ]
    .concat();
    let dev = FakeDevice::new(Script {
        phases: vec![phase1.clone(), phase2],
        ..Script::default()
    });
    let config = SessionConfig {
        event_capacity: 8,
        ..SessionConfig::default()
    };
    let (handle, mut rx) = Session::start_with(&dev, JAR, options(), config)
        .await
        .unwrap();

    // Let the session read all of phase 1 without consuming anything.
    while handle.stats().bytes_received < phase1.len() as u64 {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
    // 8 slots: device name, codec, size, config, key, 3 deltas; the rest is dropped.
    assert_eq!(handle.stats().packets_dropped, 17 + 1 + 5);
    assert!(matches!(next(&mut rx).await, SessionEvent::DeviceName(_)));
    assert!(matches!(next(&mut rx).await, SessionEvent::Codec(_)));
    assert!(matches!(
        next(&mut rx).await,
        SessionEvent::VideoSize { .. }
    ));
    assert!(packet(next(&mut rx).await).0);
    assert!(packet(next(&mut rx).await).1);
    for pts in 1..=3 {
        assert_eq!(packet(next(&mut rx).await), (false, false, pts, 50));
    }
    // Consumer caught up: the stream resumes at the next key frame.
    dev.record.gate.notify_one();
    assert_eq!(packet(next(&mut rx).await), (false, true, 100, 100));
    assert_eq!(packet(next(&mut rx).await), (false, false, 101, 50));
    assert_eq!(handle.stats().packets, 1 + 1 + 3 + 2);
}

#[tokio::test(start_paused = true)]
async fn wait_policy_never_drops() {
    let mut stream = [
        header(H264),
        media(&SPS, true, false, 0),
        media(&[1; 1000], false, true, 0),
    ]
    .concat();
    for i in 1..=60 {
        stream.extend(media(&[2; 700], false, false, i));
    }
    let dev = FakeDevice::new(Script {
        phases: vec![stream],
        ..Script::default()
    });
    let config = SessionConfig {
        event_capacity: 2,
        backpressure: Backpressure::Wait,
        ..SessionConfig::default()
    };
    let (handle, mut rx) = Session::start_with(&dev, JAR, options(), config)
        .await
        .unwrap();
    for _ in 0..3 {
        next(&mut rx).await;
    }
    for i in 0..=61 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let (_, _, pts, _) = packet(next(&mut rx).await);
        assert_eq!(pts, i.max(1) - 1);
    }
    assert_eq!(handle.stats().packets_dropped, 0);
}

#[tokio::test(start_paused = true)]
async fn stop_works_while_the_consumer_is_not_reading() {
    let mut stream = header(H264);
    for i in 0..50 {
        stream.extend(media(&[2; 700], false, i == 0, i));
    }
    let dev = FakeDevice::new(Script {
        phases: vec![stream],
        ..Script::default()
    });
    let config = SessionConfig {
        event_capacity: 1,
        backpressure: Backpressure::Wait,
        ..SessionConfig::default()
    };
    let (handle, _rx) = Session::start_with(&dev, JAR, options(), config)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(10), handle.stop())
        .await
        .expect("stop must not hang");
    dev.wait_closed().await;
}

#[tokio::test(start_paused = true)]
async fn dropping_the_receiver_stops_the_session() {
    let dev = FakeDevice::new(Script {
        phases: vec![standard_stream()],
        ..Script::default()
    });
    let (handle, rx) = Session::start(&dev, JAR, options()).await.unwrap();
    drop(rx);
    tokio::time::timeout(Duration::from_secs(10), handle.wait())
        .await
        .unwrap();
    dev.wait_closed().await;
}

#[tokio::test(start_paused = true)]
async fn dropping_the_handle_stops_the_session() {
    let dev = FakeDevice::new(Script {
        phases: vec![standard_stream()],
        ..Script::default()
    });
    let (handle, mut rx) = Session::start(&dev, JAR, options()).await.unwrap();
    drop(handle);
    assert!(ended(&mut rx).await.is_none());
    dev.wait_closed().await;
}

#[tokio::test(start_paused = true)]
async fn cancelling_start_closes_the_server_shell() {
    let dev = FakeDevice::new(Script {
        refuse_forever: true,
        ..Script::default()
    });
    let r = tokio::time::timeout(
        Duration::from_millis(300),
        Session::start(&dev, JAR, options()),
    )
    .await;
    assert!(r.is_err());
    for _ in 0..100 {
        if dev.record.shell_closed.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("shell stream not closed");
}

/// The happy path on a real multi-threaded runtime with real time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_threaded_runtime() {
    let dev = FakeDevice::new(Script {
        refuse_opens: 2,
        log: vec![INFO],
        phases: vec![standard_stream()],
        ..Script::default()
    });
    let (handle, mut rx) = Session::start(&dev, JAR, options()).await.unwrap();
    let mut packets = 0;
    while packets < 4 {
        if let SessionEvent::Packet(_) = next(&mut rx).await {
            packets += 1;
        }
    }
    let stopper = tokio::spawn(async move { handle.stop().await });
    assert!(ended(&mut rx).await.is_none());
    stopper.await.unwrap();
    dev.wait_closed().await;
}

#[tokio::test]
async fn lists_encoders_with_a_separate_jar() {
    let dev = FakeDevice::new(Script {
        shell_output: "[server] INFO: List of video encoders:\n    --video-codec=h265 --video-encoder=c2.qti.hevc.encoder (hw) [vendor]\n",
        ..Script::default()
    });
    let list = list_encoders(&dev, JAR).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(
        (list[0].video_codec(), list[0].name.as_str(), list[0].hw),
        (Some(VideoCodec::H265), "c2.qti.hevc.encoder", true)
    );
    assert_eq!(
        *dev.record.pushes.lock().unwrap(),
        [(REMOTE_LIST_SERVER_PATH.to_owned(), JAR.to_vec(), 0o644)]
    );
    assert_eq!(
        *dev.record.shells.lock().unwrap(),
        [list_encoders_command()]
    );

    let dev = FakeDevice::new(Script {
        shell_output: "[server] INFO: Device: x\n[server] ERROR: Could not list encoders\n\n",
        ..Script::default()
    });
    let Err(e) = list_encoders(&dev, JAR).await else {
        panic!()
    };
    assert_eq!(
        e.to_string(),
        "No encoders reported: [server] ERROR: Could not list encoders"
    );
}
