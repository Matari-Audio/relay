//! A bounded visual-analysis tap. The audio callback only enqueues samples;
//! the editor computes 40 logarithmic Goertzel bands at its existing 30 Hz cadence.
use std::sync::atomic::{AtomicBool, AtomicU32};
use truce::prelude::AudioTap;

pub struct SpectrumTap {
    pub audio: AudioTap<f32>,
    pub active: AtomicBool,
    pub rate: AtomicU32,
}
impl Default for SpectrumTap {
    fn default() -> Self {
        Self {
            audio: AudioTap::new(4096, 2),
            active: AtomicBool::new(false),
            rate: AtomicU32::new(48000),
        }
    }
}

const N: usize = 2048;
pub struct Spectrum {
    samples: [[f32; 2]; N],
    window: [f32; N],
    cursor: usize,
    filled: usize,
    pub bands: [f32; 40],
}
impl Default for Spectrum {
    fn default() -> Self {
        Self {
            samples: [[0.0; 2]; N],
            window: std::array::from_fn(|i| {
                0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / (N - 1) as f32).cos()
            }),
            cursor: 0,
            filled: 0,
            bands: [0.0; 40],
        }
    }
}
impl Spectrum {
    pub fn update(&mut self, tap: &SpectrumTap) {
        let mut fresh = false;
        tap.audio.drain_with(|chunk| {
            for pair in chunk.chunks_exact(2) {
                self.samples[self.cursor] = std::array::from_fn(|channel| {
                    if pair[channel].is_finite() {
                        pair[channel].clamp(-8.0, 8.0)
                    } else {
                        0.0
                    }
                });
                self.cursor = (self.cursor + 1) % N;
                self.filled = (self.filled + 1).min(N);
            }
            fresh = true;
        });
        if !fresh || self.filled < N {
            for band in &mut self.bands {
                *band *= 0.88;
            }
            return;
        }
        let rate = tap
            .rate
            .load(std::sync::atomic::Ordering::Relaxed)
            .max(8000) as f32;
        let upper = 18000.0_f32.min(rate * 0.45);
        for (index, band) in self.bands.iter_mut().enumerate() {
            let hz = 50.0 * (upper / 50.0).powf(index as f32 / 39.0);
            let coefficient = 2.0 * (std::f64::consts::TAU * f64::from(hz / rate)).cos();
            let (mut previous, mut before) = ([0.0_f64; 2], [0.0_f64; 2]);
            for i in 0..N {
                for channel in 0..2 {
                    let value =
                        f64::from(self.samples[(self.cursor + i) % N][channel] * self.window[i])
                            + coefficient * previous[channel]
                            - before[channel];
                    before[channel] = previous[channel];
                    previous[channel] = value;
                }
            }
            // Use the stronger channel so opposite-phase stereo never disappears.
            let power = (0..2)
                .map(|channel| {
                    previous[channel] * previous[channel] + before[channel] * before[channel]
                        - coefficient * previous[channel] * before[channel]
                })
                .fold(0.0_f64, f64::max);
            let amplitude = (power.sqrt() * 4.0 / N as f64).max(1e-6);
            let level = ((20.0 * amplitude.log10() + 72.0) / 72.0).clamp(0.0, 1.0) as f32;
            *band = level.max(*band * 0.88);
        }
    }
}
