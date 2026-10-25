//! Built-in chorus: up to four voices, each a delay line whose length a sine LFO sweeps
//! (phases spread evenly across voices; Width moves the right channel's phases between the
//! left's, so the sides differ however many voices there are), read with cubic
//! interpolation so the sweep stays smooth. Feedback turns it toward a flanger; Tone
//! softens the wet signal.

use crate::filter::{coefficient, log_range, norm_log};
use crate::mixer::{MidiEvent, Processor, Transport};

pub const NAME: &str = "OpenTake Chorus";
pub const PATH: &str = "chorus";
pub const RATE: u32 = 0;
pub const DEPTH: u32 = 1;
pub const DELAY: u32 = 2;
pub const VOICES: u32 = 3;
pub const WIDTH: u32 = 4;
pub const FEEDBACK: u32 = 5;
pub const TONE: u32 = 6;
pub const MIX: u32 = 7;

pub const MAX_VOICES: usize = 4;
/// Longest delay reached (base + sweep), in seconds.
const MAX_DELAY_SECS: f64 = 0.06;

pub fn rate_of(v: f64) -> f64 {
    log_range(v, 0.05, 8.0)
}
pub fn norm_rate(hz: f64) -> f64 {
    norm_log(hz, 0.05, 8.0)
}
/// Sweep depth in milliseconds.
pub fn depth_of(v: f64) -> f64 {
    10.0 * v.clamp(0.0, 1.0)
}
pub fn delay_of(v: f64) -> f64 {
    log_range(v, 1.0, 40.0)
}
pub fn norm_delay(ms: f64) -> f64 {
    norm_log(ms, 1.0, 40.0)
}
pub fn voices_of(v: f64) -> usize {
    1 + (v.clamp(0.0, 1.0) * (MAX_VOICES - 1) as f64).round() as usize
}
pub fn norm_voices(n: usize) -> f64 {
    (n.clamp(1, MAX_VOICES) - 1) as f64 / (MAX_VOICES - 1) as f64
}
pub fn feedback_of(v: f64) -> f64 {
    -0.9 + 1.8 * v.clamp(0.0, 1.0)
}
pub fn norm_feedback(f: f64) -> f64 {
    ((f + 0.9) / 1.8).clamp(0.0, 1.0)
}
pub fn tone_of(v: f64) -> f64 {
    log_range(v, 1000.0, 20_000.0)
}

pub fn params() -> Vec<(u32, String, &'static str, f64, i32)> {
    vec![
        (RATE, "Rate".into(), "Hz", norm_rate(0.6), 0),
        (DEPTH, "Depth".into(), "ms", 0.3, 0),
        (DELAY, "Delay".into(), "ms", norm_delay(12.0), 0),
        (VOICES, "Voices".into(), "", norm_voices(2), (MAX_VOICES - 1) as i32),
        (WIDTH, "Width".into(), "%", 1.0, 0),
        (FEEDBACK, "Feedback".into(), "%", norm_feedback(0.0), 0),
        (TONE, "Tone".into(), "Hz", 1.0, 0),
        (MIX, "Mix".into(), "%", 0.5, 0),
    ]
}

pub fn display(id: u32, v: f64) -> String {
    match id {
        RATE => format!("{:.2}", rate_of(v)),
        DEPTH => format!("{:.1}", depth_of(v)),
        DELAY => format!("{:.1}", delay_of(v)),
        VOICES => voices_of(v).to_string(),
        WIDTH | MIX => format!("{:.0}", v * 100.0),
        FEEDBACK => format!("{:+.0}", feedback_of(v) * 100.0),
        TONE => format!("{:.1}k", tone_of(v) / 1000.0),
        _ => format!("{v:.2}"),
    }
}

/// The settings in real units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub rate_hz: f64,
    pub depth_ms: f64,
    pub delay_ms: f64,
    pub voices: usize,
    pub width: f64,
    pub feedback: f64,
    pub tone_hz: f64,
    pub mix: f64,
}

impl Settings {
    pub fn from_params(values: &[(u32, f64)]) -> Self {
        let defaults = params();
        let v = |id: u32| values.iter().rev().find(|p| p.0 == id).map(|p| p.1).or_else(|| defaults.iter().find(|p| p.0 == id).map(|p| p.3)).unwrap_or(0.0);
        Self {
            rate_hz: rate_of(v(RATE)),
            depth_ms: depth_of(v(DEPTH)),
            delay_ms: delay_of(v(DELAY)),
            voices: voices_of(v(VOICES)),
            width: v(WIDTH),
            feedback: feedback_of(v(FEEDBACK)),
            tone_hz: tone_of(v(TONE)),
            mix: v(MIX),
        }
    }

    /// Voice `voice`'s delay (ms) at LFO phase `phase` (radians), left channel.
    pub fn voice_delay(&self, voice: usize, phase: f64) -> f64 {
        let offset = voice as f64 / self.voices as f64 * std::f64::consts::TAU;
        self.delay_ms + self.depth_ms * (0.5 + 0.5 * (phase + offset).sin())
    }
}

pub struct Chorus {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    settings: Settings,
    dirty: bool,
    buf: [Vec<f32>; 2],
    mask: usize,
    pos: usize,
    phase: f64,
    /// Smoothed base delay and depth (samples), so turning them doesn't click.
    delay: f64,
    depth: f64,
    smooth: f64,
    /// Last wet sample per channel (for feedback) and the tone filter's state.
    last: [f64; 2],
    tone: [f64; 2],
    tone_coef: f64,
}

impl Chorus {
    pub fn new(sample_rate: f64) -> Self {
        let sr = sample_rate.max(1.0);
        let len = ((MAX_DELAY_SECS * sr) as usize + 8).next_power_of_two();
        let values: Vec<(u32, f64)> = params().into_iter().map(|p| (p.0, p.3)).collect();
        let settings = Settings::from_params(&values);
        let mut c = Self {
            sample_rate: sr,
            values,
            settings,
            dirty: true,
            buf: [vec![0.0; len], vec![0.0; len]],
            mask: len - 1,
            pos: 0,
            phase: 0.0,
            delay: settings.delay_ms * 0.001 * sr,
            depth: settings.depth_ms * 0.001 * sr,
            smooth: coefficient(30.0, sr),
            last: [0.0; 2],
            tone: [0.0; 2],
            tone_coef: 0.0,
        };
        c.update();
        c
    }

    fn update(&mut self) {
        self.settings = Settings::from_params(&self.values);
        self.tone_coef = (-std::f64::consts::TAU * self.settings.tone_hz / self.sample_rate).exp();
        self.dirty = false;
    }

    /// Read channel `ch` `delay` samples back, with cubic (Catmull-Rom) interpolation.
    fn read(&self, ch: usize, delay: f64) -> f64 {
        let at = self.pos as f64 - delay;
        let i = at.floor();
        let t = at - i;
        let s = |k: i64| self.buf[ch][((i as i64 + k) as usize) & self.mask] as f64;
        let (y0, y1, y2, y3) = (s(-1), s(0), s(1), s(2));
        y1 + 0.5 * t * (y2 - y0 + t * (2.0 * y0 - 5.0 * y1 + 4.0 * y2 - y3 + t * (3.0 * (y1 - y2) + y3 - y0)))
    }
}

impl Processor for Chorus {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        let s = self.settings;
        let sr = self.sample_rate;
        let step = std::f64::consts::TAU * s.rate_hz / sr;
        let (target_delay, target_depth) = (s.delay_ms * 0.001 * sr, s.depth_ms * 0.001 * sr);
        let gain = 1.0 / (s.voices as f64).sqrt();
        let right_offset = s.width * std::f64::consts::PI / s.voices as f64;
        for i in 0..frames {
            self.delay = self.smooth * self.delay + (1.0 - self.smooth) * target_delay;
            self.depth = self.smooth * self.depth + (1.0 - self.smooth) * target_depth;
            let dry = [il[i] as f64, ir[i] as f64];
            for (ch, x) in dry.iter().enumerate() {
                self.buf[ch][self.pos] = (x + s.feedback * self.last[ch]) as f32;
            }
            let mut wet = [0.0f64; 2];
            for v in 0..s.voices {
                let offset = v as f64 / s.voices as f64 * std::f64::consts::TAU;
                for (ch, w) in wet.iter_mut().enumerate() {
                    let phase = self.phase + offset + if ch == 1 { right_offset } else { 0.0 };
                    // At least two samples back, so the cubic read never reaches the write head.
                    let d = (self.delay + self.depth * (0.5 + 0.5 * phase.sin())).max(2.0);
                    *w += self.read(ch, d);
                }
            }
            for ch in 0..2 {
                let w = wet[ch] * gain;
                // Feed back the voices' average, so the loop gain never exceeds Feedback
                // however coherently the voices add up.
                let back = wet[ch] / s.voices as f64;
                self.last[ch] = if back.abs() < 1e-20 { 0.0 } else { back };
                self.tone[ch] = self.tone_coef * self.tone[ch] + (1.0 - self.tone_coef) * w;
                let w = if s.tone_hz >= 19_999.0 { w } else { self.tone[ch] };
                let y = dry[ch] * (1.0 - s.mix) + w * s.mix;
                if ch == 0 { ol[i] = y as f32 } else { or[i] = y as f32 }
            }
            self.pos = (self.pos + 1) & self.mask;
            self.phase = (self.phase + step) % std::f64::consts::TAU;
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

    fn run(c: &mut Chorus, l: &[f32], r: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let (mut ol, mut or) = (vec![0.0f32; l.len()], vec![0.0f32; l.len()]);
        for k in (0..l.len()).step_by(256) {
            let n = (l.len() - k).min(256);
            let mut o: [&mut [f32]; 2] = [&mut ol[k..k + n], &mut or[k..k + n]];
            c.process(&[&l[k..k + n], &r[k..k + n]], &mut o, n, &Transport::default(), &[]);
        }
        (ol, or)
    }

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as f32 / u32::MAX as f32 - 0.5
            })
            .collect()
    }

    #[test]
    fn dry_at_mix_zero_and_a_plain_delay_without_sweep() {
        let x = noise(48_000, 3);
        let mut c = Chorus::new(48_000.0);
        c.set_param(MIX, 0.0);
        assert_eq!(run(&mut c, &x, &x).0, x, "mix 0 is the dry signal");
        // One voice, no sweep, 10 ms (once the delay has glided there): fully wet is the input
        // 480 samples later.
        let mut c = Chorus::new(48_000.0);
        for (id, v) in [(MIX, 1.0), (DEPTH, 0.0), (VOICES, 0.0), (DELAY, norm_delay(10.0))] {
            c.set_param(id, v);
        }
        let (y, _) = run(&mut c, &x, &x);
        let err = (36_000..48_000).map(|i| (y[i] - x[i - 480]).abs()).fold(0.0f32, f32::max);
        assert!(err < 1e-4, "{err}");
    }

    #[test]
    fn sweeping_voices_widen_the_stereo_image_and_stay_bounded() {
        let x = noise(48_000, 9);
        let mut c = Chorus::new(48_000.0);
        for (id, v) in [(MIX, 1.0), (VOICES, 1.0), (FEEDBACK, norm_feedback(0.85)), (DEPTH, 1.0)] {
            c.set_param(id, v);
        }
        let (l, r) = run(&mut c, &x, &x);
        assert!(l.iter().chain(&r).all(|v| v.is_finite() && v.abs() < 4.0), "stable with high feedback");
        let corr = |a: &[f32], b: &[f32]| {
            let (ab, aa, bb) = a.iter().zip(b).fold((0.0, 0.0, 0.0), |(ab, aa, bb), (x, y)| (ab + x * y, aa + x * x, bb + y * y));
            ab / (aa * bb).sqrt()
        };
        assert!(corr(&l[24_000..], &r[24_000..]) < 0.9, "width decorrelates the sides");
    }
}
