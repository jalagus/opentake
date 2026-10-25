//! Audio engine: device I/O, mixing, recording and plugin hosting.

pub mod audio;
pub mod engine;
pub mod latency;
pub mod media;
pub mod mixer;
pub mod recorder;
pub mod stretch;
pub mod delay;
pub mod drums;
pub mod dynamics;
pub mod multiband;
pub mod reverb;
pub mod gate;
pub mod deesser;
mod filter;
pub mod eq;
pub mod synth;

pub use audio::{AudioConfig, DeviceInfo, DeviceList};
pub use opentake_project as project;
pub use engine::{MidiFileImport, MidiFileSummary, inspect_midi_file, is_midi_file_path};
pub use engine::{GuitarProImport, GuitarProSummary, PROJECT_EXTENSION, inspect_guitar_pro, is_project_path};
pub use engine::{AudioFormat, ExportOptions, ExportReport, ExportedFile, LevelReport, MixJob, MixReport, SAMPLE_RATES};
pub use engine::{CLIP_GAIN_DB, CROSSFADE_SECS, FadeUpdate, JOIN_SECS, FlexJob, FlexRender, MAX_SEMITONES, PitchStretch, STRETCH_RANGE};
pub use engine::{Engine, EngineStatus, HistoryInfo, MidiClipUpdate, ParamView, RegionData, RegionKind, RegionRef, SendUpdate, SlotRef, TrackUpdate, Waveform};
pub use engine::{AudioUnitInfo, AutomationParam, LaneOwner};
