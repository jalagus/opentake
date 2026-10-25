//! Sends: a track (or bus) feeds buses besides its output, each send at its own level,
//! before or after the fader (e.g. several tracks into one reverb bus). Sends name their bus
//! by track id like outputs do, and the routing stays loop-free counting both.

use anyhow::{Result, anyhow, bail};
use opentake_project::{AuxSend, MAX_SEND_GAIN};

use super::Engine;
use crate::mixer::{Command, SendRt};

/// Optional changes to a send; `None` leaves a field unchanged.
#[derive(Debug, Clone, Default)]
pub struct SendUpdate {
    /// Another bus (track index).
    pub bus: Option<usize>,
    /// Linear gain, 1.0 = 0 dB (max +6 dB).
    pub level: Option<f32>,
    pub pre_fader: Option<bool>,
    pub mute: Option<bool>,
}

impl SendUpdate {
    /// Undo label and, for the level, a merge key so a knob drag is one step.
    fn describe(&self, track_id: u64, send_id: u32) -> (&'static str, Option<String>) {
        match (self.bus.is_some(), self.level.is_some(), self.pre_fader.is_some(), self.mute.is_some()) {
            (true, false, false, false) => ("Send Destination", None),
            (false, true, false, false) => ("Send Level", Some(format!("send:{track_id}:{send_id}"))),
            (false, false, true, false) => ("Send Pre/Post", None),
            (false, false, false, true) => ("Send Mute", None),
            _ => ("Send Settings", None),
        }
    }
}

impl Engine {
    fn check_send(&self, index: usize, bus: usize) -> Result<()> {
        let p = &self.project;
        match p.tracks.get(bus) {
            None => bail!("no track at index {bus}"),
            Some(b) if !b.bus => bail!("track {bus} ('{}') is not a bus; sends go to buses", b.name),
            _ if !p.can_send(index, bus) => bail!("a send from '{}' to '{}' would make a loop", p.tracks[index].name, p.tracks[bus].name),
            _ => Ok(()),
        }
    }

    fn send_index(&self, track: usize, send: usize) -> Result<usize> {
        let index = self.track_index(track)?;
        let n = self.project.tracks[index].sends.len();
        if send >= n {
            bail!("track {track} has no send {send} (it has {n})");
        }
        Ok(index)
    }

    /// Add a send from track `track` to bus `bus` (track indices) at `level` (linear).
    /// Returns the send's index on the track.
    pub fn add_send(&mut self, track: usize, bus: usize, level: f32, pre_fader: bool) -> Result<usize> {
        let index = self.track_index(track)?;
        self.check_send(index, bus)?;
        self.record("Add Send", None, |e| e.add_send_impl(index, bus, level, pre_fader))
    }

    fn add_send_impl(&mut self, index: usize, bus: usize, level: f32, pre_fader: bool) -> Result<usize> {
        let bus_id = self.project.tracks[bus].id;
        let t = &mut self.project.tracks[index];
        // An id right away: the audio thread and automation lanes name sends by it.
        let id = t.sends.iter().map(|s| s.id).max().unwrap_or(0) + 1;
        t.sends.push(AuxSend { level: level.clamp(0.0, MAX_SEND_GAIN), pre_fader, id, ..AuxSend::new(bus_id) });
        self.sync_sends(index)?;
        self.sync_routing()?;
        Ok(self.project.tracks[index].sends.len() - 1)
    }

    /// Change a send's bus, level, pre/post-fader tap or mute.
    pub fn update_send(&mut self, track: usize, send: usize, u: SendUpdate) -> Result<AuxSend> {
        let index = self.send_index(track, send)?;
        if let Some(bus) = u.bus {
            self.check_send(index, bus)?;
        }
        let (label, key) = u.describe(self.track_id(index), self.project.tracks[index].sends[send].id);
        self.record(label, key, |e| {
            let bus_id = u.bus.map(|b| e.project.tracks[b].id);
            let s = &mut e.project.tracks[index].sends[send];
            if let Some(id) = bus_id {
                s.bus = id;
            }
            if let Some(v) = u.level {
                s.level = v.clamp(0.0, MAX_SEND_GAIN);
            }
            if let Some(v) = u.pre_fader {
                s.pre_fader = v;
            }
            if let Some(v) = u.mute {
                s.mute = v;
            }
            let s = s.clone();
            e.sync_sends(index)?;
            if bus_id.is_some() {
                e.sync_routing()?;
            }
            Ok(s)
        })
    }

    pub fn remove_send(&mut self, track: usize, send: usize) -> Result<AuxSend> {
        let index = self.send_index(track, send)?;
        self.record("Remove Send", None, |e| {
            let removed = e.project.tracks[index].sends.remove(send);
            e.sync_sends(index)?;
            e.sync_routing()?;
            Ok(removed)
        })
    }

    /// New bus named `name` (to the master) with a send to it from `track` at `level`, as one
    /// undo step; with `effect`, that built-in effect is inserted on the bus if OpenTake has it
    /// (e.g. "reverb"; the bus is made without it otherwise). Returns (bus index, send index).
    pub fn send_to_new_bus(&mut self, track: usize, name: &str, effect: Option<&str>, level: f32, pre_fader: bool) -> Result<(usize, usize)> {
        let index = self.track_index(track)?;
        self.record("New Send Bus", None, |e| {
            let bus = e.add_bus_impl(name)?;
            if let Some(fx) = effect.filter(|fx| super::plugins::is_builtin_effect(fx)) {
                e.insert_plugin(bus, fx, None)?;
            }
            Ok((bus, e.add_send_impl(index, bus, level, pre_fader)?))
        })
    }

    /// Move send `send` of `track` to a new bus named `name` (to the master), as one undo
    /// step. Returns the bus's index.
    pub fn move_send_to_new_bus(&mut self, track: usize, send: usize, name: &str) -> Result<usize> {
        let index = self.send_index(track, send)?;
        self.record("New Send Bus", None, |e| {
            let bus = e.add_bus_impl(name)?;
            e.project.tracks[index].sends[send].bus = e.project.tracks[bus].id;
            e.sync_sends(index)?;
            e.sync_routing()?;
            Ok(bus)
        })
    }

    /// Before deleting bus `index`: sends to it go (undo brings them back).
    pub(super) fn drop_sends_to(&mut self, index: usize) -> Result<()> {
        for i in self.project.send_sources_of(index) {
            let gone: Vec<bool> = (0..self.project.tracks[i].sends.len()).map(|k| self.project.send_dest(i, k) == Some(index)).collect();
            let mut it = gone.into_iter();
            self.project.tracks[i].sends.retain(|_| !it.next().unwrap_or(false));
            self.sync_sends(i)?;
        }
        Ok(())
    }

    /// What the audio thread plays for a track's sends.
    pub(super) fn send_rts(&self, index: usize) -> Vec<SendRt> {
        self.project.tracks[index].sends.iter().map(|s| SendRt::new(s.id, s.bus, s.level, s.pre_fader, s.mute)).collect()
    }

    pub(super) fn sync_sends(&mut self, index: usize) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let sends = Box::new(self.send_rts(index));
        self.send(Command::SetSends { track: self.track_id(index), sends })
    }

    /// The bus a send's level target belongs to, for naming it ("Send to Reverb").
    pub(super) fn send_name(&self, track: usize, send_id: u32) -> Result<String> {
        let t = self.project.tracks.get(track).ok_or_else(|| anyhow!("no track at index {track}"))?;
        let k = t.sends.iter().position(|s| s.id == send_id).ok_or_else(|| anyhow!("no such send"))?;
        Ok(format!("Send to {}", self.project.send_dest(track, k).map_or("?", |b| self.project.tracks[b].name.as_str())))
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::tone_file;
    use super::super::{Engine, TrackUpdate};
    use super::SendUpdate;
    use opentake_project::AutomationTarget;
    use opentake_project::automation::{AutomationPoint, gain_to_pos};

    #[test]
    fn sends_are_undoable_refuse_loops_and_level_drags_merge() {
        let mut e = Engine::new();
        let gtr = e.add_track("Gtr").unwrap();
        let verb = e.add_bus("Verb").unwrap();
        let group = e.add_bus("Group").unwrap();
        assert_eq!(e.add_send(gtr, verb, 0.5, false).unwrap(), 0);
        assert!(e.add_send(gtr, gtr, 1.0, false).is_err() && e.add_send(verb, verb, 1.0, false).is_err());
        e.set_track_output(verb, Some(group)).unwrap();
        assert!(e.add_send(group, verb, 1.0, false).is_err(), "group -> verb -> group");
        assert!(e.set_track_output(group, Some(verb)).is_err());
        for v in [0.4, 0.3, 0.2] {
            e.update_send(gtr, 0, SendUpdate { level: Some(v), ..Default::default() }).unwrap();
        }
        e.update_send(gtr, 0, SendUpdate { pre_fader: Some(true), ..Default::default() }).unwrap();
        assert_eq!(e.history_info().undo_steps[..3], ["Send Pre/Post", "Send Level", "Track Output"]);
        e.undo().unwrap();
        e.undo().unwrap();
        let s = &e.project().tracks[gtr].sends[0];
        assert_eq!((s.level, s.pre_fader, s.id), (0.5, false, 1));
        assert!(e.update_send(gtr, 3, SendUpdate::default()).is_err());
        e.remove_send(gtr, 0).unwrap();
        assert!(e.project().tracks[gtr].sends.is_empty());
        e.undo().unwrap();
        assert_eq!(e.project().send_sources_of(verb), vec![gtr]);
    }

    #[test]
    fn deleting_a_bus_drops_sends_to_it_and_undo_brings_them_back() {
        let mut e = Engine::new();
        let gtr = e.add_track("Gtr").unwrap();
        let (verb, send) = e.send_to_new_bus(gtr, "Reverb", Some("reverb"), 1.0, false).unwrap();
        assert_eq!(e.history_info().undo.as_deref(), Some("New Send Bus"));
        assert_eq!(e.project().tracks[verb].name, "Reverb");
        // A send-level lane goes with the send.
        let target = AutomationTarget::Send { send: e.project().tracks[gtr].sends[send].id };
        let lane = e.add_lane(Some(gtr), target).unwrap();
        e.insert_point(Some(gtr), lane, AutomationPoint::new(0.0, 0.5)).unwrap();
        assert_eq!(e.target_name(Some(gtr), target), "Send to Reverb");
        e.remove_track(verb).unwrap();
        assert!(e.project().tracks[gtr].sends.is_empty() && e.project().tracks[gtr].automation.is_empty());
        e.undo().unwrap();
        assert_eq!(e.project().send_dest(gtr, 0), Some(verb));
        assert_eq!(e.project().tracks[gtr].automation[0].target, target);
        e.undo().unwrap();
        e.undo().unwrap();
        e.undo().unwrap();
        assert_eq!(e.project().tracks.len(), 1, "the bus and its send were one step");
    }

    #[test]
    fn sends_survive_save_and_load() {
        let dir = std::env::temp_dir().join(format!("opentake-sends-{}", std::process::id()));
        let mut e = Engine::new();
        let gtr = e.add_track("Gtr").unwrap();
        let verb = e.add_bus("Verb").unwrap();
        e.add_send(gtr, verb, 0.25, true).unwrap();
        e.update_send(gtr, 0, SendUpdate { mute: Some(true), ..Default::default() }).unwrap();
        e.move_track(verb, 0).unwrap(); // indices in the file differ from creation order
        for ext in ["otp", "rpp"] {
            let path = dir.join(format!("song.{ext}"));
            e.save_project(Some(&path)).unwrap();
            let mut loaded = Engine::new();
            loaded.load_project(&path).unwrap();
            let p = loaded.project();
            let s = &p.tracks[1].sends[0];
            assert_eq!((p.send_dest(1, 0), s.level, s.pre_fader, s.mute), (Some(0), 0.25, true, true), "{ext}");
            assert_eq!(s.bus, p.tracks[0].id, "{ext}: indices become ids");
            assert_ne!(s.id, 0, "{ext}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    fn peak(m: &mut crate::mixer::Mixer, blocks: usize) -> f32 {
        let mut out = vec![0.0f32; 2 * 480];
        let mut peak = 0.0f32;
        for _ in 0..blocks {
            m.process(&[], 0, &mut out, 2);
            peak = out.iter().fold(peak, |a, v| a.max(v.abs()));
        }
        peak
    }

    /// The track's fader is down: only a pre-fader send is heard, through the bus.
    #[test]
    fn audio_thread_follows_send_edits_and_undo() {
        let dir = std::env::temp_dir().join(format!("opentake-sends-rt-{}", std::process::id()));
        let wav = tone_file(&dir, 2.0);
        let mut e = Engine::new();
        let a = e.add_track("A").unwrap();
        e.add_clip(a, &wav, 0.0).unwrap();
        e.update_track(a, TrackUpdate { volume: Some(0.0), ..Default::default() }).unwrap();
        let mut mixer = e.attach_test_mixer(48_000);
        let verb = e.add_bus("Verb").unwrap();
        e.play().unwrap();
        assert_eq!(peak(&mut mixer, 4), 0.0, "fader down, no send");
        e.add_send(a, verb, 1.0, true).unwrap();
        assert!(peak(&mut mixer, 4) > 0.4, "pre-fader send");
        e.update_send(a, 0, SendUpdate { pre_fader: Some(false), ..Default::default() }).unwrap();
        peak(&mut mixer, 1); // the post-fader gain ramps down over a block
        assert_eq!(peak(&mut mixer, 4), 0.0, "post-fader follows the fader");
        e.undo().unwrap();
        assert!(peak(&mut mixer, 4) > 0.4, "undo resyncs the sends");
        e.remove_track(verb).unwrap();
        assert_eq!(peak(&mut mixer, 4), 0.0);
        e.undo().unwrap();
        assert!(peak(&mut mixer, 4) > 0.4, "undo restores the bus and the send to it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Offline render and export: the bus stem holds what the send brings in, and the stems
    /// still add up to the mix.
    #[test]
    fn render_and_stems_include_sends() {
        let dir = std::env::temp_dir().join(format!("opentake-sends-render-{}", std::process::id()));
        let wav = tone_file(&dir, 1.0);
        let mut e = Engine::new();
        let a = e.add_track("A").unwrap();
        e.add_clip(a, &wav, 0.0).unwrap();
        let verb = e.add_bus("Verb").unwrap();
        e.update_track(a, TrackUpdate { volume: Some(0.5), ..Default::default() }).unwrap();
        e.add_send(a, verb, 0.5, true).unwrap();
        let r = e.mix_job().render(None, None, 0.0);
        let peak = |x: &[f32]| x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let (dry, wet) = (peak(&r.tracks[a].1[0]), peak(&r.tracks[verb].1[0]));
        assert!((dry - 0.25).abs() < 0.01 && (wet - 0.25).abs() < 0.01, "pre-fader: {dry} {wet}");
        assert!((peak(&r.master[0]) - 0.5).abs() < 0.01);
        // Post-fader and automated: the send level follows its lane (silent here).
        e.update_send(a, 0, SendUpdate { pre_fader: Some(false), ..Default::default() }).unwrap();
        let r = e.mix_job().render(None, None, 0.0);
        assert!((peak(&r.tracks[verb].1[0]) - 0.125).abs() < 0.01);
        let target = AutomationTarget::Send { send: e.project().tracks[a].sends[0].id };
        let lane = e.add_lane(Some(a), target).unwrap();
        e.insert_point(Some(a), lane, AutomationPoint::new(0.0, 0.0)).unwrap();
        assert_eq!(peak(&e.mix_job().render(None, None, 0.0).tracks[verb].1[0]), 0.0);
        e.set_automation_points(Some(a), lane, vec![AutomationPoint::new(0.0, gain_to_pos(1.0))], true).unwrap();

        let options = crate::ExportOptions { stems_dir: Some(dir.join("stems")), tail_secs: 0.0, ..Default::default() };
        let report = e.export_audio(&options).unwrap();
        let names: Vec<&str> = report.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["A", "Verb"]);
        assert!((report.files[1].peak_db - report.files[0].peak_db).abs() < 0.2, "unity post-fader send: same level");
        std::fs::remove_dir_all(&dir).ok();
    }
}
