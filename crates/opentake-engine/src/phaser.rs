//! Built-in phaser: a chain of first-order all-pass stages whose corner frequency a sine LFO
//! sweeps exponentially around Center (Depth octaves wide), with feedback around the chain.
//! Mixed with the dry signal, the chain's phase shift cancels some frequencies: the notches
//! that sweep. The right channel's LFO is offset by Spread.

use std::f64::consts::{PI, TAU};

use crate::filter::{log_range, norm_log};
use crate::mixer::{MidiEvent, Processor, Transport};

pub const NAME: &str = "OpenTake Phaser";
pub const PATH: &str = "phaser";
pub const RATE: u32 = 0;
pub const DEPTH: u32 = 1;
pub const CENTER: u32 = 2;
pub const FEEDBACK: u32 = 3;
pub const STAGES: u32 = 4;
pub const SPREAD: u32 = 5;
pub const MIX: u32 = 6;

pub const STAGE_CHOICES: [usize; 6] = [2, 4, 6, 8, 10, 12];
const MAX_STAGES: usize = 12;

pub fn rate_of(v: f64) -> f64 {
    log_range(v, 0.02, 10.0)
}
pub fn norm_rate(hz: f64) -> f64 {
    norm_log(hz, 0.02, 10.0)
}
/// Sweep width in octaves.
pub fn depth_of(v: f64) -> f64 {
    6.0 * v.clamp(0.0, 1.0)
}
pub fn center_of(v: f64) -> f64 {
    log_range(v, 100.0, 5000.0)
}
pub fn norm_center(hz: f64) -> f64 {
    norm_log(hz, 100.0, 5000.0)
}
pub fn feedback_of(v: f64) -> f64 {
    -0.95 + 1.9 * v.clamp(0.0, 1.0)
}
pub fn norm_feedback(f: f64) -> f64 {
    ((f + 0.95) / 1.9).clamp(0.0, 1.0)
}
pub fn stages_of(v: f64) -> usize {
    STAGE_CHOICES[(v.clamp(0.0, 1.0) * (STAGE_CHOICES.len() - 1) as f64).round() as usize]
}
pub fn norm_stages(n: usize) -> f64 {
    STAGE_CHOICES.iter().position(|&s| s >= n).unwrap_or(STAGE_CHOICES.len() - 1) as f64 / (STAGE_CHOICES.len() - 1) as f64
}

pub fn params() -> Vec<(u32, String, &'static str, f64, i32)> {
    vec![
        (RATE, "Rate".into(), "Hz", norm_rate(0.4), 0),
        (DEPTH, "Depth".into(), "oct", 3.0 / 6.0, 0),
        (CENTER, "Center".into(), "Hz", norm_center(800.0), 0),
        (FEEDBACK, "Feedback".into(), "%", norm_feedback(0.4), 0),
        (STAGES, "Stages".into(), "", norm_stages(4), (STAGE_CHOICES.len() - 1) as i32),
        (SPREAD, "Spread".into(), "°", 0.5, 0),
        (MIX, "Mix".into(), "%", 0.5, 0),
    ]
}

pub fn display(id: u32, v: f64) -> String {
    match id {
        RATE => format!("{:.2}", rate_of(v)),
        DEPTH => format!("{:.1}", depth_of(v)),
        CENTER => {
            let hz = center_of(v);
            if hz >= 1000.0 { format!("{:.2}k", hz / 1000.0) } else { format!("{hz:.0}") }
        }
        FEEDBACK => format!("{:+.0}", feedback_of(v) * 100.0),
        STAGES => stages_of(v).to_string(),
        SPREAD => format!("{:.0}", v * 180.0),
        MIX => format!("{:.0}", v * 100.0),
        _ => format!("{v:.2}"),
    }
}

/// The settings in real units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub rate_hz: f64,
    pub depth_oct: f64,
    pub center_hz: f64,
    pub feedback: f64,
    pub stages: usize,
    pub spread: f64,
    pub mix: f64,
}

impl Settings {
    pub fn from_params(values: &[(u32, f64)]) -> Self {
        let defaults = params();
        let v = |id: u32| values.iter().rev().find(|p| p.0 == id).map(|p| p.1).or_else(|| defaults.iter().find(|p| p.0 == id).map(|p| p.3)).unwrap_or(0.0);
        Self {
            rate_hz: rate_of(v(RATE)),
            depth_oct: depth_of(v(DEPTH)),
            center_hz: center_of(v(CENTER)),
            feedback: feedback_of(v(FEEDBACK)),
            stages: stages_of(v(STAGES)),
            spread: v(SPREAD),
            mix: v(MIX),
        }
    }

    /// The swept corner frequency at LFO value `lfo` (-1..1).
    pub fn corner(&self, lfo: f64, sample_rate: f64) -> f64 {
        (self.center_hz * 2f64.powf(lfo * self.depth_oct / 2.0)).clamp(20.0, sample_rate * 0.45)
    }

    /// Magnitude response (dB) at `hz` with the stages' corner at `corner` Hz.
    pub fn response_db(&self, hz: f64, corner: f64, sample_rate: f64) -> f64 {
        // One stage: (a + z^-1) / (1 + a z^-1) at z = e^{jw}; the chain is that to the Nth
        // power, with feedback g around it: wet = A / (1 - g A).
        let a = allpass_coef(corner, sample_rate);
        let w = TAU * hz / sample_rate;
        let (zr, zi) = (w.cos(), -w.sin());
        let mul = |(ar, ai): (f64, f64), (br, bi): (f64, f64)| (ar * br - ai * bi, ar * bi + ai * br);
        let div = |(ar, ai): (f64, f64), (br, bi): (f64, f64)| {
            let d = br * br + bi * bi;
            ((ar * br + ai * bi) / d, (ai * br - ar * bi) / d)
        };
        let stage = div((a + zr, zi), (1.0 + a * zr, a * zi));
        let chain = (0..self.stages).fold((1.0, 0.0), |acc, _| mul(acc, stage));
        let wet = div(chain, (1.0 - self.feedback * chain.0, -self.feedback * chain.1));
        let (re, im) = ((1.0 - self.mix) + self.mix * wet.0, self.mix * wet.1);
        20.0 * (re * re + im * im).sqrt().max(1e-6).log10()
    }
}

fn allpass_coef(hz: f64, sample_rate: f64) -> f64 {
    let t = (PI * hz / sample_rate).tan();
    (t - 1.0) / (t + 1.0)
}

pub struct Phaser {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    settings: Settings,
    dirty: bool,
    /// All-pass states per channel and stage, and the chain's last output (for feedback).
    z: [[f64; MAX_STAGES]; 2],
    last: [f64; 2],
    phase: f64,
}

impl Phaser {
    pub fn new(sample_rate: f64) -> Self {
        let values: Vec<(u32, f64)> = params().into_iter().map(|p| (p.0, p.3)).collect();
        let settings = Settings::from_params(&values);
        Self { sample_rate: sample_rate.max(1.0), values, settings, dirty: false, z: [[0.0; MAX_STAGES]; 2], last: [0.0; 2], phase: 0.0 }
    }
}

impl Processor for Phaser {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.settings = Settings::from_params(&self.values);
            self.dirty = false;
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        let s = self.settings;
        let sr = self.sample_rate;
        let step = TAU * s.rate_hz / sr;
        for i in 0..frames {
            for ch in 0..2 {
                let lfo = (self.phase + if ch == 1 { s.spread * PI } else { 0.0 }).sin();
                let a = allpass_coef(s.corner(lfo, sr), sr);
                let x = if ch == 0 { il[i] } else { ir[i] } as f64;
                let mut y = x + s.feedback * self.last[ch];
                for z in self.z[ch].iter_mut().take(s.stages) {
                    let out = a * y + *z;
                    *z = y - a * out;
                    if z.abs() < 1e-25 {
                        *z = 0.0;
                    }
                    y = out;
                }
                self.last[ch] = y;
                let out = (x * (1.0 - s.mix) + y * s.mix) as f32;
                if ch == 0 { ol[i] = out } else { or[i] = out }
            }
            self.phase = (self.phase + step) % TAU;
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

    fn rms_out(p: &mut Phaser, hz: f64) -> f64 {
        let n = 24_000;
        let x: Vec<f32> = (0..n).map(|i| (TAU * hz * i as f64 / 48_000.0).sin() as f32).collect();
        let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
        for k in (0..n).step_by(512) {
            let m = (n - k).min(512);
            let mut o: [&mut [f32]; 2] = [&mut l[k..k + m], &mut r[k..k + m]];
            p.process(&[&x[k..k + m], &x[k..k + m]], &mut o, m, &Transport::default(), &[]);
        }
        let tail = &l[12_000..];
        (tail.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / tail.len() as f64 * 2.0).sqrt()
    }

    #[test]
    fn notches_where_the_response_says() {
        // No sweep, no feedback, four stages at 1 kHz: each stage is -90° at the corner, so
        // the chain is -180° (a notch) where each stage gives -45°: 1 kHz · tan(22.5°).
        let mut p = Phaser::new(48_000.0);
        for (id, v) in [(DEPTH, 0.0), (FEEDBACK, norm_feedback(0.0)), (CENTER, norm_center(1000.0))] {
            p.set_param(id, v);
        }
        let s = p.settings_now();
        let notch = 1000.0 * (PI / 8.0).tan();
        let corner = s.corner(0.0, 48_000.0);
        assert!(s.response_db(notch, corner, 48_000.0) < -30.0);
        let (at_notch, away) = (rms_out(&mut p, notch), rms_out(&mut Phaser::new(48_000.0).with(&[(DEPTH, 0.0), (FEEDBACK, norm_feedback(0.0)), (CENTER, norm_center(1000.0))]), 1000.0));
        assert!(at_notch < 0.05, "measured notch: {at_notch}");
        assert!((away - 1.0).abs() < 0.02, "full level at the corner (chain at -360°): {away}");
        let predicted = 10f64.powf(s.response_db(3000.0, corner, 48_000.0) / 20.0);
        let measured = rms_out(&mut Phaser::new(48_000.0).with(&[(DEPTH, 0.0), (FEEDBACK, norm_feedback(0.0)), (CENTER, norm_center(1000.0))]), 3000.0);
        assert!((predicted - measured).abs() < 0.02, "response {predicted} vs measured {measured}");
    }

    #[test]
    fn dry_at_mix_zero_and_stable_at_max_feedback() {
        let mut p = Phaser::new(48_000.0).with(&[(MIX, 0.0)]);
        assert!((rms_out(&mut p, 440.0) - 1.0).abs() < 1e-3);
        let mut p = Phaser::new(48_000.0).with(&[(FEEDBACK, 1.0), (STAGES, 1.0), (RATE, 1.0)]);
        let level = rms_out(&mut p, 700.0);
        assert!(level.is_finite() && level < 20.0, "{level}");
    }

    impl Phaser {
        fn with(mut self, values: &[(u32, f64)]) -> Self {
            for &(id, v) in values {
                self.set_param(id, v);
            }
            self
        }

        fn settings_now(&mut self) -> Settings {
            Settings::from_params(&self.values)
        }
    }
}
