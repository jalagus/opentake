//! Reaper project (`.rpp`) import and export.
//!
//! Supported: tempo/time signature, sample rate, tracks (name, volume, pan, mute,
//! solo, record arm, input, monitoring), audio items (position, length, source
//! offset, gain, mute, file) and FX chain references (VST2/VST3/CLAP, bypass,
//! opaque state). Everything else in the file is ignored on import.

pub mod tree;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::model::{Clip, PluginFormat, PluginSlot, Project, Track};
use tree::{Entry, Node};

#[derive(Debug, Error)]
pub enum RppError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Parse(#[from] tree::ParseError),
    #[error("not a Reaper project (root chunk is <{0}>)")]
    NotAProject(String),
}

/// Reaper encodes record inputs >= this value as MIDI.
const REC_INPUT_MIDI: u32 = 4096;
/// Flag added to the input index for stereo inputs.
const REC_INPUT_STEREO: u32 = 1024;

pub fn load(path: impl AsRef<Path>) -> Result<Project, RppError> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)?;
    let base = path.parent().unwrap_or(Path::new("."));
    let mut project = from_str(&text, base)?;
    if let Some(stem) = path.file_stem() {
        project.name = stem.to_string_lossy().into_owned();
    }
    Ok(project)
}

pub fn save(project: &Project, path: impl AsRef<Path>) -> Result<(), RppError> {
    let path = path.as_ref();
    let base = path.parent().unwrap_or(Path::new("."));
    std::fs::write(path, to_string(project, base))?;
    Ok(())
}

/// Parse RPP text. Relative media paths are resolved against `base_dir`.
pub fn from_str(text: &str, base_dir: &Path) -> Result<Project, RppError> {
    let root = tree::parse(text)?;
    if root.name != "REAPER_PROJECT" {
        return Err(RppError::NotAProject(root.name));
    }

    let mut project = Project::default();
    if let Some(v) = root.values("TEMPO") {
        if let Some(bpm) = v.first().and_then(|s| s.parse().ok()) {
            project.tempo_bpm = bpm;
        }
        if let (Some(n), Some(d)) = (v.get(1).and_then(|s| s.parse().ok()), v.get(2).and_then(|s| s.parse().ok())) {
            project.time_signature = (n, d);
        }
    }
    // Tempo envelope: PT <secs> <bpm> <shape> [<num + den * 65536>]. Ramps become steps.
    if let Some(env) = root.chunk("TEMPOENVEX") {
        for line in env.lines().filter(|l| l.key() == "PT") {
            let t = line.tokens();
            let (Some(at), Some(bpm)) = (t.get(1).and_then(|v| v.parse::<f64>().ok()), t.get(2).and_then(|v| v.parse::<f64>().ok())) else { continue };
            let sig = t.get(4).and_then(|v| v.parse::<i64>().ok()).filter(|&c| c > 0).map(|c| ((c & 0xffff) as u32, (c >> 16) as u32)).filter(|s| s.0 > 0 && s.1 > 0);
            if at <= 1e-9 {
                project.tempo_bpm = bpm;
                if let Some(s) = sig {
                    project.time_signature = s;
                }
            } else {
                project.tempo_changes.push(crate::TempoChange { at_secs: at, bpm, time_signature: sig });
            }
        }
    }
    if let Some(sr) = root.value::<u32>("SAMPLERATE", 0).filter(|&sr| sr > 0) {
        project.sample_rate = sr;
    }
    if let Some(v) = root.value::<f32>("MASTER_VOLUME", 0) {
        project.master_volume = v;
    }
    // Reaper's loop points are its time selection; LOOP toggles repeat.
    project.loop_enabled = root.value::<u8>("LOOP", 0).unwrap_or(0) != 0;
    if let (Some(a), Some(b)) = (root.value::<f64>("SELECTION", 0), root.value::<f64>("SELECTION", 1)) {
        (project.loop_start_secs, project.loop_end_secs) = (a.min(b), a.max(b));
    }
    project.tracks = root.chunks("TRACK").map(|t| import_track(t, base_dir)).collect();
    Ok(project)
}

fn import_track(node: &Node, base_dir: &Path) -> Track {
    let mut track = Track::new(node.string("NAME").unwrap_or_default());
    track.volume = node.value("VOLPAN", 0).unwrap_or(1.0);
    track.pan = node.value("VOLPAN", 1).unwrap_or(0.0);
    track.mute = node.value::<u8>("MUTESOLO", 0).unwrap_or(0) != 0;
    track.solo = node.value::<u8>("MUTESOLO", 1).unwrap_or(0) != 0;
    if let Some(rec) = node.values("REC") {
        let field = |i: usize| rec.get(i).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        track.armed = field(0) != 0;
        let input = field(1).max(0) as u32;
        if input < REC_INPUT_MIDI {
            track.input_channel = (input % REC_INPUT_STEREO) as u16;
        }
        track.monitor_input = field(2) != 0;
    }
    if let Some(fx) = node.chunk("FXCHAIN") {
        let mut chain = import_fx_chain(fx);
        // Reaper prefixes instruments with "VSTi:" / "VST3i:"; the first one becomes ours.
        if let Some(i) = chain.iter().position(|p| is_instrument_name(&p.name)) {
            track.instrument = Some(chain.remove(i));
        }
        track.plugins = chain;
    }
    track.clips = node.chunks("ITEM").filter_map(|i| import_item(i, base_dir)).collect();
    track
}

fn import_fx_chain(node: &Node) -> Vec<PluginSlot> {
    let mut plugins = Vec::new();
    // A `BYPASS b offline wak` line precedes each plugin chunk.
    let mut bypass = false;
    for entry in &node.children {
        match entry {
            Entry::Line(l) if l.key() == "BYPASS" => {
                bypass = l.tokens().get(1).is_some_and(|s| s != "0");
            }
            Entry::Chunk(n) => {
                let p = |i: usize| n.params.get(i).cloned().unwrap_or_default();
                let (format, path, uid) = match n.name.as_str() {
                    "VST" if p(0).starts_with("VST3") => (PluginFormat::Vst3, p(1), p(4)),
                    "VST" => (PluginFormat::Vst2, p(1), p(4)),
                    "CLAP" => (PluginFormat::Clap, String::new(), p(1)),
                    _ => (PluginFormat::Other, p(1), String::new()),
                };
                plugins.push(PluginSlot {
                    name: p(0),
                    format,
                    path,
                    uid,
                    bypass,
                    state: n.lines().map(|l| l.raw.clone()).collect(),
                    params: Vec::new(),
                    instance: 0,
                });
                bypass = false;
            }
            _ => {}
        }
    }
    plugins
}

fn import_item(node: &Node, base_dir: &Path) -> Option<Clip> {
    let file = find_source_file(node.chunk("SOURCE")?)?;
    let file = PathBuf::from(file);
    let file = if file.is_relative() { base_dir.join(file) } else { file };
    Some(Clip {
        name: node.string("NAME").unwrap_or_default(),
        file,
        position_secs: node.value("POSITION", 0).unwrap_or(0.0),
        length_secs: node.value("LENGTH", 0).unwrap_or(0.0),
        source_offset_secs: node.value("SOFFS", 0).unwrap_or(0.0),
        gain: node.value("VOLPAN", 0).unwrap_or(1.0),
        mute: node.value::<u8>("MUTE", 0).unwrap_or(0) != 0,
    })
}

/// SECTION and other wrapper sources nest another `<SOURCE>`; dig until a FILE line.
fn find_source_file(source: &Node) -> Option<String> {
    source.string("FILE").or_else(|| source.chunk("SOURCE").and_then(find_source_file))
}

/// Serialize to RPP text. Media paths under `base_dir` are written relative to it.
pub fn to_string(project: &Project, base_dir: &Path) -> String {
    let mut root = Node::new("REAPER_PROJECT").with_params(&["0.1", "7.0/OpenTake", "0"]);
    let (num, den) = project.time_signature;
    root.push_line(&["TEMPO".to_string(), fmt_f64(project.tempo_bpm), num.to_string(), den.to_string()]);
    root.push_line(&["SAMPLERATE".to_string(), project.sample_rate.to_string(), "0".into(), "0".into()]);
    root.push_line(&["MASTER_VOLUME".to_string(), fmt_f32(project.master_volume), "0".into(), "-1".into(), "-1".into(), "1".into()]);
    root.push_line(&["LOOP", bool01(project.loop_enabled)]);
    root.push_line(&["SELECTION".to_string(), fmt_f64(project.loop_start_secs), fmt_f64(project.loop_end_secs)]);
    if !project.tempo_changes.is_empty() {
        let mut env = Node::new("TEMPOENVEX");
        env.push_line(&["ACT", "1", "-1"]).push_line(&["VIS", "1", "0", "1"]).push_line(&["DEFSHAPE", "1", "-1", "-1"]);
        let code = |(n, d): (u32, u32)| (n as i64 + ((d as i64) << 16)).to_string();
        env.push_line(&["PT".to_string(), "0".into(), fmt_f64(project.tempo_bpm), "1".into(), code(project.time_signature)]);
        let mut changes = project.tempo_changes.clone();
        changes.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
        for c in changes {
            let mut line = vec!["PT".to_string(), fmt_f64(c.at_secs), fmt_f64(c.bpm), "1".into()];
            if let Some(sig) = c.time_signature {
                line.push(code(sig));
            }
            env.push_line(&line);
        }
        root.push_chunk(env);
    }
    for track in &project.tracks {
        root.push_chunk(export_track(track, base_dir));
    }
    tree::write(&root)
}

fn export_track(track: &Track, base_dir: &Path) -> Node {
    let mut node = Node::new("TRACK");
    node.push_line(&["NAME", &track.name]);
    node.push_line(&["VOLPAN", &fmt_f32(track.volume), &fmt_f32(track.pan), "-1", "-1", "1"]);
    node.push_line(&["MUTESOLO", bool01(track.mute), bool01(track.solo), "0"]);
    node.push_line(&[
        "REC",
        bool01(track.armed),
        &track.input_channel.to_string(),
        bool01(track.monitor_input),
        "0", "0", "0", "0", "0",
    ]);
    node.push_line(&["NCHAN", "2"]);

    // Instrument first (as Reaper does); built-in plugins can't be represented.
    let chain: Vec<&PluginSlot> =
        track.instrument.iter().chain(&track.plugins).filter(|p| p.format != PluginFormat::Builtin).collect();
    if !chain.is_empty() {
        let mut fx = Node::new("FXCHAIN");
        fx.push_line(&["SHOW", "0"]).push_line(&["LASTSEL", "0"]).push_line(&["DOCKED", "0"]);
        for plugin in chain {
            fx.push_line(&["BYPASS", bool01(plugin.bypass), "0", "0"]);
            let mut chunk = match plugin.format {
                PluginFormat::Clap => Node::new("CLAP").with_params(&[plugin.name.as_str(), &plugin.uid, ""]),
                _ => {
                    // Reaper looks plugins up by bundle file name.
                    let file = Path::new(&plugin.path).file_name().map_or(plugin.path.clone(), |f| f.to_string_lossy().into_owned());
                    Node::new("VST").with_params(&[plugin.name.as_str(), &file, "0", "", &plugin.uid, ""])
                }
            };
            for line in &plugin.state {
                chunk.push_raw(line.clone());
            }
            fx.push_chunk(chunk);
        }
        node.push_chunk(fx);
    }

    for clip in &track.clips {
        let mut item = Node::new("ITEM");
        item.push_line(&["POSITION", &fmt_f64(clip.position_secs)]);
        item.push_line(&["LENGTH", &fmt_f64(clip.length_secs)]);
        item.push_line(&["MUTE", bool01(clip.mute), "0"]);
        item.push_line(&["NAME", &clip.name]);
        item.push_line(&["VOLPAN", &fmt_f32(clip.gain), "0", "1", "-1"]);
        item.push_line(&["SOFFS", &fmt_f64(clip.source_offset_secs)]);
        let file = clip.file.strip_prefix(base_dir).unwrap_or(&clip.file);
        let mut source = Node::new("SOURCE").with_params(&[source_type(&clip.file)]);
        source.push_line(&["FILE", &file.to_string_lossy()]);
        item.push_chunk(source);
        node.push_chunk(item);
    }
    node
}

fn is_instrument_name(name: &str) -> bool {
    name.starts_with("VSTi:") || name.starts_with("VST3i:")
}

fn source_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("mp3") => "MP3",
        Some("flac") => "FLAC",
        Some("ogg") => "VORBIS",
        _ => "WAVE",
    }
}

fn bool01(b: bool) -> &'static str {
    if b { "1" } else { "0" }
}

fn fmt_f64(v: f64) -> String {
    format!("{v}")
}

fn fmt_f32(v: f32) -> String {
    format!("{v}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<REAPER_PROJECT 0.1 "7.27/macOS-arm64" 1729780000
  RIPPLE 0
  TEMPO 98.5 3 4
  SAMPLERATE 44100 0 0
  MASTER_VOLUME 0.7 0 -1 -1 1
  LOOP 1
  SELECTION 2 6.5
  <TRACK {6A1B6C9E-0000-0000-0000-000000000001}
    NAME "Lead Vox"
    VOLPAN 0.5 -0.25 -1 -1 1
    MUTESOLO 0 1 0
    REC 1 1025 1 0 0 0 0 0
    <FXCHAIN
      SHOW 0
      LASTSEL 0
      DOCKED 0
      BYPASS 1 0 0
      <VST "VST3: ReaEQ (Cockos)" reaeq.vst3 0 "" 1919247729{5653545265716572656165712E000000} ""
        cWVyUu5e7f4CAAAAAQAAAAAAAAACAAAAAAAAAAIAAAABAAAAAAAAAAIAAAAAAAAAWwAAAAAAAAA=
        AAAQAAAA
      >
      FLOATPOS 0 0 0 0
    >
    <ITEM
      POSITION 2.5
      SNAPOFFS 0
      LENGTH 10
      LOOP 0
      MUTE 0 0
      NAME "take 1"
      VOLPAN 0.8 0 1 -1
      SOFFS 1.25
      <SOURCE WAVE
        FILE "Audio/vox-01.wav"
      >
    >
  >
>
"#;

    #[test]
    fn tempo_map_round_trips_through_the_tempo_envelope() {
        let mut p = Project { tempo_bpm: 120.0, ..Project::default() };
        p.tempo_changes = vec![
            crate::TempoChange { at_secs: 4.0, bpm: 96.0, time_signature: Some((3, 4)) },
            crate::TempoChange { at_secs: 9.5, bpm: 140.0, time_signature: None },
        ];
        let text = to_string(&p, Path::new("/proj"));
        assert!(text.contains("<TEMPOENVEX") && text.contains("PT 4 96 1 262147"), "{text}");
        let back = from_str(&text, Path::new("/proj")).unwrap();
        assert_eq!(back.tempo_bpm, 120.0);
        assert_eq!(back.tempo_changes, p.tempo_changes);
    }

    #[test]
    fn imports_sample() {
        let p = from_str(SAMPLE, Path::new("/proj")).unwrap();
        assert_eq!(p.tempo_bpm, 98.5);
        assert_eq!(p.time_signature, (3, 4));
        assert_eq!(p.sample_rate, 44100);
        assert_eq!(p.master_volume, 0.7);
        assert!(p.loop_enabled);
        assert_eq!((p.loop_start_secs, p.loop_end_secs), (2.0, 6.5));
        let t = &p.tracks[0];
        assert_eq!(t.name, "Lead Vox");
        assert_eq!((t.volume, t.pan, t.mute, t.solo), (0.5, -0.25, false, true));
        assert!(t.armed && t.monitor_input);
        assert_eq!(t.input_channel, 1);
        let fx = &t.plugins[0];
        assert_eq!(fx.format, PluginFormat::Vst3);
        assert_eq!(fx.path, "reaeq.vst3");
        assert!(fx.bypass);
        assert_eq!(fx.state.len(), 2);
        let c = &t.clips[0];
        assert_eq!(c.file, PathBuf::from("/proj/Audio/vox-01.wav"));
        assert_eq!((c.position_secs, c.length_secs, c.source_offset_secs, c.gain), (2.5, 10.0, 1.25, 0.8));
    }

    #[test]
    fn export_import_round_trip() {
        let base = Path::new("/proj");
        let p = from_str(SAMPLE, base).unwrap();
        let text = to_string(&p, base);
        assert!(text.contains("FILE Audio/vox-01.wav"));
        let p2 = from_str(&text, base).unwrap();
        assert_eq!(p, p2);
    }
}
