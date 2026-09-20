// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cab-sim Convolution Engine Draining (Zero-Alloc Swap) — engine `RtSwapDrain`
//! integration. Replaces the active stereo-decoupled convolution pair without
//! using memory allocation in the critical path.
//!
//! T9.5 (F-PERF-20): the hand-rolled 3-phase protocol was migrated to the
//! engine's canonical scheduler ([`RtSwapDrain`] + [`RtSwapHandler`]) for this
//! dedicated `Consumer<Box<CabSimSwapPayload>>` channel — a direct
//! instantiation, not a reimplementation. Only the cold handlers
//! (`install_cabsim`/`discard_cabsim`) remain NAM-Audio-Pipe-specific.
//!
//! Budgeting: at most one structural swap applies per callback
//! (`STRUCTURAL_SWAPS_PER_CALLBACK`, shared across every RT swap drain through
//! the caller's `SwapBudget`); pairs in the coalescing window collapse to the
//! latest one (intermediate pairs discarded to GC) and the excess stays queued
//! / parked in the drain's deferred slot for the next callback. Tuning
//! preserved: [`STRUCTURAL_POPS_PER_CALLBACK`] pops per callback.

use neural_amp_modeler_rs::common::spsc::{
    CabSimSwapPayload, GcItem, GcOverflowBuffer, GcSink, RtStatusFlags, RtSwapDrain, RtSwapHandler,
    SwapBudget,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// Dedicated cab-sim swap ring drained by [`RtSwapDrain`].
pub type CabSimSwapRing = Consumer<Box<CabSimSwapPayload>>;
/// Engine drain owning the cab-sim ring and the deferred slot.
pub type CabSimSwapDrain = RtSwapDrain<CabSimSwapRing>;

/// [`RtSwapHandler`] binding the engine scheduler to the cab-sim family.
///
/// All payloads are structural (each envelope swaps the active pair under the
/// shared budget). Latest-wins identity is the build generation; the staleness
/// guard compares it with `requested_cabsim_generation` — a stale payload
/// (from a superseded rebuild) cascades to GC without modifying
/// `active_cabsim` or `applied_cabsim_generation`.
pub(crate) struct CabSimSwapHandler<'a> {
    active_cabsim: &'a mut Option<Box<CabSimPair>>,
    rt_status: &'a RtStatusFlags,
}

impl RtSwapHandler for CabSimSwapHandler<'_> {
    type Payload = CabSimSwapPayload;

    #[inline]
    fn is_structural(&self, _payload: &Self::Payload) -> bool {
        true
    }

    #[inline]
    fn coalesce_key(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    #[inline]
    fn current_generation(&self) -> Option<u64> {
        Some(
            self.rt_status
                .requested_cabsim_generation
                .load(Ordering::Acquire),
        )
    }

    #[inline]
    fn generation_of(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    #[cold]
    fn install(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        install_cabsim(payload, self.active_cabsim, gc, self.rt_status);
    }

    #[cold]
    fn discard(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        discard_cabsim(payload, gc);
    }
}

/// Drains the cab-sim pair SPSC channel and swaps the active pair atomically
/// (engine canonical 3-phase protocol).
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_cabsims(
    drain: &mut CabSimSwapDrain,
    budget: &mut SwapBudget,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    rt_status_for_process: &RtStatusFlags,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
) {
    let mut gc = GcSink {
        producer: gc_producer,
        parking_lot,
        overflow: gc_overflow_for_process,
        rt_status: rt_status_for_process,
        parking_lot_dirty: Some(parking_lot_dirty),
    };
    let mut handler = CabSimSwapHandler {
        active_cabsim,
        rt_status: rt_status_for_process,
    };
    drain.drain(&mut handler, budget, &mut gc);
}

/// Builds the drain for a fresh (re)connection, preserving the historical
/// tuning (1 structural swap/callback shared budget, 8-pop window).
pub fn cabsim_swap_drain(ring: CabSimSwapRing) -> CabSimSwapDrain {
    RtSwapDrain::new(ring, super::commands::structural_swap_tunables())
}

/// Installs a cab-sim command atomically: the active pair (or bypass) is
/// swapped into the payload envelope, the applied generation counter is updated,
/// and the retired envelope cascades to GC as a single moved `Box`.
#[cold]
fn install_cabsim(
    mut payload: Box<CabSimSwapPayload>,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    gc: &mut GcSink<'_>,
    rt_status_for_process: &RtStatusFlags,
) {
    std::mem::swap(&mut payload.pair, active_cabsim);
    rt_status_for_process
        .applied_cabsim_generation
        .store(payload.generation, Ordering::Release);

    gc.retire(GcItem::CabSimSwap(payload));
}

/// Discards an obsolete cab-sim command to the GC cascade as a single moved `Box`.
#[cold]
fn discard_cabsim(payload: Box<CabSimSwapPayload>, gc: &mut GcSink<'_>) {
    gc.retire(GcItem::CabSimSwap(payload));
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
