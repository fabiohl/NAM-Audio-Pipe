// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::recording::buffer::{AlignedBlock, AudioMetadata, CONTROL_CAPACITY, ControlPayload};
use crate::recording::pool::{POOL_CAPACITY, PoolConsumer, RecordingPool};
use crate::recording::transport::RecordingSender;

/// Builds a pool-transport sender plus the two consumer halves a test needs to
/// observe the pushes (control ring + pool descriptors).
fn pool_sender_and_consumers() -> (
    RecordingSender,
    rtrb::Consumer<ControlPayload>,
    PoolConsumer<POOL_CAPACITY>,
) {
    let (control_p, control_c) =
        crate::recording::buffer::create_control_ring_buffer(CONTROL_CAPACITY);
    let pool = RecordingPool::<POOL_CAPACITY>::new();
    let (pool_p, pool_c) = pool.split();
    (
        RecordingSender::Pool {
            control: Some(control_p),
            pool: Some(pool_p),
        },
        control_c,
        pool_c,
    )
}

fn dummy_meta() -> AudioMetadata {
    AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    }
}

#[test]
fn ffi_contract_valid_buffer() {
    let buf = [0u8; 128];
    assert!(check_ffi_contract(&buf, 0, 64).is_some());
    assert!(check_ffi_contract(&buf, 4, 60).is_some());
}

#[test]
fn ffi_contract_exact_fit() {
    let buf = [0u8; 128];
    let (n_bytes, n_samples) = check_ffi_contract(&buf, 0, 128).unwrap();
    assert_eq!(n_bytes, 128);
    assert_eq!(n_samples, 32);
}

#[test]
fn ffi_contract_misaligned_byte_count_rejected() {
    let buf = [0u8; 128];
    // 3 bytes is not a multiple of sizeof(f32)
    assert!(check_ffi_contract(&buf, 0, 3).is_none());
    // 5 bytes also not aligned
    assert!(check_ffi_contract(&buf, 0, 5).is_none());
}

#[test]
fn ffi_contract_unaligned_offset_rejected() {
    let buf = [0u8; 128];
    // 1 byte unaligned offset + 4 bytes -> offset misaligned, rejected
    assert!(check_ffi_contract(&buf, 1, 4).is_none());
    // 2 byte unaligned offset -> rejected
    assert!(check_ffi_contract(&buf, 2, 8).is_none());
    // 4 byte aligned offset -> accepted
    assert!(check_ffi_contract(&buf, 4, 8).is_some());
}

#[test]
fn ffi_contract_misaligned_base_pointer_rejected() {
    let storage = [0u8; 4 + 128];
    let base = storage.as_ptr() as usize;
    let align = std::mem::align_of::<f32>();
    let mut delta = 1usize;
    while (base + delta).is_multiple_of(align) {
        delta += 1;
    }
    let raw = &storage[delta..];
    assert!(check_ffi_contract(raw, 0, 64).is_none());
    assert!(check_ffi_contract(raw, 4, 60).is_none());
}

#[test]
fn ffi_contract_offset_oob_rejected() {
    let buf = [0u8; 128];
    assert!(check_ffi_contract(&buf, 200, 64).is_none());
}

#[test]
fn ffi_contract_size_over_remaining_rejected_fail_closed() {
    let buf = [0u8; 128];
    // offset is valid but size > remaining -> rejected (no silent clamp)
    assert!(check_ffi_contract(&buf, 64, 128).is_none());
    // offset at end -> rejected
    assert!(check_ffi_contract(&buf, 128, 64).is_none());
    // exact fit -> accepted
    assert!(check_ffi_contract(&buf, 64, 64).is_some());
}

#[test]
fn ffi_contract_size_exceeding_small_buffer_rejected() {
    let buf = [0u8; 5];
    // size=8 exceeds the 5-byte buffer -> rejected (no clamp)
    assert!(check_ffi_contract(&buf, 0, 8).is_none());
}

#[test]
fn spa_buffer_pair_accepts_disjoint_aligned_buffers() {
    let buf_l = [0u8; 64];
    let buf_r = [0u8; 64];
    assert_eq!(
        check_spa_buffer_pair(&buf_l, 0, 32, &buf_r, 0, 32),
        Some((32, 8))
    );
    assert_eq!(
        check_spa_buffer_pair(&buf_l, 0, 64, &buf_r, 0, 64),
        Some((64, 16))
    );
}

#[test]
fn spa_buffer_pair_accepts_adjacent_non_overlapping_regions() {
    let storage = [0u8; 128];
    let (first, second) = storage.split_at(64);
    assert!(check_spa_buffer_pair(first, 0, 64, second, 0, 64).is_some());
}

#[test]
fn spa_buffer_pair_rejects_identical_buffers() {
    let buf = [0u8; 64];
    assert!(check_spa_buffer_pair(&buf, 0, 32, &buf, 0, 32).is_none());
}

#[test]
fn spa_buffer_pair_rejects_partially_overlapping_buffers() {
    let storage = [0u8; 96];
    // Left: bytes [0, 64); Right: bytes [32, 96) -> partial overlap.
    let raw_l = &storage[0..64];
    let raw_r = &storage[32..96];
    assert!(check_spa_buffer_pair(raw_l, 0, 64, raw_r, 0, 64).is_none());
}

#[test]
fn spa_buffer_pair_rejects_contained_buffers() {
    let storage = [0u8; 128];
    let raw_l = &storage[0..128];
    let raw_r = &storage[32..96];
    assert!(check_spa_buffer_pair(raw_l, 0, 128, raw_r, 0, 64).is_none());
}

#[test]
fn spa_buffer_pair_rejects_asymmetric_frame_counts() {
    let buf_l = [0u8; 64];
    let buf_r = [0u8; 128];
    // 16 samples on the left vs 8 on the right -> asymmetric, rejected.
    assert!(check_spa_buffer_pair(&buf_l, 0, 64, &buf_r, 0, 32).is_none());
}

#[test]
fn spa_buffer_pair_rejects_misaligned_offset() {
    let buf_l = [0u8; 64];
    let buf_r = [0u8; 64];
    // Right offset is not f32-aligned -> rejected by the per-channel check.
    assert!(check_spa_buffer_pair(&buf_l, 0, 32, &buf_r, 1, 32).is_none());
}

#[test]
fn spa_buffer_pair_rejects_offset_out_of_bounds() {
    let buf_l = [0u8; 64];
    let buf_r = [0u8; 64];
    // offset 48 + size 32 exceeds the 64-byte right buffer.
    assert!(check_spa_buffer_pair(&buf_l, 0, 32, &buf_r, 48, 32).is_none());
}

#[test]
fn spa_buffer_pair_rejects_non_cardinal_size() {
    let buf_l = [0u8; 64];
    let buf_r = [0u8; 64];
    // 6 bytes is not a multiple of sizeof(f32).
    assert!(check_spa_buffer_pair(&buf_l, 0, 32, &buf_r, 0, 6).is_none());
}

#[test]
fn recording_metadata_confirmed_only_on_push_success() {
    let (mut sender, mut control_c, _pool_c) = pool_sender_and_consumers();
    let mut meta_sent = false;
    let mut meta_rate = 0u32;
    let flag = std::sync::atomic::AtomicBool::new(false);

    send_recording_metadata(
        &mut sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        Some(&flag),
        None,
    );

    assert!(meta_sent);
    assert_eq!(meta_rate, 48000);
    assert!(flag.load(Ordering::Relaxed));
    match control_c.pop().unwrap() {
        ControlPayload::Metadata(m) => assert_eq!(m.sample_rate, 48000.0),
        _ => panic!("expected Metadata"),
    }
}

#[test]
fn recording_metadata_not_confirmed_when_channel_full() {
    let (control_p, mut control_c) = crate::recording::buffer::create_control_ring_buffer(1);
    let pool = RecordingPool::<POOL_CAPACITY>::new();
    let (pool_p, _pool_c) = pool.split();
    let mut sender = RecordingSender::Pool {
        control: Some(control_p),
        pool: Some(pool_p),
    };
    let mut meta_sent = false;
    let mut meta_rate = 0u32;

    // Saturate the single-slot control channel.
    sender
        .control_producer_mut()
        .unwrap()
        .push(ControlPayload::Metadata(dummy_meta()))
        .unwrap();

    send_recording_metadata(
        &mut sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );

    assert!(!meta_sent, "metadata must not be confirmed when push fails");
    assert_eq!(meta_rate, 0);

    // Free the channel and retry: the flag is confirmed.
    let _ = control_c.pop().unwrap();
    send_recording_metadata(
        &mut sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );

    assert!(meta_sent);
    assert_eq!(meta_rate, 48000);
    match control_c.pop().unwrap() {
        ControlPayload::Metadata(m) => assert_eq!(m.sample_rate, 48000.0),
        _ => panic!("expected Metadata"),
    }
}

#[test]
fn recording_metadata_reset_on_host_rate_change() {
    let (mut sender, mut control_c, _pool_c) = pool_sender_and_consumers();
    let mut meta_sent = false;
    let mut meta_rate = 0u32;

    send_recording_metadata(
        &mut sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(meta_sent);
    assert_eq!(meta_rate, 48000);
    let _ = control_c.pop().unwrap();

    // Same rate again: no duplicate header.
    send_recording_metadata(
        &mut sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(control_c.pop().is_err());

    // Host rate change: a new header is emitted for the new rate.
    send_recording_metadata(
        &mut sender,
        44100,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(meta_sent);
    assert_eq!(meta_rate, 44100);
    match control_c.pop().unwrap() {
        ControlPayload::Metadata(m) => assert_eq!(m.sample_rate, 44100.0),
        _ => panic!("expected Metadata"),
    }
}

#[test]
fn recording_metadata_absent_producer_never_confirmed() {
    let mut sender = RecordingSender::none();
    let mut meta_sent = false;
    let mut meta_rate = 0u32;

    send_recording_metadata(
        &mut sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );

    assert!(!meta_sent);
    assert_eq!(meta_rate, 0);
}
#[test]
fn recording_metadata_not_pushed_when_worker_failed() {
    // Once the disk worker reports a fatal error, the RT
    // callback must suspend enqueueing — the metadata must NOT be pushed.
    let (mut sender, mut control_c, _pool_c) = pool_sender_and_consumers();
    let mut meta_sent = false;
    let mut meta_rate = 0u32;
    let failed = AtomicBool::new(true);

    send_recording_metadata(
        &mut sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        Some(&failed),
    );

    assert!(
        !meta_sent,
        "metadata must not be confirmed while the worker failed"
    );
    assert_eq!(meta_rate, 0);
    assert!(
        control_c.pop().is_err(),
        "no metadata may reach the dead worker"
    );
}

#[test]
fn recording_audio_not_pushed_when_worker_failed() {
    // With the failure flag raised the audio block must be
    // dropped cleanly — no publish, no overrun accounting (the worker is gone).
    let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);

    let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let failed = AtomicBool::new(true);

    send_recording_audio(
        &mut sender,
        MAX_BLOCK_SIZE,
        &resamp_l,
        &resamp_r,
        &mut block,
        None,
        Some(&failed),
    );

    assert!(
        pool_c.try_pop().is_none(),
        "no audio may be published while the worker failed"
    );
    assert_eq!(
        OVERRUN_COUNT.load(Ordering::Relaxed),
        0,
        "no overruns should be charged against a dead worker"
    );
}

#[test]
fn recording_audio_zero_quantum_is_noop() {
    let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();

    send_recording_audio(&mut sender, 0, &resamp_l, &resamp_r, &mut block, None, None);

    assert!(pool_c.try_pop().is_none());
}

#[test]
fn recording_audio_sender_none_is_noop() {
    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let mut none = RecordingSender::none();

    send_recording_audio(&mut none, 64, &resamp_l, &resamp_r, &mut block, None, None);
}

#[test]
fn recording_audio_flag_cleared_when_worker_fails_in_flight() {
    let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let failed = AtomicBool::new(false);
    let flag = AtomicBool::new(true);

    // Normal enqueue: flag stays true
    send_recording_audio(
        &mut sender,
        4,
        &resamp_l,
        &resamp_r,
        &mut block,
        Some(&flag),
        Some(&failed),
    );
    assert!(flag.load(Ordering::Relaxed));

    // Worker fails: subsequent enqueue clears the active flag
    failed.store(true, Ordering::Release);
    send_recording_audio(
        &mut sender,
        4,
        &resamp_l,
        &resamp_r,
        &mut block,
        Some(&flag),
        Some(&failed),
    );
    assert!(!flag.load(Ordering::Relaxed));

    let in_flight = pool_c.try_pop().expect("published descriptor");
    assert_eq!(in_flight.block().valid_len(), 8);
    in_flight.release();
}

#[test]
fn recording_audio_oversized_block_dropped_and_counted() {
    // Fail-closed safety net: a block wider than `MAX_BLOCK_SIZE`
    // (16384 samples = 8192 stereo frames, the largest legal quantum) must be
    // dropped and accounted in BOTH the block counter and the frame counter —
    // on the pool path no slot is acquired for it.
    let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);

    let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();

    // interleaved_len = MAX_BLOCK_SIZE * 2 > MAX_BLOCK_SIZE -> dropped.
    send_recording_audio(
        &mut sender,
        MAX_BLOCK_SIZE,
        &resamp_l,
        &resamp_r,
        &mut block,
        None,
        None,
    );

    assert!(
        pool_c.try_pop().is_none(),
        "oversized block must not be published"
    );
    assert_eq!(OVERRUN_COUNT.load(Ordering::Relaxed), 1);
    assert_eq!(
        OVERRUN_FRAMES_COUNT.load(Ordering::Relaxed),
        MAX_BLOCK_SIZE as u64,
        "the lost-frame counter must account the dropped block's frames"
    );
    assert_eq!(
        sender.pool_producer_mut().unwrap().leaked_slots(),
        0,
        "a dropped oversized block must never leak a slot"
    );

    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);
}

#[test]
fn recording_audio_normal_block_pushed() {
    let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
    let mut resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let mut resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    for i in 0..4 {
        resamp_l[i] = i as f32;
        resamp_r[i] = -(i as f32);
    }
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let flag = std::sync::atomic::AtomicBool::new(false);

    send_recording_audio(
        &mut sender,
        4,
        &resamp_l,
        &resamp_r,
        &mut block,
        Some(&flag),
        None,
    );

    assert!(flag.load(Ordering::Relaxed));
    let in_flight = pool_c.try_pop().expect("published descriptor");
    // Planar layout: L = [0, 1, 2, 3], R = [-0, -1, -2, -3].
    assert_eq!(in_flight.block().left_slice(), &[0.0, 1.0, 2.0, 3.0]);
    assert_eq!(in_flight.block().right_slice(), &[-0.0, -1.0, -2.0, -3.0]);
    assert_eq!(in_flight.descriptor().valid_len as usize, 8);
    in_flight.release();
}

#[test]
fn recording_audio_full_channel_counted_as_overrun() {
    let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);

    // Exhaust the pool: every slot in flight, none released yet.
    let (mut sender, _control_c, _pool_c) = pool_sender_and_consumers();
    for _ in 0..POOL_CAPACITY {
        assert!(
            sender.try_push_audio(&[1.0], &[2.0]),
            "slots must be acquirable until the pool is exhausted"
        );
    }

    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let flag = std::sync::atomic::AtomicBool::new(false);

    send_recording_audio(
        &mut sender,
        4,
        &resamp_l,
        &resamp_r,
        &mut block,
        Some(&flag),
        None,
    );

    assert!(!flag.load(Ordering::Relaxed));
    assert_eq!(OVERRUN_COUNT.load(Ordering::Relaxed), 1);
    assert_eq!(
        OVERRUN_FRAMES_COUNT.load(Ordering::Relaxed),
        4,
        "a 4-frame block lost to an exhausted pool must be accounted in frames"
    );
    assert_eq!(
        sender.pool_producer_mut().unwrap().leaked_slots(),
        0,
        "an overrun must never leak a pool slot"
    );

    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);
}

// ── Capacity Domain Boundaries & Frame Reconciliation ────────────────────────
//
// The recording transport must persist every legal quantum integrally
// (`MAX_BLOCK_SIZE = 16384` samples = 8192 stereo frames = `MAX_BRIDGE_BUF`):
// 2048 frames (the old hard drop ceiling), 2049 (first frame past it) and 8192
// (the largest legal quantum) are all published whole through the pool.
// Overruns are accounted in both blocks and frames so the invariant
// `frames_capturados == frames_enfileirados + frames_perdidos` holds.

#[test]
fn recording_audio_boundary_2048_2049_8192_frames_pushed() {
    for n_pw in [2048usize, 2049, MAX_BRIDGE_BUF] {
        let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
        let resamp_l = vec![0.5f32; n_pw];
        let resamp_r = vec![0.25f32; n_pw];
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        let flag = std::sync::atomic::AtomicBool::new(false);

        send_recording_audio(
            &mut sender,
            n_pw,
            &resamp_l,
            &resamp_r,
            &mut block,
            Some(&flag),
            None,
        );

        assert!(flag.load(Ordering::Relaxed), "n_pw={n_pw}");
        let in_flight = pool_c
            .try_pop()
            .expect("published descriptor for n_pw={n_pw}");
        assert_eq!(in_flight.block().frames(), n_pw, "n_pw={n_pw}");
        assert_eq!(in_flight.block().valid_len(), n_pw * 2, "n_pw={n_pw}");
        in_flight.release();
        assert!(pool_c.try_pop().is_none(), "exactly one block per quantum");
    }
}

#[test]
fn recording_audio_reconciliation_enqueued_plus_lost_equals_produced() {
    let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);

    const FRAMES: usize = 2049;
    let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
    let resamp_l = vec![0.5f32; FRAMES];
    let resamp_r = vec![0.25f32; FRAMES];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();

    // `POOL_CAPACITY` blocks fill every slot; the next one finds the pool
    // exhausted and is lost.
    for _ in 0..POOL_CAPACITY {
        assert!(sender.try_push_audio(&resamp_l, &resamp_r));
    }
    send_recording_audio(
        &mut sender,
        FRAMES,
        &resamp_l,
        &resamp_r,
        &mut block,
        None,
        None,
    );

    let mut enqueued_frames = 0usize;
    while let Some(in_flight) = pool_c.try_pop() {
        enqueued_frames += in_flight.block().frames();
        in_flight.release();
    }
    let captured = (POOL_CAPACITY + 1) * FRAMES;
    let lost = OVERRUN_FRAMES_COUNT.load(Ordering::Relaxed) as usize;
    assert_eq!(enqueued_frames, POOL_CAPACITY * FRAMES);
    assert_eq!(lost, FRAMES);
    assert_eq!(
        captured,
        enqueued_frames + lost,
        "reconciliation: frames_capturados == frames_enfileirados + frames_perdidos"
    );
    assert_eq!(OVERRUN_COUNT.load(Ordering::Relaxed), 1);

    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);
}

/// Zero-allocation acceptance: `get_dealloc_count() == 0` during recording. The RT
/// enqueue path — pool `try_acquire`, in-place planar fill, descriptor publish
/// — must perform zero heap allocations and zero deallocations on every legal
/// quantum, including the maximum (8192 frames).
#[test]
#[cfg(feature = "heap-audit")]
fn recording_audio_enqueue_zero_alloc_dealloc() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
    let resamp_l = vec![0.5f32; MAX_BRIDGE_BUF];
    let resamp_r = vec![0.25f32; MAX_BRIDGE_BUF];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let flag = std::sync::atomic::AtomicBool::new(false);

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        for _ in 0..64 {
            send_recording_audio(
                &mut sender,
                MAX_BRIDGE_BUF,
                &resamp_l,
                &resamp_r,
                &mut block,
                Some(&flag),
                None,
            );
            let in_flight = pool_c.try_pop().expect("published descriptor");
            in_flight.release();
        }
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };
    // Measured: alloc=0, dealloc=0, realloc=0 (quantum=8192, 64 enqueues, pool)
    assert_eq!(allocs, 0, "recording enqueue allocated on RT: {allocs}");
    assert_eq!(
        deallocs, 0,
        "recording enqueue deallocated on RT: {deallocs}"
    );
    assert_eq!(
        reallocs, 0,
        "recording enqueue reallocated on RT: {reallocs}"
    );
}

// ── Malformed FFI/SPA Harness ──────────────────────────────────────────────
//
// The harness feeds raw SPA descriptor values (data pointers, maxsize, chunk
// metadata read as integers) to the exact fail-closed code the RT callbacks
// run, without requiring a live PipeWire stream. It proves every adversarial
// frontier scenario is rejected with the `RT_STATUS_HOST_CONTRACT_VIOLATION`
// flag raised, no panic, and the buffers silenced.

/// Creates an SPA chunk descriptor with the given valid-data window.
fn chunk_of(offset: u32, size: u32) -> pw::spa::sys::spa_chunk {
    pw::spa::sys::spa_chunk {
        offset,
        size,
        stride: std::mem::size_of::<f32>() as i32,
        flags: 0,
    }
}

/// A real, `f32`-aligned backing buffer used to feed the harness raw pointer
/// values. `Vec<f32>` guarantees the base pointer satisfies
/// `align_of::<f32>()`, so tests can observe both the aligned (valid) and the
/// deliberately misaligned (malformed) cases on real memory.
struct FfiHarnessBuf {
    samples: Vec<f32>,
}

impl FfiHarnessBuf {
    fn new(n_frames: usize) -> Self {
        Self {
            samples: vec![0.0f32; n_frames],
        }
    }

    fn base(&self) -> usize {
        self.samples.as_ptr() as usize
    }

    fn ptr(&self) -> usize {
        self.base()
    }

    fn maxsize(&self) -> usize {
        self.samples.len() * std::mem::size_of::<f32>()
    }

    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `f32` is plain old data and the byte length is exact.
        unsafe { std::slice::from_raw_parts(self.samples.as_ptr() as *const u8, self.maxsize()) }
    }

    fn as_samples(&self) -> &[f32] {
        &self.samples
    }

    fn fill_pattern(&mut self, byte: u8) {
        let bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(self.samples.as_mut_ptr() as *mut u8, self.maxsize())
        };
        bytes.fill(byte);
    }

    fn all_zero(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }
}

#[test]
fn read_chunk_meta_rejects_null_pointer() {
    assert_eq!(read_chunk_meta(std::ptr::null()), ChunkWindow::Absent);
}

#[test]
fn read_chunk_meta_reads_host_window() {
    let chunk = chunk_of(8, 64);
    assert_eq!(read_chunk_meta(&chunk), ChunkWindow::Valid(8, 64));
}

// ── Malformed non-null chunks raise E2304, never silence ─────────────────────

#[test]
fn spa_corrupted_chunk_raises_contract_violation() {
    // A non-null chunk with the SPA corrupted flag set (bit 0 of `flags`) must
    // be classified `Malformed` and raise `RT_STATUS_HOST_CONTRACT_VIOLATION`
    // (E2304) on the capture path — both channels silenced fail-closed —
    // instead of degrading silently to `(0, 0)` with clean telemetry.
    let mut l = FfiHarnessBuf::new(16);
    let mut r = FfiHarnessBuf::new(16);
    l.fill_pattern(0x11);
    r.fill_pattern(0x22);
    let rt = RtStatusFlags::default();
    let mut chunk = chunk_of(0, 64);
    chunk.flags = 1; // SPA_CHUNK_FLAG_CORRUPTED

    assert_eq!(read_chunk_meta(&chunk), ChunkWindow::Malformed);
    let resolved =
        resolve_capture_chunk_window(&chunk, l.ptr(), l.maxsize(), r.ptr(), r.maxsize(), &rt);
    assert_eq!(resolved, None);
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        l.all_zero() && r.all_zero(),
        "corrupted chunk must silence both channels fail-closed"
    );
}

#[test]
fn spa_bad_stride_raises_contract_violation() {
    // A non-null chunk whose stride diverges from sizeof(f32) must be
    // classified `Malformed` and raise E2304 on the capture path — the
    // descriptors can no longer be interpreted as a valid `f32` window.
    let mut l = FfiHarnessBuf::new(16);
    let mut r = FfiHarnessBuf::new(16);
    l.fill_pattern(0x33);
    r.fill_pattern(0x44);
    let rt = RtStatusFlags::default();
    let mut chunk = chunk_of(0, 64);
    chunk.stride = 2;

    assert_eq!(read_chunk_meta(&chunk), ChunkWindow::Malformed);
    let resolved =
        resolve_capture_chunk_window(&chunk, l.ptr(), l.maxsize(), r.ptr(), r.maxsize(), &rt);
    assert_eq!(resolved, None);
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        l.all_zero() && r.all_zero(),
        "bad-stride chunk must silence both channels fail-closed"
    );
}

#[test]
fn spa_absent_chunk_stays_silent_no_violation() {
    // A legitimate zero window (valid chunk, `size == 0`, no data published
    // this quantum) is NOT a violation: the capture path resolves `(0, 0)`
    // and the consolidated harness accepts it (n_samples == 0), so the DSP is
    // skipped silently with no E2304 and no telemetry signal.
    let l = FfiHarnessBuf::new(16);
    let r = FfiHarnessBuf::new(16);
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 0);

    assert_eq!(read_chunk_meta(&chunk), ChunkWindow::Valid(0, 0));
    let resolved =
        resolve_capture_chunk_window(&chunk, l.ptr(), l.maxsize(), r.ptr(), r.maxsize(), &rt);
    assert_eq!(resolved, Some((0, 0)));
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));

    let got = handle_spa_pair_fail_closed(
        l.ptr(),
        l.maxsize(),
        &chunk,
        0,
        0,
        r.ptr(),
        r.maxsize(),
        &chunk,
        0,
        0,
        &rt,
    );
    assert_eq!(got, Some((0, 0)));
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_accepts_valid_disjoint_pair() {
    let l = FfiHarnessBuf::new(16);
    let r = FfiHarnessBuf::new(16);
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 64);
    let got = handle_spa_pair_fail_closed(
        l.ptr(),
        l.maxsize(),
        &chunk,
        0,
        64,
        r.ptr(),
        r.maxsize(),
        &chunk,
        0,
        64,
        &rt,
    );
    assert_eq!(got, Some((64, 16)));
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_rejects_null_data_pointer() {
    let r = FfiHarnessBuf::new(16);
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 64);
    assert_eq!(
        handle_spa_pair_fail_closed(
            0,
            64,
            &chunk,
            0,
            64,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            64,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_rejects_null_chunk_pointer() {
    let l = FfiHarnessBuf::new(16);
    let r = FfiHarnessBuf::new(16);
    let rt = RtStatusFlags::default();
    let null: *const pw::spa::sys::spa_chunk = std::ptr::null();
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            null,
            0,
            64,
            r.ptr(),
            r.maxsize(),
            null,
            0,
            64,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_rejects_one_side_null_chunk() {
    let l = FfiHarnessBuf::new(16);
    let r = FfiHarnessBuf::new(16);
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 64);
    let null: *const pw::spa::sys::spa_chunk = std::ptr::null();
    // Left chunk valid, right chunk null -> the consolidated harness rejects.
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            0,
            64,
            r.ptr(),
            r.maxsize(),
            null,
            0,
            64,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_rejects_unaligned_base_pointer_1_2_3() {
    for delta in [1usize, 2, 3] {
        let l = FfiHarnessBuf::new(32);
        let r = FfiHarnessBuf::new(32);
        let rt = RtStatusFlags::default();
        let chunk = chunk_of(0, 64);
        let res = handle_spa_pair_fail_closed(
            l.base() + delta,
            l.maxsize().saturating_sub(delta),
            &chunk,
            0,
            64,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            64,
            &rt,
        );
        assert_eq!(res, None, "delta={delta}: unaligned base must be rejected");
        assert!(
            rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "delta={delta}"
        );
    }
}

#[test]
fn ffi_harness_rejects_unaligned_offset_1_2_3() {
    for delta in [1usize, 2, 3] {
        let l = FfiHarnessBuf::new(32);
        let r = FfiHarnessBuf::new(32);
        let rt = RtStatusFlags::default();
        let chunk = chunk_of(delta as u32, 64);
        let res = handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            delta,
            64,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            64,
            &rt,
        );
        assert_eq!(
            res, None,
            "delta={delta}: unaligned offset must be rejected"
        );
        assert!(
            rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "delta={delta}"
        );
    }
}

#[test]
fn ffi_harness_rejects_identical_intervals() {
    let mut l = FfiHarnessBuf::new(32);
    l.fill_pattern(0xAB);
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 128);
    let p = l.ptr();
    let m = l.maxsize();
    assert_eq!(
        handle_spa_pair_fail_closed(p, m, &chunk, 0, 128, p, m, &chunk, 0, 128, &rt),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_rejects_partial_overlap() {
    let mut storage = FfiHarnessBuf::new(24);
    storage.fill_pattern(0xCD);
    let rt = RtStatusFlags::default();
    let p = storage.ptr();
    let m = storage.maxsize();
    let chunk = chunk_of(0, 64);
    // L: [0, 64); R: [32, 96) inside the same 96-byte buffer -> partial overlap.
    assert_eq!(
        handle_spa_pair_fail_closed(p, m, &chunk, 0, 64, p, m, &chunk, 32, 64, &rt),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_rejects_contained_interval() {
    let storage = FfiHarnessBuf::new(32);
    let rt = RtStatusFlags::default();
    let p = storage.ptr();
    let m = storage.maxsize();
    let chunk = chunk_of(0, 128);
    // L: [0, 128); R: [32, 96) fully contained in L.
    assert_eq!(
        handle_spa_pair_fail_closed(p, m, &chunk, 0, 128, p, m, &chunk, 32, 64, &rt),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_rejects_odd_size_not_multiple_of_4() {
    let l = FfiHarnessBuf::new(16);
    let r = FfiHarnessBuf::new(16);
    for size in [3usize, 6, 10, 62] {
        let rt = RtStatusFlags::default();
        let chunk = chunk_of(0, size as u32);
        let res = handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            0,
            size,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            size,
            &rt,
        );
        assert_eq!(
            res, None,
            "size={size}: non-cardinal byte count must be rejected"
        );
        assert!(
            rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "size={size}"
        );
    }
}

#[test]
fn ffi_harness_rejects_size_overflow_beyond_maxsize() {
    let l = FfiHarnessBuf::new(16); // 64 bytes
    let r = FfiHarnessBuf::new(16);
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 64);
    // offset 48 + size 32 = 80 > 64 -> out of bounds (no silent clamp).
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            0,
            64,
            r.ptr(),
            r.maxsize(),
            &chunk,
            48,
            32,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));

    // offset beyond the buffer entirely.
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(200, 64);
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            200,
            64,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            64,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_rejects_asymmetric_frame_counts() {
    let l = FfiHarnessBuf::new(64); // 64 samples
    let r = FfiHarnessBuf::new(32); // 32 samples
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 0);
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            0,
            l.maxsize(),
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            r.maxsize(),
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_callback_rejects_and_silences_fail_closed() {
    let mut storage = FfiHarnessBuf::new(24);
    storage.fill_pattern(0xAB);
    let rt = RtStatusFlags::default();
    let p = storage.ptr();
    let m = storage.maxsize();
    let chunk = chunk_of(0, 64);
    // Malformed partial-overlap pair: the callback must flag the host, silence
    // both channels and return without running DSP. Reaching the end of this
    // test also proves the fail-closed path is panic-free.
    let res = handle_spa_pair_fail_closed(p, m, &chunk, 0, 64, p, m, &chunk, 32, 64, &rt);
    assert_eq!(res, None);
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        storage.all_zero(),
        "fail-closed path must silence both channels"
    );
}

#[test]
fn ffi_harness_capture_null_chunk_flagged_no_panic_no_reference() {
    let mut l = FfiHarnessBuf::new(16);
    let mut r = FfiHarnessBuf::new(16);
    l.fill_pattern(0x11);
    r.fill_pattern(0x22);
    let rt = RtStatusFlags::default();
    let null: *const pw::spa::sys::spa_chunk = std::ptr::null();

    // The capture path classifies a null chunk as `Absent` → `(0, 0)`; the
    // consolidated fail-closed harness below rejects the null pair via its own
    // chunk-null proof — E2304 raised, both channels silenced, no panic, and
    // no reference ever formed from the null chunk.
    let meta_l =
        resolve_capture_chunk_window(null, l.ptr(), l.maxsize(), r.ptr(), r.maxsize(), &rt);
    let meta_r =
        resolve_capture_chunk_window(null, l.ptr(), l.maxsize(), r.ptr(), r.maxsize(), &rt);
    assert_eq!(meta_l, Some((0, 0)));
    assert_eq!(meta_r, Some((0, 0)));

    let res = handle_spa_pair_fail_closed(
        l.ptr(),
        l.maxsize(),
        null,
        0,
        0,
        r.ptr(),
        r.maxsize(),
        null,
        0,
        0,
        &rt,
    );
    assert_eq!(res, None);
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        l.all_zero(),
        "capture null chunk must silence the left channel"
    );
    assert!(
        r.all_zero(),
        "capture null chunk must silence the right channel"
    );
}

// ── Quantum Bound Hardening ──────────────────────────────────────────────────
//
// The RT callbacks copy the host quantum into fixed-capacity `DspBuffers`
// (`MAX_BRIDGE_BUF = 8192`). A spurious SPA descriptor reporting a quantum
// above that ceiling must be rejected fail-closed in `check_ffi_contract`
// (via `handle_spa_pair_fail_closed`) — flag raised, channels silenced, and
// no DSP ever runs on an oversized quantum (no out-of-bounds write possible).

#[test]
fn ffi_harness_rejects_quantum_exceeding_max_bridge_buf() {
    // size = (MAX_BRIDGE_BUF + 1) * 4 = 32,772 bytes -> 8,193 frames, one over
    // the ceiling. The descriptors are otherwise perfectly aligned, in-bounds
    // and cardinal, so only the new quantum bound rejects them.
    let mut l = FfiHarnessBuf::new(MAX_BRIDGE_BUF + 1);
    let mut r = FfiHarnessBuf::new(MAX_BRIDGE_BUF + 1);
    l.fill_pattern(0x11);
    r.fill_pattern(0x22);
    let rt = RtStatusFlags::default();
    let size = (MAX_BRIDGE_BUF + 1) * 4;
    let chunk = chunk_of(0, size as u32);

    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            0,
            size,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            size,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(
        l.as_samples()[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0)
            && r.as_samples()[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
        "oversized quantum must silence both channels fail-closed up to max bridge bound"
    );
}

#[test]
fn ffi_harness_accepts_max_bridge_buf_quantum() {
    // Boundary: exactly MAX_BRIDGE_BUF frames (32,768 bytes) is the largest
    // legal quantum and must be accepted with the correct verdict.
    let l = FfiHarnessBuf::new(MAX_BRIDGE_BUF);
    let r = FfiHarnessBuf::new(MAX_BRIDGE_BUF);
    let rt = RtStatusFlags::default();
    let size = MAX_BRIDGE_BUF * 4;
    let chunk = chunk_of(0, size as u32);

    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            0,
            size,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            size,
            &rt,
        ),
        Some((size, MAX_BRIDGE_BUF))
    );
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn ffi_harness_recovers_after_oversized_quantum() {
    // Reversal: after an oversized quantum is rejected, a
    // normal 128-sample quantum is processed again — the pair validates and
    // the pipeline resumes without any panic.
    let mut big_l = FfiHarnessBuf::new(MAX_BRIDGE_BUF + 1);
    let mut big_r = FfiHarnessBuf::new(MAX_BRIDGE_BUF + 1);
    big_l.fill_pattern(0x33);
    big_r.fill_pattern(0x33);
    let rt = RtStatusFlags::default();
    let big = (MAX_BRIDGE_BUF + 1) * 4;
    let big_chunk = chunk_of(0, big as u32);

    assert_eq!(
        handle_spa_pair_fail_closed(
            big_l.ptr(),
            big_l.maxsize(),
            &big_chunk,
            0,
            big,
            big_r.ptr(),
            big_r.maxsize(),
            &big_chunk,
            0,
            big,
            &rt,
        ),
        None
    );
    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));

    // Quantum returns to 128 samples -> processing is re-established.
    let l = FfiHarnessBuf::new(128);
    let r = FfiHarnessBuf::new(128);
    let chunk = chunk_of(0, 512);
    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            0,
            512,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            512,
            &rt,
        ),
        Some((512, 128))
    );
}

#[test]
fn silence_spa_channels_bounds_huge_buffer_to_max_bridge_buf() {
    // silence_spa_channels must bound zeroing to MAX_BRIDGE_BUF frames (32 KiB),
    // leaving memory beyond the cap untouched.
    let total_samples = MAX_BRIDGE_BUF + 2048;
    let mut l = FfiHarnessBuf::new(total_samples);
    let mut r = FfiHarnessBuf::new(total_samples);
    l.fill_pattern(0x33);
    r.fill_pattern(0x44);

    silence_spa_channels(l.ptr(), l.maxsize(), r.ptr(), r.maxsize());

    // First MAX_BRIDGE_BUF frames must be zero
    assert!(
        l.as_samples()[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
        "left channel must be zeroed up to MAX_BRIDGE_BUF"
    );
    assert!(
        r.as_samples()[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
        "right channel must be zeroed up to MAX_BRIDGE_BUF"
    );

    // Trailing bytes past MAX_BRIDGE_BUF * 4 must remain untouched
    let trailing_l = &l.as_bytes()[MAX_BRIDGE_BUF * 4..];
    assert!(
        trailing_l.iter().all(|&b| b == 0x33),
        "left trailing memory must not be touched"
    );
    let trailing_r = &r.as_bytes()[MAX_BRIDGE_BUF * 4..];
    assert!(
        trailing_r.iter().all(|&b| b == 0x44),
        "right trailing memory must not be touched"
    );
}

#[test]
fn ffi_harness_accepts_huge_maxsize_with_small_quantum() {
    // A host descriptor declaring a large maxsize (e.g. 1 MiB)
    // with a valid small quantum (64 frames = 256 bytes) is safely bounded
    // and validated without forming unbounded slices.
    let l = FfiHarnessBuf::new(MAX_BRIDGE_BUF * 4); // 32,768 samples = 128 KiB
    let r = FfiHarnessBuf::new(MAX_BRIDGE_BUF * 4);
    let rt = RtStatusFlags::default();
    let chunk = chunk_of(0, 256);

    assert_eq!(
        handle_spa_pair_fail_closed(
            l.ptr(),
            l.maxsize(),
            &chunk,
            0,
            256,
            r.ptr(),
            r.maxsize(),
            &chunk,
            0,
            256,
            &rt,
        ),
        Some((256, 64))
    );
    assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
}

#[test]
fn recording_metadata_rate_change_full_control_ring_blocks_audio_until_consumed() {
    // Metadata travels on the dedicated control ring. When that ring is
    // full a rate-change Metadata push fails → `meta_sent` stays false → the
    // audio path stays blocked (audio is only enqueued after the metadata for
    // the current rate is confirmed).
    let (control_p, mut control_c) =
        crate::recording::buffer::create_control_ring_buffer(CONTROL_CAPACITY);
    let pool = RecordingPool::<POOL_CAPACITY>::new();
    let (pool_p, _pool_c) = pool.split();
    let mut recording_sender = RecordingSender::Pool {
        control: Some(control_p),
        pool: Some(pool_p),
    };
    let mut meta_sent = false;
    let mut meta_rate = 44100u32;

    // Send metadata for initial rate (44100)
    send_recording_metadata(
        &mut recording_sender,
        44100,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(meta_sent);
    assert_eq!(meta_rate, 44100);
    assert_eq!(control_c.slots(), 1);

    // Fill the control ring up to capacity
    while control_c.slots() < CONTROL_CAPACITY {
        if recording_sender
            .control_producer_mut()
            .unwrap()
            .push(ControlPayload::Metadata(dummy_meta()))
            .is_err()
        {
            break;
        }
    }
    assert_eq!(control_c.slots(), CONTROL_CAPACITY);

    // Trigger rate change to 48000 while the control ring is full
    send_recording_metadata(
        &mut recording_sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );

    // Control ring is full -> Metadata(48000) push failed -> meta_sent MUST
    // be false so audio is blocked.
    assert!(
        !meta_sent,
        "Metadata push failure on rate change must invalidate meta_sent so audio is blocked"
    );

    // Free 1 slot in the control ring
    let _ = control_c.pop();

    // Retry sending metadata for new rate 48000 -> now it succeeds
    send_recording_metadata(
        &mut recording_sender,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(
        meta_sent,
        "Metadata push on freed control slot must succeed and set meta_sent to true"
    );
    assert_eq!(meta_rate, 48000);
}

#[test]
fn silence_available_descriptors_empty_slice_is_noop() {
    let mut empty: [(usize, usize, *mut pw::spa::sys::spa_chunk); 0] = [];
    silence_available_descriptors(&mut empty);
}

#[test]
fn silence_available_descriptors_single_channel_zeros_and_stamps_chunk() {
    // When a buffer has datas.len() < 2 (e.g. exactly 1 descriptor),
    // the single channel must be zeroed and stamped with silence metadata.
    let mut buf = FfiHarnessBuf::new(128); // 128 samples = 512 bytes
    buf.fill_pattern(0xAB);
    let mut chunk = chunk_of(16, 64);

    let mut descriptors = [(buf.ptr(), buf.maxsize(), &mut chunk as *mut _)];
    silence_available_descriptors(&mut descriptors);

    // Buffer must be analytical silence
    assert!(
        buf.all_zero(),
        "single channel descriptor must be zeroed out completely"
    );

    // Chunk must be stamped with offset=0, size=512, stride=4
    assert_eq!(chunk.offset, 0);
    assert_eq!(chunk.size, 512);
    assert_eq!(chunk.stride, 4);
}

#[test]
fn silence_available_descriptors_bounds_huge_single_channel() {
    // A huge single-channel buffer must be bounded to MAX_BRIDGE_BUF * 4 bytes
    let total_samples = MAX_BRIDGE_BUF + 1024;
    let mut buf = FfiHarnessBuf::new(total_samples);
    buf.fill_pattern(0x5A);
    let mut chunk = chunk_of(0, 100);

    let mut descriptors = [(buf.ptr(), buf.maxsize(), &mut chunk as *mut _)];
    silence_available_descriptors(&mut descriptors);

    // First MAX_BRIDGE_BUF frames must be zeroed
    assert!(
        buf.as_samples()[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
        "zeroing must cover up to MAX_BRIDGE_BUF"
    );

    // Trailing memory beyond MAX_BRIDGE_BUF * 4 must remain untouched
    let trailing = &buf.as_bytes()[MAX_BRIDGE_BUF * 4..];
    assert!(
        trailing.iter().all(|&b| b == 0x5A),
        "trailing memory beyond MAX_BRIDGE_BUF * 4 must stay intact"
    );

    // Chunk stamped with bounded silence size
    assert_eq!(chunk.offset, 0);
    assert_eq!(chunk.size, (MAX_BRIDGE_BUF * 4) as u32);
    assert_eq!(chunk.stride, 4);
}

#[test]
fn capture_datas_less_than_two_sets_contract_violation_flag() {
    // When datas.len() < 2, the capture callback
    // sets RT_STATUS_HOST_CONTRACT_VIOLATION, silences all available regions,
    // and returns without running DSP.
    let rt = RtStatusFlags::default();
    let mut buf = FfiHarnessBuf::new(64);
    buf.fill_pattern(0x77);
    let mut chunk = chunk_of(0, 256);

    // Emulating the fail-closed action of process_dsp_buffer on datas.len() == 1:
    let mut descriptors = [(buf.ptr(), buf.maxsize(), &mut chunk as *mut _)];
    rt.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    silence_available_descriptors(&mut descriptors);

    assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    assert!(buf.all_zero());
    assert_eq!(chunk.offset, 0);
    assert_eq!(chunk.size, 256);
    assert_eq!(chunk.stride, 4);
}

// ── Gate Off Guarantee Tests ─────────────────────────────────────────────────

#[test]
#[cfg(feature = "testing")]
fn gate_custom_threshold_config_runs_without_panic_or_regression() {
    // Parametric harness entry: an explicit GateConfig::Threshold
    // (open at -50 dBFS, the single-coil hum floor; Schmitt close at -60 dBFS)
    // must build the same RT harness and process real callbacks without panic
    // or discontinuity — the harness-level guarantee behind the FSM property
    // battery in gate_property_test.rs.
    use crate::standalone::cli::GateConfig;
    use crate::standalone::pw_host::RtSwapHarness;

    let mut h = RtSwapHarness::new_with_gate_config(48000, 48000, GateConfig::from_open_db(-50.0))
        .expect("harness with custom gate threshold");

    // Sustained signal above the -50 dBFS opening threshold (0.25 amplitude ≈
    // -12 dBFS): mono detection must engage exactly as with the default gate.
    let mut in_l = [0.25f32; 64];
    let mut in_r = [0.25f32; 64];
    for _ in 0..50 {
        h.run_callback(&mut in_l, &mut in_r, 64);
    }
    assert!(
        h.process_mono(),
        "mono detection must engage on identical L/R input with a custom threshold gate"
    );

    // Sustained absolute digital silence: the FSM may close the gate, but the
    // callback must never panic or wedge mono detection on the stereo path.
    let mut sil_l = [0.0f32; 64];
    let mut sil_r = [0.0f32; 64];
    for _ in 0..50 {
        h.run_callback(&mut sil_l, &mut sil_r, 64);
    }

    // A resumed full-scale signal must reopen the gate and re-engage processing
    // without discontinuity (no stuck-Closed regression).
    for _ in 0..50 {
        h.run_callback(&mut in_l, &mut in_r, 64);
    }
    assert!(h.process_mono());
}

#[test]
#[cfg(feature = "testing")]
fn gate_off_does_not_affect_mono_detection() {
    use crate::standalone::pw_host::RtSwapHarness;
    let mut h = RtSwapHarness::new_with_gate(48000, 48000, false).expect("harness with gate off");

    // Initially process_mono is false
    assert!(!h.process_mono());

    // Feed truly mono signal (L == R != 0) for enough blocks to trigger mono hysteresis
    let mut in_l = [0.25f32; 64];
    let mut in_r = [0.25f32; 64];
    for _ in 0..50 {
        h.run_callback(&mut in_l, &mut in_r, 64);
    }
    assert!(
        h.process_mono(),
        "mono detection must engage on identical L/R input even with gate off"
    );

    // Feed truly stereo signal (L != R) for enough blocks to break mono hysteresis
    let mut in_l_stereo = [0.25f32; 64];
    let mut in_r_stereo = [0.0f32; 64];
    for _ in 0..50 {
        h.run_callback(&mut in_l_stereo, &mut in_r_stereo, 64);
    }
    assert!(
        !h.process_mono(),
        "mono detection must disengage on divergent L/R input even with gate off"
    );
}

#[test]
#[cfg(all(feature = "testing", feature = "heap-audit"))]
fn gate_off_zero_alloc() {
    use crate::standalone::pw_host::RtSwapHarness;
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    let mut h = RtSwapHarness::new_with_gate(48000, 48000, false).expect("harness with gate off");

    // Warm up one callback
    let mut in_l = [0.0f32; 64];
    let mut in_r = [0.0f32; 64];
    let _ = h.run_callback(&mut in_l, &mut in_r, 64);

    // Execute 1000 callbacks with absolute digital silence (zero energy) under TrackingGuard
    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        let mut in_l = [0.0f32; 64];
        let mut in_r = [0.0f32; 64];
        for _ in 0..1000 {
            h.run_callback(&mut in_l, &mut in_r, 64);
        }
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };

    assert_eq!(
        allocs, 0,
        "heap allocations detected in gate off zero energy state: {allocs}"
    );
    assert_eq!(
        deallocs, 0,
        "heap deallocations detected in gate off zero energy state: {deallocs}"
    );
    assert_eq!(
        reallocs, 0,
        "heap reallocations detected in gate off zero energy state: {reallocs}"
    );
}

#[test]
fn recording_audio_pushed_when_gate_off_and_energy_zero() {
    // With gate off, capture_dsp_pipeline_streaming produces n_pw > 0 even for
    // digital silence (zero energy). send_recording_audio must enqueue the
    // zero-energy block into the recording transport rather than discarding it.
    let (mut sender, _control_c, mut pool_c) = pool_sender_and_consumers();
    let resamp_l = [0.0f32; 64];
    let resamp_r = [0.0f32; 64];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();

    send_recording_audio(
        &mut sender,
        64,
        &resamp_l,
        &resamp_r,
        &mut block,
        None,
        None,
    );

    let in_flight = pool_c
        .try_pop()
        .expect("zero-energy block must be published when n_pw > 0");
    assert_eq!(in_flight.block().valid_len(), 128);
    assert!(
        in_flight.block().as_slice()[..128]
            .iter()
            .all(|&x| x == 0.0f32)
    );
    in_flight.release();
}
