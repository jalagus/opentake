//! A second-order filter section shared by the built-in dynamics' detectors.

/// RBJ biquad (transposed direct form II), mono.
#[derive(Clone, Copy, Default)]
pub(crate) struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    /// Low- or high-pass at `hz` with quality `q` (0.707 = Butterworth).
    pub fn pass(&mut self, hz: f64, q: f64, sample_rate: f64, high: bool) {
        let w = std::f64::consts::TAU * (hz / sample_rate).clamp(1e-5, 0.49);
        let (sin, cos) = w.sin_cos();
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let (b0, b1) = if high { ((1.0 + cos) / 2.0, -(1.0 + cos)) } else { ((1.0 - cos) / 2.0, 1.0 - cos) };
        self.b = [b0 / a0, b1 / a0, b0 / a0];
        self.a = [-2.0 * cos / a0, (1.0 - alpha) / a0];
    }

    pub fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        // Flush denormals so silence stays cheap.
        if self.z[0].abs() < 1e-25 {
            self.z[0] = 0.0;
        }
        if self.z[1].abs() < 1e-25 {
            self.z[1] = 0.0;
        }
        y
    }
}

/// Two sections in series.
pub(crate) fn chain(s: &mut [Biquad; 2], x: f64) -> f64 {
    let y = s[0].run(x);
    s[1].run(y)
}

/// One-pole smoothing coefficient for a time constant in milliseconds.
pub(crate) fn coefficient(ms: f64, sample_rate: f64) -> f64 {
    (-1.0 / (ms.max(0.001) * 0.001 * sample_rate)).exp()
}

pub(crate) fn db(x: f64) -> f64 {
    20.0 * x.max(1e-7).log10()
}

pub(crate) fn log_range(v: f64, lo: f64, hi: f64) -> f64 {
    lo * (hi / lo).powf(v.clamp(0.0, 1.0))
}

pub(crate) fn norm_log(x: f64, lo: f64, hi: f64) -> f64 {
    ((x.clamp(lo, hi) / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}
