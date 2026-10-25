//! Automation lanes: what can be automated, undoable lane and point edits, the envelopes
//! sent to the audio thread, and recording control moves into lanes while playing
//! (Touch/Latch).
//!
//! Lanes live on their track (or the master) and name plug-in parameters by the slot's
//! persistent id, so they follow the plug-in when the chain changes and are saved with the
//! project. A lane whose plug-in is removed goes with it (`Project::prune_automation`, run
//! after every edit); undo brings both back.

use anyhow::{Result, anyhow, bail};
use opentake_project::automation::pos_to_gain;
use opentake_project::{AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Curve, PluginSlot};
use serde::Serialize;

use super::{Engine, SlotRef, track_params};
use crate::mixer::{Command, EnvTarget, EnvelopeRt, MASTER};

/// Whose lanes: a track's (by index) or, with `None`, the master's.
pub type LaneOwner = Option<usize>;

/// Something a lane can automate.
#[derive(Debug, Clone, Serialize)]
pub struct AutomationParam {
    pub target: AutomationTarget,
    pub name: String,
    /// "Track", "Master" or the plug-in's name.
    pub group: String,
    /// The plug-in slot, for parameters.
    #[serde(skip)]
    pub slot: Option<SlotRef>,
}

/// A control being written into a lane while playing.
pub(super) struct Writing {
    /// Track id or `MASTER`.
    owner: u64,
    target: AutomationTarget,
    /// Timeline position (seconds) written up to, and the value being written.
    last: f64,
    value: f32,
    /// The control is held (Latch keeps writing after it's released, until the stop), and
    /// when it last moved: controls that don't report a release (plug-in knobs) count as
    /// released once still for `TOUCH_TIMEOUT`.
    touching: bool,
    moved: std::time::Instant,
}

const TOUCH_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Points this close to the playhead are edited instead of adding another (seconds).
const NEAR_PLAYHEAD: f64 = 0.02;

impl Engine {
    fn owner_id(&self, owner: LaneOwner) -> u64 {
        owner.map_or(MASTER, |i| self.track_id(i))
    }

    fn owner_index(&self, id: u64) -> Option<LaneOwner> {
        if id == MASTER { Some(None) } else { self.project.track_by_id(id).map(Some) }
    }

    fn lanes_ref(&self, owner: LaneOwner) -> Result<&Vec<AutomationLane>> {
        match owner {
            None => Ok(&self.project.master_automation),
            Some(t) => Ok(&self.project.tracks[self.track_index(t)?].automation),
        }
    }

    fn lanes_mut(&mut self, owner: LaneOwner) -> Result<&mut Vec<AutomationLane>> {
        match owner {
            None => Ok(&mut self.project.master_automation),
            Some(t) => {
                let t = self.track_index(t)?;
                Ok(&mut self.project.tracks[t].automation)
            }
        }
    }

    fn owner_slots(&self, owner: LaneOwner) -> Vec<(SlotRef, &PluginSlot)> {
        match owner {
            None => self.project.master_plugins.iter().enumerate().map(|(i, s)| (SlotRef::Master(i), s)).collect(),
            Some(t) => {
                let Some(track) = self.project.tracks.get(t) else { return Vec::new() };
                let inst = track.instrument.iter().map(|s| (SlotRef::Instrument, s));
                inst.chain(track.plugins.iter().enumerate().map(|(i, s)| (SlotRef::Effect(i), s))).collect()
            }
        }
    }

    /// A track's (or the master's) automation lanes.
    pub fn automation(&self, owner: LaneOwner) -> Result<&[AutomationLane]> {
        self.lanes_ref(owner).map(|l| l.as_slice())
    }

    /// The target for a plug-in parameter.
    pub fn param_target(&self, owner: LaneOwner, which: SlotRef, param: u32) -> Result<AutomationTarget> {
        let slot = self.slot_ref(owner.unwrap_or(0), which)?;
        if slot.id == 0 {
            bail!("plug-in '{}' has no id yet", slot.name);
        }
        Ok(AutomationTarget::Param { slot: slot.id, param })
    }

    /// The slot a parameter target is on.
    pub fn target_slot(&self, owner: LaneOwner, target: AutomationTarget) -> Option<SlotRef> {
        let AutomationTarget::Param { slot, .. } = target else { return None };
        self.owner_slots(owner).into_iter().find(|(_, s)| s.id == slot).map(|(r, _)| r)
    }

    /// Everything that can be automated: volume, pan and mute (master: volume), then every
    /// writable parameter of each plug-in.
    pub fn automation_params(&self, owner: LaneOwner) -> Result<Vec<AutomationParam>> {
        self.lanes_ref(owner)?;
        let group = if owner.is_some() { "Track" } else { "Master" };
        let basics: &[(AutomationTarget, &str)] = match owner {
            Some(_) => &[(AutomationTarget::Volume, "Volume"), (AutomationTarget::Pan, "Pan"), (AutomationTarget::Mute, "Mute")],
            None => &[(AutomationTarget::Volume, "Volume")],
        };
        let mut list: Vec<AutomationParam> = basics.iter().map(|&(target, name)| AutomationParam { target, name: name.into(), group: group.into(), slot: None }).collect();
        for (which, slot) in self.owner_slots(owner) {
            let Ok(params) = self.slot_params_static(owner.unwrap_or(0), which) else { continue };
            let plugin = short_name(&slot.name);
            for p in params.into_iter().filter(|p| !p.read_only) {
                let target = AutomationTarget::Param { slot: slot.id, param: p.id };
                list.push(AutomationParam { target, name: p.title, group: plugin.clone(), slot: Some(which) });
            }
        }
        Ok(list)
    }

    /// A lane target's name, e.g. "Volume" or "OpenTake Compressor: Threshold".
    pub fn target_name(&self, owner: LaneOwner, target: AutomationTarget) -> String {
        let AutomationTarget::Param { param, .. } = target else {
            return format!("{target:?}");
        };
        let Some(which) = self.target_slot(owner, target) else { return format!("Parameter {param}") };
        let Ok(slot) = self.slot_ref(owner.unwrap_or(0), which) else { return format!("Parameter {param}") };
        let title = self.slot_params_static(owner.unwrap_or(0), which).ok().and_then(|ps| ps.into_iter().find(|p| p.id == param)).map_or(format!("Parameter {param}"), |p| p.title);
        format!("{}: {title}", short_name(&slot.name))
    }

    /// A value as the target shows it (dB, pan, the plug-in's own display).
    pub fn format_automation_value(&self, owner: LaneOwner, target: AutomationTarget, value: f32) -> String {
        if let (AutomationTarget::Param { param, .. }, Some(which)) = (target, self.target_slot(owner, target)) {
            if let Some(d) = self.slot_ref(owner.unwrap_or(0), which).ok().and_then(|s| self.param_display(s, param, value as f64)) {
                return d;
            }
        }
        target.format(value)
    }

    fn check_target(&self, owner: LaneOwner, target: AutomationTarget) -> Result<()> {
        match (owner, target) {
            (None, AutomationTarget::Pan | AutomationTarget::Mute) => bail!("the master bus automates its volume and plug-ins"),
            (_, AutomationTarget::Param { param, .. }) => {
                let which = self.target_slot(owner, target).ok_or_else(|| anyhow!("no such plug-in slot"))?;
                let params = self.slot_params_static(owner.unwrap_or(0), which)?;
                if !params.iter().any(|p| p.id == param && !p.read_only) {
                    bail!("the plug-in has no writable parameter with id {param}");
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The first thing without a lane yet (volume, pan, mute, then plug-in parameters).
    pub fn next_lane_target(&self, owner: LaneOwner) -> Result<AutomationTarget> {
        let lanes = self.lanes_ref(owner)?;
        self.automation_params(owner)?.into_iter().map(|p| p.target).find(|t| !lanes.iter().any(|l| l.target == *t)).ok_or_else(|| anyhow!("everything already has a lane"))
    }

    /// Run a lane edit as an undo step and send the result to the audio thread.
    fn edit_lanes<T>(&mut self, owner: LaneOwner, label: &str, merge: Option<String>, f: impl FnOnce(&mut Vec<AutomationLane>) -> Result<T>) -> Result<T> {
        self.lanes_ref(owner)?;
        self.record(label, merge, |e| {
            let r = f(e.lanes_mut(owner)?)?;
            e.sync_automation(owner)?;
            Ok(r)
        })
    }

    fn lane_mut(lanes: &mut [AutomationLane], lane: usize) -> Result<&mut AutomationLane> {
        let n = lanes.len();
        lanes.get_mut(lane).ok_or_else(|| anyhow!("no automation lane {lane} (there are {n})"))
    }

    /// Add a lane for `target` (or show the existing one) and show the track's automation.
    /// Returns the lane's index.
    pub fn add_lane(&mut self, owner: LaneOwner, target: AutomationTarget) -> Result<usize> {
        self.check_target(owner, target)?;
        let index = self.record("Add Automation Lane", None, |e| {
            let lanes = e.lanes_mut(owner)?;
            let i = match lanes.iter().position(|l| l.target == target) {
                Some(i) => i,
                None => {
                    lanes.push(AutomationLane::new(target));
                    lanes.len() - 1
                }
            };
            lanes[i].shown = true;
            e.sync_automation(owner)?;
            Ok(i)
        })?;
        if let Some(t) = owner {
            self.project.tracks[t].show_automation = true;
        }
        Ok(index)
    }

    pub fn remove_lane(&mut self, owner: LaneOwner, lane: usize) -> Result<AutomationLane> {
        self.edit_lanes(owner, "Remove Automation Lane", None, |lanes| {
            Self::lane_mut(lanes, lane)?;
            Ok(lanes.remove(lane))
        })
    }

    /// Point a lane at something else (its points stay).
    pub fn set_lane_target(&mut self, owner: LaneOwner, lane: usize, target: AutomationTarget) -> Result<()> {
        self.check_target(owner, target)?;
        if self.lanes_ref(owner)?.iter().enumerate().any(|(i, l)| i != lane && l.target == target) {
            bail!("there already is a lane for that");
        }
        self.edit_lanes(owner, "Automation Parameter", None, |lanes| {
            Self::lane_mut(lanes, lane)?.target = target;
            Ok(())
        })
    }

    pub fn set_lane_mode(&mut self, owner: LaneOwner, lane: usize, mode: AutomationMode) -> Result<()> {
        self.edit_lanes(owner, "Automation Mode", None, |lanes| {
            Self::lane_mut(lanes, lane)?.mode = mode;
            Ok(())
        })
    }

    pub fn set_lane_shown(&mut self, owner: LaneOwner, lane: usize, shown: bool) -> Result<()> {
        self.edit_lanes(owner, if shown { "Show Automation Lane" } else { "Hide Automation Lane" }, None, |lanes| {
            Self::lane_mut(lanes, lane)?.shown = shown;
            Ok(())
        })
    }

    /// Replace a lane's points, or with `replace` false add them to the existing ones.
    /// Returns the lane's point count.
    pub fn set_automation_points(&mut self, owner: LaneOwner, lane: usize, points: Vec<AutomationPoint>, replace: bool) -> Result<usize> {
        if points.iter().any(|p| !p.time_secs.is_finite() || !p.value.is_finite()) {
            bail!("point times and values must be numbers");
        }
        self.edit_lanes(owner, "Automation Points", None, |lanes| {
            let l = Self::lane_mut(lanes, lane)?;
            if replace {
                l.set_points(points);
            } else {
                for p in points {
                    l.insert(p);
                }
            }
            Ok(l.points.len())
        })
    }

    /// Add one point. Returns its index.
    pub fn insert_point(&mut self, owner: LaneOwner, lane: usize, point: AutomationPoint) -> Result<usize> {
        self.edit_lanes(owner, "Add Automation Point", None, |lanes| Ok(Self::lane_mut(lanes, lane)?.insert(point)))
    }

    /// Move a point (kept between its neighbours). `merge` makes a drag one undo step.
    pub fn move_point(&mut self, owner: LaneOwner, lane: usize, index: usize, time: f64, value: f32, merge: Option<String>) -> Result<()> {
        self.edit_lanes(owner, "Move Automation Point", merge, |lanes| {
            if !Self::lane_mut(lanes, lane)?.move_point(index, time, value) {
                bail!("lane {lane} has no point {index}");
            }
            Ok(())
        })
    }

    pub fn set_point_curve(&mut self, owner: LaneOwner, lane: usize, index: usize, curve: Curve) -> Result<()> {
        self.edit_lanes(owner, "Automation Curve", None, |lanes| {
            let p = Self::lane_mut(lanes, lane)?.points.get_mut(index).ok_or_else(|| anyhow!("lane {lane} has no point {index}"))?;
            p.curve = curve;
            Ok(())
        })
    }

    pub fn delete_points(&mut self, owner: LaneOwner, lane: usize, indices: &[usize]) -> Result<()> {
        self.edit_lanes(owner, "Delete Automation Point", None, |lanes| {
            let l = Self::lane_mut(lanes, lane)?;
            let mut i = 0;
            l.points.retain(|_| {
                i += 1;
                !indices.contains(&(i - 1))
            });
            Ok(())
        })
    }

    /// Remove a lane's points between `from` and `to` (seconds; all of them by default).
    pub fn clear_automation(&mut self, owner: LaneOwner, lane: usize, from: Option<f64>, to: Option<f64>) -> Result<()> {
        self.edit_lanes(owner, "Clear Automation", None, |lanes| {
            Self::lane_mut(lanes, lane)?.clear_range(from.unwrap_or(f64::NEG_INFINITY), to.unwrap_or(f64::INFINITY));
            Ok(())
        })
    }

    /// Show or hide automation under a track (`None` = every track). A view setting, not an
    /// edit; a track shown without lanes gets a volume lane.
    pub fn set_automation_shown(&mut self, track: Option<usize>, show: bool) -> Result<()> {
        let tracks: Vec<usize> = match track {
            Some(t) => vec![self.track_index(t)?],
            None => (0..self.project.tracks.len()).collect(),
        };
        for t in tracks {
            self.project.tracks[t].show_automation = show;
            if show && self.project.tracks[t].automation.is_empty() {
                self.add_lane(Some(t), AutomationTarget::Volume)?;
            }
        }
        Ok(())
    }

    /// Show or hide the master bus's automation lanes (a view setting). Shown without lanes,
    /// it gets a volume lane.
    pub fn set_master_automation_shown(&mut self, show: bool) -> Result<()> {
        self.project.show_master_automation = show;
        if show && self.project.master_automation.is_empty() {
            self.add_lane(None, AutomationTarget::Volume)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- audio thread

    /// The envelopes the audio thread plays for a track or the master: active lanes, minus
    /// those being written, with plug-in slots resolved to their loaded instances.
    pub(super) fn envelopes(&self, owner: LaneOwner) -> Vec<EnvelopeRt> {
        let id = self.owner_id(owner);
        let Ok(lanes) = self.lanes_ref(owner) else { return Vec::new() };
        let slots = self.owner_slots(owner);
        lanes
            .iter()
            .filter(|l| l.is_active() && !self.writing.iter().any(|w| w.owner == id && w.target == l.target))
            .filter_map(|l| {
                let target = match l.target {
                    AutomationTarget::Volume => EnvTarget::Volume,
                    AutomationTarget::Pan => EnvTarget::Pan,
                    AutomationTarget::Mute => EnvTarget::Mute,
                    AutomationTarget::Param { slot, param } => {
                        let (_, s) = slots.iter().find(|(_, s)| s.id == slot && s.instance != 0)?;
                        EnvTarget::Param { instance: s.instance, id: param }
                    }
                };
                Some(EnvelopeRt::new(target, l.points.clone()))
            })
            .collect()
    }

    /// Send a track's (or the master's) envelopes to the audio thread. Parameters that are no
    /// longer automated go back to their set values.
    pub(super) fn sync_automation(&mut self, owner: LaneOwner) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let id = self.owner_id(owner);
        let lanes = self.envelopes(owner);
        let live: Vec<EnvTarget> = lanes.iter().map(|l| l.target).collect();
        let before = self.live_automation.insert(id, live.clone()).unwrap_or_default();
        self.send(Command::SetAutomation { track: id, lanes: Box::new(lanes) })?;
        for t in before.into_iter().filter(|t| !live.contains(t)) {
            let EnvTarget::Param { instance, id: param } = t else { continue };
            let Some((which, _)) = self.owner_slots(owner).into_iter().find(|(_, s)| s.instance == instance) else { continue };
            let value = self.slot_params_static(owner.unwrap_or(0), which).ok().and_then(|ps| ps.into_iter().find(|p| p.id == param)).map(|p| p.value);
            if let Some(value) = value {
                self.send(Command::SetPluginParam { track: id, instance, id: param, value })?;
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- controls

    fn playhead_secs(&self) -> f64 {
        self.shared.position.load(std::sync::atomic::Ordering::Relaxed) as f64 / self.sample_rate().max(1) as f64
    }

    /// The automated value (normalized) of `target` at the playhead, if an active lane moves it.
    pub fn automated_value(&self, owner: LaneOwner, target: AutomationTarget) -> Option<f32> {
        opentake_project::automated(self.lanes_ref(owner).ok()?, target, self.playhead_secs())
    }

    /// A control with an active lane was moved (normalized `value`). While playing, a lane in
    /// Touch or Latch records the move; in Read the envelope wins. Stopped, the point at the
    /// playhead is set (or one added there). Returns false if no active lane moves `target`
    /// (the control then applies as usual).
    pub fn control_automation(&mut self, owner: LaneOwner, target: AutomationTarget, value: f32) -> Result<bool> {
        let value = value.clamp(0.0, 1.0);
        let Some(lane) = self.lanes_ref(owner)?.iter().position(|l| l.target == target && l.mode != AutomationMode::Off) else { return Ok(false) };
        let mode = self.lanes_ref(owner)?[lane].mode;
        let playing = self.shared.playing.load(std::sync::atomic::Ordering::Relaxed);
        let t = self.playhead_secs();
        let id = self.owner_id(owner);
        if playing && mode.writes() {
            if !self.writing.iter().any(|w| w.owner == id && w.target == target) {
                // Keep the curve up to here: a point with the value it had, then write from it.
                let before = self.lanes_ref(owner)?[lane].value_at(t).unwrap_or(value);
                self.writing.push(Writing { owner: id, target, last: t, value, touching: true, moved: std::time::Instant::now() });
                self.edit_lanes(owner, "Write Automation", Some(write_key(id, target)), |lanes| {
                    lanes[lane].insert(AutomationPoint::new(t, before));
                    Ok(())
                })?;
            }
            self.write_to(owner, lane, target, t, value)?;
            if let Some(w) = self.writing.iter_mut().find(|w| w.owner == id && w.target == target) {
                (w.value, w.touching, w.moved) = (value, true, std::time::Instant::now());
            }
            return Ok(true);
        }
        if !self.lanes_ref(owner)?[lane].is_active() {
            return Ok(false);
        }
        if playing {
            return Ok(true); // Read: the envelope plays, the move doesn't stick
        }
        self.edit_lanes(owner, "Automation Point", Some(format!("autoedit:{id}:{target:?}")), |lanes| {
            let l = &mut lanes[lane];
            match l.points.iter().position(|p| (p.time_secs - t).abs() <= NEAR_PLAYHEAD) {
                Some(i) => l.points[i].value = value,
                None => drop(l.insert(AutomationPoint::new(t, value))),
            }
            Ok(())
        })?;
        Ok(true)
    }

    /// Write `value` from where the lane was written up to, to `t`: points in between go, one
    /// at `t` is added, and the control takes the value (heard while it's written).
    fn write_to(&mut self, owner: LaneOwner, lane: usize, target: AutomationTarget, t: f64, value: f32) -> Result<()> {
        let id = self.owner_id(owner);
        let Some(w) = self.writing.iter_mut().find(|w| w.owner == id && w.target == target) else { return Ok(()) };
        // A cycle wrapped back: start a new pass here.
        let from = if t < w.last { t } else { w.last };
        w.last = t;
        self.record("Write Automation", Some(write_key(id, target)), |e| {
            let l = &mut e.lanes_mut(owner)?[lane];
            l.points.retain(|p| p.time_secs <= from || p.time_secs > t + 1e-9);
            l.insert(AutomationPoint::new(t, value));
            e.apply_control(owner, target, value)
        })
    }

    /// Set a control's static value (without an undo step of its own).
    fn apply_control(&mut self, owner: LaneOwner, target: AutomationTarget, value: f32) -> Result<()> {
        match (owner, target) {
            (Some(t), AutomationTarget::Volume | AutomationTarget::Pan | AutomationTarget::Mute) => {
                let track = &mut self.project.tracks[t];
                match target {
                    AutomationTarget::Volume => track.volume = pos_to_gain(value),
                    AutomationTarget::Pan => track.pan = value * 2.0 - 1.0,
                    _ => track.mute = value >= 0.5,
                }
                let (id, params) = (track.id, track_params(track));
                self.send(Command::SetTrack { track: id, params })
            }
            (None, AutomationTarget::Volume) => {
                self.project.master_volume = pos_to_gain(value);
                self.send(Command::SetMasterGain(self.project.master_volume))
            }
            (_, AutomationTarget::Param { param, .. }) => {
                let which = self.target_slot(owner, target).ok_or_else(|| anyhow!("no such plug-in slot"))?;
                self.apply_param(owner.unwrap_or(0), which, param, value as f64)
            }
            _ => Ok(()),
        }
    }

    /// The control was released: Touch stops writing (the lane plays again from here), Latch
    /// keeps writing its last value until the transport stops.
    pub fn release_automation(&mut self, owner: LaneOwner, target: AutomationTarget) -> Result<()> {
        let id = self.owner_id(owner);
        let latch = self.lanes_ref(owner)?.iter().any(|l| l.target == target && l.mode == AutomationMode::Latch);
        if latch {
            if let Some(w) = self.writing.iter_mut().find(|w| w.owner == id && w.target == target) {
                w.touching = false;
            }
            return Ok(());
        }
        self.finish_writing(|w| w.owner == id && w.target == target)
    }

    /// Keep writing held and latched controls up to the playhead; stop writing when the
    /// transport stops. Call regularly (the GUI does every frame).
    pub fn automation_tick(&mut self) -> Result<()> {
        if self.writing.is_empty() {
            return Ok(());
        }
        if !self.shared.playing.load(std::sync::atomic::Ordering::Relaxed) {
            return self.finish_writing(|_| true);
        }
        // Touch writes whose control went still end; latched ones stop being held.
        let mut still = Vec::new();
        for w in self.writing.iter_mut().filter(|w| w.touching && w.moved.elapsed() > TOUCH_TIMEOUT) {
            still.push((w.owner, w.target));
        }
        for (id, target) in still {
            let Some(owner) = self.owner_index(id) else { continue };
            self.release_automation(owner, target)?;
        }
        let t = self.playhead_secs();
        let due: Vec<(u64, AutomationTarget, f32)> = self.writing.iter().filter(|w| (t - w.last).abs() >= 0.05).map(|w| (w.owner, w.target, w.value)).collect();
        for (id, target, value) in due {
            let Some(owner) = self.owner_index(id) else { continue };
            let Some(lane) = self.lanes_ref(owner)?.iter().position(|l| l.target == target) else { continue };
            self.write_to(owner, lane, target, t, value)?;
        }
        Ok(())
    }

    fn finish_writing(&mut self, which: impl Fn(&Writing) -> bool) -> Result<()> {
        let mut owners = Vec::new();
        self.writing.retain(|w| {
            let done = which(w);
            if done && !owners.contains(&w.owner) {
                owners.push(w.owner);
            }
            !done
        });
        for id in owners {
            if let Some(owner) = self.owner_index(id) {
                self.sync_automation(owner)?;
            }
        }
        Ok(())
    }

    /// Whether `target` is being written (its control shows the written value).
    pub fn is_writing(&self, owner: LaneOwner, target: AutomationTarget) -> bool {
        let id = self.owner_id(owner);
        self.writing.iter().any(|w| w.owner == id && w.target == target)
    }
}

fn write_key(owner: u64, target: AutomationTarget) -> String {
    format!("write:{owner}:{target:?}")
}

/// "VST3: Name (Vendor)" -> "Name"
fn short_name(full: &str) -> String {
    super::display_name(full).unwrap_or_else(|| full.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dynamics::comp;
    use crate::mixer::AUTOMATION_BLOCK;
    use opentake_project::automation::gain_to_pos;

    fn pt(t: f64, v: f32) -> AutomationPoint {
        AutomationPoint::new(t, v)
    }

    #[test]
    fn lane_edits_undo_and_drags_merge() {
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        let lane = e.add_lane(Some(0), AutomationTarget::Volume).unwrap();
        assert!(e.project().tracks[0].show_automation, "adding a lane shows the track's automation");
        assert_eq!(e.add_lane(Some(0), AutomationTarget::Volume).unwrap(), lane, "one lane per target");
        e.insert_point(Some(0), lane, pt(1.0, 0.5)).unwrap();
        e.insert_point(Some(0), lane, pt(3.0, 0.8)).unwrap();
        for k in 1..=5 {
            e.move_point(Some(0), lane, 0, 1.0 + k as f64 * 0.1, 0.5 - k as f32 * 0.05, Some("drag".into())).unwrap();
        }
        assert_eq!(e.history_info().undo_steps[..3], ["Move Automation Point", "Add Automation Point", "Add Automation Point"]);
        let p = e.automation(Some(0)).unwrap()[0].points[0];
        assert!((p.time_secs - 1.5).abs() < 1e-9 && (p.value - 0.25).abs() < 1e-6);
        e.set_lane_mode(Some(0), lane, AutomationMode::Off).unwrap();
        e.delete_points(Some(0), lane, &[1]).unwrap();
        assert_eq!(e.automation(Some(0)).unwrap()[0].points.len(), 1);
        e.undo().unwrap();
        e.undo().unwrap();
        assert_eq!(e.automation(Some(0)).unwrap()[0].mode, AutomationMode::Read);
        e.undo().unwrap();
        assert_eq!(e.automation(Some(0)).unwrap()[0].points[0], pt(1.0, 0.5), "the whole drag is one step");
        assert!(e.move_point(Some(0), lane, 9, 0.0, 0.0, None).is_err());
        assert!(e.add_lane(None, AutomationTarget::Pan).is_err(), "the master has no pan");
        assert_eq!(e.next_lane_target(Some(0)).unwrap(), AutomationTarget::Pan);
    }

    #[test]
    fn plugin_lanes_follow_their_slot_and_go_with_it() {
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        e.insert_plugin(0, "comp", None).unwrap();
        e.insert_plugin(0, "comp", None).unwrap();
        let ids: Vec<u32> = e.project().tracks[0].plugins.iter().map(|s| s.id).collect();
        assert!(ids[0] != 0 && ids[1] != 0 && ids[0] != ids[1], "{ids:?}");
        let target = e.param_target(Some(0), SlotRef::Effect(1), comp::THRESHOLD).unwrap();
        let lane = e.add_lane(Some(0), target).unwrap();
        e.insert_point(Some(0), lane, pt(0.0, 0.25)).unwrap();
        assert!(e.automation_params(Some(0)).unwrap().iter().any(|p| p.target == target && p.group == comp::NAME));
        assert_eq!(e.target_name(Some(0), target), "OpenTake Compressor: Threshold");

        // Removing the first compressor moves the second to slot 0; the lane follows it.
        e.remove_plugin(0, 0).unwrap();
        assert_eq!(e.target_slot(Some(0), target), Some(SlotRef::Effect(0)));
        // Removing the automated one takes its lane along; undo brings both back.
        e.remove_plugin(0, 0).unwrap();
        assert!(e.automation(Some(0)).unwrap().is_empty());
        e.undo().unwrap();
        assert_eq!(e.automation(Some(0)).unwrap()[0].target, target);
        assert_eq!(e.target_slot(Some(0), target), Some(SlotRef::Effect(0)));
        // The automated value shows in the parameter list.
        let p = e.slot_params(0, SlotRef::Effect(0)).unwrap().into_iter().find(|p| p.id == comp::THRESHOLD).unwrap();
        assert_eq!(p.value, 0.25);
    }

    #[test]
    fn automation_saves_and_loads_with_slot_ids() {
        let dir = std::env::temp_dir().join(format!("opentake-auto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        e.insert_plugin(0, "eq", None).unwrap();
        e.insert_plugin(0, "comp", None).unwrap();
        let target = e.param_target(Some(0), SlotRef::Effect(1), comp::THRESHOLD).unwrap();
        let lane = e.add_lane(Some(0), target).unwrap();
        e.set_automation_points(Some(0), lane, vec![pt(0.0, 0.1), pt(2.0, 0.9)], true).unwrap();
        e.insert_master_plugin("limiter", None).unwrap();
        let master = e.add_lane(None, AutomationTarget::Volume).unwrap();
        e.insert_point(None, master, pt(1.0, 0.5)).unwrap();
        let path = dir.join("song.otp");
        e.save_project(Some(&path)).unwrap();

        let mut e2 = Engine::new();
        assert!(e2.load_project(&path).unwrap().is_empty());
        assert_eq!(e2.project().tracks[0].automation, e.project().tracks[0].automation);
        assert_eq!(e2.project().master_automation, e.project().master_automation);
        assert_eq!(e2.target_slot(Some(0), target), Some(SlotRef::Effect(1)), "the lane still names the compressor");
        // A project from before automation loads without lanes, and its slots get ids.
        let old = r#"{"name":"Old","tempo_bpm":120,"time_signature":[4,4],"sample_rate":48000,"tracks":[{"name":"T","volume":1,"pan":0,"mute":false,"solo":false,"armed":false,"input_channel":0,"monitor_input":false,"clips":[],"plugins":[{"name":"Compressor","format":"Builtin","path":"builtin:comp","uid":"builtin:comp","bypass":false,"state":[]}]}]}"#;
        std::fs::write(&path, old).unwrap();
        e2.load_project(&path).unwrap();
        assert!(e2.project().tracks[0].automation.is_empty() && e2.project().tracks[0].plugins[0].id != 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn audio_thread_gets_envelopes_after_edits_and_undo() {
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        e.insert_plugin(0, "comp", None).unwrap();
        let mut mixer = e.attach_test_mixer(48_000);
        let mut out = vec![0.0f32; 128];
        let mut live = |e: &Engine, mixer: &mut crate::mixer::Mixer| {
            mixer.process(&[], 0, &mut out, 2);
            mixer.debug_automation().into_iter().find(|(id, _)| *id == e.project().tracks[0].id).unwrap().1
        };
        let lane = e.add_lane(Some(0), AutomationTarget::Volume).unwrap();
        assert!(live(&e, &mut mixer).is_empty(), "a lane without points plays nothing");
        e.insert_point(Some(0), lane, pt(0.0, 0.5)).unwrap();
        assert_eq!(live(&e, &mut mixer), vec![EnvTarget::Volume]);
        let target = e.param_target(Some(0), SlotRef::Effect(0), comp::RATIO).unwrap();
        let lane2 = e.add_lane(Some(0), target).unwrap();
        e.insert_point(Some(0), lane2, pt(0.0, 0.5)).unwrap();
        let instance = e.project().tracks[0].plugins[0].instance;
        assert_eq!(live(&e, &mut mixer), vec![EnvTarget::Volume, EnvTarget::Param { instance, id: comp::RATIO }]);
        e.remove_plugin(0, 0).unwrap();
        assert_eq!(live(&e, &mut mixer), vec![EnvTarget::Volume]);
        e.undo().unwrap();
        assert_eq!(live(&e, &mut mixer).len(), 2, "undo restores the plug-in and its lane");
        e.undo().unwrap();
        e.undo().unwrap();
        assert_eq!(live(&e, &mut mixer), vec![EnvTarget::Volume]);
        e.set_lane_mode(Some(0), lane, AutomationMode::Off).unwrap();
        assert!(live(&e, &mut mixer).is_empty(), "off lanes don't play");
    }

    #[test]
    fn offline_render_follows_volume_automation() {
        let dir = std::env::temp_dir().join(format!("opentake-auto-render-{}", std::process::id()));
        let wav = super::super::tests::tone_file(&dir, 1.0);
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        e.add_clip(0, &wav, 0.0).unwrap();
        let lane = e.add_lane(Some(0), AutomationTarget::Volume).unwrap();
        // 0 dB, then silent from 0.5 s.
        let mut points = vec![pt(0.0, gain_to_pos(1.0)), pt(0.5, 0.0)];
        points[0].curve = Curve::Hold;
        e.set_automation_points(Some(0), lane, points, true).unwrap();
        let r = e.mix_job().render(None, None, 0.0);
        let peak = |a: usize, b: usize| r.master[0][a..b].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak(0, 23_900) > 0.4, "{}", peak(0, 23_900));
        assert_eq!(peak(24_000 + AUTOMATION_BLOCK, 48_000), 0.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn moving_an_automated_control_edits_or_writes_the_lane() {
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        assert!(!e.control_automation(Some(0), AutomationTarget::Volume, 0.5).unwrap(), "no lane: the control applies as usual");
        let lane = e.add_lane(Some(0), AutomationTarget::Volume).unwrap();
        e.insert_point(Some(0), lane, pt(0.0, 0.2)).unwrap();
        // Stopped: sets the point at the playhead, or adds one there.
        assert!(e.control_automation(Some(0), AutomationTarget::Volume, 0.6).unwrap());
        assert_eq!(e.automation(Some(0)).unwrap()[0].points, vec![pt(0.0, 0.6)]);
        e.seek(1.0).unwrap();
        e.control_automation(Some(0), AutomationTarget::Volume, 0.4).unwrap();
        assert_eq!(e.automation(Some(0)).unwrap()[0].points.len(), 2);
        assert_eq!(e.automated_value(Some(0), AutomationTarget::Volume), Some(0.4));

        // Touch while playing: the moves are recorded from where the control was grabbed.
        e.set_lane_mode(Some(0), lane, AutomationMode::Touch).unwrap();
        let mut mixer = e.attach_test_mixer(48_000);
        e.seek(2.0).unwrap();
        e.play().unwrap();
        let mut out = vec![0.0f32; 2 * 4800];
        mixer.process(&[], 0, &mut out, 2); // 0.1 s
        e.control_automation(Some(0), AutomationTarget::Volume, 0.9).unwrap();
        assert!(e.is_writing(Some(0), AutomationTarget::Volume));
        assert!((e.project().tracks[0].volume - pos_to_gain(0.9)).abs() < 1e-6, "the written value is heard");
        mixer.process(&[], 0, &mut out, 2);
        e.automation_tick().unwrap();
        e.release_automation(Some(0), AutomationTarget::Volume).unwrap();
        assert!(!e.is_writing(Some(0), AutomationTarget::Volume));
        let points = e.automation(Some(0)).unwrap()[0].points.clone();
        assert!(points.iter().any(|p| (p.time_secs - 2.1).abs() < 0.01 && p.value == 0.4), "the curve before the touch is kept: {points:?}");
        assert!(points.iter().any(|p| (p.time_secs - 2.2).abs() < 0.01 && p.value == 0.9), "{points:?}");
        assert_eq!(e.history_info().undo.as_deref(), Some("Write Automation"));
        e.undo().unwrap();
        assert_eq!(e.automation(Some(0)).unwrap()[0].points.len(), 2, "one undo step per pass");
    }
}
