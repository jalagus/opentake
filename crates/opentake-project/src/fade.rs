//! Region fades and crossfades.
//!
//! A region's own fades (`Clip::fade_in`/`fade_out`) are what the user set. Overlapping
//! regions on a track crossfade automatically: that crossfade isn't stored but derived from
//! the overlap whenever the track is built for playback or rendered ([`effective_fades`]),
//! so it follows moves and trims. The fade that plays is the longer of the two.

use std::f32::consts::{FRAC_PI_2, PI};

use serde::{Deserialize, Serialize};

use crate::Clip;

/// Shape of a fade, as a fade-in; a fade-out is its mirror image.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FadeCurve {
    #[default]
    Linear,
    /// Sine/cosine: a fade-out and a fade-in over the same span keep constant power.
    EqualPower,
    /// Starts slowly, then rises quickly.
    Exponential,
    /// Rises quickly, then settles.
    Logarithmic,
    /// Eases in and out.
    SCurve,
}

impl FadeCurve {
    pub const ALL: [FadeCurve; 5] = [FadeCurve::Linear, FadeCurve::EqualPower, FadeCurve::Exponential, FadeCurve::Logarithmic, FadeCurve::SCurve];

    pub fn label(self) -> &'static str {
        match self {
            FadeCurve::Linear => "Linear",
            FadeCurve::EqualPower => "Equal Power",
            FadeCurve::Exponential => "Exponential",
            FadeCurve::Logarithmic => "Logarithmic",
            FadeCurve::SCurve => "S-Curve",
        }
    }

    /// Gain `x` (0..1) of the way through a fade-in. Cheap enough to run per sample.
    pub fn gain(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            FadeCurve::Linear => x,
            FadeCurve::EqualPower => (x * FRAC_PI_2).sin(),
            FadeCurve::Exponential => x * x,
            FadeCurve::Logarithmic => 1.0 - (1.0 - x) * (1.0 - x),
            FadeCurve::SCurve => 0.5 - 0.5 * (x * PI).cos(),
        }
    }
}

/// A fade at one end of a region: its length in timeline seconds and its shape.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Fade {
    pub secs: f64,
    #[serde(default)]
    pub curve: FadeCurve,
}

impl Fade {
    pub fn new(secs: f64, curve: FadeCurve) -> Self {
        Self { secs, curve }
    }

    pub fn is_none(&self) -> bool {
        self.secs <= 0.0
    }

    /// This fade or `other`, whichever is longer.
    fn max(self, other: Fade) -> Fade {
        if other.secs > self.secs { other } else { self }
    }
}

/// The (fade-in, fade-out) each region plays with: its own fades, lengthened to an
/// equal-power crossfade over any overlap with another unmuted region. Only partial overlaps
/// crossfade (the later region fades in over the overlap, the earlier one fades out); a
/// region lying entirely inside another just adds to it.
pub fn effective_fades(clips: &[Clip]) -> Vec<(Fade, Fade)> {
    let mut out: Vec<(Fade, Fade)> = clips.iter().map(|c| (c.fade_in, c.fade_out)).collect();
    for i in 0..clips.len() {
        for j in 0..clips.len() {
            let (a, b) = (&clips[i], &clips[j]);
            if i == j || a.mute || b.mute {
                continue;
            }
            let (a_end, b_end) = (a.position_secs + a.length_secs, b.position_secs + b.length_secs);
            // `b` starts inside `a` and ends after it.
            if b.position_secs > a.position_secs && b.position_secs < a_end && b_end > a_end {
                let x = Fade::new(a_end - b.position_secs, FadeCurve::EqualPower);
                out[i].1 = out[i].1.max(x);
                out[j].0 = out[j].0.max(x);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::audio_clip;

    #[test]
    fn curves_run_from_silence_to_unity() {
        for c in FadeCurve::ALL {
            assert_eq!(c.gain(0.0), 0.0, "{c:?}");
            assert!((c.gain(1.0) - 1.0).abs() < 1e-6, "{c:?}");
            assert!(c.gain(0.25) <= c.gain(0.75), "{c:?} rises");
            assert_eq!(c.gain(-1.0), 0.0);
        }
        // Equal power: fade-in² + fade-out² = 1 everywhere, so -3 dB each at the midpoint.
        let p = FadeCurve::EqualPower;
        for x in [0.0, 0.1, 0.5, 0.9] {
            assert!((p.gain(x).powi(2) + p.gain(1.0 - x).powi(2) - 1.0).abs() < 1e-6);
        }
        assert!((p.gain(0.5) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        // A linear crossfade keeps constant amplitude instead.
        assert!((FadeCurve::Linear.gain(0.3) + FadeCurve::Linear.gain(0.7) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn overlaps_crossfade_and_own_fades_win_when_longer() {
        let mut a = audio_clip("a", "a.wav".into(), 0.0, 2.0, 0.0);
        let b = audio_clip("b", "b.wav".into(), 1.5, 2.0, 0.0);
        let inside = audio_clip("c", "c.wav".into(), 0.2, 0.5, 0.0);
        let f = effective_fades(&[a.clone(), b.clone(), inside.clone()]);
        assert_eq!(f[0].1, Fade::new(0.5, FadeCurve::EqualPower), "earlier region fades out over the overlap");
        assert_eq!(f[1].0, Fade::new(0.5, FadeCurve::EqualPower), "later region fades in");
        assert!(f[0].0.is_none() && f[1].1.is_none() && f[2] == (Fade::default(), Fade::default()), "contained region adds");

        a.fade_out = Fade::new(0.8, FadeCurve::SCurve);
        let f = effective_fades(&[a.clone(), b.clone()]);
        assert_eq!(f[0].1, a.fade_out);
        assert_eq!(f[1].0.secs, 0.5);
        a.mute = true;
        assert!(effective_fades(&[a, b])[1].0.is_none(), "muted regions don't crossfade");
    }
}
