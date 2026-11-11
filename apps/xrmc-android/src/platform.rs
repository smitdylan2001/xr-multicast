//! [`Platform`] for Android: USB devices opened by `UsbManager` (Java) go to the
//! hub's ADB registry as file descriptors; Wi-Fi multicast lock while scanning;
//! keep-screen-on; back button; soft keyboard.

use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use android_activity::AndroidApp;
use eframe::egui;
use xrmc_core::NoticeLevel;
use xrmc_desktop::platform::{HubHandle, Platform, PlatformNotice, TextKind};

use crate::ime::ImeBridge;
use crate::jni_bridge::{self, Java, Method};

/// How often Java's USB / keyboard events are collected.
const POLL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct State {
    hub: Option<HubHandle>,
    /// USB devices opened before the hub was running.
    queued: Vec<(OwnedFd, String, String)>,
    notices: Vec<PlatformNotice>,
    keyboard_px: f32,
    ctx: Option<egui::Context>,
}

struct Shared {
    java: Java,
    state: Mutex<State>,
}

pub struct AndroidPlatform {
    app: AndroidApp,
    snapshots: PathBuf,
    shared: Arc<Shared>,
    info: String,
    ime: Mutex<ImeBridge>,
}

impl std::fmt::Debug for AndroidPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AndroidPlatform")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl AndroidPlatform {
    /// Call outside winit's event loop (takes the VM/activity pointers from `app`).
    pub fn new(app: AndroidApp, java: Java) -> AndroidPlatform {
        let info = jni_bridge::call_string(java, Method::PlatformInfo)
            .unwrap_or_else(|e| format!("Android (platformInfo failed: {e})"));
        let snapshots = app
            .external_data_path()
            .or_else(|| app.internal_data_path())
            .unwrap_or_else(|| PathBuf::from("/sdcard/Download"))
            .join("Pictures");
        AndroidPlatform {
            app,
            snapshots,
            shared: Arc::new(Shared {
                java,
                state: Mutex::new(State::default()),
            }),
            info,
            ime: Mutex::new(ImeBridge::default()),
        }
    }

    pub fn info(&self) -> &str {
        &self.info
    }

    fn app(&self) -> &AndroidApp {
        &self.app
    }

    fn java(&self) -> Java {
        self.shared.java
    }
}

fn notice(level: NoticeLevel, message: impl Into<String>) -> PlatformNotice {
    PlatformNotice {
        level,
        message: message.into(),
    }
}

/// Give a USB device to the hub (Rust owns a duplicate of Java's descriptor).
fn attach(hub: &HubHandle, fd: OwnedFd, serial: &str, product: &str) {
    let _guard = hub.runtime.enter();
    let listed = hub.registry.add_usb_fd(fd, Some(serial.to_string()));
    log::info!("usb: {product} ({listed}) handed to the ADB client; waiting for the headset's approval");
}

impl Shared {
    fn handle(&self, line: &str) {
        let f: Vec<&str> = line.split('\t').collect();
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        match f.as_slice() {
            ["usb-attached", fd, serial, product] => {
                let Ok(raw) = fd.parse::<RawFd>() else { return };
                // SAFETY: Java keeps its UsbDeviceConnection (and this fd) open until detach;
                // we only duplicate it.
                let dup = unsafe { BorrowedFd::borrow_raw(raw) }.try_clone_to_owned();
                match dup {
                    Ok(fd) => {
                        st.notices.push(notice(
                            NoticeLevel::Info,
                            format!("{product} connected over USB. Put the headset on and accept \"Allow USB debugging\"."),
                        ));
                        match &st.hub {
                            Some(hub) => attach(hub, fd, serial, product),
                            None => st.queued.push((fd, serial.to_string(), product.to_string())),
                        }
                    }
                    Err(e) => {
                        log::error!("usb: cannot duplicate fd {raw}: {e}");
                        st.notices.push(notice(
                            NoticeLevel::Error,
                            format!("Could not open {product}: {e}"),
                        ));
                    }
                }
            }
            ["usb-detached", serial, product] => {
                log::info!("usb: {product} ({serial}) detached");
                if let Some(hub) = &st.hub {
                    hub.registry.remove(serial);
                }
                st.queued.retain(|(_, s, _)| s != serial);
            }
            ["usb-denied", product] => {
                st.notices.push(notice(
                    NoticeLevel::Warn,
                    format!("USB access to {product} was denied. Re-plug the headset and tap OK on the prompt to cast it over USB."),
                ));
            }
            ["usb-error", message] => {
                log::warn!("usb: {message}");
                st.notices.push(notice(NoticeLevel::Error, *message));
            }
            ["ime", px] => st.keyboard_px = px.parse().unwrap_or(0.0),
            [] | [""] => {}
            other => log::debug!("platform event ignored: {other:?}"),
        }
    }
}

fn poll_loop(shared: Weak<Shared>) {
    loop {
        std::thread::sleep(POLL);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        match jni_bridge::call_string(shared.java, Method::PollEvents) {
            Ok(events) if !events.is_empty() => {
                for line in events.lines() {
                    log::debug!("platform event: {line}");
                    shared.handle(line);
                }
                let ctx = shared.state.lock().ok().and_then(|s| s.ctx.clone());
                if let Some(ctx) = ctx {
                    ctx.request_repaint();
                }
            }
            Ok(_) => {}
            Err(e) => log::warn!("pollEvents(): {e}"),
        }
    }
}

impl Platform for AndroidPlatform {
    fn name(&self) -> String {
        "Android".into()
    }

    fn device_noun(&self) -> &'static str {
        "phone or tablet"
    }

    fn is_mobile(&self) -> bool {
        true
    }

    fn start(&self, ctx: &egui::Context) {
        let first = {
            let mut st = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
            let first = st.ctx.is_none();
            st.ctx = Some(ctx.clone());
            first
        };
        if first {
            let weak = Arc::downgrade(&self.shared);
            let _ = std::thread::Builder::new()
                .name("xrmc-platform".into())
                .spawn(move || poll_loop(weak));
        }
    }

    fn hub_started(&self, hub: HubHandle) {
        let mut st = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        for (fd, serial, product) in std::mem::take(&mut st.queued) {
            attach(&hub, fd, &serial, &product);
        }
        st.hub = Some(hub);
        drop(st);
        // Devices plugged in before the app started.
        if let Err(e) = jni_bridge::call_void(self.java(), Method::RescanUsb) {
            log::warn!("rescanUsb(): {e}");
        }
    }

    fn keep_screen_on(&self, on: bool) {
        // Not `AndroidApp::set_window_flags`: it locks android-activity's state, which
        // winit already holds while the UI runs (deadlock).
        let m = if on {
            Method::KeepScreenOn
        } else {
            Method::KeepScreenOff
        };
        if let Err(e) = jni_bridge::call_void(self.java(), m) {
            log::warn!("{m:?}: {e}");
        }
    }

    fn network_scan(&self, active: bool) {
        let m = if active {
            Method::MulticastOn
        } else {
            Method::MulticastOff
        };
        if let Err(e) = jni_bridge::call_void(self.java(), m) {
            log::warn!("{m:?}: {e}");
        }
    }

    fn text_input(
        &self,
        ctx: &egui::Context,
        resp: &egui::Response,
        value: &mut String,
        kind: TextKind,
    ) {
        if let Ok(mut ime) = self.ime.lock() {
            ime.sync(self.app(), ctx, resp, value, kind);
        }
    }

    fn keyboard_height(&self) -> f32 {
        let st = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        let ppp = st.ctx.as_ref().map_or(1.0, |c| c.pixels_per_point());
        st.keyboard_px / ppp.max(0.1)
    }

    fn back_at_root(&self) {
        if let Err(e) = jni_bridge::call_void(self.java(), Method::GoHome) {
            log::warn!("goHome(): {e}");
        }
    }

    fn poll_notices(&self) -> Vec<PlatformNotice> {
        let mut st = self.shared.state.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut st.notices)
    }

    fn snapshot_dir(&self) -> PathBuf {
        self.snapshots.clone()
    }

    fn usb_hint(&self) -> &'static str {
        "Android: plug the headset into this phone or tablet's USB-C port (USB OTG; a USB-C to USB-C data cable or a powered USB-C hub for several headsets). Tap OK on \"Allow XR MultiCast to access …\" here, then accept \"Allow USB debugging\" inside the headset."
    }

    fn about(&self) -> String {
        self.info.clone()
    }
}
