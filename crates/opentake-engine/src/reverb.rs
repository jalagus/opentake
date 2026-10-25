//! Built-in algorithmic stereo reverb: pre-delay, a chain of all-pass diffusers per channel,
//! then an 8-line feedback delay network mixed by a Hadamard matrix. Each line's gain comes
//! from its own length so the whole tail decays at the set RT60; a one-pole low-pass in every
//! line makes the highs die sooner (damping), and slowly modulated read positions keep the
//! tail from ringing metallically. The wet signal gets a low cut and a width control.
//!
//! All buffers are allocated in `new()`. Size changes glide the line lengths, and every other
//! control is smoothed, so moving knobs while audio plays doesn't click.

use std::sync::Arc;

use crate::dynamics::GainMeter;
use crate::mixer::{MidiEvent, Processor, Transport};

pub const NAME: &str = "OpenTake Reverb";
pub const PATH: &str = "reverb";

pub const PRE_DELAY: u32 = 0;
pub const SIZE: u32 = 1;
pub const DECAY: u32 = 2;
pub const DAMPING: u32 = 3;
pub const LOW_CUT: u32 = 4;
pub const WIDTH: u32 = 5;
pub const DIFFUSION: u32 = 6;
pub const MODULATION: u32 = 7;
pub const MIX: u32 = 8;
pub const LEVEL: u32 = 9;

const LINES: usize = 8;
/// Line lengths (ms) at a size scale of 1; spread out and mutually far from simple ratios.
const LINE_MS: [f64; LINES] = [31.1, 37.3, 41.9, 47.5, 53.3, 59.1, 67.3, 73.7];
/// Input diffusers (ms) per channel; the right chain is slightly longer to decorrelate.
const DIFFUSER_MS: [f64; 4] = [4.77, 3.59, 12.73, 9.31];
const DIFFUSER_SPREAD: f64 = 1.071;
const MAX_PRE_DELAY_MS: f64 = 200.0;
/// Peak modulation excursion (ms) at full Modulation.
const MAX_MOD_MS: f64 = 0.6;
/// LFO rates (Hz), one per line.
const LFO_HZ: [f64; LINES] = [0.31, 0.43, 0.53, 0.67, 0.79, 0.89, 1.03, 1.13];
/// Keeps the wet level in the same ballpark as the dry signal at medium decays.
const WET_GAIN: f32 = 0.45;

fn log_range(v: f64, lo: f64, hi: f64) -> f64 {
    lo * (hi / lo).powf(v.clamp(0.0, 1.0))
}

fn norm_log(x: f64, lo: f64, hi: f64) -> f64 {
    ((x.max(lo) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

pub fn pre_delay_of(v: f64) -> f64 {
    MAX_PRE_DELAY_MS * v.clamp(0.0, 1.0)
}
pub fn norm_pre_delay(ms: f64) -> f64 {
    (ms / MAX_PRE_DELAY_MS).clamp(0.0, 1.0)
}
/// Size 0..1 scales the line lengths 0.3..1.5 times.
pub fn size_scale(v: f64) -> f64 {
    0.3 + 1.2 * v.clamp(0.0, 1.0)
}
pub fn decay_of(v: f64) -> f64 {
    log_range(v, 0.2, 20.0)
}
pub fn norm_decay(secs: f64) -> f64 {
    norm_log(secs, 0.2, 20.0)
}
pub fn damping_of(v: f64) -> f64 {
    log_range(v, 1000.0, 20_000.0)
}
pub fn norm_damping(hz: f64) -> f64 {
    norm_log(hz, 1000.0, 20_000.0)
}
pub fn low_cut_of(v: f64) -> f64 {
    log_range(v, 20.0, 1000.0)
}
pub fn norm_low_cut(hz: f64) -> f64 {
    norm_log(hz, 20.0, 1000.0)
}
/// Wet level: -24..+12 dB.
pub fn level_of(v: f64) -> f64 {
    -24.0 + 36.0 * v.clamp(0.0, 1.0)
}
pub fn norm_level(db: f64) -> f64 {
    ((db + 24.0) / 36.0).clamp(0.0, 1.0)
}

/// Parameters: (id, title, units, default normalized value, steps). Mix defaults to 100% wet
/// for the usual setup, a reverb bus fed by sends; turn it down when used as an insert.
pub fn params() -> Vec<(u32, String, &'static str, f64, i32)> {
    vec![
        (PRE_DELAY, "Pre-Delay".into(), "ms", norm_pre_delay(20.0), 0),
        (SIZE, "Size".into(), "%", 0.5, 0),
        (DECAY, "Decay".into(), "s", norm_decay(2.0), 0),
        (DAMPING, "Damping".into(), "Hz", norm_damping(7000.0), 0),
        (LOW_CUT, "Low Cut".into(), "Hz", norm_low_cut(100.0), 0),
        (WIDTH, "Width".into(), "%", 1.0, 0),
        (DIFFUSION, "Diffusion".into(), "%", 0.7, 0),
        (MODULATION, "Modulation".into(), "%", 0.35, 0),
        (MIX, "Mix".into(), "% wet", 1.0, 0),
        (LEVEL, "Level".into(), "dB", norm_level(0.0), 0),
    ]
}

pub fn display(id: u32, v: f64) -> String {
    let hz = |f: f64| if f >= 1000.0 { format!("{:.1}k", f / 1000.0) } else { format!("{f:.0}") };
    match id {
        PRE_DELAY => format!("{:.0}", pre_delay_of(v)),
        DECAY => {
            let s = decay_of(v);
            if s < 10.0 { format!("{s:.2}") } else { format!("{s:.1}") }
        }
        DAMPING => hz(damping_of(v)),
        LOW_CUT => hz(low_cut_of(v)),
        LEVEL => format!("{:+.1}", level_of(v)),
        SIZE | WIDTH | DIFFUSION | MODULATION | MIX => format!("{:.0}", v.clamp(0.0, 1.0) * 100.0),
        _ => format!("{v:.2}"),
    }
}

/// All settings in real units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub pre_delay_ms: f64,
    pub size: f64,
    pub decay_secs: f64,
    pub damping_hz: f64,
    pub low_cut_hz: f64,
    pub width: f64,
    pub diffusion: f64,
    pub modulation: f64,
    pub mix: f64,
    pub level_db: f64,
}

impl Settings {
    pub fn from_params(values: &[(u32, f64)]) -> Self {
        let defaults = params();
        let v = |id: u32| values.iter().rev().find(|p| p.0 == id).map(|p| p.1).or_else(|| defaults.iter().find(|p| p.0 == id).map(|p| p.3)).unwrap_or(0.0);
        Self {
            pre_delay_ms: pre_delay_of(v(PRE_DELAY)),
            size: v(SIZE),
            decay_secs: decay_of(v(DECAY)),
            damping_hz: damping_of(v(DAMPING)),
            low_cut_hz: low_cut_of(v(LOW_CUT)),
            width: v(WIDTH),
            diffusion: v(DIFFUSION),
            modulation: v(MODULATION),
            mix: v(MIX),
            level_db: level_of(v(LEVEL)),
        }
    }

    /// Mean loop length (seconds) of the delay network at this size.
    pub fn mean_loop_secs(&self) -> f64 {
        LINE_MS.iter().sum::<f64>() / LINES as f64 * size_scale(self.size) * 0.001
    }

    /// The decay time (RT60, seconds) at `hz`: the set decay, shortened by what the damping
    /// filter takes away on every trip around the loop. The low and mid range decays at the
    /// set time; this is what the editor's high band shows.
    pub fn decay_at(&self, hz: f64, sample_rate: f64) -> f64 {
        let a = damping_coef(self.damping_hz, sample_rate);
        let w = std::f64::consts::TAU * (hz / sample_rate).min(0.5);
        let mag = (1.0 - a) / (1.0 - 2.0 * a * w.cos() + a * a).sqrt();
        let loss_per_sec = -20.0 * mag.log10() / self.mean_loop_secs();
        60.0 / (60.0 / self.decay_secs + loss_per_sec)
    }
}

/// Feedback coefficient of the in-loop one-pole low-pass (`y += (1 - a) * (x - y)`).
fn damping_coef(hz: f64, sample_rate: f64) -> f64 {
    (-std::f64::consts::TAU * hz.min(0.49 * sample_rate) / sample_rate).exp()
}

/// Removes subnormals: they'd otherwise build up as the tail fades and slow the CPU down.
#[inline]
fn flush(x: f32) -> f32 {
    (x + 1e-18) - 1e-18
}

/// A power-of-two ring buffer read at fractional delays.
struct Line {
    buf: Vec<f32>,
    mask: usize,
}

impl Line {
    fn new(max_samples: f64) -> Self {
        let len = (max_samples.ceil() as usize + 4).next_power_of_two();
        Self { buf: vec![0.0; len], mask: len - 1 }
    }

    /// Linear interpolation `delay` samples behind `write`.
    #[inline]
    fn read(&self, write: usize, delay: f32) -> f32 {
        let d = delay.max(1.0);
        let whole = d as usize;
        let frac = d - whole as f32;
        let a = self.buf[write.wrapping_sub(whole) & self.mask];
        let b = self.buf[write.wrapping_sub(whole + 1) & self.mask];
        a + (b - a) * frac
    }

    #[inline]
    fn write(&mut self, write: usize, x: f32) {
        self.buf[write & self.mask] = x;
    }
}

/// Schroeder all-pass with a fixed length.
struct AllPass {
    line: Line,
    len: usize,
}

impl AllPass {
    fn new(samples: f64) -> Self {
        Self { line: Line::new(samples), len: (samples.round() as usize).max(1) }
    }

    #[inline]
    fn run(&mut self, write: usize, x: f32, g: f32) -> f32 {
        let delayed = self.line.buf[write.wrapping_sub(self.len) & self.line.mask];
        let v = flush(x - g * delayed);
        self.line.write(write, v);
        delayed + g * v
    }
}

/// 12 dB/oct state-variable high-pass (topology-preserving, so cutoff changes are smooth).
#[derive(Clone, Copy, Default)]
struct HighPass {
    s1: f32,
    s2: f32,
}

impl HighPass {
    #[inline]
    fn run(&mut self, x: f32, g: f32, k: f32) -> f32 {
        let hp = (x - (k + g) * self.s1 - self.s2) / (1.0 + g * (k + g));
        let bp = g * hp + self.s1;
        self.s1 = flush(g * hp + bp);
        let lp = g * bp + self.s2;
        self.s2 = flush(g * bp + lp);
        hp
    }
}

/// In-place 8-point Hadamard transform, scaled to stay orthogonal (energy-preserving).
#[inline]
fn hadamard(x: &mut [f32; LINES]) {
    let mut h = 1;
    while h < LINES {
        for i in (0..LINES).step_by(h * 2) {
            for j in i..i + h {
                let (a, b) = (x[j], x[j + h]);
                x[j] = a + b;
                x[j + h] = a - b;
            }
        }
        h *= 2;
    }
    let scale = (1.0 / LINES as f32).sqrt();
    for v in x.iter_mut() {
        *v *= scale;
    }
}

/// Output taps: two orthogonal sign patterns over the lines give decorrelated left and right.
const TAP_L: [f32; LINES] = [1.0, -1.0, 1.0, -1.0, 1.0, -1.0, 1.0, -1.0];
const TAP_R: [f32; LINES] = [1.0, 1.0, -1.0, -1.0, 1.0, 1.0, -1.0, -1.0];
/// Input injection: left into the even lines, right into the odd ones, with mixed signs.
const IN_L: [f32; LINES] = [1.0, 0.0, -1.0, 0.0, 1.0, 0.0, -1.0, 0.0];
const IN_R: [f32; LINES] = [0.0, 1.0, 0.0, -1.0, 0.0, -1.0, 0.0, 1.0];

/// Values smoothed per sample toward per-block targets.
#[derive(Clone, Copy)]
struct Smooth {
    gains: [f32; LINES],
    lengths: [f32; LINES],
    pre_delay: f32,
    damp: f32,
    mod_depth: f32,
    diffusion: f32,
    mix: f32,
    wet: f32,
    width: f32,
}

pub struct Reverb {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    settings: Settings,
    pre: [Line; 2],
    diffusers: [[AllPass; 4]; 2],
    lines: [Line; LINES],
    damp_state: [f32; LINES],
    low_cut: [HighPass; 2],
    /// Low-cut frequency, smoothed per block.
    low_cut_hz: f64,
    /// Per-line LFO as a rotating phasor (cos, sin) and its per-sample rotation.
    lfo: [(f32, f32); LINES],
    lfo_step: [(f32, f32); LINES],
    write: usize,
    cur: Smooth,
    target: Smooth,
    /// The first block starts at the targets; later changes glide.
    primed: bool,
    dirty: bool,
    meter: Option<Arc<GainMeter>>,
}

impl Reverb {
    pub fn new(sample_rate: f64, meter: Option<Arc<GainMeter>>) -> Self {
        let sr = sample_rate.max(1.0);
        let ms = |m: f64| m * 0.001 * sr;
        let values: Vec<(u32, f64)> = params().into_iter().map(|p| (p.0, p.3)).collect();
        let max_line = ms(LINE_MS[LINES - 1] * size_scale(1.0) + MAX_MOD_MS) + 2.0;
        let diffusers = |spread: f64| std::array::from_fn(|i| AllPass::new(ms(DIFFUSER_MS[i] * spread)));
        let zero = Smooth { gains: [0.0; LINES], lengths: [0.0; LINES], pre_delay: 0.0, damp: 0.0, mod_depth: 0.0, diffusion: 0.0, mix: 0.0, wet: 0.0, width: 0.0 };
        let mut r = Self {
            sample_rate: sr,
            values,
            settings: Settings::from_params(&[]),
            pre: std::array::from_fn(|_| Line::new(ms(MAX_PRE_DELAY_MS) + 2.0)),
            diffusers: [diffusers(1.0), diffusers(DIFFUSER_SPREAD)],
            lines: std::array::from_fn(|_| Line::new(max_line)),
            damp_state: [0.0; LINES],
            low_cut: [HighPass::default(); 2],
            low_cut_hz: 100.0,
            lfo: std::array::from_fn(|i| {
                let phase = i as f32 * 0.785;
                (phase.cos(), phase.sin())
            }),
            lfo_step: std::array::from_fn(|i| {
                let w = std::f64::consts::TAU * LFO_HZ[i] / sr;
                (w.cos() as f32, w.sin() as f32)
            }),
            write: 0,
            cur: zero,
            target: zero,
            primed: false,
            dirty: true,
            meter,
        };
        r.update();
        r
    }

    fn update(&mut self) {
        let s = Settings::from_params(&self.values);
        self.settings = s;
        let sr = self.sample_rate;
        let scale = size_scale(s.size);
        let t = &mut self.target;
        t.lengths = LINE_MS.map(|ms| (ms * scale * 0.001 * sr) as f32);
        t.pre_delay = (s.pre_delay_ms * 0.001 * sr) as f32;
        t.damp = damping_coef(s.damping_hz, sr) as f32;
        t.mod_depth = (s.modulation * MAX_MOD_MS * 0.001 * sr) as f32;
        t.diffusion = (0.75 * s.diffusion) as f32;
        t.mix = s.mix as f32;
        t.wet = 10f32.powf(s.level_db as f32 / 20.0) * WET_GAIN;
        t.width = s.width as f32;
        self.dirty = false;
    }
}

impl Processor for Reverb {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        let sr = self.sample_rate;
        // Line gains follow the lengths as they are now, so the decay stays right while
        // Size glides: g = 10^(-3 * length / (RT60 * sr)).
        let per_sample = -3.0 / (self.settings.decay_secs * sr);
        let gains_for = |lengths: &[f32; LINES]| lengths.map(|l| 10f64.powf(per_sample * l as f64) as f32);
        if !self.primed {
            self.cur = self.target;
            self.cur.gains = gains_for(&self.cur.lengths);
            self.low_cut_hz = self.settings.low_cut_hz;
            self.primed = true;
        }
        self.target.gains = gains_for(&self.cur.lengths);
        // Low cut: glide the cutoff per block (in octaves), recompute the filter once.
        let block = frames as f64 / sr;
        let k_hz = 1.0 - (-block / 0.03).exp();
        self.low_cut_hz *= (self.settings.low_cut_hz / self.low_cut_hz).powf(k_hz);
        let g = (std::f64::consts::PI * self.low_cut_hz.min(0.45 * sr) / sr).tan() as f32;
        let k = std::f32::consts::SQRT_2;
        // ~30 ms for levels and gains, ~150 ms for line lengths and pre-delay.
        let fast = (1.0 - (-1.0 / (0.03 * sr)).exp()) as f32;
        let slow = (1.0 - (-1.0 / (0.15 * sr)).exp()) as f32;
        let (mut peak_in, mut peak_out) = (0.0f32, 0.0f32);
        let (c, t) = (&mut self.cur, &self.target);
        for i in 0..frames {
            for n in 0..LINES {
                c.gains[n] += (t.gains[n] - c.gains[n]) * fast;
                c.lengths[n] += (t.lengths[n] - c.lengths[n]) * slow;
            }
            c.pre_delay += (t.pre_delay - c.pre_delay) * slow;
            c.damp += (t.damp - c.damp) * fast;
            c.mod_depth += (t.mod_depth - c.mod_depth) * fast;
            c.diffusion += (t.diffusion - c.diffusion) * fast;
            c.mix += (t.mix - c.mix) * fast;
            c.wet += (t.wet - c.wet) * fast;
            c.width += (t.width - c.width) * fast;

            let (x_l, x_r) = (il[i], ir[i]);
            let w = self.write;
            // Pre-delay, then the diffusers smear each transient into a dense burst.
            self.pre[0].write(w, x_l);
            self.pre[1].write(w, x_r);
            let mut d = [self.pre[0].read(w, c.pre_delay), self.pre[1].read(w, c.pre_delay)];
            for (ch, x) in d.iter_mut().enumerate() {
                for ap in &mut self.diffusers[ch] {
                    *x = ap.run(w, *x, c.diffusion);
                }
            }

            // The delay network: read (modulated), damp, apply the decay gain, mix, feed back.
            let mut y = [0.0f32; LINES];
            let (mut wet_l, mut wet_r) = (0.0f32, 0.0f32);
            for n in 0..LINES {
                let (cos, sin) = self.lfo[n];
                let (sc, ss) = self.lfo_step[n];
                self.lfo[n] = (cos * sc - sin * ss, sin * sc + cos * ss);
                let raw = self.lines[n].read(w, c.lengths[n] + c.mod_depth * sin);
                let s = &mut self.damp_state[n];
                *s = flush(raw + (*s - raw) * c.damp);
                y[n] = *s * c.gains[n];
                wet_l += TAP_L[n] * y[n];
                wet_r += TAP_R[n] * y[n];
            }
            hadamard(&mut y);
            for n in 0..LINES {
                self.lines[n].write(w, flush(y[n] + IN_L[n] * d[0] + IN_R[n] * d[1]));
            }
            self.write = w.wrapping_add(1);

            let (wet_l, wet_r) = (self.low_cut[0].run(wet_l, g, k), self.low_cut[1].run(wet_r, g, k));
            let (mid, side) = ((wet_l + wet_r) * 0.5, (wet_l - wet_r) * 0.5 * c.width);
            let (wl, wr) = ((mid + side) * c.wet, (mid - side) * c.wet);
            ol[i] = x_l * (1.0 - c.mix) + wl * c.mix;
            or[i] = x_r * (1.0 - c.mix) + wr * c.mix;
            peak_in = peak_in.max(x_l.abs()).max(x_r.abs());
            peak_out = peak_out.max(ol[i].abs()).max(or[i].abs());
        }
        // Keep the LFO phasors on the unit circle (rounding would slowly drift them).
        for (cos, sin) in &mut self.lfo {
            let norm = 1.5 - 0.5 * (*cos * *cos + *sin * *sin);
            *cos *= norm;
            *sin *= norm;
        }
        if let Some(m) = &self.meter {
            let db = |x: f32| 20.0 * x.max(1e-6).log10();
            m.report(0.0, db(peak_in), db(peak_out));
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

    const SR: f64 = 48_000.0;

    fn run(r: &mut Reverb, l: &[f32], rt: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let (mut ol, mut or) = (vec![0.0f32; l.len()], vec![0.0f32; l.len()]);
        for start in (0..l.len()).step_by(512) {
            let end = (start + 512).min(l.len());
            r.process(&[&l[start..end], &rt[start..end]], &mut [&mut ol[start..end], &mut or[start..end]], end - start, &Transport::default(), &[]);
        }
        (ol, or)
    }

    fn impulse(len: usize) -> Vec<f32> {
        let mut x = vec![0.0; len];
        x[0] = 1.0;
        x
    }

    fn noise(len: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as f32 / u32::MAX as f32 * 2.0 - 1.0
            })
            .collect()
    }

    /// A reverb set for measuring: no damping, no low cut, fully wet.
    fn plain(decay: f64, size: f64) -> Reverb {
        let mut r = Reverb::new(SR, None);
        r.set_param(DECAY, norm_decay(decay));
        r.set_param(SIZE, size);
        r.set_param(DAMPING, 1.0);
        r.set_param(LOW_CUT, 0.0);
        r.set_param(PRE_DELAY, 0.0);
        r
    }

    /// RT60 from the Schroeder backward-integrated energy curve: the -5..-35 dB slope, doubled.
    fn rt60(x: &[f32]) -> f64 {
        // Measure the low/mid range (as the setting is defined): a gentle one-pole low-pass.
        let a = (-std::f64::consts::TAU * 2000.0 / SR).exp();
        let mut z = 0.0;
        let e: Vec<f64> = x.iter().map(|&v| {
            z = v as f64 + (z - v as f64) * a;
            z * z
        }).collect();
        let mut acc = 0.0;
        let mut edc = vec![0.0; e.len()];
        for i in (0..e.len()).rev() {
            acc += e[i];
            edc[i] = acc;
        }
        let db = |i: usize| 10.0 * (edc[i] / edc[0]).log10();
        let t5 = (0..edc.len()).find(|&i| db(i) < -5.0).unwrap();
        let t35 = (0..edc.len()).find(|&i| db(i) < -35.0).unwrap();
        2.0 * (t35 - t5) as f64 / SR
    }

    #[test]
    fn impulse_decays_at_the_set_rt60() {
        for (decay, size) in [(1.0, 0.5), (3.0, 0.8), (0.5, 0.2)] {
            let mut r = plain(decay, size);
            let len = (SR * decay * 3.0) as usize;
            let (l, rr) = run(&mut r, &impulse(len), &impulse(len));
            let mono: Vec<f32> = l.iter().zip(&rr).map(|(a, b)| a + b).collect();
            let measured = rt60(&mono);
            assert!((measured / decay - 1.0).abs() < 0.15, "decay {decay} s at size {size}: measured {measured:.3} s");
        }
    }

    #[test]
    fn damping_shortens_the_highs_only() {
        let s = Settings { damping_hz: 3000.0, ..Settings::from_params(&[]) };
        assert!((s.decay_at(200.0, SR) / s.decay_secs - 1.0).abs() < 0.02);
        assert!(s.decay_at(8000.0, SR) < s.decay_secs * 0.5);
    }

    #[test]
    fn stays_stable_at_the_longest_decay() {
        let mut r = plain(20.0, 1.0);
        r.set_param(MODULATION, 1.0);
        let len = (SR * 20.0) as usize;
        let mut x = noise(len, 7);
        x[(SR * 2.0) as usize..].fill(0.0);
        let (l, rr) = run(&mut r, &x, &x);
        assert!(l.iter().chain(&rr).all(|v| v.is_finite()));
        let peak = l.iter().chain(&rr).fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 8.0, "peak {peak}");
        let tail = &l[len - 4800..];
        assert!(tail.iter().any(|v| v.abs() > 1e-3), "a 20 s decay is still ringing");
    }

    #[test]
    fn tail_fades_to_clean_zeros_without_subnormals() {
        let mut r = plain(0.2, 0.0);
        let len = (SR * 30.0) as usize;
        let (l, rr) = run(&mut r, &impulse(len), &impulse(len));
        let subnormal = |v: &f32| *v != 0.0 && v.abs() < f32::MIN_POSITIVE;
        assert!(!l.iter().chain(&rr).any(subnormal));
        assert!(!r.lines.iter().flat_map(|l| &l.buf).any(subnormal), "delay lines");
        assert!(!r.damp_state.iter().any(subnormal));
        assert!(l[len - 100..].iter().all(|v| v.abs() < 1e-12), "silent after 30 s");
    }

    fn correlation(a: &[f32], b: &[f32]) -> f64 {
        let dot = |x: &[f32], y: &[f32]| x.iter().zip(y).map(|(p, q)| *p as f64 * *q as f64).sum::<f64>();
        dot(a, b) / (dot(a, a) * dot(b, b)).sqrt()
    }

    #[test]
    fn width_controls_stereo_decorrelation() {
        let x = impulse(48_000);
        let mut wide = plain(1.5, 0.5);
        let (l, r) = run(&mut wide, &x, &x);
        let c = correlation(&l[2400..], &r[2400..]);
        assert!(c.abs() < 0.3, "wide tail is decorrelated: {c}");
        let mut mono = plain(1.5, 0.5);
        mono.set_param(WIDTH, 0.0);
        let (l, r) = run(&mut mono, &x, &x);
        assert!(correlation(&l[2400..], &r[2400..]) > 0.999, "width 0 is mono");
    }

    #[test]
    fn mix_zero_passes_the_dry_signal() {
        let mut r = Reverb::new(SR, None);
        r.set_param(MIX, 0.0);
        let (x_l, x_r) = (noise(24_000, 1), noise(24_000, 2));
        let (l, rr) = run(&mut r, &x_l, &x_r);
        assert_eq!(l, x_l);
        assert_eq!(rr, x_r);
    }

    #[test]
    fn moving_knobs_mid_stream_does_not_click() {
        // A steady low tone through the reverb; the largest sample-to-sample step after big
        // parameter jumps must stay close to the steady-state one.
        let sine: Vec<f32> = (0..(SR * 4.0) as usize).map(|i| 0.5 * (std::f64::consts::TAU * 150.0 * i as f64 / SR).sin() as f32).collect();
        let mut r = Reverb::new(SR, None);
        r.set_param(DAMPING, norm_damping(3000.0));
        let half = sine.len() / 2;
        let (a, _) = run(&mut r, &sine[..half], &sine[..half]);
        for (id, v) in [(SIZE, 1.0), (DECAY, norm_decay(0.3)), (PRE_DELAY, 1.0), (MIX, 0.3), (WIDTH, 0.0), (LOW_CUT, 1.0), (LEVEL, norm_level(-6.0)), (DIFFUSION, 0.0)] {
            r.set_param(id, v);
        }
        let (b, _) = run(&mut r, &sine[half..], &sine[half..]);
        let step = |x: &[f32]| x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        let (before, after) = (step(&a[half / 2..]), step(&b));
        assert!(after < before * 2.0, "steady {before}, after the change {after}");
    }
}
