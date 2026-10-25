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
    let first_bpm = perf.tempos.first().map_or(120.0, |t| t.1);
    let first_sig = score.master_bars.first().map_or((4, 4), |m| (m.numerator, m.denominator));
    tempo_map_from_points(points, first_bpm, first_sig, start)
}

/// Opening tempo/signature and changes from song-relative points (time, tempo change,
/// signature change), placed at `start` seconds on the timeline.
fn tempo_map_from_points(
    mut points: Vec<(f64, Option<f64>, Option<(u32, u32)>)>,
    first_bpm: f64,
    first_sig: (u32, u32),
    start: f64,
) -> (f64, (u32, u32), Vec<TempoChange>) {
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut bpm, mut changes): (f64, Vec<TempoChange>) = (first_bpm, Vec::new());
    if start > 1e-9 {
        // The song's first bar starts at its position on the timeline.
        changes.push(TempoChange { at_secs: start, bpm: first_bpm.clamp(20.0, 999.0), time_signature: Some(first_sig) });
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

impl Engine {
    /// True when an imported song's own tempo (kept on its regions) differs from the
    /// project's tempo map, e.g. it was imported without adopting it.
    pub fn song_tempo_differs(&self) -> bool {
        let map = self.project.tempo_map();
        self.project.tracks.iter().flat_map(|t| &t.midi_clips).any(|c| {
            c.tempos.iter().filter(|m| m.start_secs < c.length_secs).any(|m| (map.tempo_at(c.position_secs + m.start_secs + 1e-6) - m.bpm).abs() > 0.05)
        })
    }

    /// Make the project's tempo map follow an imported song's tempo (from its regions'
    /// tempo marks and tab bars), so bars, grid and metronome match the song. One undo step.
    /// Returns false when there's no song tempo to adopt.
    pub fn adopt_song_tempo(&mut self) -> Result<bool> {
        let Some((position, bpm, sig, changes)) = song_tempo_of(&self.project) else {
            return Ok(false);
        };
        self.record("Use Song Tempo", None, |e| {
            if position <= 1e-9 {
                e.project.tempo_bpm = bpm;
                e.project.time_signature = sig;
            }
            e.project.tempo_changes = changes;
            let cmd = e.tempo_command();
            e.send(cmd)
        })?;
        Ok(true)
    }
}

/// An imported song's tempo map from its regions (tempo marks and tab bars):
/// (song position, opening tempo, opening signature, changes).
fn song_tempo_of(project: &opentake_project::Project) -> Option<(f64, f64, (u32, u32), Vec<TempoChange>)> {
    // Prefer a region with tab bars (they carry the time signatures).
    let regions = project.tracks.iter().flat_map(|t| &t.midi_clips).filter(|c| !c.tempos.is_empty());
    let clip = regions.clone().find(|c| c.tab.is_some()).or_else(|| regions.clone().next())?;
    {
        let mut points: Vec<(f64, Option<f64>, Option<(u32, u32)>)> = clip.tempos.iter().map(|m| (m.start_secs, Some(m.bpm), None)).collect();
        let mut first_sig = project.time_signature;
        if let Some(tab) = &clip.tab {
            let mut prev = None;
            for bar in &tab.bars {
                let sig = (bar.numerator, bar.denominator);
                if prev.is_none() {
                    first_sig = sig;
                }
                if prev != Some(sig) {
                    points.push((bar.start_secs, None, Some(sig)));
                }
                prev = Some(sig);
            }
        }
        let first_bpm = clip.tempos[0].bpm;
        let (bpm, sig, changes) = tempo_map_from_points(points, first_bpm, first_sig, clip.position_secs);
        Some((clip.position_secs, bpm, sig, changes))
    }
}

/// Projects saved before OpenTake had a tempo map could only adopt a song's opening tempo.
/// When a loaded project looks like that (no tempo changes, the opening tempo equals a song
/// region's at the start, and the song has changes), give it the song's whole tempo map.
/// Returns true when the project was changed.
pub(super) fn upgrade_song_tempo(project: &mut opentake_project::Project) -> bool {
    if !project.tempo_changes.is_empty() {
        return false;
    }
    let Some((position, bpm, sig, changes)) = song_tempo_of(project) else { return false };
    if position > 1e-9 || (bpm - project.tempo_bpm).abs() > 0.05 || changes.is_empty() {
        return false;
    }
    project.time_signature = sig;
    project.tempo_changes = changes;
    true
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
    fn adopts_the_song_tempo_of_a_song_imported_without_it() {
        let mut e = Engine::new();
        let opts = GuitarProImport { set_tempo: false, ..Default::default() };
        e.import_guitar_pro(Path::new(FIXTURE), &opts).unwrap();
        assert!(e.project().tempo_changes.is_empty());
        assert!(e.song_tempo_differs(), "the song slows to 60 at 8 s; the project stays at 120");
        assert!(e.adopt_song_tempo().unwrap());
        assert!(!e.song_tempo_differs());
        assert_eq!(e.project().tempo_map().tempo_at(9.0), 60.0);
        assert_eq!(e.history_info().undo.as_deref(), Some("Use Song Tempo"));
    }

    #[test]
    fn projects_from_before_the_tempo_map_get_the_song_tempo_on_load() {
        let dir = std::env::temp_dir().join(format!("opentake-upgrade-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Like an old save: the song's opening tempo adopted, no tempo changes.
        let mut e = Engine::new();
        e.import_guitar_pro(Path::new(FIXTURE), &GuitarProImport::default()).unwrap();
        e.set_tempo_changes(Vec::new(), None).unwrap();
        let old = dir.join("old.otp");
        e.save_project(Some(&old)).unwrap();
        let mut fresh = Engine::new();
        let warnings = fresh.load_project(&old).unwrap();
        assert!(warnings.iter().any(|w| w.contains("tempo")), "{warnings:?}");
        assert_eq!(fresh.project().tempo_map().tempo_at(9.0), 60.0);
        assert!(fresh.history_info().undo.is_none(), "part of loading, not an edit");

        // A project with its own, different tempo is left alone.
        e.set_tempo(100.0, None).unwrap();
        e.save_project(Some(&old)).unwrap();
        fresh.load_project(&old).unwrap();
        assert!(fresh.project().tempo_changes.is_empty());
        std::fs::remove_dir_all(&dir).ok();
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
