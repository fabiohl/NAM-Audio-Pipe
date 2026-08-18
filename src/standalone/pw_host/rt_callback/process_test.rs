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
fn ffi_contract_size_clamped_rejected_if_offset_oob() {
    let buf = [0u8; 128];
    // offset is valid but size > remaining -> size is clamped
    let r = check_ffi_contract(&buf, 64, 128);
    assert!(r.is_some());
    let (n_bytes, n_samples) = r.unwrap();
    assert_eq!(n_bytes, 64);
    assert_eq!(n_samples, 16);

    // offset at end -> 0 bytes
    let r = check_ffi_contract(&buf, 128, 64);
    assert!(r.is_some());
    assert_eq!(r.unwrap(), (0, 0));
}

#[test]
fn ffi_contract_misaligned_when_clamped() {
    let buf = [0u8; 5];
    // size=8 clamped to 5, but 5 is not f32-aligned
    assert!(check_ffi_contract(&buf, 0, 8).is_none());
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

    send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate, None);

    assert!(!meta_sent, "metadata must not be confirmed when push fails");
    assert_eq!(meta_rate, 0);

    // Free the channel and retry: the flag is confirmed.
    let _ = cons.pop().unwrap();
    send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate, None);

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

    send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate, None);
    assert!(meta_sent);
    assert_eq!(meta_rate, 48000);
    let _ = cons.pop().unwrap();

    // Same rate again: no duplicate header.
    send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate, None);
    assert!(cons.pop().is_err());

    // Host rate change: a new header is emitted for the new rate.
    send_recording_metadata(&mut prod_opt, 44100, &mut meta_sent, &mut meta_rate, None);
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

    send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate, None);

    assert!(!meta_sent);
    assert_eq!(meta_rate, 0);
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
    );

    assert!(flag.load(Ordering::Relaxed));
    match cons.pop().unwrap() {
        RingPayload::Audio(b) => {
            assert_eq!(b.valid_len, 8);
            assert_eq!(b.data[0], 0.0);
            assert_eq!(b.data[1], -0.0);
            assert_eq!(b.data[2], 1.0);
            assert_eq!(b.data[3], -1.0);
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
    );

    assert!(!flag.load(Ordering::Relaxed));
    assert_eq!(OVERRUN_COUNT.load(Ordering::Relaxed), 1);

    OVERRUN_COUNT.store(0, Ordering::Relaxed);
}
