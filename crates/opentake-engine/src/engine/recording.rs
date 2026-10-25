//! Turning finished recordings into regions: latency compensation, and cycle recording,
//! where each pass through the cycle becomes a take in a take folder.

use std::sync::atomic::Ordering;

use anyhow::{Result, bail};
use opentake_project::Clip;

use super::Engine;
use crate::mixer::{LATENCY_UNKNOWN, NO_WRAP};
use crate::recorder::RecordedFile;

/// How a recording's samples map to the timeline (all in samples). Sample `k` of the file
/// was processed at playhead `origin + k` (until the cycle wrapped), but was played
/// `latency` samples earlier: what the performer heard then was the output for that
/// earlier position.
#[derive(Debug, Clone, Copy)]
pub(super) struct RecordPlacement {
    /// Where recording was started (regions start here, not in the pre-roll).
    pub start: i64,
    pub origin: i64,
    pub latency: i64,
    pub cycle: Option<CycleWrap>,
}

/// A recording that went round the cycle.
#[derive(Debug, Clone, Copy)]
pub(super) struct CycleWrap {
    pub start: i64,
    pub end: i64,
    /// The recorded frame where the playhead first jumped back to `start`.
    pub wrap: i64,
}

/// A part of a recording on the timeline (samples; `offset` is into the file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Placed {
    pub position: i64,
    pub length: i64,
    pub offset: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Layout {
    /// What was recorded outside the cycle (before it, or without one).
    pub plain: Option<Placed>,
    /// One per pass through the cycle (all spanning the cycle).
    pub takes: Vec<Placed>,
}

impl RecordPlacement {
    pub fn layout(&self, frames: u64) -> Layout {
        let frames = frames as i64;
        // Timeline position of the file's first sample.
        let first = self.origin - self.latency;
        // A region from timeline `from` until `to` (or the end of the file).
        let plain = |from: i64, to: Option<i64>| {
            let offset = (from - first).max(0);
            let position = first + offset;
            let length = to.unwrap_or(first + frames).min(first + frames) - position;
            (length > 0 && position >= 0).then_some(Placed { position, length, offset })
        };
        let Some(c) = self.cycle else {
            return Layout { plain: plain(self.start, None), takes: Vec::new() };
        };
        let cycle_len = c.end - c.start;
        // A pass that covers less than half the cycle (punching in late, or stopping soon
        // after a wrap) isn't worth a take.
        let worth = |covered: i64| covered * 2 >= cycle_len;
        let take = |offset: i64| Placed { position: c.start, length: cycle_len, offset };
        let mut takes = Vec::new();
        // The first pass: from the record point (or the cycle start) to the cycle end.
        if worth(c.end - self.start.max(c.start)) {
            takes.push(take(c.start - first));
        }
        // Later passes start at the wrap, a cycle apart; their audio arrives `latency` later.
        let mut offset = c.wrap + self.latency;
        while offset < frames {
            if worth((frames - offset).min(cycle_len)) {
                takes.push(take(offset));
            }
            offset += cycle_len;
        }
        let before = (self.start < c.start).then(|| plain(self.start, Some(c.start))).flatten();
        Layout { plain: before, takes }
    }
}

impl Engine {
    /// Where the last recording lands on the timeline.
    pub(super) fn record_placement(&self) -> RecordPlacement {
        let measured = self.shared.record_latency.load(Ordering::Relaxed);
        let measured = if measured == LATENCY_UNKNOWN { 0 } else { measured as i64 };
        let wrap = self.shared.record_wrap.load(Ordering::Relaxed);
        let cycle = (wrap != NO_WRAP).then(|| CycleWrap {
            start: self.shared.record_cycle[0].load(Ordering::Relaxed) as i64,
            end: self.shared.record_cycle[1].load(Ordering::Relaxed) as i64,
            wrap: wrap as i64,
        });
        RecordPlacement {
            start: self.shared.record_start.load(Ordering::Relaxed) as i64,
            origin: self.shared.record_origin.load(Ordering::Relaxed),
            latency: measured + (self.record_offset_ms * self.sample_rate() as f64 / 1000.0).round() as i64,
            cycle,
        }
    }

    /// Add finished recordings to their tracks. A cycle recording's takes go into a take
    /// folder, joining one already spanning the cycle (from an earlier cycle recording) if
    /// there is one; the last take plays. Returns the regions added or changed.
    pub(super) fn add_recordings(&mut self, files: Vec<RecordedFile>, placement: &RecordPlacement) -> Result<Vec<Clip>> {
        let sr = self.sample_rate() as f64;
        let secs = |samples: i64| samples as f64 / sr;
        let mut changed = Vec::new();
        for f in files {
            let Some(index) = self.project.tracks.iter().position(|t| t.id == f.track_id) else { continue };
            let layout = placement.layout(f.frames);
            let name = f.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let region = |p: &Placed| Clip {
                name: name.clone(),
                file: f.path.clone(),
                position_secs: secs(p.position),
                length_secs: secs(p.length),
                source_offset_secs: secs(p.offset),
                gain: 1.0,
                mute: false,
                takes: Vec::new(),
                active_take: 0,
                pitch_semitones: 0,
                pitch_cents: 0,
                stretch: 1.0,
            };
            let clips = &mut self.project.tracks[index].clips;
            if let Some(p) = &layout.plain {
                clips.push(region(p));
                changed.push(clips.last().cloned().expect("just pushed"));
            }
            if let Some(first) = layout.takes.first() {
                // A pitched/stretched region doesn't take new (unprocessed) takes.
                let same = |c: &Clip| !c.is_flexed() && (c.position_secs - secs(first.position)).abs() < 0.5 / sr && (c.length_secs - secs(first.length)).abs() < 0.5 / sr;
                let (folder, new) = match clips.iter().position(same) {
                    Some(i) => (i, &layout.takes[..]),
                    None => {
                        clips.push(region(first));
                        (clips.len() - 1, &layout.takes[1..])
                    }
                };
                let folder = &mut clips[folder];
                for p in new {
                    let number = folder.takes.len().max(1) + 1;
                    folder.push_take(format!("Take {number}"), f.path.clone(), secs(p.offset));
                }
                changed.push(folder.clone());
            }
            self.resync_clips(index)?;
        }
        Ok(changed)
    }
}

impl Engine {
    /// Play take `take` of a take folder.
    pub fn select_take(&mut self, track: usize, clip: usize, take: usize) -> Result<Clip> {
        let index = self.clip_index(track, clip)?;
        let c = &self.project.tracks[index].clips[clip];
        if take >= c.takes.len() {
            bail!("region '{}' has {} take(s)", c.name, c.takes.len());
        }
        self.record("Select Take", None, |e| {
            let c = &mut e.project.tracks[index].clips[clip];
            c.select_take(take);
            let c = c.clone();
            e.resync_clips(index)?;
            Ok(c)
        })
    }

    /// Delete a take from a take folder (its audio file stays on disk).
    pub fn remove_take(&mut self, track: usize, clip: usize, take: usize) -> Result<Clip> {
        let index = self.clip_index(track, clip)?;
        let c = &self.project.tracks[index].clips[clip];
        if take >= c.takes.len() {
            bail!("region '{}' has {} take(s)", c.name, c.takes.len());
        }
        self.record("Delete Take", None, |e| {
            let c = &mut e.project.tracks[index].clips[clip];
            c.remove_take(take);
            let c = c.clone();
            e.resync_clips(index)?;
            Ok(c)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undoing_a_recording_returns_the_playhead_to_its_start() {
        let mut e = Engine::new();
        e.add_track("Guitar").unwrap();
        let id = e.project().tracks[0].id;
        let sr = e.sample_rate() as i64;
        let p = RecordPlacement { start: 2 * sr, origin: 2 * sr, latency: 0, cycle: None };
        e.history.playhead = Some(2.0);
        e.record("Record", None, |e| e.add_recordings(vec![RecordedFile { track_id: id, path: "take.wav".into(), frames: sr as u64 }], &p)).unwrap();
        e.seek(7.5).unwrap(); // the playhead ran on past the recording
        assert_eq!(e.undo().unwrap().as_deref(), Some("Record"));
        assert!(e.project().tracks[0].clips.is_empty());
        assert_eq!(e.status().position_secs, 2.0);
        // Other steps leave the playhead alone.
        e.redo().unwrap();
        e.seek(5.0).unwrap();
        e.add_track("Bass").unwrap();
        e.undo().unwrap();
        assert_eq!(e.status().position_secs, 5.0);
    }

    #[test]
    fn cycle_recordings_fill_one_take_folder() {
        let mut e = Engine::new();
        e.add_track("Vocal").unwrap();
        let id = e.project().tracks[0].id;
        let sr = e.sample_rate() as i64;
        // Cycle 1 s..2 s, recorded from its start: three passes, then two more later.
        let cycle = Some(CycleWrap { start: sr, end: 2 * sr, wrap: sr });
        let p = RecordPlacement { start: sr, origin: sr, latency: 0, cycle };
        let file = |name: &str, passes: i64| RecordedFile { track_id: id, path: name.into(), frames: (passes * sr) as u64 };
        e.record("Record", None, |e| e.add_recordings(vec![file("a.wav", 3)], &p)).unwrap();
        let c = &e.project().tracks[0].clips[0];
        assert_eq!((c.position_secs, c.length_secs, c.takes.len(), c.active_take, c.source_offset_secs), (1.0, 1.0, 3, 2, 2.0));
        e.record("Record", None, |e| e.add_recordings(vec![file("b.wav", 2)], &p)).unwrap();
        let clips = &e.project().tracks[0].clips;
        assert_eq!(clips.len(), 1, "joined the folder");
        let names: Vec<_> = clips[0].takes.iter().map(|t| (t.name.as_str(), t.file.to_str().unwrap())).collect();
        assert_eq!(names, [("Take 1", "a.wav"), ("Take 2", "a.wav"), ("Take 3", "a.wav"), ("Take 4", "b.wav"), ("Take 5", "b.wav")]);
        assert_eq!((clips[0].active_take, clips[0].file.to_str(), clips[0].source_offset_secs), (4, Some("b.wav"), 1.0));

        let c = e.select_take(0, 0, 1).unwrap();
        assert_eq!((c.file.to_str(), c.source_offset_secs), (Some("a.wav"), 1.0));
        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].clips[0].active_take, 4);
        let c = e.remove_take(0, 0, 4).unwrap();
        assert_eq!((c.takes.len(), c.active_take, c.file.to_str(), c.source_offset_secs), (4, 3, Some("b.wav"), 0.0), "the previous take plays");
        assert!(e.select_take(0, 0, 9).is_err());
    }

    const fn placed(position: i64, length: i64, offset: i64) -> Placed {
        Placed { position, length, offset }
    }

    #[test]
    fn recordings_are_placed_earlier_by_the_round_trip() {
        // Recorded from 48000 with a 256-sample pre-roll and a 300-sample round trip: the
        // first 556 samples were played before the start.
        let p = RecordPlacement { start: 48_000, origin: 48_000 - 256, latency: 300, cycle: None };
        assert_eq!(p.layout(10_000), Layout { plain: Some(placed(48_000, 10_000 - 556, 556)), takes: vec![] });
        // A negative manual offset can make the file start after the record point.
        let p = RecordPlacement { start: 48_000, origin: 48_000, latency: -100, cycle: None };
        assert_eq!(p.layout(10_000).plain, Some(placed(48_100, 10_000, 0)));
        // Recording at zero never places anything before the song start.
        let p = RecordPlacement { start: 0, origin: -256, latency: 300, cycle: None };
        assert_eq!(p.layout(10_000).plain, Some(placed(0, 10_000 - 556, 556)));
        assert_eq!(p.layout(500).plain, None, "all of it was before the start");
    }

    #[test]
    fn each_pass_through_the_cycle_is_a_take() {
        // Cycle 1000..2000, recorded from its start with no pre-roll and a 100-sample round
        // trip; the playhead wrapped after 1000 recorded frames, and stopped 3.6 passes in.
        let cycle = Some(CycleWrap { start: 1000, end: 2000, wrap: 1000 });
        let p = RecordPlacement { start: 1000, origin: 1000, latency: 100, cycle };
        let l = p.layout(3_600);
        assert_eq!(l.plain, None);
        // Each take's audio is a cycle apart, a round trip after the playhead was there.
        // The fourth pass (500 of 1000 samples heard) is half the cycle: kept.
        assert_eq!(l.takes, vec![placed(1000, 1000, 100), placed(1000, 1000, 1100), placed(1000, 1000, 2100), placed(1000, 1000, 3100)]);
        assert_eq!(p.layout(3_550).takes.len(), 3, "a pass under half the cycle is dropped");
    }

    #[test]
    fn recording_from_before_the_cycle_keeps_the_lead_in_as_a_region() {
        // Recording from 600 into the cycle 1000..2000 (no latency): 600..1000 is a plain
        // region, then the first pass and one more.
        let cycle = Some(CycleWrap { start: 1000, end: 2000, wrap: 1400 });
        let p = RecordPlacement { start: 600, origin: 600, latency: 0, cycle };
        let l = p.layout(2_400);
        assert_eq!(l.plain, Some(placed(600, 400, 0)));
        assert_eq!(l.takes, vec![placed(1000, 1000, 400), placed(1000, 1000, 1400)]);
    }

    #[test]
    fn punching_in_late_in_the_cycle_skips_the_short_first_pass() {
        // Record from 1700 in the cycle 1000..2000: the first pass has only 300 samples.
        let cycle = Some(CycleWrap { start: 1000, end: 2000, wrap: 300 });
        let p = RecordPlacement { start: 1700, origin: 1700, latency: 0, cycle };
        assert_eq!(p.layout(1_300).takes, vec![placed(1000, 1000, 300)]);
        // From 1400 the first pass covers 600: a take that starts with 400 of silence.
        let cycle = Some(CycleWrap { start: 1000, end: 2000, wrap: 600 });
        let p = RecordPlacement { start: 1400, origin: 1400, latency: 0, cycle };
        assert_eq!(p.layout(1_600).takes, vec![placed(1000, 1000, -400), placed(1000, 1000, 600)]);
    }
}
