//! Buses and track outputs. A bus is a track without regions that sums the tracks routed
//! to it, runs them through its effects and fader, and feeds the master or another bus.
//! Outputs refer to buses by track id, so they survive reordering and undo.

use anyhow::{Result, bail};
use opentake_project::Track;

use super::Engine;
use crate::mixer::Command;

impl Engine {
    /// New bus track at the end. Returns its index.
    pub fn add_bus(&mut self, name: &str) -> Result<usize> {
        self.record("New Bus", None, |e| e.add_bus_impl(name))
    }

    pub(super) fn add_bus_impl(&mut self, name: &str) -> Result<usize> {
        let mut track = Track::new_bus(name);
        track.id = self.next_id();
        self.project.tracks.push(track);
        let index = self.project.tracks.len() - 1;
        if self.audio.is_some() {
            let rt = self.build_track(index, self.sample_rate())?;
            self.send(Command::AddTrack(rt))?;
        }
        self.sync_routing()?;
        Ok(index)
    }

    /// Route track `index` to bus track `to`, or to the master with `None`. A bus can feed
    /// another bus, but never one that (through others) feeds it.
    pub fn set_track_output(&mut self, index: usize, to: Option<usize>) -> Result<()> {
        let index = self.track_index(index)?;
        self.check_output(index, to)?;
        self.record("Track Output", None, |e| e.set_output_impl(index, to))
    }

    fn check_output(&self, index: usize, to: Option<usize>) -> Result<()> {
        let p = &self.project;
        match to {
            Some(b) if b >= p.tracks.len() => bail!("no track at index {b}"),
            Some(b) if !p.tracks[b].bus => bail!("track {b} ('{}') is not a bus; outputs go to a bus or the master", p.tracks[b].name),
            _ if !p.can_route(index, to) => bail!("routing '{}' there would make a loop", p.tracks[index].name),
            _ => Ok(()),
        }
    }

    fn set_output_impl(&mut self, index: usize, to: Option<usize>) -> Result<()> {
        let output = to.map(|b| self.project.tracks[b].id);
        let t = &mut self.project.tracks[index];
        t.output = output;
        let (id, params) = (t.id, super::track_params(t));
        self.send(Command::SetTrack { track: id, params })?;
        self.sync_routing()
    }

    /// New bus that the given tracks feed (Logic's "Create Bus from Selected Tracks"); it
    /// feeds where the first of them went. One undo step. Returns the bus's index.
    pub fn create_bus_from(&mut self, tracks: &[usize], name: &str) -> Result<usize> {
        for &t in tracks {
            self.track_index(t)?;
        }
        let first_output = tracks.first().and_then(|&t| self.project.output_of(t));
        self.record("Create Bus", None, |e| {
            let bus = e.add_bus_impl(name)?;
            for &t in tracks {
                e.check_output(t, Some(bus))?;
                e.set_output_impl(t, Some(bus))?;
            }
            if first_output.is_some_and(|o| e.project.can_route(bus, Some(o))) {
                e.set_output_impl(bus, first_output)?;
            }
            Ok(bus)
        })
    }

    /// Before deleting bus `index`: whatever fed it goes where the bus went; sends to it go.
    pub(super) fn unroute_bus(&mut self, index: usize) -> Result<()> {
        let output = self.project.output_of(index);
        for s in self.project.sources_of(index) {
            self.set_output_impl(s, output)?;
        }
        self.drop_sends_to(index)
    }

    /// Tell the audio thread the processing order (every track before the buses it feeds,
    /// through its output or sends).
    pub(super) fn sync_routing(&mut self) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let order = self.routing_command();
        self.send(order)
    }

    pub(super) fn routing_command(&self) -> Command {
        let p = &self.project;
        Command::SetRouting(Box::new(p.processing_order().into_iter().map(|i| p.tracks[i].id).collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Engine, TrackUpdate};

    #[test]
    fn outputs_refuse_loops_and_non_buses() {
        let mut e = Engine::new();
        let gtr = e.add_track("Gtr").unwrap();
        let a = e.add_bus("A").unwrap();
        let b = e.add_bus("B").unwrap();
        e.set_track_output(gtr, Some(a)).unwrap();
        e.set_track_output(a, Some(b)).unwrap();
        assert!(e.set_track_output(b, Some(a)).is_err(), "B feeding A would loop");
        assert!(e.set_track_output(a, Some(a)).is_err());
        assert!(e.set_track_output(a, Some(gtr)).is_err(), "only buses take inputs");
        assert!(e.set_track_output(gtr, Some(9)).is_err());
        assert_eq!(e.project().route(gtr), vec![a, b]);
        // Buses take no regions and can't record.
        assert!(e.add_midi_clip(a, 0.0, 1.0, None).is_err());
        assert!(e.update_track(a, TrackUpdate { armed: Some(true), ..Default::default() }).is_err());
        assert_eq!(e.history_info().undo, Some("Track Output".into()));
    }

    #[test]
    fn deleting_a_bus_reroutes_its_sources_and_undo_restores_it() {
        let mut e = Engine::new();
        let gtr = e.add_track("Gtr").unwrap();
        let keys = e.add_track("Keys").unwrap();
        let outer = e.add_bus("Outer").unwrap();
        let group = e.create_bus_from(&[gtr, keys], "Group").unwrap();
        assert_eq!(e.history_info().undo, Some("Create Bus".into()));
        assert_eq!(e.project().sources_of(group), vec![gtr, keys]);
        e.set_track_output(group, Some(outer)).unwrap();

        e.remove_track(group).unwrap();
        assert_eq!(e.project().sources_of(outer), vec![gtr, keys], "sources go where the bus went");
        e.undo().unwrap();
        assert_eq!(e.project().tracks.len(), 4);
        assert_eq!(e.project().sources_of(group), vec![gtr, keys]);
        assert_eq!(e.project().route(gtr), vec![group, outer]);
        e.undo().unwrap();
        e.undo().unwrap();
        assert_eq!(e.project().tracks.len(), 3, "creating the bus and routing to it was one step");
        assert!(e.project().tracks.iter().all(|t| t.output.is_none()));
    }

    #[test]
    fn routing_survives_save_and_load() {
        let dir = std::env::temp_dir().join(format!("opentake-routing-{}", std::process::id()));
        let mut e = Engine::new();
        let gtr = e.add_track("Gtr").unwrap();
        let bus = e.create_bus_from(&[gtr], "Bus").unwrap();
        for ext in ["otp", "rpp"] {
            let path = dir.join(format!("song.{ext}"));
            e.save_project(Some(&path)).unwrap();
            let mut loaded = Engine::new();
            loaded.load_project(&path).unwrap();
            let p = loaded.project();
            assert!(p.tracks[bus].bus, "{ext}");
            assert_eq!(p.output_of(gtr), Some(bus), "{ext}");
            assert_eq!(p.tracks[gtr].output, Some(p.tracks[bus].id), "{ext}: indices become ids");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bus's fader (at zero) applies to what's routed to it, through edits and undo.
    #[test]
    fn audio_thread_follows_routing_edits_and_undo() {
        let dir = std::env::temp_dir().join(format!("opentake-routing-rt-{}", std::process::id()));
        let wav = super::super::tests::tone_file(&dir, 0.5);
        let mut e = Engine::new();
        let a = e.add_track("A").unwrap();
        e.add_clip(a, &wav, 0.0).unwrap();
        let mut mixer = e.attach_test_mixer(48_000);
        let bus = e.add_bus("Bus").unwrap();
        e.update_track(bus, TrackUpdate { volume: Some(0.0), ..Default::default() }).unwrap();
        e.play().unwrap();
        let mut out = vec![0.0f32; 2 * 4800];
        let mut loud = |m: &mut crate::mixer::Mixer| {
            m.process(&[], 0, &mut out, 2);
            out.iter().fold(0.0f32, |a, v| a.max(v.abs()))
        };
        assert!(loud(&mut mixer) > 0.1, "to the master");
        e.set_track_output(a, Some(bus)).unwrap();
        assert_eq!(loud(&mut mixer), 0.0, "through the muted-by-fader bus");
        e.undo().unwrap();
        assert!(loud(&mut mixer) > 0.1, "undo resyncs the routing");
        e.redo().unwrap();
        assert_eq!(loud(&mut mixer), 0.0);
        e.remove_track(bus).unwrap();
        assert!(loud(&mut mixer) > 0.1, "deleting the bus sends its sources to its output");
        std::fs::remove_dir_all(&dir).ok();
    }
}
