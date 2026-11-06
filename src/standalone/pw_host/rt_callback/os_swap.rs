// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Lock-free SPSC oversampling engine swap handler and drain for the RT audio callback.

use super::commands::structural_swap_tunables;
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, GcSink, RtStatusFlags, RtSwapDrain, RtSwapHandler, SwapBudget,
};
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine};
use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// Dedicated oversample swap ring drained by [`RtSwapDrain`].
pub type OsSwapRing = Consumer<Box<OsEnginePair>>;
/// Engine drain owning the oversample ring and the deferred slot.
pub type OsSwapDrain = RtSwapDrain<OsSwapRing>;

/// [`RtSwapHandler`] binding the engine scheduler to the oversample family.
///
/// All payloads are structural (each pair swaps both L and R engines under the
/// shared budget). Latest-wins identity is the rebuild generation; the
/// staleness guard compares it with `requested_os_generation` — a stale pair
/// (a newer oversample change superseded it before delivery) cascades to GC
/// without touching the active engines.
pub(crate) struct OsSwapHandler<'a> {
    os_l: &'a mut Box<OversampleEngine>,
    os_r: &'a mut Box<OversampleEngine>,
    stream_status: Option<&'a crate::standalone::pw_host::StreamStatusFlags>,
    rt_status: &'a RtStatusFlags,
}

impl RtSwapHandler for OsSwapHandler<'_> {
    type Payload = OsEnginePair;

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
                .requested_os_generation
                .load(Ordering::Acquire),
        )
    }

    #[inline]
    fn generation_of(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    #[cold]
    fn install(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        install_os_pair(
            payload,
            self.os_l,
            self.os_r,
            self.stream_status,
            gc,
            self.rt_status,
        );
    }

    #[cold]
    fn discard(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        discard_os_pair(payload, gc);
    }
}

/// Drains oversampling engines delivered by the main thread via SPSC
/// (engine canonical 3-phase protocol): swaps both L and R engines and sends
/// the obsolete envelope to the GC cascade.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_os_engines_with_status(
    drain: &mut Option<OsSwapDrain>,
    budget: &mut SwapBudget,
    os_l: &mut Box<OversampleEngine>,
    os_r: &mut Box<OversampleEngine>,
    stream_status: Option<&crate::standalone::pw_host::StreamStatusFlags>,
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
    let mut handler = OsSwapHandler {
        os_l,
        os_r,
        stream_status,
        rt_status,
    };
    drain.drain(&mut handler, budget, &mut gc);
}

/// Simple test-convenience wrapper for [`drain_os_engines_with_status`] without stream status flags.
#[cfg(test)]
#[inline]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_os_engines(
    drain: &mut Option<OsSwapDrain>,
    budget: &mut SwapBudget,
    os_l: &mut Box<OversampleEngine>,
    os_r: &mut Box<OversampleEngine>,
    rt_status: &RtStatusFlags,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
) {
    drain_os_engines_with_status(
        drain,
        budget,
        os_l,
        os_r,
        None,
        rt_status,
        gc_producer,
        parking_lot,
        parking_lot_dirty,
        gc_overflow,
    );
}

/// Builds the drain for a fresh (re)connection, preserving the historical
/// tuning (1 structural swap/callback shared budget, 8-pop window).
pub fn os_swap_drain(ring: OsSwapRing) -> OsSwapDrain {
    RtSwapDrain::new(ring, structural_swap_tunables())
}

/// Swaps both active OS engines into the envelope and cascades the replaced pair to GC.
#[cold]
fn install_os_pair(
    mut pair: Box<OsEnginePair>,
    os_l: &mut Box<OversampleEngine>,
    os_r: &mut Box<OversampleEngine>,
    stream_status: Option<&crate::standalone::pw_host::StreamStatusFlags>,
    gc: &mut GcSink<'_>,
    rt_status: &RtStatusFlags,
) {
    // Applied generation is recorded BEFORE the engines swap (Release) so the
    // main thread never observes a half-applied generation.
    rt_status
        .applied_os_generation
        .store(pair.generation, Ordering::Release);
    std::mem::swap(&mut pair.l, os_l);
    std::mem::swap(&mut pair.r, os_r);
    let os_lat = os_l.latency_samples() as u32;
    if let Some(status) = stream_status {
        status.update_oversample_latency(os_lat);
    }
    gc.retire(GcItem::OsEnginePair(pair));
}

/// Discards an obsolete OS engine pair to the GC cascade as a single moved `Box<OsEnginePair>`.
#[cold]
fn discard_os_pair(pair: Box<OsEnginePair>, gc: &mut GcSink<'_>) {
    gc.retire(GcItem::OsEnginePair(pair));
}
