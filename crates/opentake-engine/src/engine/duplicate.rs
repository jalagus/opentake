//! Duplicating a track, with a choice of what the copy takes along.

use std::collections::HashMap;

use anyhow::Result;
use opentake_project::{AutomationTarget, PluginSlot};
use serde::{Deserialize, Serialize};

use super::Engine;
use crate::mixer::Command;

/// What a duplicated track copies from the original. The name, the kind of track (audio,
/// instrument or bus) and the instrument always come along; the copy is never armed or
/// soloed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateTrack {
    /// Audio and MIDI regions.
    pub regions: bool,
    /// The effect plug-ins (insert chain), each a new instance with the original's state.
    pub plugins: bool,
    /// Automation lanes (lanes of plug-ins or sends that aren't copied are left out).
    pub automation: bool,
    /// Sends to buses.
    pub sends: bool,
    /// Volume, pan, mute, output, input channel and input monitoring.
    pub mixer: bool,
}

impl Default for DuplicateTrack {
    fn default() -> Self {
        Self { regions: true, plugins: true, automation: true, sends: true, mixer: true }
    }
}

impl DuplicateTrack {
    /// Only the channel: plug-ins, sends and mixer settings, no regions or automation.
    pub const SETTINGS_ONLY: Self = Self { regions: false, plugins: true, automation: false, sends: true, mixer: true };
}

/// The duplicate's index and anything that didn't come along (a plug-in that failed to load).
#[derive(Debug, Clone, Serialize)]
pub struct Duplicated {
    pub track: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl Engine {
    /// Copy track `index` to a new track right below it. One undo step.
    pub fn duplicate_track(&mut self, index: usize, what: DuplicateTrack) -> Result<Duplicated> {
        let index = self.track_index(index)?;
        self.record("Duplicate Track", None, |e| e.duplicate_track_impl(index, what))
    }

    fn duplicate_track_impl(&mut self, index: usize, what: DuplicateTrack) -> Result<Duplicated> {
        let mut copy = self.project.tracks[index].clone();
        copy.id = self.next_id();
        copy.name = copy_name(&copy.name, self.project.tracks.iter().map(|t| t.name.as_str()));
        copy.armed = false;
        copy.solo = false;
        if !what.regions {
            copy.clips.clear();
            copy.midi_clips.clear();
        }
        if !what.plugins {
            copy.plugins.clear();
        }
        if !what.sends {
            copy.sends.clear();
        }
        if !what.automation {
            copy.automation.clear();
            copy.show_automation = false;
        }
        if !what.mixer {
            let fresh = opentake_project::Track::new("");
            (copy.volume, copy.pan, copy.mute, copy.output) = (fresh.volume, fresh.pan, fresh.mute, None);
            (copy.input_channel, copy.monitor_input) = (fresh.input_channel, fresh.monitor_input);
        }

        // Each slot gets its own instance (with the original's current state) and a new
        // project-wide id, which the copied lanes follow.
        let mut warnings = Vec::new();
        let mut next_id = self.project.tracks.iter().flat_map(|t| t.slots()).chain(&self.project.master_plugins).map(|s| s.id).max().unwrap_or(0) + 1;
        let mut ids = HashMap::new();
        let mut load = |e: &mut Self, slot: &mut PluginSlot| -> bool {
            match e.capture_state(slot) {
                Some(Ok(state)) => slot.state = state,
                Some(Err(err)) => warnings.push(format!("'{}': current state not copied: {err:#}", slot.name)),
                None => {}
            }
            match e.load_slot(slot) {
                Ok((instance, warning)) => {
                    warnings.extend(warning);
                    slot.instance = instance;
                    ids.insert(slot.id, next_id);
                    slot.id = next_id;
                    next_id += 1;
                    true
                }
                Err(err) => {
                    warnings.push(format!("'{}' not copied: {err:#}", slot.name));
                    false
                }
            }
        };
        if let Some(mut slot) = copy.instrument.take() {
            copy.instrument = load(self, &mut slot).then_some(slot);
        }
        let mut plugins = Vec::with_capacity(copy.plugins.len());
        for mut slot in std::mem::take(&mut copy.plugins) {
            if load(self, &mut slot) {
                plugins.push(slot);
            }
        }
        copy.plugins = plugins;
        // Lanes of slots that weren't copied are dropped (by `prune_automation` in `record`).
        for lane in &mut copy.automation {
            if let AutomationTarget::Param { slot, .. } = &mut lane.target {
                *slot = ids.get(slot).copied().unwrap_or(0);
            }
        }

        let at = index + 1;
        self.project.tracks.insert(at, copy);
        if self.audio.is_some() {
            let rt = self.build_track(at, self.sample_rate())?;
            self.send(Command::AddTrack(rt))?;
            self.send(Command::ReorderTracks(Box::new(self.project.tracks.iter().map(|t| t.id).collect())))?;
            self.sync_routing()?;
        }
        Ok(Duplicated { track: at, warnings })
    }
}

/// "Gtr copy", then "Gtr copy 2", ... (a copy of a copy counts on from it).
fn copy_name<'a>(name: &str, taken: impl Iterator<Item = &'a str> + Clone) -> String {
    let base = match name.rsplit_once(" copy") {
        Some((base, n)) if n.is_empty() || n.trim_start().parse::<u32>().is_ok() && n.starts_with(' ') => base,
        _ => name,
    };
    let first = format!("{base} copy");
    if !taken.clone().any(|t| t == first) {
        return first;
    }
    (2..).map(|n| format!("{base} copy {n}")).find(|c| !taken.clone().any(|t| t == c)).expect("a free name")
}

#[cfg(test)]
mod tests {
    use super::super::tests::tone_file;
    use super::*;
    use crate::dynamics::comp;
    use crate::engine::{SlotRef, TrackUpdate};

    #[test]
    fn names_count_up() {
        let names = ["Gtr", "Gtr copy", "Gtr copy 2"];
        assert_eq!(copy_name("Bass", names.into_iter()), "Bass copy");
        assert_eq!(copy_name("Gtr", names.into_iter()), "Gtr copy 3");
        assert_eq!(copy_name("Gtr copy", names.into_iter()), "Gtr copy 3");
        assert_eq!(copy_name("Gtr copy 2", names.into_iter()), "Gtr copy 3");
        assert_eq!(copy_name("My copy of it", names.into_iter()), "My copy of it copy");
    }

    #[test]
    fn duplicate_copies_everything_below_the_original() {
        let dir = std::env::temp_dir().join(format!("opentake-dup-{}", std::process::id()));
        let wav = tone_file(&dir, 2.0);
        let mut e = Engine::new();
        let a = e.add_track("A").unwrap();
        e.add_track("B").unwrap();
        e.add_clip(a, &wav, 0.0).unwrap();
        e.insert_plugin(a, "comp", None).unwrap();
        e.update_track(a, TrackUpdate { volume: Some(0.5), pan: Some(-0.3), armed: Some(true), ..Default::default() }).unwrap();
        let target = e.param_target(Some(a), SlotRef::Effect(0), comp::THRESHOLD).unwrap();
        e.add_lane(Some(a), target).unwrap();
        e.add_lane(Some(a), AutomationTarget::Volume).unwrap();

        let d = e.duplicate_track(a, DuplicateTrack::default()).unwrap();
        assert_eq!(d.track, 1);
        let names: Vec<_> = e.project().tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["A", "A copy", "B"]);
        let (o, c) = (&e.project().tracks[0], &e.project().tracks[1]);
        assert_ne!(o.id, c.id);
        assert_eq!(c.clips.len(), 1);
        assert_eq!((c.volume, c.pan, c.armed), (0.5, -0.3, false));
        assert_eq!(c.plugins.len(), 1);
        assert_ne!(c.plugins[0].instance, o.plugins[0].instance, "its own instance");
        assert_ne!(c.plugins[0].id, o.plugins[0].id, "its own slot id");
        assert_eq!(c.plugins[0].params, o.plugins[0].params);
        assert!(c.automation.contains(&o.automation[1]));
        assert!(c.automation.iter().any(|l| l.target == AutomationTarget::Param { slot: c.plugins[0].id, param: comp::THRESHOLD }), "the param lane follows the copied slot");

        e.undo().unwrap();
        assert_eq!(e.project().tracks.len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_leaves_out_what_is_unchecked() {
        let dir = std::env::temp_dir().join(format!("opentake-dup2-{}", std::process::id()));
        let wav = tone_file(&dir, 2.0);
        let mut e = Engine::new();
        let a = e.add_track("A").unwrap();
        e.add_clip(a, &wav, 0.0).unwrap();
        e.insert_plugin(a, "comp", None).unwrap();
        e.update_track(a, TrackUpdate { volume: Some(0.5), ..Default::default() }).unwrap();
        let target = e.param_target(Some(a), SlotRef::Effect(0), comp::THRESHOLD).unwrap();
        e.add_lane(Some(a), target).unwrap();
        e.add_lane(Some(a), AutomationTarget::Pan).unwrap();

        let none = DuplicateTrack { regions: false, plugins: false, automation: true, sends: false, mixer: false };
        let c = e.duplicate_track(a, none).unwrap().track;
        let c = &e.project().tracks[c];
        assert!(c.clips.is_empty() && c.plugins.is_empty());
        assert_eq!(c.volume, 1.0);
        let targets: Vec<_> = c.automation.iter().map(|l| l.target).collect();
        assert_eq!(targets, [AutomationTarget::Pan], "the plug-in's lane goes with the plug-in");

        let c = e.duplicate_track(a, DuplicateTrack::SETTINGS_ONLY).unwrap().track;
        let c = &e.project().tracks[c];
        assert!(c.clips.is_empty() && c.automation.is_empty());
        assert_eq!((c.plugins.len(), c.volume), (1, 0.5));
        let _ = std::fs::remove_dir_all(dir);
    }
}
