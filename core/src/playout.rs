//! Audio-thread playout: hold `buffer` host blocks of audio, re-prime after
//! an underrun, and keep the fill centred with rubato's `Slip` -- it drops
//! or repeats one frame behind a crossfade, so clock drift between two
//! machines costs no latency and no filtering.

use rtrb::Consumer;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable, FixedAsync, Resampler, Slip};

use crate::CHANNELS;

/// Frames `Slip` makes per call.
const CHUNK: usize = 64;
/// Largest host block we plan scratch for.
const MAX_BLOCK: usize = 8_192;
/// Proportional gain from fill error (frames) to ratio.
const GAIN: f64 = 2e-6;

pub struct Playout {
    rx: Consumer<f32>,
    slip: Slip<f32>,
    input: Vec<f32>,
    /// Resampled frames not yet played, interleaved.
    fifo: Vec<f32>,
    fifo_len: usize,
    priming: bool,
    error: f64,
    pub underruns: u64,
}

impl Playout {
    pub fn new(rx: Consumer<f32>) -> Self {
        let slip = Slip::new(CHUNK, CHANNELS, FixedAsync::Output).expect("64 frames is a valid Slip chunk");
        let input = vec![0.0; slip.input_frames_max() * CHANNELS];
        Self {
            rx,
            slip,
            input,
            fifo: vec![0.0; (MAX_BLOCK + CHUNK) * CHANNELS],
            fifo_len: 0,
            priming: true,
            error: 0.0,
            underruns: 0,
        }
    }

    /// Frames buffered, in the ring and in the fifo.
    pub fn fill(&self) -> usize {
        (self.rx.slots() + self.fifo_len) / CHANNELS
    }

    /// Add `left.len()` frames of remote audio into `left`/`right`. `target`
    /// is the fill to hold in frames. Returns whether anything played.
    /// Allocation- and lock-free.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32], target: usize) -> bool {
        let n = left.len().min(right.len()).min(MAX_BLOCK);
        let target = target.max(CHUNK);
        // A stall or a burst left far too much queued: jump back to live.
        let excess = self.fill().saturating_sub(target * 2 + CHUNK);
        if excess > 0 {
            let drop = (excess * CHANNELS).min(self.rx.slots());
            if let Ok(chunk) = self.rx.read_chunk(drop) {
                chunk.commit_all();
            }
        }
        if self.priming {
            if self.fill() < target {
                return false;
            }
            self.priming = false;
            self.error = 0.0;
        }
        while self.fifo_len < n * CHANNELS {
            let need = self.slip.input_frames_next() * CHANNELS;
            let Ok(chunk) = self.rx.read_chunk(need) else {
                self.priming = true;
                self.underruns += 1;
                break;
            };
            let (a, b) = chunk.as_slices();
            self.input[..a.len()].copy_from_slice(a);
            self.input[a.len()..need].copy_from_slice(b);
            chunk.commit_all();
            let out = &mut self.fifo[self.fifo_len..self.fifo_len + CHUNK * CHANNELS];
            let (Ok(input), Ok(mut output)) = (
                InterleavedSlice::new(&self.input[..need], CHANNELS, need / CHANNELS),
                InterleavedSlice::new_mut(out, CHANNELS, CHUNK),
            ) else {
                break;
            };
            match self.slip.process_into_buffer(&input, &mut output, None) {
                Ok((_, written)) => self.fifo_len += written * CHANNELS,
                Err(_) => break,
            }
        }
        let frames = n.min(self.fifo_len / CHANNELS);
        for (i, frame) in self.fifo[..frames * CHANNELS].chunks_exact(CHANNELS).enumerate() {
            left[i] += frame[0];
            right[i] += frame[1];
        }
        self.fifo.copy_within(frames * CHANNELS..self.fifo_len, 0);
        self.fifo_len -= frames * CHANNELS;

        // Fill above target: take more input per output frame (ratio < 1).
        self.error += 0.05 * (self.fill() as f64 - target as f64 - self.error);
        let _ = self.slip.set_resample_ratio(1.0 - GAIN * self.error, false);
        frames > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sender runs 200 ppm fast against our 256-frame callback. Without
    /// correction the buffer would grow by ~0.05 frames a block and
    /// eventually overflow; with Slip it stays near target and never underruns
    /// once primed.
    #[test]
    fn holds_fill_under_drift() {
        let (mut tx, rx) = rtrb::RingBuffer::new(crate::RING);
        let mut playout = Playout::new(rx);
        let (block, target) = (256, 512);
        let (mut l, mut r) = (vec![0.0; block], vec![0.0; block]);
        let mut owed = 0.0;
        let mut phase = 0u64;
        for i in 0..20_000 {
            owed += block as f64 * 1.0002;
            while owed >= 1.0 {
                let s = (phase as f32 * 0.01).sin();
                phase += 1;
                owed -= 1.0;
                let _ = tx.push(s);
                let _ = tx.push(s);
            }
            l.fill(0.0);
            r.fill(0.0);
            playout.render(&mut l, &mut r, target);
            if i > 1_000 {
                let fill = playout.fill();
                assert!(fill.abs_diff(target) < 300, "block {i}: fill {fill}");
            }
        }
        assert!(playout.underruns <= 1, "underruns {}", playout.underruns);
    }
}
