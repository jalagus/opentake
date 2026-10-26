//! Reaper project (`.rpp`) import and export.
//!
//! Supported: tempo/time signature, sample rate, tracks (name, volume, pan, mute,
//! solo, record arm, input, monitoring), audio items (position, length, source
//! offset, gain, mute, file, fades), FX chain references (VST2/VST3/CLAP, bypass,
//! opaque state), buses (as receives; folders import as buses), sends (receives with their
//! volume, mute and pre/post-fader mode) and volume, pan and mute
//! automation (`VOLENV2`/`PANENV2`/`MUTEENV`; volume envelopes in Reaper's fader scaling
//! are skipped) and markers (`MARKER`; a region imports as a marker at its start). Plug-in parameter envelopes (`PARMENV`) aren't carried over: Reaper
//! numbers parameters by position, which needs the plug-in loaded. Everything else in the
//! file is ignored on import.

pub mod tree;

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::automation::{AutomationLane, AutomationMode, AutomationPoint, AutomationTarget, Curve, gain_to_pos, pos_to_gain};
use crate::fade::{Fade, FadeCurve};
use crate::marker::Marker;
use crate::model::{Clip, PluginFormat, PluginSlot, Project, Track};
use crate::sends::AuxSend;
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
    project.markers = import_markers(&root);
    project.tidy_markers();
    let nodes: Vec<&Node> = root.chunks("TRACK").collect();
    project.tracks = nodes.iter().map(|t| import_track(t, base_dir)).collect();
    import_routing(&nodes, &mut project.tracks);
    Ok(project)
}

/// Reaper's colours are the OS's native 0xBBGGRR with this flag set (0 = default colour).
const COLOR_SET: u32 = 0x100_0000;

/// `MARKER id pos "name" isrgn color ...`. A region is two lines with the same id, its
/// start (named) and its end; only the start is kept.
fn import_markers(root: &Node) -> Vec<Marker> {
    let mut seen_regions = Vec::new();
    let mut markers = Vec::new();
    for line in root.lines().filter(|l| l.key() == "MARKER") {
        let t = line.tokens();
        let (Some(id), Some(pos)) = (t.get(1).and_then(|v| v.parse::<i64>().ok()), t.get(2).and_then(|v| v.parse::<f64>().ok())) else { continue };
        let region = t.get(4).and_then(|v| v.parse::<u32>().ok()).is_some_and(|f| f & 1 != 0);
        if region {
            if seen_regions.contains(&id) {
                continue; // the region's end
            }
            seen_regions.push(id);
        }
        let color = t.get(5).and_then(|v| v.parse::<u32>().ok()).filter(|c| c & COLOR_SET != 0).map(|c| [c as u8, (c >> 8) as u8, (c >> 16) as u8]);
        let name = t.get(3).cloned().filter(|n| !n.is_empty()).unwrap_or_else(|| id.to_string());
        // Reaper's marker number is our id (regions are numbered apart; clashes get new ids).
        markers.push(Marker { id: u32::try_from(id).unwrap_or(0), color, ..Marker::new(pos, name) });
    }
    markers
}

/// Buses: a track without items or instrument that receives (`AUXRECV`) from tracks whose
/// master send is off, or a folder parent without them, becomes a bus its sources feed
/// (outputs as track indices, see `Project::outputs_from_indices`). Of a source's receives,
/// a unity post-fader one is preferred as its output (that's how buses are saved); its other
/// receives into such tracks become sends. Sidechain receives (to channels 3+) are ignored.
fn import_routing(nodes: &[&Node], tracks: &mut [Track]) {
    let can_bus = |t: &Track| t.clips.is_empty() && t.instrument.is_none();
    let mut parents: Vec<usize> = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        if let Some(&parent) = parents.last().filter(|&&p| can_bus(&tracks[p])) {
            tracks[parent].bus = true;
            tracks[i].output = Some(parent as u64);
        }
        match (node.value::<i32>("ISBUS", 0), node.value::<i32>("ISBUS", 1)) {
            (Some(1), _) => parents.push(i),
            (Some(2), Some(depth)) => parents.truncate(parents.len().saturating_sub(depth.unsigned_abs() as usize)),
            _ => {}
        }
    }
    // (source, destination, as a send)
    let mut receives: Vec<(usize, usize, AuxSend)> = Vec::new();
    for (dst, node) in nodes.iter().enumerate() {
        for line in node.lines().filter(|l| l.key() == "AUXRECV") {
            let tokens = line.tokens();
            let field = |i: usize| tokens.get(i).and_then(|s| s.parse::<f64>().ok());
            let Some(src) = field(1).map(|s| s as usize).filter(|&s| s < tracks.len() && s != dst) else { continue };
            if !can_bus(&tracks[dst]) || field(8).is_some_and(|c| c < 0.0) || field(9).is_some_and(|c| c > 0.0) {
                continue;
            }
            // Modes: 0 post-fader, 1 pre-FX, 3 pre-fader (post-FX); pre-FX is taken as pre-fader.
            let send = AuxSend { level: field(3).unwrap_or(1.0) as f32, pre_fader: field(2).is_some_and(|m| m != 0.0), mute: field(5).is_some_and(|m| m != 0.0), ..AuxSend::new(dst as u64) };
            receives.push((src, dst, send));
        }
    }
    let unity = |s: &AuxSend| s.level == 1.0 && !s.pre_fader && !s.mute;
    for unity_only in [true, false] {
        receives.retain(|(src, dst, send)| {
            let output = nodes[*src].value::<u8>("MAINSEND", 0) == Some(0) && tracks[*src].output.is_none() && (unity(send) || !unity_only);
            if output {
                tracks[*dst].bus = true;
                tracks[*src].output = Some(*dst as u64);
            }
            !output
        });
    }
    for (src, dst, mut send) in receives {
        tracks[dst].bus = true;
        if (send.level - 1.0).abs() < 1e-6 {
            send.level = 1.0; // see `export_track`
        }
        tracks[src].sends.push(send);
    }
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
    for (name, target) in ENVELOPES {
        if let Some(lane) = node.chunk(name).and_then(|env| import_envelope(env, target)) {
            track.automation.push(lane);
        }
    }
    track.show_automation = track.automation.iter().any(|l| l.shown);
    track
}

/// Track envelopes Reaper and OpenTake share.
const ENVELOPES: [(&str, AutomationTarget); 3] = [("VOLENV2", AutomationTarget::Volume), ("PANENV2", AutomationTarget::Pan), ("MUTEENV", AutomationTarget::Mute)];

/// Reaper's point values: volume as amplitude, pan from 1 (left) to -1 (right), mute 1 for
/// playing and 0 for muted.
fn to_reaper(target: AutomationTarget, v: f32) -> f32 {
    match target {
        AutomationTarget::Volume => pos_to_gain(v),
        AutomationTarget::Pan => 1.0 - 2.0 * v,
        _ => 1.0 - v.round(),
    }
}

fn from_reaper(target: AutomationTarget, v: f32) -> f32 {
    match target {
        AutomationTarget::Volume => gain_to_pos(v),
        AutomationTarget::Pan => (1.0 - v) / 2.0,
        _ => (v < 0.5) as u8 as f32,
    }
}

/// Point shapes: linear, square, slow start/end, fast start, fast end (Bézier is linear here).
const SHAPES: [Curve; 5] = [Curve::Linear, Curve::Hold, Curve::Smooth, Curve::FastStart, Curve::FastEnd];

fn import_envelope(env: &Node, target: AutomationTarget) -> Option<AutomationLane> {
    if target == AutomationTarget::Volume && env.value::<u8>("VOLTYPE", 0) == Some(1) {
        return None;
    }
    let mut lane = AutomationLane::new(target);
    lane.mode = if env.value::<u8>("ACT", 0) == Some(0) { AutomationMode::Off } else { AutomationMode::Read };
    lane.shown = env.value::<u8>("VIS", 0) != Some(0);
    let points = env.lines().filter(|l| l.key() == "PT").filter_map(|l| {
        let t = l.tokens();
        let (time, value) = (t.get(1)?.parse::<f64>().ok()?, t.get(2)?.parse::<f32>().ok()?);
        let curve = t.get(3).and_then(|s| s.parse::<usize>().ok()).and_then(|i| SHAPES.get(i).copied()).unwrap_or_default();
        Some(AutomationPoint { curve, ..AutomationPoint::new(time, from_reaper(target, value)) })
    });
    lane.set_points(points);
    (!lane.points.is_empty()).then_some(lane)
}

fn export_envelope(lane: &AutomationLane, name: &str) -> Node {
    let mut env = Node::new(name);
    env.push_line(&["ACT", bool01(lane.mode != AutomationMode::Off), "-1"]).push_line(&["VIS", bool01(lane.shown), "1", "1"]);
    env.push_line(&["ARM", "0"]).push_line(&["DEFSHAPE", if lane.target.stepped() { "1" } else { "0" }, "-1", "-1"]);
    for p in &lane.points {
        let shape = SHAPES.iter().position(|&c| c == p.curve).unwrap_or(0);
        env.push_line(&["PT".to_string(), fmt_f64(p.time_secs), fmt_f32(to_reaper(lane.target, p.value)), shape.to_string()]);
    }
    env
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
                    "AU" => (PluginFormat::AudioUnit, p(1), au_uid(&n.params)),
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
                    id: 0,
                    side_chain: None,
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
    // PLAYRATE rate preserve_pitch pitch_semitones ...: without preserve pitch the rate
    // also transposes (varispeed).
    let rate = node.value("PLAYRATE", 0).filter(|r: &f64| *r > 0.0).unwrap_or(1.0);
    let preserve = node.value::<u8>("PLAYRATE", 1).unwrap_or(1) != 0;
    let pitch = node.value("PLAYRATE", 2).unwrap_or(0.0) + if preserve { 0.0 } else { 12.0 * rate.log2() };
    let cents = (pitch * 100.0).round() as i32;
    Some(Clip {
        name: node.string("NAME").unwrap_or_default(),
        file,
        position_secs: node.value("POSITION", 0).unwrap_or(0.0),
        length_secs: node.value("LENGTH", 0).unwrap_or(0.0),
        source_offset_secs: node.value("SOFFS", 0).unwrap_or(0.0),
        gain: node.value("VOLPAN", 0).unwrap_or(1.0),
        mute: node.value::<u8>("MUTE", 0).unwrap_or(0) != 0,
        takes: Vec::new(),
        active_take: 0,
        pitch_semitones: cents / 100,
        pitch_cents: cents % 100,
        stretch: 1.0 / rate,
        fade_in: import_fade(node, "FADEIN"),
        fade_out: import_fade(node, "FADEOUT"),
        comp: Vec::new(),
        show_takes: false,
    })
}

/// `FADEIN shape length auto_length ...`. Reaper's automatic crossfade length isn't read:
/// overlaps crossfade here anyway.
fn import_fade(node: &Node, key: &str) -> Fade {
    let curve = match node.value::<u8>(key, 0).unwrap_or(0) {
        1 => FadeCurve::EqualPower,
        2 | 4 => FadeCurve::Exponential,
        3 => FadeCurve::Logarithmic,
        5 | 6 => FadeCurve::SCurve,
        _ => FadeCurve::Linear,
    };
    Fade::new(node.value(key, 1).filter(|l: &f64| *l > 0.0).unwrap_or(0.0), curve)
}

/// Reaper's nearest fade shape (0 linear, 1 fast start/equal power, 2 fast end, 3 steep
/// fast start, 5 slow start and end).
fn reaper_shape(curve: FadeCurve) -> &'static str {
    match curve {
        FadeCurve::Linear => "0",
        FadeCurve::EqualPower => "1",
        FadeCurve::Exponential => "2",
        FadeCurve::Logarithmic => "3",
        FadeCurve::SCurve => "5",
    }
}

/// SECTION and other wrapper sources nest another `<SOURCE>`; dig until a FILE line.
fn find_source_file(source: &Node) -> Option<String> {
    source.string("FILE").or_else(|| source.chunk("SOURCE").and_then(find_source_file))
}

/// Serialize to RPP text. Media paths under `base_dir` are written relative to it. Track
/// outputs and sends are track indices (see `Project::outputs_to_indices`); a bus becomes a
/// track that receives from its sources (whose master send is off) and from sends.
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
    for m in &project.markers {
        let color = m.color.map_or(0, |[r, g, b]| r as u32 | (g as u32) << 8 | (b as u32) << 16 | COLOR_SET);
        root.push_line(&["MARKER".to_string(), m.id.to_string(), fmt_f64(m.time_secs), m.name.clone(), "0".into(), color.to_string(), "1".into()]);
    }
    for (i, track) in project.tracks.iter().enumerate() {
        // Outputs are post-fader receives at unity; sends keep their level, mute and mode.
        let outputs = project.tracks.iter().enumerate().filter(|(_, s)| s.output == Some(i as u64)).map(|(s, _)| (s, AuxSend::new(i as u64)));
        // A unity post-fader send from a track that feeds a bus is written a hair above unity,
        // so loading doesn't take it for the track's output.
        let sends = project.tracks.iter().enumerate().flat_map(|(s, t)| {
            t.sends.iter().filter(|x| x.bus == i as u64).map(move |x| {
                let ambiguous = t.output.is_some() && x.level == 1.0 && !x.pre_fader && !x.mute;
                (s, AuxSend { level: if ambiguous { 1.0 + f32::EPSILON } else { x.level }, ..x.clone() })
            })
        });
        let receives: Vec<(usize, AuxSend)> = outputs.chain(sends).collect();
        root.push_chunk(export_track(track, base_dir, &receives));
    }
    tree::write(&root)
}

fn export_track(track: &Track, base_dir: &Path, receives: &[(usize, AuxSend)]) -> Node {
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
    if track.output.is_some() {
        node.push_line(&["MAINSEND", "0", "0"]);
    }
    // Stereo receives; mode 3 is pre-fader (post-FX), 0 post-fader.
    for (src, send) in receives {
        let mode = if send.pre_fader { "3" } else { "0" };
        node.push_line(&["AUXRECV", &src.to_string(), mode, &fmt_f32(send.level), "0", bool01(send.mute), "0", "0", "0", "0", "-1:U", "0", "-1", "''"]);
    }

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
                PluginFormat::AudioUnit => {
                    let mut params = vec![plugin.name.clone(), plugin.path.clone(), String::new()];
                    params.extend(au_codes(&plugin.uid));
                    Node::new("AU").with_params(&params)
                }
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

    for (name, target) in ENVELOPES {
        if let Some(lane) = track.automation.iter().find(|l| l.target == target && !l.points.is_empty()) {
            node.push_chunk(export_envelope(lane, name));
        }
    }

    for clip in &track.clips {
        let mut item = Node::new("ITEM");
        item.push_line(&["POSITION", &fmt_f64(clip.position_secs)]);
        item.push_line(&["LENGTH", &fmt_f64(clip.length_secs)]);
        item.push_line(&["MUTE", bool01(clip.mute), "0"]);
        item.push_line(&["NAME", &clip.name]);
        item.push_line(&["VOLPAN", &fmt_f32(clip.gain), "0", "1", "-1"]);
        item.push_line(&["SOFFS", &fmt_f64(clip.source_offset_secs)]);
        for (key, f) in [("FADEIN", clip.fade_in), ("FADEOUT", clip.fade_out)] {
            if !f.is_none() {
                let shape = reaper_shape(f.curve);
                item.push_line(&[key, shape, &fmt_f64(f.secs), "0", shape, "0", "0", "0"]);
            }
        }
        if clip.is_flexed() {
            item.push_line(&["PLAYRATE", &fmt_f64(1.0 / clip.stretch), "1", &fmt_f64(clip.transpose()), "-1", "0", "0.0025"]);
        }
        let file = clip.file.strip_prefix(base_dir).unwrap_or(&clip.file);
        let mut source = Node::new("SOURCE").with_params(&[source_type(&clip.file)]);
        source.push_line(&["FILE", &file.to_string_lossy()]);
        item.push_chunk(source);
        node.push_chunk(item);
    }
    node
}

fn is_instrument_name(name: &str) -> bool {
    name.starts_with("VSTi:") || name.starts_with("VST3i:") || name.starts_with("AUi:")
}

/// Reaper stores an AU's type, subtype and manufacturer as decimal codes
/// (`<AU "AU: AUDelay (Apple)" "Apple: AUDelay" "" 1635083896<inline> ...`); ours is
/// "aufx:dely:appl".
fn au_uid(params: &[String]) -> String {
    let codes: Vec<String> = params
        .iter()
        .skip(3)
        .take(3)
        .filter_map(|t| t.split('<').next()?.parse::<u32>().ok())
        .map(|c| c.to_be_bytes().iter().map(|&b| b as char).collect())
        .collect();
    if codes.len() == 3 { codes.join(":") } else { String::new() }
}

fn au_codes(uid: &str) -> Vec<String> {
    let chars: Vec<char> = uid.chars().collect();
    if chars.len() != 14 {
        return Vec::new();
    }
    [0, 5, 10].iter().map(|&at| chars[at..at + 4].iter().fold(0u32, |v, &c| (v << 8) | (c as u32 & 0xff)).to_string() + "<inline>").collect()
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
    fn markers_round_trip_and_regions_import_as_markers() {
        let mut p = Project { markers: vec![Marker { color: Some([255, 128, 0]), ..Marker::new(8.0, "Chorus 1") }, Marker::new(2.5, "Verse")], ..Project::default() };
        p.tidy_markers();
        let text = to_string(&p, Path::new("/proj"));
        assert!(text.contains("MARKER 2 2.5 Verse 0 0 1") && text.contains(r#"MARKER 1 8 "Chorus 1" 0 16810239 1"#), "{text}");
        assert_eq!(from_str(&text, Path::new("/proj")).unwrap().markers, p.markers);

        let reaper = "<REAPER_PROJECT 0.1\n  MARKER 1 4 \"Bridge\" 1 0 1 R {GUID} 0\n  MARKER 1 12 \"\" 1\n  MARKER 2 1 \"\" 0 0 1 B {GUID} 0\n>\n";
        let m = from_str(reaper, Path::new("/proj")).unwrap().markers;
        assert_eq!(m.iter().map(|m| (m.time_secs, m.name.as_str())).collect::<Vec<_>>(), vec![(1.0, "2"), (4.0, "Bridge")], "unnamed markers show their number");
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

    #[test]
    fn pitch_and_stretch_as_playrate() {
        let base = Path::new("/proj");
        let mut p = from_str(SAMPLE, base).unwrap();
        let c = &mut p.tracks[0].clips[0];
        (c.stretch, c.pitch_semitones, c.pitch_cents) = (1.25, -3, -20);
        let text = to_string(&p, base);
        assert!(text.contains("PLAYRATE 0.8 1 -3.2"), "{text}");
        let c = &from_str(&text, base).unwrap().tracks[0].clips[0];
        assert_eq!((c.stretch, c.pitch_semitones, c.pitch_cents), (1.25, -3, -20));
        // Varispeed (pitch not preserved): double rate is an octave up at half length.
        let vari = text.replace("PLAYRATE 0.8 1 -3.2", "PLAYRATE 2 0 0");
        let c = &from_str(&vari, base).unwrap().tracks[0].clips[0];
        assert_eq!((c.stretch, c.pitch_semitones, c.pitch_cents), (0.5, 12, 0));
    }

    #[test]
    fn fades_round_trip() {
        let base = Path::new("/proj");
        let mut p = from_str(SAMPLE, base).unwrap();
        let c = &mut p.tracks[0].clips[0];
        (c.fade_in, c.fade_out) = (Fade::new(0.25, FadeCurve::EqualPower), Fade::new(1.5, FadeCurve::SCurve));
        let text = to_string(&p, base);
        assert!(text.contains("FADEIN 1 0.25 0 1") && text.contains("FADEOUT 5 1.5 0 5"), "{text}");
        assert_eq!(from_str(&text, base).unwrap(), p);
        assert!(!to_string(&from_str(SAMPLE, base).unwrap(), base).contains("FADEIN"), "no fade, no line");
    }

    #[test]
    fn buses_round_trip_as_receives_and_folders_import_as_buses() {
        let mut p = from_str(SAMPLE, Path::new("/proj")).unwrap();
        p.tracks.push(Track::new_bus("Vox Bus"));
        p.tracks[0].output = Some(1);
        let text = to_string(&p, Path::new("/proj"));
        assert!(text.contains("MAINSEND 0 0") && text.contains("AUXRECV 0 0 1"), "{text}");
        assert_eq!(from_str(&text, Path::new("/proj")).unwrap(), p);

        let folder = "<REAPER_PROJECT 0.1\n<TRACK\nNAME Drums\nISBUS 1 1\n>\n<TRACK\nNAME Kick\n>\n<TRACK\nNAME Snare\nISBUS 2 -1\n>\n<TRACK\nNAME Bass\n>\n>\n";
        let p = from_str(folder, Path::new("/proj")).unwrap();
        assert!(p.tracks[0].bus);
        assert_eq!(p.tracks.iter().map(|t| t.output).collect::<Vec<_>>(), vec![None, Some(0), Some(0), None]);
    }

    #[test]
    fn sends_round_trip_as_receives_with_level_and_mode() {
        let base = Path::new("/proj");
        let mut p = from_str(SAMPLE, base).unwrap();
        p.tracks.push(Track::new_bus("Verb"));
        p.tracks.push(Track::new_bus("Group"));
        let verb = p.tracks.len() as u64 - 2;
        // Track 0 goes to the group bus (listed after the reverb) and sends to the reverb, at
        // unity too: the output still wins as the post-fader unity receive.
        p.tracks[0].output = Some(verb + 1);
        p.tracks[0].sends.push(AuxSend::new(verb));
        p.tracks[verb as usize + 1].sends.push(AuxSend { level: 0.5, pre_fader: true, mute: true, ..AuxSend::new(verb) });
        let text = to_string(&p, base);
        assert!(text.contains(&format!("AUXRECV {} 3 0.5 0 1", verb + 1)), "{text}");
        let back = from_str(&text, base).unwrap();
        assert_eq!(back.tracks, p.tracks);
        // A Reaper send from a track that keeps its master send, pre-FX at -6 dB.
        let reaper = "<REAPER_PROJECT 0.1\n<TRACK\nNAME Vox\n>\n<TRACK\nNAME Verb\nAUXRECV 0 1 0.5 0 0 0 0 0 0 -1:U 0 -1 ''\nAUXRECV 0 0 1 0 0 0 0 2 2 -1:U 0 -1 ''\n>\n>\n";
        let p = from_str(reaper, base).unwrap();
        assert!(p.tracks[1].bus && p.tracks[0].output.is_none());
        assert_eq!(p.tracks[0].sends, vec![AuxSend { level: 0.5, pre_fader: true, ..AuxSend::new(1) }], "the sidechain receive is ignored");
    }

    #[test]
    fn volume_pan_and_mute_envelopes_round_trip() {
        let base = Path::new("/proj");
        let mut p = from_str(SAMPLE, base).unwrap();
        let t = &mut p.tracks[0];
        let mut vol = AutomationLane::new(AutomationTarget::Volume);
        vol.set_points([AutomationPoint { curve: Curve::Smooth, ..AutomationPoint::new(0.0, gain_to_pos(1.0)) }, AutomationPoint::new(2.0, gain_to_pos(0.5))]);
        let mut pan = AutomationLane::new(AutomationTarget::Pan);
        pan.set_points([AutomationPoint::new(1.0, 0.0)]);
        pan.mode = AutomationMode::Off;
        let mut mute = AutomationLane::new(AutomationTarget::Mute);
        mute.set_points([AutomationPoint::new(0.0, 0.0), AutomationPoint::new(3.0, 1.0)]);
        mute.shown = false;
        t.automation = vec![vol, pan, mute];
        t.show_automation = true;
        let text = to_string(&p, base);
        assert!(text.contains("<VOLENV2") && text.contains("PT 2 0.5 0") && text.contains("PT 0 1 2"), "{text}");
        assert!(text.contains("<PANENV2") && text.contains("PT 1 1 0"), "full left is 1 in Reaper");
        assert!(text.contains("PT 3 0 0"), "muted is 0 in Reaper");
        let back = from_str(&text, base).unwrap();
        let lanes = &back.tracks[0].automation;
        assert_eq!(lanes.len(), 3);
        assert!(lanes.iter().zip(&p.tracks[0].automation).all(|(a, b)| a.target == b.target && a.mode == b.mode && a.shown == b.shown));
        for (a, b) in lanes.iter().flat_map(|l| &l.points).zip(p.tracks[0].automation.iter().flat_map(|l| &l.points)) {
            assert!((a.time_secs - b.time_secs).abs() < 1e-9 && (a.value - b.value).abs() < 1e-4 && a.curve == b.curve, "{a:?} vs {b:?}");
        }
        // Fader-scaled volume envelopes aren't guessed at.
        let faderscaled = text.replace("<VOLENV2", "<VOLENV2\nVOLTYPE 1");
        assert_eq!(from_str(&faderscaled, base).unwrap().tracks[0].automation.len(), 2);
    }

    #[test]
    fn audio_units_use_reapers_chunk() {
        let base = Path::new("/proj");
        let au = |name: &str, path: &str, uid: &str| PluginSlot {
            name: name.into(),
            format: PluginFormat::AudioUnit,
            path: path.into(),
            uid: uid.into(),
            bypass: false,
            state: vec!["YnBsaXN0".into()],
            params: Vec::new(),
            id: 0,
            side_chain: None,
            instance: 0,
        };
        let mut p = Project::default();
        let mut t = Track::new("Keys");
        t.instrument = Some(au("AUi: DLSMusicDevice (Apple)", "Apple: DLSMusicDevice", "aumu:dls :appl"));
        t.plugins = vec![au("AU: AUDelay (Apple)", "Apple: AUDelay", "aufx:dely:appl")];
        p.tracks.push(t);
        let text = to_string(&p, base);
        assert!(text.contains(r#"<AU "AU: AUDelay (Apple)" "Apple: AUDelay" "" 1635083896<inline> 1684368505<inline> 1634758764<inline>"#), "{text}");
        let back = from_str(&text, base).unwrap();
        assert_eq!(back.tracks[0].instrument, p.tracks[0].instrument);
        assert_eq!(back.tracks[0].plugins, p.tracks[0].plugins);
    }
}
