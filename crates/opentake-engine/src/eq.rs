//! Built-in parametric EQ: low cut, low shelf, two bells, high shelf and high cut
//! (RBJ biquads), plus output gain. The response maths is public so the editor can draw
//! the curve from exactly what the audio thread runs. A [`Tap`] lets the editor show the
//! audio going in and coming out (spectrum analyzer) without touching the audio thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::mixer::{MidiEvent, Processor, Transport};

pub const NAME: &str = "OpenTake EQ";
pub const PATH: &str = "eq";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandKind {
    LowCut,
    LowShelf,
    Bell,
    HighShelf,
    HighCut,
}

impl BandKind {
    pub fn has_gain(self) -> bool {
        matches!(self, BandKind::LowShelf | BandKind::Bell | BandKind::HighShelf)
    }

    /// Every band has a Q: the width of a bell, the knee of a shelf, the resonance of a cut.
    pub fn has_q(self) -> bool {
        true
    }

    /// Cuts have a selectable slope (12, 24 or 48 dB/oct).
    pub fn has_slope(self) -> bool {
        matches!(self, BandKind::LowCut | BandKind::HighCut)
    }

    fn default_q(self) -> f64 {
        if self == BandKind::Bell { 1.0 } else { std::f64::consts::FRAC_1_SQRT_2 }
    }
}

/// Most biquad sections a band can use (48 dB/oct cuts).
const MAX_SECTIONS: usize = 4;

/// Cut slope for a normalized value: 12, 24 or 48 dB/oct, as a number of biquad sections.
pub fn sections_of(v: f64) -> usize {
    match (v.clamp(0.0, 1.0) * 2.0).round() as u32 {
        0 => 1,
        1 => 2,
        _ => 4,
    }
}

/// (name, kind, default frequency Hz, default enabled)
pub const BANDS: [(&str, BandKind, f64, bool); 6] = [
    ("Low Cut", BandKind::LowCut, 80.0, false),
    ("Low Shelf", BandKind::LowShelf, 100.0, true),
    ("Low Mid", BandKind::Bell, 400.0, true),
    ("High Mid", BandKind::Bell, 2500.0, true),
    ("High Shelf", BandKind::HighShelf, 8000.0, true),
    ("High Cut", BandKind::HighCut, 18000.0, false),
];

/// Parameter fields of a band.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    On = 0,
    Freq = 1,
    Gain = 2,
    Q = 3,
    Slope = 4,
}

pub const OUTPUT: u32 = 100;

pub fn param_id(band: usize, field: Field) -> u32 {
    band as u32 * 8 + field as u32
}

/// (band, field) for a parameter id.
pub fn field_of(id: u32) -> Option<(usize, Field)> {
    let band = (id / 8) as usize;
    let field = match id % 8 {
        0 => Field::On,
        1 => Field::Freq,
        2 => Field::Gain,
        3 => Field::Q,
        4 => Field::Slope,
        _ => return None,
    };
    (band < BANDS.len()).then_some((band, field))
}

// Normalized (0..1) <-> real value mappings.
pub fn freq_of(v: f64) -> f64 {
    20.0 * 1000f64.powf(v.clamp(0.0, 1.0))
}
pub fn norm_freq(hz: f64) -> f64 {
    ((hz.max(20.0) / 20.0).ln() / 1000f64.ln()).clamp(0.0, 1.0)
}
pub fn gain_of(v: f64) -> f64 {
    -24.0 + 48.0 * v.clamp(0.0, 1.0)
}
pub fn norm_gain(db: f64) -> f64 {
    ((db + 24.0) / 48.0).clamp(0.0, 1.0)
}
pub fn q_of(v: f64) -> f64 {
    0.1 * 100f64.powf(v.clamp(0.0, 1.0))
}
pub fn norm_q(q: f64) -> f64 {
    ((q.max(0.1) / 0.1).ln() / 100f64.ln()).clamp(0.0, 1.0)
}

/// All parameters: (id, title, units, default normalized value, steps).
pub fn params() -> Vec<(u32, String, &'static str, f64, i32)> {
    let mut out = Vec::new();
    for (b, &(name, kind, freq, on)) in BANDS.iter().enumerate() {
        out.push((param_id(b, Field::On), format!("{name} On"), "", if on { 1.0 } else { 0.0 }, 1));
        out.push((param_id(b, Field::Freq), format!("{name} Freq"), "Hz", norm_freq(freq), 0));
        if kind.has_gain() {
            out.push((param_id(b, Field::Gain), format!("{name} Gain"), "dB", norm_gain(0.0), 0));
        }
        if kind.has_q() {
            out.push((param_id(b, Field::Q), format!("{name} Q"), "", norm_q(kind.default_q()), 0));
        }
        if kind.has_slope() {
            out.push((param_id(b, Field::Slope), format!("{name} Slope"), "dB/oct", 0.0, 2));
        }
    }
    out.push((OUTPUT, "Output".into(), "dB", norm_gain(0.0), 0));
    out
}

pub fn display(id: u32, v: f64) -> String {
    if id == OUTPUT {
        return format!("{:+.1}", gain_of(v));
    }
    match field_of(id) {
        Some((_, Field::On)) => if v >= 0.5 { "On" } else { "Off" }.into(),
        Some((_, Field::Freq)) => {
            let f = freq_of(v);
            if f >= 1000.0 { format!("{:.2}k", f / 1000.0) } else { format!("{f:.0}") }
        }
        Some((_, Field::Gain)) => format!("{:+.1}", gain_of(v)),
        Some((_, Field::Q)) => format!("{:.2}", q_of(v)),
        Some((_, Field::Slope)) => format!("{}", 12 * sections_of(v)),
        None => format!("{v:.2}"),
    }
}

/// One band's settings in real units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    pub kind: BandKind,
    pub on: bool,
    pub freq: f64,
    pub gain_db: f64,
    pub q: f64,
    /// Cascaded biquad sections (cut slope); 1 for the other bands.
    pub sections: usize,
}

/// The EQ's settings in real units.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub bands: [Band; 6],
    pub output_db: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self::from_params(&[])
    }
}

impl Settings {
    /// From normalized parameter values (missing ones take their defaults).
    pub fn from_params(values: &[(u32, f64)]) -> Self {
        let get = |id: u32| values.iter().rev().find(|(i, _)| *i == id).map(|p| p.1);
        let defaults = params();
        let value = |id: u32| get(id).or_else(|| defaults.iter().find(|p| p.0 == id).map(|p| p.3)).unwrap_or(0.5);
        let bands = std::array::from_fn(|b| Band {
            kind: BANDS[b].1,
            on: value(param_id(b, Field::On)) >= 0.5,
            freq: freq_of(value(param_id(b, Field::Freq))),
            gain_db: if BANDS[b].1.has_gain() { gain_of(value(param_id(b, Field::Gain))) } else { 0.0 },
            q: q_of(value(param_id(b, Field::Q))),
            sections: if BANDS[b].1.has_slope() { sections_of(value(param_id(b, Field::Slope))) } else { 1 },
        });
        Self { bands, output_db: gain_of(value(OUTPUT)) }
    }

    /// Total response in dB at `freq` Hz.
    pub fn response_db(&self, freq: f64, sample_rate: f64) -> f64 {
        let w = std::f64::consts::TAU * freq / sample_rate;
        let mut db = self.output_db;
        for band in self.bands.iter().filter(|b| b.on) {
            db += coeffs(band, sample_rate).magnitude_db(w) * band.sections as f64;
        }
        db
    }
}

/// Normalized biquad coefficients (a0 = 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coeffs {
    const IDENTITY: Coeffs = Coeffs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    fn magnitude_db(&self, w: f64) -> f64 {
        // |H(e^jw)| from the numerator and denominator polynomials.
        let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
        let num = (self.b0 + self.b1 * c1 + self.b2 * c2).hypot(-(self.b1 * s1 + self.b2 * s2));
        let den = (1.0 + self.a1 * c1 + self.a2 * c2).hypot(-(self.a1 * s1 + self.a2 * s2));
        20.0 * (num / den).max(1e-12).log10()
    }
}

/// RBJ "Audio EQ Cookbook" coefficients.
pub fn coeffs(band: &Band, sample_rate: f64) -> Coeffs {
    let f = band.freq.clamp(10.0, sample_rate * 0.49);
    let w0 = std::f64::consts::TAU * f / sample_rate;
    let (cw, sw) = (w0.cos(), w0.sin());
    let a = 10f64.powf(band.gain_db / 40.0);
    let q = band.q.max(0.05);
    let alpha = sw / (2.0 * q);
    let (b0, b1, b2, a0, a1, a2) = match band.kind {
        BandKind::LowCut => ((1.0 + cw) / 2.0, -(1.0 + cw), (1.0 + cw) / 2.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
        BandKind::HighCut => ((1.0 - cw) / 2.0, 1.0 - cw, (1.0 - cw) / 2.0, 1.0 + alpha, -2.0 * cw, 1.0 - alpha),
        BandKind::Bell => (1.0 + alpha * a, -2.0 * cw, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cw, 1.0 - alpha / a),
        BandKind::LowShelf | BandKind::HighShelf => {
            // Q form of the shelf (Q = 0.707 is the cookbook's slope S = 1); higher Q gives a
            // sharper knee with a little overshoot.
            let k = 2.0 * a.sqrt() * alpha;
            if band.kind == BandKind::LowShelf {
                (
                    a * ((a + 1.0) - (a - 1.0) * cw + k),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                    a * ((a + 1.0) - (a - 1.0) * cw - k),
                    (a + 1.0) + (a - 1.0) * cw + k,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                    (a + 1.0) + (a - 1.0) * cw - k,
                )
            } else {
                (
                    a * ((a + 1.0) + (a - 1.0) * cw + k),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                    a * ((a + 1.0) + (a - 1.0) * cw - k),
                    (a + 1.0) - (a - 1.0) * cw + k,
                    2.0 * ((a - 1.0) - (a + 1.0) * cw),
                    (a + 1.0) - (a - 1.0) * cw - k,
                )
            }
        }
    };
    Coeffs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
}

/// Ring of recent input and output samples (mono), written by the audio thread and read by
/// the editor's analyzer. Lock-free: plain atomic stores and loads.
pub struct Tap {
    pre: Vec<AtomicU32>,
    post: Vec<AtomicU32>,
    write: AtomicUsize,
    sample_rate: AtomicU32,
}

/// Samples kept per side (a power of two).
pub const TAP_LEN: usize = 8192;

impl Tap {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pre: (0..TAP_LEN).map(|_| AtomicU32::new(0)).collect(),
            post: (0..TAP_LEN).map(|_| AtomicU32::new(0)).collect(),
            write: AtomicUsize::new(0),
            sample_rate: AtomicU32::new(48_000),
        })
    }

    fn push(&self, at: usize, pre: f32, post: f32) {
        let i = at & (TAP_LEN - 1);
        self.pre[i].store(pre.to_bits(), Ordering::Relaxed);
        self.post[i].store(post.to_bits(), Ordering::Relaxed);
    }

    /// The latest `n` (≤ TAP_LEN) input and output samples, oldest first.
    pub fn read(&self, n: usize, pre: &mut Vec<f32>, post: &mut Vec<f32>) {
        let n = n.min(TAP_LEN);
        let end = self.write.load(Ordering::Acquire);
        pre.clear();
        post.clear();
        for k in 0..n {
            let i = end.wrapping_sub(n - k) & (TAP_LEN - 1);
            pre.push(f32::from_bits(self.pre[i].load(Ordering::Relaxed)));
            post.push(f32::from_bits(self.post[i].load(Ordering::Relaxed)));
        }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sample_rate.load(Ordering::Relaxed) as f64
    }
}

/// The EQ on the audio thread.
pub struct Eq {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    coeffs: [Coeffs; 6],
    on: [bool; 6],
    sections: [usize; 6],
    output: f32,
    /// Transposed direct form II state per band, section and channel.
    state: [[[[f64; 2]; 2]; MAX_SECTIONS]; 6],
    dirty: bool,
    tap: Option<Arc<Tap>>,
}

impl Eq {
    pub fn new(sample_rate: f64) -> Self {
        Self::with_tap(sample_rate, None)
    }

    /// An EQ that also feeds `tap` with what goes in and comes out.
    pub fn with_tap(sample_rate: f64, tap: Option<Arc<Tap>>) -> Self {
        // Every parameter slot exists up front, so set_param never allocates.
        let values = params().into_iter().map(|p| (p.0, p.3)).collect();
        if let Some(t) = &tap {
            t.sample_rate.store(sample_rate as u32, Ordering::Relaxed);
        }
        let mut eq = Self {
            sample_rate: sample_rate.max(1.0),
            values,
            coeffs: [Coeffs::IDENTITY; 6],
            on: [false; 6],
            sections: [1; 6],
            output: 1.0,
            state: [[[[0.0; 2]; 2]; MAX_SECTIONS]; 6],
            dirty: true,
            tap,
        };
        eq.update();
        eq
    }

    fn update(&mut self) {
        let settings = Settings::from_params(&self.values);
        for (i, band) in settings.bands.iter().enumerate() {
            self.coeffs[i] = coeffs(band, self.sample_rate);
            if band.on != self.on[i] || band.sections != self.sections[i] {
                self.state[i] = [[[0.0; 2]; 2]; MAX_SECTIONS];
            }
            self.on[i] = band.on;
            self.sections[i] = band.sections.min(MAX_SECTIONS);
        }
        self.output = 10f64.powf(settings.output_db / 20.0) as f32;
        self.dirty = false;
    }
}

impl Processor for Eq {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _events: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        for ch in 0..2.min(inputs.len()).min(outputs.len()) {
            let (input, output) = (&inputs[ch][..frames], &mut outputs[ch][..frames]);
            for (o, &x) in output.iter_mut().zip(input) {
                let mut y = x as f64;
                for b in 0..6 {
                    if !self.on[b] {
                        continue;
                    }
                    let c = &self.coeffs[b];
                    for section in &mut self.state[b][..self.sections[b]] {
                        let s = &mut section[ch];
                        let out = c.b0 * y + s[0];
                        s[0] = c.b1 * y - c.a1 * out + s[1];
                        s[1] = c.b2 * y - c.a2 * out;
                        y = out;
                    }
                }
                *o = y as f32 * self.output;
            }
        }
        if let (Some(tap), [il, ir, ..], [ol, or, ..]) = (&self.tap, inputs, &*outputs) {
            let start = tap.write.load(Ordering::Relaxed);
            for i in 0..frames {
                tap.push(start + i, (il[i] + ir[i]) * 0.5, (ol[i] + or[i]) * 0.5);
            }
            tap.write.store(start.wrapping_add(frames), Ordering::Release);
        }
    }

    fn set_param(&mut self, id: u32, normalized: f64) {
        if let Some(v) = self.values.iter_mut().find(|(i, _)| *i == id) {
            v.1 = normalized.clamp(0.0, 1.0);
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_gain(eq: &mut Eq, freq: f64) -> f64 {
        let sr = 48_000.0;
        let n = 9600;
        let x: Vec<f32> = (0..n).map(|i| (std::f64::consts::TAU * freq * i as f64 / sr).sin() as f32).collect();
        let (mut l, mut r) = (vec![0.0; n], vec![0.0; n]);
        for start in (0..n).step_by(480) {
            let end = start + 480;
            let inputs: [&[f32]; 2] = [&x[start..end], &x[start..end]];
            let mut outs: [&mut [f32]; 2] = [&mut l[start..end], &mut r[start..end]];
            eq.process(&inputs, &mut outs, 480, &Transport::default(), &[]);
        }
        let rms = |s: &[f32]| (s.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / s.len() as f64).sqrt();
        20.0 * (rms(&l[n / 2..]) / rms(&x[n / 2..])).log10()
    }

    #[test]
    fn flat_by_default() {
        let mut eq = Eq::new(48_000.0);
        for f in [50.0, 440.0, 5000.0] {
            assert!(sine_gain(&mut eq, f).abs() < 0.1, "{f} Hz");
        }
        assert!(Settings::default().response_db(1000.0, 48_000.0).abs() < 1e-6);
    }

    #[test]
    fn bell_boost_matches_the_drawn_curve() {
        let mut eq = Eq::new(48_000.0);
        eq.set_param(param_id(2, Field::Freq), norm_freq(1000.0));
        eq.set_param(param_id(2, Field::Gain), norm_gain(12.0));
        let measured = sine_gain(&mut eq, 1000.0);
        assert!((measured - 12.0).abs() < 0.3, "{measured}");
        let settings = Settings::from_params(&eq.values);
        let drawn = settings.response_db(1000.0, 48_000.0);
        assert!((drawn - measured).abs() < 0.3, "curve {drawn} vs audio {measured}");
        assert!(sine_gain(&mut eq, 100.0).abs() < 1.0, "far from the bell stays flat");
    }

    #[test]
    fn low_cut_removes_lows_and_shelves_tilt() {
        let mut eq = Eq::new(48_000.0);
        eq.set_param(param_id(0, Field::On), 1.0);
        eq.set_param(param_id(0, Field::Freq), norm_freq(200.0));
        assert!(sine_gain(&mut eq, 50.0) < -20.0);
        assert!(sine_gain(&mut eq, 2000.0).abs() < 0.5);

        let mut eq = Eq::new(48_000.0);
        eq.set_param(param_id(4, Field::Gain), norm_gain(6.0));
        assert!((sine_gain(&mut eq, 16_000.0) - 6.0).abs() < 1.0);
        assert!(sine_gain(&mut eq, 100.0).abs() < 0.5);
    }

    #[test]
    fn steeper_cut_slopes_and_tighter_q() {
        let mut eq = Eq::new(48_000.0);
        eq.set_param(param_id(0, Field::On), 1.0);
        eq.set_param(param_id(0, Field::Freq), norm_freq(400.0));
        let gentle = sine_gain(&mut eq, 100.0);
        eq.set_param(param_id(0, Field::Slope), 1.0); // 48 dB/oct
        let steep = sine_gain(&mut eq, 100.0);
        assert!(steep < gentle - 30.0, "48 dB/oct {steep} vs 12 dB/oct {gentle}");
        let drawn = Settings::from_params(&eq.values).response_db(100.0, 48_000.0);
        assert!((drawn - steep).abs() < 1.0, "curve {drawn} vs audio {steep}");

        // A tighter bell boosts less an octave away.
        let bell = |q: f64| {
            let mut eq = Eq::new(48_000.0);
            eq.set_param(param_id(2, Field::Freq), norm_freq(1000.0));
            eq.set_param(param_id(2, Field::Gain), norm_gain(12.0));
            eq.set_param(param_id(2, Field::Q), norm_q(q));
            sine_gain(&mut eq, 2000.0)
        };
        assert!(bell(4.0) < bell(0.5) - 4.0);
    }

    #[test]
    fn tap_records_input_and_output() {
        let tap = Tap::new();
        let mut eq = Eq::with_tap(48_000.0, Some(tap.clone()));
        eq.set_param(OUTPUT, norm_gain(-6.0));
        let x = vec![0.5f32; 480];
        let (mut l, mut r) = (vec![0.0; 480], vec![0.0; 480]);
        eq.process(&[&x, &x], &mut [&mut l, &mut r], 480, &Transport::default(), &[]);
        let (mut pre, mut post) = (Vec::new(), Vec::new());
        tap.read(100, &mut pre, &mut post);
        assert_eq!(pre.len(), 100);
        assert!(pre.iter().all(|&v| v == 0.5));
        assert!(post.iter().all(|&v| (v - 0.25).abs() < 0.01), "{:?}", &post[..3]);
        assert_eq!(tap.sample_rate(), 48_000.0);
    }

    #[test]
    fn param_ids_round_trip() {
        for (id, ..) in params() {
            if id != OUTPUT {
                let (b, f) = field_of(id).unwrap();
                assert_eq!(param_id(b, f), id);
            }
        }
    }
}
