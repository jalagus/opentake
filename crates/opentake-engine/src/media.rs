//! Loading audio files into memory.

use std::path::Path;

use anyhow::{Context, Result, bail};

/// De-interleaved audio at the engine's sample rate.
#[derive(Debug)]
pub struct AudioData {
    pub channels: Vec<Vec<f32>>,
    pub sample_rate: u32,
}

impl AudioData {
    pub fn frames(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }
}

/// Load a WAV file, resampling to `target_rate` if needed.
pub fn load_wav(path: &Path, target_rate: u32) -> Result<AudioData> {
    let mut reader = hound::WavReader::open(path).with_context(|| format!("opening {}", path.display()))?;
    let spec = reader.spec();
    let n_ch = spec.channels.max(1) as usize;

    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    if interleaved.is_empty() {
        bail!("{} contains no audio", path.display());
    }

    let mut channels: Vec<Vec<f32>> = (0..n_ch).map(|ch| interleaved.iter().skip(ch).step_by(n_ch).copied().collect()).collect();
    if spec.sample_rate != target_rate {
        for ch in &mut channels {
            *ch = resample_linear(ch, spec.sample_rate, target_rate);
        }
    }
    Ok(AudioData { channels, sample_rate: target_rate })
}

/// Simple linear-interpolation resampler. Good enough for previewing; a proper
/// band-limited resampler (e.g. rubato) should replace it.
pub fn resample_linear(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    let ratio = from as f64 / to as f64;
    let out_len = (input.len() as f64 / ratio).round() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input.get(idx).copied().unwrap_or(0.0);
            let b = input.get(idx + 1).copied().unwrap_or(a);
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_doubles_length() {
        let out = resample_linear(&[0.0, 1.0, 0.0, -1.0], 24_000, 48_000);
        assert_eq!(out.len(), 8);
        assert_eq!(out[1], 0.5);
    }
}
