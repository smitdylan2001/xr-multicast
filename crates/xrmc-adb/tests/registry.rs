//! `AdbRegistry` tests with fake adbd instances behind real TCP sockets.
//! The adb server and USB enumeration are disabled so nothing on this machine is touched.

mod support;

use std::time::{Duration, Instant};

use support::{AuthMode, FakeConfig};
use xrmc_adb::{
    AdbDevice, AdbError, AdbHost, AdbKey, AdbRegistry, ConnectOptions, RegistryOptions,
    TransportSource,
};

fn registry(auth_timeout: Duration) -> AdbRegistry {
    let key = AdbKey::from_pem(include_str!("data/adbkey")).unwrap();
    let host = AdbHost::with_options(
        key,
        ConnectOptions {
            handshake_timeout: Duration::from_secs(3),
            key_name: "test@host".into(),
            ..ConnectOptions::default()
        },
    );
    AdbRegistry::new(
        host,
        RegistryOptions {
            use_server: false,
            direct_usb: false,
            direct_tcp: true,
            auth_timeout,
            retry_delay: Duration::from_millis(100),
        },
    )
}

async fn state_of(reg: &AdbRegistry, serial: &str) -> Option<String> {
    reg.transports()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.serial == serial)
        .map(|t| t.state)
}

async fn wait_state(reg: &AdbRegistry, serial: &str, want: Option<&str>) {
    let start = Instant::now();
    loop {
        let state = state_of(reg, serial).await;
        if state.as_deref() == want {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{serial}: state {state:?}, wanted {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn connect_list_use_and_disconnect() {
    let (addr, fakes) = support::serve_tcp(FakeConfig::default()).await;
    let reg = registry(Duration::from_secs(5));
    let serial = addr.to_string();
    assert!(reg.transports().await.unwrap().is_empty());

    reg.connect(&serial, Duration::from_secs(2)).await.unwrap();
    let list = reg.transports().await.unwrap();
    assert_eq!(list.len(), 1);
    let t = &list[0];
    assert_eq!(t.serial, serial);
    assert_eq!(t.state, "device");
    assert!(t.network);
    assert_eq!(t.source, TransportSource::Direct);
    assert_eq!(t.props.get("model").map(String::as_str), Some("Fake_Quest"));
    assert_eq!(t.props.get("product").map(String::as_str), Some("fake"));

    // Connecting again is a no-op.
    reg.connect(&serial, Duration::from_secs(2)).await.unwrap();
    assert_eq!(fakes.lock().unwrap().len(), 1);

    let device = reg.device(&serial).await.unwrap();
    assert_eq!(device.serial(), serial);
    assert_eq!(device.shell("echo hello").await.unwrap(), b"hello\n");

    reg.disconnect(&serial).await.unwrap();
    assert!(reg.transports().await.unwrap().is_empty());
    assert!(
        device.shell("echo hello").await.is_err(),
        "connection closed by disconnect"
    );
}

#[tokio::test]
async fn unauthorized_until_the_user_accepts() {
    let config = FakeConfig {
        auth: AuthMode::Keys {
            authorized: vec![],
            accept_new_key: true,
        },
        accept_delay: Duration::from_millis(500),
        ..FakeConfig::default()
    };
    let (addr, _fakes) = support::serve_tcp(config).await;
    let reg = registry(Duration::from_secs(5));
    let serial = addr.to_string();
    let t = Instant::now();
    // Returns as soon as the prompt is showing, like `adb connect`.
    reg.connect(&serial, Duration::from_secs(2)).await.unwrap();
    assert!(t.elapsed() < Duration::from_millis(400));
    assert_eq!(
        state_of(&reg, &serial).await.as_deref(),
        Some("unauthorized")
    );
    assert!(matches!(
        reg.device(&serial).await,
        Err(AdbError::Unauthorized)
    ));
    wait_state(&reg, &serial, Some("device")).await;
    let device = reg.device(&serial).await.unwrap();
    assert_eq!(device.shell("echo hello").await.unwrap(), b"hello\n");
}

#[tokio::test]
async fn never_accepted_stays_unauthorized_then_can_retry() {
    let config = FakeConfig {
        auth: AuthMode::Keys {
            authorized: vec![],
            accept_new_key: false,
        },
        ..FakeConfig::default()
    };
    let (addr, fakes) = support::serve_tcp(config).await;
    let reg = registry(Duration::from_millis(200));
    let serial = addr.to_string();
    reg.connect(&serial, Duration::from_secs(2)).await.unwrap();
    assert_eq!(
        state_of(&reg, &serial).await.as_deref(),
        Some("unauthorized")
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let entry = reg
        .transports()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.serial == serial)
        .unwrap();
    assert_eq!(entry.state, "unauthorized");
    assert!(entry.error.unwrap().contains("not authorised"));
    // A new connect starts a new attempt (and a new prompt).
    reg.connect(&serial, Duration::from_secs(2)).await.unwrap();
    assert_eq!(fakes.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_connect_leaves_no_transport() {
    let reg = registry(Duration::from_secs(1));
    // Bind and drop to get a port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let serial = format!("127.0.0.1:{port}");
    assert!(reg.connect(&serial, Duration::from_secs(1)).await.is_err());
    assert!(reg.transports().await.unwrap().is_empty());
    assert!(reg.device(&serial).await.is_err());
    assert!(
        reg.connect("not an address", Duration::from_secs(1))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn lost_connections_disappear() {
    let (addr, fakes) = support::serve_tcp(FakeConfig::default()).await;
    let reg = registry(Duration::from_secs(5));
    let serial = addr.to_string();
    reg.connect(&serial, Duration::from_secs(2)).await.unwrap();
    assert_eq!(state_of(&reg, &serial).await.as_deref(), Some("device"));
    // Wi-Fi drops / adbd restarts.
    fakes.lock().unwrap()[0].unplug();
    wait_state(&reg, &serial, None).await;
    // Reconnecting works.
    reg.connect(&serial, Duration::from_secs(2)).await.unwrap();
    assert_eq!(state_of(&reg, &serial).await.as_deref(), Some("device"));
}

#[tokio::test]
async fn default_port_is_5555() {
    let reg = registry(Duration::from_secs(1));
    // Nothing listens on 127.0.0.1:5555 in CI usually; either way the serial is normalised.
    let _ = reg.connect("127.0.0.2", Duration::from_millis(200)).await;
    for t in reg.transports().await.unwrap() {
        assert_eq!(t.serial, "127.0.0.2:5555");
    }
}
