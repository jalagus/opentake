//! Built-in noise gate. A filtered, stereo-linked detector opens the gate above the
//! threshold and lets it close only once the level falls below the threshold minus the
//! hysteresis and the hold time has passed, so it doesn't chatter around the threshold.
//! Closing attenuates by Range (not necessarily to silence).

use std::sync::Arc;

use crate::dynamics::GainMeter;
use crate::filter::{Biquad, chain, coefficient, db, log_range, norm_log};
use crate::mixer::{MidiEvent, Processor, Transport};

pub const NAME: &str = "OpenTake Gate";
pub const PATH: &str = "gate";
pub const THRESHOLD: u32 = 0;
pub const HYSTERESIS: u32 = 1;
pub const ATTACK: u32 = 2;
pub const HOLD: u32 = 3;
pub const RELEASE: u32 = 4;
pub const RANGE: u32 = 5;
pub const KEY_LOW_CUT: u32 = 6;
pub const KEY_HIGH_CUT: u32 = 7;

pub fn threshold_of(v: f64) -> f64 {
    -80.0 + 80.0 * v.clamp(0.0, 1.0)
}
pub fn norm_threshold(db: f64) -> f64 {
    ((db + 80.0) / 80.0).clamp(0.0, 1.0)
}
pub fn hysteresis_of(v: f64) -> f64 {
    12.0 * v.clamp(0.0, 1.0)
}
pub fn attack_of(v: f64) -> f64 {
    log_range(v, 0.05, 50.0)
}
pub fn norm_attack(ms: f64) -> f64 {
    norm_log(ms, 0.05, 50.0)
}
pub fn hold_of(v: f64) -> f64 {
    500.0 * v.clamp(0.0, 1.0)
}
pub fn release_of(v: f64) -> f64 {
    log_range(v, 5.0, 2000.0)
}
pub fn norm_release(ms: f64) -> f64 {
    norm_log(ms, 5.0, 2000.0)
}
pub fn range_of(v: f64) -> f64 {
    80.0 * v.clamp(0.0, 1.0)
}
pub fn key_low_cut_of(v: f64) -> f64 {
    log_range(v, 20.0, 2000.0)
}
pub fn norm_key_low_cut(hz: f64) -> f64 {
    norm_log(hz, 20.0, 2000.0)
}
pub fn key_high_cut_of(v: f64) -> f64 {
    log_range(v, 1000.0, 20_000.0)
}
pub fn norm_key_high_cut(hz: f64) -> f64 {
    norm_log(hz, 1000.0, 20_000.0)
}

/// Parameters: (id, title, units, default normalized value, steps).
pub fn params() -> Vec<(u32, String, &'static str, f64, i32)> {
    vec![
        (THRESHOLD, "Threshold".into(), "dB", norm_threshold(-40.0), 0),
        (HYSTERESIS, "Hysteresis".into(), "dB", 4.0 / 12.0, 0),
        (ATTACK, "Attack".into(), "ms", norm_attack(1.0), 0),
        (HOLD, "Hold".into(), "ms", 50.0 / 500.0, 0),
        (RELEASE, "Release".into(), "ms", norm_release(120.0), 0),
        (RANGE, "Range".into(), "dB", 1.0, 0),
        (KEY_LOW_CUT, "Key Low Cut".into(), "Hz", 0.0, 0),
        (KEY_HIGH_CUT, "Key High Cut".into(), "Hz", 1.0, 0),
    ]
}

pub fn display(id: u32, v: f64) -> String {
    let hz = |hz: f64| if hz >= 1000.0 { format!("{:.1}k", hz / 1000.0) } else { format!("{hz:.0}") };
    match id {
        THRESHOLD => format!("{:.1}", threshold_of(v)),
        HYSTERESIS => format!("{:.1}", hysteresis_of(v)),
        ATTACK => format!("{:.2}", attack_of(v)),
        HOLD => format!("{:.0}", hold_of(v)),
        RELEASE => format!("{:.0}", release_of(v)),
        RANGE => format!("-{:.0}", range_of(v)),
        KEY_LOW_CUT => hz(key_low_cut_of(v)),
        KEY_HIGH_CUT => hz(key_high_cut_of(v)),
        _ => format!("{v:.2}"),
    }
}

pub struct Gate {
    sample_rate: f64,
    values: Vec<(u32, f64)>,
    dirty: bool,
    open_db: f64,
    close_db: f64,
    floor_db: f64,
    attack: f64,
    release: f64,
    hold: usize,
    /// Key filters per channel (low cut, high cut).
    key: [[Biquad; 2]; 2],
    /// Detector envelope (linear), samples left to hold open, and the gain (dB, ≤ 0).
    env: f64,
    held: usize,
    open: bool,
    gain_db: f64,
    meter: Option<Arc<GainMeter>>,
}

impl Gate {
    pub fn new(sample_rate: f64, meter: Option<Arc<GainMeter>>) -> Self {
        let values = params().into_iter().map(|p| (p.0, p.3)).collect();
        let mut g = Self {
            sample_rate: sample_rate.max(1.0),
            values,
            dirty: true,
            open_db: 0.0,
            close_db: 0.0,
            floor_db: -80.0,
            attack: 0.0,
            release: 0.0,
            hold: 0,
            key: [[Biquad::default(); 2]; 2],
            env: 0.0,
            held: 0,
            open: false,
            gain_db: -80.0,
            meter,
        };
        g.update();
        g.gain_db = g.floor_db;
        g
    }

    fn update(&mut self) {
        let v = |id: u32| self.values.iter().find(|p| p.0 == id).map_or(0.0, |p| p.1);
        let sr = self.sample_rate;
        self.open_db = threshold_of(v(THRESHOLD));
        self.close_db = self.open_db - hysteresis_of(v(HYSTERESIS));
        self.floor_db = -range_of(v(RANGE));
        self.attack = coefficient(attack_of(v(ATTACK)), sr);
        self.release = coefficient(release_of(v(RELEASE)), sr);
        self.hold = (hold_of(v(HOLD)) * 0.001 * sr) as usize;
        for k in &mut self.key {
            k[0].pass(key_low_cut_of(v(KEY_LOW_CUT)), 0.707, sr, true);
            k[1].pass(key_high_cut_of(v(KEY_HIGH_CUT)), 0.707, sr, false);
        }
        self.dirty = false;
    }
}

impl Processor for Gate {
    fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, t: &Transport, e: &[MidiEvent]) {
        self.process_keyed(inputs, None, outputs, frames, t, e);
    }

    /// Keyed, the side chain (through the key filters) opens and closes the gate.
    fn process_keyed(&mut self, inputs: &[&[f32]], key: Option<&[&[f32]]>, outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, _e: &[MidiEvent]) {
        if self.dirty {
            self.update();
        }
        let ([il, ir, ..], [ol, or, ..]) = (inputs, outputs) else { return };
        let (dl, dr) = match key {
            Some([kl, kr, ..]) => (*kl, *kr),
            _ => (*il, *ir),
        };
        // Peak detector: instant rise, ~5 ms fall, so it follows the signal's envelope.
        let fall = coefficient(5.0, self.sample_rate);
        let (mut peak_in, mut peak_out, mut max_atten) = (0.0f32, 0.0f32, 0.0f64);
        for i in 0..frames {
            let (l, r) = (il[i] as f64, ir[i] as f64);
            let (kl, kr) = (chain(&mut self.key[0], dl[i] as f64), chain(&mut self.key[1], dr[i] as f64));
            let level = kl.abs().max(kr.abs());
            self.env = if level > self.env { level } else { fall * self.env + (1.0 - fall) * level };
            let env_db = db(self.env);
            if env_db >= self.open_db {
                self.open = true;
                self.held = self.hold;
            } else if env_db < self.close_db {
                if self.held > 0 {
                    self.held -= 1;
                } else {
                    self.open = false;
                }
            }
            let (target, coef) = if self.open { (0.0, self.attack) } else { (self.floor_db, self.release) };
            self.gain_db = coef * self.gain_db + (1.0 - coef) * target;
            let g = 10f64.powf(self.gain_db / 20.0);
            ol[i] = (l * g) as f32;
            or[i] = (r * g) as f32;
            peak_in = peak_in.max(il[i].abs()).max(ir[i].abs());
            peak_out = peak_out.max(ol[i].abs()).max(or[i].abs());
            max_atten = max_atten.max(-self.gain_db);
        }
        if let Some(m) = &self.meter {
            m.report(max_atten as f32, db(peak_in as f64) as f32, db(peak_out as f64) as f32);
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

    fn run(g: &mut Gate, x: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0f32; x.len()];
        let mut r = vec![0.0f32; x.len()];
        for (k, chunk) in x.chunks(256).enumerate() {
            let at = k * 256;
            let n = chunk.len();
            let mut o: [&mut [f32]; 2] = [&mut out[at..at + n], &mut r[at..at + n]];
            g.process(&[chunk, chunk], &mut o, n, &Transport::default(), &[]);
        }
        out
    }

    fn tone(amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * 0.07).sin()).collect()
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0, |a, v| a.max(v.abs()))
    }

    #[test]
    fn opens_on_loud_and_closes_on_quiet() {
        let meter = GainMeter::new();
        let mut g = Gate::new(48_000.0, Some(meter.clone()));
        // Quiet noise floor (-50 dB), a loud burst (-6 dB), then the floor again.
        let mut x = tone(0.003, 9_600);
        x.extend(tone(0.5, 9_600));
        x.extend(tone(0.003, 48_000));
        let y = run(&mut g, &x);
        assert!(peak(&y[..9_000]) < 0.0003, "closed on the floor");
        assert!((peak(&y[12_000..19_000]) - 0.5).abs() < 0.01, "open on the burst");
        assert!(peak(&y[40_000..]) < 0.0003, "closed again after hold and release");
        assert!(meter.take().0 > 60.0, "attenuation metered");
    }

    #[test]
    fn hold_and_range() {
        let mut g = Gate::new(48_000.0, None);
        g.set_param(HOLD, 200.0 / 500.0);
        g.set_param(RANGE, 20.0 / 80.0);
        let mut x = tone(0.5, 4_800);
        x.extend(tone(0.003, 48_000));
        let y = run(&mut g, &x);
        // Still open 100 ms after the burst (held), then down by only 20 dB.
        assert!((peak(&y[4_800 + 4_000..4_800 + 4_800]) - 0.003).abs() < 0.0003, "held open");
        let tail = peak(&y[40_000..]);
        assert!((tail - 0.0003).abs() < 0.00005, "range 20 dB: {tail}");
    }

    #[test]
    fn key_filter_ignores_out_of_band_triggers() {
        // A loud 60 Hz hum doesn't open a gate whose key only listens above 1 kHz.
        let mut g = Gate::new(48_000.0, None);
        g.set_param(KEY_LOW_CUT, norm_key_low_cut(1000.0));
        let hum: Vec<f32> = (0..24_000).map(|i| 0.3 * (std::f32::consts::TAU * 60.0 * i as f32 / 48_000.0).sin()).collect();
        let y = run(&mut g, &hum);
        assert!(peak(&y[4_800..]) < 0.01, "stays closed: {}", peak(&y[4_800..]));
    }
}
