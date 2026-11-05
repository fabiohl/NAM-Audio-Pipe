// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cab-sim Convolution Engine Draining (Zero-Alloc Swap) — engine `RtSwapDrain`
//! integration. Replaces the active stereo-decoupled convolution pair without
//! using memory allocation in the critical path.
//!
//! The 3-phase swap protocol uses the
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

use crate::standalone::pw_host::StreamStatusFlags;
use neural_amp_modeler_rs::common::spsc::{
    CabSimSwapPayload, GcItem, GcOverflowBuffer, GcSink, RtStatusFlags, RtSwapDrain, RtSwapHandler,
    SwapBudget,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

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
    failed_key: Option<&'a mut Option<(u32, usize)>>,
    applied_latency: Option<&'a AtomicU32>,
    applied_latency_pair: Option<&'a AtomicU64>,
    stream_status: Option<&'a StreamStatusFlags>,
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
        if payload.pair.is_some() {
            if let Some(failed) = self.failed_key.as_deref_mut() {
                *failed = None;
            }
        } else {
            let rate = self
                .rt_status
                .requested_cabsim_host_rate
                .load(Ordering::Relaxed);
            let part = self
                .rt_status
                .requested_cabsim_partition_size
                .load(Ordering::Relaxed) as usize;
            if let Some(failed) = self.failed_key.as_deref_mut() {
                *failed = Some((rate, part));
            }
        }
        install_cabsim(
            payload,
            self.active_cabsim,
            gc,
            self.rt_status,
            self.applied_latency,
            self.applied_latency_pair,
            self.stream_status,
        );
    }

    #[cold]
    fn discard(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        discard_cabsim(payload, gc);
    }
}

/// Drains the cab-sim pair SPSC channel and swaps the active pair atomically with coherent pair support.
#[inline(always)]
#[cfg(test)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_cabsims_with_pair(
    drain: &mut CabSimSwapDrain,
    budget: &mut SwapBudget,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    failed_key: Option<&mut Option<(u32, usize)>>,
    applied_latency: Option<&AtomicU32>,
    applied_latency_pair: Option<&AtomicU64>,
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
        failed_key,
        applied_latency,
        applied_latency_pair,
        stream_status: None,
        rt_status: rt_status_for_process,
    };
    drain.drain(&mut handler, budget, &mut gc);
}

/// Drains the cab-sim pair SPSC channel and swaps the active pair atomically with complete `StreamStatusFlags` seqlock snapshot support.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_cabsims_with_status(
    drain: &mut CabSimSwapDrain,
    budget: &mut SwapBudget,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    failed_key: Option<&mut Option<(u32, usize)>>,
    stream_status: Option<&StreamStatusFlags>,
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
        failed_key,
        applied_latency: stream_status.map(|s| &s.applied_cabsim_latency),
        applied_latency_pair: stream_status.map(|s| &s.applied_latency_pair),
        stream_status,
        rt_status: rt_status_for_process,
    };
    drain.drain(&mut handler, budget, &mut gc);
}

/// Drains the cab-sim pair SPSC channel and swaps the active pair atomically
/// (engine canonical 3-phase protocol).
#[inline(always)]
#[cfg(test)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_cabsims(
    drain: &mut CabSimSwapDrain,
    budget: &mut SwapBudget,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    failed_key: Option<&mut Option<(u32, usize)>>,
    applied_latency: Option<&AtomicU32>,
    rt_status_for_process: &RtStatusFlags,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
) {
    drain_cabsims_with_pair(
        drain,
        budget,
        active_cabsim,
        failed_key,
        applied_latency,
        None,
        rt_status_for_process,
        gc_producer,
        parking_lot,
        parking_lot_dirty,
        gc_overflow_for_process,
    );
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
    applied_latency: Option<&AtomicU32>,
    applied_latency_pair: Option<&AtomicU64>,
    stream_status: Option<&StreamStatusFlags>,
) {
    std::mem::swap(&mut payload.pair, active_cabsim);
    let lat = active_cabsim
        .as_ref()
        .map(|p| p.l.latency_samples() as u32)
        .unwrap_or(0);
    if let Some(status) = stream_status {
        status.update_cabsim_latency(lat);
    } else {
        if let Some(applied) = applied_latency {
            applied.store(lat, Ordering::Release);
        }
        if let Some(pair) = applied_latency_pair {
            let mut cur = pair.load(Ordering::Relaxed);
            loop {
                let (res, _) = StreamStatusFlags::unpack_latency_pair(cur);
                let next = StreamStatusFlags::pack_latency_pair(res, lat);
                match pair.compare_exchange_weak(cur, next, Ordering::Release, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(actual) => cur = actual,
                }
            }
        }
    }

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
