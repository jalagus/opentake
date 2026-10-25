//! Project model (with automation) and file formats (native JSON, Reaper `.rpp`, DAWproject, and imports
//! from Ableton Live, Final Cut Pro XML/Logic and Ardour).

pub mod ableton;
pub mod automation;
pub mod ardour;
pub mod dawproject;
pub mod fade;
pub mod fcpxml;
pub mod guitarpro;
pub mod import;
pub mod model;
pub mod rpp;
pub mod tempo;

pub use automation::{AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Curve};
pub use fade::{Fade, FadeCurve, effective_fades};
pub use model::*;
pub use tempo::{BarLine, Grid, MusicalPos, TempoChange, TempoMap, TempoSegment};
