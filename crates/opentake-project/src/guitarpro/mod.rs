//! Guitar Pro 7/8 import (`.gp`, or a bare `score.gpif`).
//!
//! Ported from GuitarCraft's TypeScript reader: the GPIF document is parsed into a small
//! score model ([`Score`]) and then flattened into timed note events ([`timeline`]) with
//! repeats, tempo changes, ties, let-ring, tremolo, triplet feel, grace notes and brush
//! strokes applied. OpenTake turns those events into MIDI regions.

mod gpif;
pub mod timeline;

use std::io::Read;
use std::path::Path;

use thiserror::Error;

pub use timeline::{NoteEvent, Performance};

/// Ticks per quarter note. Divisible by 2, 3, 5 and 7, so common tuplets stay integral.
pub const TICKS_PER_QUARTER: u32 = 960;

#[derive(Debug, Error)]
pub enum GpError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not a Guitar Pro 7+ file: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid GPIF XML: {0}")]
    Xml(#[from] roxmltree::Error),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub master_bars: Vec<MasterBar>,
    pub tracks: Vec<Track>,
    pub tempos: Vec<TempoChange>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TempoChange {
    pub bar: usize,
    /// Position inside the bar, as a fraction 0..1 of the bar length.
    pub position: f64,
    /// Quarter notes per minute.
    pub bpm: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MasterBar {
    pub numerator: u32,
    pub denominator: u32,
    /// Bar length in ticks.
    pub duration: u32,
    pub section: Option<String>,
    pub repeat_start: bool,
    /// Total number of times the repeat is played (0 = no repeat end here).
    pub repeat_count: u32,
    /// Repeat passes (1-based) on which this bar is played; empty = always.
    pub alternate_endings: Vec<u32>,
    pub triplet_feel: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Guitar,
    Bass,
    Drums,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub name: String,
    pub kind: TrackKind,
    /// MIDI pitches of the open strings, lowest first. Empty for drums.
    pub tuning: Vec<u8>,
    pub tuning_name: String,
    pub capo: u8,
    pub midi_program: u8,
    /// One bar per master bar.
    pub bars: Vec<Bar>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Bar {
    pub voices: Vec<Voice>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Voice {
    pub beats: Vec<Beat>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Beat {
    /// Offset from the bar start, in ticks.
    pub start: u32,
    pub duration: u32,
    pub tuplet: Option<(u32, u32)>,
    pub notes: Vec<Note>,
    /// Tremolo picking subdivision as a note value (8, 16, 32), if any.
    pub tremolo: Option<u32>,
    /// MIDI velocity.
    pub dynamic: u8,
    /// Strum direction: `Some(true)` = up, `Some(false)` = down.
    pub brush_up: Option<bool>,
    pub grace: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// String index, 0 = lowest string (meaningless for drums).
    pub string: u8,
    pub fret: u8,
    /// Concert MIDI pitch (GM drum key on drum tracks).
    pub midi: u8,
    pub tie_origin: bool,
    pub tie_destination: bool,
    pub palm_mute: bool,
    pub dead: bool,
    pub let_ring: bool,
    pub accent: bool,
    pub ghost: bool,
    /// Bend curve as (offset 0..1 of the note, semitones). Not played back yet.
    pub bend: Vec<(f64, f64)>,
}

/// Parse a Guitar Pro file from bytes (`.gp` ZIP container or raw GPIF XML).
pub fn parse(data: &[u8]) -> Result<Score, GpError> {
    if data.starts_with(b"PK") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data))?;
        let mut xml = String::new();
        zip.by_name("Content/score.gpif")
            .map_err(|_| GpError::Invalid("this ZIP file is not a Guitar Pro 7+ file (Content/score.gpif missing)".into()))?
            .read_to_string(&mut xml)?;
        return gpif::parse(&xml);
    }
    let head = &data[..data.len().min(64)];
    if head.windows(18).any(|w| w == b"FICHIER GUITAR PRO") {
        return Err(GpError::Unsupported("Guitar Pro 3-5 files (.gp3/.gp4/.gp5) aren't supported yet; save as .gp (Guitar Pro 7+)".into()));
    }
    if data.starts_with(b"BCFZ") || data.starts_with(b"BCFS") {
        return Err(GpError::Unsupported("Guitar Pro 6 files (.gpx) aren't supported yet; save as .gp (Guitar Pro 7+)".into()));
    }
    let text = std::str::from_utf8(data).map_err(|_| GpError::Invalid("unrecognised file format".into()))?;
    gpif::parse(text)
}

pub fn load(path: impl AsRef<Path>) -> Result<Score, GpError> {
    parse(&std::fs::read(path)?)
}

/// File extensions this module can import.
pub fn is_guitar_pro_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "gp" | "gpif"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Score {
        load(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/simple.gp")).unwrap()
    }

    #[test]
    fn reads_metadata_tracks_and_bars() {
        let score = fixture();
        assert_eq!(score.title, "Fixture Song");
        assert_eq!(score.artist, "Test & Band");
        assert_eq!(score.master_bars.len(), 4);
        assert_eq!(score.master_bars[0].section.as_deref(), Some("A Intro"));
        let tracks: Vec<_> = score.tracks.iter().map(|t| (t.name.as_str(), t.kind, t.midi_program)).collect();
        assert_eq!(tracks, vec![("Guitar", TrackKind::Guitar, 30), ("Drums", TrackKind::Drums, 0)]);
        assert_eq!(score.tracks[0].tuning, vec![40, 45, 50, 55, 59, 64]);
        assert_eq!(score.tracks[0].tuning_name, "Standard");
        assert_eq!(score.tempos.iter().map(|t| t.bpm).collect::<Vec<_>>(), vec![120.0, 60.0]);
    }

    #[test]
    fn computes_durations_with_tuplets_and_dots() {
        let score = fixture();
        let beats = &score.tracks[0].bars[2].voices[0].beats;
        assert_eq!(beats.iter().map(|b| b.duration).collect::<Vec<_>>(), vec![320, 320, 320, 3 * TICKS_PER_QUARTER]);
        assert_eq!(beats.iter().map(|b| b.start).collect::<Vec<_>>(), vec![0, 320, 640, 960]);
        assert_eq!(beats[0].tuplet, Some((3, 2)));
    }

    #[test]
    fn reads_note_details() {
        let score = fixture();
        let bar0 = &score.tracks[0].bars[0].voices[0].beats;
        let (a, b) = (&bar0[0].notes[0], &bar0[1].notes[0]);
        assert_eq!((a.fret, a.string, a.midi, a.tie_origin, a.palm_mute), (5, 0, 45, true, true));
        assert!(b.tie_destination);
        let bent = &score.tracks[0].bars[2].voices[0].beats[3].notes[0];
        assert_eq!(bent.bend.last().map(|p| p.1), Some(2.0), "100 = full = 2 semitones");
        assert!(score.tracks[0].bars[3].voices[0].beats[0].notes[0].let_ring);
        // A note without a Midi property is derived from tuning + fret.
        assert_eq!(score.tracks[0].bars[2].voices[0].beats[0].notes[0].midi, 45 + 7);
    }

    #[test]
    fn raw_gpif_parses_the_same_as_the_zip() {
        let raw = load(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/simple.gpif")).unwrap();
        assert_eq!(raw, fixture());
    }

    #[test]
    fn rejects_older_formats_clearly() {
        let gp5 = b"\x18FICHIER GUITAR PRO v5.00";
        assert!(matches!(parse(gp5), Err(GpError::Unsupported(m)) if m.contains("Guitar Pro 3-5")));
        assert!(matches!(parse(b"BCFZ...."), Err(GpError::Unsupported(m)) if m.contains("Guitar Pro 6")));
    }
}
