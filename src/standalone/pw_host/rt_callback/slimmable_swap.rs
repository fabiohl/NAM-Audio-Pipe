// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Lock-free SPSC slimmable model swap handler and drain for the RT audio callback.

use super::commands::structural_swap_tunables;
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, GcSink, RtStatusFlags, RtSwapDrain, RtSwapHandler, SlimModelPair,
    SwapBudget,
};
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::models::StaticModel;
use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// Signals the main thread via `RtStatusFlags` when `AdaptiveCompute` requests a
/// slimmable channel change. Zero allocations, zero locks.
///
/// Generation-tracked: increments `requested_slimmable_generation` (`Release`)
/// before raising the request flag so the main thread can detect superseding
/// and the RT drain can discard stale in-flight pairs.
#[inline(always)]
pub fn try_slimmable_rebuild(adaptive: &mut AdaptiveCompute, rt_status: &RtStatusFlags) {
    let Some(targets) = adaptive.take_slimmable_rebuild() else {
        return;
    };
    let target_ch = targets.first().copied().unwrap_or(0);
    rt_status
        .requested_slimmable_ch
        .store(target_ch as u32, Ordering::Relaxed);
    rt_status
        .requested_slimmable_generation
        .fetch_add(1, Ordering::Release);
    rt_status
        .set_flag_release(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
}

/// Dedicated slimmable swap ring drained by [`RtSwapDrain`].
pub type SlimmableSwapRing = Consumer<Box<SlimModelPair>>;
/// Engine drain owning the slimmable ring and the deferred slot.
pub type SlimmableSwapDrain = RtSwapDrain<SlimmableSwapRing>;

/// [`RtSwapHandler`] binding the engine scheduler to the slimmable family.
///
/// All payloads are structural (each pair swaps both active model channels in
/// one all-or-nothing transaction under the shared budget). Latest-wins
/// identity is the rebuild generation; the staleness guard compares it with
/// `requested_slimmable_generation` — a stale pair (built for an older rebuild
/// generation) is discarded to the GC cascade without touching the active
/// models, so L/R can never belong to different generations or channel counts.
pub(crate) struct SlimmableSwapHandler<'a> {
    active_model_l: &'a mut Option<Box<StaticModel>>,
    active_model_r: &'a mut Option<Box<StaticModel>>,
    rt_status: &'a RtStatusFlags,
}

impl RtSwapHandler for SlimmableSwapHandler<'_> {
    type Payload = SlimModelPair;

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
                .requested_slimmable_generation
                .load(Ordering::Acquire),
        )
    }

    #[inline]
    fn generation_of(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    #[cold]
    fn install(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        install_pair(payload, self.active_model_l, self.active_model_r, gc);
    }

    #[cold]
    fn discard(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        discard_pair_whole(payload, gc);
    }
}

/// Drains slimmable-rebuilt model pairs delivered by the main thread via SPSC
/// (engine canonical 3-phase protocol).
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_slimmable_models(
    drain: &mut Option<SlimmableSwapDrain>,
    budget: &mut SwapBudget,
    active_model_l: &mut Option<Box<StaticModel>>,
    active_model_r: &mut Option<Box<StaticModel>>,
    rt_status: &RtStatusFlags,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
) {
    let Some(drain) = drain.as_mut() else {
        return;
    };
    let mut gc = GcSink {
        producer: gc_producer,
        parking_lot,
        overflow: gc_overflow,
        rt_status,
        parking_lot_dirty: Some(parking_lot_dirty),
    };
    let mut handler = SlimmableSwapHandler {
        active_model_l,
        active_model_r,
        rt_status,
    };
    drain.drain(&mut handler, budget, &mut gc);
}

/// Builds the drain for a fresh (re)connection, preserving the historical
/// tuning (1 structural swap/callback shared budget, 8-pop window).
pub fn slimmable_swap_drain(ring: SlimmableSwapRing) -> SlimmableSwapDrain {
    RtSwapDrain::new(ring, structural_swap_tunables())
}

/// Atomically swaps both active model channels from a pair: the previous L and
/// R models (if any) are swapped into the envelope and cascade to GC as a single
/// moved `Box<SlimModelPair>`.
#[cold]
fn install_pair(
    mut pair: Box<SlimModelPair>,
    active_model_l: &mut Option<Box<StaticModel>>,
    active_model_r: &mut Option<Box<StaticModel>>,
    gc: &mut GcSink<'_>,
) {
    std::mem::swap(&mut pair.l, active_model_l);
    if pair.r.is_some() {
        std::mem::swap(&mut pair.r, active_model_r);
    }
    gc.retire(GcItem::SlimModelPair(pair));
}

/// Discards a whole pair to the GC cascade as a single moved `Box<SlimModelPair>`
/// — never applied.
#[cold]
fn discard_pair_whole(pair: Box<SlimModelPair>, gc: &mut GcSink<'_>) {
    gc.retire(GcItem::SlimModelPair(pair));
}
