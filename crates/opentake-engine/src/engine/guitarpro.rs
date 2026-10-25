//! Import Guitar Pro songs as instrument tracks with MIDI regions.

use std::path::Path;

use anyhow::{Context, Result, bail};
use opentake_project::guitarpro::{self, Score, TrackKind, timeline};
use opentake_project::{MidiClip, Note, TabBar, TabLayout, TabPos, TempoChange, TempoMark};
use serde::Serialize;

use super::Engine;

/// What to import and how.
#[derive(Debug, Clone)]
pub struct GuitarProImport {
    /// Guitar Pro track indices to import; `None` = every track that has notes.
    pub tracks: Option<Vec<usize>>,
    /// Instrument for pitched tracks: "synth" or a VST3 instrument name / bundle path.
    pub instrument: String,
    /// Instrument for drum tracks (GM drum map); defaults to the built-in drum kit.
    pub drum_instrument: Option<String>,
    /// Adopt the song's opening tempo and time signature.
    pub set_tempo: bool,
    /// Where the song starts on the timeline (seconds).
    pub position_secs: f64,
}

impl Default for GuitarProImport {
    fn default() -> Self {
        Self { tracks: None, instrument: "synth".into(), drum_instrument: None, set_tempo: true, position_secs: 0.0 }
    }
}

/// Summary of a Guitar Pro file, for choosing what to import.
#[derive(Debug, Clone, Serialize)]
pub struct GuitarProSummary {
    pub title: String,
    pub artist: String,
    pub tempo_bpm: f64,
    pub time_signature: (u32, u32),
    pub duration_secs: f64,
    pub tracks: Vec<GuitarProTrackInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GuitarProTrackInfo {
    pub index: usize,
    pub name: String,
    pub kind: String,
    pub tuning: Vec<u8>,
    pub notes: usize,
}

fn kind_name(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Guitar => "guitar",
        TrackKind::Bass => "bass",
        TrackKind::Drums => "drums",
        TrackKind::Other => "other",
    }
}

/// The song's tempo map placed at `start` seconds: opening tempo and signature, plus the
/// changes (tempo changes and time-signature changes in playing order).
fn song_tempo_map(score: &Score, perf: &timeline::Performance, start: f64) -> (f64, (u32, u32), Vec<TempoChange>) {
    let mut points: Vec<(f64, Option<f64>, Option<(u32, u32)>)> = perf.tempos.iter().map(|&(t, bpm)| (t, Some(bpm), None)).collect();
    let mut prev_sig = None;
    for occ in &perf.bars {
        let mb = &score.master_bars[occ.bar];
        let sig = (mb.numerator, mb.denominator);
        if prev_sig != Some(sig) {
            points.push((occ.time, None, Some(sig)));
        }
        prev_sig = Some(sig);
    }
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let first_bpm = perf.tempos.first().map_or(120.0, |t| t.1);
    let first_sig = score.master_bars.first().map_or((4, 4), |m| (m.numerator, m.denominator));
    let (mut bpm, mut changes): (f64, Vec<TempoChange>) = (first_bpm, Vec::new());
    if start > 1e-9 {
        // The song's first bar starts at the import position.
        changes.push(TempoChange { at_secs: start, bpm: first_bpm, time_signature: Some(first_sig) });
    }
    let mut i = 0;
    while i < points.len() {
        let t = points[i].0;
        let mut sig = None;
        while i < points.len() && (points[i].0 - t).abs() < 1e-6 {
            bpm = points[i].1.unwrap_or(bpm);
            sig = points[i].2.or(sig);
            i += 1;
        }
        if t > 1e-9 {
            changes.push(TempoChange { at_secs: start + t, bpm: bpm.clamp(20.0, 999.0), time_signature: sig });
        }
    }
    (first_bpm.clamp(20.0, 999.0), first_sig, changes)
}

pub fn summarize(score: &Score) -> GuitarProSummary {
    let perf = timeline::build_performance(score);
    let first = score.master_bars.first();
    GuitarProSummary {
        title: score.title.clone(),
        artist: score.artist.clone(),
        tempo_bpm: score.tempos.first().map_or(120.0, |t| t.bpm),
        time_signature: first.map_or((4, 4), |m| (m.numerator, m.denominator)),
        duration_secs: perf.duration,
        tracks: score
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| GuitarProTrackInfo {
                index: i,
                name: t.name.clone(),
                kind: kind_name(t.kind).into(),
                tuning: t.tuning.clone(),
                notes: perf.events.iter().filter(|e| e.track == i).count(),
            })
            .collect(),
    }
}

/// Read and summarise a Guitar Pro file.
pub fn inspect(path: &Path) -> Result<GuitarProSummary> {
    let score = guitarpro::load(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(summarize(&score))
}

impl Engine {
    /// Import a Guitar Pro 7/8 file: one instrument track with one MIDI region per
    /// imported Guitar Pro track. Returns the new track indices. One undo step.
    pub fn import_guitar_pro(&mut self, path: &Path, options: &GuitarProImport) -> Result<Vec<usize>> {
        let score = guitarpro::load(path).with_context(|| format!("reading {}", path.display()))?;
        let perf = timeline::build_performance(&score);
        let wanted: Vec<usize> = match &options.tracks {
            Some(list) => {
                if let Some(bad) = list.iter().find(|&&i| i >= score.tracks.len()) {
                    bail!("the song has no track {bad} (it has {})", score.tracks.len());
                }
                list.clone()
            }
            None => (0..score.tracks.len()).filter(|&i| perf.events.iter().any(|e| e.track == i)).collect(),
        };
        if wanted.is_empty() {
            bail!("nothing to import: the song has no notes");
        }

        // Load instruments first (one instance per track), so failures leave the project untouched.
        let mut slots = Vec::new();
        for &i in &wanted {
            let query = match (score.tracks[i].kind, &options.drum_instrument) {
                (TrackKind::Drums, Some(d)) => d.as_str(),
                (TrackKind::Drums, None) => crate::drums::PATH,
                _ => options.instrument.as_str(),
            };
            slots.push(self.new_slot(query, true)?);
        }

        let song_name = if score.title.is_empty() {
            path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            score.title.clone()
        };
        let start = options.position_secs.max(0.0);
        let length = perf.events.iter().map(|e| e.time + e.duration).fold(perf.duration, f64::max).max(0.1);
        // Bar layout for the tablature view (repeats expanded, real-time positions).
        let tab_bars: Vec<TabBar> = perf
            .bars
            .iter()
            .map(|occ| {
                let mb = &score.master_bars[occ.bar];
                TabBar { start_secs: occ.time, number: occ.bar as u32 + 1, numerator: mb.numerator, denominator: mb.denominator, section: mb.section.clone() }
            })
            .collect();

        self.record("Import Guitar Pro", None, |e| {
            if options.set_tempo {
                // Adopt the song's whole tempo map (tempo and time-signature changes).
                let (bpm, sig, changes) = song_tempo_map(&score, &perf, start);
                e.project.tempo_bpm = bpm;
                e.project.time_signature = sig;
                e.project.tempo_changes = changes;
                let cmd = e.tempo_command();
                e.send(cmd)?;
            }
            let mut created = Vec::new();
            for (&gp_track, slot) in wanted.iter().zip(slots) {
                let gp = &score.tracks[gp_track];
                let tabbed = gp.kind != TrackKind::Drums && !gp.tuning.is_empty();
                let notes: Vec<Note> = perf
                    .events
                    .iter()
                    .filter(|ev| ev.track == gp_track)
                    .map(|ev| Note {
                        start_secs: ev.time,
                        length_secs: ev.duration.max(0.01),
                        pitch: ev.midi.min(127),
                        velocity: ev.velocity.clamp(1, 127),
                        tab: tabbed.then_some(TabPos { string: ev.string, fret: ev.fret }),
                    })
                    .collect();
                let tab = tabbed.then(|| TabLayout { tuning: gp.tuning.clone(), capo: gp.capo, bars: tab_bars.clone() });
                let name = score.tracks[gp_track].name.clone();
                let index = e.add_track_impl(&name)?;
                e.project.tracks[index].instrument = Some(slot);
                e.sync_instrument(index)?;
                e.project.tracks[index].midi_clips.push(MidiClip {
                    name: format!("{song_name} — {name}"),
                    position_secs: start,
                    length_secs: length,
                    mute: false,
                    notes: super::midi::sorted(notes),
                    tab,
                    tempos: perf.tempos.iter().map(|&(t, bpm)| TempoMark { start_secs: t, bpm }).collect(),
                });
                e.resync_midi(index)?;
                created.push(index);
            }
            Ok(created)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../opentake-project/tests/fixtures/simple.gp");

    #[test]
    fn inspects_tracks() {
        let s = inspect(Path::new(FIXTURE)).unwrap();
        assert_eq!(s.title, "Fixture Song");
        assert_eq!(s.tracks.len(), 2);
        assert_eq!((s.tracks[0].name.as_str(), s.tracks[0].kind.as_str()), ("Guitar", "guitar"));
        assert_eq!(s.tracks[1].kind, "drums");
        assert!((s.duration_secs - 12.0).abs() < 1e-9);
    }

    #[test]
    fn imports_tracks_as_instrument_tracks_in_one_undo_step() {
        let mut e = Engine::new();
        e.add_track("Existing").unwrap();
        let created = e.import_guitar_pro(Path::new(FIXTURE), &GuitarProImport::default()).unwrap();
        assert_eq!(created, vec![1, 2]);
        let p = e.project();
        assert_eq!(p.tempo_bpm, 120.0);
        // The song's tempo map is adopted: 120 bpm, slowing to 60 where bar 4 starts (8 s).
        assert_eq!(p.tempo_changes, vec![TempoChange { at_secs: 8.0, bpm: 60.0, time_signature: None }]);
        let map = p.tempo_map();
        assert_eq!(map.position(8.0).bar, 5, "bar 4 is the fifth bar played (after the repeat)");
        assert_eq!(map.tempo_at(9.0), 60.0);
        let guitar = &p.tracks[1];
        assert_eq!(guitar.name, "Guitar");
        assert!(guitar.is_instrument());
        let clip = &guitar.midi_clips[0];
        assert_eq!(clip.name, "Fixture Song — Guitar");
        // The tied note is one 2 s note on each pass through the repeated bar.
        let tied: Vec<(f64, f64)> = clip.notes.iter().filter(|n| n.pitch == 45).map(|n| (n.start_secs, n.length_secs)).take(2).collect();
        assert_eq!(tied, vec![(0.0, 2.0), (4.0, 2.0)]);
        assert_eq!(p.tracks[2].midi_clips[0].notes.len(), 2, "drum hits");
        assert_eq!(p.tracks[2].instrument.as_ref().map(|s| s.name.as_str()), Some(crate::drums::NAME), "drums default to the drum kit");
        // Tablature: string/fret kept on the notes, bars in playing order, no tab for drums.
        let tab = clip.tab.as_ref().expect("guitar region has a tab layout");
        assert_eq!(tab.tuning, vec![40, 45, 50, 55, 59, 64]);
        assert_eq!(tab.bars.iter().map(|b| (b.number, b.start_secs)).collect::<Vec<_>>(), vec![(1, 0.0), (2, 2.0), (1, 4.0), (3, 6.0), (4, 8.0)]);
        assert_eq!(tab.bars[0].section.as_deref(), Some("A Intro"));
        assert_eq!(clip.notes.iter().find(|n| n.pitch == 45).and_then(|n| n.tab), Some(TabPos { string: 0, fret: 5 }));
        assert!(p.tracks[2].midi_clips[0].tab.is_none());
        // The song's tempo map comes along on every region, drums included.
        assert_eq!(p.tracks[2].midi_clips[0].tempos.len(), 2);
        assert_eq!(p.song_tempo_at(1.0), Some(120.0));
        assert_eq!(p.song_tempo_at(9.0), Some(60.0));
        assert_eq!(p.song_tempo_at(20.0), None, "past the song: project tempo");

        assert_eq!(e.history_info().undo.as_deref(), Some("Import Guitar Pro"));
        e.undo().unwrap();
        assert_eq!(e.project().tracks.len(), 1, "undo removes every imported track");
    }

    #[test]
    fn imports_selected_tracks_only_and_validates_indices() {
        let mut e = Engine::new();
        let opts = GuitarProImport { tracks: Some(vec![1]), set_tempo: false, position_secs: 4.0, ..Default::default() };
        let created = e.import_guitar_pro(Path::new(FIXTURE), &opts).unwrap();
        assert_eq!(created.len(), 1);
        assert_eq!(e.project().tracks[0].name, "Drums");
        assert_eq!(e.project().tracks[0].midi_clips[0].position_secs, 4.0);
        let bad = GuitarProImport { tracks: Some(vec![7]), ..Default::default() };
        assert!(e.import_guitar_pro(Path::new(FIXTURE), &bad).is_err());
    }
}
