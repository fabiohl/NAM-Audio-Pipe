// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RATE SYNCHRONIZATION (Clock Tracking & Idempotent Resampler Rebuilds)
//!
//! Checks for sample rate discrepancies between the PipeWire host clock and the active
//! resampler / neural model, managing idempotent rebuild requests and failure rollbacks.
//!
//! # Memory Ordering & Happens-Before Protocol
//!
//! 1. **Detection (PipeWire -> RT Callback):**
//!    PipeWire's `param_changed` handler stores the negotiated sample rate into
//!    `rate_for_process` with `Ordering::Release`. The RT callback consumes it with
//!    `rate_for_process.swap(0, Ordering::Acquire)`. If non-zero, this updates
//!    `sync_state.desired_host_rate` locally. When subsequent callbacks observe 0 from
//!    the swap, `sync_state.desired_host_rate` preserves the target host rate, preventing
//!    accidental overwrites with the stale active rate.
//!
//! 2. **Publishing (RT Callback -> Main Thread):**
//!    When a new rate tuple `(desired_host_rate, current_nam_rate)` is requested:
//!    - Target rates are written to `requested_host_rate` and `requested_nam_rate` with `Relaxed`.
//!    - Generation is incremented with `requested_rate_generation.fetch_add(1, Ordering::Release)`.
//!    - `RT_STATUS_NEEDS_RESAMPLER_REBUILD` is armed with `set_flag_release`.
//!
//!    The main thread reads the flag with `check_flag_acquire`, loads `requested_rate_generation`
//!    with `Acquire`, and subsequently loads target rates, ensuring it observes the exact tuple
//!    associated with that generation snapshot.
//!
//! 3. **Delivery & Drain (Main Thread -> RT Callback):**
//!    - On build success: the main thread enqueues the `ResamplerSwapPayload` carrying `generation`.
//!      `drain_resamplers` compares `payload.generation` against `requested_rate_generation.load(Acquire)`.
//!      If equal, the payload is installed, `RT_STATUS_RESAMP_SWAP_PENDING` is cleared, and retired
//!      resamplers cascade to GC. If smaller, the payload is discarded to GC as stale.
//!    - On build failure: the main thread stores `generation` into `resampler_failed_generation`
//!      with `Ordering::Release`. In step 6 of prologue, the RT callback loads it with `Acquire`.
//!      If `failed_gen == requested_gen`, the fail-open rollback executes: unmuting to the
//!      fallback resampler, recording `sync_state.record_failure()`, and preventing retry storms.
//!      If `failed_gen < requested_gen`, the stale failure from an obsolete request is discarded.
//!
//! 4. **Retorno à Taxa Vigente (Return to Active Rate):**
//!    If the host or model reverts to the rate tuple of the active resampler while a rebuild is
//!    in flight, `sync_rate` bumps `requested_rate_generation` (invalidating the in-flight build)
//!    and immediately clears `RT_STATUS_RESAMP_SWAP_PENDING` and `RT_STATUS_NEEDS_RESAMPLER_REBUILD`,
//!    unmuting audio on the valid active resampler without waiting for an obsolete replacement.

use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;

use std::sync::atomic::Ordering;

/// Tracks local rate synchronization state across audio callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSyncState {
    /// Target host rate preserved across callbacks where atomic swap returns 0.
    pub desired_host_rate: u32,
    /// The (host_rate, nam_rate) tuple of the last published rebuild request.
    pub last_requested_rates: (u32, u32),
    /// Rate tuple that failed rebuild, to prevent infinite retry storms.
    pub failed_rates: (u32, u32),
    /// Number of failed build attempts recorded for `failed_rates`.
    pub failed_retry_count: u32,
}

impl RateSyncState {
    /// Creates a fresh rate sync state initialized with the current host rate.
    pub const fn new(host_rate: u32) -> Self {
        Self {
            desired_host_rate: host_rate,
            last_requested_rates: (0, 0),
            failed_rates: (0, 0),
            failed_retry_count: 0,
        }
    }

    /// Records that the pending rebuild request failed.
    pub fn record_failure(&mut self) {
        self.failed_rates = self.last_requested_rates;
        self.failed_retry_count = self.failed_retry_count.saturating_add(1);
    }
}

/// RATE SYNCHRONIZATION (Clock Tracking)
///
/// Checks for frequency discrepancy between host and model rates, issuing rebuild
/// requests to the main thread idempotently without redundant generation advances.
#[inline(always)]
pub fn sync_rate(
    rate_for_process: &std::sync::atomic::AtomicU32,
    resampler: &NamResampler,
    current_nam_rate: u32,
    rt_status_for_process: &RtStatusFlags,
    sync_state: &mut RateSyncState,
) -> u32 {
    let detected_host_rate = rate_for_process.swap(0, Ordering::Acquire); // Pairs with Release store in param_changed_handler
    let current_host_rate = resampler.host_rate();
    let current_nam_resamp_rate = resampler.nam_rate();

    if detected_host_rate != 0 {
        sync_state.desired_host_rate = detected_host_rate;
    } else if sync_state.desired_host_rate == 0 {
        sync_state.desired_host_rate = current_host_rate;
    }

    let host_rate_to_request = sync_state.desired_host_rate;
    let desired_tuple = (host_rate_to_request, current_nam_rate);
    let active_tuple = (current_host_rate, current_nam_resamp_rate);

    let is_swap_pending = rt_status_for_process
        .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);

    // Case 1: Desired rate matches the active resampler.
    if desired_tuple == active_tuple {
        if is_swap_pending
            && sync_state.last_requested_rates != (0, 0)
            && sync_state.last_requested_rates != active_tuple
            && rt_status_for_process
                .resampler_failed_generation
                .load(Ordering::Relaxed)
                == 0
        {
            // Retorno à taxa vigente:
            // A rebuild was in flight for a different rate (sync_state.last_requested_rates),
            // but the host/model has now returned to the active resampler's rate.
            // Invalidate the in-flight rebuild so that when its envelope arrives,
            // it will be recognized as stale (generation < current) and discarded to GC.
            let new_gen = rt_status_for_process
                .requested_rate_generation
                .fetch_add(1, Ordering::Release)
                + 1;
            rt_status_for_process
                .applied_rate_generation
                .store(new_gen, Ordering::Release);
            rt_status_for_process
                .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);
            rt_status_for_process
                .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
            sync_state.last_requested_rates = (0, 0);
        }
        if sync_state.failed_rates == desired_tuple {
            sync_state.failed_rates = (0, 0);
            sync_state.failed_retry_count = 0;
        }
        return current_host_rate;
    }

    // Case 2: Desired rate differs from active resampler.
    // Check if this exact desired tuple has already failed and exhausted retries.
    if sync_state.failed_rates == desired_tuple && sync_state.failed_retry_count >= 1 {
        // Limited failure: do not enter an infinite retry storm.
        // Maintain fallback audio on the active resampler.
        return current_host_rate;
    }

    // Check if a rebuild for this exact desired tuple is ALREADY in flight.
    if is_swap_pending && sync_state.last_requested_rates == desired_tuple {
        // Idempotent: the request for `desired_tuple` was already published and is
        // in flight. Do NOT bump generation or re-arm flags.
        return current_host_rate;
    }

    // Genuinely new request or legitimate supersession of an in-flight request.
    if host_rate_to_request != 0 && current_nam_rate != 0 {
        sync_state.last_requested_rates = desired_tuple;
        if sync_state.failed_rates != desired_tuple {
            sync_state.failed_rates = (0, 0);
            sync_state.failed_retry_count = 0;
        }

        // Rate renegotiation synchronization: publish the requested rates
        // FIRST, then increment the generation with Release. The main thread
        // captures the generation with Acquire, which orders these rate stores
        // into its build snapshot; a renegotiation arriving during a rebuild
        // bumps the generation again, so a stale envelope can be detected by
        // the RT drain and discarded without unmuting.
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
#[path = "rate_sync_test.rs"]
mod rate_sync_test;
