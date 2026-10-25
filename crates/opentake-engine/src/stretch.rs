//! Offline pitch shifting and time stretching of audio regions, using Signalsmith Stretch
//! (MIT). Runs on the control side; the audio thread just plays the result.

use signalsmith_stretch::Stretch;

use crate::media::AudioData;

/// Frequency multiplier for a transposition in semitones.
pub fn pitch_factor(semitones: f64) -> f64 {
    (semitones / 12.0).exp2()
}

/// `data` transposed by `semitones` and played `ratio` times as long (2.0 = half speed).
pub fn process(data: &AudioData, semitones: f64, ratio: f64) -> AudioData {
    let ch = data.channels.len().max(1);
    let n = data.frames();
    let out_len = (n as f64 * ratio).round() as usize;
    let mut st = Stretch::preset_default(ch as u32, data.sample_rate);
    st.set_transpose_factor(pitch_factor(semitones) as f32, None);
    // `exact` needs an output a few windows long; pad very short audio with silence.
    let min = st.output_latency() * 2 + 1;
    let pad = if out_len < min { ((min - out_len) as f64 / ratio).ceil() as usize + 1 } else { 0 };
    let total_in = n + pad;
    let total_out = ((total_in as f64) * ratio).round() as usize;
    let mut input = vec![0.0f32; total_in * ch];
    for (c, samples) in data.channels.iter().enumerate() {
        for (i, &s) in samples.iter().enumerate() {
            input[i * ch + c] = s;
        }
    }
    let mut output = vec![0.0f32; total_out.max(min) * ch];
    st.exact(&input, &mut output);
    let channels = (0..data.channels.len()).map(|c| (0..out_len).map(|i| output[i * ch + c]).collect()).collect();
    AudioData { channels, sample_rate: data.sample_rate }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, secs: f64, sr: u32) -> AudioData {
        let n = (secs * sr as f64) as usize;
        let s: Vec<f32> = (0..n).map(|i| (std::f64::consts::TAU * freq * i as f64 / sr as f64).sin() as f32 * 0.5).collect();
        AudioData { channels: vec![s.clone(), s], sample_rate: sr }
    }

    /// Frequency from upward zero crossings over the middle of the signal (skips the edges).
    fn freq(s: &[f32], sr: u32) -> f64 {
        let mid = &s[s.len() / 4..s.len() * 3 / 4];
        let ups: Vec<usize> = (1..mid.len()).filter(|&i| mid[i - 1] < 0.0 && mid[i] >= 0.0).collect();
        (ups.len() - 1) as f64 * sr as f64 / (ups[ups.len() - 1] - ups[0]) as f64
    }

    #[test]
    fn stretch_keeps_pitch_and_scales_length() {
        let src = sine(440.0, 1.0, 48_000);
        let out = process(&src, 0.0, 2.0);
        assert_eq!(out.frames(), 96_000);
        assert!((freq(&out.channels[0], 48_000) - 440.0).abs() < 3.0, "{}", freq(&out.channels[0], 48_000));
        let half = process(&src, 0.0, 0.5);
        assert_eq!(half.frames(), 24_000);
        assert!((freq(&half.channels[1], 48_000) - 440.0).abs() < 3.0);
    }

    #[test]
    fn octave_up_doubles_frequency_at_same_length() {
        let src = sine(330.0, 1.0, 48_000);
        let out = process(&src, 12.0, 1.0);
        assert_eq!(out.frames(), src.frames());
        assert!((freq(&out.channels[0], 48_000) - 660.0).abs() < 5.0, "{}", freq(&out.channels[0], 48_000));
        // Cents: +50 cents is a quarter tone.
        let q = process(&src, 0.5, 1.0);
        assert!((freq(&q.channels[0], 48_000) - 330.0 * pitch_factor(0.5)).abs() < 3.0);
    }

    #[test]
    fn very_short_audio_still_renders() {
        let out = process(&sine(440.0, 0.02, 48_000), 3.0, 1.5);
        assert_eq!(out.frames(), 1440);
        assert!(out.channels[0].iter().all(|v| v.is_finite()));
    }
}
