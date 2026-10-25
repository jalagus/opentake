//! In-memory project model shared by the engine, the MCP server and file formats.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::automation::{AutomationLane, AutomationTarget};
use crate::fade::Fade;
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
    /// Armed tracks monitor their input while stopped or recording (Logic's Auto Input
    /// Monitoring); off means only the I button monitors.
    #[serde(default = "yes")]
    pub auto_input_monitoring: bool,
    /// Insert effects on the master bus.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub master_plugins: Vec<PluginSlot>,
    /// Automation of the master fader and master plug-ins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub master_automation: Vec<AutomationLane>,
    /// The master's automation lanes are shown (below the tracks).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_master_automation: bool,
    pub tracks: Vec<Track>,
}

fn default_click() -> f32 {
    0.5
}

fn unity() -> f32 {
    1.0
}

fn yes() -> bool {
    true
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
            auto_input_monitoring: true,
            master_plugins: Vec::new(),
            master_automation: Vec::new(),
            show_master_automation: false,
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
    /// A bus (aux) track: no regions, it sums the tracks routed to it through its effects.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bus: bool,
    /// Where the track's signal goes: the master (`None`) or a bus. In a session this is
    /// the bus's `id`; in files (and from importers) its track index, since ids aren't
    /// saved (see [`Project::outputs_from_indices`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    /// Sends to buses (see `sends.rs`), in addition to the output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<crate::sends::AuxSend>,
    /// Automation lanes (fader, pan, mute, plug-in parameters).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub automation: Vec<AutomationLane>,
    /// Whether the lanes are shown under the track in the arrangement.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_automation: bool,
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
            bus: false,
            output: None,
            sends: Vec::new(),
            automation: Vec::new(),
            show_automation: false,
            id: 0,
        }
    }

    /// An empty bus track.
    pub fn new_bus(name: impl Into<String>) -> Self {
        Self { bus: true, ..Self::new(name) }
    }

    pub fn is_instrument(&self) -> bool {
        self.instrument.is_some()
    }

    /// The instrument and effect slots.
    pub fn slots(&self) -> impl Iterator<Item = &PluginSlot> {
        self.instrument.iter().chain(&self.plugins)
    }

    /// The automated value of `target` at `t` seconds, if an active lane moves it.
    pub fn automated(&self, target: AutomationTarget, t: f64) -> Option<f32> {
        automated(&self.automation, target, t)
    }
}

/// The value an active lane for `target` has at `t`.
pub fn automated(lanes: &[AutomationLane], target: AutomationTarget, t: f64) -> Option<f32> {
    lanes.iter().find(|l| l.target == target && l.is_active()).and_then(|l| l.value_at(t))
}

/// Automation bookkeeping: plug-in slots get persistent ids that lanes refer to, and lanes
/// whose plug-in is gone go with it.
impl Project {
    fn all_slots_mut(&mut self) -> impl Iterator<Item = &mut PluginSlot> {
        self.tracks.iter_mut().flat_map(|t| t.instrument.iter_mut().chain(t.plugins.iter_mut())).chain(self.master_plugins.iter_mut())
    }

    /// Give every slot without an id (new, or from a file or importer that has none) a fresh
    /// one; a duplicated id (e.g. a hand-edited file) is replaced on its later copies.
    pub fn assign_slot_ids(&mut self) {
        let mut next = self.all_slots_mut().map(|s| s.id).max().unwrap_or(0) + 1;
        let mut seen = std::collections::HashSet::new();
        for slot in self.all_slots_mut() {
            if slot.id == 0 || !seen.insert(slot.id) {
                slot.id = next;
                seen.insert(next);
                next += 1;
            }
        }
    }

    /// Drop lanes that point at a plug-in slot or send their track (or the master) no longer
    /// has, or at what a bus or the master can't move. Returns whether anything was removed.
    pub fn prune_automation(&mut self) -> bool {
        let before = self.tracks.iter().map(|t| t.automation.len()).sum::<usize>() + self.master_automation.len();
        for t in &mut self.tracks {
            let ids: Vec<u32> = t.slots().map(|s| s.id).collect();
            let sends: Vec<u32> = t.sends.iter().map(|s| s.id).collect();
            t.automation.retain(|l| match l.target {
                AutomationTarget::Param { slot, .. } => ids.contains(&slot),
                AutomationTarget::Send { send } => sends.contains(&send),
                _ => true,
            });
        }
        let ids: Vec<u32> = self.master_plugins.iter().map(|s| s.id).collect();
        self.master_automation.retain(|l| match l.target {
            AutomationTarget::Param { slot, .. } => ids.contains(&slot),
            AutomationTarget::Volume => true,
            _ => false,
        });
        before != self.tracks.iter().map(|t| t.automation.len()).sum::<usize>() + self.master_automation.len()
    }
}

/// Routing: tracks feed the master or a bus (and buses through sends); buses can feed further
/// buses, never in a loop.
impl Project {
    pub fn track_by_id(&self, id: u64) -> Option<usize> {
        self.tracks.iter().position(|t| t.id == id)
    }

    /// Index of the bus track `index` feeds (`None` = the master).
    pub fn output_of(&self, index: usize) -> Option<usize> {
        self.tracks[index].output.and_then(|id| self.track_by_id(id)).filter(|&b| b != index && self.tracks[b].bus)
    }

    /// The buses from `index` on the way to the master (bounded, in case of a bad file).
    pub fn route(&self, index: usize) -> Vec<usize> {
        let mut route = Vec::new();
        let mut at = index;
        while let Some(next) = self.output_of(at) {
            if next == index || route.contains(&next) {
                break;
            }
            route.push(next);
            at = next;
        }
        route
    }

    /// Whether track `index` may feed `to` (a bus index, `None` = master): `to` must be a bus
    /// that isn't the track itself or fed by it (through outputs or sends).
    pub fn can_route(&self, index: usize, to: Option<usize>) -> bool {
        let Some(to) = to else { return true };
        to != index && self.tracks.get(to).is_some_and(|t| t.bus) && !self.feeds(to, index)
    }

    /// Tracks feeding bus `index` directly.
    pub fn sources_of(&self, index: usize) -> Vec<usize> {
        (0..self.tracks.len()).filter(|&i| self.output_of(i) == Some(index)).collect()
    }

    /// Mixing order: every track before the buses it feeds through its output or sends
    /// (plain tracks first, then buses as soon as everything feeding them is in).
    pub fn processing_order(&self) -> Vec<usize> {
        let n = self.tracks.len();
        let targets: Vec<Vec<usize>> = (0..n).map(|i| self.targets(i)).collect();
        let mut placed: Vec<bool> = self.tracks.iter().map(|t| !t.bus).collect();
        let mut order: Vec<usize> = (0..n).filter(|&i| placed[i]).collect();
        while order.len() < n {
            let before = order.len();
            for b in 0..n {
                if !placed[b] && (0..n).all(|i| placed[i] || !targets[i].contains(&b)) {
                    placed[b] = true;
                    order.push(b);
                }
            }
            if order.len() == before {
                // A loop (never made by edits): the rest in list order.
                order.extend((0..n).filter(|&i| !placed[i]));
            }
        }
        order
    }

    /// Which tracks reach the output: not muted, and when anything is soloed, soloed, fed
    /// (through outputs or sends) by a soloed track, or feeding a soloed bus.
    pub fn audible(&self) -> Vec<bool> {
        let order = self.processing_order();
        let targets: Vec<Vec<usize>> = (0..self.tracks.len()).map(|i| self.targets(i)).collect();
        let mut soloed: Vec<bool> = self.tracks.iter().map(|t| t.solo).collect();
        for &i in &order {
            if soloed[i] {
                targets[i].iter().for_each(|&b| soloed[b] = true);
            }
        }
        let mut feeds_solo = vec![false; self.tracks.len()];
        for &i in order.iter().rev() {
            feeds_solo[i] = targets[i].iter().any(|&b| self.tracks[b].solo || feeds_solo[b]);
        }
        let any_solo = self.tracks.iter().any(|t| t.solo);
        (0..self.tracks.len()).map(|i| !self.tracks[i].mute && (!any_solo || soloed[i] || feeds_solo[i])).collect()
    }

    /// Outputs (and sends) from track indices (as files store them) to session ids; invalid
    /// outputs (not a bus, or a loop) go to the master, invalid sends go. Call once the
    /// tracks have their ids.
    pub fn outputs_from_indices(&mut self) {
        let ids: Vec<(u64, bool)> = self.tracks.iter().map(|t| (t.id, t.bus)).collect();
        for (i, t) in self.tracks.iter_mut().enumerate() {
            t.output = t.output.and_then(|o| ids.get(o as usize).filter(|(_, bus)| *bus && o as usize != i)).map(|(id, _)| *id);
        }
        for i in 0..self.tracks.len() {
            if self.output_of(i).is_some_and(|to| self.route(to).contains(&i)) {
                self.tracks[i].output = None;
            }
        }
        self.sends_from_indices();
    }

    /// Outputs (and sends) from session ids to track indices, for saving.
    pub fn outputs_to_indices(&mut self) {
        self.sends_to_indices();
        let outputs: Vec<Option<usize>> = (0..self.tracks.len()).map(|i| self.output_of(i)).collect();
        for (t, o) in self.tracks.iter_mut().zip(outputs) {
            t.output = o.map(|o| o as u64);
        }
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
    /// Offset into the source file where playback starts (negative: silence first).
    pub source_offset_secs: f64,
    pub gain: f32,
    pub mute: bool,
    /// Take folder: the alternatives recorded for this region (cycle recording), one of
    /// which (`active_take`) plays. `file` and `source_offset_secs` are the active take's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub takes: Vec<Take>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub active_take: usize,
    /// Pitch shift in semitones plus fine cents, independent of `stretch`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pitch_semitones: i32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub pitch_cents: i32,
    /// Time stretch: how many times longer the audio plays than recorded (2.0 = half speed).
    /// Position and length are timeline seconds, but `source_offset_secs` (and take offsets)
    /// stay in source seconds, so `t` seconds of the region play `t / stretch` of the source.
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub stretch: f64,
    /// The region's own fades (timeline seconds). Overlaps add crossfades; see `fade.rs`.
    #[serde(default, skip_serializing_if = "Fade::is_none")]
    pub fade_in: Fade,
    #[serde(default, skip_serializing_if = "Fade::is_none")]
    pub fade_out: Fade,
}

fn is_zero<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

fn one() -> f64 {
    1.0
}

fn is_one(v: &f64) -> bool {
    *v == 1.0
}

/// One take in a take folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Take {
    pub name: String,
    pub file: PathBuf,
    /// Its source offset relative to the region's (so the active take's is 0, and moving,
    /// trimming or splitting the region moves every take with it).
    pub offset_secs: f64,
}

impl Clip {
    /// Total pitch shift in semitones.
    pub fn transpose(&self) -> f64 {
        self.pitch_semitones as f64 + self.pitch_cents as f64 / 100.0
    }

    /// Whether the region plays processed (pitch-shifted or time-stretched) audio.
    pub fn is_flexed(&self) -> bool {
        self.pitch_semitones != 0 || self.pitch_cents != 0 || self.stretch != 1.0
    }

    /// Split `cut` timeline seconds after the region start into (left, right).
    /// Each part keeps the fade at its outer end.
    pub fn split(&self, cut: f64) -> (Clip, Clip) {
        let mut left = Clip { length_secs: cut, fade_out: Fade::default(), ..self.clone() };
        let mut right = Clip {
            position_secs: self.position_secs + cut,
            length_secs: self.length_secs - cut,
            source_offset_secs: self.source_offset_secs + cut / self.stretch,
            fade_in: Fade::default(),
            ..self.clone()
        };
        left.clamp_fades();
        right.clamp_fades();
        (left, right)
    }

    /// Keep the fades within the region: the fade-in first, the fade-out in what's left.
    pub fn clamp_fades(&mut self) {
        let len = self.length_secs.max(0.0);
        self.fade_in.secs = self.fade_in.secs.clamp(0.0, len);
        self.fade_out.secs = self.fade_out.secs.clamp(0.0, len - self.fade_in.secs);
    }

    /// Make take `index` the one that plays. Returns false if there's no such take.
    pub fn select_take(&mut self, index: usize) -> bool {
        let Some(take) = self.takes.get(index) else { return false };
        let (delta, file) = (take.offset_secs, take.file.clone());
        for t in &mut self.takes {
            t.offset_secs -= delta;
        }
        self.source_offset_secs += delta;
        self.file = file;
        self.active_take = index;
        true
    }

    /// Add a take (by its absolute source offset) and make it the one that plays. A plain
    /// region becomes a take folder whose first take is what it played so far.
    pub fn push_take(&mut self, name: String, file: PathBuf, source_offset_secs: f64) {
        if self.takes.is_empty() {
            self.takes.push(Take { name: "Take 1".into(), file: self.file.clone(), offset_secs: 0.0 });
            self.active_take = 0;
        }
        self.takes.push(Take { name, file, offset_secs: source_offset_secs - self.source_offset_secs });
        self.select_take(self.takes.len() - 1);
    }

    /// Remove a take; the next one (or the previous, for the last) plays if it was active.
    /// A folder left with one take becomes a plain region. Returns false if there's no such take.
    pub fn remove_take(&mut self, index: usize) -> bool {
        if index >= self.takes.len() || self.takes.len() < 2 {
            return false;
        }
        let active = self.active_take;
        if index == active {
            self.select_take(if index + 1 < self.takes.len() { index + 1 } else { index - 1 });
        }
        self.takes.remove(index);
        if self.active_take > index {
            self.active_take -= 1;
        }
        if self.takes.len() == 1 {
            self.takes.clear();
            self.active_take = 0;
        }
        true
    }
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
    /// Persistent id, unique within the project, that automation lanes refer to (0 = not
    /// assigned yet; see [`Project::assign_slot_ids`]).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub id: u32,
    /// Engine handle of the loaded instance (0 = not loaded). Session only.
    #[serde(skip)]
    pub instance: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum PluginFormat {
    Vst3,
    Vst2,
    Clap,
    /// macOS Audio Unit (`path` is "Manufacturer: Name", `uid` the codes, e.g. "aufx:dely:appl").
    AudioUnit,
    /// Built into OpenTake (`path` names it, e.g. "synth").
    Builtin,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(file: &str, offset: f64) -> Clip {
        Clip { name: "c".into(), file: file.into(), position_secs: 4.0, length_secs: 2.0, source_offset_secs: offset, gain: 1.0, mute: false, takes: Vec::new(), active_take: 0, pitch_semitones: 0, pitch_cents: 0, stretch: 1.0, fade_in: Fade::default(), fade_out: Fade::default() }
    }

    #[test]
    fn stretched_split_and_neutral_serialization() {
        let mut c = clip("a.wav", 1.0);
        assert!(!serde_json::to_string(&c).unwrap().contains("stretch"), "neutral values aren't written");
        c.stretch = 2.0;
        let (l, r) = c.split(0.5);
        assert_eq!((l.length_secs, r.position_secs, r.length_secs, r.source_offset_secs), (0.5, 4.5, 1.5, 1.25));
        c.pitch_semitones = -3;
        let back: Clip = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!((back.stretch, back.transpose()), (2.0, -3.0));
        // Files from before pitch/stretch load with neutral values.
        let old: Clip = serde_json::from_str(r#"{"name":"c","file":"a.wav","position_secs":0,"length_secs":1,"source_offset_secs":0,"gain":1,"mute":false}"#).unwrap();
        assert!(!old.is_flexed() && old.stretch == 1.0);
    }

    #[test]
    fn takes_switch_offsets_and_files() {
        // Two passes of a cycle in one file, then a take from another recording.
        let mut c = clip("rec.wav", 1.0);
        c.push_take("Take 2".into(), "rec.wav".into(), 3.0);
        c.push_take("Take 3".into(), "other.wav".into(), -0.5);
        assert_eq!((c.active_take, c.file.to_str(), c.source_offset_secs), (2, Some("other.wav"), -0.5));
        assert!(c.select_take(1));
        assert_eq!((c.file.to_str(), c.source_offset_secs), (Some("rec.wav"), 3.0));
        assert_eq!(c.takes.iter().map(|t| t.offset_secs).collect::<Vec<_>>(), vec![-2.0, 0.0, -3.5]);
        // A trim or split shifts the region's offset; every take keeps its alignment.
        c.source_offset_secs += 0.25;
        assert!(c.select_take(0));
        assert_eq!(c.source_offset_secs, 1.25);
        assert!(!c.select_take(3));

        // Removing the active take plays the next; one take left is a plain region.
        assert!(c.remove_take(0));
        assert_eq!((c.active_take, c.source_offset_secs, c.takes.len()), (0, 3.25, 2));
        assert!(c.remove_take(1));
        assert!(c.takes.is_empty() && c.source_offset_secs == 3.25 && c.file.to_str() == Some("rec.wav"));
        assert!(!c.remove_take(0));
    }

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

    fn routed(buses: &[bool], outputs: &[Option<usize>]) -> Project {
        let mut p = Project::default();
        for (i, (&bus, &out)) in buses.iter().zip(outputs).enumerate() {
            p.tracks.push(Track { id: 10 + i as u64, bus, output: out.map(|o| o as u64), ..Track::new(format!("T{i}")) });
        }
        p.outputs_from_indices();
        p
    }

    #[test]
    fn routing_orders_buses_after_their_sources_and_refuses_loops() {
        // 0 -> bus 2 -> bus 1 -> master; 3 -> master.
        let mut p = routed(&[false, true, true, false], &[Some(2), None, Some(1), None]);
        assert_eq!(p.tracks[0].output, Some(12), "indices become ids");
        assert_eq!(p.route(0), vec![2, 1]);
        assert_eq!(p.processing_order(), vec![0, 3, 2, 1]);
        assert_eq!(p.sources_of(1), vec![2]);
        assert!(!p.can_route(1, Some(2)), "bus 1 feeding bus 2 would loop");
        assert!(!p.can_route(2, Some(2)) && !p.can_route(0, Some(3)), "not itself, only buses");
        assert!(p.can_route(3, Some(2)) && p.can_route(1, None));
        // A loop or a non-bus target in a file goes to the master.
        let bad = routed(&[true, true, false], &[Some(1), Some(0), Some(2)]);
        assert_eq!(bad.tracks.iter().filter(|t| t.output.is_some()).count(), 1);
        p.outputs_to_indices();
        assert_eq!(p.tracks.iter().map(|t| t.output).collect::<Vec<_>>(), vec![Some(2), None, Some(1), None]);
    }

    #[test]
    fn solo_keeps_the_route_and_a_soloed_bus_keeps_its_sources() {
        // 0 -> bus 1 -> master, 2 -> master.
        let mut p = routed(&[false, true, false], &[Some(1), None, None]);
        p.tracks[0].solo = true;
        assert_eq!(p.audible(), vec![true, true, false]);
        p.tracks[0].solo = false;
        p.tracks[1].solo = true;
        assert_eq!(p.audible(), vec![true, true, false]);
        p.tracks[1].mute = true;
        assert_eq!(p.audible(), vec![true, false, false]);
    }

    #[test]
    fn slots_get_unique_ids_and_orphaned_lanes_go() {
        use crate::automation::{AutomationLane, AutomationTarget};
        let slot = |id: u32| PluginSlot { name: "fx".into(), format: PluginFormat::Builtin, path: "p".into(), uid: "p".into(), bypass: false, state: Vec::new(), params: Vec::new(), id, instance: 0 };
        let mut p = Project::default();
        let mut t = Track::new("A");
        t.plugins = vec![slot(4), slot(0), slot(4)];
        p.tracks.push(t);
        p.master_plugins.push(slot(0));
        p.assign_slot_ids();
        let ids: Vec<u32> = p.tracks[0].plugins.iter().chain(&p.master_plugins).map(|s| s.id).collect();
        assert_eq!(ids, vec![4, 5, 6, 7], "kept, new, duplicate replaced, master");
        let param = |slot| AutomationTarget::Param { slot, param: 0 };
        p.tracks[0].automation = vec![AutomationLane::new(AutomationTarget::Volume), AutomationLane::new(param(5)), AutomationLane::new(param(7))];
        p.master_automation = vec![AutomationLane::new(param(7)), AutomationLane::new(AutomationTarget::Pan)];
        assert!(p.prune_automation());
        assert_eq!(p.tracks[0].automation.iter().map(|l| l.target).collect::<Vec<_>>(), vec![AutomationTarget::Volume, param(5)], "the master's slot isn't the track's");
        assert_eq!(p.master_automation.len(), 1, "the master has no pan");
        assert!(!p.prune_automation());
        // Tracks without automation don't write the fields.
        let json = serde_json::to_string(&Track::new("B")).unwrap();
        assert!(!json.contains("automation"), "{json}");
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
