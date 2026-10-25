//! Audio engine: device I/O, mixing, recording and plugin hosting.

pub mod audio;
pub mod engine;
pub mod media;
pub mod mixer;
pub mod recorder;
pub mod drums;
pub mod eq;
pub mod synth;

pub use audio::{AudioConfig, DeviceInfo, DeviceList};
pub use opentake_project as project;
pub use engine::{GuitarProImport, GuitarProSummary, inspect_guitar_pro};
pub use engine::{Engine, EngineStatus, HistoryInfo, MidiClipUpdate, ParamView, RegionData, RegionKind, RegionRef, SlotRef, TrackUpdate, Waveform};
