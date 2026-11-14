//! Works out the build's version and embeds the app icon and version info into the Windows
//! executable.
//!
//! The version grows with every commit: `<major>.<minor>` from Cargo.toml, then the number of
//! commits on the branch (`0.1.42`). `OPENTAKE_VERSION` is that number and
//! `OPENTAKE_VERSION_LONG` adds the commit (`0.1.42 (1a2b3c4)`), so a bug report pins the exact
//! build. Without git (a source tarball) it falls back to Cargo.toml's version.

use std::path::PathBuf;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-changed=../../assets/logo/opentake.ico");
    println!("cargo:rerun-if-changed=build.rs");

    let pkg = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let (version, long) = match (git(&["rev-list", "--count", "HEAD"]), git(&["rev-parse", "--short=7", "HEAD"])) {
        (Some(count), Some(sha)) => {
            let mut parts = pkg.split('.');
            let base = format!("{}.{}", parts.next().unwrap_or("0"), parts.next().unwrap_or("0"));
            let version = format!("{base}.{count}");
            let long = format!("{version} ({sha})");
            (version, long)
        }
        _ => (pkg.clone(), pkg),
    };
    println!("cargo:rustc-env=OPENTAKE_VERSION={version}");
    println!("cargo:rustc-env=OPENTAKE_VERSION_LONG={long}");

    // Recompute after a commit, checkout or fetch: HEAD, the branch it points at, packed refs.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]).map(PathBuf::from) {
        println!("cargo:rerun-if-changed={}", dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", dir.join("packed-refs").display());
        if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={}", dir.join(head).display());
        }
    }

    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/logo/opentake.ico");
        res.set("ProductName", "OpenTake");
        res.set("FileDescription", "OpenTake");
        res.set("ProductVersion", &long);
        res.set("FileVersion", &version);
        res.compile().expect("compiling Windows resources");
    }
}
