//! Comping take folders (see `opentake_project::comp`): which take plays where, and
//! flattening a comp into one plain region.

use anyhow::{Result, bail};
use opentake_project::{Clip, CompSegment, Fade};

use super::Engine;

impl Engine {
    /// A take folder (an error for a plain region).
    fn folder(&self, track: usize, clip: usize) -> Result<(usize, &Clip)> {
        let index = self.clip_index(track, clip)?;
        let c = &self.project.tracks[index].clips[clip];
        if c.takes.is_empty() {
            bail!("region '{}' is not a take folder (cycle-record over it to make takes)", c.name);
        }
        Ok((index, c))
    }

    /// Apply a comp change to a take folder as one undo step (`merge` groups a swipe).
    fn edit_comp(&mut self, track: usize, clip: usize, label: &str, merge: Option<String>, f: impl FnOnce(&mut Clip) -> Result<()>) -> Result<Clip> {
        let (index, c) = self.folder(track, clip)?;
        let mut c = c.clone();
        f(&mut c)?;
        self.record(label, merge, |e| {
            e.project.tracks[index].clips[clip] = c.clone();
            e.resync_clips(index)?;
            Ok(c)
        })
    }

    /// Play take `take` of a take folder from `from` to `to` seconds after the region start,
    /// splitting or replacing the comp segments there (the active take plays the rest).
    pub fn set_comp_segment(&mut self, track: usize, clip: usize, from: f64, to: f64, take: usize) -> Result<Clip> {
        if !(from.is_finite() && to.is_finite()) {
            bail!("comp range must be finite seconds");
        }
        self.edit_comp(track, clip, "Comp Take", None, |c| match c.set_comp_range(from, to, take) {
            true => Ok(()),
            false => bail!("region '{}' has {} take(s)", c.name, c.takes.len()),
        })
    }

    /// Replace a take folder's whole comp (segments relative to the region start; tidied up as
    /// `set_comp_range` would). `merge` makes a swipe one undo step.
    pub fn set_comp(&mut self, track: usize, clip: usize, comp: Vec<CompSegment>, merge: Option<String>) -> Result<Clip> {
        self.edit_comp(track, clip, "Comp Take", merge, |c| {
            if let Some(s) = comp.iter().find(|s| s.take >= c.takes.len() || !(s.start_secs.is_finite() && s.end_secs.is_finite())) {
                bail!("bad comp segment {s:?} for '{}' ({} takes)", c.name, c.takes.len());
            }
            c.comp = comp;
            c.normalize_comp();
            Ok(())
        })
    }

    /// Drop a take folder's comp: the active take plays all through.
    pub fn clear_comp(&mut self, track: usize, clip: usize) -> Result<Clip> {
        self.edit_comp(track, clip, "Clear Comp", None, |c| {
            c.comp.clear();
            Ok(())
        })
    }

    /// Render what a take folder plays (its comp, crossfades at the joins, pitch/stretch) into
    /// a new file and make the folder a plain region playing it, keeping its place, gain and
    /// fades. One undo step; the take files stay on disk.
    pub fn flatten_comp(&mut self, track: usize, clip: usize) -> Result<Clip> {
        let (index, c) = self.folder(track, clip)?;
        let c = c.clone();
        // Gain and fades stay on the region, so render without them.
        let bare = Clip { gain: 1.0, mute: false, fade_in: Fade::default(), fade_out: Fade::default(), ..c.clone() };
        let (file, _) = self.bounce(&[bare], c.position_secs, c.position_secs + c.length_secs, "comp")?;
        let flat = Clip {
            file,
            source_offset_secs: 0.0,
            takes: Vec::new(),
            active_take: 0,
            comp: Vec::new(),
            show_takes: false,
            pitch_semitones: 0,
            pitch_cents: 0,
            stretch: 1.0,
            ..c
        };
        self.record("Flatten Comp", None, |e| {
            e.project.tracks[index].clips[clip] = flat.clone();
            e.resync_clips(index)?;
            Ok(flat)
        })
    }

    /// Open or close a take folder's take lanes. A view setting, not an edit.
    pub fn set_takes_shown(&mut self, track: usize, clip: usize, show: bool) -> Result<()> {
        let (index, _) = self.folder(track, clip)?;
        self.project.tracks[index].clips[clip].show_takes = show;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::path::{Path, PathBuf};

    use super::*;

    /// A 2 s mono file at 48 kHz: `freq` Hz at -6 dBFS.
    fn sine(dir: &Path, name: &str, freq: f32) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..96_000 {
            w.write_sample((TAU * freq * i as f32 / 48_000.0).sin() * 0.5).unwrap();
        }
        w.finalize().unwrap();
        path
    }

    /// A 1 s take folder at 0.5 s: take 1 a 220 Hz sine, take 2 (active) 660 Hz, both
    /// from 0.5 s into their files.
    fn engine(name: &str) -> (Engine, PathBuf) {
        let dir = std::env::temp_dir().join(format!("opentake-comp-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let (low, high) = (sine(&dir, "low.wav", 220.0), sine(&dir, "high.wav", 660.0));
        let mut e = Engine::new();
        e.project_path = Some(dir.join("song.otp"));
        e.add_track("Vox").unwrap();
        e.add_clip(0, &low, 0.5).unwrap();
        let mut folder = e.remove_clip(0, 0).unwrap();
        (folder.length_secs, folder.source_offset_secs) = (1.0, 0.5);
        folder.push_take("Take 2".into(), high, 0.5);
        e.insert_clip(0, folder).unwrap();
        (e, dir)
    }

    /// Zero crossings per second over `range` (seconds) of a render: twice the frequency.
    fn crossings(s: &[f32], from: f64, to: f64) -> f64 {
        let (a, b) = ((from * 48_000.0) as usize, (to * 48_000.0) as usize);
        s[a..b].windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count() as f64 / (to - from)
    }

    #[test]
    fn comp_plays_each_take_in_its_range_without_a_click() {
        let (mut e, dir) = engine("render");
        assert!(e.set_comp_segment(0, 0, 0.0, 0.5, 5).is_err());
        assert!(e.set_comp_segment(0, 1, 0.0, 0.5, 0).is_err());
        // Take 1 for the first half: 220 Hz until 1.0 s, then take 2's 660 Hz.
        let c = e.set_comp_segment(0, 0, -1.0, 0.5, 0).unwrap();
        assert_eq!(c.comp, vec![CompSegment { start_secs: 0.0, end_secs: 0.5, take: 0 }]);
        assert_eq!(e.history_info().undo.as_deref(), Some("Comp Take"));

        // The audio thread gets one clip per segment, crossfaded over 10 ms at the join.
        let rt = e.build_clips(0, 48_000);
        assert_eq!(rt.len(), 2);
        assert_eq!((rt[0].start, rt[0].len, rt[1].start, rt[1].len), (24_000, 24_240, 47_760, 24_240));
        assert_eq!((rt[0].fades.fade_out, rt[1].fades.fade_in), (480, 480));
        // Each reads its own take at the same place in the performance (0.5 s into the file).
        assert_eq!((rt[0].offset, rt[1].offset), (24_000, 47_760));
        assert_eq!((rt[0].data.channels[0][24_000 + 100], rt[1].data.channels[0][47_760 + 100]), (sine_at(220.0, 24_100), sine_at(660.0, 47_860)));

        let s = e.mix_job().render(None, None, 0.0).master[0].clone();
        assert!((crossings(&s, 0.55, 0.95) - 440.0).abs() < 10.0, "take 1 first");
        assert!((crossings(&s, 1.05, 1.45) - 1320.0).abs() < 10.0, "then take 2");
        // No click: around the join the signal moves no faster than the 660 Hz sine does.
        let natural = 0.5 * TAU * 660.0 / 48_000.0;
        let step = (47_000..49_000).map(|i| (s[i + 1] - s[i]).abs()).fold(0.0, f32::max);
        assert!(step < natural * 1.2, "{step} vs {natural}");
        std::fs::remove_dir_all(dir).ok();
    }

    fn sine_at(freq: f32, i: usize) -> f32 {
        (TAU * freq * i as f32 / 48_000.0).sin() * 0.5
    }

    #[test]
    fn region_edits_carry_the_comp_and_swipes_merge() {
        let (mut e, dir) = engine("edits");
        // A swipe: three updates of one gesture are one undo step.
        let seg = |a, b, take| CompSegment { start_secs: a, end_secs: b, take };
        for end in [0.2, 0.4, 0.3] {
            e.set_comp(0, 0, vec![seg(0.1, end, 0)], Some("swipe".into())).unwrap();
        }
        assert_eq!(e.project().tracks[0].clips[0].comp, vec![seg(0.1, 0.3, 0)]);
        e.undo().unwrap();
        assert!(e.project().tracks[0].clips[0].comp.is_empty(), "the whole swipe undone");
        e.redo().unwrap();

        // Trimming the start moves the comp with the audio; splitting shares it out.
        let c = e.trim_clip(0, 0, Some(0.7), None).unwrap();
        assert_eq!(c.comp.len(), 1);
        assert!((c.comp[0].start_secs - 0.0).abs() < 1e-9 && (c.comp[0].end_secs - 0.1).abs() < 1e-9, "{:?}", c.comp);
        e.undo().unwrap();
        let (l, r) = e.split_clip(0, 0, 0.7).unwrap();
        assert_eq!((l.comp.len(), r.comp.len()), (1, 1));
        assert!((r.comp[0].end_secs - 0.1).abs() < 1e-9);
        e.undo().unwrap();
        // Stretching to double length doubles the segment.
        let c = e.set_clip_pitch_stretch(0, 0, opentake_engine_ps(2.0)).unwrap();
        assert!((c.comp[0].start_secs - 0.2).abs() < 1e-9 && (c.comp[0].end_secs - 0.6).abs() < 1e-9);
        e.undo().unwrap();
        // Picking a take clears the comp; so does clear_comp.
        e.select_take(0, 0, 0).unwrap();
        assert!(e.project().tracks[0].clips[0].comp.is_empty());
        e.undo().unwrap();
        assert!(e.clear_comp(0, 0).unwrap().comp.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    fn opentake_engine_ps(stretch: f64) -> super::super::PitchStretch {
        super::super::PitchStretch { stretch: Some(stretch), ..Default::default() }
    }

    #[test]
    fn flatten_renders_the_comp_into_one_region_and_undo_restores() {
        let (mut e, dir) = engine("flatten");
        e.set_comp_segment(0, 0, 0.0, 0.5, 0).unwrap();
        e.set_clip_gain(0, 0, -6.0206).unwrap();
        let before = e.project().tracks[0].clips.clone();
        let flat = e.flatten_comp(0, 0).unwrap();
        assert_eq!(e.project().tracks[0].clips.len(), 1);
        assert!(flat.takes.is_empty() && flat.comp.is_empty() && flat.file.starts_with(dir.join("Audio")));
        assert_eq!((flat.position_secs, flat.length_secs, flat.source_offset_secs), (0.5, 1.0, 0.0));
        assert!((flat.gain - 0.5).abs() < 1e-3, "gain stays on the region");
        let s: Vec<f32> = hound::WavReader::open(&flat.file).unwrap().samples::<f32>().map(Result::unwrap).collect();
        assert_eq!(s.len(), 48_000);
        assert!((crossings(&s, 0.05, 0.45) - 440.0).abs() < 10.0 && (crossings(&s, 0.55, 0.95) - 1320.0).abs() < 10.0);
        assert!(s.iter().fold(0.0f32, |m, v| m.max(v.abs())) > 0.45, "rendered at unity");
        // The flattened region sounds like the comp did.
        let mix = e.mix_job().render(None, None, 0.0).master[0].clone();
        assert!((crossings(&mix, 0.55, 0.95) - 440.0).abs() < 10.0);
        assert!(e.flatten_comp(0, 0).is_err(), "not a folder any more");

        e.undo().unwrap();
        assert_eq!(e.project().tracks[0].clips, before);
        assert_eq!(e.history_info().undo.as_deref(), Some("Region Gain"));
        std::fs::remove_dir_all(dir).ok();
    }
}
