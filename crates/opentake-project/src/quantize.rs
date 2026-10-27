//! MIDI quantize (Logic's Q): move note starts — and optionally ends — towards a grid of
//! note values aligned to the bars, by a strength, with swing on straight grids.
//!
//! Works in quarter notes (MIDI regions' time base), so the grid follows the tempo map: a
//! sixteenth is a sixteenth at any tempo. Grid lines restart at every bar line (a 1/4 grid
//! in 7/8 starts each bar afresh), as the editors' snapping does.

use serde::{Deserialize, Serialize};

use crate::model::Note;
use crate::tempo::{TempoMap, bar_len_q};

/// A quantize grid: a note value, straight or triplet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuantizeGrid {
    #[serde(rename = "1/4")]
    Quarter,
    #[serde(rename = "1/8")]
    Eighth,
    #[serde(rename = "1/16")]
    Sixteenth,
    #[serde(rename = "1/32")]
    ThirtySecond,
    #[serde(rename = "1/4T")]
    QuarterTriplet,
    #[serde(rename = "1/8T")]
    EighthTriplet,
    #[serde(rename = "1/16T")]
    SixteenthTriplet,
}

impl QuantizeGrid {
    pub const ALL: [Self; 7] = [Self::Quarter, Self::Eighth, Self::Sixteenth, Self::ThirtySecond, Self::QuarterTriplet, Self::EighthTriplet, Self::SixteenthTriplet];

    /// Step in quarter notes.
    pub fn step(self) -> f64 {
        match self {
            Self::Quarter => 1.0,
            Self::Eighth => 0.5,
            Self::Sixteenth => 0.25,
            Self::ThirtySecond => 0.125,
            Self::QuarterTriplet => 2.0 / 3.0,
            Self::EighthTriplet => 1.0 / 3.0,
            Self::SixteenthTriplet => 1.0 / 6.0,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Quarter => "1/4",
            Self::Eighth => "1/8",
            Self::Sixteenth => "1/16",
            Self::ThirtySecond => "1/32",
            Self::QuarterTriplet => "1/4T",
            Self::EighthTriplet => "1/8T",
            Self::SixteenthTriplet => "1/16T",
        }
    }

    /// From a name like "1/16" or "1/8T" (case doesn't matter; "1/8 triplet" works too).
    pub fn parse(name: &str) -> Option<Self> {
        let n = name.trim().to_ascii_uppercase().replace(' ', "").replace("TRIPLET", "T");
        Self::ALL.into_iter().find(|g| g.name().to_ascii_uppercase() == n)
    }

    pub fn is_triplet(self) -> bool {
        matches!(self, Self::QuarterTriplet | Self::EighthTriplet | Self::SixteenthTriplet)
    }

    /// The grid with this step (quarter notes), e.g. the editor's snap value.
    pub fn from_step(step: f64) -> Option<Self> {
        Self::ALL.into_iter().find(|g| (g.step() - step).abs() < 1e-9)
    }
}

/// How to quantize.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quantize {
    pub grid: QuantizeGrid,
    /// How far notes move towards the grid, 0..1 (1 = onto it, 0.5 = halfway).
    #[serde(default = "one")]
    pub strength: f64,
    /// Swing 0..1 on straight grids: every second grid line moves later by up to a third of a
    /// step (1 = the off-beats of a triplet feel). Triplet grids ignore it.
    #[serde(default)]
    pub swing: f64,
    /// Quantize note ends too; otherwise notes keep their lengths.
    #[serde(default)]
    pub ends: bool,
    /// Only notes this close to their grid line move, as a fraction of a step (like Logic's
    /// Q-Range: fix the near misses, keep deliberate pushes); 1 = every note.
    #[serde(default = "one")]
    pub window: f64,
}

fn one() -> f64 {
    1.0
}

/// Shortest note quantizing ends leaves (quarter notes).
const MIN_LEN: f64 = 0.01;

impl Quantize {
    pub fn new(grid: QuantizeGrid) -> Self {
        Self { grid, strength: 1.0, swing: 0.0, ends: false, window: 1.0 }
    }

    /// The grid line nearest `q` (quarter notes from the song start).
    pub fn nearest(&self, map: &TempoMap, q: f64) -> f64 {
        let step = self.grid.step();
        let bar = map.bar_at(map.secs_at(q));
        // The next bar line (earlier than a full bar when a signature change cuts it short).
        let next = map.bar_at(map.secs_at(bar.q + bar_len_q(bar.sig)) + 1e-9).q;
        let shift = if self.grid.is_triplet() { 0.0 } else { self.swing.clamp(0.0, 1.0) * step / 3.0 };
        let k0 = ((q - bar.q) / step).floor() as i64;
        (k0 - 1..=k0 + 2)
            .filter(|&k| k >= 0)
            .map(|k| bar.q + k as f64 * step + if k % 2 == 1 { shift } else { 0.0 })
            .filter(|&t| t < next - 1e-9)
            .chain([next])
            .min_by(|a, b| (a - q).abs().total_cmp(&(b - q).abs()))
            .unwrap_or(q)
    }

    /// Quantize the notes `pick` chooses (by index) of a region starting at `region_q`
    /// quarter notes, in place. Returns how many changed.
    pub fn apply(&self, notes: &mut [Note], region_q: f64, map: &TempoMap, pick: impl Fn(usize) -> bool) -> usize {
        let step = self.grid.step();
        let strength = self.strength.clamp(0.0, 1.0);
        let towards = |from: f64, to: f64| from + (to - from) * strength;
        let mut changed = 0;
        for n in notes.iter_mut().enumerate().filter(|(i, _)| pick(*i)).map(|(_, n)| n) {
            let (start, end) = (region_q + n.start_beats, region_q + n.start_beats + n.length_beats);
            let target = self.nearest(map, start);
            if self.window < 1.0 && (target - start).abs() > self.window.max(0.0) * step / 2.0 + 1e-9 {
                continue;
            }
            let new_start = (towards(start, target) - region_q).max(0.0);
            let new_len = if self.ends {
                // The end goes to its grid line, never onto or before the (quantized) start.
                let mut to = self.nearest(map, end);
                if to <= target + 1e-9 {
                    to = target + step;
                }
                (towards(end, to) - region_q - new_start).max(MIN_LEN)
            } else {
                n.length_beats
            };
            if (new_start - n.start_beats).abs() > 1e-12 || (new_len - n.length_beats).abs() > 1e-12 {
                (n.start_beats, n.length_beats) = (new_start, new_len);
                changed += 1;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::TempoChange;

    fn note(start: f64, len: f64) -> Note {
        Note { start_beats: start, length_beats: len, pitch: 60, velocity: 100, tab: None }
    }

    fn starts(notes: &[Note]) -> Vec<f64> {
        notes.iter().map(|n| (n.start_beats * 1e9).round() / 1e9).collect()
    }

    #[test]
    fn grids_parse_and_have_steps() {
        assert_eq!(QuantizeGrid::parse("1/16"), Some(QuantizeGrid::Sixteenth));
        assert_eq!(QuantizeGrid::parse("1/8t"), Some(QuantizeGrid::EighthTriplet));
        assert_eq!(QuantizeGrid::parse("1/16 Triplet"), Some(QuantizeGrid::SixteenthTriplet));
        assert_eq!(QuantizeGrid::parse("1/5"), None);
        assert_eq!(QuantizeGrid::from_step(0.125), Some(QuantizeGrid::ThirtySecond));
        let q = Quantize { swing: 0.5, ..Quantize::new(QuantizeGrid::EighthTriplet) };
        let back: Quantize = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(back, q);
        assert!(serde_json::to_string(&q).unwrap().contains(r#""grid":"1/8T""#));
        let short: Quantize = serde_json::from_str(r#"{"grid":"1/4"}"#).unwrap();
        assert_eq!(short, Quantize::new(QuantizeGrid::Quarter));
    }

    #[test]
    fn full_strength_snaps_and_half_goes_halfway() {
        let map = TempoMap::new(120.0, (4, 4), &[]);
        let mut notes = vec![note(0.1, 0.5), note(0.9, 0.5), note(1.3, 0.5), note(3.8, 0.5)];
        let q = Quantize::new(QuantizeGrid::Quarter);
        assert_eq!(q.apply(&mut notes, 0.0, &map, |_| true), 4);
        assert_eq!(starts(&notes), [0.0, 1.0, 1.0, 4.0]);
        assert!(notes.iter().all(|n| n.length_beats == 0.5), "lengths kept");

        let mut notes = vec![note(0.1, 0.5), note(0.9, 0.5)];
        Quantize { strength: 0.5, ..q }.apply(&mut notes, 0.0, &map, |_| true);
        assert_eq!(starts(&notes), [0.05, 0.95]);
        // Only the picked notes; a region starting off the grid quantizes to the song's grid.
        let mut notes = vec![note(0.1, 0.5), note(0.6, 0.5)];
        assert_eq!(Quantize::new(QuantizeGrid::Eighth).apply(&mut notes, 0.25, &map, |i| i == 1), 1);
        assert_eq!(starts(&notes), [0.1, 0.75]);
        // A note just before a region that starts off the grid stays in the region.
        let mut notes = vec![note(0.05, 0.5)];
        q.apply(&mut notes, 0.4, &map, |_| true);
        assert_eq!(starts(&notes), [0.0]);
    }

    #[test]
    fn triplets_swing_ends_and_window() {
        let map = TempoMap::new(120.0, (4, 4), &[]);
        let mut notes = vec![note(0.30, 0.1), note(0.70, 0.1), note(1.40, 0.1)];
        Quantize::new(QuantizeGrid::EighthTriplet).apply(&mut notes, 0.0, &map, |_| true);
        assert_eq!(starts(&notes), [(1.0f64 / 3.0 * 1e9).round() / 1e9, (2.0f64 / 3.0 * 1e9).round() / 1e9, (4.0f64 / 3.0 * 1e9).round() / 1e9]);

        // Full swing on 1/8: the off-beats land on the triplet's third eighth.
        let swung = Quantize { swing: 1.0, ..Quantize::new(QuantizeGrid::Eighth) };
        let mut notes = vec![note(0.55, 0.1), note(1.02, 0.1), note(1.6, 0.1)];
        swung.apply(&mut notes, 0.0, &map, |_| true);
        assert_eq!(starts(&notes), [(2.0f64 / 3.0 * 1e9).round() / 1e9, 1.0, (5.0f64 / 3.0 * 1e9).round() / 1e9]);
        let half = Quantize { swing: 0.5, ..swung };
        assert!((half.nearest(&map, 0.6) - (0.5 + 0.5 / 6.0)).abs() < 1e-9);

        // Ends: to their grid line, never at or before the start.
        let ends = Quantize { ends: true, ..Quantize::new(QuantizeGrid::Sixteenth) };
        let mut notes = vec![note(0.02, 0.45), note(1.0, 0.05)];
        ends.apply(&mut notes, 0.0, &map, |_| true);
        assert_eq!(notes.iter().map(|n| (n.start_beats, n.length_beats)).collect::<Vec<_>>(), [(0.0, 0.5), (1.0, 0.25)]);

        // A window of 0.5: only notes within a quarter step of their line move.
        let tight = Quantize { window: 0.5, ..Quantize::new(QuantizeGrid::Quarter) };
        let mut notes = vec![note(0.2, 0.1), note(1.45, 0.1)];
        assert_eq!(tight.apply(&mut notes, 0.0, &map, |_| true), 1);
        assert_eq!(starts(&notes), [0.0, 1.45]);
    }

    #[test]
    fn grid_lines_restart_at_each_bar() {
        // 7/8: a 1/4 grid gives 0, 1, 2, 3 then the next bar at 3.5.
        let map = TempoMap::new(120.0, (7, 8), &[]);
        let q = Quantize::new(QuantizeGrid::Quarter);
        assert_eq!(q.nearest(&map, 3.3), 3.5);
        assert_eq!(q.nearest(&map, 4.4), 4.5, "bar 2 starts at 3.5");
        // A tempo change doesn't move the grid in quarter notes.
        let slow = TempoMap::new(120.0, (4, 4), &[TempoChange { at_secs: 1.0, bpm: 60.0, time_signature: None }]);
        assert_eq!(q.nearest(&slow, 2.6), 3.0);
    }
}
