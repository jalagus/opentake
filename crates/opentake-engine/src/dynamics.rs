//! Built-in dynamics: a compressor and a look-ahead brickwall limiter. Both feed a
//! [`GainMeter`] the editor reads (lock-free), and the limiter reports its look-ahead as
//! latency so the mixer compensates for it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::filter::Biquad;
use crate::mixer::{MidiEvent, Processor, Transport};

/// Gain reduction and levels for the editor's meters (dB, peak since the last read).
pub struct GainMeter {
    reduction: AtomicU32,
    input: AtomicU32,
    output: AtomicU32,
}

impl GainMeter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { reduction: AtomicU32::new(0), input: AtomicU32::new(f32::to_bits(-120.0)), output: AtomicU32::new(f32::to_bits(-120.0)) })
    }

    fn raise(cell: &AtomicU32, db: f32) {
        let _ = cell.try_update(Ordering::Relaxed, Ordering::Relaxed, |old| (db > f32::from_bits(old)).then_some(db.to_bits()));
    }

    pub(crate) fn report(&self, reduction_db: f32, input_db: f32, output_db: f32) {
        Self::raise(&self.reduction, reduction_db);
        Self::raise(&self.input, input_db);
        Self::raise(&self.output, output_db);
    }

    /// (gain reduction, input peak, output peak) in dB since the last call, then reset.
    pub fn take(&self) -> (f32, f32, f32) {
        let floor = f32::to_bits(-120.0);
        (
            f32::from_bits(self.reduction.swap(0, Ordering::Relaxed)),
            f32::from_bits(self.input.swap(floor, Ordering::Relaxed)),
            f32::from_bits(self.output.swap(floor, Ordering::Relaxed)),
        )
    }
}

fn to_db(x: f32) -> f32 {
    20.0 * x.max(1e-6).log10()
}

fn coefficient(ms: f64, sample_rate: f64) -> f64 {
    (-1.0 / (ms.max(0.01) * 0.001 * sample_rate)).exp()
}

fn log_range(v: f64, lo: f64, hi: f64) -> f64 {
    lo * (hi / lo).powf(v.clamp(0.0, 1.0))
}

fn norm_log(x: f64, lo: f64, hi: f64) -> f64 {
    ((x.max(lo) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------- compressor

pub mod comp {
    pub const NAME: &str = "OpenTake Compressor";
    pub const PATH: &str = "comp";
    pub const THRESHOLD: u32 = 0;
    pub const RATIO: u32 = 1;
    pub const ATTACK: u32 = 2;
    pub const RELEASE: u32 = 3;
    pub const KNEE: u32 = 4;
    pub const MAKEUP: u32 = 5;
    pub const MIX: u32 = 6;
    /// High-pass on the detector (own input or side chain), e.g. so a kick key isn't all sub.
    pub const KEY_LOW_CUT: u32 = 7;
}

pub fn threshold_of(v: f64) -> f64 {
    -60.0 + 60.0 * v.clamp(0.0, 1.0)
}
pub fn norm_threshold(db: f64) -> f64 {
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}
pub fn ratio_of(v: f64) -> f64 {
    log_range(v, 1.0, 20.0)
}
pub fn norm_ratio(r: f64) -> f64 {
    norm_log(r, 1.0, 20.0)
}
pub fn attack_of(v: f64) -> f64 {
    log_range(v, 0.1, 100.0)
}
pub fn norm_attack(ms: f64) -> f64 {
    norm_log(ms, 0.1, 100.0)
}
pub fn release_of(v: f64) -> f64 {
    log_range(v, 10.0, 1000.0)
}
pub fn norm_release(ms: f64) -> f64 {
    norm_log(ms, 10.0, 1000.0)
}
pub fn knee_of(v: f64) -> f64 {
    12.0 * v.clamp(0.0, 1.0)
}
pub fn makeup_of(v: f64) -> f64 {
    24.0 * v.clamp(0.0, 1.0)
}
/// Key low cut in Hz, `None` at the bottom of the range (off).
pub fn key_low_cut_of(v: f64) -> Option<f64> {
    (v > 0.0).then(|| log_range(v, 20.0, 1000.0))
}
pub fn norm_key_low_cut(hz: Option<f64>) -> f64 {
    hz.map_or(0.0, |hz| norm_log(hz, 20.0, 1000.0).max(1e-3))
}

/// Compressor parameters: (id, title, units, default normalized value, steps).
pub fn comp_params() -> Vec<(u32, String, &'static str, f64, i32)> {
    vec![
        (comp::THRESHOLD, "Threshold".into(), "dB", norm_threshold(-18.0), 0),
        (comp::RATIO, "Ratio".into(), ":1", norm_ratio(4.0), 0),
        (comp::ATTACK, "Attack".into(), "ms", norm_attack(10.0), 0),
        (comp::RELEASE, "Release".into(), "ms", norm_release(120.0), 0),
        (comp::KNEE, "Knee".into(), "dB", 0.5, 0),
        (comp::MAKEUP, "Makeup".into(), "dB", 0.0, 0),
        (comp::MIX, "Mix".into(), "%", 1.0, 0),
        (comp::KEY_LOW_CUT, "Key Low Cut".into(), "", 0.0, 0),
    ]
}

pub fn comp_display(id: u32, v: f64) -> String {
    match id {
        comp::THRESHOLD => format!("{:.1}", threshold_of(v)),
        comp::RATIO => format!("{:.1}", ratio_of(v)),
        comp::ATTACK => format!("{:.1}", attack_of(v)),
        comp::RELEASE => format!("{:.0}", release_of(v)),
        comp::KNEE => format!("{:.1}", knee_of(v)),
        comp::MAKEUP => format!("{:+.1}", makeup_of(v)),
        comp::MIX => format!("{:.0}", v * 100.0),
        comp::KEY_LOW_CUT => match key_low_cut_of(v) {
            None => "Off".into(),
            Some(hz) => format!("{hz:.0} Hz"),
        },
        _ => format!("{v:.2}"),
    }
}

/// The compressor's settings in real units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompSettings {
    pub threshold_db: f64,
    pub ratio: f64,
    pub attack_ms: f64,
    pub release_ms: f64,
    pub knee_db: f64,
    pub makeup_db: f64,
    pub mix: f64,
}

impl CompSettings {
    pub fn from_params(values: &[(u32, f64)]) -> Self {
        let defaults = comp_params();
        let v = |id: u32| values.iter().rev().find(|p| p.0 == id).map(|p| p.1).or_else(|| defaults.iter().find(|p| p.0 == id).map(|p| p.3)).unwrap_or(0.0);
        Self {
            threshold_db: threshold_of(v(comp::THRESHOLD)),
            ratio: ratio_of(v(comp::RATIO)),
            attack_ms: attack_of(v(comp::ATTACK)),
            release_ms: release_of(v(comp::RELEASE)),
            knee_db: knee_of(v(comp::KNEE)),
            makeup_db: makeup_of(v(comp::MAKEUP)),
            mix: v(comp::MIX),
        }
    }

    /// Static curve: output level (dB, before makeup) for an input level, with a soft knee.
    pub fn curve(&self, x: f64) -> f64 {
        let (t, r, w) = (self.threshold_db, self.ratio, self.knee_db);
        let over = x - t;
        if 2.0 * over < -w {
            x
        } else if w > 0.0 && 2.0 * over.abs() <= w {
            x + (1.0 / r - 1.0) * (over + w / 2.0).powi(2) / (2.0 * w)
        } else {
            t + over / r
        }
    }
}

pub struct Compressor {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    settings: CompSettings,
    attack: f64,
    release: f64,
    /// Smoothed gain reduction (dB, ≥ 0).
    reduction: f64,
    /// Detector high-pass per channel, when Key Low Cut is on.
    key_cut: Option<[Biquad; 2]>,
    dirty: bool,
    meter: Option<Arc<GainMeter>>,
}

impl Compressor {
    pub fn new(sample_rate: f64, meter: Option<Arc<GainMeter>>) -> Self {
        let values: Vec<(u32, f64)> = comp_params().into_iter().map(|p| (p.0, p.3)).collect();
        let settings = CompSettings::from_params(&values);
        let mut c = Self { sample_rate: sample_rate.max(1.0), values, settings, attack: 0.0, release: 0.0, reduction: 0.0, key_cut: None, dirty: true, meter };
        c.update();
        c
    }

    fn update(&mut self) {
        self.settings = CompSettings::from_params(&self.values);
        self.attack = coefficient(self.settings.attack_ms, self.sample_rate);
        self.release = coefficient(self.settings.release_ms, self.sample_rate);
        let cut = self.values.iter().find(|p| p.0 == comp::KEY_LOW_CUT).and_then(|p| key_low_cut_of(p.1));
        self.key_cut = cut.map(|hz| {
            // Keep the filters' state when only the frequency moves.
            let mut f = self.key_cut.unwrap_or_default();
            f.iter_mut().for_each(|b| b.pass(hz, 0.707, self.sample_rate, true));
            f
        });
        self.dirty = false;
    }
}

impl Processor for Compressor {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, t: &Transport, e: &[MidiEvent]) {
        self.process_keyed(inputs, None, outputs, frames, t, e);
    }

    fn process_keyed(&mut self, inputs: &[&[f32]], key: Option<&[&[f32]]>, outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        // The detector listens to the side chain when there is one, else to the input.
        let (dl, dr) = match key {
            Some([kl, kr, ..]) => (*kl, *kr),
            _ => (*il, *ir),
        };
        let s = self.settings;
        let makeup = 10f64.powf(s.makeup_db / 20.0);
        let (mut peak_in, mut peak_out, mut max_gr) = (0.0f32, 0.0f32, 0.0f64);
        for i in 0..frames {
            let (l, r) = (il[i], ir[i]);
            let (mut kl, mut kr) = (dl[i] as f64, dr[i] as f64);
            if let Some([fl, fr]) = &mut self.key_cut {
                (kl, kr) = (fl.run(kl), fr.run(kr));
            }
            // Stereo-linked peak detector, in dB.
            let level = to_db(kl.abs().max(kr.abs()) as f32) as f64;
            let target = level - s.curve(level);
            let coef = if target > self.reduction { self.attack } else { self.release };
            self.reduction = coef * self.reduction + (1.0 - coef) * target;
            let gain = (10f64.powf(-self.reduction / 20.0) * makeup) as f32;
            let mix = s.mix as f32;
            ol[i] = l * (1.0 - mix) + l * gain * mix;
            or[i] = r * (1.0 - mix) + r * gain * mix;
            peak_in = peak_in.max(l.abs()).max(r.abs());
            peak_out = peak_out.max(ol[i].abs()).max(or[i].abs());
            max_gr = max_gr.max(self.reduction);
        }
        if let Some(m) = &self.meter {
            m.report(max_gr as f32, to_db(peak_in), to_db(peak_out));
        }
    }

    fn set_param(&mut self, id: u32, normalized: f64) {
        if let Some(v) = self.values.iter_mut().find(|p| p.0 == id) {
            v.1 = normalized.clamp(0.0, 1.0);
            self.dirty = true;
        }
    }
}

// ---------------------------------------------------------------- limiter

pub mod limiter {
    pub const NAME: &str = "OpenTake Limiter";
    pub const PATH: &str = "limiter";
    pub const GAIN: u32 = 0;
    pub const CEILING: u32 = 1;
    pub const RELEASE: u32 = 2;
    /// Look-ahead (and so latency) in seconds.
    pub const LOOKAHEAD_SECS: f64 = 0.0015;
}

pub fn input_gain_of(v: f64) -> f64 {
    24.0 * v.clamp(0.0, 1.0)
}
pub fn norm_input_gain(db: f64) -> f64 {
    (db / 24.0).clamp(0.0, 1.0)
}
pub fn ceiling_of(v: f64) -> f64 {
    -12.0 + 12.0 * v.clamp(0.0, 1.0)
}
pub fn norm_ceiling(db: f64) -> f64 {
    ((db + 12.0) / 12.0).clamp(0.0, 1.0)
}
pub fn limiter_release_of(v: f64) -> f64 {
    log_range(v, 1.0, 1000.0)
}
pub fn norm_limiter_release(ms: f64) -> f64 {
    norm_log(ms, 1.0, 1000.0)
}

pub fn limiter_params() -> Vec<(u32, String, &'static str, f64, i32)> {
    vec![
        (limiter::GAIN, "Gain".into(), "dB", 0.0, 0),
        (limiter::CEILING, "Ceiling".into(), "dB", norm_ceiling(-1.0), 0),
        (limiter::RELEASE, "Release".into(), "ms", norm_limiter_release(60.0), 0),
    ]
}

pub fn limiter_display(id: u32, v: f64) -> String {
    match id {
        limiter::GAIN => format!("{:+.1}", input_gain_of(v)),
        limiter::CEILING => format!("{:.1}", ceiling_of(v)),
        limiter::RELEASE => format!("{:.0}", limiter_release_of(v)),
        _ => format!("{v:.2}"),
    }
}

/// Brickwall limiter with look-ahead: the gain needed for each sample is held over the
/// look-ahead window (a running minimum) and averaged over it, so the gain has fully come
/// down by the time the peak leaves the delay line; release smooths the way back up.
pub struct Limiter {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    gain: f32,
    ceiling: f32,
    release: f64,
    lookahead: usize,
    /// Audio delay lines (lookahead samples).
    delay: [Vec<f32>; 2],
    /// Required gain per sample over the last lookahead + 1 samples (for the running minimum)
    /// and the running minimum over the last lookahead samples (for the average).
    required: Vec<f32>,
    minima: Vec<f32>,
    minima_sum: f64,
    pos: usize,
    smoothed: f64,
    dirty: bool,
    meter: Option<Arc<GainMeter>>,
}

impl Limiter {
    pub fn new(sample_rate: f64, meter: Option<Arc<GainMeter>>) -> Self {
        let lookahead = ((limiter::LOOKAHEAD_SECS * sample_rate).round() as usize).max(1);
        let values: Vec<(u32, f64)> = limiter_params().into_iter().map(|p| (p.0, p.3)).collect();
        let mut l = Self {
            sample_rate: sample_rate.max(1.0),
            values,
            gain: 1.0,
            ceiling: 1.0,
            release: 0.0,
            lookahead,
            delay: [vec![0.0; lookahead], vec![0.0; lookahead]],
            required: vec![1.0; lookahead + 1],
            minima: vec![1.0; lookahead],
            minima_sum: lookahead as f64,
            pos: 0,
            smoothed: 1.0,
            dirty: true,
            meter,
        };
        l.update();
        l
    }

    fn update(&mut self) {
        let v = |id: u32| self.values.iter().find(|p| p.0 == id).map_or(0.0, |p| p.1);
        self.gain = 10f64.powf(input_gain_of(v(limiter::GAIN)) / 20.0) as f32;
        self.ceiling = 10f64.powf(ceiling_of(v(limiter::CEILING)) / 20.0) as f32;
        self.release = coefficient(limiter_release_of(v(limiter::RELEASE)), self.sample_rate);
        self.dirty = false;
    }
}

impl Processor for Limiter {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, t: &Transport, e: &[MidiEvent]) {
        self.process_keyed(inputs, None, outputs, frames, t, e);
    }

    /// Keyed, the gain follows the key's peaks (times Gain) instead of the input's; the
    /// ceiling clamp still guards the output.
    fn process_keyed(&mut self, inputs: &[&[f32]], key: Option<&[&[f32]]>, outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        let (dl, dr) = match key {
            Some([kl, kr, ..]) => (*kl, *kr),
            _ => (*il, *ir),
        };
        let la = self.lookahead;
        let (mut peak_in, mut peak_out, mut min_gain) = (0.0f32, 0.0f32, 1.0f64);
        for i in 0..frames {
            let (l, r) = (il[i] * self.gain, ir[i] * self.gain);
            peak_in = peak_in.max(l.abs()).max(r.abs());
            let peak = (dl[i] * self.gain).abs().max((dr[i] * self.gain).abs());
            let need = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
            // Running minimum of the required gain over the window (lookahead + 1 samples).
            let slot = self.pos % (la + 1);
            self.required[slot] = need;
            let window_min = self.required.iter().fold(1.0f32, |m, &g| m.min(g));
            // Average of that minimum over the look-ahead: a ramp that's fully down when the
            // peak comes out of the delay line.
            let m_slot = self.pos % la;
            self.minima_sum += window_min as f64 - self.minima[m_slot] as f64;
            self.minima[m_slot] = window_min;
            let target = (self.minima_sum / la as f64).min(1.0);
            self.smoothed = if target < self.smoothed { target } else { self.release * self.smoothed + (1.0 - self.release) * target };
            // Delay the audio by the look-ahead.
            let d = self.pos % la;
            let (dl, dr) = (self.delay[0][d], self.delay[1][d]);
            self.delay[0][d] = l;
            self.delay[1][d] = r;
            let g = self.smoothed as f32;
            // A last safety clamp (the ramp should already have done it).
            ol[i] = (dl * g).clamp(-self.ceiling, self.ceiling);
            or[i] = (dr * g).clamp(-self.ceiling, self.ceiling);
            peak_out = peak_out.max(ol[i].abs()).max(or[i].abs());
            min_gain = min_gain.min(self.smoothed);
            self.pos = self.pos.wrapping_add(1);
        }
        // Keep the running sum from drifting.
        if self.pos % (la * 64) < frames {
            self.minima_sum = self.minima.iter().map(|&m| m as f64).sum();
        }
        if let Some(m) = &self.meter {
            m.report(-to_db(min_gain as f32), to_db(peak_in), to_db(peak_out));
        }
    }

    fn set_param(&mut self, id: u32, normalized: f64) {
        if let Some(v) = self.values.iter_mut().find(|p| p.0 == id) {
            v.1 = normalized.clamp(0.0, 1.0);
            self.dirty = true;
        }
    }

    fn latency(&self) -> usize {
        self.lookahead
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: &mut dyn Processor, x: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0f32; x.len()];
        let mut r = vec![0.0f32; x.len()];
        for start in (0..x.len()).step_by(480) {
            let end = (start + 480).min(x.len());
            let n = end - start;
            let (a, b) = (out[start..end].as_mut_ptr(), r[start..end].as_mut_ptr());
            let (ol, or) = unsafe { (std::slice::from_raw_parts_mut(a, n), std::slice::from_raw_parts_mut(b, n)) };
            p.process(&[&x[start..end], &x[start..end]], &mut [ol, or], n, &Transport::default(), &[]);
        }
        out
    }

    fn sine(amp: f32, secs: f64) -> Vec<f32> {
        (0..(secs * 48_000.0) as usize).map(|i| amp * (std::f64::consts::TAU * 220.0 * i as f64 / 48_000.0).sin() as f32).collect()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn compressor_follows_its_static_curve() {
        let s = CompSettings { threshold_db: -20.0, ratio: 4.0, attack_ms: 1.0, release_ms: 50.0, knee_db: 0.0, makeup_db: 0.0, mix: 1.0 };
        assert_eq!(s.curve(-30.0), -30.0);
        assert!((s.curve(-4.0) - -16.0).abs() < 1e-9, "16 dB over → 4 dB over");
        let mut c = Compressor::new(48_000.0, None);
        c.set_param(comp::THRESHOLD, norm_threshold(-20.0));
        c.set_param(comp::RATIO, norm_ratio(4.0));
        c.set_param(comp::KNEE, 0.0);
        c.set_param(comp::ATTACK, norm_attack(1.0));
        // A -4 dBFS sine settles near -16 dBFS (the detector rides the waveform, so allow slack).
        let out = run(&mut c, &sine(10f32.powf(-4.0 / 20.0), 1.0));
        let settled = to_db(peak(&out[40_000..]));
        assert!((settled - -16.0).abs() < 2.5, "{settled} dB");
        // Below the threshold it's untouched.
        let quiet = run(&mut Compressor::new(48_000.0, None), &sine(0.01, 0.5));
        assert!((peak(&quiet[20_000..]) - 0.01).abs() < 1e-3);
    }

    #[test]
    fn a_side_chain_drives_the_detector_through_the_key_low_cut() {
        // A quiet input keyed by a loud 40 Hz sub: ducked, unless Key Low Cut removes the sub.
        let reduction = |cut: Option<f64>| {
            let meter = GainMeter::new();
            let mut c = Compressor::new(48_000.0, Some(meter.clone()));
            c.set_param(comp::THRESHOLD, norm_threshold(-30.0));
            c.set_param(comp::KEY_LOW_CUT, norm_key_low_cut(cut));
            let x = vec![0.01f32; 480];
            let key: Vec<f32> = (0..48_000).map(|i| 0.8 * (std::f64::consts::TAU * 40.0 * i as f64 / 48_000.0).sin() as f32).collect();
            let (mut l, mut r) = (vec![0.0f32; 480], vec![0.0f32; 480]);
            for k in key.chunks(480) {
                c.process_keyed(&[&x, &x], Some(&[k, k]), &mut [&mut l, &mut r], 480, &Transport::default(), &[]);
            }
            meter.take();
            c.process_keyed(&[&x, &x], Some(&[&key[..480], &key[..480]]), &mut [&mut l, &mut r], 480, &Transport::default(), &[]);
            meter.take().0
        };
        assert!(reduction(None) > 10.0, "keyed by the sub: {}", reduction(None));
        assert!(reduction(Some(800.0)) < 2.0, "the sub filtered out of the key: {}", reduction(Some(800.0)));
        assert_eq!(comp_display(comp::KEY_LOW_CUT, 0.0), "Off");
    }

    #[test]
    fn limiter_never_exceeds_the_ceiling_and_reports_its_lookahead() {
        let meter = GainMeter::new();
        let mut l = Limiter::new(48_000.0, Some(meter.clone()));
        l.set_param(limiter::GAIN, norm_input_gain(12.0));
        l.set_param(limiter::CEILING, norm_ceiling(-1.0));
        assert_eq!(l.latency(), 72);
        // Drums-like bursts with sudden peaks.
        let mut x = sine(0.5, 1.0);
        for k in (0..x.len()).step_by(4800) {
            x[k] = 1.0;
            x[k + 1] = -0.95;
        }
        let out = run(&mut l, &x);
        let ceiling = 10f32.powf(-1.0 / 20.0);
        assert!(peak(&out) <= ceiling + 1e-6, "{} > {ceiling}", peak(&out));
        let (gr, _, _) = meter.take();
        assert!(gr > 6.0, "gain reduction {gr} dB");
        // Quiet audio passes unchanged, just delayed by the look-ahead.
        let mut l = Limiter::new(48_000.0, None);
        let q = sine(0.1, 0.1);
        let out = run(&mut l, &q);
        for i in 0..q.len() - 72 {
            assert!((out[i + 72] - q[i]).abs() < 1e-6);
        }
    }
}
