//! Comping a take folder: picking the best parts of several takes (Logic's quick swipe
//! comping).
//!
//! A folder's comp (`Clip::comp`) lists only where a take *other than the active one* plays:
//! sorted, non-overlapping segments in timeline seconds from the region start. Everywhere
//! else (the gaps) the active take plays, so an empty comp is a plain whole-take folder and a
//! trim that extends the region needs no bookkeeping. Being region-relative, segments move
//! with the region; trims and splits crop them ([`Clip::crop_comp`]), a stretch scales them.
//!
//! Playback expands a comped folder into one plain region per segment ([`played_clips`]),
//! overlapped by [`COMP_JOIN_SECS`] at each join with equal-power fades, so joins don't
//! click and the audio thread plays them like any other regions.

use serde::{Deserialize, Serialize};

use crate::fade::{Fade, FadeCurve, effective_fades};
use crate::model::Clip;

/// Crossfade at a join between comp segments (seconds), centred on the join.
pub const COMP_JOIN_SECS: f64 = 0.01;
/// Shorter segments are dropped.
pub const MIN_COMP_SECS: f64 = 0.005;

/// Where a take plays in a comped take folder.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct CompSegment {
    /// Timeline seconds from the region start.
    pub start_secs: f64,
    pub end_secs: f64,
    /// Index into the folder's takes.
    pub take: usize,
}

impl CompSegment {
    fn len(&self) -> f64 {
        self.end_secs - self.start_secs
    }
}

impl Clip {
    /// Whether a take folder plays more than one take.
    pub fn is_comped(&self) -> bool {
        !self.comp.is_empty()
    }

    /// What plays where, covering the whole region: the comp with the gaps filled by the
    /// active take (one segment for an uncomped region).
    pub fn comp_segments(&self) -> Vec<CompSegment> {
        let len = self.content_secs().min(self.length_secs).max(0.0);
        let mut out: Vec<CompSegment> = Vec::with_capacity(2 * self.comp.len() + 1);
        let mut push = |start_secs: f64, end_secs: f64, take: usize| match out.last_mut() {
            Some(last) if last.take == take => last.end_secs = end_secs,
            _ if end_secs > start_secs => out.push(CompSegment { start_secs, end_secs, take }),
            _ => {}
        };
        let mut at = 0.0;
        for s in &self.comp {
            push(at, s.start_secs, self.active_take);
            push(s.start_secs, s.end_secs, s.take);
            at = s.end_secs;
        }
        push(at, len, self.active_take);
        out
    }

    /// The take playing `t` seconds into the region.
    pub fn take_at(&self, t: f64) -> usize {
        self.comp.iter().find(|s| t >= s.start_secs && t < s.end_secs).map_or(self.active_take, |s| s.take)
    }

    /// Play take `take` from `from` to `to` (seconds from the region start; clamped to it),
    /// cutting into or replacing what played there. Returns false if there's no such take.
    pub fn set_comp_range(&mut self, from: f64, to: f64, take: usize) -> bool {
        if take >= self.takes.len() {
            return false;
        }
        let (from, to) = (from.min(to).max(0.0), from.max(to).min(self.content_secs()));
        if to <= from {
            return true;
        }
        let mut comp = Vec::with_capacity(self.comp.len() + 2);
        for s in &self.comp {
            if s.end_secs <= from || s.start_secs >= to {
                comp.push(*s);
                continue;
            }
            // Keep what sticks out on either side.
            if s.start_secs < from {
                comp.push(CompSegment { end_secs: from, ..*s });
            }
            if s.end_secs > to {
                comp.push(CompSegment { start_secs: to, ..*s });
            }
        }
        comp.push(CompSegment { start_secs: from, end_secs: to, take });
        self.comp = comp;
        self.normalize_comp();
        true
    }

    /// Keep the comp tidy: within the region, sorted, neighbours of one take merged, and
    /// nothing that plays the active take (or no take) or is too short to hear.
    pub fn normalize_comp(&mut self) {
        let (len, active, takes) = (self.content_secs(), self.active_take, self.takes.len());
        for s in &mut self.comp {
            (s.start_secs, s.end_secs) = (s.start_secs.max(0.0), s.end_secs.min(len));
        }
        self.comp.retain(|s| s.take != active && s.take < takes);
        self.comp.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));
        let mut merged: Vec<CompSegment> = Vec::with_capacity(self.comp.len());
        for s in self.comp.drain(..) {
            match merged.last_mut() {
                Some(last) if last.take == s.take && s.start_secs <= last.end_secs + 1e-9 => last.end_secs = last.end_secs.max(s.end_secs),
                _ => merged.push(s),
            }
        }
        merged.retain(|s| s.len() >= MIN_COMP_SECS);
        self.comp = merged;
    }

    /// The comp after the region's start moved `from` seconds later (negative: earlier) and
    /// its length changed: segments shift with the audio and are cut to the region.
    pub fn crop_comp(&mut self, from: f64) {
        for s in &mut self.comp {
            s.start_secs -= from;
            s.end_secs -= from;
        }
        self.normalize_comp();
    }

    /// Scale the comp with the region (a new stretch ratio plays the same audio `k` times as long).
    pub fn scale_comp(&mut self, k: f64) {
        for s in &mut self.comp {
            s.start_secs *= k;
            s.end_secs *= k;
        }
        self.normalize_comp();
    }

    /// The plain regions this one plays, each with the fades it plays with, given the
    /// region's (effective) `fades`. A comped folder plays one part per segment, each
    /// reading its take (beyond the segment's edges where the audio has it) and crossfading
    /// into the next over [`COMP_JOIN_SECS`]; the region's own fades go on the outer ends.
    pub fn played_parts(&self, fades: (Fade, Fade)) -> Vec<(Clip, (Fade, Fade))> {
        if self.comp.is_empty() {
            return vec![(self.clone(), fades)];
        }
        let segs = self.comp_segments();
        let n = segs.len();
        // Half of each join's crossfade; at most half of either neighbour, so parts only
        // ever overlap their neighbours.
        let half: Vec<f64> = (0..=n).map(|k| if k == 0 || k == n { 0.0 } else { (COMP_JOIN_SECS / 2.0).min(segs[k - 1].len() / 2.0).min(segs[k].len() / 2.0) }).collect();
        let join = |h: f64| Fade::new(2.0 * h, FadeCurve::EqualPower);
        segs.iter()
            .enumerate()
            .map(|(k, s)| {
                let (from, to) = (s.start_secs - half[k], s.end_secs + half[k + 1]);
                let take = &self.takes[s.take];
                let mut part = Clip {
                    file: take.file.clone(),
                    position_secs: self.position_secs + from,
                    length_secs: to - from,
                    source_offset_secs: self.source_offset_secs + take.offset_secs + from / self.stretch,
                    takes: Vec::new(),
                    active_take: 0,
                    comp: Vec::new(),
                    show_takes: false,
                    looped: None,
                    // The outer fades within what the joins leave.
                    fade_in: if k == 0 { Fade { secs: fades.0.secs.min(s.len() - half[1]), ..fades.0 } } else { join(half[k]) },
                    fade_out: if k + 1 == n { Fade { secs: fades.1.secs.min(s.len() - half[k]), ..fades.1 } } else { join(half[k + 1]) },
                    ..self.clone()
                };
                part.clamp_fades();
                let f = (part.fade_in, part.fade_out);
                (part, f)
            })
            .collect()
    }
}

/// What a track's audio regions play: every region with its effective fades (crossfades
/// over overlaps, see [`effective_fades`]), looped regions as their passes, comped take
/// folders as their parts.
pub fn played_clips(clips: &[Clip]) -> Vec<(Clip, (Fade, Fade))> {
    clips.iter().zip(effective_fades(clips)).flat_map(|(c, f)| c.loop_regions_with(f)).flat_map(|(c, f)| c.played_parts(f)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Take;

    /// A 4 s folder at 10 s with three takes, take 3 (index 2) active.
    fn folder() -> Clip {
        let mut c = crate::import::audio_clip("vox", "t1.wav".into(), 10.0, 4.0, 1.0);
        c.push_take("Take 2".into(), "t2.wav".into(), 5.0);
        c.push_take("Take 3".into(), "t3.wav".into(), 9.0);
        c
    }

    fn segs(c: &Clip) -> Vec<(f64, f64, usize)> {
        c.comp.iter().map(|s| (s.start_secs, s.end_secs, s.take)).collect()
    }

    #[test]
    fn assigning_ranges_splits_and_merges_segments() {
        let mut c = folder();
        assert!(c.set_comp_range(1.0, 2.0, 0));
        assert!(c.set_comp_range(3.0, 2.5, 1), "either direction");
        assert_eq!(segs(&c), [(1.0, 2.0, 0), (2.5, 3.0, 1)]);
        // Over the middle of a segment: it's cut in two around the new one.
        c.set_comp_range(1.25, 1.5, 1);
        assert_eq!(segs(&c), [(1.0, 1.25, 0), (1.25, 1.5, 1), (1.5, 2.0, 0), (2.5, 3.0, 1)]);
        // The same take again over the gap merges; the active take just removes.
        c.set_comp_range(1.5, 2.5, 1);
        assert_eq!(segs(&c), [(1.0, 1.25, 0), (1.25, 3.0, 1)]);
        c.set_comp_range(0.0, 1.1, 2);
        assert_eq!(segs(&c), [(1.1, 1.25, 0), (1.25, 3.0, 1)]);
        // Clamped to the region; a sliver too short to hear goes.
        c.set_comp_range(3.5, 99.0, 0);
        c.set_comp_range(1.1, 1.248, 2);
        assert_eq!(segs(&c), [(1.25, 3.0, 1), (3.5, 4.0, 0)]);
        assert!(!c.set_comp_range(0.0, 1.0, 7));
        // What plays where, gaps filled by the active take.
        let all: Vec<_> = c.comp_segments().iter().map(|s| (s.start_secs, s.end_secs, s.take)).collect();
        assert_eq!(all, [(0.0, 1.25, 2), (1.25, 3.0, 1), (3.0, 3.5, 2), (3.5, 4.0, 0)]);
        assert_eq!((c.take_at(2.0), c.take_at(3.2), c.take_at(3.9)), (1, 2, 0));
        // Picking a take plays it all through.
        c.select_take(0);
        assert!(!c.is_comped() && c.comp_segments().len() == 1);
    }

    #[test]
    fn trims_splits_and_stretch_keep_the_comp_with_the_audio() {
        let mut c = folder();
        c.set_comp_range(1.0, 2.0, 0);
        c.set_comp_range(3.0, 3.5, 1);
        // Trim the start 1.5 s later: segments shift and are cut at the new start.
        let mut t = c.clone();
        (t.position_secs, t.length_secs) = (11.5, 2.5);
        t.crop_comp(1.5);
        assert_eq!(segs(&t), [(0.0, 0.5, 0), (1.5, 2.0, 1)]);
        // Trim the end into the second segment; extending the start again leaves a gap.
        t.length_secs = 1.75;
        t.crop_comp(0.0);
        assert_eq!(segs(&t), [(0.0, 0.5, 0), (1.5, 1.75, 1)]);
        t.length_secs = 2.75;
        t.crop_comp(-1.0);
        assert_eq!(segs(&t), [(1.0, 1.5, 0), (2.5, 2.75, 1)]);

        // A split gives each half its part (at 1.5 s, inside the first segment).
        let (l, r) = c.split(1.5);
        assert_eq!((segs(&l), segs(&r)), (vec![(1.0, 1.5, 0)], vec![(0.0, 0.5, 0), (1.5, 2.0, 1)]));
        // Stretching to twice the length doubles them.
        let mut s = c.clone();
        s.scale_comp(2.0);
        assert_eq!(segs(&s), [(2.0, 4.0, 0)], "until the length catches up");
        s.length_secs = 8.0;
        s.comp = c.comp.clone();
        s.scale_comp(2.0);
        assert_eq!(segs(&s), [(2.0, 4.0, 0), (6.0, 7.0, 1)]);
    }

    #[test]
    fn removed_takes_fall_back_to_the_active_one() {
        let mut c = folder();
        c.set_comp_range(0.0, 1.0, 0);
        c.set_comp_range(2.0, 3.0, 1);
        // Remove take 1 (index 0): its segment goes, later indices shift down.
        assert!(c.remove_take(0));
        assert_eq!((segs(&c), c.active_take), (vec![(2.0, 3.0, 0)], 1));
        // Remove the active take: the previous one plays, its own segment now redundant.
        assert!(c.remove_take(1));
        assert!(c.takes.is_empty() && c.comp.is_empty(), "one take left: a plain region");
        // A new pass replaces the comp.
        let mut c = folder();
        c.set_comp_range(0.0, 1.0, 0);
        c.push_take("Take 4".into(), "t4.wav".into(), 0.0);
        assert!(!c.is_comped() && c.active_take == 3);
    }

    #[test]
    fn comp_plays_as_crossfaded_parts_of_each_take() {
        let mut c = folder();
        c.fade_in = Fade::new(0.1, FadeCurve::Linear);
        c.fade_out = Fade::new(0.2, FadeCurve::SCurve);
        assert_eq!(c.played_parts((c.fade_in, c.fade_out)).len(), 1, "uncomped: itself");
        c.set_comp_range(1.0, 2.0, 0);
        let parts = c.played_parts((c.fade_in, c.fade_out));
        let r = |x: f64| (x * 1e9).round() / 1e9;
        let shape: Vec<_> = parts.iter().map(|(p, f)| (p.file.to_str().unwrap(), r(p.position_secs), r(p.length_secs), r(f.0.secs), r(f.1.secs))).collect();
        let h = COMP_JOIN_SECS / 2.0;
        let want = [("t3.wav", 10.0, 1.0 + h, 0.1, 2.0 * h), ("t1.wav", 11.0 - h, 1.0 + 2.0 * h, 2.0 * h, 2.0 * h), ("t3.wav", 12.0 - h, 2.0 + h, 2.0 * h, 0.2)];
        assert_eq!(shape, want.map(|(f, a, b, c, d)| (f, r(a), r(b), r(c), r(d))));
        // Each part reads its take where the timeline is: take 1 is 8 s earlier in its source.
        assert!((parts[1].0.source_offset_secs - (1.0 + 1.0 - h)).abs() < 1e-9);
        assert!((parts[2].0.source_offset_secs - (9.0 + 2.0 - h)).abs() < 1e-9);
        assert!(parts[1].1.0.curve == FadeCurve::EqualPower && parts[2].1.1.curve == FadeCurve::SCurve);
        assert!(parts.iter().all(|(p, _)| p.takes.is_empty() && p.comp.is_empty()));
        // Old files (no comp) load; a comp round-trips.
        let back: Clip = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
        let mut plain = serde_json::to_value(folder()).unwrap();
        assert!(plain.get("comp").is_none() && plain.get("show_takes").is_none(), "{plain}");
        plain["takes"] = serde_json::to_value(vec![Take { name: "A".into(), file: "a.wav".into(), offset_secs: 0.0 }]).unwrap();
        let old: Clip = serde_json::from_value(plain).unwrap();
        assert!(old.comp.is_empty() && !old.show_takes);
    }
}
