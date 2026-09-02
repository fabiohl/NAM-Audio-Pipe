// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cab-sim Convolution Engine Draining (Zero-Alloc Swap)
//! Replaces the active stereo-decoupled convolution pair without using memory
//! allocation in the critical path.
//!
//! Budgeting (F-RB-011 / T2.5): at most [`STRUCTURAL_SWAPS_PER_CALLBACK`]
//! structural swap applies per callback (budget shared across every RT swap
//! drain); pairs in the coalescing window collapse to the latest one
//! (intermediate pairs discarded to GC) and the excess is parked in the
//! deferred slot for the next callback.

use super::commands::{STRUCTURAL_POPS_PER_CALLBACK, STRUCTURAL_SWAPS_PER_CALLBACK};
use neural_amp_modeler_rs::common::spsc::{
    CabSimSwapPayload, GcItem, GcOverflowBuffer, RT_STATUS_STRUCTURAL_DEFERRED,
    RT_STATUS_STRUCTURAL_SUPERSEDED, RtStatusFlags, gc_cascade,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// Drains the cab-sim pair SPSC channel and swaps the active pair atomically.
///
/// Follows the same cascade pattern as `drain_resamplers`:
/// GC channel → parking_lot → overflow buffer.
///
/// Envelopes (`CabSimSwapPayload`) carry a generation timestamp. A payload is
/// installed only if its generation matches `requested_cabsim_generation`; stale
/// payloads (from superseded rebuilds) cascade to GC without modifying `active_cabsim`
/// or `applied_cabsim_generation`.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
pub fn drain_cabsims(
    cabsim_consumer: &mut Consumer<Box<CabSimSwapPayload>>,
    deferred: &mut Option<Box<CabSimSwapPayload>>,
    structural_applied: &mut usize,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    let current_req_gen = rt_status_for_process
        .requested_cabsim_generation
        .load(Ordering::Acquire);

    // Phase 0 — resolve a payload deferred by the previous callback.
    if let Some(pending) = deferred.take() {
        if pending.generation != current_req_gen {
            // Stale deferred payload: generation changed while parked.
            discard_cabsim(
                pending,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
        } else {
            let head_queued = cabsim_consumer.peek().is_ok();
            if head_queued {
                // A newer command is already queued (latest-wins): the deferred
                // payload is obsolete and cascades to GC.
                discard_cabsim(
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
                install_cabsim(
                    pending,
                    active_cabsim,
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
    }

    // Phase 1 — bounded drain with coalescing (F-RB-011 / T2.5).
    let mut candidate: Option<Box<CabSimSwapPayload>> = None;
    let mut pops = 0usize;
    while pops < STRUCTURAL_POPS_PER_CALLBACK {
        let Some(payload) = cabsim_consumer.pop().ok() else {
            break;
        };
        pops += 1;
        if payload.generation != current_req_gen {
            // Stale envelope: generation changed while payload was in transit.
            discard_cabsim(
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
            // Coalescing: an intermediate command is obsolete — its payload
            // cascades to GC (latest-wins).
            discard_cabsim(
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
    if let Some(new_payload) = candidate {
        if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_cabsim(
                new_payload,
                active_cabsim,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            *structural_applied += 1;
        } else if deferred.is_none() {
            *deferred = Some(new_payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // The deferred slot holds an older command; the popped one is newer
            // — supersede the parked command and park this one (latest-wins).
            let older = deferred.take().expect("slot occupied, checked above");
            discard_cabsim(
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
            *deferred = Some(new_payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Installs a cab-sim command atomically: the active pair (or bypass) is
/// swapped into the payload envelope, the applied generation counter is updated,
/// and the retired envelope cascades to GC as a single moved `Box` (F-RB-007).
#[inline(always)]
fn install_cabsim(
    mut payload: Box<CabSimSwapPayload>,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    std::mem::swap(&mut payload.pair, active_cabsim);
    rt_status_for_process
        .applied_cabsim_generation
        .store(payload.generation, Ordering::Release);

    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::CabSimSwap(payload)),
        gc_producer,
        parking_lot,
        gc_overflow_for_process,
        rt_status_for_process,
    );
}

/// Discards an obsolete cab-sim command to the GC cascade as a single moved `Box` (F-RB-007).
#[inline(always)]
fn discard_cabsim(
    payload: Box<CabSimSwapPayload>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::CabSimSwap(payload)),
        gc_producer,
        parking_lot,
        gc_overflow_for_process,
        rt_status_for_process,
    );
}

#[cfg(test)]
fn make_pair(ir: &[f32], partition: usize, rate: u32) -> CabSimPair {
    use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
    use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
    let make_adapter = || {
        let engine = ConvEngine::new(ir, partition).unwrap();
        CabSimAdapter::new(Box::new(engine)).unwrap()
    };
    CabSimPair {
        l: Box::new(make_adapter()),
        r: Box::new(make_adapter()),
        sample_rate: rate,
    }
}

#[cfg(test)]
#[path = "cabsim_swap_test.rs"]
mod cabsim_swap_test;
