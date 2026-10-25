//! Final Cut Pro XML (`.fcpxml`, or an `.fcpxmld` bundle) import — the route in from Logic
//! Pro (File → Export → Project to Final Cut Pro XML), whose own `.logicx` is a closed format.
//!
//! Audio clips become regions; clips are grouped into tracks by audio role (falling back to
//! the lane they sit on). Imported: positions, source offsets, trims, clip volume, disabled
//! clips, connected clips, secondary storylines and compound clips. FCPXML has no tempo,
//! MIDI or mixer, so those stay at their defaults.

use std::path::Path;

use roxmltree::Node;

use crate::import::{ImportError, Imported, audio_clip, db_to_gain, find_media, resolve_path};
use crate::model::{Clip, Project, Track};

pub fn load(path: &Path) -> Result<Imported, ImportError> {
    // A .fcpxmld bundle is a folder with the document inside.
    let doc = if path.is_dir() { path.join("Info.fcpxml") } else { path.to_path_buf() };
    let xml = std::fs::read_to_string(&doc)?;
    let mut imported = from_str(&xml, path.parent().unwrap_or(Path::new(".")))?;
    if imported.project.name.is_empty() {
        imported.project.name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    }
    Ok(imported)
}

/// Parse FCPXML; relative media paths resolve against `base_dir`.
pub fn from_str(xml: &str, base_dir: &Path) -> Result<Imported, ImportError> {
    let doc = crate::import::parse_xml(xml)?;
    let root = doc.root_element();
    if !root.has_tag_name("fcpxml") {
        return Err(ImportError::Invalid("not a Final Cut Pro XML document".into()));
    }
    // The first project's sequence (a library can hold several; Logic writes one).
    let project_node = root.descendants().find(|n| n.has_tag_name("project"));
    let Some(sequence) = project_node.and_then(|p| child(p, "sequence")).or_else(|| root.descendants().find(|n| n.has_tag_name("sequence"))) else {
        return Err(ImportError::Invalid("no project sequence in the document".into()));
    };
    let mut ctx = Ctx { root, base_dir, tracks: Vec::new(), warnings: Vec::new(), depth: 0 };
    let origin = -time(sequence, "tcStart").unwrap_or(0.0);
    if let Some(spine) = child(sequence, "spine") {
        ctx.walk(spine, origin, (0.0, f64::INFINITY), None, 0);
    }

    let mut project = Project { name: project_node.and_then(|p| p.attribute("name")).unwrap_or_default().to_string(), tracks: Vec::new(), ..Project::default() };
    if let Some(rate) = sequence.attribute("audioRate").and_then(|r| r.trim_end_matches('k').parse::<f64>().ok()) {
        project.sample_rate = if rate < 1000.0 { (rate * 1000.0) as u32 } else { rate as u32 };
    }
    for (_, mut track) in ctx.tracks {
        track.clips.sort_by(|a, b| a.position_secs.total_cmp(&b.position_secs));
        project.tracks.push(track);
    }
    let mut warnings = ctx.warnings;
    if !project.tracks.is_empty() {
        warnings.push("Final Cut Pro XML has no tempo; set the project tempo to match the song".into());
    }
    Ok(Imported { project, warnings })
}

struct Ctx<'a, 'i> {
    root: Node<'a, 'i>,
    base_dir: &'a Path,
    /// Track key (role or lane) and the track.
    tracks: Vec<(String, Track)>,
    warnings: Vec<String>,
    depth: usize,
}

impl<'a, 'i> Ctx<'a, 'i> {
    /// Walk a container's children. `origin`: the absolute time of the container's local
    /// time 0. `window`: the absolute range the container shows.
    fn walk(&mut self, node: Node<'a, 'i>, origin: f64, window: (f64, f64), role: Option<&'a str>, lane: i32) {
        self.depth += 1;
        if self.depth > 32 {
            self.depth -= 1;
            return; // a compound clip that contains itself
        }
        for el in node.children().filter(Node::is_element) {
            let tag = el.tag_name().name();
            if !matches!(tag, "asset-clip" | "audio" | "clip" | "gap" | "video" | "sync-clip" | "ref-clip" | "spine" | "mc-clip") {
                continue;
            }
            let lane = el.attribute("lane").and_then(|l| l.parse().ok()).unwrap_or(lane);
            let role = el.attribute("audioRole").or(el.attribute("role").filter(|_| tag == "audio")).or(role);
            let offset = time(el, "offset").unwrap_or(0.0);
            if tag == "spine" {
                // A secondary storyline: its first item sits at the storyline's offset.
                let first = el.children().find(Node::is_element).and_then(|c| time(c, "offset")).unwrap_or(0.0);
                let shift = if el.attribute("offset").is_some() { offset - first } else { 0.0 };
                self.walk(el, origin + shift, window, role, lane);
                continue;
            }
            let start = time(el, "start").unwrap_or(0.0);
            let at = origin + offset;
            let shown = (at.max(window.0), time(el, "duration").map_or(window.1, |d| at + d).min(window.1));
            if shown.1 <= shown.0 + 1e-9 || el.attribute("enabled") == Some("0") && tag != "asset-clip" && tag != "audio" {
                continue;
            }
            let local_origin = at - start;
            match tag {
                "asset-clip" | "audio" => self.audio(el, local_origin, shown, role, lane),
                "ref-clip" => {
                    let media = el.attribute("ref").and_then(|r| self.resource(r)).and_then(|m| child(m, "sequence"));
                    if let Some(spine) = media.and_then(|s| child(s, "spine")) {
                        let tc = media.and_then(|s| time(s, "tcStart")).unwrap_or(0.0);
                        self.walk(spine, local_origin - tc, shown, role, lane);
                    }
                    continue;
                }
                _ => {}
            }
            // Connected clips and the audio inside clips.
            self.walk(el, local_origin, shown, role, lane);
        }
        self.depth -= 1;
    }

    fn audio(&mut self, el: Node<'a, 'i>, local_origin: f64, (from, to): (f64, f64), role: Option<&'a str>, lane: i32) {
        let Some(asset) = el.attribute("ref").and_then(|r| self.resource(r)) else { return };
        if !asset.has_tag_name("asset") || asset.attribute("hasAudio") == Some("0") {
            return;
        }
        let src = asset.attribute("src").or_else(|| asset.children().filter(|n| n.has_tag_name("media-rep")).find(|m| m.attribute("kind") != Some("proxy-media")).and_then(|m| m.attribute("src")));
        let Some(src) = src else {
            self.warnings.push(format!("asset '{}' has no media file", asset.attribute("name").unwrap_or_default()));
            return;
        };
        let file = find_media(resolve_path(src, self.base_dir), self.base_dir);
        let name = el.attribute("name").or(asset.attribute("name")).map(str::to_string).unwrap_or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        let offset = from - local_origin - time(asset, "start").unwrap_or(0.0);
        let mut clip: Clip = audio_clip(name, file, from, to - from, offset);
        clip.mute = el.attribute("enabled") == Some("0");
        if let Some(db) = child(el, "adjust-volume").and_then(|v| v.attribute("amount")).and_then(|a| a.trim_end_matches("dB").trim().parse::<f64>().ok()) {
            clip.gain = db_to_gain(db);
        }
        let key = role.map_or_else(|| format!("lane {lane}"), str::to_string);
        let index = match self.tracks.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                self.tracks.push((key.clone(), Track::new(track_name(role, lane))));
                self.tracks.len() - 1
            }
        };
        self.tracks[index].1.clips.push(clip);
    }

    fn resource(&self, id: &str) -> Option<Node<'a, 'i>> {
        child(self.root, "resources")?.children().find(|r| r.attribute("id") == Some(id))
    }
}

/// "music.Bass" -> "Bass", "dialogue" -> "Dialogue", no role -> "Lane 2".
fn track_name(role: Option<&str>, lane: i32) -> String {
    match role {
        Some(r) => {
            let name = r.rsplit_once('.').map_or(r, |(_, sub)| sub);
            let mut chars = name.chars();
            chars.next().map_or(String::new(), |c| c.to_uppercase().chain(chars).collect())
        }
        None if lane == 0 => "Main".into(),
        None => format!("Lane {lane}"),
    }
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name(name))
}

/// FCPXML times are rational seconds: "1001/30000s", "5s".
fn time(node: Node, name: &str) -> Option<f64> {
    let v = node.attribute(name)?.trim().trim_end_matches('s');
    let secs = match v.split_once('/') {
        Some((n, d)) => n.parse::<f64>().ok()? / d.parse::<f64>().ok().filter(|d| *d != 0.0)?,
        None => v.parse().ok()?,
    };
    secs.is_finite().then_some(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const LOGIC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE fcpxml>
<fcpxml version="1.10">
  <resources>
    <format id="r1" frameDuration="1/25s" name="FFVideoFormat1080p25"/>
    <asset id="r2" name="Bass" start="0s" duration="20s" hasAudio="1" audioSources="1" audioChannels="2" audioRate="48000">
      <media-rep kind="original-media" src="file:///Users/me/Song/Bass%20DI.wav"/>
    </asset>
    <asset id="r3" name="Vox" start="3600s" duration="30s" hasAudio="1" src="Audio/vox.wav"/>
    <asset id="r4" name="Picture" start="0s" duration="60s" hasVideo="1" hasAudio="0" src="file:///movie.mov"/>
    <media id="r5" name="Compound">
      <sequence format="r1" duration="10s" tcStart="0s">
        <spine>
          <asset-clip ref="r2" offset="0s" start="10s" duration="4s" name="Bass tail" audioRole="music.Bass"/>
        </spine>
      </sequence>
    </media>
  </resources>
  <library>
    <event name="Logic Export">
      <project name="My Song">
        <sequence format="r1" duration="60s" tcStart="0s" audioLayout="stereo" audioRate="44.1k">
          <spine>
            <gap name="Gap" offset="0s" start="0s" duration="60s">
              <asset-clip ref="r2" lane="-1" offset="2s" start="1s" duration="8s" name="Bass 1" audioRole="music.Bass">
                <adjust-volume amount="-6dB"/>
              </asset-clip>
              <clip lane="-2" offset="10s" start="3600s" duration="5s" name="Vox clip">
                <audio ref="r3" offset="3600s" start="3602s" duration="10s" role="dialogue.Vox"/>
              </clip>
              <asset-clip ref="r4" lane="1" offset="0s" duration="60s"/>
              <ref-clip ref="r5" lane="-1" offset="20s" duration="2s" audioRole="music.Bass"/>
              <asset-clip ref="r2" lane="-3" offset="30s" duration="1s" enabled="0"/>
            </gap>
          </spine>
        </sequence>
      </project>
    </event>
  </library>
</fcpxml>"#;

    #[test]
    fn imports_logic_export() {
        let Imported { project: p, warnings } = from_str(LOGIC, Path::new("/proj")).unwrap();
        assert_eq!((p.name.as_str(), p.sample_rate), ("My Song", 44_100));
        assert_eq!(p.tracks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["Bass", "Vox", "Lane -3"]);
        let bass = &p.tracks[0].clips;
        assert_eq!(bass[0].file, PathBuf::from("/Users/me/Song/Bass DI.wav"));
        assert_eq!((bass[0].position_secs, bass[0].length_secs, bass[0].source_offset_secs), (2.0, 8.0, 1.0));
        assert!((bass[0].gain - 0.501).abs() < 1e-3);
        // Compound clip: its inner clip is cut to the 2 s the ref-clip shows.
        assert_eq!((bass[1].name.as_str(), bass[1].position_secs, bass[1].length_secs, bass[1].source_offset_secs), ("Bass tail", 20.0, 2.0, 10.0));
        // Clip at 10 s whose audio starts 2 s into the asset (asset timecode starts at 3600 s).
        let vox = &p.tracks[1].clips[0];
        assert_eq!(vox.file, PathBuf::from("/proj/Audio/vox.wav"));
        assert_eq!((vox.position_secs, vox.length_secs, vox.source_offset_secs), (10.0, 5.0, 2.0));
        assert!(p.tracks[2].clips[0].mute);
        assert!(warnings.iter().any(|w| w.contains("tempo")));
    }

    #[test]
    fn parses_rational_times_and_rejects_other_documents() {
        let doc = roxmltree::Document::parse(r#"<a t="1001/30000s" u="5s" v="x"/>"#).unwrap();
        let n = doc.root_element();
        assert!((time(n, "t").unwrap() - 0.0333667).abs() < 1e-6);
        assert_eq!((time(n, "u"), time(n, "v")), (Some(5.0), None));
        assert!(from_str("<xmeml/>", Path::new(".")).is_err());
    }
}
