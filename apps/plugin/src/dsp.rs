//! Audio-thread helpers. Everything here is allocation-free and branch-light.

use relay_audio::FrameDuration;
use truce::prelude::AudioBuffer;

/// Interleave up to two input channels into `out` (mono is duplicated).
pub fn copy_inputs(buffer: &AudioBuffer, frames: usize, out: &mut [f32]) {
    let inputs = buffer.num_input_channels();
    let left: &[f32] = if inputs >= 1 { buffer.input(0) } else { &[] };
    let right: &[f32] = if inputs >= 2 { buffer.input(1) } else { left };
    for (frame, pair) in out.chunks_exact_mut(2).take(frames).enumerate() {
        let l = left.get(frame).copied().unwrap_or(0.0);
        let r = right.get(frame).copied().unwrap_or(l);
        pair[0] = l;
        pair[1] = r;
    }
}

/// De-interleave `src` into the first two output channels.
pub fn write_outputs(buffer: &mut AudioBuffer, frames: usize, src: &[f32]) {
    for channel in 0..buffer.num_output_channels().min(2) {
        let out = buffer.output(channel);
        for (frame, sample) in out.iter_mut().take(frames).enumerate() {
            *sample = src[frame * 2 + channel];
        }
    }
}

pub fn apply_gain(samples: &mut [f32], gain: f32) {
    for sample in samples {
        *sample *= gain;
    }
}

pub fn mix_into(dst: &mut [f32], src: &[f32]) {
    for (sample, add) in dst.iter_mut().zip(src) {
        *sample += *add;
    }
}

/// Raised-cosine ramp, `0 → 1` over `t ∈ [0, 1]`.
fn raised_cosine(t: f32) -> f32 {
    0.5 - 0.5 * (core::f32::consts::PI * t).cos()
}

/// Remote-only monitoring rendered fewer samples than the block: fill the
/// rest with dry so an underrun sounds like the local track, not a hole.
/// `rendered` is the count of valid interleaved samples at the head of `out`.
pub fn splice_dry(out: &mut [f32], dry: &[f32], rendered: usize) {
    let len = out.len().min(dry.len());
    let rendered = rendered.min(len) & !1;
    if rendered >= len {
        return;
    }
    if rendered == 0 {
        let fade_frames = (len / 2).min(64);
        for (i, pair) in out.chunks_exact_mut(2).take(fade_frames).enumerate() {
            let gain = raised_cosine((i + 1) as f32 / fade_frames as f32);
            pair[0] = dry[i * 2] * gain;
            pair[1] = dry[i * 2 + 1] * gain;
        }
        out[fade_frames * 2..len].copy_from_slice(&dry[fade_frames * 2..len]);
        return;
    }
    let fade_frames = (rendered / 2).min((len - rendered) / 2).clamp(1, 32);
    let fade_start = rendered - fade_frames * 2;
    for i in 0..fade_frames {
        let gain = raised_cosine((i + 1) as f32 / fade_frames as f32);
        let o = fade_start + i * 2;
        out[o] = out[o] * (1.0 - gain) + dry[o] * gain;
        out[o + 1] = out[o + 1] * (1.0 - gain) + dry[o + 1] * gain;
    }
    out[rendered..len].copy_from_slice(&dry[rendered..len]);
}

/// Copy inputs straight to outputs when the block exceeds our staging.
pub fn host_passthrough(buffer: &mut AudioBuffer, frames: usize) {
    let inputs = buffer.num_input_channels();
    let outputs = buffer.num_output_channels();
    if inputs == 0 {
        for channel in 0..outputs {
            let out = buffer.output(channel);
            let n = frames.min(out.len());
            out[..n].fill(0.0);
        }
        return;
    }
    for channel in 0..outputs {
        let source = channel.min(inputs - 1);
        if source == channel && buffer.is_in_place(channel) {
            continue;
        }
        let (input, output) = buffer.io_pair(source, channel);
        let n = frames.min(input.len()).min(output.len());
        output[..n].copy_from_slice(&input[..n]);
    }
}

/// Per-channel absolute peaks of an interleaved stereo slice.
pub fn stereo_peaks(interleaved: &[f32]) -> (f32, f32) {
    interleaved
        .chunks_exact(2)
        .fold((0.0_f32, 0.0_f32), |(l, r), pair| {
            (l.max(pair[0].abs()), r.max(pair[1].abs()))
        })
}

/// Pick the wire frame closest to the host's block period.
pub fn frame_from_host(rate_hz: usize, max_block: usize) -> FrameDuration {
    let ms = max_block.saturating_mul(1_000) / rate_hz.max(1);
    match ms {
        0..=7 => FrameDuration::Ms5,
        8..=15 => FrameDuration::Ms10,
        _ => FrameDuration::Ms20,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splice_dry_fills_a_silent_suffix() {
        let mut out = vec![0.8; 192];
        out[128..].fill(0.0);
        let dry = vec![0.1; 192];
        splice_dry(&mut out, &dry, 128);
        assert!((out[0] - 0.8).abs() < 1e-5);
        assert!((out[190] - 0.1).abs() < 1e-5);
        assert!((out[191] - 0.1).abs() < 1e-5);
    }

    #[test]
    fn splice_dry_empty_remote_becomes_dry() {
        let mut out = [0.0; 8];
        let dry = [0.2; 8];
        splice_dry(&mut out, &dry, 0);
        assert!(out.iter().any(|s| *s > 0.05));
        assert!((out[6] - 0.2).abs() < 1e-5);
    }

    #[test]
    fn splice_dry_full_render_is_untouched() {
        let mut out = [0.7; 8];
        splice_dry(&mut out, &[0.2; 8], 8);
        assert!(out.iter().all(|s| (*s - 0.7).abs() < 1e-6));
    }

    #[test]
    fn splice_dry_rounds_odd_render_down_to_a_frame() {
        let mut out = [0.7; 8];
        splice_dry(&mut out, &[0.2; 8], 3);
        assert!((out[7] - 0.2).abs() < 1e-6);
    }

    #[test]
    fn stereo_peaks_read_both_channels() {
        let (l, r) = stereo_peaks(&[0.1, 0.8, -0.2, 0.4]);
        assert!((l - 0.2).abs() < 1e-6);
        assert!((r - 0.8).abs() < 1e-6);
    }

    #[test]
    fn frame_from_host_follows_block_period() {
        assert_eq!(frame_from_host(48_000, 64), FrameDuration::Ms5);
        assert_eq!(frame_from_host(48_000, 512), FrameDuration::Ms10);
        assert_eq!(frame_from_host(48_000, 1024), FrameDuration::Ms20);
        assert_eq!(frame_from_host(44_100, 128), FrameDuration::Ms5);
        assert_eq!(frame_from_host(0, 128), FrameDuration::Ms20);
    }
}
