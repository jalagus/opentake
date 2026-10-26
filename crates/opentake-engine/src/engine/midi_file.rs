//! Standard MIDI Files: import as instrument tracks with MIDI regions (like the Guitar Pro
//! import), and export the project's MIDI regions. Notes, control changes (except channel
//! mode messages), pitch bend, channel pressure and markers are carried both ways.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use midly::num::{u4, u7, u14, u15, u24, u28};
use midly::{Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, Timing, TrackEvent, TrackEventKind};
use opentake_project::{Controller, ControllerEvent, ControllerLane, Marker, MidiClip, Note, Project, TempoMark};
use serde::Serialize;

use super::Engine;
use super::guitarpro::tempo_map_from_points;

/// MIDI channel 10 (0-based 9) is the General MIDI drum channel.
const DRUM_CHANNEL: u8 = 9;
/// Resolution of exported files.
const EXPORT_PPQ: u16 = 480;

/// True for `.mid` / `.midi` / `.smf` paths.
pub fn is_midi_file_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "mid" | "midi" | "smf"))
}

/// What to import and how.
#[derive(Debug, Clone)]
pub struct MidiFileImport {
    /// Part indices (see `MidiFileSummary::tracks`) to import; `None` = all.
    pub tracks: Option<Vec<usize>>,
    /// Instrument for pitched parts: "synth" or a VST3 instrument name / bundle path.
    pub instrument: String,
    /// Instrument for drum parts (channel 10); defaults to the built-in drum kit.
    pub drum_instrument: Option<String>,
    /// Adopt the file's tempo map and time signatures.
    pub set_tempo: bool,
    /// Where the file starts on the timeline (seconds).
    pub position_secs: f64,
}

impl Default for MidiFileImport {
    fn default() -> Self {
        Self { tracks: None, instrument: "synth".into(), drum_instrument: None, set_tempo: true, position_secs: 0.0 }
    }
}

/// Summary of a MIDI file, for choosing what to import.
#[derive(Debug, Clone, Serialize)]
pub struct MidiFileSummary {
    pub name: String,
    /// SMF format: 0 (one track), 1 (parallel tracks) or 2 (independent patterns).
    pub format: u8,
    pub tempo_bpm: f64,
    pub time_signature: (u32, u32),
    /// Tempo and time-signature changes after the start.
    pub tempo_changes: usize,
    pub duration_secs: f64,
    /// Importable parts: one per MIDI track, split per channel where a track mixes channels.
    pub tracks: Vec<MidiFileTrackInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MidiFileTrackInfo {
    pub index: usize,
    pub name: String,
    /// "drums" (channel 10) or "pitched".
    pub kind: String,
    /// MIDI channel, 1..=16.
    pub channel: u8,
    /// General MIDI program name (pitched parts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub notes: usize,
}

/// One importable part: the notes of one channel of one MIDI track, in song seconds.
struct Part {
    name: String,
    channel: u8,
    program: Option<u8>,
    notes: Vec<Note>,
    controllers: Vec<ControllerLane>,
}

struct Song {
    name: String,
    format: u8,
    /// (seconds, bpm), starting at 0.
    tempos: Vec<(f64, f64)>,
    /// (seconds, signature) in order.
    signatures: Vec<(f64, (u32, u32))>,
    parts: Vec<Part>,
    /// Marker meta events: (seconds, name).
    markers: Vec<(f64, String)>,
    duration: f64,
}

impl Song {
    fn opening_signature(&self) -> (u32, u32) {
        self.signatures.first().filter(|s| s.0 < 1e-9).map_or((4, 4), |s| s.1)
    }

    /// Opening tempo, signature and changes placed at `start` seconds.
    fn tempo_map(&self, start: f64) -> (f64, (u32, u32), Vec<opentake_project::TempoChange>) {
        let mut points: Vec<_> = self.tempos.iter().map(|&(t, bpm)| (t, Some(bpm), None)).collect();
        points.extend(self.signatures.iter().map(|&(t, sig)| (t, None, Some(sig))));
        tempo_map_from_points(points, self.tempos[0].1, self.opening_signature(), start)
    }
}

/// Converts file ticks to seconds through the tempo map (or the SMPTE frame rate).
struct Clock {
    /// (tick, seconds at that tick, seconds per tick from there on).
    segments: Vec<(u64, f64, f64)>,
}

impl Clock {
    fn new(timing: Timing, tempos: &[(u64, u32)]) -> Self {
        match timing {
            // SMPTE time: ticks are fixed subdivisions of a frame; tempo events don't apply.
            Timing::Timecode(fps, sub) => Self { segments: vec![(0, 0.0, 1.0 / (fps.as_f32() as f64 * sub.max(1) as f64))] },
            Timing::Metrical(tpq) => {
                let tpq = tpq.as_int().max(1) as f64;
                let mut clock = Self { segments: vec![(0, 0.0, 0.5 / tpq)] };
                for &(tick, us) in tempos {
                    let seg = (tick, clock.secs(tick), us.max(1) as f64 / 1e6 / tpq);
                    let last = clock.segments.last_mut().unwrap();
                    if last.0 == tick { *last = seg } else { clock.segments.push(seg) }
                }
                clock
            }
        }
    }

    fn secs(&self, tick: u64) -> f64 {
        let i = self.segments.partition_point(|s| s.0 <= tick).saturating_sub(1);
        let (t0, s0, spt) = self.segments[i];
        s0 + (tick - t0) as f64 * spt
    }
}

fn parse(bytes: &[u8]) -> Result<Song> {
    let smf = Smf::parse(bytes).context("not a valid Standard MIDI File")?;
    // Events with absolute ticks.
    let tracks: Vec<Vec<(u64, TrackEventKind)>> = smf
        .tracks
        .iter()
        .map(|t| {
            let mut tick = 0u64;
            t.iter().map(|ev| { tick += ev.delta.as_int() as u64; (tick, ev.kind) }).collect()
        })
        .collect();
    // Format 1 keeps tempo and signatures in the first track, but accept them anywhere.
    let mut tempo_ticks: Vec<(u64, u32)> = Vec::new();
    let mut sig_ticks: Vec<(u64, (u32, u32))> = Vec::new();
    for &(tick, kind) in tracks.iter().flatten() {
        match kind {
            TrackEventKind::Meta(MetaMessage::Tempo(us)) => tempo_ticks.push((tick, us.as_int())),
            TrackEventKind::Meta(MetaMessage::TimeSignature(num, pow, ..)) if num > 0 => sig_ticks.push((tick, (num as u32, 1 << pow.min(6)))),
            _ => {}
        }
    }
    tempo_ticks.sort_by_key(|t| t.0);
    sig_ticks.sort_by_key(|t| t.0);
    let clock = Clock::new(smf.header.timing, &tempo_ticks);

    let mut tempos: Vec<(f64, f64)> = vec![(0.0, 120.0)];
    for &(tick, us) in &tempo_ticks {
        let at = (clock.secs(tick), 60e6 / us.max(1) as f64);
        let last = tempos.last_mut().unwrap();
        if (last.0 - at.0).abs() < 1e-9 {
            *last = at;
        } else if (last.1 - at.1).abs() > 1e-6 {
            tempos.push(at);
        }
    }
    let mut signatures: Vec<(f64, (u32, u32))> = Vec::new();
    for &(tick, sig) in &sig_ticks {
        let at = clock.secs(tick);
        match signatures.last_mut() {
            Some(last) if (last.0 - at).abs() < 1e-9 => last.1 = sig,
            Some(last) if last.1 == sig => {}
            _ => signatures.push((at, sig)),
        }
    }

    let mut markers: Vec<(f64, String)> = tracks
        .iter()
        .flatten()
        .filter_map(|&(tick, kind)| match kind {
            TrackEventKind::Meta(MetaMessage::Marker(n)) => Some((clock.secs(tick), String::from_utf8_lossy(n).trim().to_string())),
            _ => None,
        })
        .collect();
    markers.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut song_name = String::new();
    let mut parts = Vec::new();
    let mut duration: f64 = 0.0;
    for (ti, events) in tracks.iter().enumerate() {
        let end_tick = events.last().map_or(0, |e| e.0);
        duration = duration.max(clock.secs(end_tick));
        let mut name = String::new();
        let mut programs: [Option<u8>; 16] = [None; 16];
        let mut notes: Vec<Vec<Note>> = vec![Vec::new(); 16];
        let mut ctrl: Vec<Vec<(Controller, ControllerEvent)>> = vec![Vec::new(); 16];
        // Sounding notes by (channel, key): start tick and velocity.
        let mut open: HashMap<(u8, u8), (u64, u8)> = HashMap::new();
        let close = |notes: &mut Vec<Vec<Note>>, (ch, key): (u8, u8), (start, vel): (u64, u8), end: u64| {
            let start_secs = clock.secs(start);
            let length_secs = (clock.secs(end) - start_secs).max(0.01);
            notes[ch as usize].push(Note { start_secs, length_secs, pitch: key, velocity: vel.clamp(1, 127), tab: None });
        };
        for &(tick, kind) in events {
            match kind {
                TrackEventKind::Meta(MetaMessage::TrackName(n)) if name.is_empty() => name = String::from_utf8_lossy(n).trim().to_string(),
                TrackEventKind::Midi { channel, message } => {
                    let ch = channel.as_int();
                    match message {
                        MidiMessage::ProgramChange { program } => {
                            programs[ch as usize].get_or_insert(program.as_int());
                        }
                        // Re-striking a sounding pitch ends the earlier note there, so
                        // same-pitch notes never overlap.
                        MidiMessage::NoteOn { key, vel } if vel > 0 => {
                            if let Some(prev) = open.insert((ch, key.as_int()), (tick, vel.as_int())) {
                                close(&mut notes, (ch, key.as_int()), prev, tick);
                            }
                        }
                        // Note-on with velocity 0 is a note-off.
                        MidiMessage::NoteOff { key, .. } | MidiMessage::NoteOn { key, .. } => {
                            if let Some(prev) = open.remove(&(ch, key.as_int())) {
                                close(&mut notes, (ch, key.as_int()), prev, tick);
                            }
                        }
                        message => {
                            let (kind, raw) = match message {
                                // Channel mode messages (all notes off, reset...) aren't controllers.
                                MidiMessage::Controller { controller, value } if controller < 120 => (Controller::Cc(controller.as_int()), value.as_int() as u16),
                                MidiMessage::PitchBend { bend } => (Controller::PitchBend, bend.0.as_int()),
                                MidiMessage::ChannelAftertouch { vel } => (Controller::ChannelPressure, vel.as_int() as u16),
                                _ => continue,
                            };
                            ctrl[ch as usize].push((kind, ControllerEvent { time_secs: clock.secs(tick), value: kind.from_raw(raw) }));
                        }
                    }
                }
                _ => {}
            }
        }
        // Notes still held at the end of the track end there.
        for (k, v) in open.drain() {
            close(&mut notes, k, v, end_tick);
        }
        let channels: Vec<u8> = (0..16).filter(|&c| !notes[c as usize].is_empty()).collect();
        if channels.is_empty() {
            // A format 1 conductor track's name is the song's title.
            if ti == 0 && smf.tracks.len() > 1 {
                song_name = name;
            }
            continue;
        }
        for &ch in &channels {
            let program = programs[ch as usize];
            let label = if ch == DRUM_CHANNEL { "Drums".to_string() } else { gm_program_name(program.unwrap_or(0)).to_string() };
            let part_name = match (name.is_empty(), channels.len() > 1) {
                (true, _) => label,
                (false, false) => name.clone(),
                (false, true) => format!("{name} — {label}"),
            };
            let part_notes = std::mem::take(&mut notes[ch as usize]);
            duration = part_notes.iter().map(|n| n.start_secs + n.length_secs).fold(duration, f64::max);
            let controllers = lanes(std::mem::take(&mut ctrl[ch as usize]));
            parts.push(Part { name: part_name, channel: ch, program, notes: part_notes, controllers });
        }
    }
    let format = match smf.header.format {
        Format::SingleTrack => 0,
        Format::Parallel => 1,
        Format::Sequential => 2,
    };
    Ok(Song { name: song_name, format, tempos, signatures, parts, markers, duration })
}

/// Group controller events into lanes (by kind).
fn lanes(events: Vec<(Controller, ControllerEvent)>) -> Vec<ControllerLane> {
    let mut kinds: Vec<Controller> = events.iter().map(|e| e.0).collect();
    kinds.sort();
    kinds.dedup();
    kinds.into_iter().map(|k| ControllerLane::new(k, events.iter().filter(|e| e.0 == k).map(|e| e.1).collect())).collect()
}

fn read(path: &Path) -> Result<Song> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&bytes).with_context(|| format!("reading {}", path.display()))
}

fn summarize(song: &Song) -> MidiFileSummary {
    let opening = song.tempo_map(0.0);
    MidiFileSummary {
        name: song.name.clone(),
        format: song.format,
        tempo_bpm: opening.0,
        time_signature: opening.1,
        tempo_changes: opening.2.len(),
        duration_secs: song.duration,
        tracks: song
            .parts
            .iter()
            .enumerate()
            .map(|(i, p)| MidiFileTrackInfo {
                index: i,
                name: p.name.clone(),
                kind: if p.channel == DRUM_CHANNEL { "drums" } else { "pitched" }.into(),
                channel: p.channel + 1,
                program: (p.channel != DRUM_CHANNEL).then(|| gm_program_name(p.program.unwrap_or(0)).to_string()),
                notes: p.notes.len(),
            })
            .collect(),
    }
}

/// Read and summarise a Standard MIDI File.
pub fn inspect(path: &Path) -> Result<MidiFileSummary> {
    Ok(summarize(&read(path)?))
}

impl Engine {
    /// Import a Standard MIDI File: one instrument track with one MIDI region per part
    /// (MIDI track, or channel of a track). Returns the new track indices. One undo step.
    pub fn import_midi_file(&mut self, path: &Path, options: &MidiFileImport) -> Result<Vec<usize>> {
        let song = read(path)?;
        if song.parts.is_empty() {
            bail!("nothing to import: the file has no notes");
        }
        let wanted: Vec<usize> = match &options.tracks {
            Some(list) => {
                if let Some(bad) = list.iter().find(|&&i| i >= song.parts.len()) {
                    bail!("the file has no track {bad} (it has {})", song.parts.len());
                }
                list.clone()
            }
            None => (0..song.parts.len()).collect(),
        };
        if wanted.is_empty() {
            bail!("no tracks selected");
        }
        // Load instruments first (one instance per track), so failures leave the project untouched.
        let mut slots = Vec::new();
        for &i in &wanted {
            let query = match (song.parts[i].channel == DRUM_CHANNEL, &options.drum_instrument) {
                (true, Some(d)) => d.as_str(),
                (true, None) => crate::drums::PATH,
                _ => options.instrument.as_str(),
            };
            slots.push(self.new_slot(query, true)?);
        }
        let song_name = if song.name.is_empty() { path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default() } else { song.name.clone() };
        let start = options.position_secs.max(0.0);
        let length = song.duration.max(0.1);
        let marks: Vec<TempoMark> = song.tempos.iter().map(|&(t, bpm)| TempoMark { start_secs: t, bpm }).collect();

        self.record("Import MIDI File", None, |e| {
            if options.set_tempo {
                let (bpm, sig, changes) = song.tempo_map(start);
                e.project.tempo_bpm = bpm;
                e.project.time_signature = sig;
                e.project.tempo_changes = changes;
                let cmd = e.tempo_command();
                e.send(cmd)?;
            }
            // The song's markers, once however many of its parts come in.
            e.project.merge_markers(song.markers.iter().map(|(t, name)| Marker::new(start + t, if name.is_empty() { "Marker" } else { name })));
            let mut created = Vec::new();
            for (&i, slot) in wanted.iter().zip(slots) {
                let part = &song.parts[i];
                let index = e.add_track_impl(&part.name)?;
                e.project.tracks[index].instrument = Some(slot);
                e.sync_instrument(index)?;
                e.project.tracks[index].midi_clips.push(MidiClip {
                    notes: super::midi::sorted(part.notes.clone()),
                    tempos: marks.clone(),
                    controllers: part.controllers.clone(),
                    ..MidiClip::new(format!("{song_name} — {}", part.name), start, length)
                });
                e.resync_midi(index)?;
                created.push(index);
            }
            Ok(created)
        })
    }

    /// Write the project's MIDI regions to a Standard MIDI File (format 1): a tempo track
    /// with the tempo map, then one track per instrument track with notes and controllers
    /// (drum kits on channel 10). Muted regions are left out. Returns the number of note tracks written.
    pub fn export_midi_file(&self, path: &Path) -> Result<usize> {
        let (bytes, count) = smf_bytes(&self.project)?;
        if count == 0 {
            bail!("the project has no MIDI notes to export");
        }
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
        Ok(count)
    }
}

/// Encode the project's MIDI regions as an SMF; returns the bytes and the note-track count.
fn smf_bytes(project: &Project) -> Result<(Vec<u8>, usize)> {
    let map = project.tempo_map();
    let tick = |secs: f64| (map.quarters_at(secs.max(0.0)) * EXPORT_PPQ as f64).round().max(0.0) as u64;
    let meta = TrackEventKind::Meta;

    // Tempo track: tempo and time signature at every tempo-map segment.
    let mut tempo_events: Vec<(u64, TrackEventKind)> = vec![(0, meta(MetaMessage::TrackName(project.name.as_bytes())))];
    let mut prev_sig = None;
    for seg in map.segments() {
        let at = (seg.start_q * EXPORT_PPQ as f64).round() as u64;
        tempo_events.push((at, meta(MetaMessage::Tempo(u24::new((60e6 / seg.bpm).round() as u32)))));
        if prev_sig != Some(seg.sig) {
            let pow = seg.sig.1.max(1).ilog2() as u8;
            tempo_events.push((at, meta(MetaMessage::TimeSignature(seg.sig.0.min(255) as u8, pow, 24, 8))));
            prev_sig = Some(seg.sig);
        }
    }
    tempo_events.extend(project.markers.iter().map(|m| (tick(m.time_secs), meta(MetaMessage::Marker(m.name.as_bytes())))));
    tempo_events.sort_by_key(|e| e.0);
    let mut tracks = vec![tempo_events];

    let mut next_channel = 0u8;
    for track in project.tracks.iter().filter(|t| t.is_instrument()) {
        let drums = track.instrument.as_ref().is_some_and(|s| s.path == crate::drums::PATH);
        let channel = if drums {
            DRUM_CHANNEL
        } else {
            let c = next_channel;
            next_channel = (next_channel + 1) % 15;
            if c >= DRUM_CHANNEL { c + 1 } else { c }
        };
        let ch = u4::new(channel);
        let mut events: Vec<(u64, TrackEventKind)> = Vec::new();
        for clip in track.midi_clips.iter().filter(|c| !c.mute) {
            for n in clip.notes.iter().filter(|n| n.start_secs < clip.length_secs) {
                let on = tick(clip.position_secs + n.start_secs);
                let end = n.start_secs + n.length_secs.min(clip.length_secs - n.start_secs);
                let off = tick(clip.position_secs + end).max(on + 1);
                let key = u7::new(n.pitch.min(127));
                events.push((on, TrackEventKind::Midi { channel: ch, message: MidiMessage::NoteOn { key, vel: u7::new(n.velocity.clamp(1, 127)) } }));
                events.push((off, TrackEventKind::Midi { channel: ch, message: MidiMessage::NoteOff { key, vel: u7::new(0) } }));
            }
            for lane in &clip.controllers {
                for e in lane.events.iter().filter(|e| e.time_secs < clip.length_secs) {
                    let raw = lane.kind.raw(e.value);
                    let message = match lane.kind {
                        Controller::Cc(n) => MidiMessage::Controller { controller: u7::new(n & 0x7F), value: u7::new(raw as u8) },
                        Controller::PitchBend => MidiMessage::PitchBend { bend: PitchBend(u14::new(raw)) },
                        Controller::ChannelPressure => MidiMessage::ChannelAftertouch { vel: u7::new(raw as u8) },
                    };
                    events.push((tick(clip.position_secs + e.time_secs), TrackEventKind::Midi { channel: ch, message }));
                }
            }
        }
        if !events.iter().any(|(_, k)| matches!(k, TrackEventKind::Midi { message: MidiMessage::NoteOn { .. }, .. })) {
            continue;
        }
        // Note-offs, then controllers, then note-ons at the same tick, so repeated notes
        // re-strike and a bend applies to the note it starts with.
        events.sort_by_key(|(t, k)| {
            let rank = match k {
                TrackEventKind::Midi { message: MidiMessage::NoteOff { .. }, .. } => 0,
                TrackEventKind::Midi { message: MidiMessage::NoteOn { .. }, .. } => 2,
                _ => 1,
            };
            (*t, rank)
        });
        events.insert(0, (0, meta(MetaMessage::TrackName(track.name.as_bytes()))));
        tracks.push(events);
    }
    let count = tracks.len() - 1;

    let mut smf = Smf::new(Header::new(Format::Parallel, Timing::Metrical(u15::new(EXPORT_PPQ))));
    for events in tracks {
        let mut last = 0;
        let mut track: Vec<TrackEvent> = events
            .into_iter()
            .map(|(t, kind)| {
                let delta = u28::new((t - last).min(u28::max_value().as_int() as u64) as u32);
                last = t;
                TrackEvent { delta, kind }
            })
            .collect();
        track.push(TrackEvent { delta: u28::new(0), kind: meta(MetaMessage::EndOfTrack) });
        smf.tracks.push(track);
    }
    let mut bytes = Vec::new();
    smf.write_std(&mut bytes).context("encoding the MIDI file")?;
    Ok((bytes, count))
}

/// General MIDI level 1 program names (0-based program numbers).
pub fn gm_program_name(program: u8) -> &'static str {
    const NAMES: [&str; 128] = [
        "Acoustic Grand Piano", "Bright Acoustic Piano", "Electric Grand Piano", "Honky-tonk Piano", "Electric Piano 1", "Electric Piano 2", "Harpsichord", "Clavinet",
        "Celesta", "Glockenspiel", "Music Box", "Vibraphone", "Marimba", "Xylophone", "Tubular Bells", "Dulcimer",
        "Drawbar Organ", "Percussive Organ", "Rock Organ", "Church Organ", "Reed Organ", "Accordion", "Harmonica", "Tango Accordion",
        "Acoustic Guitar (nylon)", "Acoustic Guitar (steel)", "Electric Guitar (jazz)", "Electric Guitar (clean)", "Electric Guitar (muted)", "Overdriven Guitar", "Distortion Guitar", "Guitar Harmonics",
        "Acoustic Bass", "Electric Bass (finger)", "Electric Bass (pick)", "Fretless Bass", "Slap Bass 1", "Slap Bass 2", "Synth Bass 1", "Synth Bass 2",
        "Violin", "Viola", "Cello", "Contrabass", "Tremolo Strings", "Pizzicato Strings", "Orchestral Harp", "Timpani",
        "String Ensemble 1", "String Ensemble 2", "Synth Strings 1", "Synth Strings 2", "Choir Aahs", "Voice Oohs", "Synth Voice", "Orchestra Hit",
        "Trumpet", "Trombone", "Tuba", "Muted Trumpet", "French Horn", "Brass Section", "Synth Brass 1", "Synth Brass 2",
        "Soprano Sax", "Alto Sax", "Tenor Sax", "Baritone Sax", "Oboe", "English Horn", "Bassoon", "Clarinet",
        "Piccolo", "Flute", "Recorder", "Pan Flute", "Blown Bottle", "Shakuhachi", "Whistle", "Ocarina",
        "Lead 1 (square)", "Lead 2 (sawtooth)", "Lead 3 (calliope)", "Lead 4 (chiff)", "Lead 5 (charang)", "Lead 6 (voice)", "Lead 7 (fifths)", "Lead 8 (bass + lead)",
        "Pad 1 (new age)", "Pad 2 (warm)", "Pad 3 (polysynth)", "Pad 4 (choir)", "Pad 5 (bowed)", "Pad 6 (metallic)", "Pad 7 (halo)", "Pad 8 (sweep)",
        "FX 1 (rain)", "FX 2 (soundtrack)", "FX 3 (crystal)", "FX 4 (atmosphere)", "FX 5 (brightness)", "FX 6 (goblins)", "FX 7 (echoes)", "FX 8 (sci-fi)",
        "Sitar", "Banjo", "Shamisen", "Koto", "Kalimba", "Bagpipe", "Fiddle", "Shanai",
        "Tinkle Bell", "Agogo", "Steel Drums", "Woodblock", "Taiko Drum", "Melodic Tom", "Synth Drum", "Reverse Cymbal",
        "Guitar Fret Noise", "Breath Noise", "Seashore", "Bird Tweet", "Telephone Ring", "Helicopter", "Applause", "Gunshot",
    ];
    NAMES[(program & 0x7f) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentake_project::TempoChange;

    fn midi(channel: u8, message: MidiMessage) -> TrackEventKind<'static> {
        TrackEventKind::Midi { channel: u4::new(channel), message }
    }
    fn on(channel: u8, key: u8, vel: u8) -> TrackEventKind<'static> {
        midi(channel, MidiMessage::NoteOn { key: u7::new(key), vel: u7::new(vel) })
    }
    fn off(channel: u8, key: u8) -> TrackEventKind<'static> {
        midi(channel, MidiMessage::NoteOff { key: u7::new(key), vel: u7::new(0) })
    }

    /// An SMF from tracks of (absolute tick, event).
    fn smf(format: Format, timing: Timing, tracks: Vec<Vec<(u32, TrackEventKind<'static>)>>) -> Vec<u8> {
        let mut smf = Smf::new(Header::new(format, timing));
        for events in tracks {
            let mut last = 0;
            let mut track: Vec<TrackEvent> = events.into_iter().map(|(t, kind)| { let d = t - last; last = t; TrackEvent { delta: u28::new(d), kind } }).collect();
            track.push(TrackEvent { delta: u28::new(0), kind: TrackEventKind::Meta(MetaMessage::EndOfTrack) });
            smf.tracks.push(track);
        }
        let mut bytes = Vec::new();
        smf.write_std(&mut bytes).unwrap();
        bytes
    }

    /// Format 1, 480 ppq: 120 bpm in 4/4, then 60 bpm and 3/4 from bar 2 (2 s); a bass
    /// track and a drum track.
    fn song() -> Vec<u8> {
        let tempo = |bpm: f64| TrackEventKind::Meta(MetaMessage::Tempo(u24::new((60e6 / bpm) as u32)));
        let conductor = vec![
            (0, TrackEventKind::Meta(MetaMessage::TrackName(b"Test Song"))),
            (0, tempo(120.0)),
            (0, TrackEventKind::Meta(MetaMessage::TimeSignature(4, 2, 24, 8))),
            (0, TrackEventKind::Meta(MetaMessage::Marker(b"Intro"))),
            (1920, TrackEventKind::Meta(MetaMessage::Marker(b"Verse"))),
            (1920, tempo(60.0)),
            (1920, TrackEventKind::Meta(MetaMessage::TimeSignature(3, 2, 24, 8))),
        ];
        let bass = vec![
            (0, TrackEventKind::Meta(MetaMessage::TrackName(b"Bass"))),
            (0, midi(1, MidiMessage::ProgramChange { program: u7::new(33) })),
            (0, on(1, 40, 100)),
            (480, on(1, 40, 0)), // velocity 0 = note-off
            (1440, on(1, 43, 90)),
            (2160, off(1, 43)), // across the tempo change: 0.5 s at 120 + 0.5 s at 60
            // Re-struck before its note-off: the first note ends at the second.
            (2880, on(1, 45, 80)),
            (3360, on(1, 45, 70)),
            (3840, off(1, 45)),
            (4320, off(1, 45)),
        ];
        let drums = vec![(0, on(9, 36, 127)), (240, off(9, 36)), (960, on(9, 38, 110)), (1200, off(9, 38))];
        smf(Format::Parallel, Timing::Metrical(u15::new(480)), vec![conductor, bass, drums])
    }

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("opentake-midi-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn parses_tracks_notes_and_tempo_map() {
        let song = parse(&song()).unwrap();
        assert_eq!(song.name, "Test Song");
        assert_eq!(song.tempos, vec![(0.0, 120.0), (2.0, 60.0)]);
        assert_eq!(song.signatures, vec![(0.0, (4, 4)), (2.0, (3, 4))]);
        assert_eq!(song.parts.len(), 2);
        let bass = &song.parts[0];
        assert_eq!((bass.name.as_str(), bass.channel, bass.program), ("Bass", 1, Some(33)));
        let notes: Vec<(f64, f64, u8, u8)> = bass.notes.iter().map(|n| (n.start_secs, n.length_secs, n.pitch, n.velocity)).collect();
        assert_eq!(notes, vec![(0.0, 0.5, 40, 100), (1.5, 1.0, 43, 90), (4.0, 1.0, 45, 80), (5.0, 1.0, 45, 70)]);
        let drums = &song.parts[1];
        assert_eq!((drums.name.as_str(), drums.channel), ("Drums", 9));
        assert_eq!(drums.notes.iter().map(|n| (n.start_secs, n.pitch)).collect::<Vec<_>>(), vec![(0.0, 36), (1.0, 38)]);
        assert!((song.duration - 7.0).abs() < 1e-9);

        let s = summarize(&song);
        assert_eq!((s.format, s.tempo_bpm, s.time_signature, s.tempo_changes), (1, 120.0, (4, 4), 1));
        assert_eq!(s.tracks[0].program.as_deref(), Some("Electric Bass (finger)"));
        assert_eq!((s.tracks[1].kind.as_str(), s.tracks[1].channel, s.tracks[1].program.as_deref()), ("drums", 10, None));
    }

    #[test]
    fn format_0_splits_channels_and_smpte_timing_ignores_tempo() {
        let track = vec![
            (0, TrackEventKind::Meta(MetaMessage::Tempo(u24::new(250_000)))),
            (0, midi(0, MidiMessage::ProgramChange { program: u7::new(0) })),
            (0, on(0, 60, 100)),
            (0, on(9, 42, 100)),
            (100, off(9, 42)),
            (200, off(0, 60)),
        ];
        // 25 fps × 40 subframes = 1000 ticks per second.
        let bytes = smf(Format::SingleTrack, Timing::Timecode(midly::Fps::Fps25, 40), vec![track]);
        let song = parse(&bytes).unwrap();
        assert_eq!(song.format, 0);
        let parts: Vec<(&str, u8, f64)> = song.parts.iter().map(|p| (p.name.as_str(), p.channel, p.notes[0].length_secs)).collect();
        assert_eq!(parts, vec![("Acoustic Grand Piano", 0, 0.2), ("Drums", 9, 0.1)]);
        assert_eq!(song.tempos, vec![(0.0, 240.0)], "tempo is still reported");
    }

    #[test]
    fn imports_parts_as_instrument_tracks_in_one_undo_step() {
        let path = write("song.mid", &song());
        let mut e = Engine::new();
        e.add_track("Existing").unwrap();
        let created = e.import_midi_file(&path, &MidiFileImport::default()).unwrap();
        assert_eq!(created, vec![1, 2]);
        let p = e.project();
        assert_eq!((p.tempo_bpm, p.time_signature), (120.0, (4, 4)));
        assert_eq!(p.tempo_changes, vec![TempoChange { at_secs: 2.0, bpm: 60.0, time_signature: Some((3, 4)) }]);
        assert_eq!(p.tracks[1].name, "Bass");
        assert!(p.tracks[1].is_instrument());
        let clip = &p.tracks[1].midi_clips[0];
        assert_eq!(clip.name, "Test Song — Bass");
        assert_eq!(clip.notes.len(), 4);
        assert_eq!(p.tracks[2].instrument.as_ref().map(|s| s.name.as_str()), Some(crate::drums::NAME), "channel 10 gets the drum kit");
        assert!(!e.song_tempo_differs());
        assert_eq!(p.markers.iter().map(|m| (m.time_secs, m.name.as_str())).collect::<Vec<_>>(), vec![(0.0, "Intro"), (2.0, "Verse")]);
        assert_eq!(e.history_info().undo.as_deref(), Some("Import MIDI File"));
        e.undo().unwrap();
        assert_eq!(e.project().tracks.len(), 1, "undo removes every imported track");
        assert!(e.project().tempo_changes.is_empty(), "and the tempo map");

        // Selected parts only, at a position, keeping the project tempo.
        let opts = MidiFileImport { tracks: Some(vec![1]), set_tempo: false, position_secs: 4.0, ..Default::default() };
        let created = e.import_midi_file(&path, &opts).unwrap();
        assert_eq!(e.project().tracks[created[0]].name, "Drums");
        assert_eq!(e.project().tracks[created[0]].midi_clips[0].position_secs, 4.0);
        assert!(e.project().tempo_changes.is_empty());
        assert_eq!(e.project().markers.iter().map(|m| m.time_secs).collect::<Vec<_>>(), vec![4.0, 6.0], "markers move with the song");
        e.import_midi_file(&path, &opts).unwrap();
        assert_eq!(e.project().markers.len(), 2, "importing another part doesn't repeat them");
        let bad = MidiFileImport { tracks: Some(vec![5]), ..Default::default() };
        assert!(e.import_midi_file(&path, &bad).is_err());
        assert!(inspect(&write("junk.mid", b"not midi")).is_err());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn export_round_trips_notes_and_tempo_map() {
        let path = write("in.mid", &song());
        let mut e = Engine::new();
        e.import_midi_file(&path, &MidiFileImport::default()).unwrap();
        let out = std::env::temp_dir().join(format!("opentake-midi-{}-out.mid", std::process::id()));
        assert_eq!(e.export_midi_file(&out).unwrap(), 2);
        let again = read(&out).unwrap();
        assert_eq!(again.tempos, vec![(0.0, 120.0), (2.0, 60.0)]);
        assert_eq!(again.signatures, vec![(0.0, (4, 4)), (2.0, (3, 4))]);
        assert_eq!(again.markers, vec![(0.0, "Intro".to_string()), (2.0, "Verse".to_string())]);
        let original = parse(&song()).unwrap();
        for (a, b) in again.parts.iter().zip(&original.parts) {
            assert_eq!((a.name.as_str(), a.channel == DRUM_CHANNEL), (b.name.as_str(), b.channel == DRUM_CHANNEL));
            let times = |p: &Part| p.notes.iter().map(|n| (n.pitch, (n.start_secs * 1e3).round(), (n.length_secs * 1e3).round(), n.velocity)).collect::<Vec<_>>();
            assert_eq!(times(a), times(b));
        }
        assert!(Engine::new().export_midi_file(&out).is_err(), "nothing to export");
        std::fs::remove_file(path).ok();
        std::fs::remove_file(out).ok();
    }

    #[test]
    fn controllers_round_trip_through_a_file() {
        let bend = |raw: u16| midi(0, MidiMessage::PitchBend { bend: PitchBend(u14::new(raw)) });
        let cc = |n: u8, v: u8| midi(0, MidiMessage::Controller { controller: u7::new(n), value: u7::new(v) });
        let track = vec![
            (0, TrackEventKind::Meta(MetaMessage::Tempo(u24::new(500_000)))),
            (0, cc(7, 100)),
            (0, on(0, 60, 100)),
            (240, bend(12288)),
            (480, bend(8192)),
            (480, cc(64, 127)),
            (480, cc(123, 0)), // all notes off: not a controller
            (720, midi(0, MidiMessage::ChannelAftertouch { vel: u7::new(80) })),
            (960, off(0, 60)),
            (960, cc(64, 0)),
        ];
        let song = parse(&smf(Format::SingleTrack, Timing::Metrical(u15::new(480)), vec![track])).unwrap();
        let summary = |lanes: &[ControllerLane]| lanes.iter().map(|l| (l.kind, l.events.iter().map(|e| (e.time_secs, l.kind.raw(e.value))).collect::<Vec<_>>())).collect::<Vec<_>>();
        let want = vec![
            (Controller::PitchBend, vec![(0.25, 12288), (0.5, 8192)]),
            (Controller::Cc(7), vec![(0.0, 100)]),
            (Controller::SUSTAIN, vec![(0.5, 127), (1.0, 0)]),
            (Controller::ChannelPressure, vec![(0.75, 80)]),
        ];
        assert_eq!(summary(&song.parts[0].controllers), want);

        let path = write("cc.mid", &smf(Format::SingleTrack, Timing::Metrical(u15::new(480)), vec![vec![(0, on(0, 60, 100)), (240, bend(12288)), (480, cc(1, 64)), (960, off(0, 60))]]));
        let mut e = Engine::new();
        e.import_midi_file(&path, &MidiFileImport::default()).unwrap();
        let clip = &e.project().tracks[0].midi_clips[0];
        assert_eq!(clip.lane(Controller::PitchBend).map(|l| l.events.len()), Some(1));
        e.set_midi_controllers(0, 0, Controller::ChannelPressure, vec![ControllerEvent { time_secs: 0.75, value: 1.0 }], true).unwrap();
        let out = std::env::temp_dir().join(format!("opentake-midi-{}-cc-out.mid", std::process::id()));
        e.export_midi_file(&out).unwrap();
        let again = read(&out).unwrap();
        assert_eq!(summary(&again.parts[0].controllers), summary(&e.project().tracks[0].midi_clips[0].controllers));
        std::fs::remove_file(path).ok();
        std::fs::remove_file(out).ok();
    }

    #[test]
    fn recognises_midi_paths() {
        assert!(is_midi_file_path(Path::new("song.MID")) && is_midi_file_path(Path::new("a.midi")));
        assert!(!is_midi_file_path(Path::new("a.gp")));
    }
}
