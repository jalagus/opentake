//! Pitch shifting and time stretching of audio regions. A region's processed audio is
//! rendered on the control side (see `stretch.rs`) and cached by file, rate, pitch and
//! stretch, so the audio thread plays it like any other media.
//!
//! [`Engine::set_clip_pitch_stretch`] renders synchronously. Callers that shouldn't block
//! the engine for long files take a [`FlexJob`] under the lock, render it without the lock
//! and hand the result back with [`Engine::add_flex_render`] before applying the edit.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use opentake_project::Clip;

use super::Engine;
use crate::media::AudioData;
use crate::stretch;

/// Stretch ratios a region accepts (timeline length / source length).
pub const STRETCH_RANGE: (f64, f64) = (0.25, 4.0);
/// Pitch shift range in semitones.
pub const MAX_SEMITONES: i32 = 24;

/// Cache key of a region's processed audio: file, rate, pitch (cents) and stretch.
pub(super) type FlexKey = (PathBuf, u32, i32, u64);

pub(super) fn key(clip: &Clip, sr: u32) -> FlexKey {
    (clip.file.clone(), sr, (clip.transpose() * 100.0).round() as i32, clip.stretch.to_bits())
}

/// Pitch/stretch changes for a region; `None` keeps a value. `length_secs` fits the
/// region to a timeline length instead of giving `stretch`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PitchStretch {
    pub semitones: Option<i32>,
    pub cents: Option<i32>,
    pub stretch: Option<f64>,
    pub length_secs: Option<f64>,
}

impl PitchStretch {
    /// The region with these settings. A new stretch scales its length, so the same part of
    /// the source plays (the source offset is in source seconds and stays).
    pub fn apply(&self, c: &Clip) -> Result<Clip> {
        let semitones = self.semitones.unwrap_or(c.pitch_semitones);
        let cents = self.cents.unwrap_or(c.pitch_cents);
        if semitones.abs() > MAX_SEMITONES || cents.abs() > 100 {
            bail!("pitch must be within ±{MAX_SEMITONES} semitones and ±100 cents");
        }
        let stretch = match (self.length_secs, self.stretch) {
            (Some(len), _) if !(len.is_finite() && len >= Engine::MIN_CLIP_SECS) => bail!("length must be at least {} s", Engine::MIN_CLIP_SECS),
            (Some(len), _) => c.stretch * len / c.length_secs,
            (None, Some(s)) => s,
            (None, None) => c.stretch,
        };
        let (lo, hi) = STRETCH_RANGE;
        if !(stretch.is_finite() && (lo - 1e-9..=hi + 1e-9).contains(&stretch)) {
            bail!("stretch ratio {stretch:.3} is outside {lo}–{hi} (timeline length / original length)");
        }
        // Snap to neutral so float noise doesn't keep a region "stretched".
        let stretch = if (stretch - 1.0).abs() < 1e-6 { 1.0 } else { stretch.clamp(lo, hi) };
        let length_secs = self.length_secs.unwrap_or(c.length_secs * stretch / c.stretch);
        // Fades stretch with the region.
        let scale = length_secs / c.length_secs;
        let (mut fade_in, mut fade_out) = (c.fade_in, c.fade_out);
        (fade_in.secs, fade_out.secs) = (fade_in.secs * scale, fade_out.secs * scale);
        let mut out = Clip { pitch_semitones: semitones, pitch_cents: cents, stretch, length_secs, fade_in, fade_out, ..c.clone() };
        out.clamp_fades();
        Ok(out)
    }
}

/// A region's audio to render without holding the engine.
pub struct FlexJob {
    key: FlexKey,
    source: Arc<AudioData>,
    semitones: f64,
    stretch: f64,
}

/// A finished [`FlexJob`], for [`Engine::add_flex_render`].
pub struct FlexRender {
    key: FlexKey,
    data: Arc<AudioData>,
}

impl FlexJob {
    /// Length of the audio to process (seconds), to judge how long the render takes.
    pub fn source_secs(&self) -> f64 {
        self.source.frames() as f64 / self.source.sample_rate as f64
    }

    pub fn render(self) -> FlexRender {
        FlexRender { key: self.key, data: Arc::new(stretch::process(&self.source, self.semitones, self.stretch)) }
    }
}

impl Engine {
    /// Pitch-shift and/or time-stretch a region (one undo step). Renders the processed audio
    /// first if it isn't cached; see [`Engine::pitch_stretch_job`] to do that off the lock.
    pub fn set_clip_pitch_stretch(&mut self, track: usize, clip: usize, ps: PitchStretch) -> Result<Clip> {
        let index = self.clip_index(track, clip)?;
        let c = ps.apply(&self.project.tracks[index].clips[clip])?;
        let sr = self.sample_rate();
        self.clip_media(&c, sr)?;
        self.record("Pitch & Stretch", None, |e| {
            e.project.tracks[index].clips[clip] = c.clone();
            e.resync_clips(index)?;
            e.sync_song_end()?;
            Ok(c)
        })
    }

    /// The render `set_clip_pitch_stretch` would need, or `None` if it has nothing to render.
    pub fn pitch_stretch_job(&mut self, track: usize, clip: usize, ps: PitchStretch) -> Result<Option<FlexJob>> {
        let index = self.clip_index(track, clip)?;
        let c = ps.apply(&self.project.tracks[index].clips[clip])?;
        self.flex_job(&c, self.sample_rate())
    }

    /// Cache a render made off the lock.
    pub fn add_flex_render(&mut self, r: FlexRender) {
        self.prune_flexed();
        self.flexed.insert(r.key, r.data);
    }

    fn flex_job(&mut self, clip: &Clip, sr: u32) -> Result<Option<FlexJob>> {
        let key = key(clip, sr);
        if !clip.is_flexed() || self.flexed.contains_key(&key) {
            return Ok(None);
        }
        let source = self.load_media(&clip.file, sr)?;
        Ok(Some(FlexJob { key, source, semitones: clip.transpose(), stretch: clip.stretch }))
    }

    /// The audio a region plays at `sr`: its file, pitch-shifted/stretched if it's set to.
    pub(super) fn clip_media(&mut self, clip: &Clip, sr: u32) -> Result<Arc<AudioData>> {
        if !clip.is_flexed() {
            return self.load_media(&clip.file, sr);
        }
        if let Some(job) = self.flex_job(clip, sr)? {
            self.add_flex_render(job.render());
        }
        Ok(self.flexed[&key(clip, sr)].clone())
    }

    /// Drop renders no region uses any more (undo re-renders them if needed).
    fn prune_flexed(&mut self) {
        let sr = self.sample_rate();
        let used: Vec<FlexKey> = self.project.tracks.iter().flat_map(|t| &t.clips).map(|c| key(c, sr)).collect();
        self.flexed.retain(|k, _| used.contains(k));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::tone_file;
    use super::*;

    fn engine(name: &str) -> (Engine, PathBuf) {
        let dir = std::env::temp_dir().join(format!("opentake-flex-{name}-{}", std::process::id()));
        let wav = tone_file(&dir, 2.0);
        let mut e = Engine::new();
        e.add_track("T").unwrap();
        e.add_clip(0, &wav, 1.0).unwrap(); // 1.0 .. 3.0
        (e, dir)
    }

    #[test]
    fn stretch_scales_length_and_edits_stay_in_source_time() {
        let (mut e, dir) = engine("edit");
        let c = e.set_clip_pitch_stretch(0, 0, PitchStretch { stretch: Some(2.0), semitones: Some(-5), ..Default::default() }).unwrap();
        assert_eq!((c.length_secs, c.stretch, c.pitch_semitones), (4.0, 2.0, -5));
        assert_eq!(e.history_info().undo.as_deref(), Some("Pitch & Stretch"));
        assert_eq!(e.flexed.len(), 1, "rendered once");

        // Split 1 s into the region: that's 0.5 s of source.
        let (l, r) = e.split_clip(0, 0, 2.0).unwrap();
        assert_eq!((l.length_secs, r.source_offset_secs, r.length_secs), (1.0, 0.5, 3.0));
        // Trim limits are the stretched source: it ends at 1.0 + 2 * 2.0 = 5.0.
        let t = e.trim_clip(0, 1, Some(1.5), Some(99.0)).unwrap();
        assert_eq!((t.position_secs, t.source_offset_secs, t.position_secs + t.length_secs), (1.5, 0.25, 5.0));
        let t = e.trim_clip(0, 1, Some(0.0), None).unwrap();
        assert_eq!((t.position_secs, t.source_offset_secs), (1.0, 0.0));
        assert_eq!(e.flexed.len(), 1, "split parts share the render");

        // Fit to a length; undo restores the old ratio.
        let f = e.set_clip_pitch_stretch(0, 0, PitchStretch { length_secs: Some(0.5), ..Default::default() }).unwrap();
        assert_eq!((f.length_secs, f.stretch), (0.5, 1.0));
        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].clips[0].stretch, 2.0);
        assert!(e.set_clip_pitch_stretch(0, 0, PitchStretch { stretch: Some(9.0), ..Default::default() }).is_err());
        assert!(e.set_clip_pitch_stretch(0, 0, PitchStretch { semitones: Some(40), ..Default::default() }).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn playback_reads_the_stretched_audio() {
        let (mut e, dir) = engine("play");
        e.set_clip_pitch_stretch(0, 0, PitchStretch { stretch: Some(1.5), ..Default::default() }).unwrap();
        e.trim_clip(0, 0, Some(2.0), None).unwrap();
        let c = e.project().tracks[0].clips[0].clone();
        let rt = e.build_clip(&c, 48_000).unwrap();
        // 2 s of source plays over 3 s (1.0 .. 4.0); trimming 1 s in skips 1 s of the
        // stretched audio, which is 2/3 s of source.
        assert_eq!((rt.start, rt.len, rt.offset, rt.data.frames()), (96_000, 96_000, 48_000, 144_000));
        assert!(Arc::ptr_eq(&rt.data, &e.clip_media(&c, 48_000).unwrap()), "cached");
        // Offline renders (analysis/export) play it the same way: sound until the new end.
        assert!((c.source_offset_secs - 2.0 / 3.0).abs() < 1e-9);
        let r = e.mix_job_at(48_000.0).render(Some(3.5), Some(3.9), 0.0);
        assert!(r.master[0].iter().any(|v| v.abs() > 0.1), "stretched tail is rendered");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn job_renders_off_the_engine() {
        let (mut e, dir) = engine("job");
        let ps = PitchStretch { semitones: Some(7), cents: Some(-30), ..Default::default() };
        let job = e.pitch_stretch_job(0, 0, ps).unwrap().expect("needs a render");
        e.add_flex_render(job.render());
        assert!(e.pitch_stretch_job(0, 0, ps).unwrap().is_none(), "cached now");
        let c = e.set_clip_pitch_stretch(0, 0, ps).unwrap();
        assert_eq!((c.transpose(), c.length_secs), (6.7, 2.0));
        assert!(e.pitch_stretch_job(0, 0, PitchStretch { semitones: Some(0), cents: Some(0), ..Default::default() }).unwrap().is_none(), "neutral needs none");
        std::fs::remove_dir_all(dir).ok();
    }
}
