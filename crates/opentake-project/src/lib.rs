//! Project model (with automation) and file formats (native JSON, Reaper `.rpp`, DAWproject, and imports
//! from Ableton Live, Final Cut Pro XML/Logic and Ardour).

pub mod ableton;
pub mod automation;
pub mod comp;
pub mod controller;
pub mod ardour;
pub mod dawproject;
pub mod fade;
pub mod fcpxml;
pub mod guitarpro;
pub mod import;
pub mod marker;
pub mod migrate;
pub mod model;
pub mod quantize;
pub mod region_loop;
pub mod rpp;
pub mod sends;
pub mod sidechain;
pub mod tempo;
pub mod vstpreset;

pub use automation::{AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Curve};
pub use comp::{COMP_JOIN_SECS, CompSegment, played_clips};
pub use controller::{BEND_CENTRE, CONTROLLER_SLOTS, Controller, ControllerEvent, ControllerLane};
pub use fade::{Fade, FadeCurve, effective_fades};
pub use marker::{MARKER_COLORS, Marker};
pub use model::*;
pub use quantize::{Quantize, QuantizeGrid};
pub use region_loop::{LoopPass, MAX_LOOP_PASSES, MIN_LOOP_LEN, RegionLoop};
pub use sends::{AuxSend, MAX_SEND_GAIN};
pub use sidechain::SideChain;
pub use vstpreset::VstPreset;
pub use tempo::{BarLine, Grid, MusicalPos, TempoChange, TempoMap, TempoSegment};
