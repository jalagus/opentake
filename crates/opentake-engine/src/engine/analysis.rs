//! Offline rendering and mix analysis. [`Engine::mix_job`] snapshots the project under the
//! engine lock — media at the render rate and a separate processor for every instrument
//! and effect (built-ins created fresh, VST3s as new instances), so the live audio thread is
//! never touched. [`MixJob::render`] then renders without the lock: MIDI through the
//! instruments, audio regions, effect chains (with plug-in delay compensation), faders,
//! pans, buses and sends, the master and all their automation. Analysis measures the result (peak, BS.1770 loudness, energy per
//! band, phone-speaker translation); export writes it to files (see `export.rs`).

use std::sync::Arc;

use anyhow::Result;
use opentake_project::automation::pos_to_gain;
use opentake_project::{AutomationLane, AutomationTarget, Clip, Project, TempoMap, automated, effective_fades};
use serde::Serialize;

use super::Engine;
use super::flex::{self, FlexKey};
use crate::media::AudioData;
use crate::mixer::{AUTOMATION_BLOCK, ClipFades, MidiEvent, Processor, Transport};

/// Analysis render rate.
const ANALYSIS_RATE: f64 = 48_000.0;
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
    /// What couldn't be rendered (e.g. a missing plug-in), so the numbers exclude it.
    pub warnings: Vec<String>,
}

/// A plug-in's offline processor and its slot id (for its automation).
type SlotProcessor = (u32, Box<dyn Processor>);

/// A track's offline processors.
struct TrackProcessors {
    instrument: Option<SlotProcessor>,
    effects: Vec<SlotProcessor>,
}

/// Everything needed to render the mix, taken from the engine.
pub struct MixJob {
    pub(super) project: Project,
    sample_rate: f64,
    /// Each region's audio (pitch-shifted/stretched as set), by file and settings.
    media: Vec<(FlexKey, Arc<AudioData>)>,
    tracks: Vec<TrackProcessors>,
    master: Vec<SlotProcessor>,
    warnings: Vec<String>,
}

/// The rendered audio: each track after its fader and pan, and the master.
pub struct Rendered {
    pub sample_rate: f64,
    pub from_secs: f64,
    /// (track name, [left, right], audible in the mix).
    pub tracks: Vec<(String, [Vec<f32>; 2], bool)>,
    pub master: [Vec<f32>; 2],
    pub warnings: Vec<String>,
}

impl Engine {
    /// Snapshot the project for offline rendering at `sample_rate`.
    pub fn mix_job_at(&mut self, sample_rate: f64) -> MixJob {
        let sr = sample_rate.clamp(8_000.0, 384_000.0);
        let mut warnings = Vec::new();
        let mut media = Vec::new();
        let clips: Vec<_> = self.project.tracks.iter().flat_map(|t| &t.clips).cloned().collect();
        for clip in clips {
            let key = flex::key(&clip, sr as u32);
            if media.iter().any(|(k, _)| *k == key) {
                continue;
            }
            match self.clip_media(&clip, sr as u32) {
                Ok(d) => media.push((key, d)),
                Err(e) => warnings.push(format!("{}: {e:#}", clip.file.display())),
            }
        }
        let project = self.project.clone();
        let mut tracks = Vec::new();
        for t in &project.tracks {
            let instrument = match t.instrument.as_ref().filter(|s| !s.bypass) {
                Some(slot) => match self.offline_processor(slot, sr) {
                    Ok(p) => Some((slot.id, p)),
                    Err(e) => {
                        warnings.push(format!("track '{}': instrument '{}' not rendered: {e:#}", t.name, slot.name));
                        None
                    }
                },
                None => None,
            };
            let mut effects = Vec::new();
            for slot in t.plugins.iter().filter(|s| !s.bypass) {
                match self.offline_processor(slot, sr) {
                    Ok(p) => effects.push((slot.id, p)),
                    Err(e) => warnings.push(format!("track '{}': '{}' not rendered: {e:#}", t.name, slot.name)),
                }
            }
            tracks.push(TrackProcessors { instrument, effects });
        }
        let mut master = Vec::new();
        for slot in project.master_plugins.iter().filter(|s| !s.bypass) {
            match self.offline_processor(slot, sr) {
                Ok(p) => master.push((slot.id, p)),
                Err(e) => warnings.push(format!("master: '{}' not rendered: {e:#}", slot.name)),
            }
        }
        MixJob { project, sample_rate: sr, media, tracks, master, warnings }
    }

    /// Snapshot for analysis (48 kHz).
    pub fn mix_job(&mut self) -> MixJob {
        self.mix_job_at(ANALYSIS_RATE)
    }

    /// Analyze the mix (convenience for callers that don't mind holding the engine).
    pub fn analyze_mix(&mut self, from: Option<f64>, to: Option<f64>) -> Result<MixReport> {
        self.mix_job().analyze(from, to)
    }
}

impl MixJob {
    /// Render `from..to` seconds (default: the whole project) and measure it.
    pub fn analyze(self, from: Option<f64>, to: Option<f64>) -> Result<MixReport> {
        let r = self.render(from, to, 0.0);
        let sr = r.sample_rate;
        let tracks = r.tracks.iter().map(|(name, [l, rr], _)| measure(name, l, rr, sr)).collect();
        let clipped_samples = r.master.iter().flatten().filter(|v| v.abs() > 1.0).count();
        let master = measure("Master", &r.master[0], &r.master[1], sr);
        let len = r.master[0].len() as f64 / sr;
        Ok(MixReport { from_secs: r.from_secs, to_secs: r.from_secs + len, tracks, master, clipped_samples, warnings: r.warnings })
    }

    /// The project's end (seconds).
    pub fn length_secs(&self) -> f64 {
        self.project.length_secs()
    }

    /// The cycle range, if one is set.
    pub fn cycle(&self) -> Option<(f64, f64)> {
        let p = &self.project;
        (p.loop_end_secs > p.loop_start_secs).then_some((p.loop_start_secs, p.loop_end_secs))
    }

    pub fn project_name(&self) -> &str {
        &self.project.name
    }

    /// Render `from..to` seconds (default: the whole project) plus `tail` seconds.
    pub fn render(mut self, from: Option<f64>, to: Option<f64>, tail: f64) -> Rendered {
        let sr = self.sample_rate;
        let end = self.project.length_secs();
        let from = from.unwrap_or(0.0).clamp(0.0, end.max(0.0));
        let to = to.unwrap_or(end).max(from + 0.1) + tail.max(0.0);
        let (start, len) = ((from * sr) as usize, ((to - from) * sr) as usize);
        let audible = self.project.audible();
        let map = self.project.tempo_map();
        let silence = || [vec![0.0f32; len], vec![0.0f32; len]];
        let mut mix = silence();
        // Tracks before the buses they feed; a bus starts from the sum of its inputs.
        let mut procs: Vec<Option<TrackProcessors>> = std::mem::take(&mut self.tracks).into_iter().map(Some).collect();
        let mut outs: Vec<Option<[Vec<f32>; 2]>> = (0..procs.len()).map(|_| None).collect();
        let mut bus_in = outs.clone();
        // Side chains: a key track's signal before its fader (kept only if something keys it
        // pre-fader) or after it (its output; silence while muted). Key tracks render first.
        let keyed_pre: Vec<bool> = (0..procs.len()).map(|s| self.project.tracks.iter().flat_map(|t| &t.plugins).chain(&self.project.master_plugins).any(|p| p.side_chain.is_some_and(|k| k.pre_fader && self.project.track_by_id(k.track) == Some(s)))).collect();
        let mut pre_taps: Vec<Option<[Vec<f32>; 2]>> = outs.clone();
        let muted = silence();
        let key_of = |slots: &[opentake_project::PluginSlot], id: u32, owner: Option<usize>| -> Option<(usize, bool)> {
            let key = slots.iter().find(|s| s.id == id)?.side_chain?;
            Some((self.project.track_by_id(key.track).filter(|&s| Some(s) != owner)?, key.pre_fader))
        };
        for i in self.project.processing_order() {
            let (track, Some(mut p)) = (&self.project.tracks[i], procs[i].take()) else { continue };
            let [mut l, mut r] = bus_in[i].take().unwrap_or_else(silence);
            render_audio_regions(track, start, sr, &self.media, &mut l, &mut r);
            if let Some((id, inst)) = p.instrument.as_mut() {
                render_midi(track, inst.as_mut(), &param_lanes(&track.automation, *id), start, sr, &map, &mut l, &mut r);
            }
            for (id, fx) in &mut p.effects {
                let key = key_of(&track.plugins, *id, Some(i)).and_then(|(s, pre)| match pre {
                    true => pre_taps[s].as_ref(),
                    false if self.project.tracks[s].mute => Some(&muted),
                    false => outs[s].as_ref(),
                });
                process_stereo(fx.as_mut(), &param_lanes(&track.automation, *id), key, &mut l, &mut r, &map, start, sr);
            }
            if keyed_pre[i] {
                pre_taps[i] = Some([l.clone(), r.clone()]);
            }
            // Pre-fader sends tap the signal here, post-fader ones after the fader.
            let pre_fader = track.sends.iter().any(|s| s.pre_fader && !s.mute).then(|| [l.clone(), r.clone()]);
            apply_fader(&mut l, &mut r, start, sr, |t| {
                let volume = automated(&track.automation, AutomationTarget::Volume, t).map_or(track.volume, pos_to_gain);
                let pan = automated(&track.automation, AutomationTarget::Pan, t).map_or(track.pan, |v| v * 2.0 - 1.0);
                let mute = automated(&track.automation, AutomationTarget::Mute, t).is_some_and(|v| v >= 0.5);
                if mute { (0.0, 0.0) } else { pan_gains(volume, pan) }
            });
            if audible[i] {
                let dst = match self.project.output_of(i) {
                    Some(b) => bus_in[b].get_or_insert_with(silence),
                    None => &mut mix,
                };
                for k in 0..len {
                    dst[0][k] += l[k];
                    dst[1][k] += r[k];
                }
                for (k, send) in track.sends.iter().enumerate().filter(|(_, s)| !s.mute) {
                    let Some(b) = self.project.send_dest(i, k) else { continue };
                    let [mut wl, mut wr] = match (&pre_fader, send.pre_fader) {
                        (Some(tap), true) => tap.clone(),
                        _ => [l.clone(), r.clone()],
                    };
                    apply_fader(&mut wl, &mut wr, start, sr, |t| {
                        let level = automated(&track.automation, AutomationTarget::Send { send: send.id }, t).map_or(send.level, pos_to_gain);
                        // Pre-fader skips the fader, but not the track's mute.
                        let muted = send.pre_fader && automated(&track.automation, AutomationTarget::Mute, t).is_some_and(|v| v >= 0.5);
                        if muted { (0.0, 0.0) } else { (level, level) }
                    });
                    let dst = bus_in[b].get_or_insert_with(silence);
                    for k in 0..len {
                        dst[0][k] += wl[k];
                        dst[1][k] += wr[k];
                    }
                }
            }
            outs[i] = Some([l, r]);
        }
        let lanes = &self.project.master_automation;
        for (id, fx) in &mut self.master {
            let key = key_of(&self.project.master_plugins, *id, None).and_then(|(s, pre)| match pre {
                true => pre_taps[s].as_ref(),
                false if self.project.tracks[s].mute => Some(&muted),
                false => outs[s].as_ref(),
            });
            let [l, r] = &mut mix;
            process_stereo(fx.as_mut(), &param_lanes(lanes, *id), key, l, r, &map, start, sr);
        }
        let tracks = self.project.tracks.iter().zip(outs).zip(audible).map(|((t, o), a)| (t.name.clone(), o.unwrap_or_else(silence), a)).collect();
        let [l, r] = &mut mix;
        let volume = self.project.master_volume;
        apply_fader(l, r, start, sr, |t| {
            let g = automated(lanes, AutomationTarget::Volume, t).map_or(volume, pos_to_gain);
            (g, g)
        });
        Rendered { sample_rate: sr, from_secs: from, tracks, master: mix, warnings: self.warnings }
    }
}

/// Linear balance pan, as the mixer does.
fn pan_gains(volume: f32, pan: f32) -> (f32, f32) {
    let pan = pan.clamp(-1.0, 1.0);
    (volume * (1.0 - pan).min(1.0), volume * (1.0 + pan).min(1.0))
}

/// A plug-in slot's active parameter lanes: (parameter id, lane).
fn param_lanes(lanes: &[AutomationLane], slot: u32) -> Vec<(u32, &AutomationLane)> {
    lanes
        .iter()
        .filter(|l| l.is_active())
        .filter_map(|l| match l.target {
            AutomationTarget::Param { slot: s, param } if s == slot => Some((param, l)),
            _ => None,
        })
        .collect()
}

/// Set automated parameters for the block at `secs`; the block size to use.
fn automate(p: &mut dyn Processor, lanes: &[(u32, &AutomationLane)], secs: f64) -> usize {
    for (id, lane) in lanes {
        if let Some(v) = lane.value_at(secs) {
            p.set_param(*id, v as f64);
        }
    }
    if lanes.is_empty() { BLOCK } else { AUTOMATION_BLOCK }
}

/// Multiply a rendered track by `gains(secs)` (left, right), evaluated every automation
/// block and ramped in between, as the mixer does.
fn apply_fader(l: &mut [f32], r: &mut [f32], offset: usize, sr: f64, gains: impl Fn(f64) -> (f32, f32)) {
    let mut g0 = gains(offset as f64 / sr);
    for start in (0..l.len()).step_by(AUTOMATION_BLOCK) {
        let n = AUTOMATION_BLOCK.min(l.len() - start);
        let g1 = gains((offset + start + n) as f64 / sr);
        for i in 0..n {
            let f = (i + 1) as f32 / n as f32;
            l[start + i] *= g0.0 + (g1.0 - g0.0) * f;
            r[start + i] *= g0.1 + (g1.1 - g0.1) * f;
        }
        g0 = g1;
    }
}

/// Run an effect over a whole stereo buffer (starting `offset` samples into the song, for
/// the tempo and automation) with its side chain, if keyed (lined up with the buffer), then
/// shift the result back by the effect's latency, as the mixer's delay compensation does.
#[allow(clippy::too_many_arguments)]
fn process_stereo(p: &mut dyn Processor, lanes: &[(u32, &AutomationLane)], key: Option<&[Vec<f32>; 2]>, l: &mut [f32], r: &mut [f32], map: &TempoMap, offset: usize, sr: f64) {
    let (mut ol, mut or) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut start = 0;
    while start < l.len() {
        let n = automate(p, lanes, (offset + start) as f64 / sr).min(l.len() - start);
        let t = Transport { tempo_bpm: map.tempo_at((offset + start) as f64 / sr), playing: true, position_samples: (offset + start) as i64, ..Transport::default() };
        {
            let inputs: [&[f32]; 2] = [&l[start..start + n], &r[start..start + n]];
            let mut outputs: [&mut [f32]; 2] = [&mut ol[..n], &mut or[..n]];
            let key = key.map(|[kl, kr]| [&kl[start..start + n], &kr[start..start + n]]);
            p.process_keyed(&inputs, key.as_ref().map(|k| &k[..]), &mut outputs, n, &t, &[]);
        }
        l[start..start + n].copy_from_slice(&ol[..n]);
        r[start..start + n].copy_from_slice(&or[..n]);
        start += n;
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

fn render_audio_regions(track: &opentake_project::Track, start: usize, sr: f64, media: &[(FlexKey, Arc<AudioData>)], l: &mut [f32], r: &mut [f32]) {
    render_clips(&track.clips, start, sr, |clip| media.iter().find(|(k, _)| *k == flex::key(clip, sr as u32)).map(|(_, d)| d.clone()), l, r);
}

/// Add audio regions (as the audio thread plays them: gain, fades, crossfades over overlaps)
/// into `l`/`r`, which start at timeline sample `start`. `media` gives each region's audio.
pub(super) fn render_clips(clips: &[Clip], start: usize, sr: f64, media: impl Fn(&Clip) -> Option<Arc<AudioData>>, l: &mut [f32], r: &mut [f32]) {
    let len = l.len();
    for (clip, fades) in clips.iter().zip(effective_fades(clips)).filter(|(c, _)| !c.mute) {
        let Some(data) = media(clip) else { continue };
        let samples = |secs: f64| (secs.max(0.0) * sr).round() as u64;
        let (clip_start, clip_end) = (samples(clip.position_secs), samples(clip.position_secs + clip.length_secs));
        let fades = ClipFades::new(clip_start, clip_end.max(clip_start), fades, sr);
        // The processed audio is already stretched (offsets are in source seconds).
        let offset = (clip.source_offset_secs * clip.stretch * sr).round() as i64;
        let src_l = &data.channels[0];
        let src_r = data.channels.get(1).unwrap_or(src_l);
        for i in 0..len {
            let t = (start + i) as u64;
            if t < clip_start || t >= clip_end {
                continue;
            }
            let Ok(src) = usize::try_from(offset + (t - clip_start) as i64) else { continue }; // a take starting later
            if src >= src_l.len() {
                break;
            }
            let g = clip.gain * fades.gain(t);
            l[i] += src_l[src] * g;
            r[i] += src_r[src] * g;
        }
    }
}

/// MIDI through the instrument, rendered from the song start (so voices are in the right
/// state at `start`) and trimmed; adds into `l`/`r`.
#[allow(clippy::too_many_arguments)]
fn render_midi(track: &opentake_project::Track, inst: &mut dyn Processor, lanes: &[(u32, &AutomationLane)], start: usize, sr: f64, map: &TempoMap, l: &mut [f32], r: &mut [f32]) {
    let len = l.len();
    let total = start + len;
    let mut events: Vec<(usize, u8, u8)> = Vec::new();
    for c in track.midi_clips.iter().filter(|c| !c.mute) {
        for n in c.notes.iter().filter(|n| n.start_secs < c.length_secs) {
            let on = ((c.position_secs + n.start_secs) * sr) as usize;
            let off = ((c.position_secs + (n.start_secs + n.length_secs).min(c.length_secs)) * sr) as usize;
            events.push((on, n.pitch, n.velocity.max(1)));
            events.push((off, n.pitch, 0));
        }
    }
    events.sort_by_key(|e| (e.0, e.2 != 0));
    let silence = vec![0.0f32; BLOCK];
    let (mut ol, mut or) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let latency = inst.latency();
    let mut ei = 0;
    let mut pos = 0;
    while pos < total + latency {
        let n = automate(inst, lanes, pos as f64 / sr).min(total + latency - pos);
        let mut block = Vec::new();
        while ei < events.len() && events[ei].0 < pos + n {
            block.push(MidiEvent { offset: (events[ei].0.saturating_sub(pos)) as u32, pitch: events[ei].1, velocity: events[ei].2 });
            ei += 1;
        }
        let t = Transport { tempo_bpm: map.tempo_at(pos as f64 / sr), playing: true, position_samples: pos as i64, ..Transport::default() };
        {
            let inputs: [&[f32]; 2] = [&silence[..n], &silence[..n]];
            let mut outputs: [&mut [f32]; 2] = [&mut ol[..n], &mut or[..n]];
            inst.process(&inputs, &mut outputs, n, &t, &block);
        }
        for k in 0..n {
            // Shift back by the instrument's latency (delay compensation).
            let at = (pos + k) as isize - latency as isize - start as isize;
            if at >= 0 && (at as usize) < len {
                l[at as usize] += ol[k];
                r[at as usize] += or[k];
            }
        }
        pos += n;
    }
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

    fn pass(freq: f64, high: bool, sr: f64) -> Self {
        let w = std::f64::consts::TAU * freq.min(sr * 0.45) / sr;
        let (c, s) = (w.cos(), w.sin());
        let alpha = s / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b = if high { [(1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0] } else { [(1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0] };
        Self::new([b[0] / a0, b[1] / a0, b[2] / a0], [1.0, -2.0 * c / a0, (1.0 - alpha) / a0])
    }

    /// The two BS.1770 K-weighting stages (high shelf, then high pass) at any sample rate.
    fn k_weighting(sr: f64) -> [Self; 2] {
        let (f0, gain, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
        let k = (std::f64::consts::PI * f0 / sr).tan();
        let vh = 10f64.powf(gain / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1.0 + k / q + k * k;
        let shelf = Self::new(
            [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
            [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        );
        let (f0, q) = (38.13547087602444, 0.5003270373238773);
        let k = (std::f64::consts::PI * f0 / sr).tan();
        let a0 = 1.0 + k / q + k * k;
        let high_pass = Self::new([1.0, -2.0, 1.0], [1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0]);
        [shelf, high_pass]
    }
}

/// Integrated loudness (BS.1770-4: 400 ms blocks, 75 % overlap, absolute and relative gates).
pub(crate) fn lufs(l: &[f32], r: &[f32], sr: f64) -> f64 {
    let weigh = |x: &[f32]| {
        let [mut a, mut b] = Biquad::k_weighting(sr);
        x.iter().map(|&v| b.run(a.run(v as f64))).collect::<Vec<f64>>()
    };
    let (kl, kr) = (weigh(l), weigh(r));
    let win = (0.4 * sr) as usize;
    let hop = (win / 4).max(1);
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
fn band_powers(x: &[f32], sr: f64) -> Vec<f64> {
    let n = 4096;
    let mut acc = vec![0.0; BANDS.len()];
    let mut count = 0;
    let mut i = 0;
    while i + n <= x.len() {
        let mut re: Vec<f64> = (0..n).map(|k| x[i + k] as f64 * (0.5 - 0.5 * (std::f64::consts::TAU * k as f64 / n as f64).cos())).collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        for k in 1..n / 2 {
            let f = k as f64 * sr / n as f64;
            if let Some(b) = BANDS.iter().position(|b| f >= b.1 && f < b.2) {
                acc[b] += re[k] * re[k] + im[k] * im[k];
            }
        }
        count += 1;
        i += n * 4; // every 4th window is plenty for averages
    }
    acc.iter().map(|&p| 10.0 * (p / count.max(1) as f64).max(1e-20).log10()).collect()
}

pub(crate) fn peak_db(l: &[f32], r: &[f32]) -> f64 {
    let peak = l.iter().chain(r).fold(0.0f32, |m, v| m.max(v.abs()));
    round1(20.0 * (peak as f64).max(1e-9).log10())
}

fn measure(name: &str, l: &[f32], r: &[f32], sr: f64) -> LevelReport {
    let mono: Vec<f32> = l.iter().zip(r).map(|(a, b)| (a + b) * 0.5).collect();
    let bands = band_powers(&mono, sr).into_iter().zip(BANDS).map(|(db, b)| (b.0.to_string(), round1(db))).collect();
    // Phone speaker: little below ~300 Hz, rolled off above 12 kHz (mono).
    let (mut h1, mut h2, mut lp) = (Biquad::pass(300.0, true, sr), Biquad::pass(300.0, true, sr), Biquad::pass(12_000.0, false, sr));
    let phone: Vec<f32> = mono.iter().map(|&v| lp.run(h2.run(h1.run(v as f64))) as f32).collect();
    let full_mono = lufs(&mono, &mono, sr);
    let phone_lufs = lufs(&phone, &phone, sr);
    LevelReport {
        name: name.to_string(),
        peak_db: peak_db(l, r),
        lufs: round1(lufs(l, r, sr)),
        bands,
        phone_lufs: round1(phone_lufs),
        phone_drop: round1(phone_lufs - full_mono),
    }
}

pub(crate) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::super::Engine;
    use crate::eq::{Field, norm_freq, param_id};
    use opentake_project::Note;

    #[test]
    fn k_weighting_matches_the_standard_48k_coefficients() {
        let [shelf, hp] = super::Biquad::k_weighting(48_000.0);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(shelf.b[0], 1.53512485958697) && close(shelf.b[1], -2.69169618940638) && close(shelf.b[2], 1.19839281085285));
        assert!(close(shelf.a[1], -1.69065929318241) && close(shelf.a[2], 0.73248077421585));
        assert!(close(hp.a[1], -1.99004745483398) && close(hp.a[2], 0.99007225036621));
    }

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
