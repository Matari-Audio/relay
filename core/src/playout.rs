//! Audio-thread playout. Jitter can only be absorbed where the audio is
//! played -- the receiver's clock decides when each sample is due -- so
//! this is where the buffer lives. It sizes itself: an underrun grows the
//! target by a block, and a quiet stretch whose lowest fill shows
//! slack shrinks it. Rubato's `Slip` keeps the fill on target by dropping
//! or repeating one frame behind a crossfade, so clock drift between two
//! machines costs no latency and no filtering. Silence is a free resync:
//! after a quiet moment the target drops to what the network needs and the
//! backlog is cut, inaudibly, so latency never creeps up over a session.

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
/// Frames played between shrink decisions: about two seconds.
const WINDOW: usize = 96_000;
/// Largest target: a third of the ring, so a trim never outruns it.
const MAX_TARGET: usize = crate::RING / CHANNELS / 3;
/// Frames of silence before a resync: about 200 ms.
const QUIET: usize = 9_600;
/// Below this a sample counts as silent: -80 dBFS.
const SILENT: f32 = 1e-4;

pub struct Playout {
    rx: Consumer<f32>,
    slip: Slip<f32>,
    input: Vec<f32>,
    /// Resampled frames not yet played, interleaved.
    fifo: Vec<f32>,
    fifo_len: usize,
    priming: bool,
    error: f64,
    /// Fill to hold, in frames. 0 until the first block sets it.
    target: usize,
    /// Lowest fill seen at the start of a block this window.
    low: usize,
    played: usize,
    /// Windows since the last underrun.
    calm: u32,
    /// Silent frames played since the last sound or resync.
    quiet: usize,
    pub underruns: u64,
}

impl Playout {
    pub fn new(rx: Consumer<f32>) -> Self {
        let slip = Slip::new(CHUNK, CHANNELS, FixedAsync::Output)
            .expect("64 frames is a valid Slip chunk");
        let input = vec![0.0; slip.input_frames_max() * CHANNELS];
        Self {
            rx,
            slip,
            input,
            fifo: vec![0.0; (MAX_BLOCK + CHUNK) * CHANNELS],
            fifo_len: 0,
            priming: true,
            error: 0.0,
            target: 0,
            low: usize::MAX,
            played: 0,
            calm: 0,
            quiet: 0,
            underruns: 0,
        }
    }

    /// Frames buffered, in the ring and in the fifo.
    pub fn fill(&self) -> usize {
        (self.rx.slots() + self.fifo_len) / CHANNELS
    }

    /// The fill it holds, in frames: the latency the buffer adds.
    pub fn target(&self) -> usize {
        self.target
    }

    /// Add `left.len()` frames of remote audio into `left`/`right`.
    /// Returns whether anything played. Allocation- and lock-free.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) -> bool {
        let n = left.len().min(right.len()).min(MAX_BLOCK);
        let floor = n.max(CHUNK);
        if self.target < floor {
            self.target = 2 * floor;
        }
        let target = self.target;
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
        self.low = self.low.min(self.fill());
        while self.fifo_len < n * CHANNELS {
            let need = self.slip.input_frames_next() * CHANNELS;
            let Ok(chunk) = self.rx.read_chunk(need) else {
                self.priming = true;
                self.underruns += 1;
                self.target = (target + floor).min(MAX_TARGET);
                self.low = usize::MAX;
                self.played = 0;
                self.calm = 0;
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
        let played = &self.fifo[..frames * CHANNELS];
        if played.iter().all(|x| x.abs() < SILENT) {
            self.quiet += frames;
        } else {
            self.quiet = 0;
        }
        for (i, frame) in played.as_chunks::<CHANNELS>().0.iter().enumerate() {
            left[i] += frame[0];
            right[i] += frame[1];
        }
        self.fifo.copy_within(frames * CHANNELS..self.fifo_len, 0);
        self.fifo_len -= frames * CHANNELS;

        // Fill above target: take more input per output frame (ratio < 1).
        self.error += 0.05 * (self.fill() as f64 - target as f64 - self.error);
        let _ = self.slip.set_resample_ratio(1.0 - GAIN * self.error, false);

        if self.quiet >= QUIET {
            self.resync(n, floor);
        }

        // A block needs `n` frames plus what Slip reads ahead. Anything the
        // fill never dipped into over a window is latency we can give back,
        // an eighth at most per window so Slip drains it smoothly, and only
        // after ten calm seconds so a steady jitter does not sawtooth.
        self.played += frames;
        if self.played >= WINDOW {
            self.calm += 1;
            let spare = self.low.saturating_sub(n + floor + 2 * CHUNK);
            if self.calm >= 5 {
                self.target = (target - (spare / 2).min(target / 8)).max(floor);
            }
            self.low = usize::MAX;
            self.played = 0;
        }
        frames > 0
    }

    /// Give back all the slack this window found, then cut the queue to
    /// the new target, but only through samples that are silent too, so
    /// the next note starts whole.
    fn resync(&mut self, n: usize, floor: usize) {
        self.quiet = 0;
        if self.low != usize::MAX {
            let spare = self.low.saturating_sub(n + floor + 2 * CHUNK);
            self.target = self.target.saturating_sub(spare).max(floor);
        }
        let excess = (self.fill().saturating_sub(self.target) * CHANNELS).min(self.rx.slots());
        if let Ok(chunk) = self.rx.read_chunk(excess) {
            let (a, b) = chunk.as_slices();
            let silent = a.iter().chain(b).take_while(|x| x.abs() < SILENT).count();
            chunk.commit(silent / CHANNELS * CHANNELS);
        }
        self.low = usize::MAX;
        self.played = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `blocks` 256-frame callbacks. The sender makes 480-frame packets
    /// with its clock `ppm` fast, each arriving up to `jitter` frames late.
    fn run(
        playout: &mut Playout,
        tx: &mut rtrb::Producer<f32>,
        blocks: usize,
        ppm: f64,
        jitter: u64,
        seed: &mut u64,
        level: f32,
    ) {
        let block = 256;
        let (mut l, mut r) = (vec![0.0; block], vec![0.0; block]);
        let mut queue: std::collections::VecDeque<(f64, usize)> = Default::default();
        let (mut sent, mut clock) = (0.0f64, 0.0f64);
        for _ in 0..blocks {
            clock += block as f64;
            while sent < clock * (1.0 + ppm * 1e-6) {
                *seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let late = if jitter == 0 {
                    0
                } else {
                    (*seed >> 33) % jitter
                };
                queue.push_back((sent + late as f64, 480));
                sent += 480.0;
            }
            // In order, like the link writes them.
            while queue.front().is_some_and(|(t, _)| *t <= clock) {
                let (_, frames) = queue.pop_front().unwrap();
                for _ in 0..frames * CHANNELS {
                    let _ = tx.push(level);
                }
            }
            l.fill(0.0);
            r.fill(0.0);
            playout.render(&mut l, &mut r);
        }
    }

    /// Drift alone: the fill holds and nothing underruns once primed.
    #[test]
    fn holds_fill_under_drift() {
        let (mut tx, rx) = rtrb::RingBuffer::new(crate::RING);
        let mut playout = Playout::new(rx);
        run(&mut playout, &mut tx, 20_000, 200.0, 0, &mut 1, 0.5);
        assert!(playout.underruns <= 2, "underruns {}", playout.underruns);
        let (fill, target) = (playout.fill(), playout.target());
        assert!(fill.abs_diff(target) < 300, "fill {fill} target {target}");
    }

    /// After jitter, calm sound keeps the grown target until the calm
    /// windows pass; calm silence gives it back within a second.
    #[test]
    fn silence_resyncs() {
        for (level, shrinks) in [(0.5, false), (0.0, true)] {
            let (mut tx, rx) = rtrb::RingBuffer::new(crate::RING);
            let mut playout = Playout::new(rx);
            run(&mut playout, &mut tx, 4_000, 50.0, 960, &mut 7, 0.5);
            let rough = playout.target();
            run(&mut playout, &mut tx, 200, 50.0, 0, &mut 7, level);
            let calm = playout.target();
            assert_eq!(
                calm < rough * 3 / 4,
                shrinks,
                "level {level}: {rough} -> {calm}"
            );
        }
    }

    /// 20 ms of jitter: the target grows until underruns stop, then shrinks
    /// back once the network calms down.
    #[test]
    fn adapts_to_jitter() {
        let (mut tx, rx) = rtrb::RingBuffer::new(crate::RING);
        let mut playout = Playout::new(rx);
        let seed = &mut 7;
        run(&mut playout, &mut tx, 4_000, 50.0, 960, seed, 0.5);
        let settled = playout.underruns;
        run(&mut playout, &mut tx, 8_000, 50.0, 960, seed, 0.5);
        let rough = playout.target();
        assert!(
            playout.underruns - settled <= 1,
            "still underrunning: {} -> {}",
            settled,
            playout.underruns
        );
        assert!((512..6_000).contains(&rough), "rough target {rough}");
        run(&mut playout, &mut tx, 20_000, 50.0, 0, seed, 0.5);
        let calm = playout.target();
        assert!(calm < rough * 3 / 4, "calm target {calm}, rough {rough}");
        assert!(calm < 1_300, "calm target {calm}");
    }
}
