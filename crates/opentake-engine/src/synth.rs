//! Built-in polyphonic synth, so instrument tracks work without any plugin installed.
//!
//! Controllers: pitch bend (± the Bend Range parameter, 2 semitones by default), mod wheel
//! (CC1, vibrato up to ±half a semitone), volume (CC7) and expression (CC11) as gain (127 =
//! unity, squared like a fader), and the sustain pedal (CC64: released keys keep sounding
//! until it comes up).

use opentake_project::Controller;

use crate::mixer::{MAX_BLOCK, MidiData, MidiEvent, Processor, Transport};

const VOICES: usize = 16;

/// Parameter ids for the built-in synth.
pub mod param {
    pub const WAVE: u32 = 0;
    pub const ATTACK: u32 = 1;
    pub const DECAY: u32 = 2;
    pub const SUSTAIN: u32 = 3;
    pub const RELEASE: u32 = 4;
    pub const LEVEL: u32 = 5;
    pub const BEND_RANGE: u32 = 6;
}

/// Largest pitch-bend range (semitones), at the Bend Range parameter's top.
pub const MAX_BEND_RANGE: f64 = 24.0;
/// Vibrato rate (Hz) and depth (semitones) at full mod wheel.
const VIBRATO_HZ: f64 = 5.5;
const VIBRATO_DEPTH: f64 = 0.5;

/// The Bend Range parameter's normalized value for a range in semitones.
pub fn bend_range_value(semitones: f64) -> f64 {
    (semitones / MAX_BEND_RANGE).clamp(0.0, 1.0)
}

/// (id, title, units, default normalized value, steps)
pub const PARAMS: [(u32, &str, &str, f64, i32); 7] = [
    (param::WAVE, "Waveform", "", 0.67, 3),
    (param::ATTACK, "Attack", "ms", 0.05, 0),
    (param::DECAY, "Decay", "ms", 0.3, 0),
    (param::SUSTAIN, "Sustain", "%", 0.7, 0),
    (param::RELEASE, "Release", "ms", 0.25, 0),
    (param::LEVEL, "Level", "dB", 0.8, 0),
    (param::BEND_RANGE, "Bend Range", "st", 2.0 / MAX_BEND_RANGE, MAX_BEND_RANGE as i32),
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
        param::BEND_RANGE => format!("±{:.0}", v * MAX_BEND_RANGE),
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
    /// Let go while the sustain pedal is down: releases when it comes up.
    held: bool,
}

impl Voice {
    const IDLE: Voice = Voice { stage: Stage::Off, pitch: 0, velocity: 0.0, phase: 0.0, inc: 0.0, env: 0.0, age: 0, held: false };
}

pub struct Synth {
    sample_rate: f64,
    voices: [Voice; VOICES],
    values: [f64; 7],
    clock: u64,
    /// Pitch bend -1..1 (of the bend range), mod wheel, volume and expression 0..1, pedal.
    bend: f64,
    modulation: f64,
    volume: f32,
    expression: f32,
    sustain: bool,
    lfo: f64,
    /// Per-sample pitch ratio (bend and vibrato) of the segment being rendered.
    ratio: Vec<f64>,
}

impl Synth {
    pub fn new(sample_rate: f64) -> Self {
        Self {
            sample_rate: sample_rate.max(1.0),
            voices: [Voice::IDLE; VOICES],
            values: PARAMS.map(|p| p.3),
            clock: 0,
            bend: 0.0,
            modulation: 0.0,
            volume: 1.0,
            expression: 1.0,
            sustain: false,
            lfo: 0.0,
            ratio: vec![1.0; MAX_BLOCK],
        }
    }

    fn control(&mut self, kind: Controller, value: u16) {
        let v = kind.from_raw(value) as f64;
        match kind {
            Controller::PitchBend => self.bend = v,
            Controller::MOD_WHEEL => self.modulation = v,
            Controller::VOLUME => self.volume = v as f32,
            Controller::EXPRESSION => self.expression = v as f32,
            Controller::SUSTAIN => {
                self.sustain = value >= 64;
                if !self.sustain {
                    for v in self.voices.iter_mut().filter(|v| v.held) {
                        v.held = false;
                        if v.stage != Stage::Off {
                            v.stage = Stage::Release;
                        }
                    }
                }
            }
            _ => {}
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
            held: false,
        };
    }

    fn note_off(&mut self, pitch: u8) {
        let sustain = self.sustain;
        for v in self.voices.iter_mut().filter(|v| v.pitch == pitch && v.stage != Stage::Off && v.stage != Stage::Release) {
            if sustain {
                v.held = true;
            } else {
                v.stage = Stage::Release;
            }
        }
    }

    /// Render in pieces no longer than the pitch-ratio buffer.
    fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let chunk = self.ratio.len();
        for (l, r) in l.chunks_mut(chunk).zip(r.chunks_mut(chunk)) {
            self.render_chunk(l, r);
        }
    }

    fn render_chunk(&mut self, l: &mut [f32], r: &mut [f32]) {
        let sr = self.sample_rate;
        // Pitch bend and vibrato: one ratio per sample, shared by every voice.
        let bend = self.bend * self.values[param::BEND_RANGE as usize] * MAX_BEND_RANGE;
        let ratio = &mut self.ratio[..l.len()];
        if self.modulation > 0.0 {
            let (step, depth) = (VIBRATO_HZ / sr, self.modulation * VIBRATO_DEPTH);
            for x in ratio.iter_mut() {
                *x = ((bend + depth * (self.lfo * std::f64::consts::TAU).sin()) / 12.0).exp2();
                self.lfo = (self.lfo + step).fract();
            }
        } else {
            ratio.fill((bend / 12.0).exp2());
        }
        let attack = (1.0 / (time_secs(self.values[1]) * sr)) as f32;
        let decay = (1.0 / (time_secs(self.values[2]) * sr)) as f32;
        let sustain = self.values[3] as f32;
        let release = (1.0 / (time_secs(self.values[4]) * sr)) as f32;
        let cc = self.volume * self.expression;
        let gain = 10f64.powf(level_db(self.values[5]) / 20.0) as f32 * 0.25 * cc * cc;
        let wave = wave_index(self.values[0]);

        for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Off) {
            for ((a, b), &ratio) in l.iter_mut().zip(r.iter_mut()).zip(ratio.iter()) {
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
                let inc = v.inc * ratio;
                let s = oscillator(wave, v.phase, inc) * v.env * v.velocity * gain;
                *a += s;
                *b += s;
                v.phase = (v.phase + inc).fract();
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
            match e.data {
                MidiData::Note { pitch, velocity: 0 } => self.note_off(pitch),
                MidiData::Note { pitch, velocity } => self.note_on(pitch, velocity),
                MidiData::Control { kind, value } => self.control(kind, value),
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
        let on = run(&mut s, &[MidiEvent::note(100, 69, 127)], 512);
        assert!(on[..100].iter().all(|&x| x == 0.0), "nothing before the note-on offset");
        assert!(on[100..].iter().any(|&x| x.abs() > 0.01), "sound after note-on");
        // After a note-off and the release time, the voice is silent again.
        run(&mut s, &[MidiEvent::note(0, 69, 0)], 512);
        for _ in 0..200 {
            run(&mut s, &[], 512);
        }
        assert!(run(&mut s, &[], 512).iter().all(|&x| x.abs() < 1e-6));
    }

    /// Sine at A3 (220 Hz), then `frames` with `events` at the start; the frequency measured
    /// by counting rising zero crossings over the last part, and the spread of the per-cycle
    /// lengths (vibrato).
    fn measure(s: &mut Synth, events: &[MidiEvent], frames: usize) -> (f64, f64) {
        let out = run(s, events, frames);
        let tail = &out[frames / 4..];
        let rises: Vec<usize> = (1..tail.len()).filter(|&i| tail[i - 1] < 0.0 && tail[i] >= 0.0).collect();
        let periods: Vec<f64> = rises.windows(2).map(|w| (w[1] - w[0]) as f64).collect();
        let freq = 48_000.0 * (rises.len() - 1) as f64 / (rises[rises.len() - 1] - rises[0]) as f64;
        let (lo, hi) = periods.iter().fold((f64::MAX, 0.0f64), |(lo, hi), &p| (lo.min(p), hi.max(p)));
        (freq, hi - lo)
    }

    fn sine() -> Synth {
        let mut s = Synth::new(48_000.0);
        s.set_param(param::WAVE, 0.0);
        run(&mut s, &[MidiEvent::note(0, 57, 127)], 48_000); // past attack and decay
        s
    }

    #[test]
    fn pitch_bend_shifts_the_frequency_by_the_bend_range() {
        let mut s = sine();
        let (f, _) = measure(&mut s, &[], 48_000);
        assert!((f - 220.0).abs() < 0.5, "{f}");
        // Full bend up: +2 semitones by default.
        let (f, _) = measure(&mut s, &[MidiEvent::control(0, Controller::PitchBend, 16383)], 48_000);
        assert!((f - 220.0 * 2f64.powf(2.0 / 12.0)).abs() < 0.5, "{f}");
        // Half way down with a 12-semitone range: -6.
        s.set_param(param::BEND_RANGE, bend_range_value(12.0));
        let (f, _) = measure(&mut s, &[MidiEvent::control(0, Controller::PitchBend, 4096)], 48_000);
        assert!((f - 220.0 * 2f64.powf(-6.0 / 12.0)).abs() < 0.5, "{f}");
        assert_eq!(display(param::BEND_RANGE, bend_range_value(12.0)), "±12");
    }

    #[test]
    fn mod_wheel_adds_vibrato() {
        let mut s = sine();
        let (_, steady) = measure(&mut s, &[], 48_000);
        let (f, wobble) = measure(&mut s, &[MidiEvent::control(0, Controller::MOD_WHEEL, 127)], 48_000);
        assert!(steady <= 1.0, "no vibrato: cycles within a sample of each other ({steady})");
        assert!(wobble >= 4.0, "vibrato varies the cycle length ({wobble})");
        assert!((f - 220.0).abs() < 2.0, "around the same pitch: {f}");
    }

    #[test]
    fn volume_and_expression_scale_the_output() {
        let mut s = sine();
        let peak = |s: &mut Synth, e: &[MidiEvent]| run(s, e, 4800).iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let full = peak(&mut s, &[]);
        let half = peak(&mut s, &[MidiEvent::control(0, Controller::VOLUME, 64)]);
        assert!((half / full - 0.254).abs() < 0.02, "{}", half / full);
        let quiet = peak(&mut s, &[MidiEvent::control(0, Controller::EXPRESSION, 0)]);
        assert!(quiet < 1e-6);
    }

    #[test]
    fn sustain_pedal_holds_released_notes() {
        let mut s = Synth::new(48_000.0);
        let loud = |s: &mut Synth| run(s, &[], 512).iter().any(|x| x.abs() > 0.01);
        run(&mut s, &[MidiEvent::control(0, Controller::SUSTAIN, 127), MidiEvent::note(0, 60, 127), MidiEvent::note(256, 60, 0)], 512);
        for _ in 0..200 {
            run(&mut s, &[], 512);
        }
        assert!(loud(&mut s), "the pedal holds the note past its release");
        run(&mut s, &[MidiEvent::control(0, Controller::SUSTAIN, 0)], 512);
        for _ in 0..200 {
            run(&mut s, &[], 512);
        }
        assert!(!loud(&mut s), "and lets go when it comes up");
    }

    #[test]
    fn displays_values() {
        assert_eq!(display(param::WAVE, 0.0), "Sine");
        assert_eq!(display(param::WAVE, 1.0), "Square");
        assert_eq!(display(param::SUSTAIN, 0.5), "50");
    }
}
