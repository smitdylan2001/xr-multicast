//! Protocol tests against an in-process fake adbd (see `support/mod.rs`).

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use support::{AckPolicy, AuthMode, Fake, FakeConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use xrmc_adb::protocol::{A_VERSION, A_VERSION_MIN, MAX_PAYLOAD};
use xrmc_adb::{
    AdbConnection, AdbDevice, AdbError, AdbKey, AnyDevice, ConnectOptions, DirectDevice,
    TransportKind,
};

fn test_key() -> AdbKey {
    AdbKey::from_pem(include_str!("data/adbkey")).expect("test key")
}

fn options() -> ConnectOptions {
    ConnectOptions {
        handshake_timeout: Duration::from_secs(5),
        auth_timeout: Duration::from_secs(5),
        open_timeout: Duration::from_secs(5),
        key_name: "test@host".to_string(),
        ..ConnectOptions::default()
    }
}

async fn connect_with(
    config: FakeConfig,
    key: Option<&AdbKey>,
    opts: &ConnectOptions,
) -> (xrmc_adb::Result<AdbConnection>, Fake) {
    let (pipe, fake) = support::start(config);
    let conn = AdbConnection::connect_stream(pipe, key, opts, "fake").await;
    (conn, fake)
}

async fn connect(config: FakeConfig) -> (AdbConnection, Fake) {
    let key = test_key();
    let (conn, fake) = connect_with(config, Some(&key), &options()).await;
    (conn.expect("connect to fake adbd"), fake)
}

/// Poll a condition for up to 5 s.
async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let start = Instant::now();
    while !cond() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn echo_roundtrip(conn: &AdbConnection, data: &[u8]) {
    let mut s = conn.open("echo:").await.unwrap();
    let (mut rd, mut wr) = tokio::io::split(&mut s);
    let expected = data.to_vec();
    let writer = async {
        wr.write_all(data).await.unwrap();
        wr.flush().await.unwrap();
    };
    let reader = async {
        let mut got = vec![0u8; expected.len()];
        rd.read_exact(&mut got).await.unwrap();
        got
    };
    let ((), got) = tokio::join!(writer, reader);
    assert!(got == expected, "echoed data differs");
}

// ---------------------------------------------------------------- handshake

#[tokio::test]
async fn handshake_without_auth() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let info = conn.info();
    assert_eq!(info.version, A_VERSION);
    assert_eq!(info.max_payload, MAX_PAYLOAD as usize);
    assert_eq!(conn.banner().kind, "device");
    assert_eq!(conn.banner().model(), Some("Fake Quest"));
    assert!(conn.banner().has_feature("shell_v2"));
    assert_eq!(conn.serial(), "fake");
    let client_banner = fake.stats.client_banner.lock().unwrap().clone();
    assert!(
        client_banner.starts_with("host::features=shell_v2,"),
        "{client_banner}"
    );
    assert!(!client_banner.contains("delayed_ack"));
    assert_eq!(
        fake.stats.client_max_payload.load(Ordering::SeqCst),
        MAX_PAYLOAD as usize
    );
}

#[tokio::test]
async fn auth_with_authorized_key() {
    let key = test_key();
    let config = FakeConfig {
        auth: AuthMode::Keys {
            authorized: vec![key.private_key().to_public_key()],
            accept_new_key: false,
        },
        ..FakeConfig::default()
    };
    let (conn, fake) = connect_with(config, Some(&key), &options()).await;
    let conn = conn.unwrap();
    assert_eq!(fake.stats.signatures_checked.load(Ordering::SeqCst), 1);
    assert!(fake.stats.pubkey.lock().unwrap().is_none());
    echo_roundtrip(&conn, b"authorised").await;
}

#[tokio::test]
async fn auth_unknown_key_sends_public_key_and_user_accepts() {
    let key = test_key();
    let config = FakeConfig {
        auth: AuthMode::Keys {
            authorized: vec![],
            accept_new_key: true,
        },
        ..FakeConfig::default()
    };
    let (conn, fake) = connect_with(config, Some(&key), &options()).await;
    let conn = conn.unwrap();
    let pubkey = fake
        .stats
        .pubkey
        .lock()
        .unwrap()
        .clone()
        .expect("public key sent");
    assert_eq!(pubkey, key.auth_public_key_payload("test@host"));
    assert!(pubkey.ends_with(b" test@host\0"));
    echo_roundtrip(&conn, b"accepted").await;
}

#[tokio::test]
async fn auth_unknown_key_times_out_as_unauthorized() {
    let key = test_key();
    let config = FakeConfig {
        auth: AuthMode::Keys {
            authorized: vec![],
            accept_new_key: false,
        },
        ..FakeConfig::default()
    };
    let opts = ConnectOptions {
        auth_timeout: Duration::from_millis(300),
        ..options()
    };
    let start = Instant::now();
    let (conn, fake) = connect_with(config, Some(&key), &opts).await;
    assert!(matches!(conn, Err(AdbError::Unauthorized)), "{conn:?}");
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert!(
        fake.stats.pubkey.lock().unwrap().is_some(),
        "the prompt must have been triggered"
    );
}

#[tokio::test]
async fn auth_zero_timeout_triggers_prompt_and_fails_fast() {
    let key = test_key();
    let config = FakeConfig {
        auth: AuthMode::Keys {
            authorized: vec![],
            accept_new_key: false,
        },
        ..FakeConfig::default()
    };
    let opts = ConnectOptions {
        auth_timeout: Duration::ZERO,
        ..options()
    };
    let (conn, fake) = connect_with(config, Some(&key), &opts).await;
    assert!(matches!(conn, Err(AdbError::Unauthorized)));
    eventually("public key received", || {
        fake.stats.pubkey.lock().unwrap().is_some()
    })
    .await;
}

#[tokio::test]
async fn auth_without_key_or_without_pubkey_is_unauthorized() {
    let config = FakeConfig {
        auth: AuthMode::Keys {
            authorized: vec![],
            accept_new_key: true,
        },
        ..FakeConfig::default()
    };
    let (conn, _fake) = connect_with(config.clone(), None, &options()).await;
    assert!(matches!(conn, Err(AdbError::Unauthorized)));

    let key = test_key();
    let opts = ConnectOptions {
        send_public_key: false,
        ..options()
    };
    let (conn, fake) = connect_with(config, Some(&key), &opts).await;
    assert!(matches!(conn, Err(AdbError::Unauthorized)));
    assert!(fake.stats.pubkey.lock().unwrap().is_none());
}

#[tokio::test]
async fn stls_is_reported_as_unsupported() {
    let config = FakeConfig {
        auth: AuthMode::Tls,
        ..FakeConfig::default()
    };
    let key = test_key();
    let (conn, _fake) = connect_with(config, Some(&key), &options()).await;
    match conn {
        Err(AdbError::Protocol(msg)) => assert!(msg.contains("TLS"), "{msg}"),
        other => panic!("expected a TLS protocol error, got {other:?}"),
    }
}

#[tokio::test]
async fn silent_device_times_out() {
    let (pipe, _other_end) = tokio::io::duplex(1024);
    let opts = ConnectOptions {
        handshake_timeout: Duration::from_millis(200),
        ..options()
    };
    let res = AdbConnection::connect_stream(pipe, Some(&test_key()), &opts, "silent").await;
    assert!(matches!(res, Err(AdbError::Timeout)), "{res:?}");
}

#[tokio::test]
async fn device_hanging_up_during_handshake_is_disconnected() {
    let (pipe, other_end) = tokio::io::duplex(1024);
    drop(other_end);
    let res = AdbConnection::connect_stream(pipe, Some(&test_key()), &options(), "gone").await;
    assert!(res.is_err());
}

#[tokio::test]
async fn old_protocol_version_uses_checksums_and_small_payloads() {
    let config = FakeConfig {
        version: A_VERSION_MIN,
        max_payload: 4096,
        ..FakeConfig::default()
    };
    let (conn, fake) = connect(config).await;
    assert_eq!(conn.info().version, A_VERSION_MIN);
    assert_eq!(conn.max_payload(), 4096);
    let data = support::pattern_bytes(100_000);
    echo_roundtrip(&conn, &data).await;
    assert_eq!(fake.stats.checksum_errors.load(Ordering::SeqCst), 0);
    assert_eq!(fake.stats.oversize_writes.load(Ordering::SeqCst), 0);
    assert!(fake.stats.max_write_seen.load(Ordering::SeqCst) <= 4096);
    assert!(!conn.is_closed());
}

// ---------------------------------------------------------- multiplexing

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_concurrent_streams() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let mut tasks = Vec::new();
    for i in 0..24u32 {
        let conn = conn.clone();
        tasks.push(tokio::spawn(async move {
            let data: Vec<u8> = (0..150_000u32)
                .map(|j| (j.wrapping_mul(7) ^ i) as u8)
                .collect();
            echo_roundtrip(&conn, &data).await;
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(fake.stats.flow_violations.load(Ordering::SeqCst), 0);
    assert_eq!(fake.stats.opened.lock().unwrap().len(), 24);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_ids_are_unique_and_streams_independent() {
    let (conn, _fake) = connect(FakeConfig::default()).await;
    let mut sockets = Vec::new();
    for _ in 0..50 {
        sockets.push(conn.open("echo:").await.unwrap());
    }
    let mut ids: Vec<u32> = sockets.iter().map(|s| s.local_id()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 50);
    // Talk to them in reverse order.
    for (i, s) in sockets.iter_mut().enumerate().rev() {
        let msg = format!("stream {i}");
        s.write_all(msg.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; msg.len()];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, msg.as_bytes());
    }
}

// ----------------------------------------------------------- flow control

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_respects_flow_control_with_slow_acks() {
    let config = FakeConfig {
        ack: AckPolicy::Delayed(Duration::from_millis(2)),
        max_payload: 64 * 1024,
        ..FakeConfig::default()
    };
    let (conn, fake) = connect(config).await;
    let mut s = conn.open("sink:").await.unwrap();
    let data = vec![0x5au8; 2 * 1024 * 1024 + 123];
    s.write_all(&data).await.unwrap();
    s.flush().await.unwrap();
    eventually("all bytes sunk", || {
        fake.stats.sink_bytes.load(Ordering::SeqCst) == data.len()
    })
    .await;
    assert_eq!(fake.stats.flow_violations.load(Ordering::SeqCst), 0);
    assert_eq!(fake.stats.oversize_writes.load(Ordering::SeqCst), 0);
    assert_eq!(fake.stats.max_write_seen.load(Ordering::SeqCst), 64 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_stream_does_not_block_others() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let mut blocked = conn.open("blackhole:").await.unwrap();
    // The first write is accepted (one WRTE in flight) ...
    blocked.write_all(b"first").await.unwrap();
    // ... but the next one waits for an OKAY that never comes.
    let second =
        tokio::time::timeout(Duration::from_millis(200), blocked.write_all(b"second")).await;
    assert!(second.is_err(), "write should be blocked by flow control");
    let flush = tokio::time::timeout(Duration::from_millis(100), blocked.flush()).await;
    assert!(
        flush.is_err(),
        "flush waits for the device's acknowledgement"
    );

    // Other streams keep working meanwhile.
    for _ in 0..3 {
        echo_roundtrip(&conn, &support::pattern_bytes(300_000)).await;
    }
    assert_eq!(fake.stats.flow_violations.load(Ordering::SeqCst), 0);

    drop(blocked);
    eventually("blackhole closed", || {
        fake.stats.closed().contains(&"blackhole:".to_string())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_stream_defers_acks_without_blocking_others() {
    let max_payload = 64 * 1024;
    let config = FakeConfig {
        max_payload: max_payload as u32,
        ..FakeConfig::default()
    };
    let (conn, fake) = connect(config).await;
    let total = 8 * 1024 * 1024;
    let mut flood = conn.open(&format!("flood:{total}")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let sent = fake.stats.flood_sent.load(Ordering::SeqCst);
    // Receive window is 256 KiB here: the device must stall once it is full.
    assert!(sent > 0, "device should have sent something");
    assert!(
        sent <= 256 * 1024 + max_payload,
        "device sent {sent} bytes without the app reading"
    );

    // Another stream is unaffected.
    echo_roundtrip(&conn, b"still responsive").await;

    let mut got = Vec::with_capacity(total);
    flood.read_to_end(&mut got).await.unwrap();
    assert_eq!(got.len(), total);
    assert!(got == support::pattern_bytes(total), "flood data corrupted");
}

// ------------------------------------------------------------- closing

#[tokio::test]
async fn dropping_a_stream_sends_clse() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let s = conn.open("echo:").await.unwrap();
    drop(s);
    eventually("CLSE received", || {
        fake.stats.closed() == vec!["echo:".to_string()]
    })
    .await;
    echo_roundtrip(&conn, b"connection still fine").await;
}

#[tokio::test]
async fn shutdown_sends_clse_and_ends_reads() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let mut s = conn.open("echo:").await.unwrap();
    s.write_all(b"bye").await.unwrap();
    let mut buf = [0u8; 3];
    s.read_exact(&mut buf).await.unwrap();
    s.shutdown().await.unwrap();
    eventually("CLSE received", || {
        fake.stats.closed().contains(&"echo:".to_string())
    })
    .await;
    let mut rest = Vec::new();
    assert_eq!(s.read_to_end(&mut rest).await.unwrap(), 0);
    assert!(s.write_all(b"more").await.is_err());
    drop(s);
    // Only one CLSE for that stream.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fake.stats.closed().len(), 1);
}

#[tokio::test]
async fn device_close_gives_eof_then_broken_pipe() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let mut s = conn.open("shell:echo hello").await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    assert_eq!(out, b"hello\n");
    assert!(s.is_remote_closed());
    let err = s.write_all(b"x").await.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    drop(s);
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The device closed it; dropping must not send another CLSE.
    assert!(fake.stats.closed().is_empty());
}

#[tokio::test]
async fn refused_and_hanging_opens() {
    let opts = ConnectOptions {
        open_timeout: Duration::from_millis(200),
        ..options()
    };
    let key = test_key();
    let (conn, fake) = connect_with(FakeConfig::default(), Some(&key), &opts).await;
    let conn = conn.unwrap();
    match conn.open("no-such-service:").await {
        Err(AdbError::Refused(service)) => assert_eq!(service, "no-such-service:"),
        other => panic!("expected Refused, got {other:?}"),
    }
    assert_eq!(fake.stats.refused_open_reply.load(Ordering::SeqCst), 1);
    assert!(matches!(conn.open("hang:").await, Err(AdbError::Timeout)));
    // The connection is still usable.
    echo_roundtrip(&conn, b"ok").await;
}

#[tokio::test]
async fn device_initiated_open_is_refused() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let mut s = conn.open("open-reverse:").await.unwrap();
    eventually("reverse OPEN refused", || {
        fake.stats.reverse_refused.load(Ordering::SeqCst)
    })
    .await;
    s.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
}

// --------------------------------------------------------- disconnection

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unplug_propagates_errors_to_all_streams() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let mut a = conn.open("echo:").await.unwrap();
    let mut b = conn.open("blackhole:").await.unwrap();
    b.write_all(b"pending").await.unwrap();
    let reader = tokio::spawn(async move {
        let mut buf = [0u8; 16];
        a.read(&mut buf).await
    });
    let blocked_writer = tokio::spawn(async move {
        b.write_all(b"blocked").await?;
        b.write_all(b"blocked").await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    fake.unplug();

    let read_err = reader.await.unwrap().unwrap_err();
    assert_eq!(read_err.kind(), std::io::ErrorKind::ConnectionAborted);
    let write_err = blocked_writer.await.unwrap().unwrap_err();
    assert_eq!(write_err.kind(), std::io::ErrorKind::ConnectionAborted);
    tokio::time::timeout(Duration::from_secs(2), conn.closed())
        .await
        .expect("closed() resolves");
    assert!(conn.is_closed());
    assert!(conn.close_reason().unwrap().contains("disconnected"));
    assert!(matches!(
        conn.open("echo:").await,
        Err(AdbError::Disconnected)
    ));
}

#[tokio::test]
async fn device_disconnect_during_open_fails_the_open() {
    let (conn, _fake) = connect(FakeConfig::default()).await;
    assert!(matches!(
        conn.open("disconnect:").await,
        Err(AdbError::Disconnected)
    ));
    assert!(conn.is_closed());
}

#[tokio::test]
async fn corrupt_message_closes_the_connection() {
    let (conn, _fake) = connect(FakeConfig::default()).await;
    let mut s = conn.open("garbage:").await.unwrap();
    let mut buf = [0u8; 8];
    assert!(s.read(&mut buf).await.is_err());
    assert!(conn.is_closed());
    assert!(
        conn.close_reason().unwrap().contains("magic"),
        "{:?}",
        conn.close_reason()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_all_handles_closes_the_transport() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let s = conn.open("echo:").await.unwrap();
    drop(conn);
    // A live stream keeps the connection open.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!fake.stats.transport_eof.load(Ordering::SeqCst));
    drop(s);
    eventually("transport closed", || {
        fake.stats.transport_eof.load(Ordering::SeqCst)
    })
    .await;
}

#[tokio::test]
async fn explicit_close_fails_streams() {
    let (conn, _fake) = connect(FakeConfig::default()).await;
    let mut s = conn.open("echo:").await.unwrap();
    conn.close();
    let mut buf = [0u8; 4];
    assert_eq!(
        s.read(&mut buf).await.unwrap_err().kind(),
        std::io::ErrorKind::ConnectionAborted
    );
}

// -------------------------------------------------------------- services

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_device_services() {
    let (conn, fake) = connect(FakeConfig::default()).await;
    let device = DirectDevice::new(conn, TransportKind::Other);
    assert!(device.supports_shell_v2());
    assert_eq!(device.serial(), "fake");

    assert_eq!(device.shell("echo hello").await.unwrap(), b"hello\n");
    let out = device.shell_output("fail").await.unwrap();
    assert_eq!(out.stdout, b"partial\n");
    assert_eq!(out.stderr, b"boom\n");
    assert_eq!(out.exit_code, Some(3));
    // shell() returns stdout regardless of the exit code.
    assert_eq!(device.shell("fail").await.unwrap(), b"partial\n");
    assert_eq!(
        device.getprop("ro.product.model").await.unwrap(),
        "Fake Quest"
    );
    assert!(
        fake.stats
            .opened
            .lock()
            .unwrap()
            .contains(&"shell,v2,raw:echo hello".to_string())
    );

    let big = device.shell("big 3000000").await.unwrap();
    assert!(big == support::pattern_bytes(3_000_000));

    // push + stat
    let payload = support::pattern_bytes(300_001);
    device
        .push(&payload, "/data/local/tmp/file.bin", 0o755)
        .await
        .unwrap();
    let file = fake
        .files
        .lock()
        .unwrap()
        .get("/data/local/tmp/file.bin")
        .cloned()
        .unwrap();
    assert_eq!(file.mode, 0o100_755);
    assert!(file.data == payload);
    let st = device.stat("/data/local/tmp/file.bin").await.unwrap();
    assert!(st.exists());
    assert_eq!(st.size, 300_001);
    assert!(!device.stat("/nope").await.unwrap().exists());
    let err = device
        .push(b"x", "/system/forbidden", 0o644)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Read-only file system"), "{err}");
    // Empty file.
    device
        .push(b"", "/data/local/tmp/empty", 0o644)
        .await
        .unwrap();
    assert!(
        fake.files.lock().unwrap()["/data/local/tmp/empty"]
            .data
            .is_empty()
    );

    // shell_stream: read a few lines, closing it ends the command.
    let mut stream = device.shell_stream("stream").await.unwrap();
    let mut buf = vec![0u8; 64];
    let n = stream.read(&mut buf).await.unwrap();
    assert!(
        std::str::from_utf8(&buf[..n])
            .unwrap()
            .starts_with("tick 0")
    );
    drop(stream);
    eventually("command ended by close", || {
        fake.stats
            .commands_ended_by_close
            .lock()
            .unwrap()
            .contains(&"stream".to_string())
    })
    .await;

    // Raw service.
    let mut raw = AdbDevice::open(&device, "localabstract:scrcpy_1234")
        .await
        .unwrap();
    raw.write_all(b"\x00video").await.unwrap();
    let mut echoed = [0u8; 6];
    raw.read_exact(&mut echoed).await.unwrap();
    assert_eq!(&echoed, b"\x00video");
}

#[tokio::test]
async fn legacy_shell_without_shell_v2() {
    let config = FakeConfig {
        banner: "device::ro.product.model=Old;features=cmd".to_string(),
        ..FakeConfig::default()
    };
    let (conn, fake) = connect(config).await;
    let device = DirectDevice::new(conn, TransportKind::Other);
    assert!(!device.supports_shell_v2());
    assert_eq!(device.shell("echo hello").await.unwrap(), b"hello\n");
    let out = device.shell_output("fail").await.unwrap();
    assert_eq!(out.stdout, b"partial\nboom\n", "legacy shell merges stderr");
    assert_eq!(out.exit_code, None);
    assert!(
        fake.stats
            .opened
            .lock()
            .unwrap()
            .contains(&"shell:echo hello".to_string())
    );
}

#[tokio::test]
async fn any_device_delegates() {
    let (conn, _fake) = connect(FakeConfig::default()).await;
    let device = AnyDevice::from(DirectDevice::new(conn, TransportKind::Tcp));
    assert!(device.is_network());
    assert_eq!(device.serial(), "fake");
    assert_eq!(device.shell("echo hello").await.unwrap(), b"hello\n");
    assert_eq!(device.getprop("ro.serialno").await.unwrap(), "FAKE123");
    device
        .push(b"abc", "/data/local/tmp/a", 0o644)
        .await
        .unwrap();
    let mut s = device.open("echo:").await.unwrap();
    s.write_all(b"any").await.unwrap();
    let mut buf = [0u8; 3];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"any");
    let mut st = device.shell_stream("echo hello").await.unwrap();
    let mut out = String::new();
    st.read_to_string(&mut out).await.unwrap();
    assert_eq!(out, "hello\n");
}

/// `AdbDevice` is usable generically (as other crates do).
async fn generic_model<D: AdbDevice>(d: &D) -> String {
    String::from_utf8(d.shell("getprop ro.product.model").await.unwrap()).unwrap()
}

#[tokio::test]
async fn generic_trait_usage_and_spawn() {
    let (conn, _fake) = connect(FakeConfig::default()).await;
    let device = DirectDevice::new(conn, TransportKind::Usb);
    let handle = tokio::spawn({
        let device = device.clone();
        async move { generic_model(&device).await }
    });
    assert_eq!(handle.await.unwrap().trim(), "Fake Quest");
}
