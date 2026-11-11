//! XR MultiCast native desktop app (`xr-multicast`).

fn main() {
    if let Err(e) = xrmc_desktop::cli::run() {
        eprintln!("xr-multicast: {e}");
        std::process::exit(1);
    }
}
