//! Embeds the app icon and version info into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/logo/opentake.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/logo/opentake.ico");
        res.set("ProductName", "OpenTake");
        res.set("FileDescription", "OpenTake");
        res.compile().expect("compiling Windows resources");
    }
}
