//! MIDI regions and notes on instrument tracks.

use std::borrow::Cow;

use anyhow::{Result, anyhow, bail};
use opentake_project::{Controller, ControllerEvent, ControllerLane, MidiClip, Note};

use super::Engine;
use crate::mixer::{Command, CtrlRt, MidiClipRt, NoteRt};

/// Optional changes to a MIDI region; `None` leaves a field unchanged.
#[derive(Debug, Clone, Default)]
pub struct MidiClipUpdate {
    pub name: Option<String>,
    pub position_secs: Option<f64>,
    pub length_secs: Option<f64>,
    pub mute: Option<bool>,
}

pub(super) const MIN_NOTE_SECS: f64 = 0.005;

pub(super) fn validate_notes(notes: &[Note]) -> Result<()> {
    for n in notes {
        if n.pitch > 127 {
            bail!("note pitch {} is out of range (0..=127)", n.pitch);
        }
        if !(1..=127).contains(&n.velocity) {
            bail!("note velocity {} is out of range (1..=127)", n.velocity);
        }
        if n.length_secs < MIN_NOTE_SECS || n.start_secs < 0.0 {
            bail!("notes need start >= 0 and a length of at least {} ms", MIN_NOTE_SECS * 1000.0);
        }
    }
    Ok(())
}

pub(super) fn validate_controllers(events: &[ControllerEvent]) -> Result<()> {
    if events.iter().any(|e| !e.time_secs.is_finite() || e.time_secs < 0.0 || !e.value.is_finite()) {
        bail!("controller events need a time >= 0 and a value");
    }
    Ok(())
}

pub(super) fn sorted(mut notes: Vec<Note>) -> Vec<Note> {
    notes.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs).then(a.pitch.cmp(&b.pitch)));
    notes
}

impl Engine {
    /// Regions for the audio thread, in absolute samples with notes clipped to their region.
    /// A looped region plays one region per pass (so the feed only scans the pass playing).
    pub(super) fn build_midi(&self, index: usize, sr: u32) -> Vec<MidiClipRt> {
        let s = |secs: f64| (secs.max(0.0) * sr as f64).round() as u64;
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
                let c: &MidiClip = &c;
                let (start, end) = (s(c.position_secs), s(c.position_secs + c.length_secs));
                let mut notes: Vec<NoteRt> = c
                    .notes
                    .iter()
                    .filter(|n| n.start_secs < c.length_secs)
                    .map(|n| NoteRt {
                        start: s(c.position_secs + n.start_secs),
                        end: s(c.position_secs + n.start_secs + n.length_secs).min(end),
                        pitch: n.pitch,
                        velocity: n.velocity,
                    })
                    .collect();
                notes.sort_by_key(|n| n.start);
                let mut ctrl: Vec<CtrlRt> = c
                    .controllers
                    .iter()
                    .flat_map(|l| l.events.iter().filter(|e| e.time_secs < c.length_secs).map(move |e| CtrlRt { at: s(c.position_secs + e.time_secs), kind: l.kind, value: l.kind.raw(e.value) }))
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

    fn midi_index(&self, track: usize, clip: usize) -> Result<usize> {
        let index = self.track_index(track)?;
        if clip >= self.project.tracks[index].midi_clips.len() {
            bail!("track {track} has no MIDI region at index {clip}");
        }
        Ok(index)
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

    /// Add an empty MIDI region. Returns its index.
    pub fn add_midi_clip(&mut self, track: usize, position_secs: f64, length_secs: f64, name: Option<&str>) -> Result<usize> {
        let index = self.track_index(track)?;
        if length_secs < MIN_NOTE_SECS {
            bail!("region length must be positive");
        }
        let name = name.map(str::to_string).unwrap_or_else(|| self.project.tracks[index].name.clone());
        let clip = MidiClip { name, position_secs: position_secs.max(0.0), length_secs, mute: false, notes: Vec::new(), tab: None, tempos: Vec::new(), controllers: Vec::new(), looped: None };
        self.record("Create MIDI Region", None, |e| e.insert_midi_impl(index, clip))
    }

    /// Place an existing MIDI region (e.g. pasted). Returns its index.
    pub fn insert_midi_clip(&mut self, track: usize, clip: MidiClip) -> Result<usize> {
        let index = self.track_index(track)?;
        validate_notes(&clip.notes)?;
        self.record("Paste Region", None, |e| e.insert_midi_impl(index, clip))
    }

    fn insert_midi_impl(&mut self, index: usize, clip: MidiClip) -> Result<usize> {
        let clip = MidiClip { position_secs: clip.position_secs.max(0.0), notes: sorted(clip.notes), ..clip };
        self.project.tracks[index].midi_clips.push(clip);
        self.resync_midi(index)?;
        Ok(self.project.tracks[index].midi_clips.len() - 1)
    }

    pub fn update_midi_clip(&mut self, track: usize, clip: usize, u: MidiClipUpdate) -> Result<MidiClip> {
        let index = self.midi_index(track, clip)?;
        if u.length_secs.is_some_and(|l| l < MIN_NOTE_SECS) {
            bail!("region length must be positive");
        }
        let label = match (&u.position_secs, &u.length_secs) {
            (Some(_), None) => "Move Region",
            (None, Some(_)) | (Some(_), Some(_)) => "Resize Region",
            _ => "Region Settings",
        };
        self.record(label, None, |e| {
            let c = &mut e.project.tracks[index].midi_clips[clip];
            if let Some(v) = u.name {
                c.name = v;
            }
            if let Some(v) = u.position_secs {
                c.position_secs = v.max(0.0);
            }
            if let Some(v) = u.length_secs {
                c.length_secs = v;
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

    /// Set one controller's events in a region (times relative to the region start, values
    /// normalized: pitch bend -1..1, else 0..1): replace the lane, or add to it (an event at
    /// the time of an existing one replaces it). Empty and replacing removes the lane. One
    /// undo step; returns the lane's event count.
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

    pub fn move_midi_clip(&mut self, track: usize, clip: usize, position_secs: f64, to_track: Option<usize>) -> Result<MidiClip> {
        let from = self.midi_index(track, clip)?;
        let to = self.track_index(to_track.unwrap_or(track))?;
        self.record("Move Region", None, |e| {
            let mut c = e.project.tracks[from].midi_clips.remove(clip);
            c.position_secs = position_secs.max(0.0);
            e.project.tracks[to].midi_clips.push(c.clone());
            e.resync_midi(from)?;
            if to != from {
                e.resync_midi(to)?;
            }
            Ok(c)
        })
    }

    /// Copy a MIDI region; returns the copy's (track, index).
    pub fn copy_midi_clip(&mut self, track: usize, clip: usize, position_secs: f64, to_track: Option<usize>) -> Result<(usize, usize)> {
        let from = self.midi_index(track, clip)?;
        let to = self.track_index(to_track.unwrap_or(track))?;
        let copy = MidiClip { position_secs, ..self.project.tracks[from].midi_clips[clip].clone() };
        self.record("Copy Region", None, |e| Ok((to, e.insert_midi_impl(to, copy)?)))
    }

    /// Split a MIDI region at a timeline position. Notes that cross the split are
    /// shortened on the left; notes starting after it move to the right part, and the right
    /// part's controllers start at the values held at the split.
    pub fn split_midi_clip(&mut self, track: usize, clip: usize, at_secs: f64) -> Result<()> {
        let index = self.midi_index(track, clip)?;
        let c = self.project.tracks[index].midi_clips[clip].clone();
        let cut = at_secs - c.position_secs;
        if cut <= MIN_NOTE_SECS || cut >= c.length_secs - MIN_NOTE_SECS {
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

    fn note(start: f64, len: f64, pitch: u8) -> Note {
        Note { start_secs: start, length_secs: len, pitch, velocity: 100, tab: None }
    }

    #[test]
    fn midi_regions_notes_and_undo() {
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        let c = e.add_midi_clip(t, 1.0, 2.0, None).unwrap();
        e.set_midi_notes(t, c, vec![note(0.5, 0.25, 64), note(0.0, 0.5, 60)]).unwrap();
        let notes = &e.project().tracks[t].midi_clips[c].notes;
        assert_eq!(notes[0].pitch, 60, "notes are kept sorted by start");
        assert!(e.set_midi_notes(t, c, vec![Note { velocity: 0, ..note(0.0, 0.1, 60) }]).is_err());

        // Notes are placed in absolute samples and clipped to the region end.
        e.add_midi_notes(t, c, vec![note(1.8, 1.0, 67)]).unwrap();
        let rt = e.build_midi(t, 48_000);
        assert_eq!(rt[0].start, 48_000);
        assert_eq!(rt[0].notes[0].start, 48_000);
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
        let c = e.add_midi_clip(t, 0.0, 4.0, Some("Riff")).unwrap();
        e.set_midi_notes(t, c, vec![note(0.0, 1.0, 60), note(1.5, 1.0, 62), note(3.0, 0.5, 64)]).unwrap();
        e.split_midi_clip(t, c, 2.0).unwrap();
        let clips = &e.project().tracks[t].midi_clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].notes.len(), 2);
        assert!((clips[0].notes[1].length_secs - 0.5).abs() < 1e-9, "crossing note shortened");
        assert_eq!(clips[1].notes.len(), 1);
        assert!((clips[1].notes[0].start_secs - 1.0).abs() < 1e-9);

        let (_, i) = e.copy_midi_clip(t, 1, 8.0, None).unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[i].position_secs, 8.0);
    }

    #[test]
    fn controller_lanes_edit_split_and_play_in_samples() {
        let ev = |t: f64, v: f32| ControllerEvent { time_secs: t, value: v };
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        let c = e.add_midi_clip(t, 1.0, 4.0, None).unwrap();
        assert_eq!(e.set_midi_controllers(t, c, Controller::PitchBend, vec![ev(0.0, 0.0), ev(1.0, 1.0), ev(3.0, -0.5)], true).unwrap(), 3);
        assert_eq!(e.set_midi_controllers(t, c, Controller::PitchBend, vec![ev(1.0, 0.5), ev(2.0, 0.25)], false).unwrap(), 4, "same time replaces");
        e.set_midi_controllers(t, c, Controller::MOD_WHEEL, vec![ev(0.5, 1.0), ev(5.0, 0.0)], true).unwrap();
        assert!(e.set_midi_controllers(t, c, Controller::MOD_WHEEL, vec![ev(-1.0, 0.0)], true).is_err());
        assert_eq!(e.history_info().undo.as_deref(), Some("Edit Controllers"));

        // Absolute samples, sorted, raw values; the event past the region end is left out.
        let rt = e.build_midi(t, 1000);
        let ctrl: Vec<(u64, Controller, u16)> = rt[0].ctrl.iter().map(|c| (c.at, c.kind, c.value)).collect();
        assert_eq!(ctrl, vec![(1000, Controller::PitchBend, 8192), (1500, Controller::MOD_WHEEL, 127), (2000, Controller::PitchBend, 12288), (3000, Controller::PitchBend, 10240), (4000, Controller::PitchBend, 4096)]);

        e.split_midi_clip(t, c, 3.5).unwrap();
        let right = &e.project().tracks[t].midi_clips[1];
        assert_eq!(right.lane(Controller::PitchBend).unwrap().events, vec![ev(0.0, 0.25), ev(0.5, -0.5)]);
        e.undo().unwrap();
        e.set_midi_controllers(t, c, Controller::MOD_WHEEL, Vec::new(), true).unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[c].controllers.len(), 1, "an emptied lane goes");
        e.undo().unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[c].controllers.len(), 2);
    }
}
