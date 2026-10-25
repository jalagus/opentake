//! Region fades, crossfades and merging regions into one.
//!
//! A region's own fades are stored on it; overlapping regions crossfade automatically (see
//! `opentake_project::fade`), so [`Engine::crossfade_clips`] only has to make the overlap.
//! [`Engine::merge_clips`] renders regions offline into a new file. Where two of them butt
//! against each other it first overlaps them by [`JOIN_SECS`] (or, without audio beyond
//! their edges, fades both ends that short), so the join can't click.

use std::sync::Arc;

use anyhow::{Result, bail};
use opentake_project::{Clip, Fade, FadeCurve};
use serde::Deserialize;

use super::Engine;
use super::analysis::render_clips;
use super::flex::{self, FlexKey};
use crate::media::AudioData;

/// Default length of a crossfade made by [`Engine::crossfade_clips`] (seconds).
pub const CROSSFADE_SECS: f64 = 0.02;
/// Crossfade at a join between butted regions when merging (seconds).
pub const JOIN_SECS: f64 = 0.005;
/// Regions this close count as butted (seconds).
const BUTT_TOLERANCE: f64 = 0.001;

/// Fade changes for a region; `None` keeps a value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
pub struct FadeUpdate {
    pub fade_in_secs: Option<f64>,
    pub fade_out_secs: Option<f64>,
    pub curve_in: Option<FadeCurve>,
    pub curve_out: Option<FadeCurve>,
}

impl FadeUpdate {
    /// The region with these fades. Each fade is limited to what the other leaves free.
    pub fn apply(&self, c: &Clip) -> Result<Clip> {
        let mut c = c.clone();
        for v in [self.fade_in_secs, self.fade_out_secs].into_iter().flatten() {
            if !v.is_finite() || v < 0.0 {
                bail!("fade lengths must be zero or more seconds");
            }
        }
        if let Some(v) = self.fade_in_secs {
            c.fade_in.secs = v.min(c.length_secs - c.fade_out.secs.min(c.length_secs));
        }
        if let Some(v) = self.fade_out_secs {
            c.fade_out.secs = v.min(c.length_secs - c.fade_in.secs);
        }
        c.fade_in.curve = self.curve_in.unwrap_or(c.fade_in.curve);
        c.fade_out.curve = self.curve_out.unwrap_or(c.fade_out.curve);
        c.clamp_fades();
        Ok(c)
    }
}

/// Timeline span of a region's source audio (where the file starts and ends).
fn source_span(c: &Clip, source_secs: f64) -> (f64, f64) {
    let start = c.position_secs - c.source_offset_secs * c.stretch;
    (start, start + source_secs * c.stretch)
}

/// Move `c`'s start to `at` (timeline seconds), keeping its audio in place.
fn set_start(c: &mut Clip, at: f64) {
    let delta = at - c.position_secs;
    c.position_secs = at;
    c.length_secs -= delta;
    c.source_offset_secs += delta / c.stretch;
}

/// Overlap `a` (earlier) and `b` by `secs`, centred on their boundary, trimming or extending
/// their facing edges within the audio their sources have (`a_end` latest end of `a`,
/// `b_start` earliest start of `b`, both timeline seconds). Returns the overlap made (0 if
/// the sources don't reach).
fn overlap(a: &mut Clip, b: &mut Clip, secs: f64, a_end: f64, b_start: f64) -> f64 {
    let min = Engine::MIN_CLIP_SECS;
    let (end, b_end) = (a.position_secs + a.length_secs, b.position_secs + b.length_secs);
    let centre = (end + b.position_secs) / 2.0;
    let lo = b_start.max(a.position_secs + min).min(b.position_secs.max(a.position_secs + min));
    let hi = a_end.min(b_end - min).max(end.min(b_end - min));
    let mut from = (centre - secs / 2.0).max(lo);
    let to = (from + secs).min(hi);
    from = (to - secs).max(lo);
    if to <= from {
        return 0.0;
    }
    a.length_secs = to - a.position_secs;
    set_start(b, from);
    a.clamp_fades();
    b.clamp_fades();
    to - from
}

impl Engine {
    /// Set an audio region's fade-in/out lengths (timeline seconds) and curves. Changes in
    /// quick succession (a drag) are one undo step.
    pub fn set_clip_fades(&mut self, track: usize, clip: usize, update: FadeUpdate) -> Result<Clip> {
        let index = self.clip_index(track, clip)?;
        let c = update.apply(&self.project.tracks[index].clips[clip])?;
        let id = self.project.tracks[index].id;
        self.record("Region Fades", Some(format!("clip-fades-{id}-{clip}")), |e| {
            e.project.tracks[index].clips[clip] = c.clone();
            e.resync_clips(index)?;
            Ok(c)
        })
    }

    /// Crossfade two regions on a track over `secs` (default [`CROSSFADE_SECS`]): overlap
    /// them, centred on their boundary, as far as their audio reaches, and give both
    /// equal-power fades over the overlap. Regions whose sources can't overlap (nothing
    /// beyond their edges) get a fade-out and fade-in of half the length each instead.
    /// Returns the (earlier, later) region.
    pub fn crossfade_clips(&mut self, track: usize, a: usize, b: usize, secs: Option<f64>) -> Result<(Clip, Clip)> {
        let index = self.clip_index(track, a)?;
        self.clip_index(track, b)?;
        let secs = secs.unwrap_or(CROSSFADE_SECS);
        if a == b || !secs.is_finite() || secs <= 0.0 {
            bail!("crossfade needs two different regions and a positive length");
        }
        let clips = &self.project.tracks[index].clips;
        let (first, second) = if (clips[a].position_secs, a) <= (clips[b].position_secs, b) { (a, b) } else { (b, a) };
        let (mut x, mut y) = (clips[first].clone(), clips[second].clone());
        let gap = y.position_secs - (x.position_secs + x.length_secs);
        if gap > secs {
            bail!("the regions are {gap:.3} s apart; a {secs:.3} s crossfade can't bridge that");
        }
        if y.position_secs + y.length_secs <= x.position_secs + x.length_secs {
            bail!("'{}' lies inside '{}'; there's no boundary to crossfade", y.name, x.name);
        }
        let (_, x_end) = source_span(&x, self.source_secs(&x)?);
        let (y_start, _) = source_span(&y, self.source_secs(&y)?);
        let made = overlap(&mut x, &mut y, secs, x_end, y_start);
        let fade = if made > 0.0 { made } else { secs / 2.0 };
        x.fade_out = Fade::new(fade, FadeCurve::EqualPower);
        y.fade_in = Fade::new(fade, FadeCurve::EqualPower);
        x.clamp_fades();
        y.clamp_fades();
        self.record("Crossfade", None, |e| {
            let clips = &mut e.project.tracks[index].clips;
            clips[first] = x.clone();
            clips[second] = y.clone();
            e.resync_clips(index)?;
            Ok((x, y))
        })
    }

    /// Merge (join) audio regions of one track into a single region playing a new file in
    /// the media folder: what they play — gain, fades, crossfades, pitch/stretch, the active
    /// take — rendered over their combined span (gaps become silence). Butted regions are
    /// crossfaded over [`JOIN_SECS`] at the join. One undo step; the source files stay.
    /// Returns the new region's index and the region.
    pub fn merge_clips(&mut self, track: usize, clips: &[usize]) -> Result<(usize, Clip)> {
        let index = self.track_index(track)?;
        let mut picked: Vec<usize> = clips.to_vec();
        picked.sort_unstable();
        picked.dedup();
        if picked.len() < 2 {
            bail!("merging needs at least two regions");
        }
        for &c in &picked {
            self.clip_index(track, c)?;
        }
        let mut parts: Vec<Clip> = picked.iter().map(|&c| self.project.tracks[index].clips[c].clone()).collect();
        parts.sort_by(|a, b| a.position_secs.total_cmp(&b.position_secs));
        let from = parts[0].position_secs;
        let to = parts.iter().map(|c| c.position_secs + c.length_secs).fold(from, f64::max);

        // Smooth butted joins: overlap into the audio beyond the edges, or fade both sides.
        for i in 1..parts.len() {
            let (head, tail) = parts.split_at_mut(i);
            let (a, b) = (&mut head[i - 1], &mut tail[0]);
            if (b.position_secs - (a.position_secs + a.length_secs)).abs() > BUTT_TOLERANCE || a.mute || b.mute {
                continue;
            }
            let (_, a_end) = source_span(a, self.source_secs(a)?);
            let (b_start, _) = source_span(b, self.source_secs(b)?);
            if overlap(a, b, JOIN_SECS, a_end, b_start) <= 0.0 {
                let f = |fade: Fade| if fade.secs >= JOIN_SECS / 2.0 { fade } else { Fade::new(JOIN_SECS / 2.0, FadeCurve::EqualPower) };
                a.fade_out = f(a.fade_out);
                b.fade_in = f(b.fade_in);
                a.clamp_fades();
                b.clamp_fades();
            }
        }

        let sr = self.sample_rate();
        let mut media: Vec<(FlexKey, Arc<AudioData>)> = Vec::new();
        for c in &parts {
            let data = self.clip_media(c, sr)?;
            media.push((flex::key(c, sr), data));
        }
        let start = (from * sr as f64).round() as usize;
        let len = ((to * sr as f64).round() as usize).saturating_sub(start).max(1);
        let (mut l, mut r) = (vec![0.0f32; len], vec![0.0f32; len]);
        let lookup = |c: &Clip| media.iter().find(|(k, _)| *k == flex::key(c, sr)).map(|(_, d)| d.clone());
        render_clips(&parts, start, sr as f64, lookup, &mut l, &mut r);
        let stereo = parts.iter().filter(|c| !c.mute).any(|c| lookup(c).is_some_and(|d| d.channels.len() > 1));
        let channels = if stereo { vec![l, r] } else { vec![l] };

        // A new file next to the recordings, never overwriting one.
        let dir = self.media_dir();
        std::fs::create_dir_all(&dir)?;
        let stem = super::sanitize(&parts[0].name);
        let path = (1..)
            .map(|n| dir.join(format!("{stem} merged{}.wav", if n == 1 { String::new() } else { format!(" {n}") })))
            .find(|p| !p.exists())
            .expect("an unused name");
        write_wav(&path, &channels, sr)?;
        self.media.insert((path.clone(), sr), Arc::new(AudioData { channels, sample_rate: sr }));

        let merged = Clip {
            name: parts[0].name.clone(),
            file: path,
            position_secs: from,
            length_secs: len as f64 / sr as f64,
            source_offset_secs: 0.0,
            gain: 1.0,
            mute: false,
            takes: Vec::new(),
            active_take: 0,
            pitch_semitones: 0,
            pitch_cents: 0,
            stretch: 1.0,
            fade_in: Fade::default(),
            fade_out: Fade::default(),
        };
        self.record("Merge Regions", None, |e| {
            let list = &mut e.project.tracks[index].clips;
            for &c in picked.iter().rev() {
                list.remove(c);
            }
            list.push(merged.clone());
            let at = list.len() - 1;
            e.resync_clips(index)?;
            e.sync_song_end()?;
            Ok((at, merged))
        })
    }
}

/// Write de-interleaved channels as a 32-bit float WAV.
fn write_wav(path: &std::path::Path, channels: &[Vec<f32>], sr: u32) -> Result<()> {
    let spec = hound::WavSpec { channels: channels.len() as u16, sample_rate: sr, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    let mut w = hound::WavWriter::create(path, spec)?;
    for i in 0..channels[0].len() {
        for ch in channels {
            w.write_sample(ch[i])?;
        }
    }
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::path::{Path, PathBuf};

    use super::*;

    /// A 1 s, 440 Hz mono file at -6 dBFS starting at `phase` (radians).
    fn sine_file(dir: &Path, name: &str, phase: f32) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..48_000 {
            w.write_sample((TAU * 440.0 * i as f32 / 48_000.0 + phase).sin() * 0.5).unwrap();
        }
        w.finalize().unwrap();
        path
    }

    fn engine(name: &str) -> (Engine, PathBuf) {
        let dir = std::env::temp_dir().join(format!("opentake-fades-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let mut e = Engine::new();
        e.project_path = Some(dir.join("song.otp"));
        e.add_track("Vox").unwrap();
        (e, dir)
    }

    fn samples(path: &Path) -> Vec<f32> {
        hound::WavReader::open(path).unwrap().samples::<f32>().map(Result::unwrap).collect()
    }

    /// Largest sample-to-sample step within ±10 ms of `at` seconds.
    fn max_step(s: &[f32], at: f64) -> f32 {
        let c = (at * 48_000.0) as usize;
        (c - 480..c + 480).map(|i| (s[i + 1] - s[i]).abs()).fold(0.0, f32::max)
    }

    #[test]
    fn fades_clamp_and_drags_are_one_step() {
        let (mut e, dir) = engine("set");
        let wav = sine_file(&dir, "a.wav", 0.0);
        e.add_clip(0, &wav, 0.0).unwrap();
        e.set_clip_fades(0, 0, FadeUpdate { fade_in_secs: Some(0.1), ..Default::default() }).unwrap();
        let c = e.set_clip_fades(0, 0, FadeUpdate { fade_in_secs: Some(0.3), curve_in: Some(FadeCurve::SCurve), ..Default::default() }).unwrap();
        assert_eq!(c.fade_in, Fade::new(0.3, FadeCurve::SCurve));
        let c = e.set_clip_fades(0, 0, FadeUpdate { fade_out_secs: Some(5.0), ..Default::default() }).unwrap();
        assert!((c.fade_out.secs - 0.7).abs() < 1e-9, "fade-out limited to what the fade-in leaves");
        assert!(e.set_clip_fades(0, 0, FadeUpdate { fade_in_secs: Some(-1.0), ..Default::default() }).is_err());
        e.undo().unwrap();
        assert_eq!(e.history_info().undo.as_deref(), Some("Add Region"), "the fade drag was one step");

        // Split keeps the outer fades; trimming clamps them.
        e.set_clip_fades(0, 0, FadeUpdate { fade_in_secs: Some(0.2), fade_out_secs: Some(0.2), ..Default::default() }).unwrap();
        let (l, r) = e.split_clip(0, 0, 0.5).unwrap();
        assert_eq!((l.fade_in.secs, l.fade_out.secs, r.fade_in.secs, r.fade_out.secs), (0.2, 0.0, 0.0, 0.2));
        let t = e.trim_clip(0, 1, None, Some(0.6)).unwrap();
        assert!((t.fade_out.secs - 0.1).abs() < 1e-9, "{:?}", t.fade_out);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn crossfade_overlaps_butted_regions_within_their_audio() {
        let (mut e, dir) = engine("xfade");
        let wav = sine_file(&dir, "a.wav", 0.0);
        e.add_clip(0, &wav, 0.0).unwrap();
        e.split_clip(0, 0, 0.5).unwrap();
        let (a, b) = e.crossfade_clips(0, 1, 0, Some(0.02)).unwrap();
        assert!((a.position_secs + a.length_secs - 0.51).abs() < 1e-9 && (b.position_secs - 0.49).abs() < 1e-9, "centred on the cut");
        assert!((b.source_offset_secs - 0.49).abs() < 1e-9, "audio stays in place");
        assert!(a.fade_out == b.fade_in && a.fade_out.curve == FadeCurve::EqualPower && (a.fade_out.secs - 0.02).abs() < 1e-9);
        e.undo().unwrap();

        // No audio beyond the edges: a fade-out/fade-in pair instead.
        e.add_clip(0, &wav, 3.0).unwrap();
        e.add_clip(0, &wav, 4.0).unwrap();
        let (a, b) = e.crossfade_clips(0, 2, 3, None).unwrap();
        assert_eq!((a.length_secs, b.position_secs), (1.0, 4.0));
        assert!((a.fade_out.secs - CROSSFADE_SECS / 2.0).abs() < 1e-9 && a.fade_out == b.fade_in);
        assert!(e.crossfade_clips(0, 0, 3, None).is_err(), "too far apart");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn merging_butted_regions_does_not_click() {
        let (mut e, dir) = engine("click");
        let a = sine_file(&dir, "sin.wav", 0.0);
        let b = sine_file(&dir, "cos.wav", std::f32::consts::FRAC_PI_2);
        e.add_clip(0, &a, 0.0).unwrap();
        e.add_clip(0, &b, 0.0).unwrap();
        // sin up to 0.5 s (where it crosses zero), then cos from its peak: a 0.5 jump.
        e.trim_clip(0, 0, None, Some(0.5)).unwrap();
        e.trim_clip(0, 1, Some(0.5), None).unwrap();
        e.trim_clip(0, 1, None, Some(0.8)).unwrap();
        let natural = 0.5 * TAU * 440.0 / 48_000.0;
        let butted = e.mix_job().render(None, None, 0.0).master[0].clone();
        assert!(max_step(&butted, 0.5) > 0.3, "the unfaded join jumps");

        let (at, merged) = e.merge_clips(0, &[0, 1]).unwrap();
        assert_eq!((at, e.project().tracks[0].clips.len()), (0, 1));
        assert_eq!((merged.position_secs, merged.length_secs), (0.0, 0.8));
        assert!(merged.file.starts_with(dir.join("Audio")) && merged.file.exists());
        let s = samples(&merged.file);
        assert!(max_step(&s, 0.5) < natural * 1.5, "{} vs {natural}", max_step(&s, 0.5));
        assert!((s[24_000 - 1000] - butted[24_000 - 1000]).abs() < 1e-6, "away from the join it's untouched");

        // Butted where neither file has audio beyond the edge: short fades instead.
        e.undo().unwrap();
        let (mut f, dir2) = engine("click-edge");
        f.add_clip(0, &a, 0.0).unwrap();
        f.add_clip(0, &b, 1.0).unwrap();
        let (_, merged) = f.merge_clips(0, &[1, 0]).unwrap();
        let s = samples(&merged.file);
        assert!(max_step(&s, 1.0) < natural * 1.5);
        assert_eq!(s.len(), 96_000);
        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(dir2).ok();
    }

    #[test]
    fn merge_bakes_gain_and_fades_and_undo_restores() {
        let (mut e, dir) = engine("bake");
        let wav = sine_file(&dir, "a.wav", 0.0);
        e.add_clip(0, &wav, 0.0).unwrap();
        e.add_clip(0, &wav, 2.0).unwrap();
        e.set_clip_gain(0, 0, -6.0206).unwrap();
        e.set_clip_fades(0, 1, FadeUpdate { fade_in_secs: Some(0.5), ..Default::default() }).unwrap();
        let before = e.project().tracks[0].clips.clone();
        let (_, merged) = e.merge_clips(0, &[0, 1]).unwrap();
        assert_eq!((merged.position_secs, merged.length_secs, merged.gain), (0.0, 3.0, 1.0));
        let s = samples(&merged.file);
        let peak = |from: f64, to: f64| s[(from * 48_000.0) as usize..(to * 48_000.0) as usize].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((peak(0.1, 0.9) - 0.25).abs() < 0.01, "region gain baked in");
        assert!(peak(1.1, 1.9) == 0.0, "the gap is silence");
        assert!(peak(2.0, 2.05) < 0.1 && (peak(2.6, 2.9) - 0.5).abs() < 0.01, "fade-in baked in");
        assert!(e.merge_clips(0, &[0]).is_err());

        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].clips, before);
        assert!(merged.file.exists(), "the render stays on disk for redo");
        e.redo().unwrap();
        assert_eq!(e.project().tracks[0].clips.len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn overlapping_regions_crossfade_in_the_offline_render() {
        let (mut e, dir) = engine("render");
        let wav = sine_file(&dir, "a.wav", 0.0);
        // The same audio twice, overlapping in phase over 0.4–0.6 s: a crossfade, not a doubling.
        e.add_clip(0, &wav, 0.0).unwrap();
        e.add_clip(0, &wav, 0.0).unwrap();
        e.trim_clip(0, 0, None, Some(0.6)).unwrap();
        e.trim_clip(0, 1, Some(0.4), None).unwrap();
        let peak = |e: &mut Engine| e.mix_job().render(None, None, 0.0).master[0].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        // Equal power (automatic) on correlated audio bulges at most +3 dB, never +6 dB.
        // The audio thread gets the same crossfade, derived when the track is built.
        let rt = e.build_clips(0, 48_000);
        assert_eq!((rt[0].fades.fade_out, rt[1].fades.fade_in, rt[1].fades.curve_in), (9600, 9600, FadeCurve::EqualPower));
        let p = peak(&mut e);
        assert!(p > 0.55 && p < 0.5 * 1.415, "{p}");
        // Linear fades over the overlap (own fades at least as long win) sum to unity.
        let linear = FadeUpdate { curve_in: Some(FadeCurve::Linear), curve_out: Some(FadeCurve::Linear), ..Default::default() };
        e.set_clip_fades(0, 0, FadeUpdate { fade_out_secs: Some(0.2), ..linear }).unwrap();
        e.set_clip_fades(0, 1, FadeUpdate { fade_in_secs: Some(0.2), ..linear }).unwrap();
        assert!((peak(&mut e) - 0.5).abs() < 1e-3);
        std::fs::remove_dir_all(dir).ok();
    }
}
