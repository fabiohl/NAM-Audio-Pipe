// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.3. RATE SYNCHRONIZATION (Clock Tracking)
//! Checks for frequency discrepancy and sends a request to the Main Thread.

use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;

use std::sync::atomic::Ordering;

/// 5.1.3. RATE SYNCHRONIZATION (Clock Tracking)
/// Checks for frequency discrepancy and sends a request to the Main Thread.
#[inline(always)]
pub fn sync_rate(
    rate_for_process: &std::sync::atomic::AtomicU32,
    resampler: &NamResampler,
    current_nam_rate: u32,
    rt_status_for_process: &RtStatusFlags,
) -> u32 {
    let detected_host_rate = rate_for_process.swap(0, Ordering::Acquire); // Pairs with Release store in param_changed_handler (capture/listeners.rs)
    let current_host_rate = resampler.host_rate();

    let mut host_rate_to_request = current_host_rate;
    let mut requires_rebuild = false;

    if detected_host_rate != 0 && detected_host_rate != current_host_rate {
        host_rate_to_request = detected_host_rate;
        requires_rebuild = true;
    }

    if current_nam_rate != resampler.nam_rate() {
        requires_rebuild = true;
    }

    if requires_rebuild && host_rate_to_request != 0 {
        // F-RB-004: publish the requested rates FIRST, then increment the
        // generation with Release. The main thread captures the generation
        // with Acquire, which orders these rate stores into its build
        // snapshot; a renegotiation arriving during a rebuild bumps the
        // generation again, so a stale envelope can be detected by the RT
        // drain and discarded without unmuting.
        rt_status_for_process
            .requested_host_rate
            .store(host_rate_to_request, Ordering::Relaxed);
        rt_status_for_process
            .requested_nam_rate
            .store(current_nam_rate, Ordering::Relaxed);
        rt_status_for_process
            .requested_rate_generation
            .fetch_add(1, Ordering::Release);
        rt_status_for_process.set_flag_release(
            neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD,
        );
        rt_status_for_process
            .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);
    }

    current_host_rate
}

#[cfg(test)]
mod tests {
    use super::*;
    use neural_amp_modeler_rs::common::spsc::{
        RT_STATUS_NEEDS_RESAMPLER_REBUILD, RT_STATUS_RESAMP_SWAP_PENDING,
    };
    use std::sync::atomic::AtomicU32;

    fn make_resampler(pw: u32, nam: u32) -> NamResampler {
        NamResampler::new(pw, nam, 64).unwrap()
    }

    #[test]
    fn no_rate_change_no_flags() {
        let rate = AtomicU32::new(0);
        let rs = make_resampler(48000, 48000);
        let flags = RtStatusFlags::new();

        let result = sync_rate(&rate, &rs, 48000, &flags);

        assert_eq!(result, 48000);
        assert!(!flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
        assert!(!flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    }

    #[test]
    fn pw_rate_change_sets_flags() {
        let rate = AtomicU32::new(44100);
        let rs = make_resampler(48000, 48000);
        let flags = RtStatusFlags::new();

        let result = sync_rate(&rate, &rs, 48000, &flags);

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

        let result = sync_rate(&rate, &rs, 44100, &flags);

        assert_eq!(result, 48000);
        assert!(flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
        assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    }

    #[test]
    fn zero_detected_rate_ignored() {
        let rate = AtomicU32::new(0);
        let rs = make_resampler(48000, 44100);
        let flags = RtStatusFlags::new();

        let result = sync_rate(&rate, &rs, 44100, &flags);

        assert_eq!(result, 48000);
        assert!(!flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    }

    #[test]
    fn both_rates_unchanged_no_flags() {
        let rate = AtomicU32::new(48000);
        let rs = make_resampler(48000, 44100);
        let flags = RtStatusFlags::new();

        let result = sync_rate(&rate, &rs, 44100, &flags);

        assert_eq!(result, 48000);
        assert!(!flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    }

    #[test]
    fn rate_sync_returns_current_pw_rate() {
        let rate = AtomicU32::new(0);
        let rs = make_resampler(96000, 48000);
        let flags = RtStatusFlags::new();

        let result = sync_rate(&rate, &rs, 48000, &flags);

        assert_eq!(result, 96000);
    }

    #[test]
    fn nam_rate_mismatch_with_zero_detected_pw_rate_sets_flags() {
        let rate = AtomicU32::new(0);
        let rs = make_resampler(48000, 44100);
        let flags = RtStatusFlags::new();

        let result = sync_rate(&rate, &rs, 48000, &flags);

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

        // First detection: PW clock moves to 44100 → generation 1.
        rate.store(44100, Ordering::Release);
        sync_rate(&rate, &rs, 48000, &flags);
        assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 1);

        // The host renegotiates again while the first rebuild is still in
        // flight: 96000 → generation 2. The request must not be erased.
        rate.store(96000, Ordering::Release);
        sync_rate(&rate, &rs, 48000, &flags);
        assert_eq!(flags.requested_rate_generation.load(Ordering::Relaxed), 2);
        assert_eq!(flags.requested_host_rate.load(Ordering::Relaxed), 96000);
        assert!(flags.check_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD));
        assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    }
}
