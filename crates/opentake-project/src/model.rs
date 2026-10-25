//! In-memory project model shared by the engine, the MCP server and file formats.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::tempo::{TempoChange, TempoMap};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub name: String,
    pub tempo_bpm: f64,
    pub time_signature: (u32, u32),
    pub sample_rate: u32,
    /// Master output gain (linear, 1.0 = 0 dB).
    #[serde(default = "unity")]
    pub master_volume: f32,
    #[serde(default)]
    pub metronome_enabled: bool,
    /// Click level (linear).
    #[serde(default = "default_click")]
    pub metronome_volume: f32,
    /// Cycle (loop) playback between `loop_start_secs` and `loop_end_secs`.
    #[serde(default)]
    pub loop_enabled: bool,
    #[serde(default)]
    pub loop_start_secs: f64,
    #[serde(default)]
    pub loop_end_secs: f64,
    /// Tempo and time-signature changes after the start (`tempo_bpm`/`time_signature` are
    /// the opening values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tempo_changes: Vec<TempoChange>,
    /// Bars of metronome count-in before recording (0 = off).
    #[serde(default)]
    pub count_in_bars: u32,
    /// Also count in before plain playback.
    #[serde(default)]
    pub count_in_on_play: bool,
    /// Insert effects on the master bus.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub master_plugins: Vec<PluginSlot>,
    pub tracks: Vec<Track>,
}

fn default_click() -> f32 {
    0.5
}

fn unity() -> f32 {
    1.0
}

impl Default for Project {
    fn default() -> Self {
        Self {
            name: "Untitled".into(),
            tempo_bpm: 120.0,
            time_signature: (4, 4),
            sample_rate: 48_000,
            master_volume: 1.0,
            metronome_enabled: false,
            metronome_volume: default_click(),
            loop_enabled: false,
            loop_start_secs: 0.0,
            loop_end_secs: 0.0,
            tempo_changes: Vec::new(),
            count_in_bars: 0,
            count_in_on_play: false,
            master_plugins: Vec::new(),
            tracks: Vec::new(),
        }
    }
}

impl Project {
    /// The project's tempo map (opening tempo/signature plus changes).
    pub fn tempo_map(&self) -> TempoMap {
        TempoMap::new(self.tempo_bpm, self.time_signature, &self.tempo_changes)
    }

    /// The song tempo playing at time `t`, from an unmuted region with tempo marks
    /// (e.g. an imported Guitar Pro song). `None` when only the project tempo applies.
    pub fn song_tempo_at(&self, t: f64) -> Option<f64> {
        self.tracks
            .iter()
            .filter(|tr| !tr.mute)
            .flat_map(|tr| &tr.midi_clips)
            .filter(|c| !c.mute && !c.tempos.is_empty() && t >= c.position_secs && t < c.position_secs + c.length_secs)
            .find_map(|c| c.tempo_at(t - c.position_secs))
    }

    /// End time (seconds) of the last audio or MIDI region in the project.
    pub fn length_secs(&self) -> f64 {
        let audio = self.tracks.iter().flat_map(|t| &t.clips).map(|c| c.position_secs + c.length_secs);
        let midi = self.tracks.iter().flat_map(|t| &t.midi_clips).map(|c| c.position_secs + c.length_secs);
        audio.chain(midi).fold(0.0, f64::max)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Track {
    /// Session-unique id assigned by the engine (stable across edits and undo).
    #[serde(skip)]
    pub id: u64,
    pub name: String,
    /// Linear gain, 1.0 = 0 dB.
    pub volume: f32,
    /// -1.0 (left) .. 1.0 (right).
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub armed: bool,
    /// Zero-based input channel on the audio interface used when recording.
    pub input_channel: u16,
    pub monitor_input: bool,
    pub clips: Vec<Clip>,
    /// MIDI regions (played by `instrument`).
    #[serde(default)]
    pub midi_clips: Vec<MidiClip>,
    /// Instrument plugin; a track with one is an instrument (software) track.
    #[serde(default)]
    pub instrument: Option<PluginSlot>,
    pub plugins: Vec<PluginSlot>,
}

impl Track {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            volume: 1.0,
            pan: 0.0,
            mute: false,
            solo: false,
            armed: false,
            input_channel: 0,
            monitor_input: false,
            clips: Vec::new(),
            midi_clips: Vec::new(),
            instrument: None,
            plugins: Vec::new(),
            id: 0,
        }
    }

    pub fn is_instrument(&self) -> bool {
        self.instrument.is_some()
    }
}

/// A MIDI region on the timeline. Note times are relative to the region start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MidiClip {
    pub name: String,
    pub position_secs: f64,
    pub length_secs: f64,
    #[serde(default)]
    pub mute: bool,
    pub notes: Vec<Note>,
    /// Tablature layout when the region came from a tab (e.g. Guitar Pro import).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<TabLayout>,
    /// The song's tempo changes when the region came from a score (e.g. Guitar Pro); the
    /// notes are already timed by them. Shown so you can see what tempo is playing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tempos: Vec<TempoMark>,
}

/// A tempo change inside a region, relative to the region start.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct TempoMark {
    pub start_secs: f64,
    /// Quarter notes per minute.
    pub bpm: f64,
}

impl MidiClip {
    /// The song tempo at `rel` seconds into the region, if the region carries tempo marks.
    pub fn tempo_at(&self, rel: f64) -> Option<f64> {
        let i = self.tempos.partition_point(|m| m.start_secs <= rel + 1e-9);
        self.tempos.get(i.saturating_sub(1)).map(|m| m.bpm)
    }

    /// Split the tempo marks at `cut`: marks from `cut` on move to the returned list,
    /// shifted to start at 0, led by the tempo in effect at the cut.
    pub fn split_tempos(&mut self, cut: f64) -> Vec<TempoMark> {
        let mut right: Vec<TempoMark> = self.tempos.iter().filter(|m| m.start_secs >= cut - 1e-9).map(|m| TempoMark { start_secs: m.start_secs - cut, ..*m }).collect();
        if let Some(bpm) = self.tempo_at(cut).filter(|_| right.first().is_none_or(|m| m.start_secs > 1e-9)) {
            right.insert(0, TempoMark { start_secs: 0.0, bpm });
        }
        self.tempos.retain(|m| m.start_secs < cut - 1e-9);
        right
    }
}

/// How a MIDI region's notes are laid out as guitar/bass tablature.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabLayout {
    /// Open-string MIDI pitches, lowest string first.
    pub tuning: Vec<u8>,
    #[serde(default)]
    pub capo: u8,
    /// Bars of the original score, in playing order (repeats expanded).
    #[serde(default)]
    pub bars: Vec<TabBar>,
}

impl TabLayout {
    /// String/fret for a note: its stored position if that still matches the pitch,
    /// otherwise the lowest-fret playable position.
    pub fn position(&self, note: &Note) -> Option<TabPos> {
        let sounding = |string: u8, fret: u8| self.tuning.get(string as usize).map(|&open| open as i32 + self.capo as i32 + fret as i32);
        if let Some(p) = note.tab {
            if sounding(p.string, p.fret) == Some(note.pitch as i32) {
                return Some(p);
            }
        }
        (0..self.tuning.len() as u8)
            .filter_map(|string| {
                let fret = note.pitch as i32 - self.tuning[string as usize] as i32 - self.capo as i32;
                (0..=24).contains(&fret).then_some(TabPos { string, fret: fret as u8 })
            })
            .min_by_key(|p| p.fret)
    }

    /// Bars for the part of the region from `cut` seconds onwards, shifted to start at 0.
    pub fn split_off(&mut self, cut: f64) -> TabLayout {
        let right = self.bars.iter().filter(|b| b.start_secs >= cut - 1e-9).map(|b| TabBar { start_secs: b.start_secs - cut, ..b.clone() }).collect();
        self.bars.retain(|b| b.start_secs < cut - 1e-9);
        TabLayout { tuning: self.tuning.clone(), capo: self.capo, bars: right }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabBar {
    /// Seconds from the region start.
    pub start_secs: f64,
    /// Bar number in the score (1-based).
    pub number: u32,
    pub numerator: u32,
    pub denominator: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
}

/// A note's place on the fretboard (string 0 = lowest).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TabPos {
    pub string: u8,
    pub fret: u8,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Note {
    /// Seconds from the region start.
    pub start_secs: f64,
    pub length_secs: f64,
    /// MIDI note number (60 = middle C).
    pub pitch: u8,
    /// 1..=127
    pub velocity: u8,
    /// String/fret when the note came from tablature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<TabPos>,
}

/// A reference to a region of an audio file placed on the timeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Clip {
    pub name: String,
    pub file: PathBuf,
    pub position_secs: f64,
    pub length_secs: f64,
    /// Offset into the source file where playback starts.
    pub source_offset_secs: f64,
    pub gain: f32,
    pub mute: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginSlot {
    /// Display name, e.g. "VST3: ReaEQ (Cockos)".
    pub name: String,
    pub format: PluginFormat,
    /// Path or file name of the plugin binary/bundle (may be relative, as Reaper stores it).
    pub path: String,
    /// Format-specific identifier (VST3 class id as hex, VST2 unique id, ...).
    pub uid: String,
    pub bypass: bool,
    /// Opaque, format-specific plugin state (base64 lines as stored by the host that saved it).
    pub state: Vec<String>,
    /// Normalized parameter values set by this host, as (param id, value). Applied on load.
    #[serde(default)]
    pub params: Vec<(u32, f64)>,
    /// Engine handle of the loaded instance (0 = not loaded). Session only.
    #[serde(skip)]
    pub instance: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PluginFormat {
    Vst3,
    Vst2,
    Clap,
    /// Built into OpenTake (`path` names it, e.g. "synth").
    Builtin,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(pitch: u8, tab: Option<TabPos>) -> Note {
        Note { start_secs: 0.0, length_secs: 1.0, pitch, velocity: 100, tab }
    }

    #[test]
    fn tab_positions_keep_stored_frets_and_place_edited_notes() {
        let layout = TabLayout { tuning: vec![40, 45, 50, 55, 59, 64], capo: 0, bars: Vec::new() };
        // Stored position that matches the pitch is kept (A on the low E string, 5th fret).
        let a = TabPos { string: 0, fret: 5 };
        assert_eq!(layout.position(&note(45, Some(a))), Some(a));
        // After a pitch edit the stored position no longer matches: lowest fret wins.
        assert_eq!(layout.position(&note(47, Some(a))), Some(TabPos { string: 1, fret: 2 }));
        // Capo shifts the frets.
        let capo = TabLayout { capo: 2, ..layout.clone() };
        assert_eq!(capo.position(&note(42, None)), Some(TabPos { string: 0, fret: 0 }));
        assert_eq!(layout.position(&note(20, None)), None, "below the lowest string");
    }

    #[test]
    fn split_moves_later_bars_to_the_right_part() {
        let bar = |s: f64, n: u32| TabBar { start_secs: s, number: n, numerator: 4, denominator: 4, section: None };
        let mut left = TabLayout { tuning: vec![40], capo: 0, bars: vec![bar(0.0, 1), bar(2.0, 2), bar(4.0, 3)] };
        let right = left.split_off(2.0);
        assert_eq!(left.bars.iter().map(|b| b.number).collect::<Vec<_>>(), vec![1]);
        assert_eq!(right.bars.iter().map(|b| (b.number, b.start_secs)).collect::<Vec<_>>(), vec![(2, 0.0), (3, 2.0)]);
    }
}
