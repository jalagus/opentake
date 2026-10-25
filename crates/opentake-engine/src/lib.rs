//! Audio engine: device I/O, mixing, recording and plugin hosting.

pub mod audio;
pub mod engine;
pub mod media;
pub mod mixer;
pub mod recorder;
pub mod delay;
pub mod drums;
pub mod dynamics;
pub mod eq;
pub mod synth;

pub use audio::{AudioConfig, DeviceInfo, DeviceList};
pub use opentake_project as project;
pub use engine::{GuitarProImport, GuitarProSummary, PROJECT_EXTENSION, inspect_guitar_pro, is_project_path};
pub use engine::{LevelReport, MixJob, MixReport};
pub use engine::{Engine, EngineStatus, HistoryInfo, MidiClipUpdate, ParamView, RegionData, RegionKind, RegionRef, SlotRef, TrackUpdate, Waveform};
