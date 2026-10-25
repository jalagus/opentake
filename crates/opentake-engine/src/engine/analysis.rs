//! Offline mix analysis: renders the project (MIDI through the built-in instruments, audio
//! regions, built-in EQs, faders, pans, master) and measures each track and the master:
//! peak, integrated loudness (ITU BS.1770, gated), energy per frequency band, and how much
//! loudness survives a phone speaker. Lets an agent (or a person) balance a mix by numbers.
//!
//! [`Engine::mix_job`] takes a snapshot under the engine lock; [`MixJob::analyze`] does the
//! heavy work without it, so the GUI keeps running.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use opentake_project::{PluginFormat, PluginSlot, Project, TempoMap, Track};
use serde::Serialize;

use super::Engine;
use crate::delay::{self, Delay};
use crate::drums::{self, DrumSynth};
use crate::dynamics::{Compressor, Limiter, comp, limiter};
use crate::eq::{self, Eq};
use crate::media::AudioData;
use crate::mixer::{MidiEvent, Processor, Transport};
use crate::synth::{self, Synth};

const SR: f64 = 48_000.0;
const BLOCK: usize = 512;

/// Frequency bands reported (label, low Hz, high Hz).
pub const BANDS: [(&str, f64, f64); 7] = [
    ("sub <50", 20.0, 50.0),
    ("bass 50-120", 50.0, 120.0),
    ("low-mid 120-300", 120.0, 300.0),
    ("mid 300-1k", 300.0, 1000.0),
    ("upper-mid 1-3k", 1000.0, 3000.0),
    ("presence 3-7k", 3000.0, 7000.0),
    ("air 7k+", 7000.0, 20_000.0),
];

#[derive(Debug, Clone, Serialize)]
pub struct LevelReport {
    pub name: String,
    /// Sample peak after fader/pan (dBFS).
    pub peak_db: f64,
    /// Integrated loudness, BS.1770 gated (LUFS).
    pub lufs: f64,
    /// Average power per band (dB, relative; compare across tracks and bands).
    pub bands: Vec<(String, f64)>,
    /// Loudness through a simulated phone speaker (high-passed at 300 Hz, low-passed at 12 kHz).
    pub phone_lufs: f64,
    /// How much loudness is lost on a phone (LU); similar drops across tracks keep the balance.
    pub phone_drop: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MixReport {
    pub from_secs: f64,
    pub to_secs: f64,
    pub tracks: Vec<LevelReport>,
    pub master: LevelReport,
    /// Master samples above 0 dBFS (clipping).
    pub clipped_samples: usize,
    /// What couldn't be rendered offline (e.g. VST3 plug-ins), so the numbers exclude it.
    pub warnings: Vec<String>,
}

/// Everything needed to render the mix, taken from the engine.
pub struct MixJob {
    project: Project,
    media: Vec<(PathBuf, Arc<AudioData>)>,
    warnings: Vec<String>,
}

impl Engine {
    /// Snapshot the project (and load its media) for [`MixJob::analyze`].
    pub fn mix_job(&mut self) -> MixJob {
        let mut media = Vec::new();
        let mut warnings = Vec::new();
        let files: Vec<PathBuf> = self.project.tracks.iter().flat_map(|t| &t.clips).map(|c| c.file.clone()).collect();
        for file in files {
            if media.iter().any(|(f, _)| *f == file) {
                continue;
            }
            match self.load_media(&file, SR as u32) {
                Ok(d) => media.push((file, d)),
                Err(e) => warnings.push(format!("{}: {e:#}", file.display())),
            }
        }
        MixJob { project: self.project.clone(), media, warnings }
    }

    /// Analyze the mix (convenience for callers that don't mind holding the engine).
    pub fn analyze_mix(&mut self, from: Option<f64>, to: Option<f64>) -> Result<MixReport> {
        self.mix_job().analyze(from, to)
    }
}

impl MixJob {
    /// Render `from..to` seconds (default: the whole project) and measure it.
    pub fn analyze(mut self, from: Option<f64>, to: Option<f64>) -> Result<MixReport> {
        let end = self.project.length_secs();
        let from = from.unwrap_or(0.0).clamp(0.0, end);
        let to = to.unwrap_or(end + 1.0).max(from + 0.5);
        let (start, len) = ((from * SR) as usize, ((to - from) * SR) as usize);
        let any_solo = self.project.tracks.iter().any(|t| t.solo);
        let map = self.project.tempo_map();
        let mut mix = [vec![0.0f32; len], vec![0.0f32; len]];
        let mut tracks = Vec::new();
        for track in &self.project.tracks {
            let mono = render_track(track, start, len, &self.media, &mut self.warnings, &map);
            let (gl, gr) = pan_gains(track.volume, track.pan);
            let l: Vec<f32> = mono.iter().map(|v| v * gl).collect();
            let r: Vec<f32> = mono.iter().map(|v| v * gr).collect();
            if !track.mute && (!any_solo || track.solo) {
                for i in 0..len {
                    mix[0][i] += l[i];
                    mix[1][i] += r[i];
                }
            }
            tracks.push(measure(&track.name, &l, &r));
        }
        for slot in &self.project.master_plugins {
            if slot.bypass {
                continue;
            }
            match builtin_effect(slot) {
                Some(mut fx) => {
                    let [l, r] = &mut mix;
                    process_stereo(fx.as_mut(), l, r, &map, start);
                }
                None => self.warnings.push(format!("master: '{}' isn't rendered offline (only built-in effects are)", slot.name)),
            }
        }
        let g = self.project.master_volume;
        for ch in &mut mix {
            for s in ch.iter_mut() {
                *s *= g;
            }
        }
        let clipped_samples = mix.iter().flatten().filter(|v| v.abs() > 1.0).count();
        let master = measure("Master", &mix[0], &mix[1]);
        Ok(MixReport { from_secs: from, to_secs: to, tracks, master, clipped_samples, warnings: self.warnings })
    }
}

/// Linear balance pan, as the mixer does.
fn pan_gains(volume: f32, pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    (volume * (1.0 - pan).min(1.0), volume * (1.0 + pan).min(1.0))
}

fn builtin_instrument(slot: &PluginSlot) -> Option<Box<dyn Processor>> {
    if slot.format != PluginFormat::Builtin {
        return None;
    }
    let mut p: Box<dyn Processor> = match slot.path.as_str() {
        synth::PATH => Box::new(Synth::new(SR)),
        drums::PATH => Box::new(DrumSynth::new(SR)),
        _ => return None,
    };
    for &(id, v) in &slot.params {
        p.set_param(id, v);
    }
    Some(p)
}

fn builtin_effect(slot: &PluginSlot) -> Option<Box<dyn Processor>> {
    if slot.format != PluginFormat::Builtin {
        return None;
    }
    let mut p: Box<dyn Processor> = match slot.path.as_str() {
        eq::PATH => Box::new(Eq::new(SR)),
        comp::PATH => Box::new(Compressor::new(SR, None)),
        limiter::PATH => Box::new(Limiter::new(SR, None)),
        delay::PATH => Box::new(Delay::new(SR)),
        _ => return None,
    };
    for &(id, v) in &slot.params {
        p.set_param(id, v);
    }
    Some(p)
}

/// Run an effect over a whole stereo buffer (starting `offset` samples into the song, for
/// the tempo), then shift the result back by the effect's latency, as the mixer's
/// delay compensation does.
fn process_stereo(p: &mut dyn Processor, l: &mut [f32], r: &mut [f32], map: &TempoMap, offset: usize) {
    let (mut ol, mut or) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    for start in (0..l.len()).step_by(BLOCK) {
        let n = BLOCK.min(l.len() - start);
        let t = Transport { tempo_bpm: map.tempo_at((offset + start) as f64 / SR), ..Transport::default() };
        {
            let inputs: [&[f32]; 2] = [&l[start..start + n], &r[start..start + n]];
            let mut outputs: [&mut [f32]; 2] = [&mut ol[..n], &mut or[..n]];
            p.process(&inputs, &mut outputs, n, &t, &[]);
        }
        l[start..start + n].copy_from_slice(&ol[..n]);
        r[start..start + n].copy_from_slice(&or[..n]);
    }
    let latency = p.latency().min(l.len());
    if latency > 0 {
        for ch in [l, r] {
            ch.copy_within(latency.., 0);
            let len = ch.len();
            ch[len - latency..].fill(0.0);
        }
    }
}

/// One track before fader/pan (mono sum of its stereo output).
fn render_track(track: &Track, start: usize, len: usize, media: &[(PathBuf, Arc<AudioData>)], warnings: &mut Vec<String>, map: &TempoMap) -> Vec<f32> {
    let (mut l, mut r) = (vec![0.0f32; len], vec![0.0f32; len]);
    // Audio regions.
    for clip in track.clips.iter().filter(|c| !c.mute) {
        let Some((_, data)) = media.iter().find(|(f, _)| *f == clip.file) else { continue };
        let clip_start = (clip.position_secs * SR) as i64;
        let offset = (clip.source_offset_secs * SR) as i64;
        let clip_len = (clip.length_secs * SR) as i64;
        let src_l = &data.channels[0];
        let src_r = data.channels.get(1).unwrap_or(src_l);
        for i in 0..len as i64 {
            let rel = start as i64 + i - clip_start;
            if rel < 0 || rel >= clip_len {
                continue;
            }
            let src = (offset + rel) as usize;
            if src >= src_l.len() {
                break;
            }
            l[i as usize] += src_l[src] * clip.gain;
            r[i as usize] += src_r[src] * clip.gain;
        }
    }
    // MIDI through the instrument (rendered from the song start so voices are in the
    // right state at `start`, then trimmed).
    if let Some(slot) = &track.instrument {
        match builtin_instrument(slot) {
            Some(mut inst) if !slot.bypass => {
                let total = start + len;
                let mut events: Vec<(usize, u8, u8)> = Vec::new();
                for c in track.midi_clips.iter().filter(|c| !c.mute) {
                    for n in c.notes.iter().filter(|n| n.start_secs < c.length_secs) {
                        let on = ((c.position_secs + n.start_secs) * SR) as usize;
                        let off = ((c.position_secs + (n.start_secs + n.length_secs).min(c.length_secs)) * SR) as usize;
                        events.push((on, n.pitch, n.velocity.max(1)));
                        events.push((off, n.pitch, 0));
                    }
                }
                events.sort_by_key(|e| (e.0, e.2 != 0));
                let silence = vec![0.0f32; BLOCK];
                let (mut ol, mut or) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
                let t = Transport::default();
                let mut ei = 0;
                let mut pos = 0;
                while pos < total {
                    let n = BLOCK.min(total - pos);
                    let mut block = Vec::new();
                    while ei < events.len() && events[ei].0 < pos + n {
                        block.push(MidiEvent { offset: (events[ei].0.saturating_sub(pos)) as u32, pitch: events[ei].1, velocity: events[ei].2 });
                        ei += 1;
                    }
                    {
                        let inputs: [&[f32]; 2] = [&silence[..n], &silence[..n]];
                        let mut outputs: [&mut [f32]; 2] = [&mut ol[..n], &mut or[..n]];
                        inst.process(&inputs, &mut outputs, n, &t, &block);
                    }
                    for k in 0..n {
                        if pos + k >= start {
                            l[pos + k - start] += ol[k];
                            r[pos + k - start] += or[k];
                        }
                    }
                    pos += n;
                }
            }
            Some(_) => {}
            None => warnings.push(format!("track '{}': instrument '{}' isn't rendered offline (only built-in instruments are)", track.name, slot.name)),
        }
    }
    for slot in track.plugins.iter().filter(|s| !s.bypass) {
        match builtin_effect(slot) {
            Some(mut fx) => process_stereo(fx.as_mut(), &mut l, &mut r, map, start),
            None => warnings.push(format!("track '{}': '{}' isn't rendered offline (only built-in effects are)", track.name, slot.name)),
        }
    }
    l.iter().zip(&r).map(|(a, b)| (a + b) * 0.5).collect()
}

struct Biquad {
    b: [f64; 3],
    a: [f64; 3],
    z: [f64; 2],
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 3]) -> Self {
        Self { b, a, z: [0.0; 2] }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[1] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[2] * y;
        y
    }

    fn pass(freq: f64, high: bool) -> Self {
        let w = std::f64::consts::TAU * freq / SR;
        let (c, s) = (w.cos(), w.sin());
        let alpha = s / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b = if high { [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0] } else { [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0] };
        Self::new([b[0] / a0, b[1] / a0, b[2] / a0], [1.0, -2.0 * c / a0, (1.0 - alpha) / a0])
    }

    /// The two BS.1770 K-weighting stages at 48 kHz.
    fn k_weighting() -> [Self; 2] {
        [
            Self::new([1.53512485958697, -2.69169618940638, 1.19839281085285], [1.0, -1.69065929318241, 0.73248077421585]),
            Self::new([1.0, -2.0, 1.0], [1.0, -1.99004745483398, 0.99007225036621]),
        ]
    }
}

/// Integrated loudness (BS.1770-4: 400 ms blocks, 75 % overlap, absolute and relative gates).
fn lufs(l: &[f32], r: &[f32]) -> f64 {
    let weigh = |x: &[f32]| {
        let [mut a, mut b] = Biquad::k_weighting();
        x.iter().map(|&v| b.run(a.run(v as f64))).collect::<Vec<f64>>()
    };
    let (kl, kr) = (weigh(l), weigh(r));
    let win = (0.4 * SR) as usize;
    let hop = win / 4;
    let mut blocks = Vec::new();
    let mut i = 0;
    while i + win <= kl.len() {
        blocks.push((i..i + win).map(|j| kl[j] * kl[j] + kr[j] * kr[j]).sum::<f64>() / win as f64);
        i += hop;
    }
    let loud = |ms: f64| -0.691 + 10.0 * ms.max(1e-20).log10();
    let above: Vec<f64> = blocks.into_iter().filter(|&m| loud(m) > -70.0).collect();
    if above.is_empty() {
        return -120.0;
    }
    let relative = loud(above.iter().sum::<f64>() / above.len() as f64) - 10.0;
    let gated: Vec<f64> = above.into_iter().filter(|&m| loud(m) > relative).collect();
    loud(gated.iter().sum::<f64>() / gated.len().max(1) as f64)
}

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = -std::f64::consts::TAU / len as f64;
        let (wr, wi) = (angle.cos(), angle.sin());
        for s in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (a, b) = (s + k, s + k + len / 2);
                let (xr, xi) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - xr;
                im[b] = im[a] - xi;
                re[a] += xr;
                im[a] += xi;
                let next = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = next;
            }
        }
        len <<= 1;
    }
}

/// Average power per band in dB (Hann-windowed FFTs over windows spread through the signal).
fn band_powers(x: &[f32]) -> Vec<f64> {
    let n = 4096;
    let mut acc = vec![0.0; BANDS.len()];
    let mut count = 0;
    let mut i = 0;
    while i + n <= x.len() {
        let mut re: Vec<f64> = (0..n).map(|k| x[i + k] as f64 * (0.5 - 0.5 * (std::f64::consts::TAU * k as f64 / n as f64).cos())).collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        for k in 1..n / 2 {
            let f = k as f64 * SR / n as f64;
            if let Some(b) = BANDS.iter().position(|b| f >= b.1 && f < b.2) {
                acc[b] += re[k] * re[k] + im[k] * im[k];
            }
        }
        count += 1;
        i += n * 4; // every 4th window is plenty for averages
    }
    acc.iter().map(|&p| 10.0 * (p / count.max(1) as f64).max(1e-20).log10()).collect()
}

fn measure(name: &str, l: &[f32], r: &[f32]) -> LevelReport {
    let peak = l.iter().chain(r).fold(0.0f32, |m, v| m.max(v.abs()));
    let mono: Vec<f32> = l.iter().zip(r).map(|(a, b)| (a + b) * 0.5).collect();
    let bands = band_powers(&mono).into_iter().zip(BANDS).map(|(db, b)| (b.0.to_string(), round1(db))).collect();
    // Phone speaker: little below ~300 Hz, rolled off above 12 kHz (mono).
    let (mut h1, mut h2, mut lp) = (Biquad::pass(300.0, true), Biquad::pass(300.0, true), Biquad::pass(12_000.0, false));
    let phone: Vec<f32> = mono.iter().map(|&v| lp.run(h2.run(h1.run(v as f64))) as f32).collect();
    let full_mono = lufs(&mono, &mono);
    let phone_lufs = lufs(&phone, &phone);
    LevelReport {
        name: name.to_string(),
        peak_db: round1(20.0 * (peak as f64).max(1e-9).log10()),
        lufs: round1(lufs(l, r)),
        bands,
        phone_lufs: round1(phone_lufs),
        phone_drop: round1(phone_lufs - full_mono),
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::super::Engine;
    use crate::eq::{Field, norm_freq, param_id};
    use opentake_project::Note;

    #[test]
    fn measures_tracks_and_master_and_follows_faders_and_eq() {
        let mut e = Engine::new();
        let t = e.add_instrument_track("Keys", "synth").unwrap();
        let c = e.add_midi_clip(t, 0.0, 4.0, None).unwrap();
        let notes = (0..8).map(|i| Note { start_secs: i as f64 * 0.5, length_secs: 0.45, pitch: 45, velocity: 100, tab: None }).collect();
        e.set_midi_notes(t, c, notes).unwrap();

        let r = e.analyze_mix(None, None).unwrap();
        assert_eq!(r.tracks.len(), 1);
        let before = r.tracks[0].clone();
        assert!(before.lufs > -40.0 && before.lufs < 0.0, "{before:?}");
        assert!(before.peak_db < 0.0);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);

        // -6 dB on the fader: about 6 LU quieter.
        e.update_track(t, crate::TrackUpdate { volume: Some(0.5), ..Default::default() }).unwrap();
        let r = e.analyze_mix(None, None).unwrap();
        assert!((before.lufs - r.tracks[0].lufs - 6.0).abs() < 0.3, "{} -> {}", before.lufs, r.tracks[0].lufs);

        // A low cut at 400 Hz removes the 110 Hz note's fundamental: less bass band energy.
        e.insert_plugin(t, "eq", None).unwrap();
        e.set_plugin_param(t, 0, param_id(0, Field::On), 1.0).unwrap();
        e.set_plugin_param(t, 0, param_id(0, Field::Freq), norm_freq(400.0)).unwrap();
        let after = e.analyze_mix(None, None).unwrap();
        let bass = |r: &super::LevelReport| r.bands[1].1;
        assert!(bass(&after.tracks[0]) < bass(&r.tracks[0]) - 10.0);
        assert!(after.master.lufs < r.master.lufs);
        assert_eq!(after.clipped_samples, 0);

        // A limiter on the master holds the peak at its ceiling even with +18 dB of drive.
        e.set_master_volume(1.0).unwrap();
        e.insert_master_plugin("limiter", None).unwrap();
        let m = super::super::SlotRef::Master(0);
        e.set_slot_param(0, m, crate::dynamics::limiter::GAIN, crate::dynamics::norm_input_gain(18.0)).unwrap();
        e.set_slot_param(0, m, crate::dynamics::limiter::CEILING, crate::dynamics::norm_ceiling(-1.0)).unwrap();
        let limited = e.analyze_mix(None, None).unwrap();
        assert!(limited.master.peak_db <= -0.99, "{}", limited.master.peak_db);
        assert!(limited.master.lufs > after.master.lufs + 6.0, "louder: {} vs {}", limited.master.lufs, after.master.lufs);
        assert!(limited.warnings.is_empty(), "{:?}", limited.warnings);
    }
}
