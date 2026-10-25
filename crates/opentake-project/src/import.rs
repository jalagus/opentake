//! Opening other DAWs' projects: DAWproject (`.dawproject`), Ableton Live (`.als`), Final
//! Cut Pro XML (`.fcpxml`, which Logic Pro exports) and Ardour (`.ardour`). Each importer
//! returns the project plus warnings for what couldn't be carried over; missing media is
//! a warning, never a failure.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::model::{Clip, PluginFormat, PluginSlot, Project};
use crate::tempo::TempoChange;
use crate::{ableton, ardour, dawproject, fcpxml};

/// Extensions `load` understands.
pub const EXTENSIONS: &[&str] = &["dawproject", "als", "fcpxml", "fcpxmld", "ardour"];

#[derive(Debug, Error)]
pub enum ImportError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("not a valid archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid XML: {0}")]
    Xml(#[from] roxmltree::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub project: Project,
    pub warnings: Vec<String>,
}

pub fn is_import_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Import a project by its extension. Embedded media (DAWproject) is extracted next to it.
/// The project is named after the file unless the file names it.
pub fn load(path: &Path) -> Result<Imported, ImportError> {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match ext.as_str() {
        "dawproject" => dawproject::load(path),
        "als" => ableton::load(path),
        "fcpxml" | "fcpxmld" => fcpxml::load(path),
        "ardour" => ardour::load(path),
        _ => Err(ImportError::Invalid(format!("unknown project format: {}", path.display()))),
    }
}

/// Builds the tempo map from events placed in quarter notes (the way most DAWs store it).
#[derive(Default)]
pub(crate) struct QuarterTempo {
    /// (quarter note, tempo, time signature)
    events: Vec<TempoEvent>,
}

type TempoEvent = (f64, Option<f64>, Option<(u32, u32)>);

impl QuarterTempo {
    pub fn tempo(&mut self, q: f64, bpm: f64) {
        if bpm.is_finite() && bpm > 0.0 {
            self.events.push((q, Some(bpm), None));
        }
    }

    pub fn signature(&mut self, q: f64, sig: (u32, u32)) {
        if sig.0 > 0 && sig.1 > 0 {
            self.events.push((q, None, Some(sig)));
        }
    }

    /// Write the opening tempo/signature and the changes after it into `project`.
    pub fn apply(mut self, project: &mut Project) {
        self.events.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (mut q0, mut secs0) = (0.0, 0.0);
        for (q, bpm, sig) in self.events {
            let current = project.tempo_changes.last().map_or(project.tempo_bpm, |c| c.bpm);
            if q <= 1e-6 {
                project.tempo_bpm = bpm.unwrap_or(project.tempo_bpm);
                project.time_signature = sig.unwrap_or(project.time_signature);
                continue;
            }
            let at = secs0 + (q - q0) * 60.0 / current;
            (q0, secs0) = (q, at);
            match project.tempo_changes.last_mut() {
                // Tempo and signature at the same spot become one change.
                Some(last) if (last.at_secs - at).abs() < 1e-9 => {
                    last.bpm = bpm.unwrap_or(last.bpm);
                    last.time_signature = sig.or(last.time_signature);
                }
                _ if bpm.is_some_and(|b| (b - current).abs() < 1e-9) && sig.is_none() => {}
                _ => project.tempo_changes.push(TempoChange { at_secs: at, bpm: bpm.unwrap_or(current), time_signature: sig }),
            }
        }
    }
}

/// Parse XML, accepting a DOCTYPE (FCPXML always has one).
pub(crate) fn parse_xml(text: &str) -> Result<roxmltree::Document<'_>, roxmltree::Error> {
    roxmltree::Document::parse_with_options(text, roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() })
}

pub(crate) fn audio_clip(name: impl Into<String>, file: PathBuf, position_secs: f64, length_secs: f64, source_offset_secs: f64) -> Clip {
    Clip { name: name.into(), file, position_secs, length_secs, source_offset_secs, gain: 1.0, mute: false, takes: Vec::new(), active_take: 0, pitch_semitones: 0, pitch_cents: 0, stretch: 1.0, fade_in: Default::default(), fade_out: Default::default() }
}

/// OpenTake's built-in synth, for MIDI tracks whose instrument can't be loaded here.
pub(crate) fn synth_slot() -> PluginSlot {
    builtin("OpenTake Synth", "synth")
}

pub(crate) fn drums_slot() -> PluginSlot {
    builtin("OpenTake Drums", "drums")
}

fn builtin(name: &str, path: &str) -> PluginSlot {
    PluginSlot { name: name.into(), format: PluginFormat::Builtin, path: path.into(), uid: path.into(), bypass: false, state: Vec::new(), params: Vec::new(), id: 0, side_chain: None, instance: 0 }
}

/// A plug-in known only by name. VST3 bundles are found by file name, so guess `<name>.vst3`.
pub(crate) fn named_plugin(format: PluginFormat, name: &str, vendor: Option<&str>, uid: &str) -> PluginSlot {
    let prefix = match format {
        PluginFormat::Vst3 => "VST3",
        PluginFormat::Vst2 => "VST",
        PluginFormat::Clap => "CLAP",
        _ => "AU",
    };
    let full = match vendor.filter(|v| !v.is_empty()) {
        Some(v) => format!("{prefix}: {name} ({v})"),
        None => format!("{prefix}: {name}"),
    };
    let path = if format == PluginFormat::Vst3 { format!("{name}.vst3") } else { String::new() };
    PluginSlot { name: full, format, path, uid: uid.into(), bypass: false, state: Vec::new(), params: Vec::new(), id: 0, side_chain: None, instance: 0 }
}

/// dB to linear gain.
pub(crate) fn db_to_gain(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// A path from the project file: absolute, `file://` URL, or relative to `base`.
pub(crate) fn resolve_path(raw: &str, base: &Path) -> PathBuf {
    let raw = raw.strip_prefix("file://").map(percent_decode).unwrap_or_else(|| raw.to_string());
    // file://localhost/… and Windows file:///C:/… forms.
    let raw = raw.strip_prefix("localhost").map(str::to_string).unwrap_or(raw);
    let raw = if raw.len() > 3 && raw.as_bytes()[0] == b'/' && raw.as_bytes()[2] == b':' { raw[1..].to_string() } else { raw };
    let p = PathBuf::from(raw);
    if p.is_relative() { base.join(p) } else { p }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c))) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Missing media: look for a file of the same name in common folders near the project
/// (projects are often moved without keeping absolute paths valid).
pub(crate) fn find_media(path: PathBuf, project_dir: &Path) -> PathBuf {
    if path.exists() {
        return path;
    }
    let Some(name) = path.file_name() else { return path };
    ["", "Audio", "Audio Files", "Samples", "Samples/Recorded", "Samples/Imported", "Media", "audio"]
        .iter()
        .map(|d| project_dir.join(d).join(name))
        .find(|p| p.exists())
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_tempo_events_become_timed_changes() {
        let mut t = QuarterTempo::default();
        t.tempo(0.0, 100.0);
        t.signature(0.0, (3, 4));
        t.tempo(8.0, 120.0); // 8 quarters at 100 bpm = 4.8 s
        t.signature(8.0, (4, 4));
        t.tempo(12.0, 120.0); // no change
        t.tempo(20.0, 60.0); // 12 quarters at 120 = 6 s later
        let mut p = Project::default();
        t.apply(&mut p);
        assert_eq!((p.tempo_bpm, p.time_signature), (100.0, (3, 4)));
        assert_eq!(p.tempo_changes, vec![
            TempoChange { at_secs: 4.8, bpm: 120.0, time_signature: Some((4, 4)) },
            TempoChange { at_secs: 10.8, bpm: 60.0, time_signature: None },
        ]);
    }

    #[test]
    fn resolves_file_urls_and_relative_paths() {
        let base = Path::new("/proj");
        assert_eq!(resolve_path("file:///Users/me/My%20Song/a.wav", base), PathBuf::from("/Users/me/My Song/a.wav"));
        assert_eq!(resolve_path("file://localhost/a.wav", base), PathBuf::from("/a.wav"));
        assert_eq!(resolve_path("audio/a.wav", base), PathBuf::from("/proj/audio/a.wav"));
    }
}
