//! XR MultiCast for Android phones and tablets: the phone/tablet is the hub.
//! Headsets plug in over USB (host / OTG) or are reached over Wi-Fi; this app
//! runs its own ADB client, starts scrcpy-server on each headset and decodes the
//! video itself with MediaCodec. No PC in the middle.
//!
//! * [`platform`]: [`xrmc_desktop::platform::Platform`] for Android (USB devices
//!   from `UsbManager`, multicast lock, keep-screen-on, back button, soft keyboard).
//! * [`jni_bridge`]: calls into the thin Java shell (`MainActivity.java`).
//! * [`ime`]: soft-keyboard bridge (winit 0.30 drops GameActivity text events).
//!
//! The UI itself is the shared egui app from `apps/xrmc-desktop` (library).

#![cfg(target_os = "android")]

mod ime;
mod jni_bridge;
mod platform;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use android_activity::AndroidApp;
use eframe::egui;
use xrmc_desktop::bridge::{HubConfig, Mode};
use xrmc_desktop::ui::{App, AppOptions};
use xrmc_hub::HubPaths;

use crate::platform::AndroidPlatform;

/// Options from the launching intent (`am start ... --es connect 10.0.2.2:5573 --ez noverify true`).
#[derive(Debug, Default)]
struct Launch {
    connect: Vec<String>,
    no_verify: bool,
    demo: Option<usize>,
    cast_on_start: bool,
    show: Option<String>,
    ui_scale: Option<f32>,
    /// MediaCodec decoder names to prefer (`--es decoder c2.android.avc.decoder`).
    decoder: Option<String>,
}

fn parse_launch(s: &str) -> Launch {
    let mut l = Launch::default();
    for line in s.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "connect" => l.connect.extend(
                v.split(',')
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .map(String::from),
            ),
            "noverify" => l.no_verify = v == "true" || v == "1",
            "demo" => l.demo = v.parse().ok().filter(|n| *n > 0),
            "castonstart" => l.cast_on_start = v == "true" || v == "1",
            "show" if !v.is_empty() => l.show = Some(v.to_string()),
            "uiscale" => l.ui_scale = v.parse().ok(),
            "decoder" if !v.is_empty() => l.decoder = Some(v.to_string()),
            _ => {}
        }
    }
    l
}

/// Copy the bundled scrcpy-server from the APK assets to app storage (when it changed).
fn extract_scrcpy_server(app: &AndroidApp, dir: &Path) -> Option<PathBuf> {
    let name = c"scrcpy-server";
    let mut asset = app.asset_manager().open(name)?;
    let bytes = asset.buffer().ok()?.to_vec();
    let path = dir.join("scrcpy-server");
    let same = std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64)
        && std::fs::read(&path).is_ok_and(|b| b == bytes);
    if !same && let Err(e) = std::fs::write(&path, &bytes) {
        log::error!("cannot write {}: {e}", path.display());
        return None;
    }
    log::info!("scrcpy-server {} ({} bytes)", path.display(), bytes.len());
    Some(path)
}

/// Test hook: devices added by IP (no USB) can cast without USB verification.
fn disable_usb_verification(store: &Path) {
    let shared = xrmc_core::SharedStore::new(xrmc_core::Store::open(store));
    let mut patch = serde_json::Map::new();
    patch.insert("requireUsbVerification".into(), false.into());
    shared.write(|s| s.update_settings(&patch));
    if let Err(e) = shared.flush() {
        log::warn!("store: {e}");
    }
    log::warn!("launch option noverify: requireUsbVerification = false");
}

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("xrmc")
            .with_filter(
                android_logger::FilterBuilder::new()
                    .parse("info,wgpu_core=warn,wgpu_hal=warn,naga=warn,eframe=warn,egui_wgpu=warn")
                    .build(),
            ),
    );
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
    }));

    let java = jni_bridge::Java::new(&app);
    let platform = Arc::new(AndroidPlatform::new(app.clone(), java));
    log::info!(
        "XR MultiCast {} on {}",
        env!("CARGO_PKG_VERSION"),
        platform.info()
    );

    // Decoders this device has (Java MediaCodecList), before anything asks for capabilities.
    match jni_bridge::call_string(java, jni_bridge::Method::CodecList) {
        Ok(list) => {
            let codecs: Vec<_> = list
                .lines()
                .filter_map(xrmc_decode::AndroidCodecInfo::parse_line)
                .collect();
            log::info!("MediaCodec decoders: {}", codecs.len());
            for c in &codecs {
                log::info!(
                    "  {} {} hw={} low-latency={} 10-bit={}",
                    c.mime,
                    c.name,
                    c.hardware,
                    c.low_latency,
                    c.ten_bit
                );
            }
            xrmc_decode::set_codec_list(codecs);
        }
        Err(e) => log::error!("codecList(): {e}"),
    }
    let launch = jni_bridge::call_string(java, jni_bridge::Method::LaunchOptions)
        .map(|s| parse_launch(&s))
        .unwrap_or_default();
    log::info!("launch options: {launch:?}");
    if let Some(d) = &launch.decoder {
        // SAFETY: no other threads read the environment yet (the hub and decoders start later).
        unsafe { std::env::set_var("XRMC_MEDIACODEC_DECODER", d) };
    }

    let data = app
        .internal_data_path()
        .unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    let _ = std::fs::create_dir_all(&data);
    let paths = HubPaths {
        adb_key: data.join("adbkey"),
        store: data.join("xr-multicast.json"),
        scrcpy_server: extract_scrcpy_server(&app, &data),
    };
    if launch.no_verify {
        disable_usb_verification(&paths.store);
    }
    let mode = match launch.demo {
        Some(n) => Mode::Demo {
            headsets: n.clamp(1, 32),
        },
        None => Mode::Hub(Box::new(HubConfig {
            paths,
            // No adb server on Android: our own ADB client over USB fds and TCP.
            use_adb_server: false,
            // USB devices come from UsbManager (the registry ignores this on Android).
            direct_usb: true,
            connect: launch.connect.clone(),
        })),
    };
    let opts = AppOptions {
        mode,
        fullscreen: true,
        kiosk: false,
        ui_scale: launch.ui_scale,
        screenshot: None,
        screenshot_after: None,
        exit_after: None,
        show: launch.show.clone(),
        decoder_threads: 1,
        offscreen: false,
        cast_on_start: launch.cast_on_start,
        platform: platform.clone(),
    };
    let mut native = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        android_app: Some(app),
        multisampling: 0,
        depth_buffer: 0,
        viewport: egui::ViewportBuilder::default().with_title("XR MultiCast"),
        ..Default::default()
    };
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(c) = &mut native.wgpu_options.wgpu_setup {
        // Vulkan, with OpenGL ES as the fallback on devices without a usable Vulkan driver.
        c.instance_descriptor.backends =
            eframe::wgpu::Backends::VULKAN | eframe::wgpu::Backends::GL;
    }
    xrmc_render::request_optional_features(&mut native.wgpu_options.wgpu_setup);
    let result = eframe::run_native(
        "XR MultiCast",
        native,
        Box::new(move |cc| Ok(Box::new(App::new(cc, opts)))),
    );
    if let Err(e) = result {
        log::error!("eframe exited with error: {e}");
    }
}
