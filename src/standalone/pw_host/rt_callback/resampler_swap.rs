// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.1. Resampler Draining (Zero-Alloc Swap)
//! Replaces resamplers without using memory allocation in the critical path.
//!
//! Budgeting (F-RB-011 / T2.5): at most [`STRUCTURAL_SWAPS_PER_CALLBACK`]
//! structural swap applies per callback (budget shared across every RT swap
//! drain); current-generation envelopes in the coalescing window collapse to
//! the latest one (intermediate envelopes discarded to GC) and the excess is
//! parked in the deferred slot for the next callback.

use super::commands::{STRUCTURAL_POPS_PER_CALLBACK, STRUCTURAL_SWAPS_PER_CALLBACK};
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED,
    ResamplerSwapPayload, RtStatusFlags, gc_cascade,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// 5.1.1. Resampler Draining (Zero-Alloc Swap)
/// Replaces resamplers without using memory allocation in the critical path.
///
/// Versioned delivery (F-RB-004): each envelope carries the request generation
/// it was built for. An envelope whose generation still equals
/// `requested_rate_generation` is current and is installed (the previous
/// resampler cascades to GC). An envelope whose generation is older has been
/// superseded by a newer host renegotiation — it goes straight to the GC
/// cascade **without** unmuting and **without** clearing
/// `RT_STATUS_RESAMP_SWAP_PENDING`, so the callback keeps waiting for the build
/// that matches the most recent request.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
pub fn drain_resamplers(
    resampler_consumer: &mut Consumer<Box<ResamplerSwapPayload>>,
    deferred: &mut Option<Box<ResamplerSwapPayload>>,
    structural_applied: &mut usize,
    resampler: &mut Box<NamResampler>,
    stream: &mut Box<StreamingResampleBuffer>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    // Phase 0 — resolve a resampler deferred by the previous callback.
    if let Some(pending) = deferred.take() {
        let current_req_gen = rt_status_for_process
            .requested_rate_generation
            .load(Ordering::Acquire);
        let head_is_current = resampler_consumer
            .peek()
            .is_ok_and(|head| head.generation == current_req_gen);
        if pending.generation != current_req_gen {
            // Stale while parked (F-RB-004): discard to GC without unmuting and
            // without clearing RESAMP_SWAP_PENDING.
            discard_resampler(
                pending,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
        } else if head_is_current {
            // A newer same-generation build is already queued (latest-wins).
            discard_resampler(
                pending,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status_for_process
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
        } else if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_resampler(
                pending,
                resampler,
                stream,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            *structural_applied += 1;
        } else {
            // Budget exhausted and nothing newer queued: re-park.
            *deferred = Some(pending);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // Phase 1 — bounded drain with coalescing (F-RB-011 / T2.5).
    let current_req_gen = rt_status_for_process
        .requested_rate_generation
        .load(Ordering::Acquire);
    let mut candidate: Option<Box<ResamplerSwapPayload>> = None;
    let mut pops = 0usize;
    while pops < STRUCTURAL_POPS_PER_CALLBACK {
        let Some(payload) = resampler_consumer.pop().ok() else {
            break;
        };
        pops += 1;
        if payload.generation != current_req_gen {
            // Stale envelope (F-RB-004): the host renegotiated the clock while
            // this resampler was being built. Discard it for GC without
            // unmuting and without clearing RESAMP_SWAP_PENDING.
            discard_resampler(
                payload,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            continue;
        }
        if let Some(older) = candidate.replace(payload) {
            // Coalescing: an intermediate current-generation envelope is
            // obsolete — its resampler cascades to GC (latest-wins).
            discard_resampler(
                older,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status_for_process
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(payload) = candidate {
        if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_resampler(
                payload,
                resampler,
                stream,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            *structural_applied += 1;
        } else if deferred.is_none() {
            *deferred = Some(payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // The deferred slot holds an older envelope; the popped one is
            // newer — supersede the parked envelope and park this one
            // (latest-wins).
            let older = deferred.take().expect("slot occupied, checked above");
            discard_resampler(
                older,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status_for_process
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
            *deferred = Some(payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Installs a current-generation resampler envelope: swaps the active
/// resampler and streaming adapter, records the applied generation and active rates,
/// unmutes (clears `RT_STATUS_RESAMP_SWAP_PENDING`) and cascades the retired
/// resampler envelope to GC.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time swap helper receiving active resources, queues, parking lot, and flags"
)]
fn install_resampler(
    mut payload: Box<ResamplerSwapPayload>,
    resampler: &mut Box<NamResampler>,
    stream: &mut Box<StreamingResampleBuffer>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    std::mem::swap(&mut payload.resampler, resampler);
    std::mem::swap(&mut payload.stream, stream);

    rt_status_for_process
        .applied_rate_generation
        .store(payload.generation, Ordering::Release);
    rt_status_for_process
        .active_rate
        .store(resampler.host_rate(), Ordering::Relaxed);
    rt_status_for_process
        .active_rate_changed
        .store(resampler.host_rate(), Ordering::Relaxed);

    rt_status_for_process
        .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);

    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::ResamplerSwap(payload)),
        gc_producer,
        parking_lot,
        gc_overflow_for_process,
        rt_status_for_process,
    );
}

/// Discards a resampler envelope to the GC cascade **without** unmuting and
/// **without** clearing `RT_STATUS_RESAMP_SWAP_PENDING` (stale or superseded
/// builds never substitute the most recent request).
#[inline(always)]
fn discard_resampler(
    payload: Box<ResamplerSwapPayload>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::ResamplerSwap(payload)),
        gc_producer,
        parking_lot,
        gc_overflow_for_process,
        rt_status_for_process,
    );
}

#[cfg(test)]
#[path = "resampler_swap_test.rs"]
mod resampler_swap_test;
