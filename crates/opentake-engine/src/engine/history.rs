//! Undo/redo. Each edit stores the project as it was before the edit; undo swaps the
//! current project for that snapshot and updates the audio thread with only the
//! differences (tracks and plugin instances keep their ids, so nothing is reloaded).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use opentake_project::{PluginSlot, Project, Track};
use serde::Serialize;

use super::{Engine, track_params};
use crate::mixer::Command;

const MAX_STEPS: usize = 200;
/// Edits with the same merge key closer together than this become one step
/// (e.g. a fader drag).
const MERGE_WINDOW: Duration = Duration::from_millis(1500);

struct Entry {
    label: String,
    project: Project,
    key: Option<String>,
    at: Instant,
    /// Where undoing this step puts the playhead (seconds), e.g. back where a recording began.
    playhead: Option<f64>,
}

#[derive(Default)]
pub(super) struct History {
    undo: Vec<Entry>,
    redo: Vec<Entry>,
    /// Nesting depth of `record` calls; only the outermost one creates a step.
    depth: usize,
    /// `Entry::playhead` for the step being recorded.
    pub(super) playhead: Option<f64>,
}

impl History {
    pub(super) fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// What can be undone/redone, most recent first.
#[derive(Debug, Clone, Serialize, Default)]
pub struct HistoryInfo {
    pub undo: Option<String>,
    pub redo: Option<String>,
    pub undo_steps: Vec<String>,
    pub redo_steps: Vec<String>,
}

fn slots(t: &Track) -> impl Iterator<Item = &PluginSlot> {
    t.instrument.iter().chain(t.plugins.iter())
}

fn instances(p: &Project) -> impl Iterator<Item = u64> + '_ {
    p.tracks.iter().flat_map(slots).chain(&p.master_plugins).map(|s| s.instance).filter(|&i| i != 0)
}

fn chain_signature(t: &Track) -> Vec<(u64, bool)> {
    t.plugins.iter().map(|s| (s.instance, s.bypass)).collect()
}

impl Engine {
    /// Run an edit as one undoable step labelled `label`. If it fails, the project is
    /// put back the way it was.
    pub(super) fn record<T>(&mut self, label: impl Into<String>, key: Option<String>, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        if self.history.depth > 0 {
            return f(self);
        }
        let before = self.project.clone();
        self.history.depth += 1;
        let mut result = f(self);
        self.history.depth -= 1;
        if result.is_ok() {
            // New plug-ins get their automation ids; lanes of removed ones go with them.
            self.project.assign_slot_ids();
            self.project.prune_automation();
        }
        // Bus tracks only mix: whatever path an edit took, it can't give one regions or input.
        if let (Ok(_), Some(bus)) = (&result, self.project.tracks.iter().find(|t| t.bus && (!t.clips.is_empty() || !t.midi_clips.is_empty() || t.instrument.is_some() || t.armed || t.monitor_input))) {
            result = Err(anyhow::anyhow!("'{}' is a bus: it has no regions, instrument or input", bus.name));
        }
        let playhead = self.history.playhead.take();
        match result {
            Ok(v) => {
                if self.project != before {
                    let merge = key.is_some()
                        && self.history.undo.last().is_some_and(|e| e.key == key && e.at.elapsed() < MERGE_WINDOW);
                    match self.history.undo.last_mut() {
                        Some(last) if merge => last.at = Instant::now(),
                        _ => {
                            self.history.undo.push(Entry { label: label.into(), project: before, key, at: Instant::now(), playhead });
                            if self.history.undo.len() > MAX_STEPS {
                                self.history.undo.remove(0);
                                self.prune_pool();
                            }
                        }
                    }
                    self.history.redo.clear();
                    self.sync_song_end()?;
                }
                Ok(v)
            }
            Err(e) => {
                if self.project != before {
                    let _ = self.restore(before);
                }
                Err(e)
            }
        }
    }

    /// Undo the last edit. Returns its label, or `None` if there was nothing to undo.
    pub fn undo(&mut self) -> Result<Option<String>> {
        let Some(entry) = self.history.undo.pop() else { return Ok(None) };
        let current = self.project.clone();
        self.restore(entry.project)?;
        if let Some(secs) = entry.playhead {
            self.seek(secs)?;
        }
        self.history.redo.push(Entry { label: entry.label.clone(), project: current, key: None, at: Instant::now(), playhead: entry.playhead });
        Ok(Some(entry.label))
    }

    /// Redo the last undone edit. Returns its label, or `None` if there was nothing to redo.
    pub fn redo(&mut self) -> Result<Option<String>> {
        let Some(entry) = self.history.redo.pop() else { return Ok(None) };
        let current = self.project.clone();
        self.restore(entry.project)?;
        self.history.undo.push(Entry { label: entry.label.clone(), project: current, key: None, at: Instant::now(), playhead: entry.playhead });
        Ok(Some(entry.label))
    }

    pub fn history_info(&self) -> HistoryInfo {
        let labels = |v: &Vec<Entry>| v.iter().rev().map(|e| e.label.clone()).collect::<Vec<_>>();
        HistoryInfo {
            undo: self.history.undo.last().map(|e| e.label.clone()),
            redo: self.history.redo.last().map(|e| e.label.clone()),
            undo_steps: labels(&self.history.undo),
            redo_steps: labels(&self.history.redo),
        }
    }

    /// Drop pooled plugin instances that neither the project nor the history refers to.
    pub(super) fn prune_pool(&mut self) {
        let mut used: HashSet<u64> = instances(&self.project).collect();
        for e in self.history.undo.iter().chain(&self.history.redo) {
            used.extend(instances(&e.project));
        }
        self.pool.retain(|id, _| used.contains(id));
    }

    /// Make `target` the current project, updating plugins and the audio thread with only
    /// what differs from the current state.
    fn restore(&mut self, mut target: Project) -> Result<()> {
        // Session settings are not part of the edit history.
        target.metronome_enabled = self.project.metronome_enabled;
        target.metronome_volume = self.project.metronome_volume;
        target.loop_enabled = self.project.loop_enabled;
        target.loop_start_secs = self.project.loop_start_secs;
        target.loop_end_secs = self.project.loop_end_secs;
        target.count_in_bars = self.project.count_in_bars;
        target.count_in_on_play = self.project.count_in_on_play;
        target.auto_input_monitoring = self.project.auto_input_monitoring;
        target.sample_rate = self.project.sample_rate;
        // Whether a track shows its automation is a view setting too.
        for t in &mut target.tracks {
            if let Some(o) = self.project.tracks.iter().find(|o| o.id == t.id) {
                t.show_automation = o.show_automation;
            }
        }
        // Controls being written refer to the lanes as they were.
        self.writing.clear();
        let old = std::mem::replace(&mut self.project, target);

        // Plugin parameter values: controllers exist whether or not audio is running.
        let tracks = self.project.tracks.clone();
        for t in &tracks {
            let old_track = old.tracks.iter().find(|o| o.id == t.id);
            for slot in slots(t) {
                let old_slot = old_track.and_then(|o| slots(o).find(|s| s.instance == slot.instance));
                for &(id, value) in &slot.params {
                    let prev = old_slot.and_then(|s| s.params.iter().find(|p| p.0 == id)).map(|p| p.1);
                    if prev == Some(value) {
                        continue;
                    }
                    if let Some(plugin) = self.pool.get(&slot.instance) {
                        plugin.set_param(id, value);
                    }
                    if old_slot.is_some() {
                        self.send(Command::SetPluginParam { track: t.id, instance: slot.instance, id, value })?;
                    }
                }
            }
        }

        if self.audio.is_none() {
            return Ok(());
        }
        let sr = self.sample_rate();
        let old_ids: Vec<u64> = old.tracks.iter().map(|t| t.id).collect();
        let new_ids: Vec<u64> = tracks.iter().map(|t| t.id).collect();
        for id in old_ids.iter().filter(|id| !new_ids.contains(id)) {
            self.live_chain.remove(id);
            self.live_instrument.remove(id);
            self.send(Command::RemoveTrack(*id))?;
        }
        let mut added = false;
        for (i, t) in tracks.iter().enumerate() {
            match old.tracks.iter().find(|o| o.id == t.id) {
                None => {
                    added = true;
                    let rt = self.build_track(i, sr)?;
                    self.send(Command::AddTrack(rt))?;
                }
                Some(o) => {
                    if track_params(o) != track_params(t) {
                        self.send(Command::SetTrack { track: t.id, params: track_params(t) })?;
                    }
                    if o.clips != t.clips {
                        self.resync_clips(i)?;
                    }
                    if o.midi_clips != t.midi_clips {
                        self.resync_midi(i)?;
                    }
                    let sig = |s: &Option<PluginSlot>| s.as_ref().map(|s| (s.instance, s.bypass));
                    if sig(&o.instrument) != sig(&t.instrument) {
                        self.sync_instrument(i)?;
                    }
                    if chain_signature(o) != chain_signature(t) {
                        self.sync_chain(i)?;
                    }
                    if o.automation != t.automation {
                        self.sync_automation(Some(i))?;
                    }
                }
            }
        }
        if added || old_ids != new_ids {
            self.send(Command::ReorderTracks(Box::new(new_ids)))?;
        }
        let outputs = |p: &Project| p.tracks.iter().map(|t| t.output).collect::<Vec<_>>();
        if added || old_ids != self.project.tracks.iter().map(|t| t.id).collect::<Vec<_>>() || outputs(&old) != outputs(&self.project) {
            self.sync_routing()?;
        }
        // Master inserts: parameter values and the chain.
        for slot in self.project.master_plugins.clone() {
            let old_slot = old.master_plugins.iter().find(|s| s.instance == slot.instance);
            for &(id, value) in &slot.params {
                if old_slot.and_then(|s| s.params.iter().find(|p| p.0 == id)).map(|p| p.1) == Some(value) {
                    continue;
                }
                if let Some(plugin) = self.pool.get(&slot.instance) {
                    plugin.set_param(id, value);
                }
                if old_slot.is_some() {
                    self.send(Command::SetPluginParam { track: crate::mixer::MASTER, instance: slot.instance, id, value })?;
                }
            }
        }
        let master_sig = |p: &Project| p.master_plugins.iter().map(|s| (s.instance, s.bypass)).collect::<Vec<_>>();
        if master_sig(&old) != master_sig(&self.project) {
            self.sync_master_chain()?;
        } else if old.master_automation != self.project.master_automation {
            self.sync_automation(None)?;
        }
        if (old.tempo_bpm, old.time_signature, &old.tempo_changes) != (self.project.tempo_bpm, self.project.time_signature, &self.project.tempo_changes) {
            let cmd = self.tempo_command();
            self.send(cmd)?;
        }
        if old.master_volume != self.project.master_volume {
            self.send(Command::SetMasterGain(self.project.master_volume))?;
        }
        self.sync_song_end()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::tone_file;
    use super::super::{Engine, TrackUpdate};

    #[test]
    fn tempo_changes_and_master_eq_are_undoable_but_count_in_is_not() {
        use crate::eq::{self, Field};
        use opentake_project::TempoChange;
        let mut e = Engine::new();
        let change = TempoChange { at_secs: 4.0, bpm: 90.0, time_signature: Some((3, 4)) };
        e.set_tempo_changes(vec![change], None).unwrap();
        assert_eq!(e.project().tempo_map().tempo_at(5.0), 90.0);
        assert!(e.set_tempo_changes(vec![TempoChange { at_secs: 0.0, ..change }], None).is_err(), "changes come after the start");
        assert!(e.set_tempo_changes(vec![TempoChange { time_signature: Some((5, 3)), ..change }], None).is_err());

        e.insert_master_plugin("eq", None).unwrap();
        assert_eq!(e.project().master_plugins[0].name, eq::NAME);
        let gain = eq::param_id(2, Field::Gain);
        e.set_slot_param(0, super::super::SlotRef::Master(0), gain, eq::norm_gain(6.0)).unwrap();
        let params = e.slot_params(0, super::super::SlotRef::Master(0)).unwrap();
        assert_eq!(params.iter().find(|p| p.id == gain).and_then(|p| p.display.clone()).as_deref(), Some("+6.0"));
        e.set_count_in(Some(2), Some(true)).unwrap();

        e.undo().unwrap(); // EQ gain
        e.undo().unwrap(); // EQ insert
        assert!(e.project().master_plugins.is_empty());
        e.undo().unwrap(); // tempo change
        assert!(e.project().tempo_changes.is_empty());
        assert_eq!((e.project().count_in_bars, e.project().count_in_on_play), (2, true), "count-in is a setting, not an edit");
        e.redo().unwrap();
        e.redo().unwrap();
        assert_eq!(e.project().tempo_changes, vec![change]);
        assert_eq!(e.project().master_plugins.len(), 1);
    }

    #[test]
    fn undo_redo_track_and_clip_edits() {
        let dir = std::env::temp_dir().join(format!("opentake-undo-{}", std::process::id()));
        let wav = tone_file(&dir, 2.0);
        let mut e = Engine::new();
        let t = e.add_track("A").unwrap();
        e.add_clip(t, &wav, 0.0).unwrap();
        e.move_clip(t, 0, 3.0, None).unwrap();
        assert_eq!(e.history_info().undo.as_deref(), Some("Move Region"));

        assert_eq!(e.undo().unwrap().as_deref(), Some("Move Region"));
        assert_eq!(e.project().tracks[0].clips[0].position_secs, 0.0);
        assert_eq!(e.undo().unwrap().as_deref(), Some("Add Region"));
        assert!(e.project().tracks[0].clips.is_empty());
        assert_eq!(e.redo().unwrap().as_deref(), Some("Add Region"));
        assert_eq!(e.redo().unwrap().as_deref(), Some("Move Region"));
        assert_eq!(e.project().tracks[0].clips[0].position_secs, 3.0);
        assert_eq!(e.redo().unwrap(), None);

        // A new edit clears the redo stack.
        e.undo().unwrap();
        e.update_track(t, TrackUpdate { mute: Some(true), ..Default::default() }).unwrap();
        assert!(e.history_info().redo.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fader_drags_merge_into_one_step() {
        let mut e = Engine::new();
        let t = e.add_track("A").unwrap();
        for v in [0.9, 0.8, 0.7, 0.6] {
            e.update_track(t, TrackUpdate { volume: Some(v), ..Default::default() }).unwrap();
        }
        assert_eq!(e.history_info().undo_steps, vec!["Volume", "New Track"]);
        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].volume, 1.0);
    }

    #[test]
    fn undo_restores_deleted_track_with_same_identity() {
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        e.add_track("B").unwrap();
        let id_b = e.project().tracks[1].id;
        e.remove_track(0).unwrap();
        e.undo().unwrap();
        let names: Vec<_> = e.project().tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"], "track comes back in its old position");
        assert_eq!(e.project().tracks[1].id, id_b);
    }

    #[test]
    fn failed_edit_leaves_no_step() {
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        assert!(e.split_clip(0, 0, 1.0).is_err());
        assert_eq!(e.history_info().undo_steps, vec!["New Track"]);
    }

    /// The audio thread must mirror the project after every edit, undo and redo.
    #[test]
    fn undo_redo_keep_audio_thread_in_sync() {
        let mut e = Engine::new();
        let mut mixer = e.attach_test_mixer(48_000);
        let mut out = vec![0.0f32; 2 * 64];
        let mut check = |e: &Engine, mixer: &mut crate::mixer::Mixer, what: &str| {
            mixer.process(&[], 0, &mut out, 2);
            let expected: Vec<_> = e
                .project()
                .tracks
                .iter()
                .map(|t| {
                    let chain = t.plugins.iter().map(|s| (s.instance, s.bypass)).collect::<Vec<_>>();
                    (t.id, t.instrument.as_ref().map(|s| s.instance), chain, t.clips.len(), t.midi_clips.len())
                })
                .collect();
            assert_eq!(mixer.debug_tracks(), expected, "audio thread out of sync after {what}");
        };

        let a = e.add_track("A").unwrap();
        let k = e.add_instrument_track("Keys", "synth").unwrap();
        e.add_midi_clip(k, 0.0, 2.0, None).unwrap();
        // The built-in synth doubles as an "effect" here so chains can be tested without VST3s.
        e.insert_plugin(a, "synth", None).unwrap();
        e.insert_plugin(a, "synth", None).unwrap();
        e.set_plugin_bypass(a, 1, true).unwrap();
        check(&e, &mut mixer, "edits");
        let instances: Vec<u64> = e.project().tracks[a].plugins.iter().map(|s| s.instance).collect();

        e.remove_plugin(a, 0).unwrap();
        check(&e, &mut mixer, "remove plugin");
        e.undo().unwrap();
        check(&e, &mut mixer, "undo remove plugin");
        let restored: Vec<u64> = e.project().tracks[a].plugins.iter().map(|s| s.instance).collect();
        assert_eq!(restored, instances, "the same instances come back, in order");

        e.remove_track(a).unwrap();
        check(&e, &mut mixer, "delete track");
        e.undo().unwrap();
        check(&e, &mut mixer, "undo delete track (order restored)");

        e.set_instrument(k, None).unwrap();
        check(&e, &mut mixer, "remove instrument");
        e.undo().unwrap();
        check(&e, &mut mixer, "undo remove instrument");

        while e.undo().unwrap().is_some() {
            check(&e, &mut mixer, "undo all");
        }
        while e.redo().unwrap().is_some() {
            check(&e, &mut mixer, "redo all");
        }
    }
}
