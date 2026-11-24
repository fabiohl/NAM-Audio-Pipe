// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RT SPSC drain for neural cabinet slot payloads.

use super::slot::{CabNamSlot, PipeCabNamPayload};
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcSink, RT_STATUS_SPSC_DRAIN_TRUNCATED, RT_STATUS_STRUCTURAL_DEFERRED,
    RT_STATUS_STRUCTURAL_SUPERSEDED, SwapBudget,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-callback accounting returned by [`drain_cab_nam_payloads`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CabNamDrainStats {
    /// Payloads popped from the SPSC consumer during this callback.
    pub pops: usize,
    /// Neural cabinet slots installed into `active_nam` (`0` or `1`).
    pub installed: usize,
    /// Payloads retired to GC without becoming the active slot (stale, superseded, failed, or malformed).
    pub retired: usize,
    /// Valid non-stale payloads left queued because `SwapBudget` was exhausted (`0` or `1`).
    pub deferred: usize,
}

/// Checks if `requested_nam_gen` exceeds `applied_gen` atomically on the RT drain,
/// enforcing the callback-shared [`SwapBudget`] via `peek()` before `pop()`.
///
/// Kept out of line: the callback invokes it on every quantum, but the
/// payload-carrying code paths and their unwind cleanup belong here instead of
/// inflating the capture closure or emitting structural drops there.
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "The drain needs the budget, both generation counters, the exclusive WAV/NAM slots, the latency sink, the failure latch and the GC sink"
)]
pub fn drain_cab_nam_payloads(
    consumer: &mut rtrb::Consumer<PipeCabNamPayload>,
    budget: &mut SwapBudget,
    requested_generation: &AtomicU64,
    applied_generation: &AtomicU64,
    active_wav: &mut Option<Box<CabSimPair>>,
    active_nam: &mut Option<CabNamSlot>,
    active_cabsim_latency: &dyn Fn(u32),
    failed_key: Option<&mut Option<(u32, usize)>>,
    gc: &mut GcSink<'_>,
) -> CabNamDrainStats {
    let max_pops = crate::standalone::pw_host::STRUCTURAL_POPS_PER_CALLBACK;
    let target_gen = requested_generation.load(Ordering::Acquire);
    let mut stats = CabNamDrainStats::default();
    let mut latest: Option<PipeCabNamPayload> = None;

    while stats.pops < max_pops {
        let Ok(head) = consumer.peek() else {
            break;
        };
        if head.generation < target_gen {
            if let Ok(payload) = consumer.pop() {
                stats.pops += 1;
                stats.retired += 1;
                payload.retire_remainder_to_gc(gc);
            }
            continue;
        }
        if latest.is_some() {
            if let Ok(payload) = consumer.pop() {
                stats.pops += 1;
                stats.retired += 1;
                gc.rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
                gc.rt_status
                    .structural_superseded_total
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(prev) = latest.replace(payload) {
                    prev.retire_remainder_to_gc(gc);
                }
            }
            continue;
        }
        if !budget.can_apply() {
            stats.deferred += 1;
            gc.rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            gc.rt_status
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
            break;
        }
        if let Ok(payload) = consumer.pop() {
            stats.pops += 1;
            debug_assert!(latest.is_none(), "cabinet candidate slot must be empty");
            std::mem::forget(latest.replace(payload));
        }
    }

    if stats.pops >= max_pops && !consumer.is_empty() {
        gc.rt_status.set_flag(RT_STATUS_SPSC_DRAIN_TRUNCATED);
    }

    let Some(mut chosen) = latest else {
        return stats;
    };
    if let Some(partition) = chosen.failed_partition {
        if let Some(failed) = failed_key {
            *failed = Some((chosen.host_rate, partition as usize));
        }
        applied_generation.store(chosen.generation, Ordering::Release);
        chosen.retire_remainder_to_gc(gc);
        stats.retired += 1;
        return stats;
    }
    let model_l = chosen.model_l.take();
    let resampler = chosen.resampler.take();
    let (model_l, resampler) = match (model_l, resampler) {
        (Some(model_l), Some(resampler)) => (model_l, resampler),
        (model_l, resampler) => {
            if let Some(model) = model_l {
                gc.retire(GcItem::Model(model));
            }
            if let Some(resampler) = resampler {
                gc.retire(GcItem::Streaming(resampler));
            }
            chosen.retire_remainder_to_gc(gc);
            stats.retired += 1;
            return stats;
        }
    };
    #[cfg(feature = "dual-mono")]
    let model_r = chosen.model_r.take();

    if let Some(old_wav) = active_wav.take() {
        gc.retire(GcItem::CabSimPair(old_wav));
    }
    if let Some(old_nam) = active_nam.take() {
        old_nam.retire_to_gc(gc);
    }

    let slot = CabNamSlot {
        kind: chosen.kind,
        host_rate: chosen.host_rate,
        source_rate: chosen.source_rate,
        resampler,
        model_l,
        #[cfg(feature = "dual-mono")]
        model_r,
        tail_budget: chosen.tail_budget,
        remaining_tail: 0,
    };
    let lat = slot.latency_samples();
    debug_assert!(active_nam.is_none());
    std::mem::forget(active_nam.replace(slot));
    budget.consume();
    stats.installed += 1;
    if let Some(failed) = failed_key {
        *failed = None;
    }
    active_cabsim_latency(lat);
    applied_generation.store(chosen.generation, Ordering::Release);
    std::mem::forget(chosen);
    stats
}
