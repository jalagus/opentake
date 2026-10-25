//! Built-in polyphonic synth, so instrument tracks work without any plugin installed.

use crate::mixer::{MidiEvent, Processor, Transport};

const VOICES: usize = 16;

/// Parameter ids for the built-in synth.
pub mod param {
    pub const WAVE: u32 = 0;
    pub const ATTACK: u32 = 1;
    pub const DECAY: u32 = 2;
    pub const SUSTAIN: u32 = 3;
    pub const RELEASE: u32 = 4;
    pub const LEVEL: u32 = 5;
}

/// (id, title, units, default normalized value, steps)
pub const PARAMS: [(u32, &str, &str, f64, i32); 6] = [
    (param::WAVE, "Waveform", "", 0.67, 3),
    (param::ATTACK, "Attack", "ms", 0.05, 0),
    (param::DECAY, "Decay", "ms", 0.3, 0),
    (param::SUSTAIN, "Sustain", "%", 0.7, 0),
    (param::RELEASE, "Release", "ms", 0.25, 0),
    (param::LEVEL, "Level", "dB", 0.8, 0),
];

pub const NAME: &str = "OpenTake Synth";
/// Earlier name of the built-in synth, still accepted.
pub const OLD_NAME: &str = "mcp-daw Synth";
pub const PATH: &str = "synth";

const WAVE_NAMES: [&str; 4] = ["Sine", "Triangle", "Saw", "Square"];

fn wave_index(v: f64) -> usize {
    ((v * 3.0).round() as usize).min(3)
}

/// Envelope times: 1 ms .. 4 s, skewed so the knob's lower half covers short times.
fn time_secs(v: f64) -> f64 {
    0.001 + v * v * 4.0
}

fn level_db(v: f64) -> f64 {
    -48.0 + v * 54.0 // -48 .. +6 dB
}

/// Human-readable value for the UI.
pub fn display(id: u32, v: f64) -> String {
    match id {
        param::WAVE => WAVE_NAMES[wave_index(v)].to_string(),
        param::ATTACK | param::DECAY | param::RELEASE => format!("{:.0}", time_secs(v) * 1000.0),
        param::SUSTAIN => format!("{:.0}", v * 100.0),
        param::LEVEL => format!("{:+.1}", level_db(v)),
        _ => format!("{v:.2}"),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Off,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Clone, Copy)]
struct Voice {
    stage: Stage,
    pitch: u8,
    velocity: f32,
    phase: f64,
    inc: f64,
    env: f32,
    age: u64,
}

impl Voice {
    const IDLE: Voice = Voice { stage: Stage::Off, pitch: 0, velocity: 0.0, phase: 0.0, inc: 0.0, env: 0.0, age: 0 };
}

pub struct Synth {
    sample_rate: f64,
    voices: [Voice; VOICES],
    values: [f64; 6],
    clock: u64,
}

impl Synth {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            sample_rate: sample_rate.max(1.0),
            voices: [Voice::IDLE; VOICES],
            values: PARAMS.map(|p| p.3),
            clock: 0,
        }
    }

    fn note_on(&mut self, pitch: u8, velocity: u8) {
        self.clock += 1;
        // Reuse a voice already playing this pitch, else a free one, else the oldest.
        let idx = self
            .voices
            .iter()
            .position(|v| v.stage != Stage::Off && v.pitch == pitch)
            .or_else(|| self.voices.iter().position(|v| v.stage == Stage::Off))
            .unwrap_or_else(|| (0..VOICES).min_by_key(|&i| self.voices[i].age).unwrap_or(0));
        let freq = 440.0 * 2f64.powf((pitch as f64 - 69.0) / 12.0);
        let v = &mut self.voices[idx];
        *v = Voice {
            stage: Stage::Attack,
            pitch,
            velocity: velocity as f32 / 127.0,
            phase: if v.stage == Stage::Off { 0.0 } else { v.phase },
            inc: freq / self.sample_rate,
            env: if v.stage == Stage::Off { 0.0 } else { v.env },
            age: self.clock,
        };
    }

    fn note_off(&mut self, pitch: u8) {
        for v in self.voices.iter_mut().filter(|v| v.pitch == pitch && v.stage != Stage::Off && v.stage != Stage::Release) {
            v.stage = Stage::Release;
        }
    }

    fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let sr = self.sample_rate;
        let attack = (1.0 / (time_secs(self.values[1]) * sr)) as f32;
        let decay = (1.0 / (time_secs(self.values[2]) * sr)) as f32;
        let sustain = self.values[3] as f32;
        let release = (1.0 / (time_secs(self.values[4]) * sr)) as f32;
        let gain = 10f64.powf(level_db(self.values[5]) / 20.0) as f32 * 0.25;
        let wave = wave_index(self.values[0]);

        for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Off) {
            for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                match v.stage {
                    Stage::Attack => {
                        v.env += attack;
                        if v.env >= 1.0 {
                            v.env = 1.0;
                            v.stage = Stage::Decay;
                        }
                    }
                    Stage::Decay => {
                        v.env -= decay * (1.0 - sustain).max(0.001);
                        if v.env <= sustain {
                            v.env = sustain;
                            v.stage = Stage::Sustain;
                        }
                    }
                    Stage::Sustain => {}
                    Stage::Release => {
                        v.env -= release;
                        if v.env <= 0.0 {
                            v.env = 0.0;
                            v.stage = Stage::Off;
                        }
                    }
                    Stage::Off => break,
                }
                let s = oscillator(wave, v.phase, v.inc) * v.env * v.velocity * gain;
                *a += s;
                *b += s;
                v.phase = (v.phase + v.inc).fract();
            }
        }
    }
}

/// One sample of the chosen waveform; saw and square use PolyBLEP to tame aliasing.
fn oscillator(wave: usize, phase: f64, inc: f64) -> f32 {
    let blep = |t: f64| -> f64 {
        if t < inc {
            let t = t / inc;
            t + t - t * t - 1.0
        } else if t > 1.0 - inc {
            let t = (t - 1.0) / inc;
            t * t + t + t + 1.0
        } else {
            0.0
        }
    };
    let s = match wave {
        0 => (phase * std::f64::consts::TAU).sin(),
        1 => 1.0 - 4.0 * (phase - 0.5).abs(),
        2 => (2.0 * phase - 1.0) - blep(phase),
        _ => {
            let sq = if phase < 0.5 { 1.0 } else { -1.0 };
            sq + blep(phase) - blep((phase + 0.5).fract())
        }
    };
    s as f32
}

impl Processor for Synth {
    fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize, _t: &Transport, events: &[MidiEvent]) {
        let [l, r] = outputs else { return };
        l[..frames].fill(0.0);
        r[..frames].fill(0.0);
        // Render in segments between events for sample-accurate timing.
        let mut at = 0usize;
        for e in events {
            let offset = (e.offset as usize).min(frames);
            if offset > at {
                self.render(&mut l[at..offset], &mut r[at..offset]);
                at = offset;
            }
            if e.velocity > 0 {
                self.note_on(e.pitch, e.velocity);
            } else {
                self.note_off(e.pitch);
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

    fn run(s: &mut Synth, events: &[MidiEvent], frames: usize) -> Vec<f32> {
        let (mut l, mut r) = (vec![0.0; frames], vec![0.0; frames]);
        {
            let mut outs: [&mut [f32]; 2] = [&mut l, &mut r];
            s.process(&[], &mut outs, frames, &Transport::default(), events);
        }
        l
    }

    #[test]
    fn plays_and_releases_notes() {
        let mut s = Synth::new(48_000.0);
        let silent = run(&mut s, &[], 256);
        assert!(silent.iter().all(|&x| x == 0.0));
        let on = run(&mut s, &[MidiEvent { offset: 100, pitch: 69, velocity: 127 }], 512);
        assert!(on[..100].iter().all(|&x| x == 0.0), "nothing before the note-on offset");
        assert!(on[100..].iter().any(|&x| x.abs() > 0.01), "sound after note-on");
        // After a note-off and the release time, the voice is silent again.
        run(&mut s, &[MidiEvent { offset: 0, pitch: 69, velocity: 0 }], 512);
        for _ in 0..200 {
            run(&mut s, &[], 512);
        }
        assert!(run(&mut s, &[], 512).iter().all(|&x| x.abs() < 1e-6));
    }

    #[test]
    fn displays_values() {
        assert_eq!(display(param::WAVE, 0.0), "Sine");
        assert_eq!(display(param::WAVE, 1.0), "Square");
        assert_eq!(display(param::SUSTAIN, 0.5), "50");
    }
}
