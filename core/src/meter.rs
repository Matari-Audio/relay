//! A true-peak meter in the spirit of ITU-R BS.1770-4: 4x oversampling
//! through a 48-tap windowed-sinc interpolator, reporting the highest
//! reconstructed sample. Catches the inter-sample overs a codec or a DAC
//! would clip.

use crate::CHANNELS;

const PHASES: usize = 4;
const TAPS: usize = 12;

pub struct TruePeak {
    /// Per phase, `TAPS` coefficients, each phase summing to 1.
    coef: [[f32; TAPS]; PHASES],
    history: [[f32; TAPS]; CHANNELS],
    at: usize,
}

impl Default for TruePeak {
    fn default() -> Self {
        let mut coef = [[0.0; TAPS]; PHASES];
        let centre = (PHASES * TAPS) as f64 / 2.0 - 0.5;
        for (p, phase) in coef.iter_mut().enumerate() {
            for (k, c) in phase.iter_mut().enumerate() {
                let n = (k * PHASES + p) as f64;
                let x = (n - centre) / PHASES as f64;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let hann = 0.5
                    - 0.5 * (2.0 * std::f64::consts::PI * (n + 0.5) / (PHASES * TAPS) as f64).cos();
                *c = (sinc * hann) as f32;
            }
            let sum: f32 = phase.iter().sum();
            phase.iter_mut().for_each(|c| *c /= sum);
        }
        Self {
            coef,
            history: [[0.0; TAPS]; CHANNELS],
            at: 0,
        }
    }
}

impl TruePeak {
    /// The highest true peak in this stereo block, linear.
    pub fn process(&mut self, left: &[f32], right: &[f32]) -> f32 {
        let mut peak = 0.0f32;
        for (&l, &r) in left.iter().zip(right) {
            self.at = (self.at + 1) % TAPS;
            for (ch, s) in [l, r].into_iter().enumerate() {
                self.history[ch][self.at] = s;
                for phase in &self.coef {
                    let mut y = 0.0;
                    for (k, c) in phase.iter().enumerate() {
                        y += c * self.history[ch][(self.at + TAPS - k) % TAPS];
                    }
                    peak = peak.max(y.abs());
                }
            }
        }
        peak
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-scale sine at fs/4 sampled 45 degrees off its crests reads
    /// -3 dB sample peak; its true peak is 0 dB.
    #[test]
    fn finds_the_intersample_peak() {
        let x: Vec<f32> = (0..4_800)
            .map(|n| {
                (std::f64::consts::FRAC_PI_2 * n as f64 + std::f64::consts::FRAC_PI_4).sin() as f32
            })
            .collect();
        let sample = x.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let mut tp = TruePeak::default();
        let peak = tp.process(&x, &x);
        assert!((sample - 0.707).abs() < 0.01, "sample peak {sample}");
        assert!(peak > 0.95 && peak < 1.05, "true peak {peak}");
    }
}
