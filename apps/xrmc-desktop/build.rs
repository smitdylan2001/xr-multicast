// Windows (MSVC) builds: embed the app icon and version info in xr-multicast.exe.
fn main() {
    println!("cargo:rerun-if-changed=../../packaging/windows/xr-multicast.ico");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.ends_with("windows-msvc") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../packaging/windows/xr-multicast.ico")
            .set("ProductName", "XR MultiCast")
            .set("FileDescription", "XR MultiCast")
            .set("CompanyName", "Dunk Technologies")
            .set("LegalCopyright", "MIT License, Dunk XR / Dunk Technologies");
        if let Err(e) = res.compile() {
            println!("cargo:warning=no Windows resources embedded: {e}");
        }
    }
}
