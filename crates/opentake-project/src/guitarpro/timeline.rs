//! Turns a [`Score`] into a flat, time-ordered performance: repeats are expanded, tempo
//! changes applied, ties merged and effects (tremolo, let ring, triplet feel, grace notes,
//! brush strokes) resolved. Ported from GuitarCraft's `timeline.ts`.

use std::collections::HashMap;

use super::{Score, TICKS_PER_QUARTER, TrackKind};

/// One played occurrence of a master bar.
#[derive(Debug, Clone)]
pub struct BarOccurrence {
    pub bar: usize,
    /// Seconds from the song start.
    pub time: f64,
    pub duration: f64,
    /// Tempo segments inside the bar: (tick, time, bpm).
    segments: Vec<(f64, f64, f64)>,
}

impl BarOccurrence {
    fn tick_to_time(&self, tick: f64) -> f64 {
        let s = self.segments.iter().rev().find(|s| s.0 <= tick).unwrap_or(&self.segments[0]);
        s.1 + (tick - s.0) * 60.0 / (s.2 * TICKS_PER_QUARTER as f64)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoteEvent {
    pub track: usize,
    /// Seconds from the song start.
    pub time: f64,
    pub duration: f64,
    pub midi: u8,
    pub velocity: u8,
    pub string: u8,
    pub fret: u8,
}

#[derive(Debug, Clone)]
pub struct Performance {
    pub bars: Vec<BarOccurrence>,
    pub events: Vec<NoteEvent>,
    /// Tempo changes in playing order: (seconds from the song start, quarter notes per minute).
    pub tempos: Vec<(f64, f64)>,
    /// Song length in seconds.
    pub duration: f64,
}

/// Bar indices in the order they're played (repeats and alternate endings expanded).
pub fn playback_order(score: &Score) -> Vec<usize> {
    let bars = &score.master_bars;
    let mut order = Vec::new();
    let (mut repeat_start, mut pass, mut jumped_back, mut prev_had_alternates) = (0usize, 1u32, false, false);
    let mut i = 0;
    while i < bars.len() && order.len() < 20_000 {
        let mb = &bars[i];
        if mb.repeat_start && !jumped_back {
            repeat_start = i;
            pass = 1;
        }
        jumped_back = false;

        if !mb.alternate_endings.is_empty() {
            prev_had_alternates = true;
            if !mb.alternate_endings.contains(&pass) {
                i += 1;
                continue;
            }
        } else if prev_had_alternates {
            // Leaving a volta section: whatever repeat it belonged to is over.
            prev_had_alternates = false;
            pass = 1;
        }

        order.push(i);
        if mb.repeat_count > 0 {
            if pass < mb.repeat_count {
                pass += 1;
                i = repeat_start;
                jumped_back = true;
                prev_had_alternates = false;
                continue;
            }
            if mb.alternate_endings.is_empty() {
                pass = 1;
            }
            repeat_start = i + 1;
        }
        i += 1;
    }
    order
}

/// Piecewise-linear swing warp for triplet feel, applied to tick positions in a bar.
fn feel_warp(feel: Option<&str>) -> Option<impl Fn(f64) -> f64> {
    let q = TICKS_PER_QUARTER as f64;
    let (unit, from, to) = match feel? {
        "Triplet8th" => (q, q / 2.0, q * 2.0 / 3.0),
        "Triplet16th" => (q / 2.0, q / 4.0, q / 3.0),
        "Dotted8th" => (q, q / 2.0, q * 3.0 / 4.0),
        "Dotted16th" => (q / 2.0, q / 4.0, q * 3.0 / 8.0),
        _ => return None,
    };
    Some(move |tick: f64| {
        let base = (tick / unit).floor() * unit;
        let t = tick - base;
        let warped = if t <= from { t * to / from } else { to + (t - from) * (unit - to) / (unit - from) };
        base + warped
    })
}

fn brush_delay(up: bool, string: u8, string_count: usize) -> f64 {
    // String 0 is the lowest; a down strum hits the low strings first.
    let order = if up { string_count.saturating_sub(1).saturating_sub(string as usize) } else { string as usize };
    order as f64 * 0.012
}

pub fn build_performance(score: &Score) -> Performance {
    let order = playback_order(score);
    let mut tempo_by_bar: HashMap<usize, Vec<(f64, f64)>> = HashMap::new();
    for t in &score.tempos {
        if let Some(mb) = score.master_bars.get(t.bar) {
            tempo_by_bar.entry(t.bar).or_default().push(((t.position * mb.duration as f64).round(), t.bpm));
        }
    }

    let mut bpm = score.tempos.first().map_or(120.0, |t| t.bpm);
    let mut time = 0.0;
    let bars: Vec<BarOccurrence> = order
        .iter()
        .map(|&bar| {
            let mb = &score.master_bars[bar];
            let mut segments = vec![(0.0, time, bpm)];
            for &(tick, new_bpm) in tempo_by_bar.get(&bar).map(Vec::as_slice).unwrap_or_default() {
                let last = *segments.last().unwrap();
                let t = last.1 + (tick - last.0) * 60.0 / (last.2 * TICKS_PER_QUARTER as f64);
                if tick == 0.0 {
                    segments[0] = (0.0, time, new_bpm);
                } else {
                    segments.push((tick, t, new_bpm));
                }
            }
            let mut occ = BarOccurrence { bar, time, duration: 0.0, segments };
            bpm = occ.segments.last().unwrap().2;
            let end = occ.tick_to_time(mb.duration as f64);
            occ.duration = end - time;
            time = end;
            occ
        })
        .collect();

    let mut events: Vec<NoteEvent> = Vec::new();
    for (ti, track) in score.tracks.iter().enumerate() {
        let drums = track.kind == TrackKind::Drums;
        // Last event per voice+string (or per drum key), for ties and cutting off ringing notes.
        let mut last: HashMap<(usize, u8), usize> = HashMap::new();
        for occ in &bars {
            let mb = &score.master_bars[occ.bar];
            let warp = feel_warp(mb.triplet_feel.as_deref());
            let at = |tick: f64| occ.tick_to_time(warp.as_ref().map_or(tick, |w| w(tick)));
            let Some(bar) = track.bars.get(occ.bar) else { continue };
            for (vi, voice) in bar.voices.iter().enumerate() {
                for beat in &voice.beats {
                    if beat.start >= mb.duration {
                        continue;
                    }
                    let (start, end) = if beat.grace {
                        let next = at(beat.start as f64);
                        ((next - 0.06).max(occ.time), next)
                    } else {
                        (at(beat.start as f64), at((beat.start + beat.duration).min(mb.duration) as f64))
                    };
                    for note in &beat.notes {
                        let key = if drums { (usize::MAX, note.midi) } else { (vi, note.string) };
                        let prev = last.get(&key).copied();
                        if note.tie_destination && !drums {
                            if let Some(p) = prev {
                                let e = &mut events[p];
                                e.duration = e.duration.max(end - e.time);
                                continue;
                            }
                        }
                        if let Some(p) = prev.filter(|_| !drums) {
                            let e = &mut events[p];
                            if e.time + e.duration > start {
                                e.duration = (start - e.time).max(0.01);
                            }
                        }
                        let brush = beat.brush_up.map_or(0.0, |up| brush_delay(up, note.string, track.tuning.len()));
                        let velocity = (beat.dynamic as i32 + if note.accent { 15 } else { 0 } - if note.ghost { 30 } else { 0 }).clamp(1, 127) as u8;
                        let make = |time: f64, duration: f64| NoteEvent { track: ti, time, duration, midi: note.midi, velocity, string: note.string, fret: note.fret };
                        if let Some(sub) = beat.tremolo.filter(|_| !beat.grace) {
                            let step = 4.0 * TICKS_PER_QUARTER as f64 / sub as f64;
                            let stop = (beat.start + beat.duration).min(mb.duration) as f64;
                            let mut t = beat.start as f64;
                            while t < stop {
                                events.push(make(at(t), at(t + step) - at(t)));
                                last.insert(key, events.len() - 1);
                                t += step;
                            }
                            continue;
                        }
                        let ringing = if note.let_ring { (end - start).max(3.0) } else { end - start };
                        events.push(make(start + brush, (ringing - brush).max(0.02)));
                        last.insert(key, events.len() - 1);
                    }
                }
            }
        }
    }
    events.sort_by(|a, b| a.time.total_cmp(&b.time));
    let mut tempos: Vec<(f64, f64)> = Vec::new();
    for &(_, t, bpm) in bars.iter().flat_map(|b| &b.segments) {
        if tempos.last().is_none_or(|l| (l.1 - bpm).abs() > 1e-6) {
            tempos.push((t, bpm));
        }
    }
    Performance { bars, events, tempos, duration: time }
}

#[cfg(test)]
mod tests {
    use super::super::load;
    use super::*;

    fn fixture() -> Score {
        load(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/simple.gp")).unwrap()
    }

    #[test]
    fn expands_repeats_with_alternate_endings() {
        assert_eq!(playback_order(&fixture()), vec![0, 1, 0, 2, 3]);
    }

    #[test]
    fn applies_tempo_changes_and_merges_ties() {
        let perf = build_performance(&fixture());
        assert_eq!(perf.bars.iter().map(|b| b.time).collect::<Vec<_>>(), vec![0.0, 2.0, 4.0, 6.0, 8.0]);
        assert!((perf.duration - 12.0).abs() < 1e-9);
        // 120 bpm, slowing to 60 at bar 4 (played fifth, after the repeat).
        assert_eq!(perf.tempos, vec![(0.0, 120.0), (8.0, 60.0)]);

        let guitar: Vec<&NoteEvent> = perf.events.iter().filter(|e| e.track == 0).collect();
        // Tied half notes become one 2 s event, on both passes through bar 0.
        let tied: Vec<(f64, f64)> = guitar.iter().filter(|e| e.midi == 45 && e.fret == 5).map(|e| (e.time, e.duration)).collect();
        assert_eq!(tied, vec![(0.0, 2.0), (4.0, 2.0)]);
        let triplets: Vec<f64> = guitar.iter().filter(|e| e.time >= 6.0 && e.time < 6.5).map(|e| e.time).collect();
        assert_eq!(triplets.len(), 3);
        for (t, want) in triplets.iter().zip([6.0, 6.0 + 1.0 / 6.0, 6.0 + 2.0 / 6.0]) {
            assert!((t - want).abs() < 1e-9, "{t} vs {want}");
        }
        let drums: Vec<f64> = perf.events.iter().filter(|e| e.track == 1).map(|e| e.time).collect();
        assert_eq!(drums, vec![0.0, 4.0]);
    }
}
