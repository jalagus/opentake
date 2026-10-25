//! Operations on several regions (audio and MIDI) at once, each one undo step.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use opentake_project::{Clip, MidiClip, Note};
use serde::{Deserialize, Serialize};

use super::Engine;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RegionKind {
    Audio,
    Midi,
}

/// A region: (track, kind, index within that track's audio or MIDI list).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RegionRef {
    pub track: usize,
    pub kind: RegionKind,
    pub index: usize,
}

impl RegionRef {
    pub fn audio(track: usize, index: usize) -> Self {
        Self { track, kind: RegionKind::Audio, index }
    }

    pub fn midi(track: usize, index: usize) -> Self {
        Self { track, kind: RegionKind::Midi, index }
    }
}

/// A region's content, detached from any track (for copy/paste).
#[derive(Clone, Debug, PartialEq)]
pub enum RegionData {
    Audio(Clip),
    Midi(MidiClip),
}

impl RegionData {
    pub fn position(&self) -> f64 {
        match self {
            Self::Audio(c) => c.position_secs,
            Self::Midi(c) => c.position_secs,
        }
    }

    pub fn length(&self) -> f64 {
        match self {
            Self::Audio(c) => c.length_secs,
            Self::Midi(c) => c.length_secs,
        }
    }

    fn with_position(self, position: f64) -> Self {
        let position = position.max(0.0);
        match self {
            Self::Audio(c) => Self::Audio(Clip { position_secs: position, ..c }),
            Self::Midi(c) => Self::Midi(MidiClip { position_secs: position, ..c }),
        }
    }
}

impl Engine {
    pub fn region_data(&self, r: RegionRef) -> Result<RegionData> {
        let Some(track) = self.project.tracks.get(r.track) else { bail!("no track at index {}", r.track) };
        let data = match r.kind {
            RegionKind::Audio => track.clips.get(r.index).cloned().map(RegionData::Audio),
            RegionKind::Midi => track.midi_clips.get(r.index).cloned().map(RegionData::Midi),
        };
        data.ok_or_else(|| anyhow::anyhow!("track {} has no {:?} region at index {}", r.track, r.kind, r.index))
    }

    fn resync_tracks(&mut self, tracks: BTreeSet<usize>) -> Result<()> {
        for t in tracks {
            self.resync_clips(t)?;
            self.resync_midi(t)?;
        }
        Ok(())
    }

    /// Remove regions (in an order that keeps the remaining indices valid).
    fn remove_regions_impl(&mut self, regions: &[RegionRef]) {
        let mut sorted: Vec<RegionRef> = regions.to_vec();
        sorted.sort();
        sorted.dedup();
        for r in sorted.into_iter().rev() {
            let track = &mut self.project.tracks[r.track];
            match r.kind {
                RegionKind::Audio => drop(track.clips.remove(r.index)),
                RegionKind::Midi => drop(track.midi_clips.remove(r.index)),
            }
        }
    }

    fn insert_region_impl(&mut self, track: usize, data: RegionData) -> Result<RegionRef> {
        let t = &mut self.project.tracks[track];
        Ok(match data {
            RegionData::Audio(c) => {
                t.clips.push(c);
                RegionRef::audio(track, t.clips.len() - 1)
            }
            RegionData::Midi(c) => {
                t.midi_clips.push(c);
                RegionRef::midi(track, t.midi_clips.len() - 1)
            }
        })
    }

    /// Move (or with `copy`, duplicate) regions by a time offset and a track offset.
    /// Returns where the moved/copied regions ended up.
    pub fn move_regions(&mut self, regions: &[RegionRef], delta_secs: f64, delta_track: isize, copy: bool) -> Result<Vec<RegionRef>> {
        if regions.is_empty() {
            return Ok(Vec::new());
        }
        let n = self.project.tracks.len() as isize;
        let mut moves = Vec::new();
        for &r in regions {
            let target = r.track as isize + delta_track;
            if target < 0 || target >= n {
                bail!("can't move a region to track {target}");
            }
            let data = self.region_data(r)?;
            let pos = data.position() + delta_secs;
            moves.push((target as usize, data.with_position(pos)));
        }
        // Validate audio media before changing anything.
        let sr = self.sample_rate();
        for (_, d) in &moves {
            if let RegionData::Audio(c) = d {
                self.build_clip(c, sr)?;
            }
        }
        let label = match (copy, regions.len()) {
            (true, 1) => "Copy Region",
            (true, _) => "Copy Regions",
            (false, 1) => "Move Region",
            (false, _) => "Move Regions",
        };
        self.record(label, None, |e| {
            let mut touched: BTreeSet<usize> = moves.iter().map(|(t, _)| *t).collect();
            if !copy {
                touched.extend(regions.iter().map(|r| r.track));
                e.remove_regions_impl(regions);
            }
            let mut out = Vec::new();
            for (track, data) in moves {
                out.push(e.insert_region_impl(track, data)?);
            }
            e.resync_tracks(touched)?;
            Ok(out)
        })
    }

    /// Delete several regions as one step.
    pub fn remove_regions(&mut self, regions: &[RegionRef]) -> Result<()> {
        for &r in regions {
            self.region_data(r)?;
        }
        let label = if regions.len() == 1 { "Delete Region" } else { "Delete Regions" };
        self.record(label, None, |e| {
            let touched = regions.iter().map(|r| r.track).collect();
            e.remove_regions_impl(regions);
            e.resync_tracks(touched)
        })
    }

    /// Paste detached regions: each `(track, data)` is placed as given. Returns the new refs.
    pub fn insert_regions(&mut self, items: Vec<(usize, RegionData)>) -> Result<Vec<RegionRef>> {
        let sr = self.sample_rate();
        for (track, d) in &items {
            self.track_index(*track)?;
            match d {
                RegionData::Audio(c) => drop(self.build_clip(c, sr)?),
                RegionData::Midi(c) => super::midi::validate_notes(&c.notes)?,
            }
        }
        let label = if items.len() == 1 { "Paste Region" } else { "Paste Regions" };
        self.record(label, None, |e| {
            let touched = items.iter().map(|(t, _)| *t).collect();
            let mut out = Vec::new();
            for (track, data) in items {
                let data = match data {
                    RegionData::Midi(c) => RegionData::Midi(MidiClip { notes: super::midi::sorted(c.notes), ..c }),
                    other => other,
                };
                out.push(e.insert_region_impl(track, data)?);
            }
            e.resync_tracks(touched)?;
            Ok(out)
        })
    }

    /// Split every given region that spans `at_secs`, as one step. Others are ignored.
    pub fn split_regions(&mut self, regions: &[RegionRef], at_secs: f64) -> Result<usize> {
        let mut targets: Vec<RegionRef> = regions
            .iter()
            .copied()
            .filter(|&r| {
                self.region_data(r)
                    .is_ok_and(|d| at_secs > d.position() + Self::MIN_CLIP_SECS && at_secs < d.position() + d.length() - Self::MIN_CLIP_SECS)
            })
            .collect();
        if targets.is_empty() {
            bail!("no region under {at_secs:.3}s to split");
        }
        targets.sort();
        targets.dedup();
        let count = targets.len();
        let label = if count == 1 { "Split Region" } else { "Split Regions" };
        self.record(label, None, |e| {
            // Highest index first so inserting the right halves doesn't shift the others.
            for r in targets.iter().rev() {
                let data = e.region_data(*r)?;
                let cut = at_secs - data.position();
                let t = &mut e.project.tracks[r.track];
                match data {
                    RegionData::Audio(c) => {
                        let (left, right) = c.split(cut);
                        t.clips[r.index] = left;
                        t.clips.insert(r.index + 1, right);
                    }
                    RegionData::Midi(c) => {
                        let left_notes = c
                            .notes
                            .iter()
                            .filter(|n| n.start_secs < cut)
                            .map(|n| Note { length_secs: n.length_secs.min(cut - n.start_secs), ..*n })
                            .collect();
                        let right_notes = c.notes.iter().filter(|n| n.start_secs >= cut).map(|n| Note { start_secs: n.start_secs - cut, ..*n }).collect();
                        let mut left_tab = c.tab.clone();
                        let right_tab = left_tab.as_mut().map(|tab| tab.split_off(cut));
                        let mut left = MidiClip { length_secs: cut, notes: left_notes, tab: left_tab, ..c.clone() };
                        let right_tempos = left.split_tempos(cut);
                        t.midi_clips[r.index] = left;
                        t.midi_clips.insert(r.index + 1, MidiClip { position_secs: at_secs, length_secs: c.length_secs - cut, notes: right_notes, tab: right_tab, tempos: right_tempos, ..c });
                    }
                }
            }
            e.resync_tracks(targets.iter().map(|r| r.track).collect())?;
            Ok(count)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::tone_file;
    use super::*;

    fn setup() -> (Engine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("opentake-multi-{}-{:?}", std::process::id(), std::thread::current().id()));
        let wav = tone_file(&dir, 2.0);
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        e.add_track("B").unwrap();
        e.add_instrument_track("K", "synth").unwrap();
        e.add_clip(0, &wav, 0.0).unwrap();
        e.add_clip(0, &wav, 4.0).unwrap();
        e.add_clip(1, &wav, 1.0).unwrap();
        e.add_midi_clip(2, 0.0, 2.0, None).unwrap();
        (e, dir)
    }

    #[test]
    fn move_and_copy_groups_as_one_step() {
        let (mut e, dir) = setup();
        let group = [RegionRef::audio(0, 0), RegionRef::audio(1, 0)];
        let moved = e.move_regions(&group, 1.0, 1, false).unwrap();
        assert_eq!(moved, vec![RegionRef::audio(1, 0), RegionRef::audio(2, 0)]);
        assert_eq!(e.project().tracks[0].clips.len(), 1, "one left on A");
        assert_eq!(e.project().tracks[1].clips[0].position_secs, 1.0);
        assert_eq!(e.project().tracks[2].clips[0].position_secs, 2.0);
        assert_eq!(e.history_info().undo.as_deref(), Some("Move Regions"));
        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].clips.len(), 2);

        let copies = e.move_regions(&[RegionRef::audio(0, 1), RegionRef::midi(2, 0)], 8.0, 0, true).unwrap();
        assert_eq!(copies, vec![RegionRef::audio(0, 2), RegionRef::midi(2, 1)]);
        assert_eq!(e.project().tracks[0].clips.len(), 3, "originals stay");
        assert!(e.move_regions(&group, 0.0, 5, false).is_err(), "can't move past the last track");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn remove_split_and_paste_groups() {
        let (mut e, dir) = setup();
        e.remove_regions(&[RegionRef::audio(0, 0), RegionRef::audio(0, 1)]).unwrap();
        assert!(e.project().tracks[0].clips.is_empty());

        let n = e.split_regions(&[RegionRef::audio(1, 0), RegionRef::midi(2, 0)], 1.5).unwrap();
        assert_eq!(n, 2);
        assert_eq!(e.project().tracks[1].clips.len(), 2);
        assert_eq!(e.project().tracks[2].midi_clips.len(), 2);

        let data = e.region_data(RegionRef::audio(1, 0)).unwrap();
        let pasted = e.insert_regions(vec![(0, data.with_position(10.0))]).unwrap();
        assert_eq!(pasted, vec![RegionRef::audio(0, 0)]);
        assert_eq!(e.project().tracks[0].clips[0].position_secs, 10.0);
        std::fs::remove_dir_all(dir).ok();
    }
}
