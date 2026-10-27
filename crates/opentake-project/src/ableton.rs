//! Ableton Live set (`.als`, gzipped XML) import, Live 9–12.
//!
//! Imported: tempo, time signature and tempo automation (as steps), audio and MIDI tracks
//! (name, volume, pan, track on/off, solo), arrangement audio clips (sample, position,
//! start/loop markers, gain, disabled) and MIDI clips with notes, plug-in references,
//! arrangement locators (as markers).
//! Warped clips play at their original speed. Not imported: session-view clips, group and
//! return tracks (tracks inside groups come in flat), Live's own devices and automation.

use std::io::Read;
use std::path::{Path, PathBuf};

use roxmltree::Node;

use crate::import::{ImportError, Imported, QuarterTempo, audio_clip, drums_slot, find_media, named_plugin, synth_slot};
use crate::marker::Marker;
use crate::model::{MidiClip, Note, PluginFormat, Project, Track};
use crate::tempo::TempoMap;

pub fn load(path: &Path) -> Result<Imported, ImportError> {
    let data = std::fs::read(path)?;
    let mut imported = from_bytes(&data, path.parent().unwrap_or(Path::new(".")))?;
    if let Some(stem) = path.file_stem() {
        imported.project.name = stem.to_string_lossy().into_owned();
    }
    Ok(imported)
}

/// Import from the file's bytes (gzipped or plain XML); samples resolve against `project_dir`.
pub fn from_bytes(data: &[u8], project_dir: &Path) -> Result<Imported, ImportError> {
    let xml = if data.starts_with(&[0x1f, 0x8b]) {
        let mut s = String::new();
        flate2::read::GzDecoder::new(data).read_to_string(&mut s)?;
        s
    } else {
        String::from_utf8_lossy(data).into_owned()
    };
    from_str(&xml, project_dir)
}

pub fn from_str(xml: &str, project_dir: &Path) -> Result<Imported, ImportError> {
    let doc = crate::import::parse_xml(xml)?;
    let root = doc.root_element();
    let set = (root.has_tag_name("Ableton")).then(|| child(root, "LiveSet")).flatten();
    let Some(set) = set else { return Err(ImportError::Invalid("not an Ableton Live set".into())) };
    let mut project = Project { tracks: Vec::new(), ..Project::default() };
    let mut warnings = Vec::new();

    // Live 12 renamed the master track to "main".
    let master = child(set, "MasterTrack").or_else(|| child(set, "MainTrack"));
    let mixer = master.and_then(|m| at(m, &["DeviceChain", "Mixer"]));
    let mut tempo = QuarterTempo::default();
    if let Some(t) = mixer.and_then(|m| child(m, "Tempo")) {
        if let Some(bpm) = num(t, &["Manual"]) {
            tempo.tempo(0.0, bpm);
        }
        // Tempo automation: the envelope pointing at the tempo's automation target.
        let target = at(t, &["AutomationTarget"]).and_then(|a| a.attribute("Id"));
        let envelopes = master.and_then(|m| at(m, &["AutomationEnvelopes", "Envelopes"]));
        let envelope = envelopes.into_iter().flat_map(|e| e.children()).find(|e| target.is_some() && value(*e, &["EnvelopeTarget", "PointeeId"]) == target);
        for ev in envelope.and_then(|e| at(e, &["Automation", "Events"])).into_iter().flat_map(|e| e.children()).filter(|e| e.has_tag_name("FloatEvent")) {
            if let (Some(time), Some(bpm)) = (attr(ev, "Time"), attr(ev, "Value")) {
                tempo.tempo(time.max(0.0), bpm);
            }
        }
    }
    if let Some(code) = mixer.and_then(|m| num(m, &["TimeSignature", "Manual"])) {
        // Encoded as (denominator index) * 99 + (numerator - 1), denominators 1, 2, 4, 8, 16.
        let code = code as u32;
        tempo.signature(0.0, (code % 99 + 1, 1 << (code / 99).min(4)));
    }
    tempo.apply(&mut project);
    if let Some(gain) = mixer.and_then(|m| num(m, &["Volume", "Manual"])) {
        project.master_volume = gain as f32;
    }

    let map = project.tempo_map();
    // Locators (Live's markers): <Locators><Locators><Locator><Time/><Name/>, times in beats.
    let locators = at(set, &["Locators", "Locators"]).into_iter().flat_map(|l| l.children()).filter(|n| n.has_tag_name("Locator"));
    project.markers = locators.filter_map(|l| Some(Marker::new(map.secs_at(num(l, &["Time"])?.max(0.0)), value(l, &["Name"]).unwrap_or_default()))).collect();
    project.tidy_markers();
    let mut skipped = Vec::new();
    for node in child(set, "Tracks").into_iter().flat_map(|t| t.children()).filter(Node::is_element) {
        let name = value(node, &["Name", "EffectiveName"]).filter(|n| !n.is_empty()).or_else(|| value(node, &["Name", "UserName"])).unwrap_or("Track").to_string();
        let midi = match node.tag_name().name() {
            "AudioTrack" => false,
            "MidiTrack" => true,
            "ReturnTrack" => {
                warnings.push(format!("return track '{name}' isn't imported (no sends)"));
                continue;
            }
            _ => continue, // group tracks: their tracks are listed separately
        };
        let mut track = Track::new(name);
        if let Some(m) = at(node, &["DeviceChain", "Mixer"]) {
            track.volume = num(m, &["Volume", "Manual"]).unwrap_or(1.0) as f32;
            track.pan = num(m, &["Pan", "Manual"]).unwrap_or(0.0).clamp(-1.0, 1.0) as f32;
            track.mute = value(m, &["Speaker", "Manual"]) == Some("false");
            track.solo = value(m, &["SoloSink"]).or(value(m, &["Solo"])) == Some("true");
        }
        devices(node, &mut track, midi, &mut skipped);
        if let Some(seq) = at(node, &["DeviceChain", "MainSequencer"]) {
            // Arrangement clips only (session clips live in the ClipSlotList).
            let arranged = seq.descendants().filter(|n| n.has_tag_name("ArrangerAutomation")).flat_map(|a| a.descendants());
            for clip in arranged.filter(|n| n.has_tag_name("AudioClip") || n.has_tag_name("MidiClip")) {
                if clip.has_tag_name("MidiClip") {
                    if let Some(c) = midi_clip(clip) {
                        track.midi_clips.push(c);
                    }
                } else if let Some(c) = audio_clips(clip, &map, project_dir, &mut warnings, &track.name) {
                    track.clips.extend(c);
                }
            }
            let mut session = seq.descendants().filter(|n| n.has_tag_name("ClipSlotList")).flat_map(|n| n.descendants());
            if session.any(|n| n.has_tag_name("AudioClip") || n.has_tag_name("MidiClip")) {
                warnings.push(format!("track '{}': session view clips aren't imported", track.name));
            }
        }
        track.clips.sort_by(|a, b| a.position_secs.total_cmp(&b.position_secs));
        track.midi_clips.sort_by(|a, b| a.position_beats.total_cmp(&b.position_beats));
        if midi && track.instrument.is_none() {
            track.instrument = Some(synth_slot());
        }
        project.tracks.push(track);
    }
    if !skipped.is_empty() {
        skipped.sort();
        skipped.dedup();
        warnings.push(format!("Live devices aren't available here: {}", skipped.join(", ")));
    }
    Ok(Imported { project, warnings })
}

/// Plug-ins on the track; Live's own instruments become the built-in synth (drum racks the
/// built-in drums). On a MIDI track the first device that isn't a MIDI effect is the instrument.
fn devices(track_node: Node, track: &mut Track, midi: bool, skipped: &mut Vec<String>) {
    let Some(devices) = at(track_node, &["DeviceChain", "DeviceChain", "Devices"]) else { return };
    let mut want_instrument = midi;
    for d in devices.children().filter(Node::is_element) {
        let tag = d.tag_name().name();
        if tag.starts_with("Midi") {
            continue; // MIDI effects (arpeggiator, chord, …)
        }
        let enabled = value(d, &["On", "Manual"]) != Some("false");
        let slot = if tag == "PluginDevice" {
            let desc = at(d, &["PluginDesc"]).and_then(|p| p.children().find(Node::is_element));
            desc.and_then(|info| {
                let name = |paths: &[&[&str]]| paths.iter().find_map(|p| value(info, p)).filter(|n| !n.is_empty());
                match info.tag_name().name() {
                    "Vst3PluginInfo" => {
                        let uid = at(info, &["Uid"]).map(vst3_uid).unwrap_or_default();
                        Some(named_plugin(PluginFormat::Vst3, name(&[&["Name"]])?, None, &uid))
                    }
                    "VstPluginInfo" => Some(named_plugin(PluginFormat::Vst2, name(&[&["PlugName"], &["Name"]])?, None, value(info, &["UniqueId"]).unwrap_or_default())),
                    "AuPluginInfo" => Some(named_plugin(PluginFormat::Other, name(&[&["Name"]])?, value(info, &["Manufacturer"]), "")),
                    _ => None,
                }
            })
        } else {
            None
        };
        match slot {
            Some(mut slot) => {
                slot.bypass = !enabled;
                if want_instrument {
                    track.instrument = Some(slot);
                } else {
                    track.plugins.push(slot);
                }
            }
            None if want_instrument => {
                track.instrument = Some(if tag == "DrumGroupDevice" { drums_slot() } else { synth_slot() });
                skipped.push(tag.to_string());
            }
            None => skipped.push(tag.to_string()),
        }
        want_instrument = false;
    }
}

/// Live stores a VST3 class id as four signed 32-bit fields.
fn vst3_uid(uid: Node) -> String {
    let fields: Vec<u32> = uid.children().filter(Node::is_element).filter_map(|f| f.attribute("Value")?.parse::<i64>().ok()).map(|v| v as u32).collect();
    if fields.len() == 4 { fields.iter().map(|f| format!("{f:08X}")).collect() } else { String::new() }
}

/// Where a clip sits on the arrangement and which part of its content plays, in beats.
struct Span {
    start: f64,
    end: f64,
    /// Content time at the clip start, and the loop (if on).
    content_start: f64,
    looped: Option<(f64, f64)>,
}

fn span(clip: Node) -> Option<Span> {
    let start = num(clip, &["CurrentStart"]).or_else(|| attr(clip, "Time"))?;
    let end = num(clip, &["CurrentEnd"])?;
    let lp = child(clip, "Loop");
    let ls = lp.and_then(|l| num(l, &["LoopStart"])).unwrap_or(0.0);
    let le = lp.and_then(|l| num(l, &["LoopEnd"])).unwrap_or(f64::INFINITY);
    let content_start = ls + lp.and_then(|l| num(l, &["StartRelative"])).unwrap_or(0.0);
    let looped = (lp.and_then(|l| value(l, &["LoopOn"])) == Some("true") && le > ls + 1e-9).then_some((ls, le));
    (end > start + 1e-9).then_some(Span { start, end, content_start, looped })
}

impl Span {
    /// Passes through the content: (arrangement start, content start, length), in clip units.
    fn passes(&self) -> Vec<(f64, f64, f64)> {
        let total = self.end - self.start;
        match self.looped {
            Some((ls, le)) if self.content_start < le => {
                let mut v = vec![(self.start, self.content_start, (le - self.content_start).min(total))];
                while let Some(&(at, _, len)) = v.last().filter(|p| p.0 + p.2 < self.end - 1e-9 && v.len() < 10_000) {
                    v.push((at + len, ls, (le - ls).min(self.end - at - len)));
                }
                v
            }
            _ => vec![(self.start, self.content_start, total)],
        }
    }
}

/// A MIDI clip: Live counts in beats (quarter notes), as MIDI regions do.
fn midi_clip(clip: Node) -> Option<MidiClip> {
    let span = span(clip)?;
    let mut notes = Vec::new();
    // Live 11+: KeyTracks hold the pitch; each event has Time/Duration/Velocity.
    let events = clip.descendants().filter(|n| n.has_tag_name("MidiNoteEvent"));
    for (pass_at, content, len) in span.passes() {
        for ev in events.clone() {
            let pitch = ev.ancestors().find(|a| a.has_tag_name("KeyTrack")).and_then(|k| num(k, &["MidiKey"]));
            let (Some(pitch), Some(t), Some(d)) = (pitch, attr(ev, "Time"), attr(ev, "Duration")) else { continue };
            if ev.attribute("IsEnabled") == Some("false") || t < content - 1e-9 || t >= content + len - 1e-9 || !(0.0..=127.0).contains(&pitch) {
                continue;
            }
            let s = pass_at + t - content;
            let e = pass_at + (t + d).min(content + len) - content;
            let velocity = attr(ev, "Velocity").unwrap_or(100.0).round().clamp(1.0, 127.0) as u8;
            notes.push(Note { start_beats: s - span.start, length_beats: (e - s).max(0.002), pitch: pitch as u8, velocity, tab: None });
        }
    }
    notes.sort_by(|a, b| a.start_beats.total_cmp(&b.start_beats));
    Some(MidiClip {
        name: value(clip, &["Name"]).unwrap_or_default().to_string(),
        position_beats: span.start,
        length_beats: span.end - span.start,
        mute: value(clip, &["Disabled"]) == Some("true"),
        notes,
        tab: None,
        tempos: Vec::new(),
        controllers: Vec::new(),
        looped: None,
    })
}

fn audio_clips(clip: Node, map: &TempoMap, project_dir: &Path, warnings: &mut Vec<String>, track: &str) -> Option<Vec<crate::Clip>> {
    let span = span(clip)?;
    let Some(file) = child(clip, "SampleRef").and_then(|s| child(s, "FileRef")).and_then(|f| sample_path(f, project_dir)) else {
        warnings.push(format!("track '{track}': audio clip without a sample file skipped"));
        return None;
    };
    let warped = value(clip, &["IsWarped"]) != Some("false");
    let markers: Vec<(f64, f64)> = child(clip, "WarpMarkers")
        .into_iter()
        .flat_map(|w| w.children().filter(|n| n.has_tag_name("WarpMarker")))
        .filter_map(|m| Some((attr(m, "BeatTime")?, attr(m, "SecTime")?)))
        .collect();
    // Content time -> seconds into the sample. Unwarped clips keep their markers in seconds.
    let sample_secs = |content: f64| match (warped, markers.len()) {
        (false, _) => content,
        (true, n) if n >= 2 => {
            let i = markers.partition_point(|m| m.0 <= content).clamp(1, n - 1);
            let ((b0, s0), (b1, s1)) = (markers[i - 1], markers[i]);
            if (b1 - b0).abs() < 1e-12 { s0 } else { s0 + (content - b0) * (s1 - s0) / (b1 - b0) }
        }
        (true, n) => {
            let (b0, s0) = if n == 1 { markers[0] } else { (0.0, 0.0) };
            s0 + (content - b0) * 60.0 / map.tempo_at(map.secs_at(span.start))
        }
    };
    let name = value(clip, &["Name"]).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    let gain = num(clip, &["SampleVolume"]).unwrap_or(1.0) as f32;
    let muted = value(clip, &["Disabled"]) == Some("true");
    let mut stretched = false;
    let mut clips = Vec::new();
    for (pass_at, content, len) in span.passes() {
        let (start, end) = (map.secs_at(pass_at), map.secs_at(pass_at + len));
        // Unwarped clips measure content in seconds, but position and length in beats.
        let (offset, offset_end) = if warped { (sample_secs(content), sample_secs(content + len)) } else { (content, content + end - start) };
        stretched |= ((offset_end - offset) / (end - start) - 1.0).abs() > 0.01;
        let mut c = audio_clip(name.clone(), file.clone(), start, end - start, offset);
        (c.gain, c.mute) = (gain, muted);
        clips.push(c);
    }
    if stretched {
        warnings.push(format!("track '{track}': warped clip '{name}' plays at its original speed"));
    }
    Some(clips)
}

/// The sample's location: relative to the set (Live 11+ `RelativePath`, or the older list
/// of `RelativePathElement`s), the absolute `Path`, or by name in the usual folders.
fn sample_path(file_ref: Node, project_dir: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    let rel = child(file_ref, "RelativePath");
    if let Some(r) = rel.and_then(|r| r.attribute("Value")).filter(|r| !r.is_empty()) {
        candidates.push(project_dir.join(r));
    }
    let name = value(file_ref, &["Name"]).filter(|n| !n.is_empty());
    if let Some(name) = name {
        let dirs: PathBuf = rel.into_iter().flat_map(|r| r.children()).filter_map(|e| e.attribute("Dir")).collect();
        candidates.push(project_dir.join(dirs).join(name));
    }
    if let Some(p) = value(file_ref, &["Path"]).filter(|p| !p.is_empty()) {
        candidates.push(PathBuf::from(p));
    }
    let found = candidates.iter().find(|p| p.exists()).cloned();
    found.or_else(|| candidates.into_iter().next().map(|p| find_media(p, project_dir)))
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|n| n.has_tag_name(name))
}

fn at<'a, 'i>(node: Node<'a, 'i>, path: &[&str]) -> Option<Node<'a, 'i>> {
    path.iter().try_fold(node, |n, p| child(n, p))
}

/// The `Value` attribute of the element at `path` (Live's way of storing every scalar).
fn value<'a>(node: Node<'a, '_>, path: &[&str]) -> Option<&'a str> {
    at(node, path)?.attribute("Value")
}

fn num(node: Node, path: &[&str]) -> Option<f64> {
    value(node, path)?.trim().parse().ok().filter(|v: &f64| v.is_finite())
}

fn attr(node: Node, name: &str) -> Option<f64> {
    node.attribute(name)?.trim().parse().ok().filter(|v: &f64| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const SET: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Ableton MajorVersion="5" MinorVersion="11.0_433" Creator="Ableton Live 11.3">
  <LiveSet>
    <Tracks>
      <AudioTrack Id="8">
        <Name><EffectiveName Value="Drums" /><UserName Value="" /></Name>
        <DeviceChain>
          <Mixer>
            <Pan><Manual Value="-0.5" /></Pan>
            <Speaker><Manual Value="false" /></Speaker>
            <Volume><Manual Value="0.5" /></Volume>
          </Mixer>
          <MainSequencer>
            <ClipSlotList><ClipSlot><ClipSlot><Value><AudioClip Time="0"/></Value></ClipSlot></ClipSlot></ClipSlotList>
            <Sample>
              <ArrangerAutomation>
                <Events>
                  <AudioClip Id="0" Time="4">
                    <CurrentStart Value="4" />
                    <CurrentEnd Value="12" />
                    <Loop>
                      <LoopStart Value="0" /><LoopEnd Value="4" /><StartRelative Value="1" /><LoopOn Value="true" />
                    </Loop>
                    <Name Value="Break" />
                    <Disabled Value="false" />
                    <SampleRef>
                      <FileRef>
                        <RelativePathType Value="3" />
                        <RelativePath Value="Samples/Imported/break.wav" />
                        <Path Value="/Users/someone/Music/break.wav" />
                      </FileRef>
                    </SampleRef>
                    <IsWarped Value="true" />
                    <WarpMarkers>
                      <WarpMarker Id="1" SecTime="0" BeatTime="0" />
                      <WarpMarker Id="2" SecTime="2" BeatTime="4" />
                    </WarpMarkers>
                    <SampleVolume Value="0.8" />
                  </AudioClip>
                  <AudioClip Id="1" Time="16">
                    <CurrentStart Value="16" /><CurrentEnd Value="20" />
                    <Loop><LoopStart Value="0.5" /><LoopEnd Value="10" /><StartRelative Value="0" /><LoopOn Value="false" /></Loop>
                    <Name Value="" />
                    <Disabled Value="true" />
                    <SampleRef><FileRef><RelativePath><RelativePathElement Dir="Samples" /><RelativePathElement Dir="Recorded" /></RelativePath><Name Value="vox.wav" /></FileRef></SampleRef>
                    <IsWarped Value="false" />
                  </AudioClip>
                </Events>
              </ArrangerAutomation>
            </Sample>
          </MainSequencer>
          <DeviceChain><Devices>
            <Eq8 Id="1"><On><Manual Value="true" /></On></Eq8>
            <PluginDevice Id="2"><On><Manual Value="false" /></On>
              <PluginDesc><Vst3PluginInfo Id="0"><Name Value="Pro-Q 3" /><Uid><Fields.0 Value="1" /><Fields.1 Value="-1" /><Fields.2 Value="16" /><Fields.3 Value="255" /></Uid></Vst3PluginInfo></PluginDesc>
            </PluginDevice>
          </Devices></DeviceChain>
        </DeviceChain>
      </AudioTrack>
      <GroupTrack Id="9"><Name><EffectiveName Value="Group" /></Name></GroupTrack>
      <MidiTrack Id="10">
        <Name><EffectiveName Value="Keys" /></Name>
        <DeviceChain>
          <Mixer><Volume><Manual Value="1" /></Volume><SoloSink Value="true" /></Mixer>
          <MainSequencer>
            <ClipTimeable>
              <ArrangerAutomation>
                <Events>
                  <MidiClip Id="0" Time="0">
                    <CurrentStart Value="0" /><CurrentEnd Value="4" />
                    <Loop><LoopStart Value="0" /><LoopEnd Value="4" /><StartRelative Value="0" /><LoopOn Value="false" /></Loop>
                    <Name Value="Chords" />
                    <Notes>
                      <KeyTracks>
                        <KeyTrack Id="0">
                          <Notes>
                            <MidiNoteEvent Time="0" Duration="1" Velocity="100" OffVelocity="64" IsEnabled="true" />
                            <MidiNoteEvent Time="3" Duration="2" Velocity="90" IsEnabled="true" />
                            <MidiNoteEvent Time="1" Duration="1" Velocity="90" IsEnabled="false" />
                          </Notes>
                          <MidiKey Value="60" />
                        </KeyTrack>
                      </KeyTracks>
                    </Notes>
                  </MidiClip>
                </Events>
              </ArrangerAutomation>
            </ClipTimeable>
          </MainSequencer>
          <DeviceChain><Devices>
            <MidiArpeggiator Id="1" />
            <PluginDevice Id="2"><PluginDesc><VstPluginInfo Id="0"><PlugName Value="Serum" /><UniqueId Value="1483109208" /></VstPluginInfo></PluginDesc></PluginDevice>
          </Devices></DeviceChain>
        </DeviceChain>
      </MidiTrack>
      <MidiTrack Id="11">
        <Name><EffectiveName Value="Beat" /></Name>
        <DeviceChain><DeviceChain><Devices><DrumGroupDevice Id="1" /></Devices></DeviceChain></DeviceChain>
      </MidiTrack>
      <ReturnTrack Id="12"><Name><EffectiveName Value="A-Reverb" /></Name></ReturnTrack>
    </Tracks>
    <MasterTrack>
      <AutomationEnvelopes>
        <Envelopes>
          <AutomationEnvelope Id="0">
            <EnvelopeTarget><PointeeId Value="8" /></EnvelopeTarget>
            <Automation><Events>
              <FloatEvent Id="1" Time="-63072000" Value="120" />
              <FloatEvent Id="2" Time="16" Value="60" />
            </Events></Automation>
          </AutomationEnvelope>
        </Envelopes>
      </AutomationEnvelopes>
      <DeviceChain>
        <Mixer>
          <Volume><Manual Value="0.9" /></Volume>
          <Tempo><LomId Value="0" /><Manual Value="120" /><AutomationTarget Id="8"><LockEnvelope Value="0" /></AutomationTarget></Tempo>
          <TimeSignature><Manual Value="200" /></TimeSignature>
        </Mixer>
      </DeviceChain>
    </MasterTrack>
    <Locators><Locators>
      <Locator Id="1"><LomId Value="0" /><Time Value="24" /><Name Value="Outro" /><Annotation Value="" /><IsSongStart Value="false" /></Locator>
      <Locator Id="0"><LomId Value="0" /><Time Value="8" /><Name Value="Verse" /><Annotation Value="" /><IsSongStart Value="false" /></Locator>
    </Locators></Locators>
  </LiveSet>
</Ableton>"#;

    fn gz(text: &str) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(text.as_bytes()).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn imports_gzipped_live_set() {
        let Imported { project: p, warnings } = from_bytes(&gz(SET), Path::new("/proj")).unwrap();
        assert_eq!((p.tempo_bpm, p.time_signature, p.master_volume), (120.0, (3, 4), 0.9));
        assert_eq!(p.tempo_changes.len(), 1);
        assert!((p.tempo_changes[0].at_secs - 8.0).abs() < 1e-9 && p.tempo_changes[0].bpm == 60.0);
        assert_eq!(p.tracks.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["Drums", "Keys", "Beat"]);

        let drums = &p.tracks[0];
        assert_eq!((drums.volume, drums.pan, drums.mute), (0.5, -0.5, true));
        assert_eq!(drums.plugins.len(), 1);
        assert!(drums.plugins[0].bypass && drums.plugins[0].uid == "00000001FFFFFFFF00000010000000FF");
        // Looped clip: beats 4..12 at 120 bpm = 2..6 s; first pass starts one beat in.
        let c: Vec<(f64, f64, f64)> = drums.clips.iter().map(|c| (c.position_secs, c.length_secs, c.source_offset_secs)).collect();
        assert_eq!(c[..3], [(2.0, 1.5, 0.5), (3.5, 2.0, 0.0), (5.5, 0.5, 0.0)]);
        assert_eq!((drums.clips[0].file.as_path(), drums.clips[0].gain), (Path::new("/proj/Samples/Imported/break.wav"), 0.8));
        // Unwarped clip at beat 16 = 8 s (tempo drops to 60 there): 4 beats = 4 s, offset in seconds.
        let vox = &drums.clips[3];
        assert_eq!((vox.position_secs, vox.length_secs, vox.source_offset_secs, vox.mute), (8.0, 4.0, 0.5, true));
        assert_eq!(vox.file, PathBuf::from("/proj/Samples/Recorded/vox.wav"));
        assert_eq!(vox.name, "vox");

        let keys = &p.tracks[1];
        assert!(keys.solo);
        assert_eq!(keys.instrument.as_ref().map(|i| (i.name.as_str(), i.format)), Some(("VST: Serum", PluginFormat::Vst2)));
        let m = &keys.midi_clips[0];
        // In beats, as Live stores them (at 120 bpm: 0..2 s, notes at 0 and 1.5 s).
        assert_eq!((m.name.as_str(), m.position_beats, m.length_beats), ("Chords", 0.0, 4.0));
        let notes: Vec<(f64, f64, u8, u8)> = m.notes.iter().map(|n| (n.start_beats, n.length_beats, n.pitch, n.velocity)).collect();
        assert_eq!(notes, vec![(0.0, 1.0, 60, 100), (3.0, 1.0, 60, 90)]);
        assert_eq!(p.tracks[2].instrument.as_ref().map(|i| i.path.as_str()), Some("drums"));

        // Locators: beat 8 = 4 s; beat 24 is 8 beats past the drop to 60 bpm at beat 16 = 16 s.
        assert_eq!(p.markers.iter().map(|m| (m.time_secs, m.name.as_str())).collect::<Vec<_>>(), vec![(4.0, "Verse"), (16.0, "Outro")]);
        assert!(warnings.iter().any(|w| w.contains("Eq8")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("A-Reverb")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("session view")), "{warnings:?}");
    }

    #[test]
    fn rejects_other_xml() {
        assert!(from_str("<Project/>", Path::new(".")).is_err());
        let p = from_str(r#"<Ableton><LiveSet/></Ableton>"#, Path::new(".")).unwrap().project;
        assert!(p.tracks.is_empty() && p.tempo_bpm == 120.0);
    }
}
