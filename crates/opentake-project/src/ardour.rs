//! Ardour session (`.ardour`) import, Ardour 5–8.
//!
//! Imported: tempo map (tempo and meter changes), audio tracks (name, gain, pan, mute,
//! solo, VST3 plug-in references), their playlists' regions (position, length, source
//! offset, gain, mute) and the master bus gain. Stereo regions recorded as two mono files
//! use the first one. Not imported: MIDI regions (Ardour keeps them in separate `.mid`
//! files), buses, sends, automation, fades, LV2/LADSPA/AU plug-ins.

use std::path::{Path, PathBuf};

use roxmltree::Node;

use crate::import::{ImportError, Imported, audio_clip, find_media, named_plugin, synth_slot};
use crate::model::{PluginFormat, Project, Track};
use crate::tempo::{TempoChange, TempoMap};

pub fn load(path: &Path) -> Result<Imported, ImportError> {
    let xml = std::fs::read_to_string(path)?;
    let mut imported = from_str(&xml, path.parent().unwrap_or(Path::new(".")))?;
    if imported.project.name.is_empty() {
        imported.project.name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }
    Ok(imported)
}

/// Ardour 7's default superclock rate (sessions state theirs in the tempo map).
const SUPERCLOCK_RATE: f64 = 282_240_000.0;
const TICKS_PER_BEAT: f64 = 1920.0;

/// Parse a session; media resolves against `session_dir`.
pub fn from_str(xml: &str, session_dir: &Path) -> Result<Imported, ImportError> {
    let doc = crate::import::parse_xml(xml)?;
    let session = doc.root_element();
    if !session.has_tag_name("Session") {
        return Err(ImportError::Invalid("not an Ardour session".into()));
    }
    let name = session.attribute("name").unwrap_or_default().to_string();
    let sample_rate = session.attribute("sample-rate").and_then(|s| s.parse::<u32>().ok()).filter(|&s| s > 0).unwrap_or(48_000);
    let mut project = Project { name: name.clone(), sample_rate, tracks: Vec::new(), ..Project::default() };
    let mut warnings = Vec::new();

    let tempo_map = child(session, "TempoMap");
    let scps = tempo_map.and_then(|t| attr(t, "superclocks-per-second")).unwrap_or(SUPERCLOCK_RATE);
    let mut time = Time { sample_rate: sample_rate as f64, scps, map: project.tempo_map() };
    if let Some(tm) = tempo_map {
        // Ardour 7+ nests them in <Tempos>/<Meters>; older sessions list them directly.
        let points = |tag: &str| tm.descendants().filter(|n| n.has_tag_name(tag)).collect::<Vec<_>>();
        let mut changes: Vec<TempoChange> = Vec::new();
        for t in points("Tempo") {
            let Some(bpm) = attr(t, "npm").or_else(|| attr(t, "beats-per-minute")) else { continue };
            let note = attr(t, "note-type").filter(|&n| n > 0.0).unwrap_or(4.0);
            changes.push(TempoChange { at_secs: time.point(t), bpm: bpm * 4.0 / note, time_signature: None });
        }
        for m in points("Meter") {
            let (Some(n), Some(d)) = (attr(m, "divisions-per-bar"), attr(m, "note-value").or_else(|| attr(m, "note-type"))) else { continue };
            let at = time.point(m);
            match changes.iter_mut().find(|c| (c.at_secs - at).abs() < 1e-6) {
                Some(c) => c.time_signature = Some((n as u32, d as u32)),
                None => changes.push(TempoChange { at_secs: at, bpm: f64::NAN, time_signature: Some((n as u32, d as u32)) }),
            }
        }
        changes.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
        // A meter change keeps the tempo playing there.
        let mut bpm = project.tempo_bpm;
        for c in &mut changes {
            if c.bpm.is_nan() { c.bpm = bpm } else { bpm = c.bpm }
        }
        if let Some(first) = changes.first().filter(|c| c.at_secs <= 1e-6).copied() {
            project.tempo_bpm = first.bpm;
            project.time_signature = first.time_signature.unwrap_or(project.time_signature);
            changes.remove(0);
        }
        project.tempo_changes = changes;
        time.map = project.tempo_map();
    }

    let sources: Vec<Node> = child(session, "Sources").into_iter().flat_map(|s| s.children()).filter(|s| s.has_tag_name("Source")).collect();
    let playlists: Vec<Node> = child(session, "Playlists").into_iter().flat_map(|p| p.children()).filter(|p| p.has_tag_name("Playlist")).collect();
    let audio_dirs = audio_dirs(session_dir, &name);
    let mut skipped_plugins = Vec::new();
    for route in child(session, "Routes").into_iter().flat_map(|r| r.children()).filter(|r| r.has_tag_name("Route")) {
        let flags = child(route, "PresentationInfo").and_then(|p| p.attribute("flags")).unwrap_or_default();
        let route_name = route.attribute("name").unwrap_or("Track").to_string();
        let gain = route.descendants().find(|c| c.has_tag_name("Controllable") && c.attribute("name") == Some("gaincontrol")).and_then(|c| attr(c, "value"));
        if flags.contains("MasterOut") {
            project.master_volume = gain.unwrap_or(1.0) as f32;
            continue;
        }
        if flags.contains("MonitorOut") {
            continue;
        }
        let midi = flags.contains("MidiTrack") || route.attribute("default-type") == Some("midi") && !flags.contains("Bus");
        if flags.contains("Bus") || !flags.contains("Track") && !route.has_attribute("audio-playlist") && !route.has_attribute("playlist") && child(route, "Diskstream").is_none() {
            warnings.push(format!("bus '{route_name}' isn't imported (no buses or sends)"));
            continue;
        }
        let mut track = Track::new(route_name.clone());
        track.volume = gain.unwrap_or(1.0) as f32;
        let control = |name: &str| route.children().find(|c| c.has_tag_name("Controllable") && c.attribute("name") == Some(name));
        track.mute = control("mute").and_then(|c| attr(c, "value")).is_some_and(|v| v != 0.0);
        track.solo = control("solo").is_some_and(|c| attr(c, "value").is_some_and(|v| v != 0.0) || c.attribute("self-solo") == Some("1"));
        let azimuth = route.descendants().find(|c| c.has_tag_name("Controllable") && c.attribute("name") == Some("pan-azimuth")).and_then(|c| attr(c, "value"));
        track.pan = azimuth.map_or(0.0, |a| (a * 2.0 - 1.0).clamp(-1.0, 1.0) as f32);
        for p in route.children().filter(|p| p.has_tag_name("Processor")) {
            match p.attribute("type") {
                Some("vst3") => {
                    let uid = p.children().find_map(|c| c.attribute("unique-id")).unwrap_or_default();
                    let mut slot = named_plugin(PluginFormat::Vst3, p.attribute("name").unwrap_or("VST3"), None, uid);
                    slot.bypass = p.attribute("active") == Some("0");
                    if midi && track.instrument.is_none() && track.plugins.is_empty() {
                        track.instrument = Some(slot);
                    } else {
                        track.plugins.push(slot);
                    }
                }
                Some("lv2" | "ladspa" | "windows-vst" | "lxvst" | "mac-vst" | "audiounit" | "luaproc") => {
                    skipped_plugins.push(p.attribute("name").unwrap_or("?").to_string());
                }
                _ => {}
            }
        }
        if midi {
            track.instrument.get_or_insert_with(synth_slot);
            warnings.push(format!("MIDI track '{route_name}': its regions aren't imported (import its .mid files instead)"));
            project.tracks.push(track);
            continue;
        }

        // The track's playlist: by id (Ardour 6+), by name (older), or by owner.
        let id = route.attribute("id");
        let wanted = route.attribute("audio-playlist").or(route.attribute("playlist")).or_else(|| child(route, "Diskstream").and_then(|d| d.attribute("playlist")));
        let playlist = playlists
            .iter()
            .find(|p| wanted.is_some() && (p.attribute("id") == wanted || p.attribute("name") == wanted))
            .or_else(|| playlists.iter().find(|p| id.is_some() && p.attribute("orig-track-id") == id));
        let mut split_stereo = false;
        for region in playlist.into_iter().flat_map(|p| p.children()).filter(|r| r.has_tag_name("Region")) {
            let Some(source_id) = region.attribute("source-0") else { continue };
            split_stereo |= region.has_attribute("source-1") && region.attribute("source-1") != Some(source_id);
            let source = sources.iter().find(|s| s.attribute("id") == Some(source_id));
            let Some(file_name) = source.and_then(|s| s.attribute("name")) else {
                warnings.push(format!("track '{route_name}': region '{}' has no source", region.attribute("name").unwrap_or_default()));
                continue;
            };
            let file = media_path(file_name, &audio_dirs, session_dir);
            let position = region.attribute("position").map_or(0.0, |v| time.secs(v));
            let length = region.attribute("length").map_or(0.0, |v| time.duration(v, position));
            let start = region.attribute("start").map_or(0.0, |v| time.duration(v, 0.0));
            let mut clip = audio_clip(region.attribute("name").unwrap_or_default(), file, position, length, start);
            clip.mute = region.attribute("muted") == Some("1");
            clip.gain = attr(region, "scale-amplitude").unwrap_or(1.0) as f32;
            track.clips.push(clip);
        }
        if split_stereo {
            warnings.push(format!("track '{route_name}': stereo regions use only their left-channel file"));
        }
        track.clips.sort_by(|a, b| a.position_secs.total_cmp(&b.position_secs));
        project.tracks.push(track);
    }
    if !skipped_plugins.is_empty() {
        skipped_plugins.sort();
        skipped_plugins.dedup();
        warnings.push(format!("plug-ins that can't be loaded here: {}", skipped_plugins.join(", ")));
    }
    Ok(Imported { project, warnings })
}

/// Ardour's time values: samples (up to Ardour 6), or Ardour 7's "a<superclocks>" (audio
/// time) and "b<ticks>" (musical time).
struct Time {
    sample_rate: f64,
    scps: f64,
    map: TempoMap,
}

impl Time {
    fn secs(&self, v: &str) -> f64 {
        let parse = |s: &str| s.parse::<f64>().unwrap_or(0.0);
        match v.as_bytes().first() {
            Some(b'a') => parse(&v[1..]) / self.scps,
            Some(b'b') => self.map.secs_at(parse(&v[1..]) / TICKS_PER_BEAT),
            _ => parse(v) / self.sample_rate,
        }
    }

    /// A length starting at `from` seconds (musical lengths depend on where they start).
    fn duration(&self, v: &str, from: f64) -> f64 {
        match v.strip_prefix('b') {
            Some(ticks) => {
                let q = self.map.quarters_at(from) + ticks.parse::<f64>().unwrap_or(0.0) / TICKS_PER_BEAT;
                self.map.secs_at(q) - from
            }
            None => self.secs(v),
        }
    }

    /// A tempo-map point's position: "sclock" (Ardour 7+) or "frame" (samples).
    fn point(&self, n: Node) -> f64 {
        attr(n, "sclock").map(|s| s / self.scps).or_else(|| attr(n, "frame").map(|f| f / self.sample_rate)).unwrap_or(0.0)
    }
}

/// `interchange/<session>/audiofiles`, plus any other folders under `interchange` (a
/// renamed session keeps its old one).
fn audio_dirs(session_dir: &Path, name: &str) -> Vec<PathBuf> {
    let interchange = session_dir.join("interchange");
    let mut dirs = vec![interchange.join(name).join("audiofiles")];
    if let Ok(entries) = std::fs::read_dir(&interchange) {
        let first = dirs[0].clone();
        dirs.extend(entries.flatten().map(|e| e.path().join("audiofiles")).filter(|d| *d != first));
    }
    dirs
}

fn media_path(name: &str, audio_dirs: &[PathBuf], session_dir: &Path) -> PathBuf {
    let p = Path::new(name);
    if p.is_absolute() {
        return find_media(p.to_path_buf(), session_dir);
    }
    let found = audio_dirs.iter().map(|d| d.join(name)).find(|p| p.exists());
    found.unwrap_or_else(|| find_media(audio_dirs[0].join(name), session_dir))
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name(name))
}

fn attr(node: Node, name: &str) -> Option<f64> {
    node.attribute(name)?.trim().parse().ok().filter(|v: &f64| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARDOUR6: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Session version="6000" name="demo" sample-rate="48000">
  <Sources>
    <Source name="Gtr-1.wav" type="audio" id="101"/>
    <Source name="Gtr-1%R.wav" type="audio" id="102"/>
  </Sources>
  <Routes>
    <Route id="1" name="Master" default-type="audio">
      <PresentationInfo order="0" flags="MasterOut,OrderSet"/>
      <Processor type="amp" name="Amp"><Controllable name="gaincontrol" value="0.5"/></Processor>
    </Route>
    <Route id="2" name="Gtr" default-type="audio" audio-playlist="20">
      <PresentationInfo order="1" flags="AudioTrack,OrderSet"/>
      <Controllable name="solo" value="0" self-solo="1"/>
      <Controllable name="mute" value="1"/>
      <Processor type="amp" name="Amp"><Controllable name="gaincontrol" value="0.25"/></Processor>
      <Pannable><Controllable name="pan-azimuth" value="0.75"/></Pannable>
      <Processor type="lv2" name="ACE EQ" active="1"/>
      <Processor type="vst3" name="Pro-Q 3" active="0"><VST3Plugin unique-id="ABCD"/></Processor>
    </Route>
    <Route id="3" name="FX" default-type="audio">
      <PresentationInfo order="2" flags="AudioBus,OrderSet"/>
    </Route>
    <Route id="4" name="Synth" default-type="midi" midi-playlist="40">
      <PresentationInfo order="3" flags="MidiTrack,OrderSet"/>
    </Route>
  </Routes>
  <Playlists>
    <Playlist id="19" name="Gtr.1" type="audio" orig-track-id="2">
      <Region name="old take" position="0" length="48000" start="0" source-0="101"/>
    </Playlist>
    <Playlist id="20" name="Gtr" type="audio" orig-track-id="2">
      <Region name="Gtr-1" muted="0" position="96000" length="48000" start="24000" scale-amplitude="2" source-0="101" source-1="102"/>
    </Playlist>
  </Playlists>
  <TempoMap>
    <Tempo pulse="0" frame="0" beats-per-minute="140" note-type="4"/>
    <Tempo pulse="8" frame="480000" beats-per-minute="60" note-type="8"/>
    <Meter pulse="0" frame="0" note-type="4" divisions-per-bar="3"/>
  </TempoMap>
</Session>"#;

    #[test]
    fn imports_ardour6_session() {
        let Imported { project: p, warnings } = from_str(ARDOUR6, Path::new("/sessions/demo")).unwrap();
        assert_eq!((p.name.as_str(), p.sample_rate, p.master_volume), ("demo", 48_000, 0.5));
        assert_eq!((p.tempo_bpm, p.time_signature), (140.0, (3, 4)));
        assert_eq!(p.tempo_changes, vec![TempoChange { at_secs: 10.0, bpm: 30.0, time_signature: None }]);
        assert_eq!(p.tracks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["Gtr", "Synth"]);
        let gtr = &p.tracks[0];
        assert_eq!((gtr.volume, gtr.pan, gtr.mute, gtr.solo), (0.25, 0.5, true, true));
        assert!(gtr.plugins[0].bypass && gtr.plugins[0].uid == "ABCD");
        let c = &gtr.clips[0];
        assert_eq!(gtr.clips.len(), 1, "only the active playlist");
        assert_eq!(c.file, PathBuf::from("/sessions/demo/interchange/demo/audiofiles/Gtr-1.wav"));
        assert_eq!((c.position_secs, c.length_secs, c.source_offset_secs, c.gain), (2.0, 1.0, 0.5, 2.0));
        assert!(p.tracks[1].is_instrument());
        for w in ["ACE EQ", "bus 'FX'", "left-channel", "MIDI track 'Synth'"] {
            assert!(warnings.iter().any(|x| x.contains(w)), "{w}: {warnings:?}");
        }
    }

    #[test]
    fn reads_ardour7_superclock_and_beat_times() {
        let xml = r#"<Session version="7003" name="new" sample-rate="44100">
  <Sources><Source name="/abs/take.wav" id="7"/></Sources>
  <Routes><Route id="2" name="Vox" audio-playlist="9"><PresentationInfo flags="AudioTrack"/></Route></Routes>
  <Playlists><Playlist id="9" name="Vox"><Region name="r" position="a56448000" length="b3840" start="a0" source-0="7"/></Playlist></Playlists>
  <TempoMap superclocks-per-second="56448000">
    <Tempos><Tempo npm="120" note-type="4" sclock="0"/></Tempos>
    <Meters><Meter note-value="8" divisions-per-bar="6" sclock="0"/></Meters>
  </TempoMap>
</Session>"#;
        let p = from_str(xml, Path::new("/s")).unwrap().project;
        assert_eq!((p.tempo_bpm, p.time_signature), (120.0, (6, 8)));
        let c = &p.tracks[0].clips[0];
        // 1 s in; two beats at 120 bpm = 1 s long.
        assert_eq!((c.file.to_str(), c.position_secs, c.length_secs), (Some("/abs/take.wav"), 1.0, 1.0));
        assert!(from_str("<Ableton/>", Path::new(".")).is_err());
    }
}
