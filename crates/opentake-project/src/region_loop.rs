//! Region looping (Logic's loop handle): a looped region's content — its first
//! [`RegionLoop::len_secs`] — repeats until the region's end.
//!
//! The region's length is the whole span, repeats included, so trimming or dragging the end
//! changes how many repeats play (the last one may be partial) and moving or copying needs
//! nothing special. Trimming the start shifts into the content: the content starts later and
//! is that much shorter, the end stays ([`Clip::move_start`]).
//!
//! A split keeps both parts looping as they sounded. The left part keeps the content. An
//! audio region's right part plays the same content from where the cut fell in it
//! ([`RegionLoop::phase_secs`]); a MIDI region's right part gets its content rotated to start
//! there instead, so the notes you edit still start at the region start.
//!
//! Playback, rendering and exports expand a looped region into one plain region per pass
//! ([`Clip::loop_regions`], [`MidiClip::loop_regions`], [`MidiClip::unrolled`]): the region's
//! fades go on the outer ends only, notes are cut at the end of their pass, and each repeat
//! starts with the controller values its content starts with. "Convert Loops to Regions"
//! keeps those passes as real regions.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::controller::{ControllerEvent, ControllerLane};
use crate::fade::Fade;
use crate::model::{Clip, MidiClip, Note, TempoMark};

/// Passes beyond this many are left silent (a region looping a tiny content for minutes).
pub const MAX_LOOP_PASSES: usize = 1024;
/// Shortest loop content (seconds).
pub const MIN_LOOP_SECS: f64 = 0.01;

/// How a region loops.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct RegionLoop {
    /// The content's length (timeline seconds from the region start): the loop length.
    pub len_secs: f64,
    /// Where in the content the region starts (seconds): an audio region split off a
    /// looped one starts playing its loop part-way through.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub phase_secs: f64,
}

fn is_zero(v: &f64) -> bool {
    *v == 0.0
}

/// One pass of the content within a region.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoopPass {
    /// 0 for the first pass (the content as written), then the repeats.
    pub index: usize,
    /// Where it starts (seconds from the region start).
    pub at: f64,
    /// Where in the content it starts (only a first pass starts later than 0).
    pub from: f64,
    pub len: f64,
}

impl RegionLoop {
    pub fn new(len_secs: f64) -> Self {
        Self { len_secs, phase_secs: 0.0 }
    }

    /// The passes that cover `length` seconds of a region (at most [`MAX_LOOP_PASSES`]).
    pub fn passes(&self, length: f64) -> Vec<LoopPass> {
        let l = self.len_secs.max(MIN_LOOP_SECS);
        let phase = self.phase_secs.rem_euclid(l);
        // Pass k's content starts at k·l - phase; each end is the next start, computed the
        // same way, so passes butt exactly.
        let start = |k: usize| k as f64 * l - phase;
        let mut out = Vec::new();
        for k in 0..MAX_LOOP_PASSES {
            let at = start(k).max(0.0);
            if at >= length - 1e-9 {
                break;
            }
            out.push(LoopPass { index: k, at, from: at - start(k), len: start(k + 1).min(length) - at });
        }
        out
    }
}

/// The region's one pass when it doesn't loop.
fn whole(length: f64) -> Vec<LoopPass> {
    vec![LoopPass { index: 0, at: 0.0, from: 0.0, len: length }]
}

impl Clip {
    /// The content's length: the loop length, or the whole region.
    pub fn content_secs(&self) -> f64 {
        self.looped.map_or(self.length_secs, |l| l.len_secs)
    }

    pub fn loop_passes(&self) -> Vec<LoopPass> {
        self.looped.map_or_else(|| whole(self.length_secs), |l| l.passes(self.length_secs))
    }

    /// Pass `p` as a plain region (no fades): the content from `p.from`, with its comp.
    pub fn pass_region(&self, p: &LoopPass) -> Clip {
        let mut c = Clip {
            position_secs: self.position_secs + p.at,
            length_secs: p.len,
            source_offset_secs: self.source_offset_secs + p.from / self.stretch,
            fade_in: Fade::default(),
            fade_out: Fade::default(),
            looped: None,
            ..self.clone()
        };
        c.crop_comp(p.from);
        c
    }

    /// What the region plays as plain regions: one per pass, the fades (`fades`, the effective
    /// ones) on the outer ends. A region that doesn't loop is itself.
    pub fn loop_regions_with(&self, fades: (Fade, Fade)) -> Vec<(Clip, (Fade, Fade))> {
        if self.looped.is_none() {
            return vec![(self.clone(), fades)];
        }
        let passes = self.loop_passes();
        let n = passes.len();
        passes
            .iter()
            .enumerate()
            .map(|(k, p)| {
                let mut c = self.pass_region(p);
                c.fade_in = if k == 0 { fades.0 } else { Fade::default() };
                c.fade_out = if k + 1 == n { fades.1 } else { Fade::default() };
                c.clamp_fades();
                let f = (c.fade_in, c.fade_out);
                (c, f)
            })
            .collect()
    }

    /// The passes as plain regions with the region's own fades ("Convert Loops to Regions").
    pub fn loop_regions(&self) -> Vec<Clip> {
        self.loop_regions_with((self.fade_in, self.fade_out)).into_iter().map(|(c, _)| c).collect()
    }

    /// Move the start `delta` seconds later (negative: earlier), keeping the audio in place.
    /// A looped region's content starts there too and the end stays, so the loop gets shorter
    /// (longer); its first pass sounds as before.
    pub fn move_start(&mut self, delta: f64) {
        self.position_secs += delta;
        self.length_secs -= delta;
        self.source_offset_secs += delta / self.stretch;
        if let Some(l) = &mut self.looped {
            l.len_secs = (l.len_secs - delta).max(MIN_LOOP_SECS);
            l.phase_secs = l.phase_secs.min(l.len_secs - MIN_LOOP_SECS).max(0.0);
        }
        self.crop_comp(delta);
    }

    /// Loop the whole source file (Reaper's item loop: `source_secs` long, the region starting
    /// at its source offset in it). A region that never reaches the source's end plays plain.
    pub fn loop_whole_source(&mut self, source_secs: f64) {
        let len = source_secs * self.stretch;
        let phase = self.source_offset_secs * self.stretch;
        if len < MIN_LOOP_SECS || (phase >= 0.0 && self.length_secs <= len - phase + 1e-9) {
            self.looped = None;
            return;
        }
        self.looped = Some(RegionLoop { len_secs: len, phase_secs: phase.rem_euclid(len) });
        self.source_offset_secs = 0.0;
        self.takes.clear();
        self.comp.clear();
    }

    /// Stop looping: the region becomes its first pass (as Logic's Loop checkbox does).
    pub fn unloop(&mut self) {
        let Some(l) = self.looped.take() else { return };
        let phase = l.phase_secs.rem_euclid(l.len_secs.max(MIN_LOOP_SECS));
        self.source_offset_secs += phase / self.stretch;
        self.length_secs = self.length_secs.min(l.len_secs - phase);
        self.crop_comp(phase);
        self.clamp_fades();
    }
}

impl MidiClip {
    pub fn content_secs(&self) -> f64 {
        self.looped.map_or(self.length_secs, |l| l.len_secs)
    }

    pub fn loop_passes(&self) -> Vec<LoopPass> {
        self.looped.map_or_else(|| whole(self.length_secs), |l| l.passes(self.length_secs))
    }

    /// `len` seconds of the region's content from `from`, as a plain region starting at 0
    /// (its position is left as it is): notes starting there, cut at its end; controllers led
    /// by the values they hold at `from` (or their neutral values) when `chase` is set.
    pub fn window(&self, from: f64, len: f64, chase: bool) -> MidiClip {
        let to = from + len;
        let notes = self
            .notes
            .iter()
            .filter(|n| n.start_secs >= from - 1e-9 && n.start_secs < to - 1e-9)
            .map(|n| Note { start_secs: (n.start_secs - from).max(0.0), length_secs: n.length_secs.min(to - n.start_secs), ..*n })
            .collect();
        let controllers = self
            .controllers
            .iter()
            .map(|lane| {
                let lead = chase.then(|| lane.value_at(from).or(lane.kind.neutral())).flatten().map(|value| ControllerEvent { time_secs: 0.0, value });
                let inside = lane.events.iter().filter(|e| e.time_secs >= from - 1e-9 && e.time_secs < to - 1e-9).map(|e| ControllerEvent { time_secs: (e.time_secs - from).max(0.0), ..*e });
                ControllerLane::new(lane.kind, lead.into_iter().chain(inside).collect())
            })
            .filter(|l| !l.events.is_empty())
            .collect();
        let mut rest = self.clone();
        let mut tempos = if from > 0.0 { rest.split_tempos(from) } else { rest.tempos.clone() };
        tempos.retain(|m| m.start_secs < len - 1e-9);
        let tab = self.tab.clone().map(|mut t| {
            let mut w = t.split_off(from);
            w.bars.retain(|b| b.start_secs < len - 1e-9);
            w
        });
        MidiClip { length_secs: len, notes, controllers, tempos, tab, looped: None, ..self.clone() }
    }

    /// Pass `p` as a plain region.
    pub fn pass_region(&self, p: &LoopPass) -> MidiClip {
        let w = self.window(p.from, p.len, p.index > 0 || p.from > 0.0);
        MidiClip { position_secs: self.position_secs + p.at, ..w }
    }

    /// The passes as plain regions ("Convert Loops to Regions"); a region that doesn't loop is
    /// itself.
    pub fn loop_regions(&self) -> Vec<MidiClip> {
        match self.looped {
            None => vec![self.clone()],
            Some(_) => self.loop_passes().iter().map(|p| self.pass_region(p)).collect(),
        }
    }

    /// The region with its repeats written out: one plain region over the same span (for
    /// renders and file exports).
    pub fn unrolled(&self) -> Cow<'_, MidiClip> {
        if self.looped.is_none() {
            return Cow::Borrowed(self);
        }
        let mut out = MidiClip { notes: Vec::new(), controllers: Vec::new(), tempos: Vec::new(), looped: None, ..self.clone() };
        let mut events: Vec<(crate::Controller, ControllerEvent)> = Vec::new();
        if let Some(t) = &mut out.tab {
            t.bars.clear();
        }
        for p in self.loop_passes() {
            let w = self.window(p.from, p.len, p.index > 0 || p.from > 0.0);
            out.notes.extend(w.notes.iter().map(|n| Note { start_secs: n.start_secs + p.at, ..*n }));
            for l in &w.controllers {
                events.extend(l.events.iter().map(|e| (l.kind, ControllerEvent { time_secs: e.time_secs + p.at, ..*e })));
            }
            out.tempos.extend(w.tempos.iter().map(|m| TempoMark { start_secs: m.start_secs + p.at, ..*m }));
            if let (Some(t), Some(wt)) = (&mut out.tab, &w.tab) {
                t.bars.extend(wt.bars.iter().map(|b| crate::TabBar { start_secs: b.start_secs + p.at, ..b.clone() }));
            }
        }
        let mut kinds: Vec<_> = events.iter().map(|(k, _)| *k).collect();
        kinds.sort();
        kinds.dedup();
        for kind in kinds {
            out.set_lane(ControllerLane::new(kind, events.iter().filter(|(k, _)| *k == kind).map(|(_, e)| *e).collect()));
        }
        Cow::Owned(out)
    }

    /// The content rotated to start `at` seconds into it, still looping (a split's right part).
    pub(crate) fn rotated(&self, at: f64) -> MidiClip {
        let len = self.content_secs();
        let turned = MidiClip { length_secs: len, looped: Some(RegionLoop { len_secs: len, phase_secs: at }), ..self.clone() };
        MidiClip { looped: Some(RegionLoop::new(len)), length_secs: self.length_secs, ..turned.unrolled().into_owned() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::Controller;
    use crate::import::audio_clip;

    fn spans(passes: &[LoopPass]) -> Vec<(usize, f64, f64, f64)> {
        let r = |x: f64| (x * 1e9).round() / 1e9;
        passes.iter().map(|p| (p.index, r(p.at), r(p.from), r(p.len))).collect()
    }

    #[test]
    fn passes_cover_the_region_and_the_last_one_may_be_partial() {
        let l = RegionLoop::new(2.0);
        assert_eq!(spans(&l.passes(5.0)), [(0, 0.0, 0.0, 2.0), (1, 2.0, 0.0, 2.0), (2, 4.0, 0.0, 1.0)]);
        assert_eq!(spans(&l.passes(6.0)), [(0, 0.0, 0.0, 2.0), (1, 2.0, 0.0, 2.0), (2, 4.0, 0.0, 2.0)], "no sliver at an exact end");
        assert_eq!(spans(&l.passes(1.5)), [(0, 0.0, 0.0, 1.5)], "shorter than the content");
        // A phase starts part-way into the content.
        let p = RegionLoop { len_secs: 2.0, phase_secs: 0.5 };
        assert_eq!(spans(&p.passes(4.0)), [(0, 0.0, 0.5, 1.5), (1, 1.5, 0.0, 2.0), (2, 3.5, 0.0, 0.5)]);
        assert_eq!(RegionLoop::new(0.01).passes(1e6).len(), MAX_LOOP_PASSES);
    }

    #[test]
    fn audio_passes_play_the_content_with_fades_on_the_outer_ends() {
        let mut c = audio_clip("loop", "a.wav".into(), 10.0, 5.0, 1.0);
        c.looped = Some(RegionLoop::new(2.0));
        c.stretch = 2.0;
        (c.fade_in, c.fade_out) = (Fade::new(0.1, Default::default()), Fade::new(0.2, Default::default()));
        let parts = c.loop_regions();
        let shape: Vec<_> = parts.iter().map(|p| (p.position_secs, p.length_secs, p.source_offset_secs, p.fade_in.secs, p.fade_out.secs, p.looped)).collect();
        assert_eq!(shape, [(10.0, 2.0, 1.0, 0.1, 0.0, None), (12.0, 2.0, 1.0, 0.0, 0.0, None), (14.0, 1.0, 1.0, 0.0, 0.2, None)]);
        // Not looped: itself.
        let plain = audio_clip("p", "a.wav".into(), 0.0, 1.0, 0.0);
        assert_eq!(plain.loop_regions(), vec![plain.clone()]);
    }

    #[test]
    fn splits_trims_and_unloop_keep_what_plays() {
        let mut c = audio_clip("loop", "a.wav".into(), 0.0, 7.0, 1.0);
        c.looped = Some(RegionLoop::new(2.0));
        // Split at 3 s, inside the second pass: the right part plays from 1 s into the loop.
        let (l, r) = c.split(3.0);
        assert_eq!((l.length_secs, l.looped), (3.0, c.looped));
        assert_eq!((r.position_secs, r.length_secs, r.source_offset_secs), (3.0, 4.0, 1.0));
        assert_eq!(r.looped, Some(RegionLoop { len_secs: 2.0, phase_secs: 1.0 }));
        let played = |c: &Clip| c.loop_regions().iter().map(|p| (p.position_secs, p.length_secs, p.source_offset_secs)).collect::<Vec<_>>();
        let mut both = played(&l);
        both.extend(played(&r));
        // The same passes as before the split, the one at the cut in two.
        assert_eq!(both, [(0.0, 2.0, 1.0), (2.0, 1.0, 1.0), (3.0, 1.0, 2.0), (4.0, 2.0, 1.0), (6.0, 1.0, 1.0)]);
        assert_eq!(played(&c).len(), 4);

        // Trimming the start shifts into the content: shorter loop, same end.
        let mut t = c.clone();
        t.move_start(0.5);
        assert_eq!((t.position_secs, t.length_secs, t.source_offset_secs, t.looped.unwrap().len_secs), (0.5, 6.5, 1.5, 1.5));
        // Unloop the right part: its first pass as a plain region.
        let mut u = r.clone();
        u.unloop();
        assert_eq!((u.position_secs, u.length_secs, u.source_offset_secs, u.looped), (3.0, 1.0, 2.0, None));
        // Serde: the loop round-trips, plain regions don't write it.
        let back: Clip = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(back, r);
        assert!(!serde_json::to_string(&u).unwrap().contains("looped"));
        assert!(serde_json::to_string(&c).unwrap().contains(r#""looped":{"len_secs":2.0}"#));
    }

    fn note(start: f64, len: f64, pitch: u8) -> Note {
        Note { start_secs: start, length_secs: len, pitch, velocity: 100, tab: None }
    }

    fn ev(t: f64, v: f32) -> ControllerEvent {
        ControllerEvent { time_secs: t, value: v }
    }

    #[test]
    fn midi_repeats_cut_notes_and_restart_controllers() {
        let mut c = MidiClip::new("riff", 4.0, 5.0);
        // A note past the loop end is cut there; one after the content never plays.
        c.notes = vec![note(0.0, 0.5, 60), note(1.5, 1.0, 64), note(2.5, 0.5, 67)];
        c.set_lane(ControllerLane::new(Controller::MOD_WHEEL, vec![ev(1.0, 1.0)]));
        c.looped = Some(RegionLoop::new(2.0));
        let u = c.unrolled();
        let notes: Vec<_> = u.notes.iter().map(|n| (n.start_secs, n.length_secs, n.pitch)).collect();
        assert_eq!(notes, [(0.0, 0.5, 60), (1.5, 0.5, 64), (2.0, 0.5, 60), (3.5, 0.5, 64), (4.0, 0.5, 60)]);
        // Each repeat starts back at the neutral value, then moves as the content does.
        assert_eq!(u.lane(Controller::MOD_WHEEL).unwrap().events, vec![ev(1.0, 1.0), ev(2.0, 0.0), ev(3.0, 1.0), ev(4.0, 0.0)]);
        assert!(u.looped.is_none() && u.length_secs == 5.0);
        let regions = c.loop_regions();
        assert_eq!(regions.iter().map(|r| (r.position_secs, r.length_secs, r.notes.len())).collect::<Vec<_>>(), [(4.0, 2.0, 2), (6.0, 2.0, 2), (8.0, 1.0, 1)]);

        // A split keeps the content on the left and rotates it on the right, so both together
        // play what the region did.
        let (l, r) = c.split(3.0);
        assert_eq!((l.length_secs, l.notes.len(), l.looped), (3.0, 3, c.looped));
        let rn: Vec<_> = r.notes.iter().map(|n| (n.start_secs, n.length_secs, n.pitch)).collect();
        assert_eq!(rn, [(0.5, 0.5, 64), (1.0, 0.5, 60)]);
        assert_eq!((r.position_secs, r.length_secs, r.looped), (7.0, 2.0, Some(RegionLoop::new(2.0))));
        assert_eq!(r.lane(Controller::MOD_WHEEL).unwrap().events, vec![ev(0.0, 1.0), ev(1.0, 0.0)]);
        let mut joined: Vec<_> = l.unrolled().notes.iter().map(|n| (n.start_secs + l.position_secs, n.pitch)).collect();
        joined.extend(r.unrolled().notes.iter().map(|n| (n.start_secs + r.position_secs, n.pitch)));
        let all: Vec<_> = u.notes.iter().map(|n| (n.start_secs + c.position_secs, n.pitch)).collect();
        assert_eq!(joined, all);
        let old: MidiClip = serde_json::from_str(r#"{"name":"m","position_secs":0,"length_secs":1,"notes":[]}"#).unwrap();
        assert!(old.looped.is_none());
    }
}
