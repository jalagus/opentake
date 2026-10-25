//! Loopback latency measurement. The mixer plays a test signal (short chirps at irregular
//! intervals) while capturing an input; with the output looped back to that input (a cable,
//! or speakers into a microphone) the lag of the best match is the true round trip, in the
//! same terms recordings are placed in. Devices don't always report their latency
//! correctly (the built-in Mac audio is off by tens of milliseconds), so this calibrates it.

use anyhow::{Result, bail};
use serde::Serialize;

/// Chirp start times (seconds). Irregular, so no lag matches a shifted copy of itself.
const CHIRPS: [f64; 5] = [0.10, 0.39, 0.71, 0.98, 1.33];
const CHIRP_SECS: f64 = 0.004;
/// Longest round trip looked for.
pub const MAX_ROUND_TRIP_SECS: f64 = 0.5;

/// Samples to capture: the whole signal plus the longest round trip.
pub fn capture_len(sample_rate: f64) -> usize {
    ((CHIRPS[CHIRPS.len() - 1] + CHIRP_SECS + MAX_ROUND_TRIP_SECS + 0.05) * sample_rate) as usize
}

/// The test signal at sample `index`: Hann-windowed chirps sweeping 600 Hz to 6 kHz.
pub fn signal(index: usize, sample_rate: f64) -> f32 {
    let t = index as f64 / sample_rate;
    for start in CHIRPS {
        let x = t - start;
        if (0.0..CHIRP_SECS).contains(&x) {
            let (f0, f1) = (600.0, 6000.0);
            let phase = std::f64::consts::TAU * (f0 * x + (f1 - f0) * x * x / (2.0 * CHIRP_SECS));
            let window = 0.5 - 0.5 * (std::f64::consts::TAU * x / CHIRP_SECS).cos();
            return (0.5 * window * phase.sin()) as f32;
        }
    }
    0.0
}

#[derive(Debug, Clone, Serialize)]
pub struct LoopbackMatch {
    /// Round trip in samples.
    pub lag: usize,
    /// How far the best match stands out from the next best (> 1; under ~1.5 is doubtful).
    pub clarity: f64,
    /// Peak level of the captured input (dBFS).
    pub input_peak_db: f64,
}

/// Find the test signal in `captured`.
pub fn find(captured: &[f32], sample_rate: f64) -> Result<LoopbackMatch> {
    let peak = captured.iter().fold(0.0f32, |a, s| a.max(s.abs()));
    let input_peak_db = if peak > 0.0 { 20.0 * (peak as f64).log10() } else { -120.0 };
    if peak < 1e-4 {
        bail!("no signal came back on the input — connect the output to it (or turn the speakers up near the microphone)");
    }
    // The signal is sparse, so correlate over its non-zero samples only.
    let reference: Vec<(usize, f32)> =
        (0..capture_len(sample_rate)).map(|i| (i, signal(i, sample_rate))).filter(|&(_, v)| v != 0.0).collect();
    let max_lag = ((MAX_ROUND_TRIP_SECS * sample_rate) as usize).min(captured.len().saturating_sub(reference.last().map_or(0, |r| r.0) + 1));
    let corr: Vec<f64> = (0..max_lag)
        .map(|lag| reference.iter().map(|&(i, v)| v as f64 * captured[i + lag] as f64).sum::<f64>().abs())
        .collect();
    let Some((lag, &best)) = corr.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) else {
        bail!("capture too short");
    };
    // The next best match away from the peak (and its own ringing).
    let guard = (0.002 * sample_rate) as usize;
    let runner_up = corr.iter().enumerate().filter(|(i, _)| i.abs_diff(lag) > guard).map(|(_, &v)| v).fold(0.0, f64::max);
    let clarity = if runner_up > 0.0 { best / runner_up } else { f64::INFINITY };
    if clarity < 1.3 {
        bail!("the test signal wasn't clear on the input (too noisy or too quiet); input peak {input_peak_db:.0} dBFS");
    }
    Ok(LoopbackMatch { lag, clarity, input_peak_db })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_delayed_noisy_copy() {
        let sr = 48_000.0;
        let delay = 3_117;
        let mut seed = 1u32;
        let mut noise = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        // Quieter and inverted, with noise: a microphone across the room.
        let captured: Vec<f32> =
            (0..capture_len(sr)).map(|i| if i >= delay { -0.05 * signal(i - delay, sr) } else { 0.0 } + 0.01 * noise()).collect();
        let m = find(&captured, sr).unwrap();
        assert_eq!(m.lag, delay);
        assert!(m.clarity > 2.0, "{}", m.clarity);
    }

    #[test]
    fn silence_or_noise_is_an_error() {
        let sr = 48_000.0;
        assert!(find(&vec![0.0; capture_len(sr)], sr).is_err());
        let mut seed = 7u32;
        let noise: Vec<f32> = (0..capture_len(sr))
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        assert!(find(&noise, sr).is_err());
    }
}
