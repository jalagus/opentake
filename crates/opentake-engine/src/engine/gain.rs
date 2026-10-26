//! Region gain: a level per audio region, before the track fader, and normalizing it so the
//! region's loudest sample reaches a target peak.

use anyhow::{Result, bail};
use opentake_project::Clip;

use super::Engine;

/// Region gain range in dB.
pub const CLIP_GAIN_DB: (f32, f32) = (-60.0, 24.0);

impl Engine {
    /// Set an audio region's gain in dB. Changes in quick succession (a drag) are one undo step.
    pub fn set_clip_gain(&mut self, track: usize, clip: usize, gain_db: f32) -> Result<Clip> {
        let index = self.clip_index(track, clip)?;
        if !gain_db.is_finite() || !(CLIP_GAIN_DB.0..=CLIP_GAIN_DB.1).contains(&gain_db) {
            bail!("region gain must be within {} to +{} dB", CLIP_GAIN_DB.0, CLIP_GAIN_DB.1);
        }
        let id = self.project.tracks[index].id;
        self.record("Region Gain", Some(format!("clip-gain-{id}-{clip}")), |e| {
            let c = &mut e.project.tracks[index].clips[clip];
            c.gain = 10f32.powf(gain_db / 20.0);
            let c = c.clone();
            e.resync_clips(index)?;
            Ok(c)
        })
    }

    /// Peak level (linear, before region gain) of what an audio region plays, including its
    /// pitch/stretch processing (every take a comp plays).
    pub fn clip_peak(&mut self, track: usize, clip: usize) -> Result<f32> {
        let index = self.clip_index(track, clip)?;
        let sr = self.sample_rate();
        let mut peak = 0.0f32;
        // One pass is all a looped region plays (passes may start part-way into it).
        let c = &self.project.tracks[index].clips[clip];
        let content = Clip { length_secs: c.content_secs(), looped: None, ..c.clone() };
        for (c, _) in content.played_parts(Default::default()) {
            let data = self.clip_media(&c, sr)?;
            // The part of the (processed) audio the region spans; a negative offset is silence.
            let frames = |secs: f64| (secs * sr as f64).round().max(0.0) as usize;
            let from = frames(c.source_offset_secs * c.stretch).min(data.frames());
            let to = frames(c.source_offset_secs * c.stretch + c.length_secs).min(data.frames());
            peak = data.channels.iter().flat_map(|ch| &ch[from..to]).fold(peak, |a, s| a.max(s.abs()));
        }
        Ok(peak)
    }

    /// Set an audio region's gain so its peak reaches `target_db` (dBFS). Returns the region.
    pub fn normalize_clip(&mut self, track: usize, clip: usize, target_db: f32) -> Result<Clip> {
        if !(-60.0..=0.0).contains(&target_db) {
            bail!("normalize target must be within -60 to 0 dBFS");
        }
        let peak = self.clip_peak(track, clip)?;
        if peak < 1e-6 {
            bail!("the region is silent");
        }
        let gain_db = (target_db - 20.0 * peak.log10()).clamp(CLIP_GAIN_DB.0, CLIP_GAIN_DB.1);
        let index = self.clip_index(track, clip)?;
        self.record("Normalize Region", None, |e| {
            let c = &mut e.project.tracks[index].clips[clip];
            c.gain = 10f32.powf(gain_db / 20.0);
            let c = c.clone();
            e.resync_clips(index)?;
            Ok(c)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::tone_file;
    use super::*;

    #[test]
    fn region_gain_and_normalize() {
        let dir = std::env::temp_dir().join(format!("opentake-gain-{}", std::process::id()));
        let wav = tone_file(&dir, 1.0); // peaks at 0.5 (-6 dBFS)
        let mut e = Engine::new();
        e.add_track("Vox").unwrap();
        e.add_clip(0, &wav, 0.0).unwrap();

        // A drag of gain changes is one undo step.
        e.set_clip_gain(0, 0, -3.0).unwrap();
        let c = e.set_clip_gain(0, 0, -6.0).unwrap();
        assert!((c.gain - 0.501).abs() < 1e-3);
        assert!(e.set_clip_gain(0, 0, 30.0).is_err());

        assert!((e.clip_peak(0, 0).unwrap() - 0.5).abs() < 1e-3);
        let c = e.normalize_clip(0, 0, 0.0).unwrap();
        assert!((c.gain - 2.0).abs() < 1e-2, "{}", c.gain);
        let c = e.normalize_clip(0, 0, -1.0).unwrap();
        assert!((20.0 * (0.5 * c.gain).log10() + 1.0).abs() < 0.01);

        e.undo().unwrap();
        e.undo().unwrap();
        assert_eq!(e.history_info().undo.as_deref(), Some("Region Gain"));
        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].clips[0].gain, 1.0);
        std::fs::remove_dir_all(dir).ok();
    }
}
