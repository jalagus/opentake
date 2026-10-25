//! Discovering `.vst3` bundles on disk.

use std::path::{Path, PathBuf};

/// Standard VST3 install locations for the current platform.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        if let Some(home) = &home {
            paths.push(home.join("Library/Audio/Plug-Ins/VST3"));
        }
        paths.push("/Library/Audio/Plug-Ins/VST3".into());
    } else if cfg!(target_os = "windows") {
        for var in ["CommonProgramFiles", "CommonProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(var) {
                paths.push(PathBuf::from(dir).join("VST3"));
            }
        }
    } else {
        if let Some(home) = &home {
            paths.push(home.join(".vst3"));
        }
        paths.push("/usr/lib/vst3".into());
        paths.push("/usr/local/lib/vst3".into());
    }
    paths
}

/// Recursively find `.vst3` bundles (directories or single-file DLLs) under `roots`.
pub fn find_bundles(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        walk(root, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_vst3(&path) {
            found.push(path);
        } else if path.is_dir() {
            walk(&path, found);
        }
    }
}

fn is_vst3(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("vst3"))
}

/// Resolve the loadable binary inside a bundle.
pub fn bundle_binary(bundle: &Path) -> Option<PathBuf> {
    if bundle.is_file() {
        // Legacy Windows single-file plugin.
        return Some(bundle.to_path_buf());
    }
    let stem = bundle.file_stem()?.to_string_lossy().into_owned();
    let candidate = if cfg!(target_os = "macos") {
        bundle.join("Contents/MacOS").join(&stem)
    } else if cfg!(target_os = "windows") {
        let arch = if cfg!(target_arch = "aarch64") { "arm64-win" } else { "x86_64-win" };
        bundle.join("Contents").join(arch).join(format!("{stem}.vst3"))
    } else {
        let arch = if cfg!(target_arch = "aarch64") { "aarch64-linux" } else { "x86_64-linux" };
        bundle.join("Contents").join(arch).join(format!("{stem}.so"))
    };
    candidate.exists().then_some(candidate)
}
