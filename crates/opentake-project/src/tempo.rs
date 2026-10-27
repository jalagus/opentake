//! Tempo map: converts between seconds, quarter notes and bars/beats across tempo and
//! time-signature changes. Audio regions, automation and markers are in seconds; MIDI regions
//! are in quarter notes and so follow it. The map decides where bars and beats fall (ruler,
//! grid, snapping, metronome, plug-in transport) and when MIDI plays.

use serde::{Deserialize, Serialize};

/// A tempo (and optionally time-signature) change at a point on the timeline.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct TempoChange {
    pub at_secs: f64,
    /// Quarter notes per minute from here on.
    pub bpm: f64,
    /// New time signature from here on; it starts a new bar. `None` keeps the current one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_signature: Option<(u32, u32)>,
}

/// One stretch of constant tempo and time signature.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TempoSegment {
    pub start_secs: f64,
    /// Position in quarter notes from the song start.
    pub start_q: f64,
    pub bpm: f64,
    pub sig: (u32, u32),
    /// A bar line at or before the segment start (quarter notes) and its 0-based bar number.
    pub bar_q: f64,
    pub bar_index: i64,
}

impl TempoSegment {
    pub fn bar_len_q(&self) -> f64 {
        bar_len_q(self.sig)
    }

    pub fn beat_len_q(&self) -> f64 {
        beat_len_q(self.sig)
    }
}

/// Bar length in quarter notes.
pub fn bar_len_q((num, den): (u32, u32)) -> f64 {
    num.max(1) as f64 * 4.0 / den.max(1) as f64
}

/// Beat length (the denominator's note value) in quarter notes.
pub fn beat_len_q((_, den): (u32, u32)) -> f64 {
    4.0 / den.max(1) as f64
}

/// A bar line on the timeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarLine {
    /// 0-based bar number.
    pub index: i64,
    pub secs: f64,
    pub q: f64,
    pub sig: (u32, u32),
    /// The signature differs from the previous bar's.
    pub sig_change: bool,
}

/// Bar, beat and position within the beat (1-based bar and beat, fraction 0..1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MusicalPos {
    pub bar: i64,
    pub beat: u32,
    pub fraction: f64,
}

/// What to snap to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Grid {
    Bar,
    /// A step in quarter notes (1.0 = quarter, 0.5 = eighth), aligned to bar lines.
    Quarters(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TempoMap {
    /// Never empty; the first segment starts at 0.
    segments: Vec<TempoSegment>,
}

const EPS: f64 = 1e-9;

impl TempoMap {
    /// Build the map from the initial tempo/signature and the changes (any order).
    pub fn new(bpm: f64, sig: (u32, u32), changes: &[TempoChange]) -> Self {
        let sig = (sig.0.max(1), sig.1.max(1));
        let mut segments = vec![TempoSegment { start_secs: 0.0, start_q: 0.0, bpm: clamp_bpm(bpm), sig, bar_q: 0.0, bar_index: 0 }];
        let mut changes: Vec<TempoChange> = changes.iter().copied().filter(|c| c.at_secs.is_finite() && c.at_secs >= 0.0).collect();
        changes.sort_by(|a, b| a.at_secs.total_cmp(&b.at_secs));
        for c in changes {
            let prev = *segments.last().unwrap();
            let q = prev.start_q + (c.at_secs - prev.start_secs) * prev.bpm / 60.0;
            let mut seg = TempoSegment { start_secs: c.at_secs, start_q: q, bpm: clamp_bpm(c.bpm), ..prev };
            if let Some(new_sig) = c.time_signature.map(|(n, d)| (n.max(1), d.max(1))) {
                // A signature change starts a new bar; a partly played bar still counts.
                let bars = ((q - prev.bar_q) / prev.bar_len_q() - EPS).ceil().max(0.0) as i64;
                seg.sig = new_sig;
                seg.bar_q = q;
                seg.bar_index = prev.bar_index + bars;
            }
            if c.at_secs <= prev.start_secs + EPS {
                *segments.last_mut().unwrap() = TempoSegment { start_secs: prev.start_secs, start_q: prev.start_q, ..seg };
            } else {
                segments.push(seg);
            }
        }
        Self { segments }
    }

    pub fn segments(&self) -> &[TempoSegment] {
        &self.segments
    }

    fn index_at_secs(&self, secs: f64) -> usize {
        self.segments.partition_point(|s| s.start_secs <= secs + EPS).saturating_sub(1)
    }

    fn index_at_q(&self, q: f64) -> usize {
        self.segments.partition_point(|s| s.start_q <= q + EPS).saturating_sub(1)
    }

    pub fn segment_at(&self, secs: f64) -> &TempoSegment {
        &self.segments[self.index_at_secs(secs)]
    }

    /// Quarter notes from the song start at `secs` (negative before 0, at the first tempo).
    pub fn quarters_at(&self, secs: f64) -> f64 {
        let s = self.segment_at(secs);
        s.start_q + (secs - s.start_secs) * s.bpm / 60.0
    }

    pub fn secs_at(&self, q: f64) -> f64 {
        let s = &self.segments[self.index_at_q(q)];
        s.start_secs + (q - s.start_q) * 60.0 / s.bpm
    }

    pub fn tempo_at(&self, secs: f64) -> f64 {
        self.segment_at(secs).bpm
    }

    pub fn signature_at(&self, secs: f64) -> (u32, u32) {
        self.segment_at(secs).sig
    }

    /// Seconds per beat at `secs`.
    pub fn beat_secs_at(&self, secs: f64) -> f64 {
        let s = self.segment_at(secs);
        s.beat_len_q() * 60.0 / s.bpm
    }

    /// Seconds per bar at `secs` (at the tempo there).
    pub fn bar_secs_at(&self, secs: f64) -> f64 {
        let s = self.segment_at(secs);
        s.bar_len_q() * 60.0 / s.bpm
    }

    /// The bar containing `secs`.
    pub fn bar_at(&self, secs: f64) -> BarLine {
        let s = self.segment_at(secs);
        let q = self.quarters_at(secs);
        let k = ((q - s.bar_q) / s.bar_len_q() + EPS).floor();
        let bar_q = s.bar_q + k * s.bar_len_q();
        let prev_sig = self.segment_at(self.secs_at(bar_q) - EPS * 10.0).sig;
        BarLine { index: s.bar_index + k as i64, secs: self.secs_at(bar_q), q: bar_q, sig: s.sig, sig_change: bar_q > EPS && prev_sig != s.sig }
    }

    pub fn position(&self, secs: f64) -> MusicalPos {
        let bar = self.bar_at(secs);
        let q = self.quarters_at(secs);
        let beat = ((q - bar.q) / beat_len_q(bar.sig)).max(0.0);
        let whole = (beat + EPS).floor();
        MusicalPos { bar: bar.index + 1, beat: whole as u32 + 1, fraction: (beat - whole).max(0.0) }
    }

    /// Time of a 1-based bar and beat (fractional beats allowed).
    pub fn secs_of(&self, bar: i64, beat: f64) -> f64 {
        let index = bar - 1;
        let seg = self.segments.iter().rev().find(|s| s.bar_index <= index).unwrap_or(&self.segments[0]);
        let q = seg.bar_q + (index - seg.bar_index) as f64 * seg.bar_len_q() + (beat - 1.0).max(0.0) * seg.beat_len_q();
        self.secs_at(q)
    }

    /// Bar lines from `from` to `to` seconds (inclusive of a line exactly at `from`).
    pub fn bars(&self, from: f64, to: f64) -> Vec<BarLine> {
        let mut out = Vec::new();
        let first = self.bar_at(from.max(0.0));
        let mut secs = first.secs;
        let mut guard = 0;
        while secs <= to + EPS && guard < 100_000 {
            let bar = self.bar_at(secs + EPS * 10.0);
            if bar.secs >= from - EPS || out.is_empty() && bar.secs <= from {
                out.push(bar);
            }
            let next_q = bar.q + bar_len_q(bar.sig);
            // A signature change can cut a bar short.
            let cut = self.segments.iter().find(|s| s.start_q > bar.q + EPS && s.start_q < next_q - EPS && s.sig != bar.sig).map(|s| s.start_q);
            secs = self.secs_at(cut.unwrap_or(next_q));
            guard += 1;
        }
        out
    }

    /// Grid lines (seconds) of `step` quarter notes from `from` to `to`, aligned to bars.
    pub fn grid_lines(&self, step_q: f64, from: f64, to: f64) -> Vec<f64> {
        let mut out = Vec::new();
        if step_q <= 0.0 {
            return out;
        }
        let bars = self.bars(from, to);
        for (i, bar) in bars.iter().enumerate() {
            let end_q = bars.get(i + 1).map_or_else(|| bar.q + bar_len_q(bar.sig), |b| b.q);
            let mut k = 0;
            loop {
                let q = bar.q + k as f64 * step_q;
                if q >= end_q - EPS || k > 4096 {
                    break;
                }
                let t = self.secs_at(q);
                if t >= from - EPS && t <= to + EPS {
                    out.push(t);
                }
                k += 1;
            }
        }
        out
    }

    /// The nearest grid position to `secs`.
    pub fn snap(&self, secs: f64, grid: Grid) -> f64 {
        let bar = self.bar_at(secs.max(0.0));
        let q = self.quarters_at(secs.max(0.0));
        let step = match grid {
            Grid::Bar => bar_len_q(bar.sig),
            Grid::Quarters(s) => s.max(1e-6),
        };
        let candidate = bar.q + ((q - bar.q) / step).round() * step;
        // Never snap past the next bar line (a shorter bar after a signature change).
        let next = self.bar_at(self.secs_at(bar.q + bar_len_q(bar.sig)) + EPS * 10.0);
        let candidate = if candidate > next.q + EPS { next.q } else { candidate };
        self.secs_at(candidate).max(0.0)
    }

    /// True when the map has changes after the start.
    pub fn is_constant(&self) -> bool {
        self.segments.len() == 1
    }
}

fn clamp_bpm(bpm: f64) -> f64 {
    if bpm.is_finite() { bpm.clamp(5.0, 999.0) } else { 120.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn constant_tempo_maps_bars_and_beats() {
        let m = TempoMap::new(120.0, (4, 4), &[]);
        assert!(close(m.quarters_at(1.0), 2.0));
        assert_eq!(m.position(2.25), MusicalPos { bar: 2, beat: 1, fraction: 0.5 });
        assert!(close(m.secs_of(3, 2.0), 4.5));
        let bars: Vec<f64> = m.bars(0.0, 6.0).iter().map(|b| b.secs).collect();
        assert_eq!(bars, vec![0.0, 2.0, 4.0, 6.0]);
        assert!(close(m.snap(1.1, Grid::Quarters(1.0)), 1.0));
        assert!(close(m.snap(1.1, Grid::Bar), 2.0));
    }

    #[test]
    fn tempo_change_mid_song() {
        // 120 bpm for two bars (4 s), then 60 bpm.
        let m = TempoMap::new(120.0, (4, 4), &[TempoChange { at_secs: 4.0, bpm: 60.0, time_signature: None }]);
        assert_eq!(m.tempo_at(3.9), 120.0);
        assert_eq!(m.tempo_at(4.0), 60.0);
        assert!(close(m.quarters_at(5.0), 9.0));
        assert!(close(m.secs_at(9.0), 5.0));
        let bars: Vec<f64> = m.bars(0.0, 12.0).iter().map(|b| b.secs).collect();
        assert_eq!(bars, vec![0.0, 2.0, 4.0, 8.0, 12.0]);
        assert_eq!(m.position(6.0).bar, 3);
        assert_eq!(m.position(6.0).beat, 3);
        assert!(close(m.secs_of(4, 1.0), 8.0));
    }

    #[test]
    fn signature_change_starts_a_new_bar_and_counts_a_cut_bar() {
        // 4/4 at 120; at 3 s (mid bar 2) switch to 3/4.
        let m = TempoMap::new(120.0, (4, 4), &[TempoChange { at_secs: 3.0, bpm: 120.0, time_signature: Some((3, 4)) }]);
        let bars: Vec<(i64, f64, (u32, u32))> = m.bars(0.0, 6.0).iter().map(|b| (b.index, b.secs, b.sig)).collect();
        assert_eq!(bars, vec![(0, 0.0, (4, 4)), (1, 2.0, (4, 4)), (2, 3.0, (3, 4)), (3, 4.5, (3, 4)), (4, 6.0, (3, 4))]);
        assert!(m.bars(0.0, 6.0)[2].sig_change);
        assert_eq!(m.position(3.6), MusicalPos { bar: 3, beat: 2, fraction: m.position(3.6).fraction });
        // Grid lines restart at the new bar.
        let beats = m.grid_lines(1.0, 2.0, 4.5);
        assert_eq!(beats, vec![2.0, 2.5, 3.0, 3.5, 4.0, 4.5]);
    }

    #[test]
    fn eighth_note_meters_and_compound_grids() {
        let m = TempoMap::new(120.0, (6, 8), &[]);
        assert!(close(m.bar_secs_at(0.0), 1.5));
        assert!(close(m.beat_secs_at(0.0), 0.25));
        assert_eq!(m.position(0.75), MusicalPos { bar: 1, beat: 4, fraction: 0.0 });
    }
}
