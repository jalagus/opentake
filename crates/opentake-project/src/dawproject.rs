//! DAWproject (`.dawproject`) import and export — the open exchange format of Bitwig
//! Studio, Studio One and Cubase 14+: a zip with `project.xml`, `metadata.xml` and the
//! audio files (<https://github.com/bitwig/dawproject>).
//!
//! Imported: tempo/time signature and their automation (as steps), tracks (folders are
//! flattened; the master channel sets the master volume), volume/pan/mute/solo, plug-in
//! references (by name and id; their states aren't applied), audio clips (embedded audio
//! is extracted next to the project; warped clips play at their original speed), MIDI
//! clips with notes, clip loops, nested clip timelines, arrangement markers. Not imported: other automation,
//! sends/returns, fades, built-in devices of the other DAW, the clip launcher.

use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use roxmltree::Node;

use crate::import::{ImportError, Imported, QuarterTempo, audio_clip, db_to_gain, find_media, named_plugin, resolve_path, synth_slot};
use crate::marker::Marker;
use crate::model::{Clip, MidiClip, Note, PluginFormat, PluginSlot, Project, Track};
use crate::tempo::TempoMap;

pub fn load(path: &Path) -> Result<Imported, ImportError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let stem = path.file_stem().map_or("project".into(), |s| s.to_string_lossy().into_owned());
    let file = std::fs::File::open(path)?;
    let mut imported = from_reader(file, dir, &dir.join(format!("{stem} Media")))?;
    imported.project.name = stem;
    Ok(imported)
}

/// Import from a DAWproject archive. Embedded media is extracted into `media_dir`;
/// external file references are resolved against `project_dir`.
pub fn from_reader<R: Read + Seek>(reader: R, project_dir: &Path, media_dir: &Path) -> Result<Imported, ImportError> {
    let mut zip = zip::ZipArchive::new(reader)?;
    let mut xml = String::new();
    zip.by_name("project.xml")?.read_to_string(&mut xml)?;
    let mut media = Media { zip, project_dir: project_dir.to_path_buf(), media_dir: media_dir.to_path_buf(), extracted: HashMap::new() };
    parse(&xml, &mut |f: Node| media.resolve(f))
}

struct Media<R> {
    zip: zip::ZipArchive<R>,
    project_dir: PathBuf,
    media_dir: PathBuf,
    extracted: HashMap<String, PathBuf>,
}

impl<R: Read + Seek> Media<R> {
    /// The file a `<File path=… external=…>` element points to, extracting it if embedded.
    fn resolve(&mut self, file: Node) -> Result<PathBuf, String> {
        let raw = file.attribute("path").unwrap_or_default();
        if file.attribute("external") == Some("true") {
            return Ok(find_media(resolve_path(raw, &self.project_dir), &self.project_dir));
        }
        if let Some(p) = self.extracted.get(raw) {
            return Ok(p.clone());
        }
        let mut entry = self.zip.by_name(raw).map_err(|_| format!("audio file '{raw}' is missing from the archive"))?;
        let rel = entry.enclosed_name().ok_or_else(|| format!("unsafe path '{raw}' in the archive"))?;
        let out = self.media_dir.join(rel);
        // Re-importing the same project reuses what was extracted before.
        if std::fs::metadata(&out).map(|m| m.len()).ok() != Some(entry.size()) {
            let mut write = || -> std::io::Result<()> {
                std::fs::create_dir_all(out.parent().unwrap_or(&self.media_dir))?;
                std::io::copy(&mut entry, &mut std::fs::File::create(&out)?)?;
                Ok(())
            };
            write().map_err(|e| format!("couldn't extract '{raw}': {e}"))?;
        }
        self.extracted.insert(raw.to_string(), out.clone());
        Ok(out)
    }
}

/// Parse `project.xml`. `media` turns a `<File>` element into a local path.
pub fn parse(xml: &str, media: &mut dyn FnMut(Node) -> Result<PathBuf, String>) -> Result<Imported, ImportError> {
    let doc = crate::import::parse_xml(xml)?;
    let root = doc.root_element();
    if root.tag_name().name() != "Project" {
        return Err(ImportError::Invalid(format!("not a DAWproject (root element is <{}>)", root.tag_name().name())));
    }
    let mut project = Project { tracks: Vec::new(), ..Project::default() };
    let mut warnings = Vec::new();

    let transport = child(root, "Transport");
    let mut tempo = QuarterTempo::default();
    if let Some(bpm) = transport.and_then(|t| child(t, "Tempo")).and_then(|t| attr_f64(t, "value")) {
        tempo.tempo(0.0, bpm);
    }
    if let Some(sig) = transport.and_then(|t| child(t, "TimeSignature")).and_then(signature) {
        tempo.signature(0.0, sig);
    }
    let arrangement = child(root, "Arrangement");
    let seconds = arrangement.and_then(|a| child(a, "Lanes")).and_then(|l| l.attribute("timeUnit")) == Some("seconds");
    for (name, is_tempo) in [("TempoAutomation", true), ("TimeSignatureAutomation", false)] {
        let Some(points) = arrangement.and_then(|a| child(a, name)) else { continue };
        if seconds || points.attribute("timeUnit") == Some("seconds") {
            warnings.push("tempo automation in seconds isn't imported".into());
            continue;
        }
        for p in points.children().filter(Node::is_element) {
            let Some(q) = attr_f64(p, "time") else { continue };
            match (is_tempo, attr_f64(p, "value"), signature(p)) {
                (true, Some(bpm), _) => tempo.tempo(q, bpm),
                (false, _, Some(sig)) => tempo.signature(q, sig),
                _ => {}
            }
        }
    }
    tempo.apply(&mut project);

    if let Some(markers) = arrangement.and_then(|a| child(a, "Markers")) {
        let map = project.tempo_map();
        let in_secs = seconds || markers.attribute("timeUnit") == Some("seconds");
        let imported = markers.children().filter(|n| n.has_tag_name("Marker")).filter_map(|m| {
            let t = attr_f64(m, "time")?;
            let color = m.attribute("color").and_then(hex_color);
            Some(Marker { color, ..Marker::new(if in_secs { t } else { map.secs_at(t) }, m.attribute("name").unwrap_or("Marker")) })
        });
        project.markers = imported.collect();
        project.tidy_markers();
    }

    let mut ctx = Ctx { map: project.tempo_map(), tracks: Vec::new(), ids: HashMap::new(), warnings, media, plugin_states: false };
    if let Some(structure) = child(root, "Structure") {
        for node in structure.children().filter(Node::is_element) {
            ctx.structure(node, &mut project);
        }
    }
    if let Some(lanes) = arrangement.and_then(|a| child(a, "Lanes")) {
        let base = Base { seconds: lanes.attribute("timeUnit") == Some("seconds"), origin: 0.0 };
        ctx.content(lanes, base, (0.0, f64::INFINITY), None, None);
    }
    if child(root, "Scenes").is_some_and(|s| s.descendants().any(|n| n.has_tag_name("Clip"))) {
        ctx.warnings.push("clip launcher scenes aren't imported, only the arrangement".into());
    }
    if ctx.plugin_states {
        ctx.warnings.push("plug-in states (presets) aren't imported; plug-ins start with their defaults".into());
    }
    for track in &mut ctx.tracks {
        track.clips.sort_by(|a, b| a.position_secs.total_cmp(&b.position_secs));
        track.midi_clips.sort_by(|a, b| a.position_secs.total_cmp(&b.position_secs));
        if !track.midi_clips.is_empty() && track.instrument.as_ref().is_none_or(|i| i.format != PluginFormat::Vst3) {
            if let Some(i) = &track.instrument {
                ctx.warnings.push(format!("track '{}': instrument '{}' can't be loaded; using the built-in synth", track.name, i.name));
            }
            track.instrument = Some(synth_slot());
        }
    }
    project.tracks = ctx.tracks;
    Ok(Imported { project, warnings: ctx.warnings })
}

/// A timeline's time base: local time `t` is at `origin + t` beats (quarter notes) or seconds.
#[derive(Clone, Copy)]
struct Base {
    seconds: bool,
    origin: f64,
}

struct Ctx<'a> {
    map: TempoMap,
    tracks: Vec<Track>,
    ids: HashMap<String, usize>,
    warnings: Vec<String>,
    media: &'a mut dyn FnMut(Node) -> Result<PathBuf, String>,
    plugin_states: bool,
}

impl Ctx<'_> {
    fn secs(&self, base: Base, t: f64) -> f64 {
        if base.seconds { base.origin + t } else { self.map.secs_at(base.origin + t) }
    }

    /// Absolute position of local time `t` in the other unit's terms (for a child timeline).
    fn abs_in(&self, base: Base, t: f64, seconds: bool) -> f64 {
        match (base.seconds, seconds) {
            (true, true) | (false, false) => base.origin + t,
            (false, true) => self.map.secs_at(base.origin + t),
            (true, false) => self.map.quarters_at(base.origin + t),
        }
    }

    fn local(&self, base: Base, secs: f64) -> f64 {
        if base.seconds { secs - base.origin } else { self.map.quarters_at(secs) - base.origin }
    }

    fn structure(&mut self, node: Node, project: &mut Project) {
        let channel = if node.has_tag_name("Channel") { Some(node) } else { child(node, "Channel") };
        let role = channel.and_then(|c| c.attribute("role")).unwrap_or("regular");
        let name = node.attribute("name").or_else(|| channel.and_then(|c| c.attribute("name"))).unwrap_or("Track").to_string();
        let sub: Vec<Node> = node.children().filter(|n| n.has_tag_name("Track")).collect();
        if role == "master" {
            if let Some(c) = channel {
                project.master_volume = volume(c);
                if let Some(id) = node.attribute("id") {
                    self.ids.insert(id.into(), usize::MAX);
                }
            }
            return;
        }
        if !sub.is_empty() {
            // Folder/group: its tracks come in flat (no group buses here).
            sub.into_iter().for_each(|t| self.structure(t, project));
            return;
        }
        if !node.has_tag_name("Track") {
            return;
        }
        if matches!(role, "effect" | "submix" | "vca") {
            self.warnings.push(format!("{role} channel '{name}' isn't imported (no sends or buses)"));
            return;
        }
        let mut track = Track::new(name);
        if let Some(c) = channel {
            track.volume = volume(c);
            track.pan = child(c, "Pan").map_or(0.0, pan);
            track.mute = child(c, "Mute").and_then(|m| m.attribute("value")) == Some("true");
            track.solo = c.attribute("solo") == Some("true");
            for device in child(c, "Devices").into_iter().flat_map(|d| d.children().filter(Node::is_element)) {
                self.device(device, &mut track);
            }
        }
        if let Some(id) = node.attribute("id") {
            self.ids.insert(id.into(), self.tracks.len());
        }
        if let Some(id) = channel.and_then(|c| c.attribute("id")) {
            self.ids.insert(id.into(), self.tracks.len());
        }
        self.tracks.push(track);
    }

    fn device(&mut self, d: Node, track: &mut Track) {
        let name = d.attribute("deviceName").or(d.attribute("name")).unwrap_or("Device");
        let format = match d.tag_name().name() {
            "Vst3Plugin" => PluginFormat::Vst3,
            "Vst2Plugin" => PluginFormat::Vst2,
            "ClapPlugin" => PluginFormat::Clap,
            "AuPlugin" => PluginFormat::Other,
            _ => {
                self.warnings.push(format!("track '{}': device '{name}' isn't available here", track.name));
                return;
            }
        };
        self.plugin_states |= child(d, "State").is_some();
        let mut slot: PluginSlot = named_plugin(format, name, d.attribute("deviceVendor"), d.attribute("deviceID").unwrap_or_default());
        slot.bypass = child(d, "Enabled").and_then(|e| e.attribute("value")) == Some("false");
        if d.attribute("deviceRole") == Some("instrument") && track.instrument.is_none() {
            track.instrument = Some(slot);
        } else if d.attribute("deviceRole") != Some("noteFX") {
            track.plugins.push(slot);
        }
    }

    fn track_of(&self, node: Node, current: Option<usize>) -> Option<usize> {
        node.attribute("track").map_or(current, |id| self.ids.get(id).copied())
    }

    /// Walk a timeline element: `Lanes`, `Clips`, or bare content outside a clip.
    fn content(&mut self, node: Node, base: Base, window: (f64, f64), track: Option<usize>, name: Option<&str>) {
        let track = self.track_of(node, track);
        let base = Base { seconds: node.attribute("timeUnit").map_or(base.seconds, |u| u == "seconds"), ..base };
        match node.tag_name().name() {
            "Lanes" => node.children().filter(Node::is_element).for_each(|n| self.content(n, base, window, track, name)),
            "Clips" => node.children().filter(|n| n.has_tag_name("Clip")).for_each(|c| self.clip(c, base, window, track, name)),
            "ClipSlot" => {}
            "Notes" | "Audio" | "Warps" => self.leaf(node, name, &[(base, window)], window, track),
            _ => {}
        }
    }

    fn clip(&mut self, clip: Node, base: Base, window: (f64, f64), track: Option<usize>, name: Option<&str>) {
        let track = self.track_of(clip, track);
        let Some(time) = attr_f64(clip, "time") else { return };
        let duration = attr_f64(clip, "duration");
        let start = self.secs(base, time).max(window.0);
        let end = duration.map_or(window.1, |d| self.secs(base, time + d)).min(window.1);
        if end <= start + 1e-9 {
            return;
        }
        let cu_seconds = clip.attribute("contentTimeUnit").map_or(base.seconds, |u| u == "seconds");
        let play_start = attr_f64(clip, "playStart").unwrap_or(0.0);
        // Each pass through the content (one, or several for a looped clip): (offset in
        // the clip, content time it starts at, length).
        let looped = (attr_f64(clip, "loopStart"), attr_f64(clip, "loopEnd"), duration);
        let passes: Vec<(f64, f64, f64)> = match looped {
            (Some(ls), Some(le), Some(d)) if le > ls + 1e-9 && cu_seconds == base.seconds && play_start < le => {
                let mut v = vec![(0.0, play_start, le - play_start)];
                while let Some(&(off, _, len)) = v.last().filter(|p| p.0 + p.2 < d - 1e-9 && v.len() < 10_000) {
                    v.push((off + len, ls, le - ls));
                }
                v
            }
            _ => vec![(0.0, play_start, duration.unwrap_or(f64::INFINITY))],
        };
        let bases: Vec<(Base, (f64, f64))> = passes
            .iter()
            .map(|&(off, content, len)| {
                let b = Base { seconds: cu_seconds, origin: self.abs_in(base, time + off, cu_seconds) - content };
                let w = (self.secs(base, time + off).max(start), if len.is_finite() { self.secs(base, time + off + len).min(end) } else { end });
                (b, w)
            })
            .filter(|(_, w)| w.1 > w.0 + 1e-9)
            .collect();
        let name = clip.attribute("name").or(name);
        let before: Vec<(usize, usize)> = self.tracks.iter().map(|t| (t.clips.len(), t.midi_clips.len())).collect();
        for content in clip.children().filter(Node::is_element) {
            match content.tag_name().name() {
                "Notes" | "Audio" | "Warps" => self.leaf(content, name, &bases, (start, end), track),
                _ => bases.iter().for_each(|&(b, w)| self.content(content, b, w, track, name)),
            }
        }
        // A disabled clip mutes everything it holds.
        if clip.attribute("enable") == Some("false") {
            for (t, (a, m)) in self.tracks.iter_mut().zip(before) {
                t.clips[a..].iter_mut().for_each(|c| c.mute = true);
                t.midi_clips[m..].iter_mut().for_each(|c| c.mute = true);
            }
        }
    }

    /// Notes or audio played through `passes` (time base and window of each pass).
    fn leaf(&mut self, node: Node, name: Option<&str>, passes: &[(Base, (f64, f64))], span: (f64, f64), track: Option<usize>) {
        let track = self.track_of(node, track);
        let Some(t) = track else { return };
        if t == usize::MAX {
            return; // master lane
        }
        match node.tag_name().name() {
            "Notes" => {
                let mut notes = Vec::new();
                for &(base, (ws, we)) in passes {
                    for n in node.children().filter(|n| n.has_tag_name("Note")) {
                        let (Some(time), Some(dur), Some(key)) = (attr_f64(n, "time"), attr_f64(n, "duration"), attr_f64(n, "key")) else { continue };
                        let (s, e) = (self.secs(base, time), self.secs(base, time + dur).min(we));
                        if s < ws - 1e-9 || s >= we - 1e-9 || !(0.0..=127.0).contains(&key) {
                            continue;
                        }
                        let velocity = (attr_f64(n, "vel").unwrap_or(0.8) * 127.0).round().clamp(1.0, 127.0) as u8;
                        notes.push(Note { start_secs: s, length_secs: (e - s).max(0.001), pitch: key as u8, velocity, tab: None });
                    }
                }
                let start = span.0;
                let end = if span.1.is_finite() { span.1 } else { notes.iter().map(|n| n.start_secs + n.length_secs).fold(start, f64::max) };
                notes.iter_mut().for_each(|n| n.start_secs -= start);
                notes.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));
                let clip = MidiClip { name: name.unwrap_or_default().to_string(), position_secs: start, length_secs: end - start, mute: false, notes, tab: None, tempos: Vec::new(), controllers: Vec::new(), looped: None };
                self.tracks[t].midi_clips.push(clip);
            }
            _ => {
                let (audio, warps) = if node.has_tag_name("Warps") { (child(node, "Audio"), Some(node)) } else { (Some(node), None) };
                let Some(audio) = audio else { return };
                let Some(file) = child(audio, "File") else { return };
                let file = match (self.media)(file) {
                    Ok(f) => f,
                    Err(e) => {
                        self.warnings.push(format!("track '{}': {e}", self.tracks[t].name));
                        return;
                    }
                };
                let file_len = attr_f64(audio, "duration").unwrap_or(f64::INFINITY);
                let warp_points: Vec<(f64, f64)> = warps
                    .into_iter()
                    .flat_map(|w| w.children().filter(|n| n.has_tag_name("Warp")))
                    .filter_map(|w| Some((attr_f64(w, "time")?, attr_f64(w, "contentTime")?)))
                    .collect();
                let warp_base_seconds = warps.and_then(|w| w.attribute("timeUnit")).map(|u| u == "seconds");
                let mut stretched = false;
                for &(base, (ws, we)) in passes {
                    let base = Base { seconds: warp_base_seconds.unwrap_or(base.seconds), ..base };
                    let (offset, end_offset) = if warp_points.len() >= 2 {
                        (warp(&warp_points, self.local(base, ws)), warp(&warp_points, self.local(base, we)))
                    } else {
                        // Unwarped: the file starts at the content's time 0.
                        let zero = self.secs(base, 0.0);
                        (ws - zero, we - zero)
                    };
                    stretched |= ((end_offset - offset) / (we - ws) - 1.0).abs() > 0.01;
                    let we = if file_len.is_finite() && !we.is_finite() { ws + file_len - offset } else { we };
                    let clip_name = name.map(str::to_string).unwrap_or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
                    self.tracks[t].clips.push(audio_clip(clip_name, file.clone(), ws, we - ws, offset));
                }
                if stretched {
                    let name = self.tracks[t].name.clone();
                    self.warnings.push(format!("track '{name}': time-stretched clip of {} plays at its original speed", file.display()));
                }
            }
        }
    }
}

/// Map a content time through warp markers (piecewise linear, extended at both ends).
fn warp(points: &[(f64, f64)], t: f64) -> f64 {
    let i = points.partition_point(|p| p.0 <= t).clamp(1, points.len() - 1);
    let ((t0, c0), (t1, c1)) = (points[i - 1], points[i]);
    if (t1 - t0).abs() < 1e-12 { c0 } else { c0 + (t - t0) * (c1 - c0) / (t1 - t0) }
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name(name))
}

fn attr_f64(node: Node, name: &str) -> Option<f64> {
    node.attribute(name)?.trim().parse().ok().filter(|v: &f64| v.is_finite())
}

/// "#rrggbb" (alpha, if any, ignored).
fn hex_color(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    let byte = |i: usize| h.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok());
    Some([byte(0)?, byte(2)?, byte(4)?])
}

fn signature(node: Node) -> Option<(u32, u32)> {
    Some((node.attribute("numerator")?.parse().ok()?, node.attribute("denominator")?.parse().ok()?))
}

fn volume(channel: Node) -> f32 {
    let Some(v) = child(channel, "Volume") else { return 1.0 };
    let value = attr_f64(v, "value").unwrap_or(1.0);
    match v.attribute("unit") {
        Some("decibel") => db_to_gain(value),
        _ => value.max(0.0) as f32,
    }
}

fn pan(p: Node) -> f32 {
    let value = attr_f64(p, "value").unwrap_or(0.5);
    let (min, max) = match p.attribute("unit") {
        Some("percent") => (-100.0, 100.0),
        Some("linear") => (-1.0, 1.0),
        _ => (0.0, 1.0),
    };
    let (min, max) = (attr_f64(p, "min").unwrap_or(min), attr_f64(p, "max").unwrap_or(max));
    if max - min < 1e-9 { 0.0 } else { (((value - min) / (max - min)) * 2.0 - 1.0).clamp(-1.0, 1.0) as f32 }
}

// ------------------------------------------------------------------------------ export

/// Write a DAWproject archive (audio files embedded) that Bitwig, Studio One and Cubase
/// can open. Positions are written in beats along the tempo map. Built-in plug-ins, plug-in
/// parameter values and take folders' inactive takes aren't exported. Returns warnings.
pub fn save(project: &Project, path: &Path) -> Result<Vec<String>, ImportError> {
    let tmp = path.with_extension("dawproject.tmp");
    let warnings = write(project, std::fs::File::create(&tmp)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(warnings)
}

pub fn write<W: Write + Seek>(project: &Project, out: W) -> Result<Vec<String>, ImportError> {
    let mut warnings = Vec::new();
    let mut zip = zip::ZipWriter::new(out);
    let deflate = zip::write::SimpleFileOptions::default();
    // Audio is already dense; storing it saves time.
    let stored = deflate.compression_method(zip::CompressionMethod::Stored).large_file(true);
    let mut files: HashMap<PathBuf, String> = HashMap::new();
    for clip in project.tracks.iter().flat_map(|t| &t.clips) {
        if files.contains_key(&clip.file) {
            continue;
        }
        let stem = clip.file.file_name().map_or("audio.wav".into(), |n| n.to_string_lossy().into_owned());
        let mut name = format!("audio/{stem}");
        let mut i = 1;
        while files.values().any(|v| *v == name) {
            i += 1;
            name = format!("audio/{i}-{stem}");
        }
        match std::fs::File::open(&clip.file) {
            Ok(mut f) => {
                zip.start_file(name.as_str(), stored)?;
                std::io::copy(&mut f, &mut zip)?;
            }
            Err(e) => warnings.push(format!("{}: {e}; referenced, not embedded", clip.file.display())),
        }
        files.insert(clip.file.clone(), name);
    }
    for p in project.tracks.iter().flat_map(|t| t.instrument.iter().chain(&t.plugins)).chain(&project.master_plugins) {
        if p.format == PluginFormat::Builtin {
            warnings.push(format!("built-in '{}' isn't exported (the other DAW doesn't have it)", p.name));
        }
    }
    zip.start_file("project.xml", deflate)?;
    zip.write_all(project_xml(project, &files).as_bytes())?;
    zip.start_file("metadata.xml", deflate)?;
    zip.write_all(format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<MetaData>\n  <Title>{}</Title>\n</MetaData>\n", esc(&project.name)).as_bytes())?;
    zip.finish()?;
    Ok(warnings)
}

/// `project.xml` for `project`; `files` maps media paths to their names in the archive
/// (others are written as external references).
pub fn project_xml(project: &Project, files: &HashMap<PathBuf, String>) -> String {
    use std::fmt::Write as _;
    let map = project.tempo_map();
    let q = |secs: f64| map.quarters_at(secs);
    let mut id = 0;
    let mut next = || {
        id += 1;
        format!("id{id}")
    };
    let mut x = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<Project version=\"1.0\">\n");
    let _ = writeln!(x, "  <Application name=\"OpenTake\" version=\"{}\"/>", env!("CARGO_PKG_VERSION"));
    let (num, den) = project.time_signature;
    let _ = writeln!(x, "  <Transport>\n    <Tempo unit=\"bpm\" value=\"{}\" min=\"20\" max=\"999\" id=\"{}\" name=\"Tempo\"/>", project.tempo_bpm, next());
    let _ = writeln!(x, "    <TimeSignature numerator=\"{num}\" denominator=\"{den}\" id=\"{}\"/>\n  </Transport>\n  <Structure>", next());
    let master_id = "master".to_string();
    let mut track_ids = Vec::new();
    for t in &project.tracks {
        let (tid, cid) = (next(), next());
        let kind = if t.is_instrument() { "notes" } else { "audio" };
        let _ = writeln!(x, "    <Track contentType=\"{kind}\" loaded=\"true\" id=\"{tid}\" name=\"{}\">", esc(&t.name));
        let _ = writeln!(x, "      <Channel audioChannels=\"2\" destination=\"{master_id}\" role=\"regular\" solo=\"{}\" id=\"{cid}\">", t.solo);
        let plugins: Vec<(&PluginSlot, &str)> = t.instrument.iter().map(|p| (p, "instrument")).chain(t.plugins.iter().map(|p| (p, "audioFX"))).filter(|(p, _)| p.format != PluginFormat::Builtin).collect();
        if !plugins.is_empty() {
            x.push_str("        <Devices>\n");
            for (p, role) in plugins {
                let tag = match p.format {
                    PluginFormat::Vst3 => "Vst3Plugin",
                    PluginFormat::Vst2 => "Vst2Plugin",
                    PluginFormat::Clap => "ClapPlugin",
                    _ => "AuPlugin",
                };
                // Reaper-style "<id>{<cid>}" uids carry the VST3 class id in braces.
                let uid = p.uid.rsplit('{').next().unwrap_or_default().trim_end_matches('}');
                let name = plugin_display_name(&p.name);
                let _ = writeln!(x, "          <{tag} deviceID=\"{}\" deviceName=\"{}\" deviceRole=\"{role}\" loaded=\"true\" id=\"{}\" name=\"{}\">", esc(uid), esc(&name), next(), esc(&name));
                let _ = writeln!(x, "            <Enabled value=\"{}\" id=\"{}\" name=\"On/Off\"/>\n          </{tag}>", !p.bypass, next());
            }
            x.push_str("        </Devices>\n");
        }
        let _ = writeln!(x, "        <Mute value=\"{}\" id=\"{}\" name=\"Mute\"/>", t.mute, next());
        let _ = writeln!(x, "        <Pan unit=\"normalized\" min=\"0\" max=\"1\" value=\"{}\" id=\"{}\" name=\"Pan\"/>", (t.pan as f64 + 1.0) / 2.0, next());
        let _ = writeln!(x, "        <Volume unit=\"linear\" min=\"0\" max=\"2\" value=\"{}\" id=\"{}\" name=\"Volume\"/>\n      </Channel>\n    </Track>", t.volume, next());
        track_ids.push(tid);
    }
    let _ = writeln!(x, "    <Track contentType=\"audio notes\" loaded=\"true\" id=\"{}\" name=\"Master\">", next());
    let _ = writeln!(x, "      <Channel audioChannels=\"2\" role=\"master\" solo=\"false\" id=\"{master_id}\">");
    let _ = writeln!(x, "        <Volume unit=\"linear\" min=\"0\" max=\"2\" value=\"{}\" id=\"{}\" name=\"Volume\"/>\n      </Channel>\n    </Track>\n  </Structure>", project.master_volume, next());

    let _ = writeln!(x, "  <Arrangement id=\"{}\">\n    <Lanes timeUnit=\"beats\" id=\"{}\">", next(), next());
    for (t, tid) in project.tracks.iter().zip(&track_ids) {
        let _ = writeln!(x, "      <Lanes track=\"{tid}\" id=\"{}\">\n        <Clips id=\"{}\">", next(), next());
        // Looped regions are written out as their passes.
        for c in t.clips.iter().flat_map(Clip::loop_regions) {
            let c = &c;
            let (start, end) = (q(c.position_secs), q(c.position_secs + c.length_secs));
            let _ = writeln!(x, "          <Clip time=\"{start}\" duration=\"{}\" playStart=\"0\" name=\"{}\"{}>", end - start, esc(&c.name), if c.mute { " enable=\"false\"" } else { "" });
            let _ = writeln!(x, "            <Warps contentTimeUnit=\"seconds\" timeUnit=\"beats\" id=\"{}\">", next());
            let wav_info = audio_info(&c.file);
            let (file, external) = match files.get(&c.file) {
                Some(name) => (name.clone(), ""),
                None => (c.file.to_string_lossy().into_owned(), " external=\"true\""),
            };
            let _ = writeln!(x, "              <Audio channels=\"{}\" sampleRate=\"{}\" duration=\"{}\" algorithm=\"raw\" id=\"{}\">", wav_info.0, wav_info.1, wav_info.2.unwrap_or(c.source_offset_secs + c.length_secs), next());
            let _ = writeln!(x, "                <File path=\"{}\"{external}/>\n              </Audio>", esc(&file));
            // Warp markers at the clip edges and at every tempo change inside keep the audio
            // at its own speed whatever the tempo does.
            let mut marks = vec![c.position_secs];
            marks.extend(project.tempo_changes.iter().map(|tc| tc.at_secs).filter(|&s| s > c.position_secs && s < c.position_secs + c.length_secs));
            marks.push(c.position_secs + c.length_secs);
            for s in marks {
                let _ = writeln!(x, "              <Warp time=\"{}\" contentTime=\"{}\"/>", q(s) - start, c.source_offset_secs + s - c.position_secs);
            }
            x.push_str("            </Warps>\n          </Clip>\n");
        }
        for m in t.midi_clips.iter().map(MidiClip::unrolled) {
            let (start, end) = (q(m.position_secs), q(m.position_secs + m.length_secs));
            let _ = writeln!(x, "          <Clip time=\"{start}\" duration=\"{}\" playStart=\"0\" name=\"{}\"{}>", end - start, esc(&m.name), if m.mute { " enable=\"false\"" } else { "" });
            let _ = writeln!(x, "            <Notes id=\"{}\">", next());
            for n in &m.notes {
                let s = q(m.position_secs + n.start_secs);
                let e = q(m.position_secs + n.start_secs + n.length_secs);
                let vel = n.velocity as f64 / 127.0;
                let _ = writeln!(x, "              <Note time=\"{}\" duration=\"{}\" channel=\"0\" key=\"{}\" vel=\"{vel}\" rel=\"{vel}\"/>", s - start, e - s, n.pitch);
            }
            x.push_str("            </Notes>\n          </Clip>\n");
        }
        x.push_str("        </Clips>\n      </Lanes>\n");
    }
    x.push_str("    </Lanes>\n");
    if !project.tempo_changes.is_empty() {
        let mut changes = project.tempo_changes.clone();
        changes.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
        let _ = writeln!(x, "    <TempoAutomation unit=\"bpm\" id=\"{}\">\n      <Target parameter=\"id1\"/>", next());
        let _ = writeln!(x, "      <RealPoint time=\"0\" value=\"{}\" interpolation=\"hold\"/>", project.tempo_bpm);
        for c in &changes {
            let _ = writeln!(x, "      <RealPoint time=\"{}\" value=\"{}\" interpolation=\"hold\"/>", q(c.at_secs), c.bpm);
        }
        x.push_str("    </TempoAutomation>\n");
        if changes.iter().any(|c| c.time_signature.is_some()) {
            let _ = writeln!(x, "    <TimeSignatureAutomation id=\"{}\">\n      <Target parameter=\"id2\"/>", next());
            let _ = writeln!(x, "      <TimeSignaturePoint time=\"0\" numerator=\"{num}\" denominator=\"{den}\"/>");
            for c in &changes {
                if let Some((n, d)) = c.time_signature {
                    let _ = writeln!(x, "      <TimeSignaturePoint time=\"{}\" numerator=\"{n}\" denominator=\"{d}\"/>", q(c.at_secs));
                }
            }
            x.push_str("    </TimeSignatureAutomation>\n");
        }
    }
    if !project.markers.is_empty() {
        let _ = writeln!(x, "    <Markers timeUnit=\"beats\" id=\"{}\">", next());
        for m in &project.markers {
            let color = m.color.map(|[r, g, b]| format!(" color=\"#{r:02x}{g:02x}{b:02x}\"")).unwrap_or_default();
            let _ = writeln!(x, "      <Marker time=\"{}\" name=\"{}\"{color}/>", q(m.time_secs), esc(&m.name));
        }
        x.push_str("    </Markers>\n");
    }
    x.push_str("  </Arrangement>\n  <Scenes/>\n</Project>\n");
    x
}

/// Channels, sample rate and duration (seconds) of a WAV file, with fallbacks.
fn audio_info(path: &Path) -> (u16, u32, Option<f64>) {
    wav_header(path).unwrap_or((2, 48_000, None))
}

fn wav_header(path: &Path) -> Option<(u16, u32, Option<f64>)> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf).ok()?;
    let buf = &buf[..n];
    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return None;
    }
    let total = f.metadata().ok()?.len();
    let (mut pos, mut fmt) = (12, None);
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let size = u32::from_le_bytes(buf[pos + 4..pos + 8].try_into().ok()?) as usize;
        if id == b"fmt " && pos + 24 <= buf.len() {
            let channels = u16::from_le_bytes([buf[pos + 10], buf[pos + 11]]);
            let rate = u32::from_le_bytes(buf[pos + 12..pos + 16].try_into().ok()?);
            let byte_rate = u32::from_le_bytes(buf[pos + 16..pos + 20].try_into().ok()?);
            fmt = Some((channels, rate, byte_rate));
        } else if id == b"data" {
            let (channels, rate, byte_rate) = fmt?;
            let data = (size as u64).min(total.saturating_sub(pos as u64 + 8));
            return Some((channels, rate, (byte_rate > 0).then(|| data as f64 / byte_rate as f64)));
        }
        pos += 8 + size + (size & 1);
    }
    fmt.map(|(c, r, _)| (c, r, None))
}

/// "VST3: Diva (u-he)" -> "Diva"
fn plugin_display_name(full: &str) -> String {
    let rest = full.split_once(": ").map_or(full, |(_, r)| r);
    rest.rsplit_once(" (").map_or(rest, |(n, _)| n).to_string()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Project version="1.0">
  <Application name="Bitwig Studio" version="5.0"/>
  <Transport>
    <Tempo max="666.000000" min="20.000000" unit="bpm" value="120.000000" id="id0" name="Tempo"/>
    <TimeSignature denominator="4" numerator="3" id="id1"/>
  </Transport>
  <Structure>
    <Track contentType="notes" loaded="true" id="id2" name="Bass" color="#a2eabf">
      <Channel audioChannels="2" destination="id15" role="regular" solo="true" id="id3">
        <Devices>
          <Vst3Plugin deviceID="ABCDEF0123456789ABCDEF0123456789" deviceName="Diva" deviceVendor="u-he" deviceRole="instrument" loaded="true" id="id7" name="Diva">
            <Parameters/>
            <Enabled value="true" id="id8" name="On/Off"/>
            <State path="plugins/diva.vstpreset"/>
          </Vst3Plugin>
          <Vst3Plugin deviceID="0011" deviceName="Pro-Q 3" deviceRole="audioFX" id="id9">
            <Enabled value="false" id="id10" name="On/Off"/>
          </Vst3Plugin>
        </Devices>
        <Mute value="false" id="id6" name="Mute"/>
        <Pan max="1.000000" min="0.000000" unit="normalized" value="0.250000" id="id5" name="Pan"/>
        <Volume max="2.000000" min="0.000000" unit="linear" value="0.5" id="id4" name="Volume"/>
      </Channel>
    </Track>
    <Track contentType="tracks" id="id30" name="Drums Folder">
      <Channel role="submix" id="id31"/>
      <Track contentType="audio" loaded="true" id="id9b" name="Drumloop">
        <Channel audioChannels="2" destination="id15" role="regular" id="id10b">
          <Mute value="true" id="id13" name="Mute"/>
          <Volume unit="decibel" value="-6.0" id="id11" name="Volume"/>
        </Channel>
      </Track>
    </Track>
    <Track contentType="audio" id="id40" name="Return"><Channel role="effect" id="id41"/></Track>
    <Track contentType="audio notes" loaded="true" id="id14" name="Master">
      <Channel audioChannels="2" role="master" solo="false" id="id15">
        <Volume max="2.000000" min="0.000000" unit="linear" value="0.8" id="id16" name="Volume"/>
      </Channel>
    </Track>
  </Structure>
  <Arrangement id="id19">
    <Lanes timeUnit="beats" id="id20">
      <Lanes track="id2" id="id21">
        <Clips id="id22">
          <Clip time="4.0" duration="8.0" playStart="0.0" loopStart="0.0" loopEnd="4.0" name="Riff">
            <Notes id="id23">
              <Note time="0.0" duration="1.0" channel="0" key="40" vel="1.0" rel="0.5"/>
              <Note time="2.0" duration="4.0" channel="0" key="43" vel="0.5"/>
              <Note time="5.0" duration="1.0" channel="0" key="45" vel="0.5"/>
            </Notes>
          </Clip>
        </Clips>
      </Lanes>
      <Lanes track="id9b" id="id24">
        <Clips id="id25">
          <Clip time="8.0" duration="4.0" playStart="2.0" name="Drumfunk">
            <Clips id="id26">
              <Clip time="0.0" duration="8.0" contentTimeUnit="beats" playStart="0.0">
                <Warps contentTimeUnit="seconds" timeUnit="beats" id="id28">
                  <Audio algorithm="stretch" channels="2" duration="4.0" sampleRate="48000" id="id27">
                    <File path="audio/drums.wav"/>
                  </Audio>
                  <Warp time="0.0" contentTime="0.0"/>
                  <Warp time="8.0" contentTime="4.0"/>
                </Warps>
              </Clip>
            </Clips>
          </Clip>
          <Clip time="16.0" duration="2.0" enable="false">
            <Audio channels="2" duration="10.0" sampleRate="48000"><File path="/elsewhere/missing.wav" external="true"/></Audio>
          </Clip>
        </Clips>
      </Lanes>
    </Lanes>
    <TempoAutomation unit="bpm"><Target parameter="id0"/>
      <RealPoint time="0" value="120" interpolation="hold"/>
      <RealPoint time="24" value="60" interpolation="hold"/>
    </TempoAutomation>
  </Arrangement>
  <Scenes/>
</Project>"##;

    fn parse_sample(xml: &str) -> Imported {
        parse(xml, &mut |f: Node| Ok(PathBuf::from("/media").join(f.attribute("path").unwrap()))).unwrap()
    }

    #[test]
    fn imports_tracks_mixer_and_plugins() {
        let Imported { project: p, warnings } = parse_sample(SAMPLE);
        assert_eq!((p.tempo_bpm, p.time_signature), (120.0, (3, 4)));
        assert_eq!(p.tempo_changes.len(), 1);
        assert!((p.tempo_changes[0].at_secs - 12.0).abs() < 1e-9 && p.tempo_changes[0].bpm == 60.0);
        assert_eq!(p.master_volume, 0.8);
        assert_eq!(p.tracks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["Bass", "Drumloop"]);
        let bass = &p.tracks[0];
        assert_eq!((bass.volume, bass.pan, bass.mute, bass.solo), (0.5, -0.5, false, true));
        let inst = bass.instrument.as_ref().unwrap();
        assert_eq!((inst.name.as_str(), inst.path.as_str(), inst.uid.as_str()), ("VST3: Diva (u-he)", "Diva.vst3", "ABCDEF0123456789ABCDEF0123456789"));
        assert!(bass.plugins[0].bypass);
        let drums = &p.tracks[1];
        assert!(drums.mute && (drums.volume - 0.501).abs() < 1e-3);
        assert!(warnings.iter().any(|w| w.contains("presets")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("effect channel 'Return'")), "{warnings:?}");
    }

    #[test]
    fn imports_looped_midi_and_nested_audio_clips() {
        let p = parse_sample(SAMPLE).project;
        // 120 bpm: a beat is 0.5 s. The clip loops its first 4 beats twice.
        let m = &p.tracks[0].midi_clips[0];
        assert_eq!((m.name.as_str(), m.position_secs, m.length_secs), ("Riff", 2.0, 4.0));
        let notes: Vec<(f64, f64, u8, u8)> = m.notes.iter().map(|n| (n.start_secs, n.length_secs, n.pitch, n.velocity)).collect();
        // The long note is cut at the loop end; the note after the loop end never plays.
        assert_eq!(notes, vec![(0.0, 0.5, 40, 127), (1.0, 1.0, 43, 64), (2.0, 0.5, 40, 127), (3.0, 1.0, 43, 64)]);

        let clips = &p.tracks[1].clips;
        assert_eq!(clips.len(), 2);
        // Outer clip at beat 8 (4 s), playStart 2 beats into the inner timeline: the audio
        // (warped 8 beats -> 4 s, i.e. natural speed at 120) starts 1 s in.
        let c = &clips[0];
        // Compared as paths: joined paths use the platform's separator.
        assert_eq!((c.file.as_path(), c.position_secs, c.length_secs, c.source_offset_secs), (Path::new("/media/audio/drums.wav"), 4.0, 2.0, 1.0));
        assert_eq!(c.name, "Drumfunk");
        assert!(clips[1].mute && clips[1].position_secs == 8.0 && clips[1].file == Path::new("/elsewhere/missing.wav"));
    }

    #[test]
    fn tolerates_minimal_documents() {
        let p = parse_sample(r#"<Project version="1.0"><Application name="x" version="1"/><Structure><Track name="A"/></Structure></Project>"#).project;
        assert_eq!(p.tracks.len(), 1);
        assert!(parse(r#"<NotAProject/>"#, &mut |_| Err(String::new())).is_err());
    }

    #[test]
    fn archive_round_trip_extracts_and_embeds_audio() {
        let dir = std::env::temp_dir().join(format!("opentake-dawproject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A tiny real WAV so the header is read back.
        let wav = dir.join("take.wav");
        let mut bytes = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 0, 1, 0]);
        bytes.extend_from_slice(&8000u32.to_le_bytes());
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.extend_from_slice(&[2, 0, 16, 0]);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&16000u32.to_le_bytes());
        bytes.resize(bytes.len() + 16000, 0);
        std::fs::write(&wav, &bytes).unwrap();

        let mut p = Project { name: "Song".into(), tempo_bpm: 90.0, ..Project::default() };
        p.tempo_changes.push(crate::TempoChange { at_secs: 8.0, bpm: 120.0, time_signature: Some((6, 8)) });
        let mut audio = Track::new("Vox");
        audio.pan = 0.5;
        audio.clips.push(audio_clip("v1", wav.clone(), 6.0, 4.0, 0.25));
        let mut keys = Track::new("Keys");
        keys.instrument = Some(synth_slot());
        keys.midi_clips.push(MidiClip {
            name: "chords".into(),
            position_secs: 2.0,
            length_secs: 8.0,
            mute: false,
            notes: vec![Note { start_secs: 0.5, length_secs: 7.0, pitch: 60, velocity: 100, tab: None }],
            tab: None,
            tempos: Vec::new(),
            controllers: Vec::new(),
            looped: None,
        });
        p.tracks = vec![audio, keys];
        p.markers = vec![Marker::new(2.0, "Verse & Co"), Marker { color: Some([255, 0, 128]), ..Marker::new(10.0, "Chorus") }];
        p.tidy_markers();

        let file = dir.join("Song.dawproject");
        let warnings = save(&p, &file).unwrap();
        assert!(warnings.iter().any(|w| w.contains("OpenTake Synth")), "{warnings:?}");
        let back = load(&file).unwrap();
        let q = &back.project;
        assert_eq!((q.name.as_str(), q.tempo_bpm, q.tracks.len()), ("Song", 90.0, 2));
        assert_eq!(q.tempo_changes.len(), 1);
        assert!((q.tempo_changes[0].at_secs - 8.0).abs() < 1e-9 && q.tempo_changes[0].time_signature == Some((6, 8)));
        let c = &q.tracks[0].clips[0];
        assert_eq!(c.file, dir.join("Song Media/audio/take.wav"));
        assert_eq!(std::fs::read(&c.file).unwrap(), bytes);
        assert!((c.position_secs - 6.0).abs() < 1e-9 && (c.length_secs - 4.0).abs() < 1e-9 && (c.source_offset_secs - 0.25).abs() < 1e-9);
        assert_eq!(q.tracks[0].pan, 0.5);
        let m = &q.tracks[1].midi_clips[0];
        assert!((m.position_secs - 2.0).abs() < 1e-9 && (m.length_secs - 8.0).abs() < 1e-9);
        let n = m.notes[0];
        assert!((n.start_secs - 0.5).abs() < 1e-9 && (n.length_secs - 7.0).abs() < 1e-9 && n.pitch == 60 && n.velocity == 100);
        assert!(!back.warnings.iter().any(|w| w.contains("original speed")), "{:?}", back.warnings);
        // Markers, in beats in the file, come back at their times (across the tempo change).
        let markers: Vec<_> = q.markers.iter().map(|m| ((m.time_secs * 1e6).round() / 1e6, m.name.as_str(), m.color)).collect();
        assert_eq!(markers, vec![(2.0, "Verse & Co", None), (10.0, "Chorus", Some([255, 0, 128]))]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
