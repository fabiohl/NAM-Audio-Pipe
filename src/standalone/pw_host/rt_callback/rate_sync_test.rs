// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_NEEDS_RESAMPLER_REBUILD, RT_STATUS_RESAMP_SWAP_PENDING,
};
use std::sync::atomic::AtomicU32;

fn make_resampler(pw: u32, nam: u32) -> NamResampler {
    NamResampler::new_simple(pw, nam).unwrap()
}

#[test]
fn no_rate_change_no_flags() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    let result = sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);

    assert_eq!(result, 48000);
    assert!(!flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(!flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
}

#[test]
fn pw_rate_change_sets_flags() {
    let rate = AtomicU32::new(44100);
    let rs = make_resampler(48000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    let result = sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);

    assert_eq!(result, 48000);
    assert!(flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    assert_eq!(flags.requested_host_rate.load(Ordering::Relaxed), 44100);
    assert_eq!(
        flags.requested_rate_generation.load(Ordering::Relaxed),
        1,
        "first rebuild request must publish generation 1"
    );
}

#[test]
fn nam_rate_change_sets_flags() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    let result = sync_rate(&rate, &rs, 44100, &flags, &mut sync_state);

    assert_eq!(result, 48000);
    assert!(flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
}

#[test]
fn zero_detected_rate_ignored() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 44100);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    let result = sync_rate(&rate, &rs, 44100, &flags, &mut sync_state);

    assert_eq!(result, 48000);
    assert!(!flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
}

#[test]
fn both_rates_unchanged_no_flags() {
    let rate = AtomicU32::new(48000);
    let rs = make_resampler(48000, 44100);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    let result = sync_rate(&rate, &rs, 44100, &flags, &mut sync_state);

    assert_eq!(result, 48000);
    assert!(!flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
}

#[test]
fn rate_sync_returns_current_pw_rate() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(96000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(96000);

    let result = sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);

    assert_eq!(result, 96000);
}

#[test]
fn nam_rate_mismatch_with_zero_detected_pw_rate_sets_flags() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 44100);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    let result = sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);

    assert_eq!(result, 48000);
    assert!(flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    assert_eq!(flags.requested_nam_rate.load(Ordering::Relaxed), 48000);
}

#[test]
fn consecutive_renegotiations_increment_generation() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    // First detection: PW clock moves to 44100 -> generation 1.
    rate.store(44100, Ordering::Release);
    sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
    assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 1);

    // The host renegotiates again while the first rebuild is still in
    // flight: 96000 -> generation 2. The request must not be erased.
    rate.store(96000, Ordering::Release);
    sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
    assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 2);
    assert_eq!(flags.requested_host_rate.load(Ordering::Relaxed), 96000);
    assert!(flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
}

#[test]
fn idempotent_across_multiple_quanta_preserves_generation() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    // Initial mismatch: PW clock moves to 96000 -> generation 1
    rate.store(96000, Ordering::Release);
    sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
    assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 1);
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));

    // Subsequent 50 audio callbacks execute before main thread finishes building.
    // In each callback, rate.swap(0) yields 0, active resampler is still (48k, 48k).
    // sync_rate MUST NOT bump generation or clear pending!
    for _ in 0..50 {
        sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
        assert_eq!(
            flags.requested_rate_generation.load(Ordering::Relaxed),
            1,
            "generation must remain 1 throughout multiple in-flight quanta"
        );
        assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
        assert_eq!(flags.requested_host_rate.load(Ordering::Relaxed), 96000);
    }
}

#[test]
fn preserved_desired_host_rate_when_atomic_swap_returns_zero() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    // Quantum 1: PipeWire notifies 96000.
    rate.store(96000, Ordering::Release);
    sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
    assert_eq!(sync_state.desired_host_rate, 96000);

    // Quantum 2: rate atomic was swapped to 0. A model change occurs (current_nam_rate = 44100).
    // sync_rate must preserve desired_host_rate (96000), NOT fall back to active resampler (48000)!
    sync_rate(&rate, &rs, 44100, &flags, &mut sync_state);
    assert_eq!(
        flags.requested_host_rate.load(Ordering::Relaxed),
        96000,
        "must preserve target host rate 96000 instead of reverting to active 48000"
    );
    assert_eq!(flags.requested_nam_rate.load(Ordering::Relaxed), 44100);
    assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 2);
}

#[test]
fn return_to_active_rate_invalidates_in_flight_and_unmutes() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    // Host asks for 96000 -> generation 1, swap pending
    rate.store(96000, Ordering::Release);
    sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
    assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 1);
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));

    // 5 callbacks pass with build in flight
    for _ in 0..5 {
        sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
        assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 1);
    }

    // Host renegotiates back to 48000 (retorno à taxa vigente)
    rate.store(48000, Ordering::Release);
    sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);

    // Generation must be advanced to 2 to invalidate in-flight generation 1
    assert_eq!(
        flags.requested_rate_generation.load(Ordering::Relaxed),
        2,
        "generation must advance to invalidate obsolete in-flight build"
    );
    assert_eq!(
        flags.applied_rate_generation.load(Ordering::Relaxed),
        2,
        "applied generation must match requested generation"
    );
    assert!(
        !flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING),
        "swap pending must be cleared immediately since active resampler matches desired rate"
    );
    assert!(
        !flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD),
        "needs rebuild must be cleared"
    );
}

#[test]
fn failed_build_rollback_and_retry_limit() {
    let rate = AtomicU32::new(0);
    let rs = make_resampler(48000, 48000);
    let flags = RtStatusFlags::new();
    let mut sync_state = RateSyncState::new(48000);

    // Request rebuild for 192000
    rate.store(192000, Ordering::Release);
    sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
    assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 1);
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));

    // Simulate main-thread build failure & fail-open rollback
    flags.clear_flag(RT_STATUS_RESAMP_SWAP_PENDING);
    flags.clear_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD);
    flags.applied_rate_generation.store(1, Ordering::Release);
    sync_state.record_failure();

    assert_eq!(sync_state.failed_rates, (192000, 48000));
    assert_eq!(sync_state.failed_retry_count, 1);

    // Next 10 callbacks with same rate mismatch: must NOT issue new rebuild requests!
    for _ in 0..10 {
        sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
        assert!(
            !flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD),
            "must not re-arm rebuild for failed rate tuple"
        );
        assert!(
            !flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING),
            "must not mute audio for already failed rate tuple"
        );
        assert_eq!(
            flags.requested_rate_generation.load(Ordering::Relaxed),
            1,
            "generation must not advance"
        );
    }

    // New request for a different rate (96000): failure latch clears and rebuild proceeds!
    rate.store(96000, Ordering::Release);
    sync_rate(&rate, &rs, 48000, &flags, &mut sync_state);
    assert_eq!(
        flags.requested_rate_generation.load(Ordering::Relaxed),
        2,
        "new valid rate must trigger generation 2"
    );
    assert!(flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    assert_eq!(sync_state.failed_rates, (0, 0));
    assert_eq!(sync_state.failed_retry_count, 0);
}
