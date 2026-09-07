//! Sixty virtual seconds through the public real-codec audio path.

use std::{f32::consts::TAU, time::Duration};

use relay_audio::{
    AdaptiveClockConfig, AudioPipelineConfig, AudioPipelineConfigInput, Bitrate, CaptureInput,
    ClockRecoveryConfig, EncoderPolicyV1, ExtendedSequence, ExtendedTimestamp, FrameDuration,
    FrameSource, InbandFec, IngressStatus, MAX_PACKET_BYTES, NetworkAction, NetworkTime,
    PacketBatch, PacketLossPercent, PayloadType, PlaybackConfig, PlaybackPublication, RenderState,
    RtpTimestamp, RxStreamConfig, RxWorker, ScheduleStatus, SequenceNumber, Ssrc, TxProcessOutcome,
    TxStreamConfig, TxWorker, playback_pair,
};

const CHANNELS: usize = 2;
const RATE_HZ: usize = 48_000;
const CAPTURE_CHUNK_FRAMES: usize = 480;
const CAPTURE_CHUNKS: usize = 6_000;
const INPUT_FRAMES: usize = RATE_HZ * 60;
const PACKET_FRAMES: usize = 960;
const PACKETS: usize = INPUT_FRAMES / PACKET_FRAMES;
const RENDERED_FRAMES: usize = 2_880_028;
const SSRC: u32 = 0x51a7_60c5;
const PAYLOAD_TYPE: u8 = 111;
const INITIAL_SEQUENCE: u64 = (7_u64 << 16) | 65_532;
const INITIAL_TIMESTAMP: u32 = u32::MAX - 700;
const LEFT_ENERGY: f64 = 6.967_62e4;
const RIGHT_ENERGY: f64 = 4.159_11e4;
const STEREO_DIFFERENCE: f64 = 4.661_49e5;

/// libopus picks its SIMD kernels at run time and different builds of the same
/// release therefore emit slightly different float samples: the CI-built 1.6.1
/// and the distribution 1.6.1 render the same 60 seconds at 65 dB SNR, peak
/// deviation 6.5e-3 (-44 dBFS). Freeze the rendered signal's energy within a
/// relative tolerance instead of hashing raw f32 bits; observed spread between
/// those builds is 3e-4, so 5e-3 is snug while staying build independent.
const SIGNAL_TOLERANCE: f64 = 5e-3;

#[track_caller]
fn assert_signal(actual: f64, golden: f64, what: &str) {
    let deviation = (actual - golden).abs() / golden.abs();
    assert!(
        deviation <= SIGNAL_TOLERANCE,
        "{what} is {actual:e}, golden {golden:e}, relative deviation {deviation:e}"
    );
}

fn pcm_chunk(start_frame: usize) -> Vec<f32> {
    let mut pcm = vec![0.0; CAPTURE_CHUNK_FRAMES * CHANNELS];
    for frame in 0..CAPTURE_CHUNK_FRAMES {
        let position = (start_frame + frame) as f32 / RATE_HZ as f32;
        pcm[frame * CHANNELS] = (TAU * 311.0 * position).sin() * 0.22;
        pcm[frame * CHANNELS + 1] = (TAU * 617.0 * position).sin() * 0.17;
    }
    pcm
}

#[test]
fn clean_real_public_path_runs_for_sixty_virtual_seconds() {
    let config = AudioPipelineConfig::new(AudioPipelineConfigInput {
        capture_rate_hz: RATE_HZ,
        playback_rate_hz: RATE_HZ,
        channels: CHANNELS,
        frame_duration: FrameDuration::Ms20,
        capture_src_chunk_frames: CAPTURE_CHUNK_FRAMES,
        capture_ring_samples: RATE_HZ * CHANNELS,
        playback_ring_samples: RATE_HZ * CHANNELS,
        tx_accumulator_samples: RATE_HZ * CHANNELS,
        reorder_capacity: 64,
        network_capacity: PACKETS + 16,
        network_due_batch_capacity: PACKETS + 16,
        packet_capacity: MAX_PACKET_BYTES,
        controller_cadence_frames: CAPTURE_CHUNK_FRAMES,
        clock_recovery: ClockRecoveryConfig::default(),
        adaptive_clock: AdaptiveClockConfig::default(),
    })
    .expect("60-second pipeline");
    let stream = TxStreamConfig {
        ssrc: Ssrc::new(SSRC),
        payload_type: PayloadType::new(PAYLOAD_TYPE).expect("payload type"),
        initial_sequence: SequenceNumber::new(INITIAL_SEQUENCE as u16),
        initial_timestamp: RtpTimestamp::new(INITIAL_TIMESTAMP),
        encoding_policy: EncoderPolicyV1::new(
            Bitrate::try_new(96_000).expect("bitrate"),
            InbandFec::Disabled,
            PacketLossPercent::ZERO,
        ),
    };
    let mut tx = TxWorker::new(config, stream).expect("real Opus TX worker");
    let mut batch = PacketBatch::new(PACKETS + 1).expect("bounded TX batch");
    let mut capture_frames_consumed = 0_usize;
    let mut media_frames_produced = 0_usize;
    let mut packets_emitted = 0_usize;
    let mut packets = Vec::with_capacity(PACKETS);

    for chunk in 0..CAPTURE_CHUNKS {
        let pcm = pcm_chunk(chunk * CAPTURE_CHUNK_FRAMES);
        let report = match tx.process_capture(CaptureInput::Chunk(&pcm), &mut batch) {
            TxProcessOutcome::Complete(report) | TxProcessOutcome::BatchFull(report) => report,
            other => panic!("unexpected TX outcome: {other:?}"),
        };
        assert!(
            !report.input_pending,
            "capture chunk {chunk} remained pending"
        );
        capture_frames_consumed += report.capture_frames_consumed;
        media_frames_produced += report.media_frames_produced;
        packets_emitted += report.packets_emitted;
        while let Some(packet) = batch.take_next() {
            packets.push(packet);
        }
    }

    assert_eq!(CAPTURE_CHUNKS * CAPTURE_CHUNK_FRAMES, INPUT_FRAMES);
    assert_eq!(capture_frames_consumed, INPUT_FRAMES);
    // This exercises exactly 60 seconds of live-stream input/media through the
    // fixed 48 kHz -> 48 kHz bypass; it does not exercise finite capture finish.
    assert_eq!(media_frames_produced, INPUT_FRAMES);
    assert_eq!(packets_emitted, PACKETS);
    assert_eq!(packets.len(), PACKETS);
    assert_eq!(PACKETS, 3_000);

    let packet_micros = 20_000_u64;
    let mut network = config
        .create_deterministic_network()
        .expect("bounded deterministic network");
    let mut due = config.create_due_batch().expect("bounded due batch");
    for (index, packet) in packets.into_iter().enumerate() {
        let outcome = network.schedule(
            packet,
            NetworkAction::Delay {
                delay: Duration::from_micros((index as u64 + 1) * packet_micros),
            },
        );
        assert!(
            matches!(outcome.status(), ScheduleStatus::Scheduled { .. }),
            "clean packet {index} was rejected: {:?}",
            outcome.status()
        );
    }

    let final_network_time = NetworkTime::from_micros(PACKETS as u64 * packet_micros);
    assert_eq!(final_network_time, NetworkTime::from_micros(60_000_000));

    let rx_stream = RxStreamConfig {
        ssrc: stream.ssrc,
        payload_type: stream.payload_type,
        initial_sequence: ExtendedSequence::new(INITIAL_SEQUENCE),
        initial_timestamp: stream.initial_timestamp,
    };
    let mut rx = RxWorker::new(config, rx_stream).expect("RX worker");
    let (mut playback, mut renderer, ring_metrics) =
        playback_pair(config, PlaybackConfig::for_pipeline(config)).expect("playback pair");
    let mut emitted = 0_usize;
    let mut rendered_frames = 0_usize;
    let mut maximum_ring_samples = 0_usize;
    let mut left_energy = 0.0_f64;
    let mut right_energy = 0.0_f64;
    let mut stereo_difference = 0.0_f64;
    let mut final_sequence = None;
    let mut final_timestamp = None;

    let mut consume = |outcome: relay_audio::FrameOutcome<'_>| {
        let offset = outcome
            .sequence()
            .get()
            .checked_sub(INITIAL_SEQUENCE)
            .expect("RX sequence remains in epoch");
        let media_delta = offset * PACKET_FRAMES as u64;
        let expected_timestamp = INITIAL_TIMESTAMP.wrapping_add(media_delta as u32);
        assert_eq!(outcome.timestamp(), RtpTimestamp::new(expected_timestamp));
        assert_eq!(outcome.source(), FrameSource::Packet);
        let report = playback
            .process_frame(
                outcome.frame(),
                ExtendedTimestamp::new(u64::from(INITIAL_TIMESTAMP) + media_delta),
                media_delta,
            )
            .expect("scheduled 48 kHz playback mapping");
        maximum_ring_samples = maximum_ring_samples.max(renderer.available_samples());
        assert_eq!(report.publication, PlaybackPublication::Published);
        assert_eq!(report.control_fault, None);
        let mut output = vec![f32::NAN; report.output_frames * CHANNELS];
        let render = renderer.render(&mut output);
        assert_eq!(render.state, RenderState::Complete);
        assert_eq!(render.rendered_samples, output.len());
        for frame in output.chunks_exact(CHANNELS) {
            assert!(frame[0].is_finite() && frame[1].is_finite());
            left_energy += f64::from(frame[0]).powi(2);
            right_energy += f64::from(frame[1]).powi(2);
            stereo_difference += f64::from(frame[0] - frame[1]).abs();
        }
        rendered_frames += report.output_frames;
        emitted += 1;
        final_sequence = Some(outcome.sequence().get());
        final_timestamp = Some(outcome.timestamp().get());
    };

    for slot in 0..PACKETS {
        network
            .advance_to(
                NetworkTime::from_micros((slot as u64 + 1) * packet_micros),
                &mut due,
            )
            .expect("monotonic virtual delivery");
        while let Some(packet) = due.take_next() {
            assert_eq!(
                rx.ingress(packet).status(),
                IngressStatus::AcceptedInOrder,
                "clean valid packet was not accepted in order"
            );
        }
        if let Some(outcome) = rx.tick() {
            consume(outcome);
        }
    }
    consume(rx.drain().expect("mandatory final RX drain"));

    let final_offset = PACKETS as u64 - 1;
    let final_media_delta = final_offset * PACKET_FRAMES as u64;
    let final_scheduled_local = final_media_delta;
    assert_eq!(final_media_delta, 2_879_040);
    assert_eq!(final_scheduled_local, 2_879_040);
    assert_eq!(emitted, PACKETS);
    assert_eq!(rx.metrics().emitted_frames, PACKETS as u64);
    // Freeze only the output produced by the live process_frame calls; no
    // adaptive-playback finish/drain or post-input settling tail is exercised.
    assert_eq!(rendered_frames, RENDERED_FRAMES);
    assert_eq!(final_sequence, Some(527_283));
    assert_eq!(final_timestamp, Some(2_878_339));
    assert_signal(left_energy, LEFT_ENERGY, "left energy");
    assert_signal(right_energy, RIGHT_ENERGY, "right energy");
    assert_signal(stereo_difference, STEREO_DIFFERENCE, "stereo difference");
    assert_eq!(
        renderer.available_samples(),
        0,
        "ring contains no unrendered live-call output"
    );

    let network_metrics = network.metrics();
    assert_eq!(network_metrics.submitted, PACKETS as u64);
    assert_eq!(network_metrics.scheduled_copies, PACKETS as u64);
    assert_eq!(network_metrics.delivered_copies, PACKETS as u64);
    assert_eq!(network_metrics.simulated_drops, 0);
    assert_eq!(network_metrics.duplicate_requests, 0);
    assert_eq!(network_metrics.duplicate_copies_scheduled, 0);
    assert_eq!(network_metrics.duplicate_capacity_rejections, 0);
    assert_eq!(network_metrics.capacity_rejections, 0);
    assert_eq!(network_metrics.time_overflow_rejections, 0);
    assert_eq!(network_metrics.ordinal_overflow_rejections, 0);

    let rx_metrics = rx.metrics();
    assert_eq!(rx_metrics.ingress_packets, PACKETS as u64);
    assert_eq!(rx_metrics.accepted_in_order, PACKETS as u64);
    assert_eq!(rx_metrics.accepted_reordered, 0);
    assert_eq!(rx_metrics.duplicates, 0);
    assert_eq!(rx_metrics.late, 0);
    assert_eq!(rx_metrics.ahead_of_window, 0);
    assert_eq!(rx_metrics.identity_mismatches, 0);
    assert_eq!(rx_metrics.duration_timestamp_mismatches, 0);
    assert_eq!(rx_metrics.malformed_packets, 0);
    assert_eq!(rx_metrics.oversized_packets, 0);
    assert_eq!(rx_metrics.extension_rejections, 0);
    assert_eq!(rx_metrics.deadline_decisions, PACKETS as u64);
    assert_eq!(rx_metrics.packet_frames, PACKETS as u64);
    assert_eq!(rx_metrics.codec_errors, 0);
    assert_eq!(rx_metrics.fec_attempts, 0);
    assert_eq!(rx_metrics.plc_frames, 0);

    let playback_metrics = playback.metrics();
    assert_eq!(playback_metrics.input_frames, INPUT_FRAMES as u64);
    assert_eq!(playback_metrics.output_frames, RENDERED_FRAMES as u64);
    assert_eq!(playback_metrics.published_chunks, PACKETS as u64);
    assert_eq!(playback_metrics.dropped_full_chunks, 0);
    assert_eq!(playback_metrics.disconnected_chunks, 0);
    assert_eq!(playback_metrics.clock_discontinuities, 0);
    assert_eq!(playback_metrics.controller_updates, PACKETS as u64 - 1);
    assert_eq!(playback_metrics.resets, 0);
    let ring = ring_metrics.snapshot();
    assert_eq!(ring.dropped_samples, 0);
    assert_eq!(ring.underruns, 0);
    assert_eq!(ring.underrun_samples, 0);
    assert!(
        maximum_ring_samples <= (PACKET_FRAMES + 64) * CHANNELS,
        "playback ring high-water was {maximum_ring_samples} scalar samples"
    );
}

const CAPTURE_RATE_HZ_5MS: usize = 44_100;
const PLAYBACK_RATE_HZ_5MS: usize = 192_000;
const CAPTURE_CHUNK_FRAMES_5MS: usize = 441;
const CAPTURE_CHUNKS_5MS: usize = 6_000;
const CAPTURE_INPUT_FRAMES_5MS: usize = CAPTURE_RATE_HZ_5MS * 60;
const MEDIA_FRAMES_5MS: usize = RATE_HZ * 60;
const PACKET_FRAMES_5MS: usize = 240;
const PACKETS_5MS: usize = MEDIA_FRAMES_5MS / PACKET_FRAMES_5MS;
const RENDERED_FRAMES_5MS: usize = 11_520_568;
const LEFT_ENERGY_5MS: f64 = 2.790_07e5;
const RIGHT_ENERGY_5MS: f64 = 1.666_62e5;
const STEREO_DIFFERENCE_5MS: f64 = 1.865_47e6;

fn pcm_chunk_5ms(start_frame: usize) -> Vec<f32> {
    let mut pcm = vec![0.0; CAPTURE_CHUNK_FRAMES_5MS * CHANNELS];
    for frame in 0..CAPTURE_CHUNK_FRAMES_5MS {
        let position = (start_frame + frame) as f32 / CAPTURE_RATE_HZ_5MS as f32;
        pcm[frame * CHANNELS] = (TAU * 311.0 * position).sin() * 0.22;
        pcm[frame * CHANNELS + 1] = (TAU * 617.0 * position).sin() * 0.17;
    }
    pcm
}

#[test]
fn clean_real_public_path_runs_five_ms_with_cross_rate_srcs() {
    let config = AudioPipelineConfig::new(AudioPipelineConfigInput {
        capture_rate_hz: CAPTURE_RATE_HZ_5MS,
        playback_rate_hz: PLAYBACK_RATE_HZ_5MS,
        channels: CHANNELS,
        frame_duration: FrameDuration::Ms5,
        capture_src_chunk_frames: CAPTURE_CHUNK_FRAMES_5MS,
        capture_ring_samples: CAPTURE_RATE_HZ_5MS * CHANNELS,
        playback_ring_samples: PLAYBACK_RATE_HZ_5MS * CHANNELS,
        tx_accumulator_samples: RATE_HZ * CHANNELS,
        reorder_capacity: 64,
        network_capacity: PACKETS_5MS + 16,
        network_due_batch_capacity: PACKETS_5MS + 16,
        packet_capacity: MAX_PACKET_BYTES,
        controller_cadence_frames: PLAYBACK_RATE_HZ_5MS / 100,
        clock_recovery: ClockRecoveryConfig::default(),
        adaptive_clock: AdaptiveClockConfig::default(),
    })
    .expect("60-second pipeline");
    let stream = TxStreamConfig {
        ssrc: Ssrc::new(SSRC),
        payload_type: PayloadType::new(PAYLOAD_TYPE).expect("payload type"),
        initial_sequence: SequenceNumber::new(INITIAL_SEQUENCE as u16),
        initial_timestamp: RtpTimestamp::new(INITIAL_TIMESTAMP),
        encoding_policy: EncoderPolicyV1::new(
            Bitrate::try_new(96_000).expect("bitrate"),
            InbandFec::Disabled,
            PacketLossPercent::ZERO,
        ),
    };
    let mut tx = TxWorker::new(config, stream).expect("real Opus TX worker");
    let mut batch = PacketBatch::new(PACKETS_5MS + 1).expect("bounded TX batch");
    let mut capture_frames_consumed = 0_usize;
    let mut media_frames_produced = 0_usize;
    let mut packets_emitted = 0_usize;
    let mut packets = Vec::with_capacity(PACKETS_5MS);

    for chunk in 0..CAPTURE_CHUNKS_5MS {
        let pcm = pcm_chunk_5ms(chunk * CAPTURE_CHUNK_FRAMES_5MS);
        let report = match tx.process_capture(CaptureInput::Chunk(&pcm), &mut batch) {
            TxProcessOutcome::Complete(report) | TxProcessOutcome::BatchFull(report) => report,
            other => panic!("unexpected TX outcome: {other:?}"),
        };
        assert!(
            !report.input_pending,
            "capture chunk {chunk} remained pending"
        );
        capture_frames_consumed += report.capture_frames_consumed;
        media_frames_produced += report.media_frames_produced;
        packets_emitted += report.packets_emitted;
        while let Some(packet) = batch.take_next() {
            packets.push(packet);
        }
    }

    assert_eq!(
        CAPTURE_CHUNKS_5MS * CAPTURE_CHUNK_FRAMES_5MS,
        CAPTURE_INPUT_FRAMES_5MS
    );
    assert_eq!(capture_frames_consumed, CAPTURE_INPUT_FRAMES_5MS);
    // Live calls consume exactly 60 seconds at 44.1 kHz and produce exactly
    // 60 seconds at 48 kHz; finite capture finish/trim is not exercised.
    assert_eq!(media_frames_produced, MEDIA_FRAMES_5MS);
    assert_eq!(packets_emitted, PACKETS_5MS);
    assert_eq!(packets.len(), PACKETS_5MS);
    assert_eq!(PACKETS_5MS, 12_000);

    let packet_micros = 5_000_u64;
    let mut network = config
        .create_deterministic_network()
        .expect("bounded deterministic network");
    let mut due = config.create_due_batch().expect("bounded due batch");
    for (index, packet) in packets.into_iter().enumerate() {
        let outcome = network.schedule(
            packet,
            NetworkAction::Delay {
                delay: Duration::from_micros((index as u64 + 1) * packet_micros),
            },
        );
        assert!(
            matches!(outcome.status(), ScheduleStatus::Scheduled { .. }),
            "clean packet {index} was rejected: {:?}",
            outcome.status()
        );
    }

    let final_network_time = NetworkTime::from_micros(PACKETS_5MS as u64 * packet_micros);
    assert_eq!(final_network_time, NetworkTime::from_micros(60_000_000));

    let rx_stream = RxStreamConfig {
        ssrc: stream.ssrc,
        payload_type: stream.payload_type,
        initial_sequence: ExtendedSequence::new(INITIAL_SEQUENCE),
        initial_timestamp: stream.initial_timestamp,
    };
    let mut rx = RxWorker::new(config, rx_stream).expect("RX worker");
    let (mut playback, mut renderer, ring_metrics) =
        playback_pair(config, PlaybackConfig::for_pipeline(config)).expect("playback pair");
    let mut emitted = 0_usize;
    let mut rendered_frames = 0_usize;
    let mut maximum_ring_samples = 0_usize;
    let mut left_energy = 0.0_f64;
    let mut right_energy = 0.0_f64;
    let mut stereo_difference = 0.0_f64;
    let mut final_sequence = None;
    let mut final_timestamp = None;

    let mut consume = |outcome: relay_audio::FrameOutcome<'_>| {
        let offset = outcome
            .sequence()
            .get()
            .checked_sub(INITIAL_SEQUENCE)
            .expect("RX sequence remains in epoch");
        let media_delta = offset * PACKET_FRAMES_5MS as u64;
        let expected_timestamp = INITIAL_TIMESTAMP.wrapping_add(media_delta as u32);
        assert_eq!(outcome.timestamp(), RtpTimestamp::new(expected_timestamp));
        assert_eq!(outcome.source(), FrameSource::Packet);
        let scheduled_local_frames = media_delta
            .checked_mul(PLAYBACK_RATE_HZ_5MS as u64)
            .and_then(|scaled| scaled.checked_add(RATE_HZ as u64 / 2))
            .expect("scheduled local frame rounding")
            / RATE_HZ as u64;
        assert_eq!(scheduled_local_frames, media_delta * 4);
        let report = playback
            .process_frame(
                outcome.frame(),
                ExtendedTimestamp::new(u64::from(INITIAL_TIMESTAMP) + media_delta),
                scheduled_local_frames,
            )
            .expect("scheduled 192 kHz playback mapping");
        maximum_ring_samples = maximum_ring_samples.max(renderer.available_samples());
        assert_eq!(report.publication, PlaybackPublication::Published);
        assert_eq!(report.control_fault, None);
        let mut output = vec![f32::NAN; report.output_frames * CHANNELS];
        let render = renderer.render(&mut output);
        assert_eq!(render.state, RenderState::Complete);
        assert_eq!(render.rendered_samples, output.len());
        for frame in output.chunks_exact(CHANNELS) {
            assert!(frame[0].is_finite() && frame[1].is_finite());
            left_energy += f64::from(frame[0]).powi(2);
            right_energy += f64::from(frame[1]).powi(2);
            stereo_difference += f64::from(frame[0] - frame[1]).abs();
        }
        rendered_frames += report.output_frames;
        emitted += 1;
        final_sequence = Some(outcome.sequence().get());
        final_timestamp = Some(outcome.timestamp().get());
    };

    for slot in 0..PACKETS_5MS {
        network
            .advance_to(
                NetworkTime::from_micros((slot as u64 + 1) * packet_micros),
                &mut due,
            )
            .expect("monotonic virtual delivery");
        while let Some(packet) = due.take_next() {
            assert_eq!(
                rx.ingress(packet).status(),
                IngressStatus::AcceptedInOrder,
                "clean valid packet was not accepted in order"
            );
        }
        if let Some(outcome) = rx.tick() {
            consume(outcome);
        }
    }
    consume(rx.drain().expect("mandatory final RX drain"));

    let final_offset = PACKETS_5MS as u64 - 1;
    let final_media_delta = final_offset * PACKET_FRAMES_5MS as u64;
    let final_scheduled_local = final_media_delta
        .checked_mul(PLAYBACK_RATE_HZ_5MS as u64)
        .and_then(|scaled| scaled.checked_add(RATE_HZ as u64 / 2))
        .expect("final scheduled local frame rounding")
        / RATE_HZ as u64;
    assert_eq!(final_media_delta, 2_879_760);
    assert_eq!(final_scheduled_local, 11_519_040);
    assert_eq!(emitted, PACKETS_5MS);
    assert_eq!(rx.metrics().emitted_frames, PACKETS_5MS as u64);
    // Freeze only the output produced by the live process_frame calls; no
    // adaptive-playback finish/drain or post-input settling tail is exercised.
    assert_eq!(rendered_frames, RENDERED_FRAMES_5MS);
    assert_eq!(final_sequence, Some(536_283));
    assert_eq!(final_timestamp, Some(2_879_059));
    assert_signal(left_energy, LEFT_ENERGY_5MS, "left energy");
    assert_signal(right_energy, RIGHT_ENERGY_5MS, "right energy");
    assert_signal(
        stereo_difference,
        STEREO_DIFFERENCE_5MS,
        "stereo difference",
    );
    assert_eq!(
        renderer.available_samples(),
        0,
        "ring contains no unrendered live-call output"
    );

    let network_metrics = network.metrics();
    assert_eq!(network_metrics.submitted, PACKETS_5MS as u64);
    assert_eq!(network_metrics.scheduled_copies, PACKETS_5MS as u64);
    assert_eq!(network_metrics.delivered_copies, PACKETS_5MS as u64);
    assert_eq!(network_metrics.simulated_drops, 0);
    assert_eq!(network_metrics.duplicate_requests, 0);
    assert_eq!(network_metrics.duplicate_copies_scheduled, 0);
    assert_eq!(network_metrics.duplicate_capacity_rejections, 0);
    assert_eq!(network_metrics.capacity_rejections, 0);
    assert_eq!(network_metrics.time_overflow_rejections, 0);
    assert_eq!(network_metrics.ordinal_overflow_rejections, 0);

    let rx_metrics = rx.metrics();
    assert_eq!(rx_metrics.ingress_packets, PACKETS_5MS as u64);
    assert_eq!(rx_metrics.accepted_in_order, PACKETS_5MS as u64);
    assert_eq!(rx_metrics.accepted_reordered, 0);
    assert_eq!(rx_metrics.duplicates, 0);
    assert_eq!(rx_metrics.late, 0);
    assert_eq!(rx_metrics.ahead_of_window, 0);
    assert_eq!(rx_metrics.identity_mismatches, 0);
    assert_eq!(rx_metrics.duration_timestamp_mismatches, 0);
    assert_eq!(rx_metrics.malformed_packets, 0);
    assert_eq!(rx_metrics.oversized_packets, 0);
    assert_eq!(rx_metrics.extension_rejections, 0);
    assert_eq!(rx_metrics.deadline_decisions, PACKETS_5MS as u64);
    assert_eq!(rx_metrics.packet_frames, PACKETS_5MS as u64);
    assert_eq!(rx_metrics.codec_errors, 0);
    assert_eq!(rx_metrics.fec_attempts, 0);
    assert_eq!(rx_metrics.plc_frames, 0);

    let playback_metrics = playback.metrics();
    assert_eq!(playback_metrics.input_frames, MEDIA_FRAMES_5MS as u64);
    assert_eq!(playback_metrics.output_frames, RENDERED_FRAMES_5MS as u64);
    assert_eq!(playback_metrics.published_chunks, PACKETS_5MS as u64);
    assert_eq!(playback_metrics.dropped_full_chunks, 0);
    assert_eq!(playback_metrics.disconnected_chunks, 0);
    assert_eq!(playback_metrics.clock_discontinuities, 0);
    assert_eq!(
        playback_metrics.controller_updates,
        PACKETS_5MS as u64 / 2 - 1
    );
    assert_eq!(playback_metrics.resets, 0);
    let ring = ring_metrics.snapshot();
    assert_eq!(ring.dropped_samples, 0);
    assert_eq!(ring.underruns, 0);
    assert_eq!(ring.underrun_samples, 0);
    assert!(
        maximum_ring_samples <= (PACKET_FRAMES_5MS * 4 + 64) * CHANNELS,
        "playback ring high-water was {maximum_ring_samples} samples"
    );
}

const CAPTURE_RATE_HZ_10MS: usize = 96_000;
const PLAYBACK_RATE_HZ_10MS: usize = 44_100;
const CAPTURE_CHUNK_FRAMES_10MS: usize = 960;
const CAPTURE_CHUNKS_10MS: usize = 6_000;
const CAPTURE_INPUT_FRAMES_10MS: usize = CAPTURE_RATE_HZ_10MS * 60;
const MEDIA_FRAMES_10MS: usize = RATE_HZ * 60;
const PACKET_FRAMES_10MS: usize = 480;
const PACKETS_10MS: usize = MEDIA_FRAMES_10MS / PACKET_FRAMES_10MS;
const RENDERED_FRAMES_10MS: usize = 2_646_023;
const LEFT_ENERGY_10MS: f64 = 6.411_06e4;
const RIGHT_ENERGY_10MS: f64 = 3.832_27e4;
const STEREO_DIFFERENCE_10MS: f64 = 4.286_66e5;

fn pcm_chunk_10ms(start_frame: usize) -> Vec<f32> {
    let mut pcm = vec![0.0; CAPTURE_CHUNK_FRAMES_10MS * CHANNELS];
    for frame in 0..CAPTURE_CHUNK_FRAMES_10MS {
        let position = (start_frame + frame) as f32 / CAPTURE_RATE_HZ_10MS as f32;
        pcm[frame * CHANNELS] = (TAU * 311.0 * position).sin() * 0.22;
        pcm[frame * CHANNELS + 1] = (TAU * 617.0 * position).sin() * 0.17;
    }
    pcm
}

#[test]
fn media_60s_10ms_96k_capture_48k_media_44k1_playback() {
    let config = AudioPipelineConfig::new(AudioPipelineConfigInput {
        capture_rate_hz: CAPTURE_RATE_HZ_10MS,
        playback_rate_hz: PLAYBACK_RATE_HZ_10MS,
        channels: CHANNELS,
        frame_duration: FrameDuration::Ms10,
        capture_src_chunk_frames: CAPTURE_CHUNK_FRAMES_10MS,
        capture_ring_samples: CAPTURE_RATE_HZ_10MS * CHANNELS,
        playback_ring_samples: PLAYBACK_RATE_HZ_10MS * CHANNELS,
        tx_accumulator_samples: RATE_HZ * CHANNELS,
        reorder_capacity: 64,
        network_capacity: PACKETS_10MS + 16,
        network_due_batch_capacity: PACKETS_10MS + 16,
        packet_capacity: MAX_PACKET_BYTES,
        controller_cadence_frames: PLAYBACK_RATE_HZ_10MS / 100,
        clock_recovery: ClockRecoveryConfig::default(),
        adaptive_clock: AdaptiveClockConfig::default(),
    })
    .expect("60-second pipeline");
    let stream = TxStreamConfig {
        ssrc: Ssrc::new(SSRC),
        payload_type: PayloadType::new(PAYLOAD_TYPE).expect("payload type"),
        initial_sequence: SequenceNumber::new(INITIAL_SEQUENCE as u16),
        initial_timestamp: RtpTimestamp::new(INITIAL_TIMESTAMP),
        encoding_policy: EncoderPolicyV1::new(
            Bitrate::try_new(96_000).expect("bitrate"),
            InbandFec::Disabled,
            PacketLossPercent::ZERO,
        ),
    };
    let mut tx = TxWorker::new(config, stream).expect("real Opus TX worker");
    let mut batch = PacketBatch::new(PACKETS_10MS + 1).expect("bounded TX batch");
    let mut capture_frames_consumed = 0_usize;
    let mut media_frames_produced = 0_usize;
    let mut packets_emitted = 0_usize;
    let mut packets = Vec::with_capacity(PACKETS_10MS);

    for chunk in 0..CAPTURE_CHUNKS_10MS {
        let pcm = pcm_chunk_10ms(chunk * CAPTURE_CHUNK_FRAMES_10MS);
        let report = match tx.process_capture(CaptureInput::Chunk(&pcm), &mut batch) {
            TxProcessOutcome::Complete(report) | TxProcessOutcome::BatchFull(report) => report,
            other => panic!("unexpected TX outcome: {other:?}"),
        };
        assert!(
            !report.input_pending,
            "capture chunk {chunk} remained pending"
        );
        capture_frames_consumed += report.capture_frames_consumed;
        media_frames_produced += report.media_frames_produced;
        packets_emitted += report.packets_emitted;
        while let Some(packet) = batch.take_next() {
            packets.push(packet);
        }
    }

    assert_eq!(
        CAPTURE_CHUNKS_10MS * CAPTURE_CHUNK_FRAMES_10MS,
        CAPTURE_INPUT_FRAMES_10MS
    );
    assert_eq!(capture_frames_consumed, CAPTURE_INPUT_FRAMES_10MS);
    // Live calls consume exactly 60 seconds at 96 kHz and produce exactly
    // 60 seconds at 48 kHz; finite capture finish/trim is not exercised.
    assert_eq!(media_frames_produced, MEDIA_FRAMES_10MS);
    assert_eq!(packets_emitted, PACKETS_10MS);
    assert_eq!(packets.len(), PACKETS_10MS);
    assert_eq!(PACKETS_10MS, 6_000);

    let packet_micros = 10_000_u64;
    let mut network = config
        .create_deterministic_network()
        .expect("bounded deterministic network");
    let mut due = config.create_due_batch().expect("bounded due batch");
    for (index, packet) in packets.into_iter().enumerate() {
        let outcome = network.schedule(
            packet,
            NetworkAction::Delay {
                delay: Duration::from_micros((index as u64 + 1) * packet_micros),
            },
        );
        assert!(
            matches!(outcome.status(), ScheduleStatus::Scheduled { .. }),
            "clean packet {index} was rejected: {:?}",
            outcome.status()
        );
    }

    let final_network_time = NetworkTime::from_micros(PACKETS_10MS as u64 * packet_micros);
    assert_eq!(final_network_time, NetworkTime::from_micros(60_000_000));

    let rx_stream = RxStreamConfig {
        ssrc: stream.ssrc,
        payload_type: stream.payload_type,
        initial_sequence: ExtendedSequence::new(INITIAL_SEQUENCE),
        initial_timestamp: stream.initial_timestamp,
    };
    let mut rx = RxWorker::new(config, rx_stream).expect("RX worker");
    let (mut playback, mut renderer, ring_metrics) =
        playback_pair(config, PlaybackConfig::for_pipeline(config)).expect("playback pair");
    let mut emitted = 0_usize;
    let mut rendered_frames = 0_usize;
    let mut maximum_ring_samples = 0_usize;
    let mut left_energy = 0.0_f64;
    let mut right_energy = 0.0_f64;
    let mut stereo_difference = 0.0_f64;
    let mut final_sequence = None;
    let mut final_timestamp = None;

    let mut consume = |outcome: relay_audio::FrameOutcome<'_>| {
        let offset = outcome
            .sequence()
            .get()
            .checked_sub(INITIAL_SEQUENCE)
            .expect("RX sequence remains in epoch");
        let media_delta = offset * PACKET_FRAMES_10MS as u64;
        let expected_timestamp = INITIAL_TIMESTAMP.wrapping_add(media_delta as u32);
        assert_eq!(outcome.timestamp(), RtpTimestamp::new(expected_timestamp));
        assert_eq!(outcome.source(), FrameSource::Packet);
        let scheduled_local_frames = media_delta
            .checked_mul(PLAYBACK_RATE_HZ_10MS as u64)
            .and_then(|scaled| scaled.checked_add(RATE_HZ as u64 / 2))
            .expect("scheduled local frame rounding")
            / RATE_HZ as u64;
        assert_eq!(scheduled_local_frames, offset * 441);
        let report = playback
            .process_frame(
                outcome.frame(),
                ExtendedTimestamp::new(u64::from(INITIAL_TIMESTAMP) + media_delta),
                scheduled_local_frames,
            )
            .expect("scheduled 44.1 kHz playback mapping");
        maximum_ring_samples = maximum_ring_samples.max(renderer.available_samples());
        assert_eq!(report.publication, PlaybackPublication::Published);
        assert_eq!(report.control_fault, None);
        let mut output = vec![f32::NAN; report.output_frames * CHANNELS];
        let render = renderer.render(&mut output);
        assert_eq!(render.state, RenderState::Complete);
        assert_eq!(render.rendered_samples, output.len());
        for frame in output.chunks_exact(CHANNELS) {
            assert!(frame[0].is_finite() && frame[1].is_finite());
            left_energy += f64::from(frame[0]).powi(2);
            right_energy += f64::from(frame[1]).powi(2);
            stereo_difference += f64::from(frame[0] - frame[1]).abs();
        }
        rendered_frames += report.output_frames;
        emitted += 1;
        final_sequence = Some(outcome.sequence().get());
        final_timestamp = Some(outcome.timestamp().get());
    };

    for slot in 0..PACKETS_10MS {
        network
            .advance_to(
                NetworkTime::from_micros((slot as u64 + 1) * packet_micros),
                &mut due,
            )
            .expect("monotonic virtual delivery");
        while let Some(packet) = due.take_next() {
            assert_eq!(
                rx.ingress(packet).status(),
                IngressStatus::AcceptedInOrder,
                "clean valid packet was not accepted in order"
            );
        }
        if let Some(outcome) = rx.tick() {
            consume(outcome);
        }
    }
    consume(rx.drain().expect("mandatory final RX drain"));

    let final_offset = PACKETS_10MS as u64 - 1;
    let final_media_delta = final_offset * PACKET_FRAMES_10MS as u64;
    let final_scheduled_local = final_media_delta
        .checked_mul(PLAYBACK_RATE_HZ_10MS as u64)
        .and_then(|scaled| scaled.checked_add(RATE_HZ as u64 / 2))
        .expect("final scheduled local frame rounding")
        / RATE_HZ as u64;
    assert_eq!(final_media_delta, 2_879_520);
    assert_eq!(final_scheduled_local, 2_645_559);
    assert_eq!(emitted, PACKETS_10MS);
    assert_eq!(rx.metrics().emitted_frames, PACKETS_10MS as u64);
    // Freeze only the output produced by the live process_frame calls; no
    // adaptive-playback finish/drain or post-input settling tail is exercised.
    assert_eq!(rendered_frames, RENDERED_FRAMES_10MS);
    assert_eq!(final_sequence, Some(530_283));
    assert_eq!(final_timestamp, Some(2_878_819));
    assert_signal(left_energy, LEFT_ENERGY_10MS, "left energy");
    assert_signal(right_energy, RIGHT_ENERGY_10MS, "right energy");
    assert_signal(
        stereo_difference,
        STEREO_DIFFERENCE_10MS,
        "stereo difference",
    );
    assert_eq!(
        renderer.available_samples(),
        0,
        "ring contains no unrendered live-call output"
    );

    let network_metrics = network.metrics();
    assert_eq!(network_metrics.submitted, PACKETS_10MS as u64);
    assert_eq!(network_metrics.scheduled_copies, PACKETS_10MS as u64);
    assert_eq!(network_metrics.delivered_copies, PACKETS_10MS as u64);
    assert_eq!(network_metrics.simulated_drops, 0);
    assert_eq!(network_metrics.duplicate_requests, 0);
    assert_eq!(network_metrics.duplicate_copies_scheduled, 0);
    assert_eq!(network_metrics.duplicate_capacity_rejections, 0);
    assert_eq!(network_metrics.capacity_rejections, 0);
    assert_eq!(network_metrics.time_overflow_rejections, 0);
    assert_eq!(network_metrics.ordinal_overflow_rejections, 0);

    let rx_metrics = rx.metrics();
    assert_eq!(rx_metrics.ingress_packets, PACKETS_10MS as u64);
    assert_eq!(rx_metrics.accepted_in_order, PACKETS_10MS as u64);
    assert_eq!(rx_metrics.accepted_reordered, 0);
    assert_eq!(rx_metrics.duplicates, 0);
    assert_eq!(rx_metrics.late, 0);
    assert_eq!(rx_metrics.ahead_of_window, 0);
    assert_eq!(rx_metrics.identity_mismatches, 0);
    assert_eq!(rx_metrics.duration_timestamp_mismatches, 0);
    assert_eq!(rx_metrics.malformed_packets, 0);
    assert_eq!(rx_metrics.oversized_packets, 0);
    assert_eq!(rx_metrics.extension_rejections, 0);
    assert_eq!(rx_metrics.deadline_decisions, PACKETS_10MS as u64);
    assert_eq!(rx_metrics.packet_frames, PACKETS_10MS as u64);
    assert_eq!(rx_metrics.codec_errors, 0);
    assert_eq!(rx_metrics.fec_attempts, 0);
    assert_eq!(rx_metrics.plc_frames, 0);

    let playback_metrics = playback.metrics();
    assert_eq!(playback_metrics.input_frames, MEDIA_FRAMES_10MS as u64);
    assert_eq!(playback_metrics.output_frames, RENDERED_FRAMES_10MS as u64);
    assert_eq!(playback_metrics.published_chunks, PACKETS_10MS as u64);
    assert_eq!(playback_metrics.dropped_full_chunks, 0);
    assert_eq!(playback_metrics.disconnected_chunks, 0);
    assert_eq!(playback_metrics.clock_discontinuities, 0);
    assert_eq!(playback_metrics.controller_updates, PACKETS_10MS as u64 - 1);
    assert_eq!(playback_metrics.resets, 0);
    let ring = ring_metrics.snapshot();
    assert_eq!(ring.dropped_samples, 0);
    assert_eq!(ring.underruns, 0);
    assert_eq!(ring.underrun_samples, 0);
    assert!(
        maximum_ring_samples
            <= (PACKET_FRAMES_10MS * PLAYBACK_RATE_HZ_10MS / RATE_HZ + 64) * CHANNELS,
        "playback ring high-water was {maximum_ring_samples} samples"
    );
}
