//! MIDI regions and notes on instrument tracks.
//!
//! Regions live in quarter notes (see `MidiClip`): the timeline positions the API takes in
//! seconds are converted through the tempo map where they land, and the audio thread gets
//! samples computed from the map, rebuilt whenever it changes.

use std::borrow::Cow;

use anyhow::{Result, anyhow, bail};
use opentake_project::{Controller, ControllerEvent, ControllerLane, MidiClip, Note, Quantize};

use super::Engine;
use super::regions::{RegionKind, RegionRef};
use crate::mixer::{Command, CtrlRt, MidiClipRt, NoteRt};

/// Optional changes to a MIDI region; `None` leaves a field unchanged. Position and length
/// are timeline seconds (converted through the tempo map where the region lands) or quarter
/// notes; quarter notes win when both are given.
#[derive(Debug, Clone, Default)]
pub struct MidiClipUpdate {
    pub name: Option<String>,
    pub position_secs: Option<f64>,
    pub length_secs: Option<f64>,
    pub position_beats: Option<f64>,
    pub length_beats: Option<f64>,
    pub mute: Option<bool>,
}

/// Shortest note (quarter notes).
pub(super) const MIN_NOTE_BEATS: f64 = 0.001;
/// Shortest recorded note (seconds).
pub(super) const MIN_NOTE_SECS: f64 = 0.005;
/// Shortest region (quarter notes).
const MIN_REGION_BEATS: f64 = 0.01;

pub(super) fn validate_notes(notes: &[Note]) -> Result<()> {
    for n in notes {
        if n.pitch > 127 {
            bail!("note pitch {} is out of range (0..=127)", n.pitch);
        }
        if !(1..=127).contains(&n.velocity) {
            bail!("note velocity {} is out of range (1..=127)", n.velocity);
        }
        if !(n.length_beats >= MIN_NOTE_BEATS && n.start_beats >= 0.0 && n.start_beats.is_finite() && n.length_beats.is_finite()) {
            bail!("notes need start >= 0 and a length of at least {MIN_NOTE_BEATS} beats");
        }
    }
    Ok(())
}

pub(super) fn validate_controllers(events: &[ControllerEvent]) -> Result<()> {
    if events.iter().any(|e| !e.time_beats.is_finite() || e.time_beats < 0.0 || !e.value.is_finite()) {
        bail!("controller events need a time >= 0 and a value");
    }
    Ok(())
}

pub(super) fn sorted(mut notes: Vec<Note>) -> Vec<Note> {
    notes.sort_by(|a, b| a.start_beats.total_cmp(&b.start_beats).then(a.pitch.cmp(&b.pitch)));
    notes
}

impl Engine {
    /// Regions for the audio thread, in absolute samples (through the tempo map) with notes
    /// clipped to their region. A looped region plays one region per pass (so the feed only
    /// scans the pass playing).
    pub(super) fn build_midi(&self, index: usize, sr: u32) -> Vec<MidiClipRt> {
        let map = self.project.tempo_map();
        let s = |q: f64| (map.secs_at(q).max(0.0) * sr as f64).round() as u64;
        fn passes(c: &MidiClip) -> Vec<Cow<'_, MidiClip>> {
            match c.looped {
                None => vec![Cow::Borrowed(c)],
                Some(_) => c.loop_regions().into_iter().map(Cow::Owned).collect(),
            }
        }
        self.project.tracks[index]
            .midi_clips
            .iter()
            .flat_map(passes)
            .map(|c| {
                let (c, p): (&MidiClip, f64) = (&c, c.position_beats);
                let (start, end) = (s(p), s(c.end_beats()));
                let mut notes: Vec<NoteRt> = c
                    .notes
                    .iter()
                    .filter(|n| n.start_beats < c.length_beats)
                    .map(|n| NoteRt { start: s(p + n.start_beats), end: s(p + n.start_beats + n.length_beats).min(end), pitch: n.pitch, velocity: n.velocity })
                    .collect();
                notes.sort_by_key(|n| n.start);
                let mut ctrl: Vec<CtrlRt> = c
                    .controllers
                    .iter()
                    .flat_map(|l| l.events.iter().filter(|e| e.time_beats < c.length_beats).map(move |e| CtrlRt { at: s(p + e.time_beats), kind: l.kind, value: l.kind.raw(e.value) }))
                    .collect();
                ctrl.sort_by_key(|c| c.at);
                MidiClipRt { start, end, mute: c.mute, notes, ctrl }
            })
            .collect()
    }

    pub(super) fn resync_midi(&mut self, index: usize) -> Result<()> {
        if self.audio.is_some() {
            let rt = self.build_midi(index, self.sample_rate());
            self.send(Command::ReplaceMidi { track: self.track_id(index), clips: Box::new(rt) })?;
        }
        Ok(())
    }

    /// The tempo map changed: send it, and re-time every track's MIDI (which follows it).
    pub(super) fn sync_tempo(&mut self) -> Result<()> {
        let cmd = self.tempo_command();
        self.send(cmd)?;
        for i in 0..self.project.tracks.len() {
            if !self.project.tracks[i].midi_clips.is_empty() {
                self.resync_midi(i)?;
            }
        }
        self.sync_song_end()
    }

    fn midi_index(&self, track: usize, clip: usize) -> Result<usize> {
        let index = self.track_index(track)?;
        if clip >= self.project.tracks[index].midi_clips.len() {
            bail!("track {track} has no MIDI region at index {clip}");
        }
        Ok(index)
    }

    /// Quarter notes from the song start at timeline time `secs` (the tempo map's).
    pub fn beats_at(&self, secs: f64) -> f64 {
        self.project.tempo_map().quarters_at(secs)
    }

    /// New track with an instrument ("synth" for the built-in synth, or a VST3 instrument).
    pub fn add_instrument_track(&mut self, name: &str, instrument: &str) -> Result<usize> {
        let slot = self.new_slot(instrument, true)?;
        self.record("New Instrument Track", None, |e| {
            let index = e.add_track_impl(name)?;
            e.project.tracks[index].instrument = Some(slot);
            e.sync_instrument(index)?;
            Ok(index)
        })
    }

    /// Add an empty MIDI region spanning `position_secs` for `length_secs` (timeline seconds,
    /// stored as quarter notes). Returns its index.
    pub fn add_midi_clip(&mut self, track: usize, position_secs: f64, length_secs: f64, name: Option<&str>) -> Result<usize> {
        let map = self.project.tempo_map();
        let start = map.quarters_at(position_secs.max(0.0));
        self.add_midi_clip_beats(track, start, map.quarters_at(position_secs.max(0.0) + length_secs) - start, name)
    }

    /// Add an empty MIDI region at `position_beats` quarter notes, `length_beats` long.
    pub fn add_midi_clip_beats(&mut self, track: usize, position_beats: f64, length_beats: f64, name: Option<&str>) -> Result<usize> {
        let index = self.track_index(track)?;
        if length_beats.is_nan() || length_beats < MIN_REGION_BEATS || !position_beats.is_finite() {
            bail!("region length must be positive");
        }
        let name = name.map(str::to_string).unwrap_or_else(|| self.project.tracks[index].name.clone());
        let clip = MidiClip::new(name, position_beats.max(0.0), length_beats);
        self.record("Create MIDI Region", None, |e| e.insert_midi_impl(index, clip))
    }

    /// Place an existing MIDI region (e.g. pasted). Returns its index.
    pub fn insert_midi_clip(&mut self, track: usize, clip: MidiClip) -> Result<usize> {
        let index = self.track_index(track)?;
        validate_notes(&clip.notes)?;
        self.record("Paste Region", None, |e| e.insert_midi_impl(index, clip))
    }

    fn insert_midi_impl(&mut self, index: usize, clip: MidiClip) -> Result<usize> {
        let clip = MidiClip { position_beats: clip.position_beats.max(0.0), notes: sorted(clip.notes), ..clip };
        self.project.tracks[index].midi_clips.push(clip);
        self.resync_midi(index)?;
        Ok(self.project.tracks[index].midi_clips.len() - 1)
    }

    pub fn update_midi_clip(&mut self, track: usize, clip: usize, u: MidiClipUpdate) -> Result<MidiClip> {
        let index = self.midi_index(track, clip)?;
        let map = self.project.tempo_map();
        let c = &self.project.tracks[index].midi_clips[clip];
        let position = u.position_beats.or(u.position_secs.map(|s| map.quarters_at(s.max(0.0)))).map(|p| p.max(0.0));
        let start = position.unwrap_or(c.position_beats);
        // A length in seconds runs from where the region (then) starts.
        let length = u.length_beats.or(u.length_secs.map(|l| map.quarters_at(map.secs_at(start) + l) - start));
        if length.is_some_and(|l| l.is_nan() || l < MIN_REGION_BEATS) {
            bail!("region length must be positive");
        }
        let label = match (&position, &length) {
            (Some(_), None) => "Move Region",
            (None, Some(_)) | (Some(_), Some(_)) => "Resize Region",
            _ => "Region Settings",
        };
        self.record(label, None, |e| {
            let c = &mut e.project.tracks[index].midi_clips[clip];
            if let Some(v) = u.name {
                c.name = v;
            }
            if let Some(v) = position {
                c.position_beats = v;
            }
            if let Some(v) = length {
                c.length_beats = v;
            }
            if let Some(v) = u.mute {
                c.mute = v;
            }
            let c = c.clone();
            e.resync_midi(index)?;
            Ok(c)
        })
    }

    /// Replace all notes of a region (one undo step).
    pub fn set_midi_notes(&mut self, track: usize, clip: usize, notes: Vec<Note>) -> Result<()> {
        let index = self.midi_index(track, clip)?;
        validate_notes(&notes)?;
        self.record("Edit Notes", None, |e| {
            e.project.tracks[index].midi_clips[clip].notes = sorted(notes);
            e.resync_midi(index)
        })
    }

    /// Append notes to a region. Returns the new note count.
    pub fn add_midi_notes(&mut self, track: usize, clip: usize, notes: Vec<Note>) -> Result<usize> {
        let index = self.midi_index(track, clip)?;
        validate_notes(&notes)?;
        let mut all = self.project.tracks[index].midi_clips[clip].notes.clone();
        all.extend(notes);
        let count = all.len();
        self.record("Add Notes", None, |e| {
            e.project.tracks[index].midi_clips[clip].notes = sorted(all);
            e.resync_midi(index)?;
            Ok(count)
        })
    }

    /// Quantize a region's notes (those at `notes` indices, or all), one undo step. Returns
    /// how many moved.
    pub fn quantize_midi(&mut self, track: usize, clip: usize, q: &Quantize, notes: Option<&[usize]>) -> Result<usize> {
        let index = self.midi_index(track, clip)?;
        let c = &self.project.tracks[index].midi_clips[clip];
        if let Some(&bad) = notes.and_then(|n| n.iter().find(|&&i| i >= c.notes.len())) {
            bail!("the region has no note {bad} (it has {})", c.notes.len());
        }
        let mut quantized = c.notes.clone();
        let moved = q.apply(&mut quantized, c.position_beats, &self.project.tempo_map(), |i| notes.is_none_or(|n| n.contains(&i)));
        if moved > 0 {
            self.record("Quantize", None, |e| {
                e.project.tracks[index].midi_clips[clip].notes = sorted(quantized);
                e.resync_midi(index)
            })?;
        }
        Ok(moved)
    }

    /// Quantize every note of the given MIDI regions (audio regions are skipped), one undo
    /// step. Returns how many notes moved.
    pub fn quantize_regions(&mut self, regions: &[RegionRef], q: &Quantize) -> Result<usize> {
        let map = self.project.tempo_map();
        let mut changed = Vec::new();
        for r in regions.iter().filter(|r| r.kind == RegionKind::Midi) {
            let index = self.midi_index(r.track, r.index)?;
            let c = &self.project.tracks[index].midi_clips[r.index];
            let mut notes = c.notes.clone();
            if q.apply(&mut notes, c.position_beats, &map, |_| true) > 0 {
                let moved = notes.iter().zip(&c.notes).filter(|(a, b)| a != b).count();
                changed.push((index, r.index, sorted(notes), moved));
            }
        }
        let moved = changed.iter().map(|c| c.3).sum();
        if moved > 0 {
            self.record("Quantize", None, |e| {
                for (index, clip, notes, _) in changed.iter().cloned() {
                    e.project.tracks[index].midi_clips[clip].notes = notes;
                }
                e.resync_tracks(changed.iter().map(|c| c.0).collect())
            })?;
        }
        Ok(moved)
    }

    /// "Quantize on record": recorded notes snap to this (`None` = off). A setting, not an
    /// undoable edit.
    pub fn set_record_quantize(&mut self, q: Option<Quantize>) {
        self.project.record_quantize = q;
    }

    /// Set one controller's events in a region (times in quarter notes from the region start,
    /// values normalized: pitch bend -1..1, else 0..1): replace the lane, or add to it (an
    /// event at the time of an existing one replaces it). Empty and replacing removes the
    /// lane. One undo step; returns the lane's event count.
    pub fn set_midi_controllers(&mut self, track: usize, clip: usize, kind: Controller, events: Vec<ControllerEvent>, replace: bool) -> Result<usize> {
        let index = self.midi_index(track, clip)?;
        validate_controllers(&events)?;
        let c = &self.project.tracks[index].midi_clips[clip];
        let mut all = if replace { Vec::new() } else { c.lane(kind).map(|l| l.events.clone()).unwrap_or_default() };
        // Existing events first, so new ones at the same time win.
        all.extend(events);
        let lane = ControllerLane::new(kind, all);
        let count = lane.events.len();
        let label = if kind == Controller::PitchBend { "Edit Pitch Bend" } else { "Edit Controllers" };
        self.record(label, None, |e| {
            e.project.tracks[index].midi_clips[clip].set_lane(lane);
            e.resync_midi(index)?;
            Ok(count)
        })
    }

    pub fn remove_midi_clip(&mut self, track: usize, clip: usize) -> Result<MidiClip> {
        let index = self.midi_index(track, clip)?;
        self.record("Delete Region", None, |e| {
            let removed = e.project.tracks[index].midi_clips.remove(clip);
            e.resync_midi(index)?;
            Ok(removed)
        })
    }

    /// Move a MIDI region to start at timeline time `position_secs` (stored as quarter notes).
    pub fn move_midi_clip(&mut self, track: usize, clip: usize, position_secs: f64, to_track: Option<usize>) -> Result<MidiClip> {
        let beats = self.beats_at(position_secs.max(0.0));
        self.move_midi_clip_beats(track, clip, beats, to_track)
    }

    pub fn move_midi_clip_beats(&mut self, track: usize, clip: usize, position_beats: f64, to_track: Option<usize>) -> Result<MidiClip> {
        let from = self.midi_index(track, clip)?;
        let to = self.track_index(to_track.unwrap_or(track))?;
        self.record("Move Region", None, |e| {
            let mut c = e.project.tracks[from].midi_clips.remove(clip);
            c.position_beats = position_beats.max(0.0);
            e.project.tracks[to].midi_clips.push(c.clone());
            e.resync_midi(from)?;
            if to != from {
                e.resync_midi(to)?;
            }
            Ok(c)
        })
    }

    /// Copy a MIDI region to timeline time `position_secs`; returns the copy's (track, index).
    pub fn copy_midi_clip(&mut self, track: usize, clip: usize, position_secs: f64, to_track: Option<usize>) -> Result<(usize, usize)> {
        let beats = self.beats_at(position_secs.max(0.0));
        self.copy_midi_clip_beats(track, clip, beats, to_track)
    }

    pub fn copy_midi_clip_beats(&mut self, track: usize, clip: usize, position_beats: f64, to_track: Option<usize>) -> Result<(usize, usize)> {
        let from = self.midi_index(track, clip)?;
        let to = self.track_index(to_track.unwrap_or(track))?;
        let copy = MidiClip { position_beats, ..self.project.tracks[from].midi_clips[clip].clone() };
        self.record("Copy Region", None, |e| Ok((to, e.insert_midi_impl(to, copy)?)))
    }

    /// Split a MIDI region at a timeline position. Notes that cross the split are
    /// shortened on the left; notes starting after it move to the right part, and the right
    /// part's controllers start at the values held at the split.
    pub fn split_midi_clip(&mut self, track: usize, clip: usize, at_secs: f64) -> Result<()> {
        let index = self.midi_index(track, clip)?;
        let c = self.project.tracks[index].midi_clips[clip].clone();
        let cut = c.beats_at(&self.project.tempo_map(), at_secs);
        if cut <= MIN_REGION_BEATS || cut >= c.length_beats - MIN_REGION_BEATS {
            bail!("split point is outside the region");
        }
        let (left, right) = c.split(cut);
        self.record("Split Region", None, |e| {
            let clips = &mut e.project.tracks[index].midi_clips;
            clips[clip] = left;
            clips.insert(clip + 1, right);
            e.resync_midi(index)
        })
    }

    /// Play a note on a track's instrument right now (e.g. clicking the piano-roll
    /// keyboard). Velocity 0 releases it. Not part of the project.
    pub fn live_note(&mut self, track: usize, pitch: u8, velocity: u8) -> Result<()> {
        let index = self.track_index(track)?;
        if self.project.tracks[index].instrument.is_none() {
            return Err(anyhow!("track {track} has no instrument"));
        }
        self.send(Command::LiveNote { track: self.track_id(index), pitch: pitch.min(127), velocity: velocity.min(127) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentake_project::{QuantizeGrid, TempoChange};

    fn note(start: f64, len: f64, pitch: u8) -> Note {
        Note { start_beats: start, length_beats: len, pitch, velocity: 100, tab: None }
    }

    #[test]
    fn midi_regions_notes_and_undo() {
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        // 1 s for 2 s at 120 bpm: beats 2..6.
        let c = e.add_midi_clip(t, 1.0, 2.0, None).unwrap();
        assert_eq!((e.project().tracks[t].midi_clips[c].position_beats, e.project().tracks[t].midi_clips[c].length_beats), (2.0, 4.0));
        e.set_midi_notes(t, c, vec![note(1.0, 0.5, 64), note(0.0, 1.0, 60)]).unwrap();
        let notes = &e.project().tracks[t].midi_clips[c].notes;
        assert_eq!(notes[0].pitch, 60, "notes are kept sorted by start");
        assert!(e.set_midi_notes(t, c, vec![Note { velocity: 0, ..note(0.0, 0.1, 60) }]).is_err());
        assert!(e.set_midi_notes(t, c, vec![note(0.0, 0.0, 60)]).is_err(), "no zero-length notes");

        // Notes are placed in absolute samples and clipped to the region end.
        e.add_midi_notes(t, c, vec![note(3.6, 2.0, 67)]).unwrap();
        let rt = e.build_midi(t, 48_000);
        assert_eq!(rt[0].start, 48_000);
        assert_eq!(rt[0].notes[0].start, 48_000);
        assert_eq!(rt[0].notes[1].start, 72_000, "a beat into the region: 1.5 s");
        assert_eq!(rt[0].notes.last().unwrap().end, rt[0].end, "note clipped to region end");

        e.undo().unwrap(); // add notes
        e.undo().unwrap(); // set notes
        assert!(e.project().tracks[t].midi_clips[c].notes.is_empty());
        e.redo().unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[c].notes.len(), 2);
    }

    #[test]
    fn split_and_copy_midi_regions() {
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        let c = e.add_midi_clip_beats(t, 0.0, 8.0, Some("Riff")).unwrap();
        e.set_midi_notes(t, c, vec![note(0.0, 2.0, 60), note(3.0, 2.0, 62), note(6.0, 1.0, 64)]).unwrap();
        // 2 s = beat 4.
        e.split_midi_clip(t, c, 2.0).unwrap();
        let clips = &e.project().tracks[t].midi_clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].notes.len(), 2);
        assert!((clips[0].notes[1].length_beats - 1.0).abs() < 1e-9, "crossing note shortened");
        assert_eq!(clips[1].notes.len(), 1);
        assert!((clips[1].notes[0].start_beats - 2.0).abs() < 1e-9);
        assert_eq!((clips[1].position_beats, clips[1].length_beats), (4.0, 4.0));

        let (_, i) = e.copy_midi_clip(t, 1, 8.0, None).unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[i].position_beats, 16.0);
        e.move_midi_clip_beats(t, i, 12.0, None).unwrap();
        assert_eq!(e.project().tracks[t].midi_clips.last().unwrap().position_beats, 12.0);
    }

    #[test]
    fn controller_lanes_edit_split_and_play_in_samples() {
        let ev = |t: f64, v: f32| ControllerEvent { time_beats: t, value: v };
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        let c = e.add_midi_clip_beats(t, 2.0, 8.0, None).unwrap();
        assert_eq!(e.set_midi_controllers(t, c, Controller::PitchBend, vec![ev(0.0, 0.0), ev(2.0, 1.0), ev(6.0, -0.5)], true).unwrap(), 3);
        assert_eq!(e.set_midi_controllers(t, c, Controller::PitchBend, vec![ev(2.0, 0.5), ev(4.0, 0.25)], false).unwrap(), 4, "same time replaces");
        e.set_midi_controllers(t, c, Controller::MOD_WHEEL, vec![ev(1.0, 1.0), ev(10.0, 0.0)], true).unwrap();
        assert!(e.set_midi_controllers(t, c, Controller::MOD_WHEEL, vec![ev(-1.0, 0.0)], true).is_err());
        assert_eq!(e.history_info().undo.as_deref(), Some("Edit Controllers"));

        // Absolute samples, sorted, raw values; the event past the region end is left out.
        let rt = e.build_midi(t, 1000);
        let ctrl: Vec<(u64, Controller, u16)> = rt[0].ctrl.iter().map(|c| (c.at, c.kind, c.value)).collect();
        assert_eq!(ctrl, vec![(1000, Controller::PitchBend, 8192), (1500, Controller::MOD_WHEEL, 127), (2000, Controller::PitchBend, 12288), (3000, Controller::PitchBend, 10240), (4000, Controller::PitchBend, 4096)]);

        e.split_midi_clip(t, c, 3.5).unwrap();
        let right = &e.project().tracks[t].midi_clips[1];
        assert_eq!(right.lane(Controller::PitchBend).unwrap().events, vec![ev(0.0, 0.25), ev(1.0, -0.5)]);
        e.undo().unwrap();
        e.set_midi_controllers(t, c, Controller::MOD_WHEEL, Vec::new(), true).unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[c].controllers.len(), 1, "an emptied lane goes");
        e.undo().unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[c].controllers.len(), 2);
    }

    /// Note-on sample offsets the audio thread holds for track 0.
    fn mixer_note_starts(m: &mut crate::mixer::Mixer) -> Vec<u64> {
        let mut out = vec![0.0f32; 2 * 64];
        m.process(&[], 0, &mut out, 2); // apply what was sent
        m.debug_midi().into_iter().next().map(|(_, notes)| notes.into_iter().map(|n| n.0).collect()).unwrap_or_default()
    }

    #[test]
    fn tempo_changes_retime_midi_with_the_bars_and_undo_restores_it() {
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        let mut m = e.attach_test_mixer(1000);
        // A note on bar 3 (beat 8): 4 s at 120 bpm.
        let c = e.add_midi_clip_beats(t, 4.0, 8.0, None).unwrap();
        e.set_midi_notes(t, c, vec![note(4.0, 1.0, 60)]).unwrap();
        assert_eq!(mixer_note_starts(&mut m), [4000]);
        let bar3 = |e: &Engine| e.project().tempo_map().secs_of(3, 1.0);

        // Half the tempo: still on bar 3, which is now at 8 s, in the model and on the audio thread.
        e.set_tempo(60.0, None).unwrap();
        let clip = &e.project().tracks[t].midi_clips[c];
        assert_eq!((clip.position_beats + clip.notes[0].start_beats, clip.secs_of(&e.project().tempo_map(), clip.notes[0].start_beats)), (8.0, bar3(&e)));
        assert_eq!(bar3(&e), 8.0);
        assert_eq!(mixer_note_starts(&mut m), [8000]);
        assert_eq!(e.build_midi(t, 1000)[0].notes[0].end, 9000, "a beat long: now a second");

        // A tempo change in the middle of the region re-times what follows it.
        e.set_tempo_changes(vec![TempoChange { at_secs: 6.0, bpm: 120.0, time_signature: None }], None).unwrap();
        assert_eq!(mixer_note_starts(&mut m), [7000], "beat 6 at 6 s, then two beats in a second");
        assert_eq!(e.project().length_secs(), 9.0, "the region ends at beat 12: 6 s + 3 s");

        // Undo restores the timing on the audio thread too.
        e.undo().unwrap();
        assert_eq!(mixer_note_starts(&mut m), [8000]);
        e.undo().unwrap();
        assert_eq!(mixer_note_starts(&mut m), [4000]);
        e.redo().unwrap();
        assert_eq!(mixer_note_starts(&mut m), [8000]);
    }

    #[test]
    fn regions_placed_in_seconds_land_on_the_tempo_map() {
        let mut e = Engine::new();
        e.set_tempo_changes(vec![TempoChange { at_secs: 4.0, bpm: 60.0, time_signature: None }], None).unwrap();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        // 3 s (beat 6) to 6 s (beat 8 + 2).
        let c = e.add_midi_clip(t, 3.0, 3.0, None).unwrap();
        let clip = |e: &Engine| e.project().tracks[t].midi_clips[c].clone();
        assert_eq!((clip(&e).position_beats, clip(&e).length_beats), (6.0, 4.0));
        e.update_midi_clip(t, c, MidiClipUpdate { position_secs: Some(5.0), length_secs: Some(2.0), ..Default::default() }).unwrap();
        assert_eq!((clip(&e).position_beats, clip(&e).length_beats), (9.0, 2.0));
        e.update_midi_clip(t, c, MidiClipUpdate { length_beats: Some(3.0), length_secs: Some(9.0), ..Default::default() }).unwrap();
        assert_eq!(clip(&e).length_beats, 3.0, "beats win");
        assert!(e.update_midi_clip(t, c, MidiClipUpdate { length_beats: Some(0.0), ..Default::default() }).is_err());
        e.move_midi_clip(t, c, 1.0, None).unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[0].position_beats, 2.0);
    }

    #[test]
    fn loading_a_seconds_based_project_converts_its_midi() {
        let dir = std::env::temp_dir().join(format!("opentake-midi-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Version 1: 120 bpm then 60 from 2 s; a region at 1 s with a note at 1.5..2.5 s.
        let old = r#"{"name":"Old","tempo_bpm":120.0,"time_signature":[4,4],"sample_rate":48000,
            "tempo_changes":[{"at_secs":2.0,"bpm":60.0}],
            "tracks":[{"name":"Keys","volume":1.0,"pan":0.0,"mute":false,"solo":false,"armed":false,"input_channel":0,"monitor_input":false,"clips":[],"plugins":[],
            "midi_clips":[{"name":"R","position_secs":1.0,"length_secs":2.0,"notes":[{"start_secs":0.5,"length_secs":1.0,"pitch":60,"velocity":100}]}]}]}"#;
        std::fs::write(dir.join("old.otp"), old).unwrap();
        let mut e = Engine::new();
        e.load_project(&dir.join("old.otp")).unwrap();
        let c = &e.project().tracks[0].midi_clips[0];
        assert_eq!((c.position_beats, c.length_beats, c.notes[0].start_beats, c.notes[0].length_beats), (2.0, 3.0, 1.0, 1.5));
        assert_eq!(e.build_midi(0, 1000)[0].notes.iter().map(|n| (n.start, n.end)).collect::<Vec<_>>(), [(1500, 2500)], "plays where it did");
        assert!(e.history_info().undo.is_none(), "part of loading, not an edit");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn quantize_moves_notes_as_one_step() {
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        let c = e.add_midi_clip_beats(t, 0.0, 4.0, None).unwrap();
        e.set_midi_notes(t, c, vec![note(0.1, 0.5, 60), note(1.2, 0.5, 62), note(2.0, 0.5, 64)]).unwrap();
        let q = Quantize::new(QuantizeGrid::Quarter);
        assert_eq!(e.quantize_midi(t, c, &q, Some(&[1])).unwrap(), 1, "only the chosen note");
        let starts = |e: &Engine| e.project().tracks[t].midi_clips[c].notes.iter().map(|n| (n.start_beats * 1e9).round() / 1e9).collect::<Vec<_>>();
        assert_eq!(starts(&e), [0.1, 1.0, 2.0]);
        assert_eq!(e.history_info().undo.as_deref(), Some("Quantize"));
        assert!(e.quantize_midi(t, c, &q, Some(&[7])).is_err());
        // Every note of the selected regions; ones already on the grid don't count.
        let c2 = e.add_midi_clip_beats(t, 8.0, 4.0, None).unwrap();
        e.set_midi_notes(t, c2, vec![note(0.4, 0.5, 60)]).unwrap();
        let half = Quantize { strength: 0.5, ..q };
        assert_eq!(e.quantize_regions(&[RegionRef::midi(t, c), RegionRef::midi(t, c2), RegionRef::audio(t, 0)], &half).unwrap(), 2);
        assert_eq!(starts(&e), [0.05, 1.0, 2.0]);
        assert!((e.project().tracks[t].midi_clips[c2].notes[0].start_beats - 0.2).abs() < 1e-9);
        e.undo().unwrap();
        assert_eq!(starts(&e), [0.1, 1.0, 2.0], "one undo step for both regions");
        assert_eq!(e.quantize_regions(&[RegionRef::midi(t, c2)], &Quantize::new(QuantizeGrid::Sixteenth)).unwrap(), 1);
        assert_eq!(e.quantize_regions(&[RegionRef::midi(t, c2)], &Quantize::new(QuantizeGrid::Sixteenth)).unwrap(), 0, "nothing left to move");
    }
}
