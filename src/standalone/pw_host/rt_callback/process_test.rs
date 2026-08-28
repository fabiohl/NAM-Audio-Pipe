// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::recording::buffer::create_audio_ring_buffer;

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
    let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(2);
    let mut prod_opt = Some(prod);
    let mut meta_sent = false;
    let mut meta_rate = 0u32;
    let flag = std::sync::atomic::AtomicBool::new(false);

    send_recording_metadata(
        &mut prod_opt,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        Some(&flag),
        None,
    );

    assert!(meta_sent);
    assert_eq!(meta_rate, 48000);
    assert!(flag.load(Ordering::Relaxed));
    match cons.pop().unwrap() {
        RingPayload::Metadata(m) => assert_eq!(m.sample_rate, 48000.0),
        _ => panic!("expected Metadata"),
    }
}

#[test]
fn recording_metadata_not_confirmed_when_channel_full() {
    let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(1);
    let mut prod_opt = Some(prod);
    let mut meta_sent = false;
    let mut meta_rate = 0u32;

    // Saturate the single-slot channel.
    prod_opt
        .as_mut()
        .unwrap()
        .push(RingPayload::Audio(AlignedBlock::new()))
        .unwrap();

    send_recording_metadata(
        &mut prod_opt,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );

    assert!(!meta_sent, "metadata must not be confirmed when push fails");
    assert_eq!(meta_rate, 0);

    // Free the channel and retry: the flag is confirmed.
    let _ = cons.pop().unwrap();
    send_recording_metadata(
        &mut prod_opt,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );

    assert!(meta_sent);
    assert_eq!(meta_rate, 48000);
    match cons.pop().unwrap() {
        RingPayload::Metadata(m) => assert_eq!(m.sample_rate, 48000.0),
        _ => panic!("expected Metadata"),
    }
}

#[test]
fn recording_metadata_reset_on_host_rate_change() {
    let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
    let mut prod_opt = Some(prod);
    let mut meta_sent = false;
    let mut meta_rate = 0u32;

    send_recording_metadata(
        &mut prod_opt,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(meta_sent);
    assert_eq!(meta_rate, 48000);
    let _ = cons.pop().unwrap();

    // Same rate again: no duplicate header.
    send_recording_metadata(
        &mut prod_opt,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(cons.pop().is_err());

    // Host rate change: a new header is emitted for the new rate.
    send_recording_metadata(
        &mut prod_opt,
        44100,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(meta_sent);
    assert_eq!(meta_rate, 44100);
    match cons.pop().unwrap() {
        RingPayload::Metadata(m) => assert_eq!(m.sample_rate, 44100.0),
        _ => panic!("expected Metadata"),
    }
}

#[test]
fn recording_metadata_absent_producer_never_confirmed() {
    let mut prod_opt: Option<Producer<RingPayload<MAX_BLOCK_SIZE>>> = None;
    let mut meta_sent = false;
    let mut meta_rate = 0u32;

    send_recording_metadata(
        &mut prod_opt,
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
    // F-RB-009 / T3.3: once the disk worker reports a fatal error, the RT
    // callback must suspend enqueueing — the metadata must NOT be pushed.
    let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(2);
    let mut prod_opt = Some(prod);
    let mut meta_sent = false;
    let mut meta_rate = 0u32;
    let failed = AtomicBool::new(true);

    send_recording_metadata(
        &mut prod_opt,
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
    assert!(cons.pop().is_err(), "no metadata may reach the dead worker");
}

#[test]
fn recording_audio_not_pushed_when_worker_failed() {
    // F-RB-009 / T3.3: with the failure flag raised the audio block must be
    // dropped cleanly — no push, no overrun accounting (the worker is gone).
    let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);

    let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
    let mut prod_opt = Some(prod);
    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let failed = AtomicBool::new(true);

    send_recording_audio(
        &mut prod_opt,
        4,
        &resamp_l,
        &resamp_r,
        &mut block,
        None,
        Some(&failed),
    );

    assert!(cons.pop().is_err(), "no audio may reach the dead worker");
    assert_eq!(
        OVERRUN_COUNT.load(Ordering::Relaxed),
        0,
        "suspended enqueueing must not inflate the overrun counter"
    );
    OVERRUN_COUNT.store(0, Ordering::Relaxed);
}

#[test]
fn recording_audio_pushed_again_once_failure_clears() {
    // The failure flag is latched by the worker; if it is ever cleared the RT
    // path resumes pushing normally (flag is the sole gate).
    let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
    let mut prod_opt = Some(prod);
    let resamp_l = [0.0f32; 4];
    let resamp_r = [0.0f32; 4];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let failed = AtomicBool::new(false);

    send_recording_audio(
        &mut prod_opt,
        4,
        &resamp_l,
        &resamp_r,
        &mut block,
        None,
        Some(&failed),
    );

    match cons.pop().unwrap() {
        RingPayload::Audio(b) => assert_eq!(b.valid_len(), 8),
        _ => panic!("expected Audio"),
    }
}

#[test]
fn recording_audio_oversized_block_dropped_and_counted() {
    let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);

    let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
    let mut prod_opt = Some(prod);
    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();

    // interleaved_len = MAX_BLOCK_SIZE * 2 > MAX_BLOCK_SIZE -> dropped.
    send_recording_audio(
        &mut prod_opt,
        MAX_BLOCK_SIZE,
        &resamp_l,
        &resamp_r,
        &mut block,
        None,
        None,
    );

    assert!(cons.pop().is_err(), "oversized block must not be pushed");
    assert_eq!(OVERRUN_COUNT.load(Ordering::Relaxed), 1);

    OVERRUN_COUNT.store(0, Ordering::Relaxed);
}

#[test]
fn recording_audio_normal_block_pushed() {
    let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
    let mut prod_opt = Some(prod);
    let mut resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let mut resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    for i in 0..4 {
        resamp_l[i] = i as f32;
        resamp_r[i] = -(i as f32);
    }
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let flag = std::sync::atomic::AtomicBool::new(false);

    send_recording_audio(
        &mut prod_opt,
        4,
        &resamp_l,
        &resamp_r,
        &mut block,
        Some(&flag),
        None,
    );

    assert!(flag.load(Ordering::Relaxed));
    match cons.pop().unwrap() {
        RingPayload::Audio(b) => {
            assert_eq!(b.valid_len(), 8);
            // Planar layout: L = [0, 1, 2, 3], R = [-0, -1, -2, -3].
            assert_eq!(b.as_slice(), &[0.0, 1.0, 2.0, 3.0, -0.0, -1.0, -2.0, -3.0]);
        }
        _ => panic!("expected Audio"),
    }
}

#[test]
fn recording_audio_full_channel_counted_as_overrun() {
    let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);

    let (prod, _cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(1);
    let mut prod_opt = Some(prod);
    prod_opt
        .as_mut()
        .unwrap()
        .push(RingPayload::Audio(AlignedBlock::new()))
        .unwrap();

    let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
    let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    let flag = std::sync::atomic::AtomicBool::new(false);

    send_recording_audio(
        &mut prod_opt,
        4,
        &resamp_l,
        &resamp_r,
        &mut block,
        Some(&flag),
        None,
    );

    assert!(!flag.load(Ordering::Relaxed));
    assert_eq!(OVERRUN_COUNT.load(Ordering::Relaxed), 1);

    OVERRUN_COUNT.store(0, Ordering::Relaxed);
}

// ── Malformed FFI/SPA harness (F-RB-003 Part 3 / T1.5) ──────────────────────
//
// The harness feeds raw SPA descriptor values (data pointers, maxsize, chunk
// metadata read as integers) to the exact fail-closed code the RT callbacks
// run, without requiring a live PipeWire stream. It proves every adversarial
// frontier scenario from F-RB-003 / ER-1 step 4 is rejected with the
// `RT_STATUS_HOST_CONTRACT_VIOLATION` flag raised, no panic, and the buffers
// silenced.

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
    assert_eq!(read_chunk_meta(std::ptr::null()), None);
}

#[test]
fn read_chunk_meta_reads_host_window() {
    let chunk = chunk_of(8, 64);
    assert_eq!(read_chunk_meta(&chunk), Some((8, 64)));
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

    // The capture path reads chunk metadata via `read_chunk_meta`; a null chunk
    // must never form a reference and the callback must react fail-closed
    // (flag + silence + early return) — the exact branch `process_dsp_buffer`
    // takes when `read_chunk_meta` yields `None`.
    let meta_l = read_chunk_meta(std::ptr::null());
    let meta_r = read_chunk_meta(std::ptr::null());
    if meta_l.is_none() || meta_r.is_none() {
        report_ffi_contract_violation(&rt, l.ptr(), l.maxsize(), r.ptr(), r.maxsize());
    }

    assert!(meta_l.is_none() && meta_r.is_none());
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

// ── Quantum bound hardening (G-RB-003 / T6.2) ────────────────────────────────
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
    // Reversal (T6.2 acceptance): after an oversized quantum is rejected, a
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
fn recording_metadata_rate_change_full_ring_blocks_audio_until_consumed() {
    let (prod, mut cons) = create_audio_ring_buffer(crate::recording::buffer::RING_CAPACITY);
    let mut recording_producer = Some(prod);
    let mut meta_sent = false;
    let mut meta_rate = 44100u32;

    // Send metadata for initial rate (44100)
    send_recording_metadata(
        &mut recording_producer,
        44100,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(meta_sent);
    assert_eq!(meta_rate, 44100);
    assert_eq!(cons.slots(), 1);

    // Fill the ring buffer up to capacity
    while cons.slots() < crate::recording::buffer::RING_CAPACITY {
        let block = crate::recording::buffer::AlignedBlock::new_uninit();
        if recording_producer
            .as_mut()
            .unwrap()
            .push(crate::recording::buffer::RingPayload::Audio(block))
            .is_err()
        {
            break;
        }
    }
    assert_eq!(cons.slots(), crate::recording::buffer::RING_CAPACITY);

    // Trigger rate change to 48000 while ring is full
    send_recording_metadata(
        &mut recording_producer,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );

    // Ring is full -> Metadata(48000) push failed -> meta_sent MUST be false
    assert!(
        !meta_sent,
        "Metadata push failure on rate change must invalidate meta_sent so audio is blocked"
    );

    // Free 1 slot in ring buffer
    let _ = cons.pop();

    // Retry sending metadata for new rate 48000 -> now it succeeds
    send_recording_metadata(
        &mut recording_producer,
        48000,
        &mut meta_sent,
        &mut meta_rate,
        None,
        None,
    );
    assert!(
        meta_sent,
        "Metadata push on freed ring slot must succeed and set meta_sent to true"
    );
    assert_eq!(meta_rate, 48000);
}
