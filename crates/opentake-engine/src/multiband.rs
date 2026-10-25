//! Built-in three-band compressor. Linkwitz–Riley (24 dB/oct) crossovers split the signal so
//! the bands add back up flat: the low band also goes through the high crossover's all-pass,
//! matching the phase the mid and high bands pick up there. Each band has its own
//! stereo-linked compressor (with the single-band compressor's curve), bypass and solo.

use std::sync::Arc;

use crate::dynamics::{self, CompSettings, GainMeter};
use crate::mixer::{MidiEvent, Processor, Transport};

pub const NAME: &str = "OpenTake Multiband Compressor";
pub const PATH: &str = "multiband";
pub const BANDS: usize = 3;
pub const BAND_NAMES: [&str; BANDS] = ["Low", "Mid", "High"];

/// Per-band parameter offsets; band `b`'s ids are `b * BAND_STRIDE + offset`.
pub const THRESHOLD: u32 = 0;
pub const RATIO: u32 = 1;
pub const ATTACK: u32 = 2;
pub const RELEASE: u32 = 3;
pub const GAIN: u32 = 4;
pub const BYPASS: u32 = 5;
pub const SOLO: u32 = 6;
pub const BAND_STRIDE: u32 = 10;
/// Global parameters.
pub const LOW_CROSSOVER: u32 = 30;
pub const HIGH_CROSSOVER: u32 = 31;
pub const OUTPUT: u32 = 32;

pub fn band_param(band: usize, offset: u32) -> u32 {
    band as u32 * BAND_STRIDE + offset
}

/// Crossover ranges (Hz): low/mid between 30 Hz and 1 kHz, mid/high between 1 and 16 kHz.
pub const LOW_RANGE: (f64, f64) = (30.0, 1000.0);
pub const HIGH_RANGE: (f64, f64) = (1000.0, 16000.0);

pub fn crossover_of(id: u32, v: f64) -> f64 {
    let (lo, hi) = if id == LOW_CROSSOVER { LOW_RANGE } else { HIGH_RANGE };
    lo * (hi / lo).powf(v.clamp(0.0, 1.0))
}
pub fn norm_crossover(id: u32, hz: f64) -> f64 {
    let (lo, hi) = if id == LOW_CROSSOVER { LOW_RANGE } else { HIGH_RANGE };
    ((hz.clamp(lo, hi) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}
/// Band gain and output: -12..+12 dB.
pub fn gain_of(v: f64) -> f64 {
    -12.0 + 24.0 * v.clamp(0.0, 1.0)
}
pub fn norm_gain(db: f64) -> f64 {
    ((db + 12.0) / 24.0).clamp(0.0, 1.0)
}

/// Parameters: (id, title, units, default normalized value, steps).
pub fn params() -> Vec<(u32, String, &'static str, f64, i32)> {
    let mut p = Vec::new();
    let defaults = [(10.0, 160.0), (5.0, 110.0), (3.0, 80.0)];
    for (b, name) in BAND_NAMES.iter().enumerate() {
        let (attack, release) = defaults[b];
        p.push((band_param(b, THRESHOLD), format!("{name} Threshold"), "dB", dynamics::norm_threshold(-20.0), 0));
        p.push((band_param(b, RATIO), format!("{name} Ratio"), ":1", dynamics::norm_ratio(3.0), 0));
        p.push((band_param(b, ATTACK), format!("{name} Attack"), "ms", dynamics::norm_attack(attack), 0));
        p.push((band_param(b, RELEASE), format!("{name} Release"), "ms", dynamics::norm_release(release), 0));
        p.push((band_param(b, GAIN), format!("{name} Gain"), "dB", norm_gain(0.0), 0));
        p.push((band_param(b, BYPASS), format!("{name} Bypass"), "", 0.0, 1));
        p.push((band_param(b, SOLO), format!("{name} Solo"), "", 0.0, 1));
    }
    p.push((LOW_CROSSOVER, "Low Crossover".into(), "Hz", norm_crossover(LOW_CROSSOVER, 200.0), 0));
    p.push((HIGH_CROSSOVER, "High Crossover".into(), "Hz", norm_crossover(HIGH_CROSSOVER, 2500.0), 0));
    p.push((OUTPUT, "Output".into(), "dB", norm_gain(0.0), 0));
    p
}

pub fn display(id: u32, v: f64) -> String {
    match id {
        LOW_CROSSOVER | HIGH_CROSSOVER => {
            let hz = crossover_of(id, v);
            if hz >= 1000.0 { format!("{:.2}k", hz / 1000.0) } else { format!("{hz:.0}") }
        }
        OUTPUT => format!("{:+.1}", gain_of(v)),
        _ => match id % BAND_STRIDE {
            THRESHOLD => format!("{:.1}", dynamics::threshold_of(v)),
            RATIO => format!("{:.1}", dynamics::ratio_of(v)),
            ATTACK => format!("{:.1}", dynamics::attack_of(v)),
            RELEASE => format!("{:.0}", dynamics::release_of(v)),
            GAIN => format!("{:+.1}", gain_of(v)),
            BYPASS | SOLO => (if v >= 0.5 { "On" } else { "Off" }).into(),
            _ => format!("{v:.2}"),
        },
    }
}

/// Meters for the editor: one per band (gain reduction, band input, band output) and one for
/// the whole (input, output).
pub struct MultibandMeter {
    pub bands: [Arc<GainMeter>; BANDS],
    pub total: Arc<GainMeter>,
}

impl MultibandMeter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { bands: std::array::from_fn(|_| GainMeter::new()), total: GainMeter::new() })
    }
}

/// One band's settings in real units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub comp: CompSettings,
    pub gain_db: f64,
    pub bypass: bool,
    pub solo: bool,
}

/// All settings in real units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub bands: [Band; BANDS],
    pub low_hz: f64,
    pub high_hz: f64,
    pub output_db: f64,
}

impl Settings {
    pub fn from_params(values: &[(u32, f64)]) -> Self {
        let defaults = params();
        let v = |id: u32| values.iter().rev().find(|p| p.0 == id).map(|p| p.1).or_else(|| defaults.iter().find(|p| p.0 == id).map(|p| p.3)).unwrap_or(0.0);
        let band = |b: usize| Band {
            comp: CompSettings {
                threshold_db: dynamics::threshold_of(v(band_param(b, THRESHOLD))),
                ratio: dynamics::ratio_of(v(band_param(b, RATIO))),
                attack_ms: dynamics::attack_of(v(band_param(b, ATTACK))),
                release_ms: dynamics::release_of(v(band_param(b, RELEASE))),
                knee_db: 6.0,
                makeup_db: 0.0,
                mix: 1.0,
            },
            gain_db: gain_of(v(band_param(b, GAIN))),
            bypass: v(band_param(b, BYPASS)) >= 0.5,
            solo: v(band_param(b, SOLO)) >= 0.5,
        };
        Self {
            bands: std::array::from_fn(band),
            low_hz: crossover_of(LOW_CROSSOVER, v(LOW_CROSSOVER)),
            high_hz: crossover_of(HIGH_CROSSOVER, v(HIGH_CROSSOVER)),
            output_db: gain_of(v(OUTPUT)),
        }
    }
}

/// Second-order section (transposed direct form II).
#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    /// Butterworth (Q = 1/√2) low- or high-pass; two in series make a Linkwitz–Riley filter.
    fn butterworth(&mut self, hz: f64, sample_rate: f64, high: bool) {
        let w = std::f64::consts::TAU * (hz / sample_rate).min(0.49);
        let (sin, cos) = w.sin_cos();
        let alpha = sin / std::f64::consts::SQRT_2;
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high { ((1.0 + cos) / 2.0, -(1.0 + cos)) } else { ((1.0 - cos) / 2.0, 1.0 - cos) };
        self.b = [b0 / a0, b1 / a0, b0 / a0];
        self.a = [-2.0 * cos / a0, (1.0 - alpha) / a0];
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// A 4th-order Linkwitz–Riley low- or high-pass.
#[derive(Clone, Copy, Default)]
struct Lr4([Biquad; 2]);

impl Lr4 {
    fn set(&mut self, hz: f64, sample_rate: f64, high: bool) {
        for s in &mut self.0 {
            s.butterworth(hz, sample_rate, high);
        }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.0[0].run(x);
        self.0[1].run(y)
    }
}

/// The crossover network for one channel.
#[derive(Clone, Copy, Default)]
struct Splitter {
    low_lp: Lr4,
    low_hp: Lr4,
    high_lp: Lr4,
    high_hp: Lr4,
    /// The high crossover's all-pass (its low + high outputs) for the low band.
    ap_lp: Lr4,
    ap_hp: Lr4,
}

impl Splitter {
    fn set(&mut self, low_hz: f64, high_hz: f64, sr: f64) {
        self.low_lp.set(low_hz, sr, false);
        self.low_hp.set(low_hz, sr, true);
        for (f, high) in [(&mut self.high_lp, false), (&mut self.high_hp, true), (&mut self.ap_lp, false), (&mut self.ap_hp, true)] {
            f.set(high_hz, sr, high);
        }
    }

    fn split(&mut self, x: f64) -> [f64; BANDS] {
        let low = self.low_lp.run(x);
        let rest = self.low_hp.run(x);
        let low = self.ap_lp.run(low) + self.ap_hp.run(low);
        [low, self.high_lp.run(rest), self.high_hp.run(rest)]
    }
}

pub struct Multiband {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    settings: Settings,
    split: [Splitter; 2],
    /// Per band: attack and release coefficients, smoothed gain reduction (dB).
    coefs: [(f64, f64); BANDS],
    reduction: [f64; BANDS],
    dirty: bool,
    meter: Option<Arc<MultibandMeter>>,
}

impl Multiband {
    pub fn new(sample_rate: f64, meter: Option<Arc<MultibandMeter>>) -> Self {
        let values: Vec<(u32, f64)> = params().into_iter().map(|p| (p.0, p.3)).collect();
        let settings = Settings::from_params(&values);
        let mut m = Self {
            sample_rate: sample_rate.max(1.0),
            values,
            settings,
            split: [Splitter::default(); 2],
            coefs: [(0.0, 0.0); BANDS],
            reduction: [0.0; BANDS],
            dirty: true,
            meter,
        };
        m.update();
        m
    }

    fn update(&mut self) {
        self.settings = Settings::from_params(&self.values);
        let s = self.settings;
        for split in &mut self.split {
            split.set(s.low_hz, s.high_hz, self.sample_rate);
        }
        let coef = |ms: f64| (-1.0 / (ms.max(0.01) * 0.001 * self.sample_rate)).exp();
        self.coefs = s.bands.map(|b| (coef(b.comp.attack_ms), coef(b.comp.release_ms)));
        self.dirty = false;
    }
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-6).log10()
}

impl Processor for Multiband {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        let s = self.settings;
        let any_solo = s.bands.iter().any(|b| b.solo);
        let gains = s.bands.map(|b| 10f64.powf(b.gain_db / 20.0));
        let output = 10f64.powf(s.output_db / 20.0);
        let (mut band_in, mut band_out, mut max_gr) = ([0.0f64; BANDS], [0.0f64; BANDS], [0.0f64; BANDS]);
        let (mut peak_in, mut peak_out) = (0.0f64, 0.0f64);
        for i in 0..frames {
            let (l, r) = (il[i] as f64, ir[i] as f64);
            let (bl, br) = (self.split[0].split(l), self.split[1].split(r));
            let (mut yl, mut yr) = (0.0, 0.0);
            for b in 0..BANDS {
                let band = s.bands[b];
                let level = bl[b].abs().max(br[b].abs());
                band_in[b] = band_in[b].max(level);
                let mut gain = gains[b];
                if !band.bypass {
                    let level_db = db(level);
                    let target = level_db - band.comp.curve(level_db);
                    let (attack, release) = self.coefs[b];
                    let coef = if target > self.reduction[b] { attack } else { release };
                    self.reduction[b] = coef * self.reduction[b] + (1.0 - coef) * target;
                    max_gr[b] = max_gr[b].max(self.reduction[b]);
                    gain *= 10f64.powf(-self.reduction[b] / 20.0);
                }
                band_out[b] = band_out[b].max(level * gain);
                if !any_solo || band.solo {
                    yl += bl[b] * gain;
                    yr += br[b] * gain;
                }
            }
            ol[i] = (yl * output) as f32;
            or[i] = (yr * output) as f32;
            peak_in = peak_in.max(l.abs()).max(r.abs());
            peak_out = peak_out.max((yl * output).abs()).max((yr * output).abs());
        }
        if let Some(m) = &self.meter {
            for b in 0..BANDS {
                m.bands[b].report(max_gr[b] as f32, db(band_in[b]) as f32, db(band_out[b]) as f32);
            }
            m.total.report(0.0, db(peak_in) as f32, db(peak_out) as f32);
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

    fn run(m: &mut Multiband, x: &[f32]) -> Vec<f32> {
        let mut out = [vec![0.0f32; x.len()], vec![0.0f32; x.len()]];
        for (inp, chunk) in x.chunks(512).zip(0..) {
            let n = inp.len();
            let [l, r] = &mut out;
            let at = chunk * 512;
            let mut o: [&mut [f32]; 2] = [&mut l[at..at + n], &mut r[at..at + n]];
            m.process(&[inp, inp], &mut o, n, &Transport::default(), &[]);
        }
        out[0].clone()
    }

    fn sine(hz: f64, amp: f32, secs: f64) -> Vec<f32> {
        (0..(48_000.0 * secs) as usize).map(|i| amp * (std::f64::consts::TAU * hz * i as f64 / 48_000.0).sin() as f32).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn bands_sum_flat_when_nothing_compresses() {
        // Thresholds at 0 dB: nothing is compressed, so every frequency comes out at its level.
        let mut m = Multiband::new(48_000.0, None);
        for b in 0..BANDS {
            m.set_param(band_param(b, THRESHOLD), 1.0);
        }
        for hz in [60.0, 200.0, 700.0, 2500.0, 8000.0] {
            let x = sine(hz, 0.1, 0.5);
            let y = run(&mut m, &x);
            let ratio = rms(&y[12_000..]) / rms(&x[12_000..]);
            assert!((ratio - 1.0).abs() < 0.02, "{hz} Hz: {ratio}");
        }
    }

    #[test]
    fn only_the_loud_band_is_compressed() {
        let meter = MultibandMeter::new();
        let mut m = Multiband::new(48_000.0, Some(meter.clone()));
        // A loud bass tone and a quiet treble tone.
        let x: Vec<f32> = sine(80.0, 0.8, 0.5).iter().zip(sine(6000.0, 0.01, 0.5)).map(|(a, b)| a + b).collect();
        run(&mut m, &x);
        let gr: Vec<f32> = meter.bands.iter().map(|b| b.take().0).collect();
        assert!(gr[0] > 6.0, "low band reduced: {gr:?}");
        assert!(gr[2] < 0.1, "high band untouched: {gr:?}");
    }

    #[test]
    fn solo_and_bypass() {
        let mut m = Multiband::new(48_000.0, None);
        m.set_param(band_param(2, SOLO), 1.0);
        let low = run(&mut m, &sine(60.0, 0.05, 0.3));
        assert!(rms(&low[6000..]) < 0.002, "soloing the highs mutes a bass tone");
        m.set_param(band_param(2, SOLO), 0.0);
        m.set_param(band_param(0, BYPASS), 1.0);
        m.set_param(band_param(0, GAIN), norm_gain(6.0));
        let x = sine(60.0, 0.5, 0.5);
        let y = run(&mut m, &x);
        let ratio = rms(&y[12_000..]) / rms(&x[12_000..]);
        assert!((ratio - 2.0).abs() < 0.05, "bypassed band keeps its gain only: {ratio}");
    }
}
