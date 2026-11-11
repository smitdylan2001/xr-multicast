//! `xrmc`: exercise the XR MultiCast core from the command line.
//!
//! ```text
//! xrmc devices                         # like `adb devices -l` (adb server + direct USB/TCP)
//! xrmc connect 192.168.1.50            # direct TCP connection (port 5555 by default)
//! xrmc info <serial>                   # identify: brand, model, panel size, battery, IP
//! xrmc encoders <serial> --server-jar vendor/scrcpy-server
//! xrmc stream <serial> --seconds 5 --out cap.h264 --server-jar vendor/scrcpy-server
//! xrmc hub --seconds 20 --cast <hardware-serial> --server-jar vendor/scrcpy-server
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use tokio::io::AsyncWriteExt;
use xrmc_adb::{AdbDevice, AdbHost, AdbKey, AdbRegistry, RegistryOptions};
use xrmc_core::brands;
use xrmc_core::device_info;
use xrmc_core::events::CoreEvent;
use xrmc_core::stream_manager::{SessionConfig, StartOptions};
use xrmc_hub::{Hub, HubOptions, HubPaths};
use xrmc_scrcpy::{Session, SessionEvent};

#[derive(Parser)]
#[command(name = "xrmc", version, about = "XR MultiCast core command-line tool")]
struct Cli {
    /// ADB host key (created if missing). Default: the per-user XR MultiCast config dir.
    #[arg(long, global = true)]
    key: Option<PathBuf>,
    /// Settings/verified-headsets store (Electron-compatible JSON).
    #[arg(long, global = true)]
    store: Option<PathBuf>,
    /// scrcpy-server binary (v5.0.1).
    #[arg(long, global = true, env = "XRMC_SCRCPY_SERVER")]
    server_jar: Option<PathBuf>,
    /// Don't use a running adb server; direct USB/TCP only.
    #[arg(long, global = true)]
    no_server: bool,
    /// Don't open USB devices directly.
    #[arg(long, global = true)]
    no_usb: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List transports (adb server + direct connections).
    Devices,
    /// Open a direct TCP connection to `host[:port]`.
    Connect { address: String },
    /// Identify a device.
    Info { serial: String },
    /// List the device's video encoders (runs scrcpy-server in list mode).
    Encoders { serial: String },
    /// Stream video to a raw bitstream file.
    Stream {
        serial: String,
        #[arg(long, default_value_t = 5)]
        seconds: u64,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 1280)]
        max_size: u32,
        #[arg(long, default_value_t = 4_000_000)]
        bit_rate: u32,
        #[arg(long, default_value_t = 30)]
        max_fps: u32,
        #[arg(long, default_value = "h264")]
        codec: String,
    },
    /// Run the full hub (device + stream managers) and print events.
    Hub {
        #[arg(long, default_value_t = 15)]
        seconds: u64,
        /// Start casting this headset (hardware serial) once it is online.
        #[arg(long)]
        cast: Option<String>,
        /// Connect to `host:port` directly first.
        #[arg(long)]
        connect: Option<String>,
    },
}

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

fn paths(cli: &Cli) -> AnyResult<HubPaths> {
    let mut p =
        HubPaths::default_for_user().ok_or("no config directory (set XDG_CONFIG_HOME or HOME)")?;
    if let Some(k) = &cli.key {
        p.adb_key = k.clone();
    }
    if let Some(s) = &cli.store {
        p.store = s.clone();
    }
    p.scrcpy_server = cli.server_jar.clone();
    Ok(p)
}

fn registry_options(cli: &Cli) -> RegistryOptions {
    RegistryOptions {
        use_server: !cli.no_server,
        direct_usb: !cli.no_usb,
        ..RegistryOptions::default()
    }
}

fn registry(cli: &Cli) -> AnyResult<AdbRegistry> {
    let key = AdbKey::load_or_generate(&paths(cli)?.adb_key)?;
    Ok(AdbRegistry::new(AdbHost::new(key), registry_options(cli)))
}

/// A device by adb serial; `host:port` serials are connected directly on demand.
async fn device(reg: &AdbRegistry, serial: &str) -> AnyResult<xrmc_adb::AnyDevice> {
    if device_info::host_of(serial).is_some()
        && !reg
            .transports()
            .await?
            .iter()
            .any(|t| t.serial == serial && t.is_ready())
    {
        reg.connect(serial, Duration::from_secs(10)).await?;
    }
    Ok(reg.device(serial).await?)
}

fn jar(cli: &Cli) -> AnyResult<Vec<u8>> {
    let p = cli
        .server_jar
        .as_ref()
        .ok_or("--server-jar (or XRMC_SCRCPY_SERVER) is required")?;
    Ok(std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?)
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> AnyResult<()> {
    match &cli.cmd {
        Cmd::Devices => {
            let reg = registry(&cli)?;
            for t in reg.transports().await? {
                let props: Vec<String> = t.props.iter().map(|(k, v)| format!("{k}:{v}")).collect();
                println!(
                    "{:<28} {:<14} {:<7} {:?} {}",
                    t.serial,
                    t.state,
                    if t.network { "net" } else { "usb" },
                    t.source,
                    props.join(" ")
                );
            }
        }
        Cmd::Connect { address } => {
            let reg = registry(&cli)?;
            reg.connect(address, Duration::from_secs(10)).await?;
            println!("connected to {address}");
        }
        Cmd::Info { serial } => {
            let reg = registry(&cli)?;
            let dev = device(&reg, serial).await?;
            let out = String::from_utf8_lossy(&dev.shell(device_info::IDENTIFY_COMMAND).await?)
                .to_string();
            let id = device_info::parse_identify(&out);
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({ "identity": format!("{id:?}") })
                )?
            );
            let wm = String::from_utf8_lossy(&dev.shell(device_info::WM_SIZE_COMMAND).await?)
                .to_string();
            println!("panel: {:?}", device_info::parse_wm_size(&wm));
            let bat = String::from_utf8_lossy(&dev.shell(device_info::BATTERY_COMMAND).await?)
                .to_string();
            println!("battery: {:?}", device_info::parse_battery(&bat));
            let ip =
                String::from_utf8_lossy(&dev.shell(device_info::IP_COMMAND).await?).to_string();
            println!("ip: {:?}", device_info::parse_ip(&ip));
            let _ = brands::detect_brand; // brand detection happens in the device manager (see `xrmc hub`)
        }
        Cmd::Encoders { serial } => {
            let reg = registry(&cli)?;
            let dev = device(&reg, serial).await?;
            for e in xrmc_scrcpy::list_encoders(&dev, &jar(&cli)?).await? {
                println!(
                    "{:<5} {:<40} {}{}",
                    e.codec,
                    e.name,
                    if e.hw { "hw" } else { "sw" },
                    if e.alias { " (alias)" } else { "" }
                );
            }
        }
        Cmd::Stream {
            serial,
            seconds,
            out,
            max_size,
            bit_rate,
            max_fps,
            codec,
        } => {
            let reg = registry(&cli)?;
            let dev = device(&reg, serial).await?;
            let cfg = SessionConfig {
                codec: serde_json::from_value(serde_json::Value::String(codec.clone()))?,
                encoder: None,
                max_size: *max_size,
                bit_rate: *bit_rate,
                max_fps: *max_fps,
                crop: None,
                codec_options: vec![],
            };
            let started = Instant::now();
            let (handle, mut rx) =
                Session::start(&dev, &jar(&cli)?, xrmc_hub::server_options(&cfg)).await?;
            println!(
                "connected in {} ms (scid {})",
                started.elapsed().as_millis(),
                handle.scid()
            );
            let mut file = match out {
                Some(p) => Some(tokio::fs::File::create(p).await?),
                None => None,
            };
            let (mut packets, mut keys, mut bytes, mut first_frame) = (0u64, 0u64, 0u64, None);
            let deadline = tokio::time::sleep(Duration::from_secs(*seconds));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    ev = rx.recv() => match ev {
                        Some(SessionEvent::DeviceName(n)) => println!("device: {n}"),
                        Some(SessionEvent::Codec(c)) => println!("codec: {c:?}"),
                        Some(SessionEvent::VideoSize { width, height, .. }) => println!("video size: {width}x{height}"),
                        Some(SessionEvent::Packet(p)) => {
                            if !p.config {
                                packets += 1;
                                if p.key { keys += 1; }
                                if first_frame.is_none() { first_frame = Some(started.elapsed()); }
                            }
                            bytes += p.data.len() as u64;
                            if let Some(f) = file.as_mut() { f.write_all(&p.data).await?; }
                        }
                        Some(SessionEvent::Log(_)) => {}
                        Some(SessionEvent::Ended(err)) => { println!("ended: {err:?}"); break; }
                        None => break,
                    }
                }
            }
            handle.stop().await;
            if let Some(f) = file.as_mut() {
                f.flush().await?;
            }
            let secs = started.elapsed().as_secs_f64();
            println!(
                "frames: {packets} ({keys} key) · {:.1} fps · {:.2} Mbps · first frame after {} ms · dropped {}",
                packets as f64 / secs,
                bytes as f64 * 8.0 / secs / 1e6,
                first_frame.map(|d| d.as_millis()).unwrap_or(0),
                handle.stats().packets_dropped
            );
        }
        Cmd::Hub {
            seconds,
            cast,
            connect,
        } => {
            let mut opts = HubOptions::new(paths(&cli)?);
            opts.registry = registry_options(&cli);
            let hub = Arc::new(Hub::open(opts)?);
            let mut events = hub.subscribe();
            if let Some(addr) = connect {
                hub.devices.connect_address(addr).await?;
                println!("connected to {addr}");
            }
            let _tasks = hub.start();
            println!(
                "hub running for {seconds} s (scrcpy-server available: {})",
                hub.streams.server_available()
            );
            let deadline = tokio::time::sleep(Duration::from_secs(*seconds));
            tokio::pin!(deadline);
            let mut cast_started = false;
            let (mut packets, mut bytes) = (0u64, 0u64);
            loop {
                tokio::select! {
                    _ = &mut deadline => break,
                    ev = events.recv() => match ev {
                        Ok(CoreEvent::DevicesChanged(snap)) => {
                            for h in &snap.headsets {
                                println!("headset {:<24} {:<10} brand={:<10} model={:<16} trusted={} usb={:?} wifi={:?}", h.id, h.status, h.brand, h.model, h.trusted, h.usb, h.wifi);
                            }
                            if let Some(id) = cast
                                && !cast_started
                                && snap.headsets.iter().any(|h| &h.id == id && h.status == "online")
                            {
                                    cast_started = true;
                                    let hub = hub.clone();
                                    let id = id.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = hub.streams.start(&id, StartOptions::default()).await {
                                            eprintln!("cast failed: {e}");
                                        }
                                    });
                            }
                        }
                        Ok(CoreEvent::StreamState(s)) => println!("stream {} → {}", s.id, serde_json::to_string(&s)?),
                        Ok(CoreEvent::Packet { packet, .. }) => { packets += 1; bytes += packet.data.len() as u64; }
                        Ok(CoreEvent::Notice(n)) => println!("notice [{:?}] {}", n.level, n.message),
                        Ok(CoreEvent::StreamLog { .. }) | Ok(CoreEvent::AutoCast { .. }) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => println!("(missed {n} events)"),
                        Err(_) => break,
                    }
                }
            }
            hub.streams.stop_all().await;
            println!("received {packets} packets, {:.2} MB", bytes as f64 / 1e6);
        }
    }
    Ok(())
}
