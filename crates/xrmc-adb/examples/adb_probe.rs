//! Manual test tool for the pure-Rust ADB client.
//!
//! ```sh
//! cargo run -p xrmc-adb --example adb_probe -- usb                       # list USB ADB devices
//! cargo run -p xrmc-adb --example adb_probe -- usb <serial> [command]     # connect directly over USB (or via the adb server if busy)
//! cargo run -p xrmc-adb --example adb_probe -- tcp 192.168.1.50 [command] # direct TCP (adb tcpip mode)
//! cargo run -p xrmc-adb --example adb_probe -- server [serial] [command]  # through a running adb server
//! ```
//!
//! The host key is kept in `$XRMC_ADB_KEY` (default `~/.xr-multicast/adbkey`);
//! the first connection shows "Allow USB debugging" on the headset.

use std::time::{Duration, Instant};

use xrmc_adb::{AdbDevice, AdbHost, AnyDevice};

fn key_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("XRMC_ADB_KEY") {
        return p.into();
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap_or_else(|| ".".into());
    std::path::Path::new(&home)
        .join(".xr-multicast")
        .join("adbkey")
}

async fn describe(device: &AnyDevice, command: Option<&str>) -> xrmc_adb::Result<()> {
    let t = Instant::now();
    let model = device.getprop("ro.product.model").await?;
    let manufacturer = device.getprop("ro.product.manufacturer").await?;
    let serialno = device.getprop("ro.serialno").await?;
    let release = device.getprop("ro.build.version.release").await?;
    println!(
        "{}: {manufacturer} {model}, serialno {serialno}, Android {release} ({:?})",
        device.serial(),
        t.elapsed()
    );
    if let Some(command) = command {
        let out = device.shell_output(command).await?;
        print!("{}", out.stdout_str());
        eprint!("{}", out.stderr_str());
        println!("[exit code: {:?}]", out.exit_code);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("usb");
    let target = args.get(1).map(String::as_str);
    let command = args.get(2).map(String::as_str);

    let path = key_path();
    let host = tokio::task::spawn_blocking(move || AdbHost::load_or_generate(&path)).await??;
    eprintln!(
        "host key: {} ({})",
        key_path().display(),
        &host.key().public_key_base64()[..24]
    );

    match mode {
        "usb" => {
            let devices = host.list_usb().await?;
            if devices.is_empty() {
                println!("no USB devices with an ADB interface found");
            }
            for d in &devices {
                println!(
                    "{:<20} {:04x}:{:04x} {} {} at {}",
                    d.adb_serial(),
                    d.vendor_id,
                    d.product_id,
                    d.manufacturer.as_deref().unwrap_or("?"),
                    d.product.as_deref().unwrap_or("?"),
                    d.location()
                );
            }
            // (On Android USB devices come from UsbManager, see `AdbHost::connect_usb_fd`.)
            #[cfg(not(target_os = "android"))]
            if let Some(serial) = target {
                let info = devices
                    .iter()
                    .find(|d| d.adb_serial() == serial)
                    .ok_or_else(|| format!("no USB device {serial}"))?;
                let device = host.connect_usb_or_server(info).await?;
                println!(
                    "connected ({})",
                    if matches!(device, AnyDevice::Direct(_)) {
                        "direct USB"
                    } else {
                        "via adb server"
                    }
                );
                describe(&device, command).await?;
            }
        }
        "tcp" => {
            let addr = target.ok_or("usage: adb_probe tcp <ip[:port]> [command]")?;
            let device = host.connect_tcp(addr, Duration::from_secs(5)).await?;
            println!("banner: {}", device.banner().raw);
            describe(&device.into(), command).await?;
        }
        "server" => {
            let devices = host.server_devices().await?;
            for d in &devices {
                println!("{:<24} {:<14} {:?}", d.serial, d.state, d.props);
            }
            if let Some(serial) = target {
                let device = host.connect_server(serial).await?;
                describe(&device.into(), command).await?;
            }
        }
        other => return Err(format!("unknown mode {other}; use usb, tcp or server").into()),
    }
    Ok(())
}
