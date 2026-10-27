//! Looping regions (Logic's loop handle and Loop checkbox; see
//! `opentake_project::region_loop`): turning loops on and off, extending them, and
//! converting the repeats into real regions.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use opentake_project::{MAX_LOOP_PASSES, MIN_LOOP_LEN, Project, RegionLoop};
use serde::Deserialize;

use super::Engine;
use super::regions::{RegionData, RegionKind, RegionRef};

/// How far [`Engine::set_region_loop`] loops a region.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub enum LoopTo {
    /// Stop looping: the region becomes its first pass.
    Off,
    /// Until the next region on the track (Logic's Loop checkbox), or four passes if none follows.
    Fill,
    /// Until this timeline time (seconds; at least one pass).
    Until(f64),
    /// This many passes in all (the content plus repeats).
    Passes(usize),
}

/// Passes `LoopTo::Fill` gives a region with nothing after it.
const FILL_PASSES: usize = 4;

impl Engine {
    /// Loop regions, or stop looping them (one undo step). A region that doesn't loop yet loops
    /// all of itself: its length becomes the loop length (an audio region's rounded to whole
    /// samples, so every pass plays the same samples; a MIDI region's in quarter notes, so
    /// the repeats stay on the beat whatever the tempo does).
    pub fn set_region_loop(&mut self, regions: &[RegionRef], to: LoopTo) -> Result<()> {
        if let LoopTo::Until(t) = to
            && !t.is_finite()
        {
            bail!("loop end must be a time in seconds");
        }
        let sr = self.sample_rate() as f64;
        let map = self.project.tempo_map();
        let mut changed = Vec::new();
        for &r in regions {
            let data = self.region_data(r)?;
            // In the region's units: seconds, or quarter notes for MIDI (timeline times convert).
            let (pos, len, looped, units): (f64, f64, _, &dyn Fn(f64) -> f64) = match &data {
                RegionData::Audio(c) => (c.position_secs, c.length_secs, c.looped, &|t| t),
                RegionData::Midi(c) => (c.position_beats, c.length_beats, c.looped, &|t| map.quarters_at(t)),
            };
            let whole = match &data {
                RegionData::Audio(_) => (len * sr).round() / sr,
                RegionData::Midi(_) => len,
            };
            let l = looped.unwrap_or_else(|| RegionLoop::new(whole.max(MIN_LOOP_LEN)));
            let first = l.len - l.phase.rem_euclid(l.len);
            let end = match to {
                LoopTo::Off => None,
                LoopTo::Fill => Some(next_start(&self.project, r, data.position(&map)).map(units).unwrap_or(pos + FILL_PASSES as f64 * l.len - (l.len - first))),
                LoopTo::Until(t) => Some(units(t)),
                LoopTo::Passes(n) => Some(pos + n.max(1) as f64 * l.len - (l.len - first)),
            };
            // At least the first pass, at most as many passes as play.
            let length = end.map(|e| (e - pos).max(first.min(len)).min(MAX_LOOP_PASSES as f64 * l.len - (l.len - first)));
            changed.push((r, match data {
                RegionData::Audio(mut c) => {
                    match length {
                        None => c.unloop(),
                        Some(length) => {
                            (c.looped, c.length_secs) = (Some(l), length);
                            c.clamp_fades();
                        }
                    }
                    RegionData::Audio(c)
                }
                RegionData::Midi(mut c) => {
                    match length {
                        None => (c.looped, c.length_beats) = (None, len.min(l.len)),
                        Some(length) => (c.looped, c.length_beats) = (Some(l), length),
                    }
                    RegionData::Midi(c)
                }
            }));
        }
        let label = match (to, regions.len()) {
            (LoopTo::Off, 1) => "Unloop Region",
            (LoopTo::Off, _) => "Unloop Regions",
            (_, 1) => "Loop Region",
            _ => "Loop Regions",
        };
        self.record(label, None, |e| {
            for (r, data) in changed.iter().cloned() {
                let t = &mut e.project.tracks[r.track];
                match data {
                    RegionData::Audio(c) => t.clips[r.index] = c,
                    RegionData::Midi(c) => t.midi_clips[r.index] = c,
                }
            }
            e.resync_tracks(changed.iter().map(|(r, _)| r.track).collect())?;
            e.sync_song_end()
        })
    }

    /// Turn looped regions' passes into real regions (Logic's "Convert Loops to Regions"), as
    /// one step; regions that don't loop are left alone. Returns the new regions.
    pub fn convert_loops(&mut self, regions: &[RegionRef]) -> Result<Vec<RegionRef>> {
        let mut looped = Vec::new();
        for &r in regions {
            match self.region_data(r)? {
                RegionData::Audio(c) if c.looped.is_some() => looped.push((r, c.loop_regions().into_iter().map(RegionData::Audio).collect::<Vec<_>>())),
                RegionData::Midi(c) if c.looped.is_some() => looped.push((r, c.loop_regions().into_iter().map(RegionData::Midi).collect())),
                _ => {}
            }
        }
        if looped.is_empty() {
            bail!("none of the regions loops");
        }
        self.record("Convert Loops to Regions", None, |e| {
            let gone: Vec<RegionRef> = looped.iter().map(|(r, _)| *r).collect();
            let touched: BTreeSet<usize> = gone.iter().map(|r| r.track).collect();
            e.remove_regions_impl(&gone);
            let mut out = Vec::new();
            for (r, parts) in looped {
                for p in parts {
                    out.push(e.insert_region_impl(r.track, p)?);
                }
            }
            e.resync_tracks(touched)?;
            Ok(out)
        })
    }

    /// Give regions imported with Reaper's item loop (the whole source file loops) their loop
    /// now that the files can be read; a missing file, or a region that never reaches the
    /// source's end, plays plain.
    pub(super) fn resolve_source_loops(&mut self, project: &mut Project) {
        let sr = self.sample_rate();
        for c in project.tracks.iter_mut().flat_map(|t| &mut t.clips).filter(|c| c.looped.is_some_and(|l| l.len <= 0.0)) {
            match self.load_media(&c.file, sr) {
                Ok(data) => c.loop_whole_source(data.frames() as f64 / sr as f64),
                Err(_) => c.looped = None,
            }
        }
    }
}

/// Where (seconds) the next region on `r`'s track after `pos` seconds starts, if one does.
fn next_start(project: &Project, r: RegionRef, pos: f64) -> Option<f64> {
    let t = &project.tracks[r.track];
    let map = project.tempo_map();
    let audio = t.clips.iter().enumerate().filter(|(i, _)| !(r.kind == RegionKind::Audio && *i == r.index)).map(|(_, c)| c.position_secs);
    let midi = t.midi_clips.iter().enumerate().filter(|(i, _)| !(r.kind == RegionKind::Midi && *i == r.index)).map(|(_, c)| c.start_secs(&map));
    audio.chain(midi).filter(|&s| s > pos + 1e-9).min_by(f64::total_cmp)
}

#[cfg(test)]
mod tests {
    use super::super::analysis::render_clips;
    use super::super::tests::tone_file;
    use super::*;
    use opentake_project::{Controller, ControllerEvent, ControllerLane, Note};

    fn engine(name: &str) -> (Engine, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("opentake-loop-{name}-{}", std::process::id()));
        let wav = tone_file(&dir, 2.0);
        let mut e = Engine::new();
        e.add_track("A").unwrap();
        e.add_clip(0, &wav, 1.0).unwrap();
        // 0.5 s of the tone from 0.25 s in, at 1 s.
        e.trim_clip(0, 0, Some(1.25), Some(1.75)).unwrap();
        e.move_clip(0, 0, 1.0, None).unwrap();
        (e, dir)
    }

    #[test]
    fn audio_passes_play_the_content_and_render_as_copies() {
        let (mut e, dir) = engine("audio");
        let r = RegionRef::audio(0, 0);
        e.set_region_loop(&[r], LoopTo::Passes(3)).unwrap();
        let c = e.project().tracks[0].clips[0].clone();
        assert_eq!((c.length_secs, c.looped), (1.5, Some(RegionLoop::new(0.5))));
        assert_eq!(e.history_info().undo.as_deref(), Some("Loop Region"));
        // One runtime region per pass, butted, each reading the same audio.
        let rt = e.build_clips(0, 48_000);
        assert_eq!(rt.iter().map(|c| (c.start, c.len, c.offset)).collect::<Vec<_>>(), [(48_000, 24_000, 12_000), (72_000, 24_000, 12_000), (96_000, 24_000, 12_000)]);
        // Rendered offline: three sample-exact copies of the content.
        let media = |c: &opentake_project::Clip| e.media.get(&(c.file.clone(), 48_000)).cloned();
        let (mut l, mut rr) = (vec![0.0f32; 96_000], vec![0.0f32; 96_000]);
        render_clips(&e.project().tracks[0].clips, 48_000, 48_000.0, media, &mut l, &mut rr);
        assert!(l[..24_000].iter().any(|s| s.abs() > 0.1));
        assert_eq!(&l[..24_000], &l[24_000..48_000]);
        assert_eq!(&l[..24_000], &l[48_000..72_000]);
        assert!(l[72_000..].iter().all(|&s| s == 0.0), "and nothing after");

        // Trimming the end changes the repeats; the start shifts into the loop.
        e.trim_clip(0, 0, None, Some(3.25)).unwrap();
        assert_eq!(e.build_clips(0, 48_000).len(), 5);
        let t = e.trim_clip(0, 0, Some(1.1), None).unwrap();
        assert_eq!((t.position_secs, t.looped.map(|l| (l.len * 1e6).round()), t.position_secs + t.length_secs), (1.1, Some(400_000.0), 3.25));
        // Fill runs up to the next region; off goes back to the first pass. All undoable.
        e.undo().unwrap();
        let copy = e.copy_clip(0, 0, 4.0, None).unwrap();
        e.set_region_loop(&[RegionRef::audio(0, 0)], LoopTo::Fill).unwrap();
        assert_eq!(e.project().tracks[0].clips[0].length_secs, 3.0);
        e.set_region_loop(&[RegionRef::audio(0, 0)], LoopTo::Off).unwrap();
        let c = &e.project().tracks[0].clips[0];
        assert_eq!((c.length_secs, c.looped), (0.5, None));
        e.undo().unwrap();
        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].clips[0].length_secs, 2.25);
        assert_eq!(copy, (0, 1));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn converting_loops_makes_one_region_per_pass() {
        let (mut e, dir) = engine("convert");
        e.set_region_loop(&[RegionRef::audio(0, 0)], LoopTo::Until(2.75)).unwrap();
        let made = e.convert_loops(&[RegionRef::audio(0, 0)]).unwrap();
        assert_eq!(made.len(), 4);
        let clips = &e.project().tracks[0].clips;
        assert_eq!(clips.iter().map(|c| (c.position_secs, c.length_secs, c.looped.is_none())).collect::<Vec<_>>(), [(1.0, 0.5, true), (1.5, 0.5, true), (2.0, 0.5, true), (2.5, 0.25, true)]);
        assert_eq!(e.history_info().undo.as_deref(), Some("Convert Loops to Regions"));
        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].clips.len(), 1);
        assert!(e.convert_loops(&[]).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn reaper_item_loops_resolve_against_the_source() {
        let dir = std::env::temp_dir().join(format!("opentake-loop-rpp-{}", std::process::id()));
        tone_file(&dir, 2.0);
        let item = |len: f64| format!("<ITEM\nPOSITION 1\nLENGTH {len}\nLOOP 1\nSOFFS 0.5\n<SOURCE WAVE\nFILE \"tone.wav\"\n>\n>\n");
        let rpp = format!("<REAPER_PROJECT 0.1\n<TRACK\nNAME A\n{}{}>\n>\n", item(5.0), item(1.0));
        std::fs::write(dir.join("p.rpp"), rpp).unwrap();
        let mut e = Engine::new();
        e.load_project(&dir.join("p.rpp")).unwrap();
        let clips = &e.project().tracks[0].clips;
        assert_eq!((clips[0].source_offset_secs, clips[0].looped), (0.0, Some(RegionLoop { len: 2.0, phase: 0.5 })));
        assert_eq!((clips[1].source_offset_secs, clips[1].looped), (0.5, None), "never reaches the end: plain");
        assert_eq!(e.build_clips(0, 48_000).len(), 4);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn midi_passes_repeat_notes_and_controllers() {
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        // 1 s for 1 s at 120 bpm: beats 2..4.
        let c = e.add_midi_clip(t, 1.0, 1.0, None).unwrap();
        // The second note runs past the loop end: cut there in every pass.
        e.set_midi_notes(t, c, vec![Note { start_beats: 0.0, length_beats: 0.5, pitch: 60, velocity: 100, tab: None }, Note { start_beats: 1.5, length_beats: 2.0, pitch: 64, velocity: 90, tab: None }]).unwrap();
        e.set_midi_controllers(t, c, Controller::PitchBend, vec![ControllerEvent { time_beats: 1.0, value: 1.0 }], true).unwrap();
        e.set_region_loop(&[RegionRef::midi(t, c)], LoopTo::Until(3.5)).unwrap();
        let rt = e.build_midi(t, 1000);
        assert_eq!(rt.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>(), [(1000, 2000), (2000, 3000), (3000, 3500)]);
        let notes: Vec<_> = rt.iter().flat_map(|r| r.notes.iter().map(|n| (n.start, n.end, n.pitch))).collect();
        assert_eq!(notes, [(1000, 1250, 60), (1750, 2000, 64), (2000, 2250, 60), (2750, 3000, 64), (3000, 3250, 60)]);
        // Each repeat bends back to the centre first, then up halfway through.
        let ctrl: Vec<_> = rt.iter().flat_map(|r| r.ctrl.iter().map(|c| (c.at, c.value))).collect();
        assert_eq!(ctrl, [(1500, 16383), (2000, 8192), (2500, 16383), (3000, 8192)]);
        // A split keeps both parts looping as they sounded.
        e.split_regions(&[RegionRef::midi(t, c)], 2.5).unwrap();
        let rt = e.build_midi(t, 1000);
        let notes: Vec<_> = rt.iter().flat_map(|r| r.notes.iter().map(|n| (n.start, n.end, n.pitch))).collect();
        assert_eq!(notes, [(1000, 1250, 60), (1750, 2000, 64), (2000, 2250, 60), (2750, 3000, 64), (3000, 3250, 60)]);
        let right = &e.project().tracks[t].midi_clips[1];
        assert_eq!((right.position_beats, right.looped.map(|l| l.len)), (5.0, Some(2.0)));
        assert_eq!(right.lane(Controller::PitchBend).unwrap(), &ControllerLane::new(Controller::PitchBend, vec![ControllerEvent { time_beats: 0.0, value: 1.0 }, ControllerEvent { time_beats: 1.0, value: 0.0 }]));

        // The loop is in beats: at half the tempo every pass takes twice as long.
        e.undo().unwrap();
        e.set_tempo(60.0, None).unwrap();
        let rt = e.build_midi(t, 1000);
        assert_eq!(rt.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>(), [(2000, 4000), (4000, 6000), (6000, 7000)]);
        // Trimming and looping to a time convert it through the map.
        e.set_region_loop(&[RegionRef::midi(t, c)], LoopTo::Until(9.0)).unwrap();
        assert_eq!(e.project().tracks[t].midi_clips[c].length_beats, 7.0);
    }
}
