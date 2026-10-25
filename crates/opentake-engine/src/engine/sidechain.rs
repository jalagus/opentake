//! Side chains: an effect slot's detector keyed by another track or bus (a kick ducking a
//! pad, a vocal ducking a guitar bus). Keys name their track by id like outputs do, and the
//! routing stays loop-free counting outputs, sends and keys; deleting the key track clears
//! its keys (undo brings them back).

use anyhow::{Result, anyhow, bail};
use opentake_project::SideChain;

use super::Engine;
use super::plugins::{Loaded, SlotRef};
use crate::mixer::{Command, KeyRt, MASTER};

impl Engine {
    /// Whether a slot can use a side chain: the built-in compressor, limiter, multiband
    /// compressor, gate and de-esser, and VST3 / AU effects with a side-chain input.
    pub fn supports_side_chain(&self, track: usize, which: SlotRef) -> bool {
        if which == SlotRef::Instrument {
            return false;
        }
        let Ok(slot) = self.slot_ref(track, which) else { return false };
        match self.pool.get(&slot.instance) {
            Some(Loaded::Compressor(_) | Loaded::Limiter(_) | Loaded::Multiband(_) | Loaded::Gate(_) | Loaded::DeEsser(_)) => true,
            Some(Loaded::Vst3(plugin)) => plugin.has_side_chain(),
            #[cfg(target_os = "macos")]
            Some(Loaded::Au(plugin)) => plugin.has_side_chain(),
            _ => false,
        }
    }

    /// The track keying a slot (index), and whether it's tapped pre-fader.
    pub fn side_chain(&self, track: usize, which: SlotRef) -> Option<(usize, bool)> {
        let (owner, k) = match which {
            SlotRef::Effect(k) => (Some(self.track_index(track).ok()?), k),
            SlotRef::Master(k) => (None, k),
            SlotRef::Instrument => return None,
        };
        let pre = self.slot_ref(track, which).ok()?.side_chain?.pre_fader;
        Some((self.project.key_source(owner, k)?, pre))
    }

    /// Key an effect slot (a track's or, with `SlotRef::Master`, the master's) from track
    /// `source` (index), tapped before or after its fader; `None` removes the key.
    pub fn set_side_chain(&mut self, track: usize, which: SlotRef, source: Option<usize>, pre_fader: bool) -> Result<()> {
        let owner = match which {
            SlotRef::Effect(_) => Some(self.track_index(track)?),
            SlotRef::Master(_) => None,
            SlotRef::Instrument => bail!("instruments take no side chain; key an effect slot"),
        };
        let slot = self.slot_ref(track, which)?;
        if let Some(s) = source {
            let p = &self.project;
            let name = |i: usize| p.tracks[i].name.clone();
            match owner {
                _ if s >= p.tracks.len() => bail!("no track at index {s}"),
                Some(i) if i == s => bail!("a plug-in can't be keyed by its own track"),
                _ if !self.supports_side_chain(track, which) => bail!("'{}' has no side-chain input", slot.name),
                Some(i) if !p.can_key(Some(i), s) => bail!("keying '{}' from '{}' would make a loop", name(i), name(s)),
                _ => {}
            }
        }
        let key = source.map(|s| SideChain { track: self.project.tracks[s].id, pre_fader });
        self.record(if key.is_some() { "Side Chain" } else { "Remove Side Chain" }, None, |e| {
            e.slot_mut(track, which)?.side_chain = key;
            e.sync_side_chains(owner)?;
            e.sync_routing()
        })
    }

    /// Before deleting track `index`: keys from it go.
    pub(super) fn drop_keys_from(&mut self, index: usize) -> Result<()> {
        let (tracks, master) = self.project.drop_keys_from(index);
        for i in tracks {
            self.sync_side_chains(Some(i))?;
        }
        if master {
            self.sync_side_chains(None)?;
        }
        Ok(())
    }

    /// Tell the audio thread every effect slot's key on track `index` (`None` = the master).
    pub(super) fn sync_side_chains(&mut self, index: Option<usize>) -> Result<()> {
        if self.audio.is_none() {
            return Ok(());
        }
        let (target, slots) = match index {
            Some(i) => (self.track_id(i), &self.project.tracks[i].plugins),
            None => (MASTER, &self.project.master_plugins),
        };
        let keys: Vec<(u64, Option<KeyRt>)> = slots.iter().filter(|s| s.instance != 0).map(|s| (s.instance, s.side_chain.map(|k| KeyRt::new(k.track, k.pre_fader)))).collect();
        for (instance, key) in keys {
            self.send(Command::SetSideChain { track: target, instance, key })?;
        }
        Ok(())
    }

    /// Name of the track keying a slot, for headers and listings.
    pub fn side_chain_name(&self, track: usize, which: SlotRef) -> Option<String> {
        self.side_chain(track, which).map(|(s, _)| self.project.tracks[s].name.clone())
    }

    /// The tracks a slot may be keyed by (indices): every other track or bus not mixed after it.
    pub fn side_chain_sources(&self, track: usize, which: SlotRef) -> Result<Vec<usize>> {
        let owner = match which {
            SlotRef::Effect(_) => Some(self.track_index(track)?),
            SlotRef::Master(_) => None,
            SlotRef::Instrument => return Err(anyhow!("instruments take no side chain")),
        };
        Ok((0..self.project.tracks.len()).filter(|&s| self.project.can_key(owner, s)).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::{Engine, TrackUpdate};
    use super::SlotRef;
    use crate::dynamics::{comp, norm_attack, norm_ratio, norm_release, norm_threshold};

    fn wav(dir: &Path, name: &str, samples: &[f32]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        samples.iter().for_each(|&s| w.write_sample(s).unwrap());
        w.finalize().unwrap();
        path
    }

    /// The pad's compressor keyed by the kick (fader down, keying pre-fader): the live mix and
    /// the offline render duck alike; undo, files and deleting the kick handle the key.
    #[test]
    fn keyed_compressor_live_offline_undo_and_files() {
        let dir = std::env::temp_dir().join(format!("opentake-sidechain-{}", std::process::id()));
        let mut kick = vec![0.0f32; 48_000];
        for h in [4_000, 28_000] {
            kick[h..h + 200].fill(0.8);
        }
        let mut e = Engine::new();
        let pad = e.add_track("Pad").unwrap();
        let kt = e.add_track("Kick").unwrap();
        e.add_clip(pad, &wav(&dir, "pad.wav", &[0.25; 48_000]), 0.0).unwrap();
        e.add_clip(kt, &wav(&dir, "kick.wav", &kick), 0.0).unwrap();
        e.update_track(kt, TrackUpdate { volume: Some(0.0), ..Default::default() }).unwrap();
        let eq = e.insert_plugin(pad, "eq", None).unwrap();
        let c = e.insert_plugin(pad, "comp", None).unwrap();
        let settings = [(comp::THRESHOLD, norm_threshold(-40.0)), (comp::RATIO, norm_ratio(20.0)), (comp::ATTACK, norm_attack(0.1)), (comp::RELEASE, norm_release(50.0))];
        e.set_slot_params(pad, SlotRef::Effect(c), &settings, "Ducking").unwrap();

        assert!(e.set_side_chain(pad, SlotRef::Effect(eq), Some(kt), false).is_err(), "the EQ has no side chain");
        assert!(e.set_side_chain(pad, SlotRef::Effect(c), Some(pad), false).is_err(), "not its own track");
        assert!(e.set_side_chain(kt, SlotRef::Instrument, Some(pad), false).is_err());
        e.set_side_chain(pad, SlotRef::Effect(c), Some(kt), true).unwrap();
        assert_eq!((e.side_chain(pad, SlotRef::Effect(c)), e.history_info().undo.as_deref()), (Some((kt, true)), Some("Side Chain")));
        let kc = e.insert_plugin(kt, "gate", None).unwrap();
        assert!(e.set_side_chain(kt, SlotRef::Effect(kc), Some(pad), false).is_err(), "pad keyed by kick keyed by pad");
        assert_eq!(e.side_chain_sources(kt, SlotRef::Effect(kc)).unwrap(), Vec::<usize>::new());
        assert_eq!(e.project().processing_order(), vec![kt, pad]);

        // Live (no plug-in delay: output sample i is timeline i) against the offline render.
        let mut mixer = e.attach_test_mixer(48_000);
        e.play().unwrap();
        let mut live = Vec::new();
        let mut out = vec![0.0f32; 2 * 512];
        for _ in 0..90 {
            mixer.process(&[], 0, &mut out, 2);
            live.extend(out.chunks(2).map(|f| f[0]));
        }
        let offline = e.mix_job_at(48_000.0).render(None, None, 0.0).master[0].clone();
        assert!(live[4_100] < 0.02 && offline[4_100] < 0.02 && (live[3_900] - 0.25).abs() < 1e-4, "both duck on the kick: {} {} {} {}", live[4_100], offline[4_100], live[3_900], offline[3_900]);
        let worst = live.iter().zip(&offline).take(48_000).fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(worst < 1e-4, "live and offline differ by {worst}");

        // Undo/redo, a saved project, and deleting the kick (undo restores the key).
        e.undo().unwrap(); // the gate
        e.undo().unwrap();
        assert_eq!(e.side_chain(pad, SlotRef::Effect(c)), None);
        e.redo().unwrap();
        assert_eq!(e.side_chain(pad, SlotRef::Effect(c)), Some((kt, true)));
        let path = e.save_project(Some(&dir.join("keyed.otp"))).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains(r#""side_chain": {"#));
        let mut loaded = Engine::new();
        loaded.load_project(&path).unwrap();
        assert_eq!(loaded.side_chain(pad, SlotRef::Effect(c)), Some((kt, true)));
        assert_eq!(loaded.side_chain_name(pad, SlotRef::Effect(c)).as_deref(), Some("Kick"));
        e.remove_track(kt).unwrap();
        assert!(e.project().tracks[pad].plugins[c].side_chain.is_none(), "the key went with its track");
        e.undo().unwrap();
        assert_eq!(e.side_chain(pad, SlotRef::Effect(c)), Some((kt, true)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
