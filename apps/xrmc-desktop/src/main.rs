//! XR MultiCast native desktop app (`xr-multicast`).

// Release builds on Windows are GUI programs (no console window); output still reaches the
// console the app was started from.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() {
    #[cfg(all(windows, not(debug_assertions)))]
    attach_parent_console();
    if let Err(e) = xrmc_desktop::cli::run() {
        eprintln!("xr-multicast: {e}");
        std::process::exit(1);
    }
}

#[cfg(all(windows, not(debug_assertions)))]
fn attach_parent_console() {
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    // Fails harmlessly when started from Explorer or a shortcut (no parent console).
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}
