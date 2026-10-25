//! Export the mix and/or one stem per track to WAV (16-bit with dither, 24-bit, 32-bit
//! float) or FLAC (16/24-bit), rendered offline at the chosen sample rate.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Engine;
use super::analysis::{MixJob, lufs, peak_db, round1};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    Wav,
    Flac,
}

impl AudioFormat {
    pub fn extension(self) -> &'static str {
        match self {
            AudioFormat::Wav => "wav",
            AudioFormat::Flac => "flac",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: AudioFormat,
    /// 16, 24, or 32 (float; WAV only).
    pub bit_depth: u16,
    pub sample_rate: u32,
    /// Where to write the full mix (the format's extension is added when missing).
    pub mix_path: Option<PathBuf>,
    /// Folder for one file per track (audible tracks only).
    pub stems_dir: Option<PathBuf>,
    /// Range in seconds; `None` = the whole song.
    pub range: Option<(f64, f64)>,
    /// Extra seconds after the end, for delay repeats and other tails.
    pub tail_secs: f64,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self { format: AudioFormat::Wav, bit_depth: 24, sample_rate: 48_000, mix_path: None, stems_dir: None, range: None, tail_secs: 1.0 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportedFile {
    pub path: PathBuf,
    /// "mix" or "stem".
    pub kind: &'static str,
    pub name: String,
    pub peak_db: f64,
    pub lufs: f64,
    /// Samples that were above full scale and got clipped in the file.
    pub clipped_samples: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub files: Vec<ExportedFile>,
    pub seconds: f64,
    pub sample_rate: u32,
    pub format: AudioFormat,
    pub bit_depth: u16,
    pub warnings: Vec<String>,
}

pub const SAMPLE_RATES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

impl ExportOptions {
    pub fn validate(&self) -> Result<()> {
        if self.mix_path.is_none() && self.stems_dir.is_none() {
            bail!("nothing to export: give a mix path and/or a stems folder");
        }
        if !SAMPLE_RATES.contains(&self.sample_rate) {
            bail!("unsupported sample rate {} (use one of {SAMPLE_RATES:?})", self.sample_rate);
        }
        match (self.format, self.bit_depth) {
            (AudioFormat::Wav, 16 | 24 | 32) | (AudioFormat::Flac, 16 | 24) => Ok(()),
            (AudioFormat::Flac, b) => bail!("FLAC supports 16 or 24 bits, not {b}"),
            (AudioFormat::Wav, b) => bail!("WAV supports 16, 24 or 32 (float) bits, not {b}"),
        }
    }
}

impl Engine {
    /// Snapshot for an export (call [`MixJob::export`] on it, e.g. off the engine lock).
    pub fn export_job(&mut self, options: &ExportOptions) -> Result<MixJob> {
        options.validate()?;
        Ok(self.mix_job_at(options.sample_rate as f64))
    }

    /// Render and write the files (holds the engine while it works; for the GUI and MCP
    /// use [`Engine::export_job`] and export off the lock).
    pub fn export_audio(&mut self, options: &ExportOptions) -> Result<ExportReport> {
        self.export_job(options)?.export(options)
    }
}

impl MixJob {
    pub fn export(self, options: &ExportOptions) -> Result<ExportReport> {
        options.validate()?;
        let (from, to) = match options.range {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        };
        let name = self.project_name().to_string();
        let rendered = self.render(from, to, options.tail_secs);
        let sr = rendered.sample_rate;
        let mut files = Vec::new();
        if let Some(path) = &options.mix_path {
            let path = with_extension(path, options.format);
            let clipped = write_audio(&path, &rendered.master, options)?;
            files.push(report(path, "mix", &name, &rendered.master, sr, clipped));
        }
        if let Some(dir) = &options.stems_dir {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            for (i, (track, audio, audible)) in rendered.tracks.iter().enumerate() {
                if !audible || audio.iter().flatten().all(|v| v.abs() < 1e-7) {
                    continue;
                }
                let file = dir.join(format!("{:02} {}.{}", i + 1, file_safe(track), options.format.extension()));
                let clipped = write_audio(&file, audio, options)?;
                files.push(report(file, "stem", track, audio, sr, clipped));
            }
        }
        Ok(ExportReport {
            files,
            seconds: round1(rendered.master[0].len() as f64 / sr),
            sample_rate: options.sample_rate,
            format: options.format,
            bit_depth: options.bit_depth,
            warnings: rendered.warnings,
        })
    }
}

fn report(path: PathBuf, kind: &'static str, name: &str, audio: &[Vec<f32>; 2], sr: f64, clipped: usize) -> ExportedFile {
    ExportedFile { path, kind, name: name.to_string(), peak_db: peak_db(&audio[0], &audio[1]), lufs: round1(lufs(&audio[0], &audio[1], sr)), clipped_samples: clipped }
}

fn with_extension(path: &Path, format: AudioFormat) -> PathBuf {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case(format.extension()) => path.to_path_buf(),
        _ => {
            let mut p = path.as_os_str().to_owned();
            p.push(".");
            p.push(format.extension());
            PathBuf::from(p)
        }
    }
}

fn file_safe(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { '_' }).collect();
    let s = s.trim().to_string();
    if s.is_empty() { "Track".into() } else { s }
}

/// Integer samples for `bits`, with TPDF dither at 16 bits. Returns (interleaved, clipped).
fn quantize(audio: &[Vec<f32>; 2], bits: u16) -> (Vec<i32>, usize) {
    let max = ((1i64 << (bits - 1)) - 1) as f64;
    let mut seed = 0x9e37_79b9u32;
    let mut noise = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f64 / u32::MAX as f64
    };
    let dither = bits <= 16;
    let mut clipped = 0;
    let mut out = Vec::with_capacity(audio[0].len() * 2);
    for i in 0..audio[0].len() {
        for ch in audio {
            let mut v = ch[i] as f64 * max;
            if dither {
                v += noise() - noise(); // triangular, ±1 LSB
            }
            if v.abs() > max + 0.5 {
                clipped += 1;
            }
            out.push(v.round().clamp(-max - 1.0, max) as i32);
        }
    }
    (out, clipped)
}

/// Write a stereo file. Returns how many samples were clipped.
fn write_audio(path: &Path, audio: &[Vec<f32>; 2], options: &ExportOptions) -> Result<usize> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let sr = options.sample_rate;
    match (options.format, options.bit_depth) {
        (AudioFormat::Wav, 32) => {
            let spec = hound::WavSpec { channels: 2, sample_rate: sr, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
            let mut w = hound::WavWriter::create(path, spec).with_context(|| format!("creating {}", path.display()))?;
            for i in 0..audio[0].len() {
                w.write_sample(audio[0][i])?;
                w.write_sample(audio[1][i])?;
            }
            w.finalize()?;
            Ok(0) // float keeps everything, even above full scale
        }
        (AudioFormat::Wav, bits) => {
            let spec = hound::WavSpec { channels: 2, sample_rate: sr, bits_per_sample: bits, sample_format: hound::SampleFormat::Int };
            let mut w = hound::WavWriter::create(path, spec).with_context(|| format!("creating {}", path.display()))?;
            let (samples, clipped) = quantize(audio, bits);
            for s in samples {
                w.write_sample(s)?;
            }
            w.finalize()?;
            Ok(clipped)
        }
        (AudioFormat::Flac, bits) => {
            use flacenc::component::BitRepr;
            use flacenc::error::Verify;
            let (samples, clipped) = quantize(audio, bits);
            let config = flacenc::config::Encoder::default().into_verified().map_err(|e| anyhow::anyhow!("FLAC encoder config: {e:?}"))?;
            let source = flacenc::source::MemSource::from_samples(&samples, 2, bits as usize, sr as usize);
            let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size).map_err(|e| anyhow::anyhow!("FLAC encoding failed: {e:?}"))?;
            let mut sink = flacenc::bitsink::ByteSink::new();
            stream.write(&mut sink).map_err(|e| anyhow::anyhow!("FLAC encoding failed: {e:?}"))?;
            std::fs::write(path, sink.as_slice()).with_context(|| format!("writing {}", path.display()))?;
            Ok(clipped)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentake_project::Note;

    fn song() -> Engine {
        let mut e = Engine::new();
        for (name, pitch) in [("Bass", 40u8), ("Lead", 64)] {
            let t = e.add_instrument_track(name, "synth").unwrap();
            let c = e.add_midi_clip(t, 0.0, 2.0, None).unwrap();
            let notes = (0..4).map(|i| Note { start_secs: i as f64 * 0.5, length_secs: 0.4, pitch, velocity: 100, tab: None }).collect();
            e.set_midi_notes(t, c, notes).unwrap();
        }
        e
    }

    #[test]
    fn exports_mix_and_stems_as_wav_and_flac() {
        let dir = std::env::temp_dir().join(format!("opentake-export-{}", std::process::id()));
        let mut e = song();
        let opts = ExportOptions { mix_path: Some(dir.join("song")), stems_dir: Some(dir.join("stems")), tail_secs: 0.5, ..Default::default() };
        let report = e.export_audio(&opts).unwrap();
        assert_eq!(report.files.len(), 3, "{report:?}");
        let mix = &report.files[0];
        assert_eq!(mix.path, dir.join("song.wav"));
        assert_eq!(mix.clipped_samples, 0);
        let reader = hound::WavReader::open(&mix.path).unwrap();
        assert_eq!((reader.spec().channels, reader.spec().sample_rate, reader.spec().bits_per_sample), (2, 48_000, 24));
        assert_eq!(reader.duration(), 120_000, "2 s song + 0.5 s tail");
        assert!(dir.join("stems").join("01 Bass.wav").exists() && dir.join("stems").join("02 Lead.wav").exists());

        // 16-bit FLAC at 44.1 kHz.
        let flac = ExportOptions { format: AudioFormat::Flac, bit_depth: 16, sample_rate: 44_100, mix_path: Some(dir.join("song.flac")), ..Default::default() };
        let report = e.export_audio(&flac).unwrap();
        let bytes = std::fs::read(&report.files[0].path).unwrap();
        assert_eq!(&bytes[..4], b"fLaC");
        assert!(report.files[0].lufs > -60.0);

        // A muted track is left out of the mix and the stems.
        e.update_track(1, crate::TrackUpdate { mute: Some(true), ..Default::default() }).unwrap();
        let only = ExportOptions { stems_dir: Some(dir.join("stems2")), ..Default::default() };
        let report = e.export_audio(&only).unwrap();
        assert_eq!(report.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Bass"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_bad_settings() {
        let mut e = song();
        let base = ExportOptions { mix_path: Some(std::env::temp_dir().join("x")), ..Default::default() };
        assert!(e.export_audio(&ExportOptions { format: AudioFormat::Flac, bit_depth: 32, ..base.clone() }).is_err());
        assert!(e.export_audio(&ExportOptions { sample_rate: 12_345, ..base.clone() }).is_err());
        assert!(e.export_audio(&ExportOptions { mix_path: None, ..base }).is_err());
    }

    #[test]
    fn sixteen_bit_is_dithered_and_clipping_is_counted() {
        let loud = [vec![1.5f32; 100], vec![0.0f32; 100]];
        let (_, clipped) = quantize(&loud, 16);
        assert_eq!(clipped, 100);
        let quiet = [vec![0.0f32; 1000], vec![0.0f32; 1000]];
        let (s, _) = quantize(&quiet, 16);
        assert!(s.iter().any(|&v| v != 0), "silence gets dither noise at 16 bits");
        let (s, _) = quantize(&quiet, 24);
        assert!(s.iter().all(|&v| v == 0), "no dither at 24 bits");
    }
}
