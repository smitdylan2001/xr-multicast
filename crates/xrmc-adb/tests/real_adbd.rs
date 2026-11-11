//! Integration tests against a real adbd (an Android emulator). Ignored by default.
//!
//! ```sh
//! # direct TCP to the emulator's adbd (console port + 1):
//! XRMC_ADB_TEST_TCP=127.0.0.1:5561 \
//! # through the shared adb server:
//! XRMC_ADB_TEST_SERIAL=emulator-5560 \
//! # optional: a scrcpy-server jar (+ version) to test `localabstract:`:
//! XRMC_ADB_TEST_SCRCPY_SERVER=/path/to/scrcpy-server XRMC_ADB_TEST_SCRCPY_VERSION=5.0.1 \
//! cargo test -p xrmc-adb --test real_adbd -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Only point these at emulators: they push files to `/data/local/tmp` and
//! start short-lived processes. They never reboot, root, `tcpip` or
//! disconnect anything.

use std::time::{Duration, Instant};

use sha2::Digest as _;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use xrmc_adb::{AdbDevice, AdbError, AdbHost, AdbKey, AnyDevice, ConnectOptions};

fn host() -> AdbHost {
    let key = match std::env::var("XRMC_ADB_TEST_KEY") {
        Ok(path) => AdbKey::load_or_generate(path).expect("test key"),
        Err(_) => AdbKey::from_pem(include_str!("data/adbkey")).expect("test key"),
    };
    AdbHost::with_options(
        key,
        ConnectOptions {
            auth_timeout: Duration::from_secs(30),
            key_name: "xrmc-adb-test@localhost".into(),
            ..ConnectOptions::default()
        },
    )
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// Deterministic pseudo-random bytes (xorshift).
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn sha256_hex(data: &[u8]) -> String {
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn exercise(device: &AnyDevice, label: &str) {
    let t0 = Instant::now();
    let model = device.getprop("ro.product.model").await.unwrap();
    println!("[{label}] serial={} model={model:?}", device.serial());
    assert!(!model.is_empty());

    // Shell v2: stdout, stderr and exit code are separated.
    let out = device
        .shell_output("echo out; echo err >&2; exit 7")
        .await
        .unwrap();
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
    assert_eq!(out.exit_code, Some(7));
    assert_eq!(device.shell("echo hello").await.unwrap(), b"hello\n");
    // Commands that read stdin see EOF instead of hanging.
    assert_eq!(device.shell("cat; echo done").await.unwrap(), b"done\n");

    // Large output (several MiB through flow control).
    let seq = device.shell("seq 1 300000").await.unwrap();
    let expected: String = (1..=300_000).map(|i| format!("{i}\n")).collect();
    assert!(
        seq == expected.as_bytes(),
        "seq output differs ({} bytes)",
        seq.len()
    );

    // Push 1 MiB and verify on the device.
    let data = noise(1024 * 1024, 0x5eed);
    let path = format!(
        "/data/local/tmp/xrmc-adb-test-{}-{label}.bin",
        std::process::id()
    );
    let t = Instant::now();
    device.push(&data, &path, 0o640).await.unwrap();
    println!("[{label}] pushed 1 MiB in {:?}", t.elapsed());
    let sum = device.shell(&format!("sha256sum {path}")).await.unwrap();
    let sum = String::from_utf8_lossy(&sum);
    assert_eq!(
        sum.split_whitespace().next(),
        Some(sha256_hex(&data).as_str()),
        "sha256 mismatch: {sum}"
    );
    // adbd copies the owner permission bits to group and other: 0640 -> 0666.
    assert_eq!(
        device.shell(&format!("stat -c %a {path}")).await.unwrap(),
        b"666\n"
    );
    let st = device.stat(&path).await.unwrap();
    assert_eq!(st.size, 1024 * 1024);
    assert_eq!(st.mode & 0o777, 0o666);
    assert_eq!(st.mode & 0o170_000, 0o100_000, "regular file");
    // Pulling it back through a shell (binary-safe raw shell v2).
    let back = device.shell(&format!("cat {path}")).await.unwrap();
    assert!(back == data, "cat output differs");
    device.shell(&format!("rm -f {path}")).await.unwrap();
    assert!(!device.stat(&path).await.unwrap().exists());

    // Throughput (informational): 16 MiB down through a raw shell stream, 16 MiB pushed.
    let t = Instant::now();
    let mut zeros = device
        .shell_stream("head -c 16777216 /dev/zero")
        .await
        .unwrap();
    let mut total = 0usize;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = zeros.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        total += n;
    }
    assert_eq!(total, 16 * 1024 * 1024);
    let down = t.elapsed();
    let big = noise(16 * 1024 * 1024, 3);
    let big_path = format!(
        "/data/local/tmp/xrmc-adb-test-big-{}-{label}.bin",
        std::process::id()
    );
    let t = Instant::now();
    device.push(&big, &big_path, 0o644).await.unwrap();
    let up = t.elapsed();
    assert_eq!(device.stat(&big_path).await.unwrap().size, 16 * 1024 * 1024);
    device.shell(&format!("rm -f {big_path}")).await.unwrap();
    println!(
        "[{label}] 16 MiB down in {down:?} ({:.0} MiB/s), 16 MiB push in {up:?} ({:.0} MiB/s)",
        16.0 / down.as_secs_f64(),
        16.0 / up.as_secs_f64()
    );

    // Pushing somewhere read-only reports the device's error.
    let err = device
        .push(b"x", "/system/xrmc-adb-test", 0o644)
        .await
        .unwrap_err();
    println!("[{label}] expected push failure: {err}");

    // Concurrent commands.
    let mut tasks = Vec::new();
    for i in 0..16 {
        let device = device.clone();
        tasks.push(tokio::spawn(async move {
            let out = device
                .shell(&format!("echo {i}; sleep 0.2; echo {i}"))
                .await
                .unwrap();
            assert_eq!(out, format!("{i}\n{i}\n").into_bytes());
        }));
    }
    let t = Instant::now();
    for task in tasks {
        task.await.unwrap();
    }
    assert!(
        t.elapsed() < Duration::from_secs(3),
        "commands did not run concurrently"
    );

    // Raw `tcp:` service against a toybox nc echo server; the shell stream reports its PID.
    let port =
        27_000 + (std::process::id() % 1000) as u16 + if label == "server" { 1000 } else { 0 };
    let server = device
        .shell_stream(&format!(
            "echo $$; exec toybox nc -L -s 127.0.0.1 -p {port} cat"
        ))
        .await
        .unwrap();
    let mut server = BufReader::new(server);
    let mut pid = String::new();
    server.read_line(&mut pid).await.unwrap();
    let pid = pid.trim().to_string();
    let mut echo = None;
    for _ in 0..50 {
        match device.open(&format!("tcp:{port}")).await {
            Ok(s) => {
                echo = Some(s);
                break;
            }
            Err(AdbError::Refused(_)) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("open tcp:{port}: {e}"),
        }
    }
    let mut echo = echo.expect("nc echo server came up");
    let payload = noise(200_000, 7);
    let (mut rd, mut wr) = tokio::io::split(&mut echo);
    let writer = async {
        wr.write_all(&payload).await.unwrap();
        wr.flush().await.unwrap();
    };
    let reader = async {
        let mut got = vec![0u8; payload.len()];
        rd.read_exact(&mut got).await.unwrap();
        got
    };
    let ((), got) = tokio::join!(writer, reader);
    assert!(got == payload, "tcp echo differs");
    drop(echo);
    drop(server);
    device
        .shell(&format!("kill {pid} 2>/dev/null; true"))
        .await
        .unwrap();

    if let Some(jar) = env("XRMC_ADB_TEST_SCRCPY_SERVER") {
        scrcpy_localabstract(device, &jar, label).await;
    }
    println!("[{label}] all checks passed in {:?}", t0.elapsed());
}

/// Start scrcpy-server and connect to its `localabstract:` socket (what xrmc-scrcpy does).
async fn scrcpy_localabstract(device: &AnyDevice, jar: &str, label: &str) {
    let version = env("XRMC_ADB_TEST_SCRCPY_VERSION").expect("XRMC_ADB_TEST_SCRCPY_VERSION");
    let data = std::fs::read(jar).expect("read scrcpy-server");
    let remote = format!(
        "/data/local/tmp/xrmc-adb-test-scrcpy-{}.jar",
        std::process::id()
    );
    device.push(&data, &remote, 0o644).await.unwrap();
    let salt = label
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let scid = format!(
        "{:08x}",
        (std::process::id().wrapping_mul(7919) ^ salt) & 0x7fff_ffff
    );
    let cmd = format!(
        "CLASSPATH={remote} app_process / com.genymobile.scrcpy.Server {version} scid={scid} log_level=info \
         video=true audio=false control=false tunnel_forward=true cleanup=false max_size=640 \
         send_device_meta=true send_frame_meta=true send_dummy_byte=true send_stream_meta=true"
    );
    let mut server_log = device.shell_stream(&cmd).await.unwrap();
    let service = format!("localabstract:scrcpy_{scid}");
    let mut video = None;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        match device.open(&service).await {
            Ok(mut s) => {
                // With a forward tunnel the server sends a dummy byte once it accepted us.
                let mut dummy = [0u8; 1];
                if s.read_exact(&mut dummy).await.is_ok() {
                    video = Some(s);
                    break;
                }
            }
            Err(AdbError::Refused(_)) => {}
            Err(e) => panic!("open {service}: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let Some(mut video) = video else {
        let mut log = vec![0u8; 4096];
        let n = tokio::time::timeout(Duration::from_secs(1), server_log.read(&mut log))
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(0);
        panic!(
            "scrcpy server did not accept: {}",
            String::from_utf8_lossy(&log[..n])
        );
    };
    let mut meta = [0u8; 64];
    video.read_exact(&mut meta).await.unwrap();
    let name_end = meta.iter().position(|&b| b == 0).unwrap_or(64);
    let name = String::from_utf8_lossy(&meta[..name_end]).into_owned();
    let mut codec = [0u8; 4];
    video.read_exact(&mut codec).await.unwrap();
    println!(
        "[{label}] scrcpy over {service}: device {name:?}, codec {:?}",
        String::from_utf8_lossy(&codec)
    );
    assert_eq!(&codec, b"h264");
    // Read the first session/config packet header to see real data flowing.
    let mut header = [0u8; 12];
    tokio::time::timeout(Duration::from_secs(10), video.read_exact(&mut header))
        .await
        .expect("video data")
        .unwrap();
    drop(video);
    drop(server_log);
    // `[s]cid` so the pattern does not match this cleanup shell's own command line.
    device
        .shell(&format!(
            "rm -f {remote}; pkill -f '[s]cid={scid}' 2>/dev/null; true"
        ))
        .await
        .unwrap();
    let leftover = device.stat(&remote).await.unwrap();
    assert!(!leftover.exists(), "{remote} not removed");
}

#[tokio::test]
#[ignore = "needs an emulator: set XRMC_ADB_TEST_TCP=127.0.0.1:<console port + 1>"]
async fn real_adbd_direct_tcp() {
    let Some(addr) = env("XRMC_ADB_TEST_TCP") else {
        eprintln!("XRMC_ADB_TEST_TCP not set; skipping");
        return;
    };
    let host = host();
    let t = Instant::now();
    let device = host
        .connect_tcp(&addr, Duration::from_secs(5))
        .await
        .unwrap();
    let conn = device.connection();
    println!(
        "[direct] connected to {addr} in {:?}: version 0x{:08x}, max_payload {}, banner {:?}",
        t.elapsed(),
        conn.info().version,
        conn.max_payload(),
        conn.banner().raw
    );
    assert!(device.supports_shell_v2());
    assert!(conn.banner().model().is_some());
    let any = AnyDevice::from(device.clone());
    exercise(&any, "direct").await;
    assert!(!device.is_closed());
}

#[tokio::test]
#[ignore = "needs an emulator: set XRMC_ADB_TEST_SERIAL=emulator-XXXX and XRMC_ADB_TEST_TCP=127.0.0.1:<port>"]
async fn real_registry() {
    let (Some(serial), Some(addr)) = (env("XRMC_ADB_TEST_SERIAL"), env("XRMC_ADB_TEST_TCP")) else {
        eprintln!("XRMC_ADB_TEST_SERIAL / XRMC_ADB_TEST_TCP not set; skipping");
        return;
    };
    assert!(
        serial.starts_with("emulator-"),
        "only run these tests against emulators"
    );
    // Direct USB stays off: never claim real headsets from a test.
    let registry = xrmc_adb::AdbRegistry::new(
        host(),
        xrmc_adb::RegistryOptions {
            use_server: true,
            direct_usb: false,
            direct_tcp: true,
            ..xrmc_adb::RegistryOptions::default()
        },
    );
    let before = registry.transports().await.unwrap();
    println!("[registry] transports: {before:?}");
    assert!(before.iter().any(|t| t.serial == serial
        && t.is_ready()
        && t.source == xrmc_adb::TransportSource::Server));

    registry
        .connect(&addr, Duration::from_secs(5))
        .await
        .unwrap();
    let after = registry.transports().await.unwrap();
    let direct = after
        .iter()
        .find(|t| t.serial == addr)
        .expect("direct transport listed");
    println!("[registry] direct entry: {direct:?}");
    assert!(direct.is_ready());
    assert_eq!(direct.source, xrmc_adb::TransportSource::Direct);
    assert!(direct.network);

    let via_direct = registry.device(&addr).await.unwrap();
    let via_server = registry.device(&serial).await.unwrap();
    assert!(matches!(via_direct, AnyDevice::Direct(_)));
    assert!(matches!(via_server, AnyDevice::Server(_)));
    let a = via_direct.getprop("ro.serialno").await.unwrap();
    let b = via_server.getprop("ro.serialno").await.unwrap();
    assert_eq!(a, b, "both transports reach the same device");

    // Only drops our own direct connection; the server is not asked.
    registry.disconnect(&addr).await.unwrap();
    assert!(
        !registry
            .transports()
            .await
            .unwrap()
            .iter()
            .any(|t| t.serial == addr)
    );
    assert!(
        registry
            .transports()
            .await
            .unwrap()
            .iter()
            .any(|t| t.serial == serial)
    );
}

#[tokio::test]
#[ignore = "needs an emulator on the adb server: set XRMC_ADB_TEST_SERIAL=emulator-XXXX"]
async fn real_adbd_via_server() {
    let Some(serial) = env("XRMC_ADB_TEST_SERIAL") else {
        eprintln!("XRMC_ADB_TEST_SERIAL not set; skipping");
        return;
    };
    assert!(
        serial.starts_with("emulator-"),
        "only run these tests against emulators"
    );
    let host = host();
    let version = host.server().version().await.unwrap();
    let devices = host.server_devices().await.unwrap();
    println!("[server] adb server version {version}, devices: {devices:?}");
    assert!(devices.iter().any(|d| d.serial == serial && d.is_ready()));
    let device = host.connect_server(&serial).await.unwrap();
    assert!(device.supports_shell_v2().await.unwrap());
    let any = AnyDevice::from(device);
    exercise(&any, "server").await;

    // A failing host:connect has no side effects on the server.
    let err = host
        .server()
        .connect_device("127.0.0.1:1")
        .await
        .unwrap_err();
    println!("[server] expected connect failure: {err}");
    assert!(matches!(
        host.connect_server("no-such-device-xrmc").await,
        Err(AdbError::Other(_))
    ));
}
