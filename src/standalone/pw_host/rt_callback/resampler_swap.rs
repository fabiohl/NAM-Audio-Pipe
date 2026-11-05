// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Resampler Draining (Zero-Alloc Swap) — engine `RtSwapDrain` integration.
//! Replaces resamplers without using memory allocation in the critical path.
//!
//! The 3-phase swap protocol uses the
//! engine's canonical scheduler
//! ([`RtSwapDrain`] + [`RtSwapHandler`], `NeuralAmpModeler-rs/common/spsc/swap.rs`)
//! for this dedicated `Consumer<Box<ResamplerSwapPayload>>` channel — the ring
//! types match `SwapRing` exactly, so this is a direct instantiation, not a
//! reimplementation. Only the cold handlers
//! (`install_resampler`/`discard_resampler`) remain NAM-Audio-Pipe-specific.
//!
//! Budgeting: at most one structural swap applies per callback
//! (`STRUCTURAL_SWAPS_PER_CALLBACK`, shared across every RT swap drain through
//! the caller's `SwapBudget`); current-generation envelopes in the coalescing
//! window collapse to the latest one (intermediate envelopes discarded to GC)
//! and the excess stays queued / parked in the drain's deferred slot for the
//! next callback (canonical latest-wins protocol). Tuning preserved:
//! [`STRUCTURAL_POPS_PER_CALLBACK`] pops per callback.

use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, GcSink, ResamplerSwapPayload, RtStatusFlags, RtSwapDrain,
    RtSwapHandler,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;

use crate::standalone::pw_host::StreamStatusFlags;
use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Dedicated resampler swap ring drained by [`RtSwapDrain`].
pub type ResamplerSwapRing = Consumer<Box<ResamplerSwapPayload>>;
/// Engine drain owning the resampler ring and the deferred slot.
pub type ResamplerSwapDrain = RtSwapDrain<ResamplerSwapRing>;

/// [`RtSwapHandler`] binding the engine scheduler to the resampler family.
///
/// All payloads are structural (each envelope swaps the active resampler +
/// streaming adapter under the shared budget). Latest-wins identity is the
/// build generation; the staleness guard compares it with
/// `requested_rate_generation` — an envelope built for an older request is
/// discarded to GC **without** unmuting and **without** clearing
/// `RT_STATUS_RESAMP_SWAP_PENDING`, so the callback keeps waiting for the
/// build that matches the most recent request.
pub(crate) struct ResamplerSwapHandler<'a> {
    resampler: &'a mut Box<NamResampler>,
    stream: &'a mut Box<StreamingResampleBuffer>,
    rt_status: &'a RtStatusFlags,
    applied_latency: Option<&'a AtomicU32>,
    applied_latency_pair: Option<&'a AtomicU64>,
    stream_status: Option<&'a StreamStatusFlags>,
}

impl RtSwapHandler for ResamplerSwapHandler<'_> {
    type Payload = ResamplerSwapPayload;

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
                .requested_rate_generation
                .load(Ordering::Acquire),
        )
    }

    #[inline]
    fn generation_of(&self, payload: &Self::Payload) -> Option<u64> {
        Some(payload.generation)
    }

    #[cold]
    fn install(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        install_resampler(
            payload,
            self.resampler,
            self.stream,
            gc,
            self.rt_status,
            self.applied_latency,
            self.applied_latency_pair,
            self.stream_status,
        );
    }

    #[cold]
    fn discard(&mut self, payload: Box<Self::Payload>, gc: &mut GcSink<'_>) {
        discard_resampler(payload, gc);
    }
}

/// Runs one callback drain of the resampler swap ring with coherent pair latency support.
#[inline(always)]
#[cfg(test)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_resamplers_with_pair(
    drain: &mut ResamplerSwapDrain,
    budget: &mut neural_amp_modeler_rs::common::spsc::SwapBudget,
    resampler: &mut Box<NamResampler>,
    stream: &mut Box<StreamingResampleBuffer>,
    rt_status_for_process: &RtStatusFlags,
    applied_latency: Option<&AtomicU32>,
    applied_latency_pair: Option<&AtomicU64>,
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
    let mut handler = ResamplerSwapHandler {
        resampler,
        stream,
        rt_status: rt_status_for_process,
        applied_latency,
        applied_latency_pair,
        stream_status: None,
    };
    drain.drain(&mut handler, budget, &mut gc);
}

/// Runs one callback drain of the resampler swap ring with complete `StreamStatusFlags` seqlock snapshot support.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_resamplers_with_status(
    drain: &mut ResamplerSwapDrain,
    budget: &mut neural_amp_modeler_rs::common::spsc::SwapBudget,
    resampler: &mut Box<NamResampler>,
    stream: &mut Box<StreamingResampleBuffer>,
    rt_status_for_process: &RtStatusFlags,
    stream_status: Option<&StreamStatusFlags>,
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
    let mut handler = ResamplerSwapHandler {
        resampler,
        stream,
        rt_status: rt_status_for_process,
        applied_latency: stream_status.map(|s| &s.applied_resampler_latency),
        applied_latency_pair: stream_status.map(|s| &s.applied_latency_pair),
        stream_status,
    };
    drain.drain(&mut handler, budget, &mut gc);
}

/// Runs one callback drain of the resampler swap ring (engine protocol).
#[inline(always)]
#[cfg(test)]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain signature: engine drain + shared budget + GC cascade parameters"
)]
pub fn drain_resamplers(
    drain: &mut ResamplerSwapDrain,
    budget: &mut neural_amp_modeler_rs::common::spsc::SwapBudget,
    resampler: &mut Box<NamResampler>,
    stream: &mut Box<StreamingResampleBuffer>,
    rt_status_for_process: &RtStatusFlags,
    applied_latency: Option<&AtomicU32>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
) {
    drain_resamplers_with_pair(
        drain,
        budget,
        resampler,
        stream,
        rt_status_for_process,
        applied_latency,
        None,
        gc_producer,
        parking_lot,
        parking_lot_dirty,
        gc_overflow_for_process,
    );
}

/// Builds the drain for a fresh (re)connection, preserving the historical
/// tuning (1 structural swap/callback shared budget, 8-pop window).
pub fn resampler_swap_drain(ring: ResamplerSwapRing) -> ResamplerSwapDrain {
    RtSwapDrain::new(ring, super::commands::structural_swap_tunables())
}

/// Installs a current-generation resampler envelope: swaps the active
/// resampler and streaming adapter, records the applied generation and active rates,
/// unmutes (clears `RT_STATUS_RESAMP_SWAP_PENDING`) and cascades the retired
/// resampler envelope to GC.
#[cold]
#[expect(
    clippy::too_many_arguments,
    reason = "RT callback drain install signature takes individual engine/status components"
)]
fn install_resampler(
    mut payload: Box<ResamplerSwapPayload>,
    resampler: &mut Box<NamResampler>,
    stream: &mut Box<StreamingResampleBuffer>,
    gc: &mut GcSink<'_>,
    rt_status_for_process: &RtStatusFlags,
    applied_latency: Option<&AtomicU32>,
    applied_latency_pair: Option<&AtomicU64>,
    stream_status: Option<&StreamStatusFlags>,
) {
    std::mem::swap(&mut payload.resampler, resampler);
    std::mem::swap(&mut payload.stream, stream);

    let lat = stream.latency_samples();
    if let Some(status) = stream_status {
        status.update_resampler_latency(lat);
        status
            .applied_nam_rate
            .store(resampler.nam_rate(), Ordering::Release);
    } else {
        if let Some(applied) = applied_latency {
            applied.store(lat, Ordering::Release);
        }
        if let Some(pair) = applied_latency_pair {
            let mut cur = pair.load(Ordering::Relaxed);
            loop {
                let (_, cab) = StreamStatusFlags::unpack_latency_pair(cur);
                let next = StreamStatusFlags::pack_latency_pair(lat, cab);
                match pair.compare_exchange_weak(cur, next, Ordering::Release, Ordering::Relaxed) {
                    Ok(_) => break,
                    Err(actual) => cur = actual,
                }
            }
        }
    }

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

    gc.retire(GcItem::ResamplerSwap(payload));
}

/// Discards a resampler envelope to the GC cascade **without** unmuting and
/// **without** clearing `RT_STATUS_RESAMP_SWAP_PENDING` (stale or superseded
/// builds never substitute the most recent request).
#[cold]
fn discard_resampler(payload: Box<ResamplerSwapPayload>, gc: &mut GcSink<'_>) {
    gc.retire(GcItem::ResamplerSwap(payload));
}

#[cfg(test)]
#[path = "resampler_swap_test.rs"]
mod resampler_swap_test;
