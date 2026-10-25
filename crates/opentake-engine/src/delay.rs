//! Built-in stereo delay: time in milliseconds or synced to the tempo (straight, dotted and
//! triplet note values), feedback with low/high cuts on the repeats, ping-pong, dry/wet mix.
//! The buffer is allocated up front (4 s); delay-time changes glide instead of clicking.

use crate::mixer::{MidiEvent, Processor, Transport};

pub const NAME: &str = "OpenTake Delay";
pub const PATH: &str = "delay";

pub const SYNC: u32 = 0;
pub const TIME: u32 = 1;
pub const NOTE: u32 = 2;
pub const FEEDBACK: u32 = 3;
pub const MIX: u32 = 4;
pub const LOW_CUT: u32 = 5;
pub const HIGH_CUT: u32 = 6;
pub const PING_PONG: u32 = 7;

const MAX_SECS: f64 = 4.0;

/// Synced note values: (label, length in quarter notes).
pub const NOTES: [(&str, f64); 12] = [
    ("1/32", 0.125),
    ("1/16T", 1.0 / 6.0),
    ("1/16", 0.25),
    ("1/16D", 0.375),
    ("1/8T", 1.0 / 3.0),
    ("1/8", 0.5),
    ("1/8D", 0.75),
    ("1/4T", 2.0 / 3.0),
    ("1/4", 1.0),
    ("1/4D", 1.5),
    ("1/2", 2.0),
    ("1/1", 4.0),
];

fn log_range(v: f64, lo: f64, hi: f64) -> f64 {
    lo * (hi / lo).powf(v.clamp(0.0, 1.0))
}

fn norm_log(x: f64, lo: f64, hi: f64) -> f64 {
    ((x.max(lo) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

pub fn time_of(v: f64) -> f64 {
    log_range(v, 1.0, 2000.0)
}
pub fn norm_time(ms: f64) -> f64 {
    norm_log(ms, 1.0, 2000.0)
}
pub fn note_of(v: f64) -> usize {
    ((v.clamp(0.0, 1.0) * (NOTES.len() - 1) as f64).round() as usize).min(NOTES.len() - 1)
}
pub fn norm_note(index: usize) -> f64 {
    index.min(NOTES.len() - 1) as f64 / (NOTES.len() - 1) as f64
}
pub fn feedback_of(v: f64) -> f64 {
    0.95 * v.clamp(0.0, 1.0)
}
pub fn low_cut_of(v: f64) -> f64 {
    log_range(v, 20.0, 2000.0)
}
pub fn norm_low_cut(hz: f64) -> f64 {
    norm_log(hz, 20.0, 2000.0)
}
pub fn high_cut_of(v: f64) -> f64 {
    log_range(v, 1000.0, 20_000.0)
}
pub fn norm_high_cut(hz: f64) -> f64 {
    norm_log(hz, 1000.0, 20_000.0)
}

/// Parameters: (id, title, units, default normalized value, steps).
pub fn params() -> Vec<(u32, String, &'static str, f64, i32)> {
    vec![
        (SYNC, "Sync".into(), "", 1.0, 1),
        (TIME, "Time".into(), "ms", norm_time(375.0), 0),
        (NOTE, "Note".into(), "", norm_note(6), (NOTES.len() - 1) as i32),
        (FEEDBACK, "Feedback".into(), "%", 0.35 / 0.95, 0),
        (MIX, "Mix".into(), "%", 0.25, 0),
        (LOW_CUT, "Low Cut".into(), "Hz", norm_low_cut(150.0), 0),
        (HIGH_CUT, "High Cut".into(), "Hz", norm_high_cut(6000.0), 0),
        (PING_PONG, "Ping-Pong".into(), "", 0.0, 1),
    ]
}

pub fn display(id: u32, v: f64) -> String {
    let hz = |f: f64| if f >= 1000.0 { format!("{:.1}k", f / 1000.0) } else { format!("{f:.0}") };
    match id {
        SYNC | PING_PONG => if v >= 0.5 { "On" } else { "Off" }.into(),
        TIME => format!("{:.0}", time_of(v)),
        NOTE => NOTES[note_of(v)].0.into(),
        FEEDBACK => format!("{:.0}", feedback_of(v) * 100.0),
        MIX => format!("{:.0}", v * 100.0),
        LOW_CUT => hz(low_cut_of(v)),
        HIGH_CUT => hz(high_cut_of(v)),
        _ => format!("{v:.2}"),
    }
}

/// One-pole filter state for the repeats.
#[derive(Clone, Copy, Default)]
struct OnePole {
    z: f32,
}

impl OnePole {
    fn low(&mut self, x: f32, a: f32) -> f32 {
        self.z += a * (x - self.z);
        self.z
    }
}

pub struct Delay {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    buffer: [Vec<f32>; 2],
    write: usize,
    /// Current (gliding) and target delay in samples.
    current: f64,
    target_ms_fixed: Option<f64>,
    feedback: f32,
    mix: f32,
    low_cut: f32,
    high_cut: f32,
    ping_pong: bool,
    synced_note: Option<f64>,
    lows: [OnePole; 2],
    highs: [OnePole; 2],
    dirty: bool,
    /// The first block starts at the set time; later changes glide.
    primed: bool,
}

impl Delay {
    pub fn new(sample_rate: f64) -> Self {
        let len = (MAX_SECS * sample_rate) as usize + 2;
        let values: Vec<(u32, f64)> = params().into_iter().map(|p| (p.0, p.3)).collect();
        let mut d = Self {
            sample_rate: sample_rate.max(1.0),
            values,
            buffer: [vec![0.0; len], vec![0.0; len]],
            write: 0,
            current: 0.0,
            target_ms_fixed: None,
            feedback: 0.0,
            mix: 0.0,
            low_cut: 0.0,
            high_cut: 1.0,
            ping_pong: false,
            synced_note: None,
            lows: [OnePole::default(); 2],
            highs: [OnePole::default(); 2],
            dirty: true,
            primed: false,
        };
        d.update();
        d.current = d.target_samples(120.0);
        d
    }

    fn update(&mut self) {
        let v = |id: u32| self.values.iter().find(|p| p.0 == id).map_or(0.0, |p| p.1);
        let sync = v(SYNC) >= 0.5;
        self.synced_note = sync.then(|| NOTES[note_of(v(NOTE))].1);
        self.target_ms_fixed = (!sync).then(|| time_of(v(TIME)));
        self.feedback = feedback_of(v(FEEDBACK)) as f32;
        self.mix = v(MIX) as f32;
        // One-pole coefficients: low cut = input minus its low-passed part.
        let coef = |hz: f64| (1.0 - (-std::f64::consts::TAU * hz / self.sample_rate).exp()) as f32;
        self.low_cut = coef(low_cut_of(v(LOW_CUT)));
        self.high_cut = coef(high_cut_of(v(HIGH_CUT)));
        self.ping_pong = v(PING_PONG) >= 0.5;
        self.dirty = false;
    }

    /// Delay length in samples at `bpm` (quarter notes per minute).
    fn target_samples(&self, bpm: f64) -> f64 {
        let ms = match (self.synced_note, self.target_ms_fixed) {
            (Some(quarters), _) => quarters * 60_000.0 / bpm.max(1.0),
            (None, Some(ms)) => ms,
            _ => 375.0,
        };
        (ms * 0.001 * self.sample_rate).clamp(1.0, (self.buffer[0].len() - 2) as f64)
    }

    fn read(&self, ch: usize, delay: f64) -> f32 {
        let len = self.buffer[ch].len();
        let pos = self.write as f64 - delay;
        let pos = if pos < 0.0 { pos + len as f64 } else { pos };
        let (i, frac) = (pos.floor() as usize % len, pos.fract() as f32);
        let a = self.buffer[ch][i];
        let b = self.buffer[ch][(i + 1) % len];
        a + (b - a) * frac
    }
}

impl Processor for Delay {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        let target = self.target_samples(if t.tempo_bpm > 0.0 { t.tempo_bpm } else { 120.0 });
        if !self.primed {
            self.current = target;
            self.primed = true;
        }
        // Glide time ~40 ms, so tempo changes and knob moves don't click.
        let glide = 1.0 - (-1.0 / (0.04 * self.sample_rate)).exp();
        let len = self.buffer[0].len();
        for i in 0..frames {
            self.current += (target - self.current) * glide;
            let (dl, dr) = (self.read(0, self.current), self.read(1, self.current));
            // Shape the repeats: high cut, then low cut.
            let mut shape = |ch: usize, x: f32| {
                let hc = self.highs[ch].low(x, self.high_cut);
                hc - self.lows[ch].low(hc, self.low_cut)
            };
            let (wl, wr) = (shape(0, dl), shape(1, dr));
            let (x_l, x_r) = (il[i], ir[i]);
            if self.ping_pong {
                // Mono input into the left line; each line feeds the other.
                self.buffer[0][self.write] = (x_l + x_r) * 0.5 + wr * self.feedback;
                self.buffer[1][self.write] = wl * self.feedback;
            } else {
                self.buffer[0][self.write] = x_l + wl * self.feedback;
                self.buffer[1][self.write] = x_r + wr * self.feedback;
            }
            self.write = (self.write + 1) % len;
            ol[i] = x_l * (1.0 - self.mix) + wl * self.mix;
            or[i] = x_r * (1.0 - self.mix) + wr * self.mix;
        }
    }

    fn set_param(&mut self, id: u32, normalized: f64) {
        if let Some(v) = self.values.iter_mut().find(|p| p.0 == id) {
            v.1 = normalized.clamp(0.0, 1.0);
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(d: &mut Delay, x: &[f32], bpm: f64) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (vec![0.0f32; x.len()], vec![0.0f32; x.len()]);
        let t = Transport { tempo_bpm: bpm, ..Default::default() };
        for start in (0..x.len()).step_by(480) {
            let end = (start + 480).min(x.len());
            let (ol, or) = (&mut l[start..end], &mut r[start..end]);
            d.process(&[&x[start..end], &x[start..end]], &mut [ol, or], end - start, &t, &[]);
        }
        (l, r)
    }

    fn impulse(len: usize) -> Vec<f32> {
        let mut x = vec![0.0; len];
        x[0] = 1.0;
        x
    }

    fn peaks(x: &[f32]) -> Vec<usize> {
        (1..x.len() - 1).filter(|&i| x[i].abs() > 0.02 && x[i].abs() >= x[i - 1].abs() && x[i].abs() >= x[i + 1].abs()).collect()
    }

    #[test]
    fn echoes_at_the_set_time_and_fade_with_feedback() {
        let mut d = Delay::new(48_000.0);
        d.set_param(SYNC, 0.0);
        d.set_param(TIME, norm_time(250.0));
        d.set_param(MIX, 1.0);
        d.set_param(LOW_CUT, 0.0);
        d.set_param(HIGH_CUT, 1.0);
        let (l, _) = run(&mut d, &impulse(48_000), 120.0);
        let echoes = peaks(&l);
        assert!(echoes.len() >= 3, "{echoes:?}");
        for (k, &p) in echoes.iter().take(3).enumerate() {
            let want = 12_000 * (k + 1);
            assert!(p.abs_diff(want) <= 2, "echo {k} at {p}, want {want}");
        }
        assert!(l[echoes[1]].abs() < l[echoes[0]].abs(), "repeats fade");
    }

    #[test]
    fn synced_time_follows_the_tempo_and_ping_pong_alternates() {
        let mut d = Delay::new(48_000.0);
        d.set_param(SYNC, 1.0);
        d.set_param(NOTE, norm_note(8)); // 1/4
        d.set_param(MIX, 1.0);
        d.set_param(LOW_CUT, 0.0);
        d.set_param(HIGH_CUT, 1.0);
        d.set_param(PING_PONG, 1.0);
        let (l, r) = run(&mut d, &impulse(96_000), 100.0); // quarter = 0.6 s = 28800 samples
        let (pl, pr) = (peaks(&l), peaks(&r));
        assert!(pl.first().is_some_and(|&p| p.abs_diff(28_800) <= 2), "left {pl:?}");
        assert!(pr.first().is_some_and(|&p| p.abs_diff(57_600) <= 2), "right {pr:?}");
    }
}
