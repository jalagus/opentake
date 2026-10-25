//! Built-in de-esser. A Linkwitz–Riley crossover at Frequency splits the signal; the high
//! band is the detector, and above the threshold the gain comes down by at most Range. In
//! Split mode only the high band is reduced (the two bands add back up flat, so with no
//! reduction the level is unchanged at every frequency); in Wideband mode the whole signal
//! is. Listen plays the high band, to tune Frequency by ear.

use std::sync::Arc;

use crate::dynamics::GainMeter;
use crate::filter::{Biquad, chain, coefficient, db, log_range, norm_log};
use crate::mixer::{MidiEvent, Processor, Transport};

pub const NAME: &str = "OpenTake De-Esser";
pub const PATH: &str = "deesser";
pub const FREQUENCY: u32 = 0;
pub const THRESHOLD: u32 = 1;
pub const RANGE: u32 = 2;
pub const RELEASE: u32 = 3;
/// 0 = Split (reduce only the sibilant band), 1 = Wideband.
pub const MODE: u32 = 4;
pub const LISTEN: u32 = 5;

/// Above the threshold the band is reduced at this ratio (until Range).
const RATIO: f64 = 6.0;
const ATTACK_MS: f64 = 0.5;

pub fn frequency_of(v: f64) -> f64 {
    log_range(v, 2000.0, 16_000.0)
}
pub fn norm_frequency(hz: f64) -> f64 {
    norm_log(hz, 2000.0, 16_000.0)
}
pub fn threshold_of(v: f64) -> f64 {
    -60.0 + 60.0 * v.clamp(0.0, 1.0)
}
pub fn norm_threshold(db: f64) -> f64 {
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}
pub fn range_of(v: f64) -> f64 {
    24.0 * v.clamp(0.0, 1.0)
}
pub fn release_of(v: f64) -> f64 {
    log_range(v, 10.0, 300.0)
}
pub fn norm_release(ms: f64) -> f64 {
    norm_log(ms, 10.0, 300.0)
}

pub fn params() -> Vec<(u32, String, &'static str, f64, i32)> {
    vec![
        (FREQUENCY, "Frequency".into(), "Hz", norm_frequency(6000.0), 0),
        (THRESHOLD, "Threshold".into(), "dB", norm_threshold(-30.0), 0),
        (RANGE, "Range".into(), "dB", 10.0 / 24.0, 0),
        (RELEASE, "Release".into(), "ms", norm_release(60.0), 0),
        (MODE, "Wideband".into(), "", 0.0, 1),
        (LISTEN, "Listen".into(), "", 0.0, 1),
    ]
}

pub fn display(id: u32, v: f64) -> String {
    match id {
        FREQUENCY => format!("{:.1}k", frequency_of(v) / 1000.0),
        THRESHOLD => format!("{:.1}", threshold_of(v)),
        RANGE => format!("-{:.1}", range_of(v)),
        RELEASE => format!("{:.0}", release_of(v)),
        MODE => (if v >= 0.5 { "Wideband" } else { "Split" }).into(),
        LISTEN => (if v >= 0.5 { "On" } else { "Off" }).into(),
        _ => format!("{v:.2}"),
    }
}

pub struct DeEsser {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    dirty: bool,
    threshold_db: f64,
    range_db: f64,
    attack: f64,
    release: f64,
    wideband: bool,
    listen: bool,
    /// Per channel the crossover's low and high halves (4th-order: two sections each).
    low: [[Biquad; 2]; 2],
    band: [[Biquad; 2]; 2],
    reduction_db: f64,
    meter: Option<Arc<GainMeter>>,
}

impl DeEsser {
    pub fn new(sample_rate: f64, meter: Option<Arc<GainMeter>>) -> Self {
        let values = params().into_iter().map(|p| (p.0, p.3)).collect();
        let mut d = Self {
            sample_rate: sample_rate.max(1.0),
            values,
            dirty: true,
            threshold_db: 0.0,
            range_db: 0.0,
            attack: 0.0,
            release: 0.0,
            wideband: false,
            listen: false,
            low: [[Biquad::default(); 2]; 2],
            band: [[Biquad::default(); 2]; 2],
            reduction_db: 0.0,
            meter,
        };
        d.update();
        d
    }

    fn update(&mut self) {
        let v = |id: u32| self.values.iter().find(|p| p.0 == id).map_or(0.0, |p| p.1);
        let sr = self.sample_rate;
        self.threshold_db = threshold_of(v(THRESHOLD));
        self.range_db = range_of(v(RANGE));
        self.attack = coefficient(ATTACK_MS, sr);
        self.release = coefficient(release_of(v(RELEASE)), sr);
        self.wideband = v(MODE) >= 0.5;
        self.listen = v(LISTEN) >= 0.5;
        let hz = frequency_of(v(FREQUENCY));
        for (low, band) in self.low.iter_mut().zip(&mut self.band) {
            for (l, h) in low.iter_mut().zip(band.iter_mut()) {
                l.pass(hz, 0.707, sr, false);
                h.pass(hz, 0.707, sr, true);
            }
        }
        self.dirty = false;
    }
}

impl Processor for DeEsser {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        let (mut peak_in, mut peak_out, mut max_gr) = (0.0f32, 0.0f32, 0.0f64);
        for i in 0..frames {
            let (l, r) = (il[i] as f64, ir[i] as f64);
            let (hl, hr) = (chain(&mut self.band[0], l), chain(&mut self.band[1], r));
            let over = db(hl.abs().max(hr.abs())) - self.threshold_db;
            let target = (over * (1.0 - 1.0 / RATIO)).clamp(0.0, self.range_db);
            let coef = if target > self.reduction_db { self.attack } else { self.release };
            self.reduction_db = coef * self.reduction_db + (1.0 - coef) * target;
            let g = 10f64.powf(-self.reduction_db / 20.0);
            let (yl, yr) = if self.listen {
                (hl, hr)
            } else if self.wideband {
                (l * g, r * g)
            } else {
                (chain(&mut self.low[0], l) + hl * g, chain(&mut self.low[1], r) + hr * g)
            };
            ol[i] = yl as f32;
            or[i] = yr as f32;
            peak_in = peak_in.max(il[i].abs()).max(ir[i].abs());
            peak_out = peak_out.max(ol[i].abs()).max(or[i].abs());
            max_gr = max_gr.max(self.reduction_db);
        }
        if let Some(m) = &self.meter {
            m.report(max_gr as f32, db(peak_in as f64) as f32, db(peak_out as f64) as f32);
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

    fn run(d: &mut DeEsser, x: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0f32; x.len()];
        let mut r = vec![0.0f32; x.len()];
        for (k, chunk) in x.chunks(256).enumerate() {
            let at = k * 256;
            let n = chunk.len();
            let mut o: [&mut [f32]; 2] = [&mut out[at..at + n], &mut r[at..at + n]];
            d.process(&[chunk, chunk], &mut o, n, &Transport::default(), &[]);
        }
        out
    }

    fn sine(hz: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / 48_000.0).sin()).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn reduces_sibilance_but_not_the_voice() {
        // A loud 10 kHz "ess" comes down by about Range (10 dB); a 200 Hz voice passes.
        let meter = GainMeter::new();
        let ess = sine(10_000.0, 0.3, 24_000);
        let y = run(&mut DeEsser::new(48_000.0, Some(meter.clone())), &ess);
        let ratio = rms(&y[12_000..]) / rms(&ess[12_000..]);
        assert!(ratio < 0.4, "the ess is reduced: {ratio}");
        let gr = meter.take().0;
        assert!(gr > 8.0 && gr <= 10.01, "reduction within range: {gr}");
        let voice = sine(200.0, 0.3, 24_000);
        let y = run(&mut DeEsser::new(48_000.0, None), &voice);
        assert!((rms(&y[12_000..]) / rms(&voice[12_000..]) - 1.0).abs() < 0.01, "the voice passes untouched");
    }

    #[test]
    fn split_mode_keeps_levels_below_threshold() {
        // With nothing to reduce the bands add back up flat, across the crossover too.
        let mut d = DeEsser::new(48_000.0, None);
        d.set_param(THRESHOLD, 1.0); // 0 dB: never triggers
        for hz in [300.0, 4000.0, 6000.0, 9000.0] {
            let x = sine(hz, 0.3, 9_600);
            let y = run(&mut d, &x);
            let ratio = rms(&y[4_800..]) / rms(&x[4_800..]);
            assert!((ratio - 1.0).abs() < 0.02, "{hz} Hz: {ratio}");
        }
    }

    #[test]
    fn listen_plays_the_detector_band() {
        let mut d = DeEsser::new(48_000.0, None);
        d.set_param(LISTEN, 1.0);
        let low = run(&mut d, &sine(200.0, 0.5, 9_600));
        assert!(rms(&low[4_800..]) < 0.005, "the voice is filtered out");
    }
}
