//! Plug-ins' own editors (VST3 views, Audio Unit views) and the edits made in them.
//!
//! An editor is a native window, owned by the GUI on the main thread ([`PluginEditor`]
//! isn't `Send`). Its parameter edits are queued by the plug-in host (VST3's
//! `IComponentHandler`, an AU parameter listener) and applied here like the GUI's own
//! controls: to the processor, the project's saved values, undo (one step per gesture)
//! and automation (a gesture is a touch: Touch and Latch lanes write while playing).
//! Changes the other way (generic sliders, undo) reach the plug-in through
//! `Loaded::set_param`; [`Engine::sync_editors`] adds automation playback.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, anyhow, bail};
use opentake_project::AutomationTarget;
use opentake_vst3::Edit;

use super::plugins::{Loaded, SlotRef};
use super::{Engine, LaneOwner, display_name};

/// An open plug-in editor window. Keep it on the main thread; dropping it closes the
/// window cleanly (the view is detached first). It keeps its plug-in instance alive until
/// then, so close it when the slot goes (see [`Engine::slot_of_instance`]).
pub enum PluginEditor {
    Vst3(opentake_vst3::Vst3Editor),
    #[cfg(target_os = "macos")]
    Au(opentake_au::AuEditor),
}

impl PluginEditor {
    /// False once the user closed the window.
    pub fn is_open(&self) -> bool {
        match self {
            PluginEditor::Vst3(e) => e.is_open(),
            #[cfg(target_os = "macos")]
            PluginEditor::Au(e) => e.is_open(),
        }
    }

    /// Call every frame: follows window and view resizes (and shows AUv3 views when ready).
    pub fn idle(&mut self) {
        match self {
            PluginEditor::Vst3(e) => e.idle(),
            #[cfg(target_os = "macos")]
            PluginEditor::Au(e) => e.idle(),
        }
    }

    pub fn set_title(&mut self, title: &str) {
        match self {
            PluginEditor::Vst3(e) => e.set_title(title),
            #[cfg(target_os = "macos")]
            PluginEditor::Au(e) => e.set_title(title),
        }
    }

    pub fn bring_to_front(&self) {
        match self {
            PluginEditor::Vst3(e) => e.bring_to_front(),
            #[cfg(target_os = "macos")]
            PluginEditor::Au(e) => e.bring_to_front(),
        }
    }

    /// The native window's id (macOS window number, Windows HWND), for screenshots.
    pub fn window_id(&self) -> u64 {
        match self {
            PluginEditor::Vst3(e) => e.window_id(),
            #[cfg(target_os = "macos")]
            PluginEditor::Au(e) => e.window_id(),
        }
    }
}

/// Editor bookkeeping: gestures per (instance, parameter) and what automation showed.
#[derive(Default)]
pub(super) struct EditorState {
    /// The latest gesture's number, and whether it's still held.
    gestures: HashMap<(u64, u32), (u64, bool)>,
    next_gesture: u64,
    /// Parameters an open editor was shown automated values for.
    shown: HashSet<(u64, u32)>,
    /// Values when the editor opened. By the time an edit arrives the plug-in already holds
    /// the new value, so undo needs these for parameters the project hasn't saved yet.
    opened_with: HashMap<(u64, u32), f64>,
    /// A GUI shows editors (MCP requests them; see `request_editor`), and what it was asked.
    host: bool,
    requests: Vec<(usize, SlotRef)>,
}

fn gesture_key(instance: u64, id: u32, gesture: u64) -> String {
    format!("gesture:{instance}:{id}:{gesture}")
}

impl Loaded {
    fn take_edits(&self) -> Vec<Edit> {
        match self {
            Loaded::Vst3(p) => p.take_edits(),
            #[cfg(target_os = "macos")]
            Loaded::Au(p) => p.take_edits(),
            _ => Vec::new(),
        }
    }

    /// The value the plug-in itself holds (hosted plug-ins only).
    fn get_param(&self, id: u32) -> Option<f64> {
        match self {
            Loaded::Vst3(p) => p.get_param(id),
            #[cfg(target_os = "macos")]
            Loaded::Au(p) => p.get_param(id),
            _ => None,
        }
    }
}

impl Engine {
    /// Where a plug-in instance sits in the project: (track index, slot); the track is 0
    /// for master slots. `None` once it's no longer in the project.
    pub fn slot_of_instance(&self, instance: u64) -> Option<(usize, SlotRef)> {
        if instance == 0 {
            return None;
        }
        for (i, t) in self.project.tracks.iter().enumerate() {
            if t.instrument.as_ref().is_some_and(|s| s.instance == instance) {
                return Some((i, SlotRef::Instrument));
            }
            if let Some(s) = t.plugins.iter().position(|s| s.instance == instance) {
                return Some((i, SlotRef::Effect(s)));
            }
        }
        self.project.master_plugins.iter().position(|s| s.instance == instance).map(|s| (0, SlotRef::Master(s)))
    }

    /// Whether a slot's plug-in may have an editor of its own (built-ins have OpenTake's).
    pub fn has_editor(&self, track: usize, which: SlotRef) -> bool {
        let Ok(slot) = self.slot_ref(track, which) else { return false };
        match self.pool.get(&slot.instance) {
            Some(Loaded::Vst3(p)) => p.has_editor(),
            #[cfg(target_os = "macos")]
            Some(Loaded::Au(p)) => p.has_editor(),
            _ => false,
        }
    }

    /// Open a slot's plug-in editor in a native window titled `title`. Main thread only.
    /// Returns the instance it edits with the editor.
    pub fn open_editor(&mut self, track: usize, which: SlotRef, title: &str) -> Result<(u64, PluginEditor)> {
        let slot = self.slot_ref(track, which)?;
        let name = display_name(&slot.name).unwrap_or_else(|| slot.name.clone());
        let editor = match self.pool.get(&slot.instance) {
            Some(Loaded::Vst3(p)) => PluginEditor::Vst3(p.open_editor(title).map_err(|e| anyhow!("{name}: {e}"))?),
            #[cfg(target_os = "macos")]
            Some(Loaded::Au(p)) => PluginEditor::Au(p.open_editor(title).map_err(|e| anyhow!("{name}: {e}"))?),
            Some(_) => bail!("{name} has no editor window of its own"),
            None => bail!("plugin '{name}' is not loaded"),
        };
        let instance = slot.instance;
        self.remember_values(track, which);
        Ok((instance, editor))
    }

    /// A GUI (on the main thread, where editor windows live) will open requested editors.
    pub fn set_editor_host(&mut self, host: bool) {
        self.editor_state.host = host;
    }

    /// Ask the GUI to open a slot's editor (from any thread, e.g. an MCP request).
    pub fn request_editor(&mut self, track: usize, which: SlotRef) -> Result<()> {
        self.slot_ref(track, which)?;
        if !self.editor_state.host {
            bail!("plug-in editors open in the OpenTake window; this instance runs without one");
        }
        self.editor_state.requests.push((track, which));
        Ok(())
    }

    /// Editors requested since the last call, for the GUI to open.
    pub fn take_editor_requests(&mut self) -> Vec<(usize, SlotRef)> {
        std::mem::take(&mut self.editor_state.requests)
    }

    /// Note a slot's current values, as undo's "before" for what its editor changes.
    pub(super) fn remember_values(&mut self, track: usize, which: SlotRef) {
        let Ok(instance) = self.slot_ref(track, which).map(|s| s.instance) else { return };
        for p in self.slot_params_static(track, which).unwrap_or_default() {
            self.editor_state.opened_with.insert((instance, p.id), p.value);
        }
    }

    /// Apply what was edited in plug-in editors since the last call. Call regularly on the
    /// GUI thread (the GUI does every frame).
    pub fn apply_editor_edits(&mut self) -> Result<()> {
        let mut pending: Vec<(u64, Vec<Edit>)> = self.pool.iter().map(|(&id, p)| (id, p.take_edits())).filter(|(_, e)| !e.is_empty()).collect();
        pending.sort_by_key(|p| p.0);
        let mut result = Ok(());
        for (instance, edits) in pending {
            if let Err(e) = self.apply_edits(instance, edits) {
                result = result.and(Err(e));
            }
        }
        // A control held in an editor keeps a Touch write going while it's still.
        let held: Vec<(u64, u32)> = self.editor_state.gestures.iter().filter(|(_, g)| g.1).map(|(k, _)| *k).collect();
        for (instance, id) in held {
            if let Some((owner, target)) = self.param_owner(instance, id) {
                self.hold_control(owner, target);
            }
        }
        result
    }

    /// An editor closed: gestures it left held end (as if released).
    pub fn editor_closed(&mut self, instance: u64) -> Result<()> {
        let held: Vec<u32> = self.editor_state.gestures.iter().filter(|(k, g)| k.0 == instance && g.1).map(|(k, _)| k.1).collect();
        self.apply_edits(instance, held.into_iter().map(Edit::End).collect())
    }

    /// The lane owner and automation target of an instance's parameter.
    fn param_owner(&self, instance: u64, id: u32) -> Option<(LaneOwner, AutomationTarget)> {
        let (track, which) = self.slot_of_instance(instance)?;
        let owner = if matches!(which, SlotRef::Master(_)) { None } else { Some(track) };
        Some((owner, self.param_target(owner, which, id).ok()?))
    }

    /// Apply one plug-in's editor edits (in order).
    pub(super) fn apply_edits(&mut self, instance: u64, edits: Vec<Edit>) -> Result<()> {
        // A plug-in no longer in the project (removed, its insertion undone) edits nothing.
        let Some((track, which)) = self.slot_of_instance(instance) else { return Ok(()) };
        let mut edits = edits.into_iter().peekable();
        while let Some(edit) = edits.next() {
            match edit {
                Edit::Begin(id) => {
                    self.editor_state.next_gesture += 1;
                    let n = self.editor_state.next_gesture;
                    if let Some((old, true)) = self.editor_state.gestures.insert((instance, id), (n, true)) {
                        self.history.held.remove(&gesture_key(instance, id, old));
                    }
                    self.history.held.insert(gesture_key(instance, id, n));
                }
                Edit::Perform(id, value) => {
                    // A drag reports many values between frames: the last one counts.
                    if matches!(edits.peek(), Some(Edit::Perform(next, _)) if *next == id) {
                        continue;
                    }
                    if let Some((owner, target)) = self.param_owner(instance, id)
                        && self.control_automation(owner, target, value as f32)?
                    {
                        continue;
                    }
                    // The value before the edit, if the project doesn't have it (as
                    // `set_slot_param` stores it, outside the undo step).
                    let before = self.editor_state.opened_with.get(&(instance, id)).copied();
                    let slot = self.slot_mut(track, which)?;
                    if let (false, Some(v)) = (slot.params.iter().any(|p| p.0 == id), before) {
                        slot.params.push((id, v));
                    }
                    self.editor_state.opened_with.insert((instance, id), value);
                    // Edits outside a gesture merge as a slider's do; after one, into it.
                    let n = self.editor_state.gestures.get(&(instance, id)).map_or(0, |g| g.0);
                    self.set_slot_param_keyed(track, which, id, value, gesture_key(instance, id, n))?;
                }
                Edit::End(id) => {
                    if let Some(g) = self.editor_state.gestures.get_mut(&(instance, id)) {
                        g.1 = false;
                        self.history.held.remove(&gesture_key(instance, id, g.0));
                    }
                    if let Some((owner, target)) = self.param_owner(instance, id)
                        && self.automation(owner)?.iter().any(|l| l.target == target)
                    {
                        self.release_automation(owner, target)?;
                    }
                }
                Edit::Reload => self.reload_params(track, which)?,
            }
        }
        Ok(())
    }

    /// The plug-in changed values itself (a preset): saved values that differ follow, as
    /// one undo step, and the processor gets them.
    fn reload_params(&mut self, track: usize, which: SlotRef) -> Result<()> {
        let slot = self.slot_ref(track, which)?;
        let Some(plugin) = self.pool.get(&slot.instance) else { return Ok(()) };
        let changed: Vec<(u32, f64)> = slot.params.iter().filter_map(|&(id, saved)| plugin.get_param(id).filter(|v| (v - saved).abs() > 1e-6).map(|v| (id, v))).collect();
        if changed.is_empty() {
            return Ok(());
        }
        let index = if matches!(which, SlotRef::Master(_)) { track } else { self.track_index(track)? };
        self.record("Change Plug-in Settings", None, |e| {
            for (id, value) in changed {
                e.apply_param(index, which, id, value)?;
            }
            Ok(())
        })
    }

    /// Show automation playback in open editors (`instances`): automated parameters follow
    /// their lanes at the playhead, and go back to their set values when no longer
    /// automated. (Other changes reach the plug-in as they're made.) Call every frame.
    pub fn sync_editors(&mut self, instances: &[u64]) {
        let mut shown = HashSet::new();
        for &instance in instances {
            let Some((track, which)) = self.slot_of_instance(instance) else { continue };
            let owner = if matches!(which, SlotRef::Master(_)) { None } else { Some(track) };
            let Ok(slot) = self.slot_ref(track, which) else { continue };
            let (slot_id, saved) = (slot.id, slot.params.clone());
            let Some(plugin) = self.pool.get(&instance) else { continue };
            let lanes = self.automation(owner).map(|l| l.to_vec()).unwrap_or_default();
            for lane in lanes.iter().filter(|l| l.is_active()) {
                let AutomationTarget::Param { slot, param } = lane.target else { continue };
                if slot != slot_id || self.is_writing(owner, lane.target) {
                    continue;
                }
                let Some(value) = self.automated_value(owner, lane.target) else { continue };
                if plugin.get_param(param).is_some_and(|v| (v - value as f64).abs() > 1e-4) {
                    plugin.set_param(param, value as f64);
                }
                shown.insert((instance, param));
            }
            // No longer automated: back to the set value.
            for &(inst, param) in self.editor_state.shown.iter().filter(|k| k.0 == instance && !shown.contains(k)) {
                if let (Some(&(_, value)), Some(plugin)) = (saved.iter().find(|p| p.0 == param), self.pool.get(&inst)) {
                    plugin.set_param(param, value);
                }
            }
        }
        self.editor_state.shown = shown;
        self.editor_state.opened_with.retain(|k, _| instances.contains(&k.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentake_project::{AutomationMode, AutomationPoint};

    use crate::reverb;

    fn value(e: &Engine, t: usize, id: u32) -> f64 {
        e.project().tracks[t].plugins[0].params.iter().find(|p| p.0 == id).map(|p| p.1).unwrap()
    }

    /// The routing is the same whatever plug-in the edits come from; a built-in stands in.
    #[test]
    fn editor_gestures_set_params_with_one_undo_step_each() {
        let mut e = Engine::new();
        let t = e.add_track("Gtr").unwrap();
        e.insert_plugin(t, "reverb", None).unwrap();
        let instance = e.project().tracks[t].plugins[0].instance;
        let initial = e.plugin_params(t, 0).unwrap().into_iter().find(|p| p.id == reverb::DECAY).unwrap().value;
        let before = e.history_info().undo_steps.len();
        e.apply_edits(instance, vec![Edit::Begin(reverb::DECAY), Edit::Perform(reverb::DECAY, 0.2), Edit::Perform(reverb::DECAY, 0.3)]).unwrap();
        assert_eq!(value(&e, t, reverb::DECAY), 0.3);
        e.apply_edits(instance, vec![Edit::Perform(reverb::DECAY, 0.4)]).unwrap();
        // Held, it merges however long it lasts (past the merge window too).
        let key = gesture_key(instance, reverb::DECAY, e.editor_state.next_gesture);
        assert!(e.history.held.contains(&key));
        e.apply_edits(instance, vec![Edit::Perform(reverb::DECAY, 0.5), Edit::End(reverb::DECAY)]).unwrap();
        assert!(e.history.held.is_empty());
        assert_eq!(e.history_info().undo_steps.len(), before + 1, "one gesture, one step");
        // The next gesture is a step of its own, even right after.
        e.apply_edits(instance, vec![Edit::Begin(reverb::DECAY), Edit::Perform(reverb::DECAY, 0.9), Edit::End(reverb::DECAY)]).unwrap();
        assert_eq!(e.history_info().undo_steps.len(), before + 2);
        e.undo().unwrap();
        assert_eq!(value(&e, t, reverb::DECAY), 0.5);
        e.undo().unwrap();
        assert_eq!(value(&e, t, reverb::DECAY), initial, "back to before the first gesture");

        // Closing the editor mid-gesture ends it.
        e.apply_edits(instance, vec![Edit::Begin(reverb::SIZE)]).unwrap();
        e.editor_closed(instance).unwrap();
        assert!(e.history.held.is_empty() && e.editor_state.gestures.values().all(|g| !g.1));

        // A plug-in no longer in the project edits nothing.
        e.remove_plugin(t, 0).unwrap();
        e.apply_edits(instance, vec![Edit::Perform(reverb::DECAY, 0.1)]).unwrap();
        assert!(e.slot_of_instance(instance).is_none());
    }

    #[test]
    fn editor_gestures_write_latch_automation() {
        let mut e = Engine::new();
        let t = e.add_track("Gtr").unwrap();
        e.insert_plugin(t, "reverb", None).unwrap();
        let instance = e.project().tracks[t].plugins[0].instance;
        let target = e.param_target(Some(t), SlotRef::Effect(0), reverb::MIX).unwrap();
        let lane = e.add_lane(Some(t), target).unwrap();
        e.insert_point(Some(t), lane, AutomationPoint::new(0.0, 0.2)).unwrap();
        e.set_lane_mode(Some(t), lane, AutomationMode::Latch).unwrap();
        let mut mixer = e.attach_test_mixer(48_000);
        e.play().unwrap();
        let mut out = vec![0.0f32; 2 * 4800];
        mixer.process(&[], 0, &mut out, 2); // 0.1 s

        e.apply_edits(instance, vec![Edit::Begin(reverb::MIX), Edit::Perform(reverb::MIX, 0.8)]).unwrap();
        assert!(e.is_writing(Some(t), target), "a gesture writes the lane");
        mixer.process(&[], 0, &mut out, 2);
        e.apply_editor_edits().unwrap();
        e.automation_tick().unwrap();
        e.apply_edits(instance, vec![Edit::End(reverb::MIX)]).unwrap();
        assert!(e.is_writing(Some(t), target), "Latch keeps writing after the release");
        mixer.process(&[], 0, &mut out, 2);
        e.automation_tick().unwrap();
        e.stop().unwrap();
        mixer.process(&[], 0, &mut out, 2);
        e.automation_tick().unwrap();
        assert!(!e.is_writing(Some(t), target));
        let points = &e.automation(Some(t)).unwrap()[lane].points;
        assert!(points.iter().any(|p| (p.value - 0.8).abs() < 1e-6 && p.time_secs > 0.25), "{points:?}");
        assert_eq!(e.history_info().undo.as_deref(), Some("Write Automation"));
    }

    #[test]
    fn builtins_have_no_editor_window() {
        let mut e = Engine::new();
        let t = e.add_track("Gtr").unwrap();
        e.insert_plugin(t, "reverb", None).unwrap();
        assert!(!e.has_editor(t, SlotRef::Effect(0)));
        assert!(e.open_editor(t, SlotRef::Effect(0), "x").is_err());
        assert_eq!(e.slot_of_instance(e.project().tracks[t].plugins[0].instance), Some((t, SlotRef::Effect(0))));
        // Requests (MCP) need a GUI to show editors.
        assert!(e.request_editor(t, SlotRef::Effect(0)).is_err());
        e.set_editor_host(true);
        assert!(e.request_editor(t, SlotRef::Effect(3)).is_err(), "no such slot");
        e.request_editor(t, SlotRef::Effect(0)).unwrap();
        assert_eq!(e.take_editor_requests(), vec![(t, SlotRef::Effect(0))]);
        assert!(e.take_editor_requests().is_empty());
    }
}
