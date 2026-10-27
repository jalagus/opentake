//! Minimal VST3 hosting: scanning, loading and processing audio effect plugins.

pub mod editor;
pub mod host;
pub mod module;
pub mod plugin;
pub mod scan;
pub mod window;

use std::path::PathBuf;

pub use editor::{Edit, EditQueue, Vst3Editor};
pub use module::{ClassInfo, Module};
pub use plugin::{NoteEvent, ParamInfo, PluginState, Transport, Vst3Plugin, Vst3Processor};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not a loadable VST3 bundle: {0}")]
    BadBundle(PathBuf),
    #[error("failed to load library: {0}")]
    Load(#[from] libloading::Error),
    #[error("module entry function returned false")]
    EntryFailed,
    #[error("plugin has no factory")]
    NoFactory,
    #[error("could not instantiate {0}")]
    Instantiate(String),
    #[error("missing COM interface: {0}")]
    Com(&'static str),
    #[error("{0} failed with code {1}")]
    Call(&'static str, i32),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A plugin class discovered by scanning.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScannedPlugin {
    pub bundle: PathBuf,
    #[serde(flatten)]
    pub class: ClassInfo,
}

/// Scan bundles and list their audio classes. Each bundle is loaded and unloaded.
pub fn scan_plugins(roots: &[PathBuf]) -> Vec<(PathBuf, std::result::Result<Vec<ClassInfo>, String>)> {
    scan::find_bundles(roots)
        .into_iter()
        .map(|bundle| {
            let classes = Module::load(&bundle).map(|m| m.audio_classes()).map_err(|e| e.to_string());
            (bundle, classes)
        })
        .collect()
}
