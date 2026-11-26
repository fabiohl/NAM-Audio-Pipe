// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Canonical real-time quantum prologue shared by the PipeWire capture callback
//! and the offline swap harness.

use crate::standalone::pw_host::capture::state::{CaptureState, RtHostChannels};
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcSink, RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_NEEDS_RESAMPLER_REBUILD,
    RT_STATUS_RESAMP_SWAP_PENDING, RtStatusFlags, SwapBudget,
};
use neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT;

use super::{
    STRUCTURAL_SWAPS_PER_CALLBACK, drain_cabsims_with_slot_status, drain_os_engines_with_status,
    drain_resamplers_with_status, drain_slimmable_models, receive_commands, sync_rate,
    try_slimmable_rebuild,
};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Verdict of running the real-time quantum prologue.
///
/// Distinguishes whether the quantum should proceed with audio buffer dequeue and DSP processing,
/// or whether DSP execution must be skipped (e.g. while an off-RT resampler swap is in flight,
/// or following an FFI contract violation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrologueOutcome {
    /// Prologue completed successfully; DSP processing may proceed for this quantum.
    Proceed {
        /// Active host sample rate synchronized for this quantum.
        current_host_rate: u32,
        /// Number of structural swaps applied in this quantum (budgeted).
        structural_applied: usize,
        /// Number of scalar parameter payloads popped from the param SPSC this quantum.
        param_pops: usize,
    },
    /// The quantum must be skipped: either an off-RT resampler swap is pending,
    /// or a drain contract violation occurred. The caller must skip DSP processing
    /// and deliver deterministic silence / dequeue without processing.
    Skip {
        /// Active host sample rate at the time of skip (or 0 if contract violation).
        current_host_rate: u32,
        /// Number of structural swaps applied in this quantum (if any).
        structural_applied: usize,
        /// Number of scalar parameter payloads popped from the param SPSC this quantum (if any).
        param_pops: usize,
    },
}

#[cfg(feature = "testing")]
impl PrologueOutcome {
    /// Returns the synchronized host rate.
    #[inline(always)]
    pub fn current_host_rate(&self) -> u32 {
        match *self {
            PrologueOutcome::Proceed {
                current_host_rate, ..
            }
            | PrologueOutcome::Skip {
                current_host_rate, ..
            } => current_host_rate,
        }
    }

    /// Returns the count of structural swaps applied during this quantum.
    #[inline(always)]
    pub fn structural_applied(&self) -> usize {
        match *self {
            PrologueOutcome::Proceed {
                structural_applied, ..
            }
            | PrologueOutcome::Skip {
                structural_applied, ..
            } => structural_applied,
        }
    }

    /// Returns the count of scalar parameter payloads popped during this quantum.
    #[inline(always)]
    pub fn param_pops(&self) -> usize {
        match *self {
            PrologueOutcome::Proceed { param_pops, .. }
            | PrologueOutcome::Skip { param_pops, .. } => param_pops,
        }
    }
}

/// Unified real-time callback prologue shared verbatim by the production PipeWire capture
/// callback (`capture/setup.rs`) and the offline swap harness (`rt_callback/harness.rs`).
///
/// Executes the exact, canonical sequence instruction-by-instruction with zero heap allocations,
/// zero locks, and zero log calls:
///
/// 1. **GC Parking-Lot Flush**: pushes deferred retired items to the SPSC GC channel, clearing
///    the dirty latch when all 16 slots are drained.
/// 2. **Shared Command Budgeting**: enforces [`STRUCTURAL_SWAPS_PER_CALLBACK`] across all drains
///    via [`SwapBudget`].
/// 3. **Budgeted Drains**:
///    - Resampler swap drain ([`drain_resamplers_with_status`])
///    - Cab-sim swap drain ([`drain_cabsims_with_slot_status`])
///    - Neural cabinet swap drain ([`crate::standalone::cabinet::drain_cab_nam_payloads`])
///    - Param & model swap consumer ([`receive_commands`])
///    - Slimmable model rebuild trigger ([`try_slimmable_rebuild`])
///    - Slimmable model drain ([`drain_slimmable_models`])
///    - Oversampling engines drain ([`drain_os_engines_with_status`])
/// 4. **Rate Synchronization**: reads the target host rate ([`sync_rate`]) using `state.shared_target_rate`.
/// 5. **Gain Multiplier Recomputation**: recomputes input/output gain multipliers when parameters changed.
/// 6. **Rollback Guard**: inspects `RT_STATUS_RESAMP_SWAP_PENDING`; if a rebuild failed, performs the
///    fail-open rollback to previous resampler generation; if a rebuild is in flight, returns [`PrologueOutcome::Skip`].
#[inline(always)]
pub fn rt_quantum_prologue(
    state: &mut CaptureState,
    channels: &mut RtHostChannels,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    rt_status: &Arc<RtStatusFlags>,
    lut: &GainLUT,
) -> PrologueOutcome {
    // 1. Fast-path parking-lot flush: skip the 16-slot scan if no GC item was ever parked since last drain.
    if parking_lot_dirty.load(Ordering::Acquire) {
        let mut any_remaining = false;
        for slot in parking_lot.iter_mut() {
            let Some(old) = slot.take() else { continue };
            if let Err(rtrb::PushError::Full(old_back)) = channels.gc_producer.push(old) {
                // The slot was just emptied by `take()`, so re-parking cannot
                // destroy an existing item. `mem::forget` keeps the polymorphic
                // `drop_glue` for `GcItem` out of the audio callback.
                debug_assert!(slot.is_none(), "parking-lot slot must be empty");
                std::mem::forget(slot.replace(old_back));
                any_remaining = true;
                break;
            }
        }
        if !any_remaining {
            parking_lot_dirty.store(false, Ordering::Release);
        }
    }

    // 2. Command Budgeting: shared structural budget across all drains.
    let mut budget = SwapBudget::new(STRUCTURAL_SWAPS_PER_CALLBACK);

    // 3. Budgeted drains in exact production order.
    let Some(resampler_drain) = state.resampler_drain.as_mut() else {
        core::hint::cold_path();
        rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        return PrologueOutcome::Skip {
            current_host_rate: 0,
            structural_applied: budget.used(),
            param_pops: 0,
        };
    };
    drain_resamplers_with_status(
        resampler_drain,
        &mut budget,
        &mut state.resampler,
        &mut state.stream,
        rt_status,
        state.stream_status.as_deref(),
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
    );

    let Some(cabsim_drain) = state.cabsim_drain.as_mut() else {
        core::hint::cold_path();
        rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        return PrologueOutcome::Skip {
            current_host_rate: 0,
            structural_applied: budget.used(),
            param_pops: 0,
        };
    };
    drain_cabsims_with_slot_status(
        cabsim_drain,
        &mut budget,
        &mut state.active_cabsim,
        &mut state.active_cab_nam,
        Some(&mut state.cabsim_failed_key),
        state.stream_status.as_deref(),
        rt_status,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
    );

    if let Some(nam_cons) = state.cab_nam_consumer.as_mut() {
        let mut gc = GcSink {
            producer: &mut channels.gc_producer,
            parking_lot,
            overflow: &channels.gc_overflow,
            rt_status,
            parking_lot_dirty: Some(parking_lot_dirty),
        };
        let stream_status_ref = state.stream_status.as_deref();
        crate::standalone::cabinet::drain_cab_nam_payloads(
            nam_cons,
            &mut budget,
            &rt_status.requested_cabsim_generation,
            &rt_status.applied_cabsim_generation,
            &mut state.active_cabsim,
            &mut state.active_cab_nam,
            &|lat| {
                if let Some(status) = stream_status_ref {
                    status.update_cabsim_latency(lat);
                }
            },
            Some(&mut state.cabsim_failed_key),
            &mut gc,
        );
    }

    let active_cab_lat = crate::standalone::cabinet::active_cabinet_latency(
        state.active_cabsim.as_deref(),
        state.active_cab_nam.as_ref(),
    ) as usize;
    state.cab_dry_delay.set_delay(active_cab_lat);

    let prev_gate_params = state.gate_params;
    let (param_changed, param_pops) = receive_commands(
        &mut channels.param_consumer,
        &mut state.deferred_model,
        &mut budget,
        &mut state.model_input_mult_adj,
        &mut state.model_output_mult_adj,
        &mut state.current_nam_rate,
        &mut state.active_model_l,
        &mut state.active_model_r,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
        rt_status,
        &mut state.user_input_gain_mult,
        &mut state.user_output_gain_mult,
        &mut state.gate_params,
        &mut state.threshold_open_sq,
        &mut state.threshold_close_sq,
        lut,
        &mut state.adaptive_compute,
    );
    if state.gate_params != prev_gate_params {
        state.apply_gate_resolution(lut);
    }

    try_slimmable_rebuild(&mut state.adaptive_compute, rt_status);

    drain_slimmable_models(
        &mut state.slimmable_drain,
        &mut budget,
        &mut state.active_model_l,
        &mut state.active_model_r,
        rt_status,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
    );

    drain_os_engines_with_status(
        &mut state.os_drain,
        &mut budget,
        &mut state.os_l,
        &mut state.os_r,
        state.stream_status.as_deref(),
        rt_status,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
    );

    // 4. Rate synchronization.
    let current_host_rate = sync_rate(
        state.shared_target_rate.as_ref(),
        &state.resampler,
        state.current_nam_rate,
        rt_status,
        &mut state.rate_sync,
    );

    // 5. Gain recomputation on parameter change.
    if param_changed {
        rt_setup::compute_gain_multipliers(
            state.user_input_gain_mult,
            state.user_output_gain_mult,
            state.model_input_mult_adj,
            state.model_output_mult_adj,
            &mut state.input_gain_mult,
            &mut state.output_gain_mult,
        );
    }

    // 6. Fail-open rollback guard.
    if rt_status.check_flag(RT_STATUS_RESAMP_SWAP_PENDING) {
        let failed_gen = rt_status
            .resampler_failed_generation
            .load(Ordering::Acquire);
        let requested_gen = rt_status.requested_rate_generation.load(Ordering::Acquire);

        if failed_gen != 0 {
            if failed_gen == requested_gen {
                // Fail-open rollback: the rebuild failed for requested_gen,
                // so the callback resumes with the previous resampler in safe
                // bypass/mute mode. Record the requested generation as
                // resolved so the invariant
                // `applied_rate_generation == requested_rate_generation`
                // holds on unmute — the old resampler is the accepted
                // fallback for this request, never a stale replacement.
                rt_status
                    .applied_rate_generation
                    .store(requested_gen, Ordering::Release);
                rt_status.clear_flag(RT_STATUS_RESAMP_SWAP_PENDING);
                rt_status.clear_flag(RT_STATUS_NEEDS_RESAMPLER_REBUILD);
                rt_status
                    .resampler_failed_generation
                    .store(0, Ordering::Release);
                state.rate_sync.record_failure();
            } else if failed_gen < requested_gen {
                // Stale failure from an obsolete request that was superseded:
                // clear the stale marker and keep waiting for requested_gen.
                rt_status
                    .resampler_failed_generation
                    .store(0, Ordering::Release);
                return PrologueOutcome::Skip {
                    current_host_rate,
                    structural_applied: budget.used(),
                    param_pops,
                };
            }
        } else {
            return PrologueOutcome::Skip {
                current_host_rate,
                structural_applied: budget.used(),
                param_pops,
            };
        }
    }

    PrologueOutcome::Proceed {
        current_host_rate,
        structural_applied: budget.used(),
        param_pops,
    }
}
