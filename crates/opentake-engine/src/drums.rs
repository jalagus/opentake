//! Built-in drum kit: synthesised one-shots on the General MIDI drum map, so drum parts
//! (e.g. from Guitar Pro) can be heard without a drum plugin. Each hit mixes a pitched
//! sine sweep (kick, toms, snare body), filtered noise (snare, hats, cymbals) and a
//! metallic cluster of square waves (hats, cymbals, cowbell), each with its own decay.

use std::f32::consts::TAU;

use crate::mixer::{MidiEvent, Processor, Transport};

const VOICES: usize = 32;

pub mod param {
    pub const DECAY: u32 = 0;
    pub const TONE: u32 = 1;
    pub const LEVEL: u32 = 2;
}

/// (id, title, units, default normalized value, steps)
pub const PARAMS: [(u32, &str, &str, f64, i32); 3] = [
    (param::DECAY, "Decay", "%", 0.5, 0),
    (param::TONE, "Tone", "%", 0.5, 0),
    (param::LEVEL, "Level", "dB", 0.8, 0),
];

pub const NAME: &str = "OpenTake Drums";
pub const PATH: &str = "drums";

fn decay_scale(v: f64) -> f32 {
    2f32.powf((v as f32 - 0.5) * 2.0) // 0.5x .. 2x
}

fn tone_scale(v: f64) -> f32 {
    2f32.powf((v as f32 - 0.5) * 1.0) // ±half an octave
}

fn level_db(v: f64) -> f64 {
    -48.0 + v * 54.0
}

pub fn display(id: u32, v: f64) -> String {
    match id {
        param::DECAY => format!("{:.0}", decay_scale(v) * 100.0),
        param::TONE => format!("{:.0}", tone_scale(v) * 100.0),
        param::LEVEL => format!("{:+.1}", level_db(v)),
        _ => format!("{v:.2}"),
    }
}

/// How one drum sounds. Times in seconds, frequencies in Hz.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Patch {
    /// Sine body: start and end frequency, sweep time constant, decay, level.
    tone: (f32, f32, f32, f32, f32),
    /// Noise: band centre, decay, level.
    noise: (f32, f32, f32),
    /// Metallic squares: base multiplier, decay, level.
    metal: (f32, f32, f32),
    /// Clap-style re-triggered noise bursts.
    clap: bool,
    /// Choke group: a new hit cuts voices in the same group (hi-hats).
    choke: u8,
    gain: f32,
}

const SILENT: Patch = Patch { tone: (0.0, 0.0, 1.0, 0.1, 0.0), noise: (1000.0, 0.1, 0.0), metal: (1.0, 0.1, 0.0), clap: false, choke: 0, gain: 1.0 };

fn tom(freq: f32) -> Patch {
    Patch { tone: (freq * 1.6, freq, 0.04, 0.38, 1.0), noise: (freq * 6.0, 0.03, 0.12), ..SILENT }
}

/// General MIDI percussion key → patch.
fn patch(key: u8) -> Patch {
    match key {
        35 | 36 => Patch { tone: (160.0, 48.0, 0.035, 0.42, 1.2), noise: (3000.0, 0.008, 0.25), gain: 1.1, ..SILENT },
        37 => Patch { tone: (800.0, 750.0, 0.01, 0.04, 0.5), noise: (2500.0, 0.03, 0.5), ..SILENT },
        38 | 40 => Patch { tone: (240.0, 185.0, 0.02, 0.12, 0.6), noise: (4000.0, 0.17, 0.85), ..SILENT },
        39 => Patch { noise: (1400.0, 0.16, 1.0), clap: true, ..SILENT },
        41 => tom(82.0),
        43 => tom(98.0),
        45 => tom(110.0),
        47 => tom(131.0),
        48 => tom(147.0),
        50 => tom(175.0),
        42 => Patch { noise: (9000.0, 0.045, 0.6), metal: (1.0, 0.04, 0.35), choke: 1, gain: 0.7, ..SILENT },
        44 => Patch { noise: (8000.0, 0.035, 0.45), metal: (1.0, 0.03, 0.3), choke: 1, gain: 0.6, ..SILENT },
        46 => Patch { noise: (9000.0, 0.45, 0.55), metal: (1.0, 0.4, 0.35), choke: 1, gain: 0.65, ..SILENT },
        49 | 57 => Patch { noise: (7000.0, 1.6, 0.7), metal: (1.3, 1.1, 0.3), gain: 0.65, ..SILENT },
        52 => Patch { noise: (5000.0, 1.0, 0.75), metal: (0.9, 0.8, 0.45), gain: 0.6, ..SILENT },
        55 => Patch { noise: (9000.0, 0.6, 0.7), metal: (1.5, 0.45, 0.3), gain: 0.6, ..SILENT },
        51 | 59 => Patch { noise: (8000.0, 0.9, 0.3), metal: (1.6, 1.0, 0.45), gain: 0.55, ..SILENT },
        53 => Patch { tone: (2400.0, 2400.0, 1.0, 0.7, 0.25), metal: (2.0, 0.8, 0.6), gain: 0.55, ..SILENT },
        54 => Patch { noise: (7000.0, 0.18, 0.8), metal: (2.2, 0.12, 0.3), gain: 0.5, ..SILENT },
        // Cowbell: metal base 0 selects the two 808 cowbell squares, unfiltered.
        56 => Patch { metal: (0.0, 0.25, 0.9), gain: 0.5, ..SILENT },
        // Anything else in the percussion range: a short woody click.
        _ => Patch { tone: (900.0, 600.0, 0.01, 0.06, 0.6), noise: (3000.0, 0.03, 0.3), gain: 0.6, ..SILENT },
    }
}

/// The six 808 cymbal oscillator frequencies (Hz).
const METAL: [f32; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];

#[derive(Clone, Copy)]
struct Voice {
    active: bool,
    patch: Patch,
    velocity: f32,
    age: u64,
    t: f32,
    phase: f32,
    metal_phase: [f32; 6],
    env_tone: f32,
    env_noise: f32,
    env_metal: f32,
    /// State-variable filter for the noise (band-pass) and metal (high-pass).
    svf_noise: (f32, f32),
    svf_metal: (f32, f32),
}

impl Voice {
    const IDLE: Voice = Voice {
        active: false,
        patch: SILENT,
        velocity: 0.0,
        age: 0,
        t: 0.0,
        phase: 0.0,
        metal_phase: [0.0; 6],
        env_tone: 0.0,
        env_noise: 0.0,
        env_metal: 0.0,
        svf_noise: (0.0, 0.0),
        svf_metal: (0.0, 0.0),
    };
}

pub struct DrumSynth {
    sample_rate: f32,
    voices: [Voice; VOICES],
    values: [f64; 3],
    clock: u64,
    noise: u32,
}

impl DrumSynth {
    pub fn new(sample_rate: f64) -> Self {
        Self { sample_rate: sample_rate.max(1.0) as f32, voices: [Voice::IDLE; VOICES], values: PARAMS.map(|p| p.3), clock: 0, noise: 0x1234_5678 }
    }

    fn hit(&mut self, key: u8, velocity: u8) {
        self.clock += 1;
        let patch = patch(key);
        if patch.choke != 0 {
            for v in self.voices.iter_mut().filter(|v| v.active && v.patch.choke == patch.choke) {
                // Fast fade (~10 ms) instead of a hard cut, to avoid clicks.
                v.patch.tone.3 = 0.01;
                v.patch.noise.1 = 0.01;
                v.patch.metal.1 = 0.01;
                v.patch.clap = false;
            }
        }
        let idx = self.voices.iter().position(|v| !v.active).unwrap_or_else(|| (0..VOICES).min_by_key(|&i| self.voices[i].age).unwrap_or(0));
        let vel = velocity as f32 / 127.0;
        let on = |level: f32| if level > 0.0 { 1.0 } else { 0.0 };
        self.voices[idx] = Voice {
            active: true,
            patch,
            velocity: vel * vel.sqrt(),
            age: self.clock,
            env_tone: on(patch.tone.4),
            env_noise: on(patch.noise.2),
            env_metal: on(patch.metal.2),
            ..Voice::IDLE
        };
    }

    fn white(&mut self) -> f32 {
        // xorshift32
        let mut x = self.noise;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.noise = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let sr = self.sample_rate;
        let dt = 1.0 / sr;
        let decay = decay_scale(self.values[0]);
        let tone = tone_scale(self.values[1]);
        let gain = 10f64.powf(level_db(self.values[2]) / 20.0) as f32 * 0.5;
        // Per-sample envelope multiplier for a decay time.
        let k = |secs: f32| (-dt / (secs * decay).max(0.002)).exp();

        for vi in 0..VOICES {
            if !self.voices[vi].active {
                continue;
            }
            let p = self.voices[vi].patch;
            let (kt, kn, km) = (k(p.tone.3), k(p.noise.1), k(p.metal.1));
            // SVF coefficients (Chamberlin), clamped for stability.
            let f_noise = (2.0 * (std::f32::consts::PI * (p.noise.0 * tone).min(sr * 0.2) / sr).sin()).min(1.2);
            let f_metal = (2.0 * (std::f32::consts::PI * (6000.0 * tone).min(sr * 0.2) / sr).sin()).min(1.2);
            let q = 0.9;
            for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                let n = self.white();
                let v = &mut self.voices[vi];
                let mut s = 0.0;
                if p.tone.4 > 0.0 {
                    let freq = (p.tone.1 + (p.tone.0 - p.tone.1) * (-v.t / p.tone.2).exp()) * tone;
                    v.phase = (v.phase + freq * dt).fract();
                    s += (v.phase * TAU).sin() * v.env_tone * p.tone.4;
                    v.env_tone *= kt;
                }
                if p.noise.2 > 0.0 {
                    let (low, band) = &mut v.svf_noise;
                    *low += f_noise * *band;
                    let high = n - *low - q * *band;
                    *band += f_noise * high;
                    let mut env = v.env_noise;
                    if p.clap && v.t < 0.03 {
                        env = 1.0 - (v.t % 0.01) / 0.01;
                    }
                    s += *band * 1.5 * env * p.noise.2;
                    v.env_noise *= kn;
                }
                if p.metal.2 > 0.0 {
                    let mut m = 0.0;
                    let freqs: &[f32] = if p.metal.0 == 0.0 { &[540.0, 800.0] } else { &METAL };
                    let mult = if p.metal.0 == 0.0 { tone } else { p.metal.0 * tone };
                    for (ph, f) in v.metal_phase.iter_mut().zip(freqs) {
                        *ph = (*ph + f * mult * dt).fract();
                        m += if *ph < 0.5 { 1.0 } else { -1.0 };
                    }
                    m /= freqs.len() as f32;
                    let out = if p.metal.0 == 0.0 {
                        m
                    } else {
                        let (low, band) = &mut v.svf_metal;
                        *low += f_metal * *band;
                        let high = m - *low - q * *band;
                        *band += f_metal * high;
                        high
                    };
                    s += out * v.env_metal * p.metal.2;
                    v.env_metal *= km;
                }
                let out = s * v.velocity * p.gain * gain;
                *a += out;
                *b += out;
                v.t += dt;
                if v.env_tone.max(v.env_noise).max(v.env_metal) < 1e-4 && v.t > 0.005 {
                    v.active = false;
                    break;
                }
            }
        }
    }
}

impl Processor for DrumSynth {
    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, events: &[MidiEvent]) {
        let [l, r] = outputs else { return };
        l[..frames].fill(0.0);
        r[..frames].fill(0.0);
        let mut at = 0usize;
        for e in events {
            let offset = (e.offset as usize).min(frames);
            if offset > at {
                self.render(&mut l[at..offset], &mut r[at..offset]);
                at = offset;
            }
            // One-shots: note-offs are ignored.
            if e.velocity > 0 {
                self.hit(e.pitch, e.velocity);
            }
        }
        if at < frames {
            self.render(&mut l[at..frames], &mut r[at..frames]);
        }
    }

    fn set_param(&mut self, id: u32, normalized: f64) {
        if let Some(v) = self.values.get_mut(id as usize) {
            *v = normalized.clamp(0.0, 1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &mut DrumSynth, events: &[MidiEvent], frames: usize) -> Vec<f32> {
        let (mut l, mut r) = (vec![0.0; frames], vec![0.0; frames]);
        {
            let mut outs: [&mut [f32]; 2] = [&mut l, &mut r];
            s.process(&[], &mut outs, frames, &Transport::default(), events);
        }
        l
    }

    fn peak(x: &[f32]) -> f32 {
        x.iter().fold(0.0f32, |m, v| m.max(v.abs()))
    }

    #[test]
    fn every_gm_drum_sounds_and_dies_away() {
        for key in 27..=87u8 {
            let mut s = DrumSynth::new(48_000.0);
            let hit = run(&mut s, &[MidiEvent { offset: 10, pitch: key, velocity: 120 }], 4800);
            assert!(hit[..10].iter().all(|&x| x == 0.0), "key {key}: silent before the hit");
            let p = peak(&hit);
            assert!(p > 0.01 && p < 2.0, "key {key}: peak {p}");
            assert!(hit.iter().all(|x| x.is_finite()));
            for _ in 0..200 {
                run(&mut s, &[], 4800);
            }
            assert!(peak(&run(&mut s, &[], 4800)) < 1e-3, "key {key}: voice should end");
            assert!(s.voices.iter().all(|v| !v.active), "key {key}: voice freed");
        }
    }

    #[test]
    fn closed_hat_chokes_open_hat() {
        let mut s = DrumSynth::new(48_000.0);
        run(&mut s, &[MidiEvent { offset: 0, pitch: 46, velocity: 120 }], 4800);
        run(&mut s, &[MidiEvent { offset: 0, pitch: 42, velocity: 1 }], 4800);
        // 0.2 s later the open hat (0.45 s decay) would still ring if not choked.
        let tail = run(&mut s, &[], 9600);
        assert!(peak(&tail[4800..]) < 0.01, "open hat choked: {}", peak(&tail[4800..]));
    }

    #[test]
    fn kick_is_low_and_loud_hats_are_bright() {
        let mut s = DrumSynth::new(48_000.0);
        let kick = run(&mut s, &[MidiEvent { offset: 0, pitch: 36, velocity: 127 }], 4800);
        let zero_crossings = |x: &[f32]| x.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let mut s = DrumSynth::new(48_000.0);
        let hat = run(&mut s, &[MidiEvent { offset: 0, pitch: 42, velocity: 127 }], 4800);
        assert!(zero_crossings(&kick) < 40, "kick crossings {}", zero_crossings(&kick));
        assert!(zero_crossings(&hat[..1200]) > 100, "hat crossings {}", zero_crossings(&hat[..1200]));
        assert!(peak(&kick) > 0.2);
    }
}
