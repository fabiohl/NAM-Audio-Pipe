// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.2. COMMAND RECEPTION (SPSC Channel)
//! Processes commands from the command-line interface or control system (volume, model, noise gate).
//!
//! # Command Budgeting (F-RB-011 / T2.5)
//!
//! The callback drains under fixed per-quantum budgets so a continuously
//! refilling producer can never monopolize the audio thread:
//! - Scalar parameters (`InputGain`, `OutputGain`, `GateConfig`,
//!   `SlimOverride`) are consumed at most [`MAX_PARAM_BUDGET`] per callback and
//!   coalesced latest-wins inside the budget.
//! - Structural commands (`LoadModel`, and the dedicated swap channels drained
//!   by the other RT modules) apply at most [`STRUCTURAL_SWAPS_PER_CALLBACK`]
//!   per callback; obsolete intermediate commands are discarded to the GC
//!   cascade (coalescing) and the excess is parked in a deferred slot resolved
//!   at the start of the next callback.
//! - When the scalar budget is exhausted with commands still queued, the
//!   `RT_STATUS_PARAM_QUEUE_BACKLOG` flag records the occurrence for the main
//!   thread (telemetry only; no command is ever lost).

use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, ParamPayload, RT_STATUS_NEEDS_OS_REBUILD,
    RT_STATUS_PARAM_QUEUE_BACKLOG, RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED,
    RtStatusFlags, SlimModelPair, gc_cascade,
};
use neural_amp_modeler_rs::dsp::adaptive::{AdaptiveCompute, SlimOverride};
use neural_amp_modeler_rs::dsp::gate::GateParams;
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine};
use neural_amp_modeler_rs::models::StaticModel;

use rtrb::Consumer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum number of structural swaps applied per audio callback, shared across
/// every RT swap drain (resampler, cab-sim, model pair, oversampling) —
/// F-RB-011 / T2.5. Heavy state movement is bounded to one transaction per
/// audio quantum.
pub const STRUCTURAL_SWAPS_PER_CALLBACK: usize = 1;

/// Maximum number of payloads popped from a single structural SPSC channel per
/// audio callback (coalescing window). Bounds the drain loop even when the
/// producer refills the ring continuously (F-RB-011). The channels have
/// capacity 4, so this window covers the full queue plus producer refills.
pub const STRUCTURAL_POPS_PER_CALLBACK: usize = 8;

/// Maximum number of `ParamPayload` commands consumed per audio callback —
/// the scalar parameter budget (F-RB-011 / T2.5). Scalar commands inside the
/// budget are coalesced latest-wins; the excess is left in the ring for the
/// next callback and flagged via `RT_STATUS_PARAM_QUEUE_BACKLOG`.
pub const MAX_PARAM_BUDGET: usize = 16;

/// 5.1.2. COMMAND RECEPTION (SPSC Channel)
/// Processes commands from the command-line interface or control system (volume, model, noise gate).
///
/// Runs under the [`MAX_PARAM_BUDGET`] drain budget (F-RB-011 / T2.5):
/// scalar parameters are coalesced latest-wins inside the budget, `LoadModel`
/// structural swaps obey the shared [`STRUCTURAL_SWAPS_PER_CALLBACK`] budget
/// and park in `deferred` when exhausted, and a non-empty channel after the
/// budget raises `RT_STATUS_PARAM_QUEUE_BACKLOG`.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn receive_commands(
    consumer: &mut rtrb::Consumer<ParamPayload>,
    deferred: &mut Option<ParamPayload>,
    structural_applied: &mut usize,
    model_input_mult_adj: &mut f32,
    model_output_mult_adj: &mut f32,
    current_nam_rate: &mut u32,
    active_model_l: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    active_model_r: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &Arc<RtStatusFlags>,
    user_input_gain_mult: &mut f32,
    user_output_gain_mult: &mut f32,
    gate_params: &mut GateParams,
    threshold_open_sq: &mut f32,
    threshold_close_sq: &mut f32,
    lut: &neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT,
    adaptive: &mut AdaptiveCompute,
) -> bool {
    let mut param_changed = false;

    // Phase 0 — resolve a `LoadModel` deferred by the previous callback. It is
    // causally before everything still in the ring, so it applies first; a
    // newer `LoadModel` already queued supersedes it (latest-wins coalescing).
    if let Some(payload) = deferred.take() {
        let queued_newer = consumer
            .peek()
            .is_ok_and(|head| matches!(head, ParamPayload::LoadModel { .. }));
        if queued_newer {
            discard_load_model(
                payload,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
        } else if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_load_model(
                payload,
                model_input_mult_adj,
                model_output_mult_adj,
                current_nam_rate,
                active_model_l,
                active_model_r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
                adaptive,
            );
            *structural_applied += 1;
            param_changed = true;
        } else {
            // Budget already exhausted by an earlier drain in this callback and
            // no newer model queued: re-park for the next callback.
            *deferred = Some(payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // Phase 1 — bounded drain with latest-wins coalescing. Scalars accumulate
    // into pending locals (only the last value of each is applied); structural
    // `LoadModel` payloads are collapsed to a single candidate.
    let mut pending_input_gain: Option<f32> = None;
    let mut pending_output_gain: Option<f32> = None;
    let mut pending_gate: Option<GateParams> = None;
    let mut pending_slim_override: Option<SlimOverride> = None;
    let mut pending_model: Option<ParamPayload> = None;
    let mut pops = 0usize;
    while pops < MAX_PARAM_BUDGET {
        let Some(payload) = consumer.pop().ok() else {
            break;
        };
        pops += 1;
        match payload {
            ParamPayload::LoadModel { .. } => {
                if let Some(older) = pending_model.replace(payload) {
                    // An intermediate `LoadModel` is obsolete — its boxes are
                    // discarded to the GC cascade (coalescing, latest-wins).
                    discard_load_model(
                        older,
                        gc_producer,
                        parking_lot,
                        parking_lot_dirty,
                        gc_overflow_for_process,
                        rt_status_for_process,
                    );
                }
            }
            ParamPayload::InputGain(mult) => pending_input_gain = Some(mult),
            ParamPayload::OutputGain(mult) => pending_output_gain = Some(mult),
            ParamPayload::GateConfig(params) => pending_gate = Some(params),
            ParamPayload::SlimOverride(ov) => pending_slim_override = Some(ov),
            // Lightweight request (atomics only): the actual engine swap is
            // budgeted when the delivered engines are drained in
            // `drain_os_engines`. Latest value wins by overwrite.
            ParamPayload::SetOversample(factor) => {
                rt_status_for_process
                    .requested_os_factor
                    .store(factor.to_f32() as u32, Ordering::Relaxed);
                rt_status_for_process.set_flag_release(RT_STATUS_NEEDS_OS_REBUILD);
            }
        }
    }

    // Apply the coalesced scalar parameters (latest-wins).
    if let Some(mult) = pending_input_gain {
        *user_input_gain_mult = mult;
        param_changed = true;
    }
    if let Some(mult) = pending_output_gain {
        *user_output_gain_mult = mult;
        param_changed = true;
    }
    if let Some(params) = pending_gate {
        let open_lin = lut.db_to_linear(params.threshold_open_db);
        let close_lin = lut.db_to_linear(params.threshold_close_db);
        *threshold_open_sq = open_lin * open_lin;
        *threshold_close_sq = close_lin * close_lin;
        *gate_params = params;
    }
    if let Some(ov) = pending_slim_override {
        adaptive.set_slim_override(ov);
    }

    // Apply the single coalesced structural command under the shared budget.
    if let Some(payload) = pending_model {
        if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_load_model(
                payload,
                model_input_mult_adj,
                model_output_mult_adj,
                current_nam_rate,
                active_model_l,
                active_model_r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
                adaptive,
            );
            *structural_applied += 1;
            param_changed = true;
        } else if deferred.is_none() {
            *deferred = Some(payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // The deferred slot holds an older model; the popped one is newer
            // — supersede the parked command and park this one (latest-wins).
            let older = deferred.take().expect("slot occupied, checked above");
            discard_load_model(
                older,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            *deferred = Some(payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // Backlog telemetry (F-RB-011 / T2.5): the channel still held commands
    // after the fixed budget — the remainder is drained by the next callback.
    if !consumer.is_empty() {
        rt_status_for_process.set_flag(RT_STATUS_PARAM_QUEUE_BACKLOG);
    }

    param_changed
}

/// Installs a `LoadModel` payload atomically: swaps both active channel
/// pointers, injects the RT status, and cascades the replaced models to GC.
/// `#[cold]` — a structural apply, never the per-block hot path.
#[cold]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
fn install_load_model(
    payload: ParamPayload,
    model_input_mult_adj: &mut f32,
    model_output_mult_adj: &mut f32,
    current_nam_rate: &mut u32,
    active_model_l: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    active_model_r: &mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &Arc<RtStatusFlags>,
    adaptive: &mut AdaptiveCompute,
) {
    let ParamPayload::LoadModel {
        model_l,
        model_r,
        input_mult_adj,
        output_mult_adj,
        sample_rate,
    } = payload
    else {
        unreachable!("only LoadModel reaches install_load_model");
    };

    if model_l.is_some() || model_r.is_some() {
        *model_input_mult_adj = input_mult_adj;
        *model_output_mult_adj = output_mult_adj;
        *current_nam_rate = sample_rate;
    } else {
        *model_input_mult_adj = 1.0;
        *model_output_mult_adj = 1.0;
        *current_nam_rate = 48_000;
    }

    let mut old_models: [Option<Box<neural_amp_modeler_rs::models::StaticModel>>; 2] = [None, None];
    if let Some(old) = std::mem::replace(active_model_l, model_l) {
        old_models[0] = Some(old);
    }
    if let Some(model) = active_model_l {
        model.inject_rt_status(Arc::clone(rt_status_for_process));
        if let StaticModel::WavenetDyn(w) = model.as_ref() {
            adaptive.set_wavenet_full_ch(w.ch, model.is_slimmable_capable());
        }
    }
    if let Some(old) = std::mem::replace(active_model_r, model_r) {
        old_models[1] = Some(old);
    }
    if let Some(model) = active_model_r {
        model.inject_rt_status(Arc::clone(rt_status_for_process));
    }

    for m_opt in &mut old_models {
        if let Some(m) = m_opt.take() {
            parking_lot_dirty.store(true, Ordering::Release);
            gc_cascade(
                Some(GcItem::Model(m)),
                gc_producer,
                parking_lot,
                gc_overflow_for_process,
                rt_status_for_process,
            );
        }
    }
}

/// Discards an obsolete `LoadModel` payload to the GC cascade (command
/// coalescing — latest-wins). Its model boxes drop off-RT; the payload is
/// never applied and never dropped on the audio thread.
#[cold]
fn discard_load_model(
    payload: ParamPayload,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &Arc<RtStatusFlags>,
) {
    let ParamPayload::LoadModel {
        model_l, model_r, ..
    } = payload
    else {
        unreachable!("only LoadModel reaches discard_load_model");
    };
    for model in [model_l, model_r].into_iter().flatten() {
        parking_lot_dirty.store(true, Ordering::Release);
        gc_cascade(
            Some(GcItem::Model(model)),
            gc_producer,
            parking_lot,
            gc_overflow_for_process,
            rt_status_for_process,
        );
    }
    rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
    rt_status_for_process
        .structural_superseded_total
        .fetch_add(1, Ordering::Relaxed);
}

/// Signals the main thread to rebuild WaveNet models with a reduced channel count.
///
/// The audio thread ONLY sets the atomic flag, target channel count, and the
/// rebuild generation. All allocation, prewarm, and mmap happen on the main
/// thread. The generation is bumped with `Release` before the flag is armed so
/// the main thread's `Acquire` capture observes the full request (F-RB-004
/// ordering pattern) and the RT drain can discard stale in-flight pairs.
#[inline(always)]
pub fn try_slimmable_rebuild(adaptive: &mut AdaptiveCompute, rt_status: &RtStatusFlags) {
    let Some(target_ch) = adaptive.take_slimmable_rebuild() else {
        return;
    };
    rt_status
        .requested_slimmable_ch
        .store(target_ch as u32, Ordering::Relaxed);
    rt_status
        .requested_slimmable_generation
        .fetch_add(1, Ordering::Release);
    rt_status
        .set_flag_release(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
}

/// Drains slimmable-rebuilt model pairs delivered by the main thread via SPSC
/// (F-RB-005).
///
/// Each [`SlimModelPair`] is consumed with a single `pop()` and both channels
/// are swapped in the same logical block — an all-or-nothing transaction.
/// Stale pairs (built for an older rebuild generation) are discarded to the GC
/// cascade without touching the active models, so L/R can never belong to
/// different generations or channel counts.
///
/// Budgeting (F-RB-011 / T2.5): at most [`STRUCTURAL_SWAPS_PER_CALLBACK`]
/// structural swap applies per callback (shared budget); current-generation
/// pairs in the coalescing window collapse to the latest one (intermediate
/// pairs discarded to GC) and the excess is parked in `deferred` for the next
/// callback.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
pub fn drain_slimmable_models(
    slimmable_rx: &mut Option<Consumer<Box<SlimModelPair>>>,
    deferred: &mut Option<Box<SlimModelPair>>,
    structural_applied: &mut usize,
    active_model_l: &mut Option<Box<StaticModel>>,
    active_model_r: &mut Option<Box<StaticModel>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    let Some(rx) = slimmable_rx.as_mut() else {
        return;
    };

    // Phase 0 — resolve a pair deferred by the previous callback.
    if let Some(pending) = deferred.take() {
        let current_gen = rt_status
            .requested_slimmable_generation
            .load(Ordering::Acquire);
        let head_is_current = rx.peek().is_ok_and(|head| head.generation == current_gen);
        if pending.generation != current_gen {
            // Stale while parked: discard whole (L and R together) to GC —
            // never installed (F-RB-005).
            discard_pair_whole(
                pending.l,
                pending.r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
        } else if head_is_current {
            // A newer same-generation pair is already queued (latest-wins).
            discard_pair_whole(
                pending.l,
                pending.r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
        } else if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_pair(
                pending.l,
                pending.r,
                active_model_l,
                active_model_r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            *structural_applied += 1;
        } else {
            // Budget exhausted and nothing newer queued: re-park.
            *deferred = Some(pending);
            rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // Phase 1 — bounded drain with coalescing (F-RB-011 / T2.5).
    let current_gen = rt_status
        .requested_slimmable_generation
        .load(Ordering::Acquire);
    let mut candidate: Option<Box<SlimModelPair>> = None;
    let mut pops = 0usize;
    while pops < STRUCTURAL_POPS_PER_CALLBACK {
        let Some(pair) = rx.pop().ok() else {
            break;
        };
        pops += 1;
        if pair.generation != current_gen {
            // Stale-rebuild guard: the pair is obsolete and is discarded whole
            // (L and R together) to the GC cascade.
            discard_pair_whole(
                pair.l,
                pair.r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            continue;
        }
        if let Some(older) = candidate.replace(pair) {
            // Coalescing: an intermediate current-generation pair is obsolete.
            discard_pair_whole(
                older.l,
                older.r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(pair) = candidate {
        if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_pair(
                pair.l,
                pair.r,
                active_model_l,
                active_model_r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            *structural_applied += 1;
        } else if deferred.is_none() {
            *deferred = Some(pair);
            rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // The deferred slot holds an older pair; the popped one is newer
            // — supersede the parked pair and park this one (latest-wins).
            let older = deferred.take().expect("slot occupied, checked above");
            discard_pair_whole(
                older.l,
                older.r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
            *deferred = Some(pair);
            rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Atomically swaps both active model channels from a pair: the previous L and
/// R models (if any) cascade to GC. L and R always belong to the same pair.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
fn install_pair(
    l: Box<StaticModel>,
    r: Option<Box<StaticModel>>,
    active_model_l: &mut Option<Box<StaticModel>>,
    active_model_r: &mut Option<Box<StaticModel>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    let old_l = active_model_l.replace(l);
    let old_r = r.and_then(|r| active_model_r.replace(r));
    if let Some(old) = old_l {
        parking_lot_dirty.store(true, Ordering::Release);
        gc_cascade(
            Some(GcItem::Model(old)),
            gc_producer,
            parking_lot,
            gc_overflow,
            rt_status,
        );
    }
    if let Some(old) = old_r {
        parking_lot_dirty.store(true, Ordering::Release);
        gc_cascade(
            Some(GcItem::Model(old)),
            gc_producer,
            parking_lot,
            gc_overflow,
            rt_status,
        );
    }
}

/// Discards a whole pair (L and R together) to the GC cascade — never applied
/// (F-RB-005). Both channels of an obsolete/stale pair always travel together.
#[inline(always)]
fn discard_pair_whole(
    l: Box<StaticModel>,
    r: Option<Box<StaticModel>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::Model(l)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
    if let Some(r) = r {
        parking_lot_dirty.store(true, Ordering::Release);
        gc_cascade(
            Some(GcItem::Model(r)),
            gc_producer,
            parking_lot,
            gc_overflow,
            rt_status,
        );
    }
}

/// Drains oversampling engines delivered by the main thread via SPSC.
/// Swaps both L and R engines and sends the obsolete ones to the GC cascade.
///
/// Budgeting (F-RB-011 / T2.5): at most [`STRUCTURAL_SWAPS_PER_CALLBACK`]
/// structural swap applies per callback (shared budget); engine pairs in the
/// coalescing window collapse to the latest one and the excess is parked in
/// `deferred` for the next callback.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
pub fn drain_os_engines(
    os_rx: &mut Option<Consumer<Box<OsEnginePair>>>,
    deferred: &mut Option<Box<OsEnginePair>>,
    structural_applied: &mut usize,
    os_l: &mut Box<OversampleEngine>,
    os_r: &mut Box<OversampleEngine>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    let Some(rx) = os_rx.as_mut() else {
        return;
    };

    // Phase 0 — resolve an engine pair deferred by the previous callback.
    if let Some(pending) = deferred.take() {
        let head_queued = rx.peek().is_ok();
        if head_queued {
            // A newer pair is already queued (latest-wins): the deferred pair
            // is obsolete and its engines cascade to GC.
            discard_os_pair(
                *pending,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
        } else if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_os_pair(
                *pending,
                os_l,
                os_r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            *structural_applied += 1;
        } else {
            // Budget exhausted and nothing newer queued: re-park.
            *deferred = Some(pending);
            rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // Phase 1 — bounded drain with coalescing (F-RB-011 / T2.5).
    let mut candidate: Option<Box<OsEnginePair>> = None;
    let mut pops = 0usize;
    while pops < STRUCTURAL_POPS_PER_CALLBACK {
        let Some(pair) = rx.pop().ok() else {
            break;
        };
        pops += 1;
        if let Some(older) = candidate.replace(pair) {
            discard_os_pair(
                *older,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(pair) = candidate {
        if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_os_pair(
                *pair,
                os_l,
                os_r,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            *structural_applied += 1;
        } else if deferred.is_none() {
            *deferred = Some(pair);
            rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // The deferred slot holds an older pair; the popped one is newer
            // — supersede the parked pair and park this one (latest-wins).
            let older = deferred.take().expect("slot occupied, checked above");
            discard_os_pair(
                *older,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
            *deferred = Some(pair);
            rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Swaps both active OS engines and cascades the replaced pair to GC.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
fn install_os_pair(
    pair: OsEnginePair,
    os_l: &mut Box<OversampleEngine>,
    os_r: &mut Box<OversampleEngine>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    let old_l = std::mem::replace(os_l, pair.l);
    let old_r = std::mem::replace(os_r, pair.r);
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::Oversample(old_l)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
    gc_cascade(
        Some(GcItem::Oversample(old_r)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
}

/// Discards an obsolete OS engine pair to the GC cascade.
#[inline(always)]
fn discard_os_pair(
    pair: OsEnginePair,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::Oversample(pair.l)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
    gc_cascade(
        Some(GcItem::Oversample(pair.r)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use neural_amp_modeler_rs::common::params::AdaptiveComputeMode;
    use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
    use neural_amp_modeler_rs::math::common::AlignedVec;
    use neural_amp_modeler_rs::models::wavenet::WaveNetModelDyn;
    use std::assert_matches;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    /// Builds a structurally minimal `WavenetDyn` for drain tests. The drain
    /// path only inspects `channels()` and drops models — no DSP is run, so an
    /// empty `arrays` vector is sufficient and deterministic.
    fn fake_wavenet(ch: usize) -> Box<StaticModel> {
        Box::new(StaticModel::WavenetDyn(Box::new(WaveNetModelDyn {
            ch,
            k: 3,
            head: 1,
            arrays: vec![],
            head_scale: 1.0,
            receptive_field_size: 1,
            condition_dsp: None,
            condition_dsp_output: AlignedVec::new(1, 0.0f32).expect("alloc"),
            post_stack_head: None,
            head_output_scratch: AlignedVec::new(1, 0.0f32).expect("alloc"),
            prewarm_on_reset: false,
            slimmable_capable: true,
            allowed_channels: None,
            pending_slim_channel: None,
        })))
    }

    fn make_pair(generation: u64, ch: usize, stereo: bool) -> Box<SlimModelPair> {
        Box::new(SlimModelPair {
            generation,
            channels: ch,
            l: fake_wavenet(ch),
            r: stereo.then(|| fake_wavenet(ch)),
        })
    }

    fn load_model_payload(
        model_l: Option<Box<StaticModel>>,
        model_r: Option<Box<StaticModel>>,
    ) -> ParamPayload {
        ParamPayload::LoadModel {
            model_l,
            model_r,
            input_mult_adj: 1.0,
            output_mult_adj: 1.0,
            sample_rate: 48_000,
        }
    }

    /// Harness: runs one `receive_commands` callback drain and returns the
    /// coalesced scalar state (input gain, output gain, slim override) and the
    /// installed L/R models.
    fn run_receive_commands(
        consumer: &mut rtrb::Consumer<ParamPayload>,
        deferred: &mut Option<ParamPayload>,
        structural_applied: &mut usize,
        flags: &Arc<RtStatusFlags>,
    ) -> (
        f32,
        f32,
        SlimOverride,
        Option<Box<StaticModel>>,
        Option<Box<StaticModel>>,
    ) {
        let lut = neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut();
        let (mut gc_p, _gc_c) = rtrb::RingBuffer::<GcItem>::new(64);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let mut input_gain = 1.0f32;
        let mut output_gain = 1.0f32;
        let mut gate_params = GateParams::default();
        let mut thr_open = 0.0f32;
        let mut thr_close = 0.0f32;
        let mut adaptive = AdaptiveCompute::new(AdaptiveComputeMode::Off);
        let mut model_l: Option<Box<StaticModel>> = None;
        let mut model_r: Option<Box<StaticModel>> = None;
        let mut in_adj = 1.0f32;
        let mut out_adj = 1.0f32;
        let mut nam_rate = 48_000u32;

        receive_commands(
            consumer,
            deferred,
            structural_applied,
            &mut in_adj,
            &mut out_adj,
            &mut nam_rate,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            flags,
            &mut input_gain,
            &mut output_gain,
            &mut gate_params,
            &mut thr_open,
            &mut thr_close,
            lut,
            &mut adaptive,
        );
        (
            input_gain,
            output_gain,
            adaptive.slim_override(),
            model_l,
            model_r,
        )
    }

    #[test]
    fn drain_slimmable_empty_no_change() {
        let mut rx = None;
        let mut model_l = None;
        let mut model_r = None;
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();
        let mut deferred = None;
        let mut structural_applied = 0usize;

        drain_slimmable_models(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert!(!parking_lot_dirty.load(Ordering::Acquire));
        assert!(gc_c.pop().is_err());
    }

    #[test]
    fn drain_os_engines_swaps_and_sets_dirty() {
        let (mut prod, cons) = rtrb::RingBuffer::new(4);
        let mut rx = Some(cons);
        let mut os_l = Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap());
        let mut os_r = Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap());
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();
        let mut deferred = None;
        let mut structural_applied = 0usize;

        let pair = Box::new(OsEnginePair {
            l: Box::new(OversampleEngine::new(OversampleFactor::X2, 64).unwrap()),
            r: Box::new(OversampleEngine::new(OversampleFactor::X2, 64).unwrap()),
        });
        prod.push(pair).unwrap();

        drain_os_engines(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut os_l,
            &mut os_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert!(parking_lot_dirty.load(Ordering::Acquire));
        let old1 = gc_c.pop().unwrap();
        let old2 = gc_c.pop().unwrap();
        assert_matches!(old1, GcItem::Oversample(_));
        assert_matches!(old2, GcItem::Oversample(_));
    }

    /// F-RB-005 core: a stereo pair is consumed with a single `pop()` and BOTH
    /// channels are swapped together — the active L/R always belong to the same
    /// pair (same generation and channel count). The previous complete pair is
    /// sent to the GC cascade.
    #[test]
    fn drain_slimmable_pair_swaps_both_atomically() {
        let (mut prod, cons) = rtrb::RingBuffer::new(4);
        let mut rx = Some(cons);
        let mut model_l = Some(fake_wavenet(4));
        let mut model_r = Some(fake_wavenet(4));
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(8);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();
        let mut deferred = None;
        let mut structural_applied = 0usize;
        flags
            .requested_slimmable_generation
            .store(1, Ordering::Release);

        prod.push(make_pair(1, 8, true)).unwrap();

        drain_slimmable_models(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert!(parking_lot_dirty.load(Ordering::Acquire));
        let (l, r) = (model_l.as_ref().unwrap(), model_r.as_ref().unwrap());
        assert_eq!(l.channels(), 8);
        assert_eq!(r.channels(), 8);

        // The old complete pair (both channels) went to GC.
        let old1 = gc_c.pop().unwrap();
        let old2 = gc_c.pop().unwrap();
        match (old1, old2) {
            (GcItem::Model(m1), GcItem::Model(m2)) => {
                let mut chs = [m1.channels(), m2.channels()];
                chs.sort_unstable();
                assert_eq!(chs, [4, 4]);
            }
            _ => panic!("expected two GcItem::Model for the old pair"),
        }
        assert!(gc_c.pop().is_err());
    }

    /// Mono pairs (`r == None`) must not touch the active R channel: L is
    /// swapped alone and the previous R is preserved.
    #[test]
    fn drain_slimmable_mono_pair_leaves_r_untouched() {
        let (mut prod, cons) = rtrb::RingBuffer::new(4);
        let mut rx = Some(cons);
        let mut model_l = Some(fake_wavenet(4));
        let mut model_r = Some(fake_wavenet(8));
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(8);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();
        let mut deferred = None;
        let mut structural_applied = 0usize;
        flags
            .requested_slimmable_generation
            .store(1, Ordering::Release);

        prod.push(make_pair(1, 8, false)).unwrap();

        drain_slimmable_models(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert_eq!(model_l.as_ref().unwrap().channels(), 8);
        assert_eq!(
            model_r.as_ref().unwrap().channels(),
            8,
            "mono pair must leave the active R model untouched"
        );

        // Only the old L was replaced (and therefore GC'd).
        let old = gc_c.pop().unwrap();
        assert_matches!(old, GcItem::Model(m) if m.channels() == 4);
        assert!(gc_c.pop().is_err());
    }

    /// Stale pairs (built for an older rebuild generation) are discarded whole
    /// — L and R together — to the GC cascade without touching the active
    /// models (F-RB-005 latest-wins).
    #[test]
    fn drain_slimmable_discards_stale_pair_latest_wins() {
        let (mut prod, cons) = rtrb::RingBuffer::new(4);
        let mut rx = Some(cons);
        let mut model_l = Some(fake_wavenet(4));
        let mut model_r = Some(fake_wavenet(4));
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(8);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();
        let mut deferred = None;
        let mut structural_applied = 0usize;

        // A stale pair (gen 1) is in the channel while the request advances to 2.
        prod.push(make_pair(1, 8, true)).unwrap();
        flags
            .requested_slimmable_generation
            .store(2, Ordering::Release);
        prod.push(make_pair(2, 4, true)).unwrap();

        drain_slimmable_models(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        // The stale pair was discarded whole to GC (never installed).
        let stale_l = gc_c.pop().unwrap();
        let stale_r = gc_c.pop().unwrap();
        match (stale_l, stale_r) {
            (GcItem::Model(m1), GcItem::Model(m2)) => {
                let mut chs = [m1.channels(), m2.channels()];
                chs.sort_unstable();
                assert_eq!(chs, [8, 8], "stale pair must be discarded whole");
            }
            _ => panic!("expected stale GcItem::Model pair"),
        }

        // The latest pair (gen 2) was installed atomically.
        let (l, r) = (model_l.as_ref().unwrap(), model_r.as_ref().unwrap());
        assert_eq!(l.channels(), 4);
        assert_eq!(r.channels(), 4);
    }

    /// Flooding acceptance (F-RB-005): hundreds of pairs with alternating
    /// channel counts pushed from multiple threads must never desynchronize L/R
    /// — at no point may `active_model_l.channels() != active_model_r.channels()`
    /// or channels be inverted (a partial/failed delivery is impossible because
    /// each pair is atomic). The SPSC producer is single-writer by contract, so
    /// the producer threads serialize through a `Mutex` (the real main thread
    /// is a single writer; the point is stress on the atomic consumer swap).
    #[test]
    fn drain_slimmable_flood_never_desyncs() {
        const TOTAL: usize = 600;
        const CAPACITY: usize = 8;
        const PRODUCERS: usize = 3;

        let (prod, cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(CAPACITY);
        let mut rx = Some(cons);
        let mut model_l = Some(fake_wavenet(4));
        let mut model_r = Some(fake_wavenet(4));
        let (mut gc_p, mut _gc_c) = rtrb::RingBuffer::<GcItem>::new(4096);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();
        let mut deferred = None;
        let mut structural_applied;

        // All pairs share generation 0 so the drain installs every one; the
        // producers alternate channel counts (8/4) per pair.
        let prod = Arc::new(std::sync::Mutex::new(prod));
        let pushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(PRODUCERS);
        for p in 0..PRODUCERS {
            let prod = Arc::clone(&prod);
            let pushed = Arc::clone(&pushed);
            handles.push(std::thread::spawn(move || {
                let mut count = 0usize;
                while count < TOTAL / PRODUCERS {
                    let ch = if (p + count).is_multiple_of(2) { 8 } else { 4 };
                    let pair = make_pair(0, ch, true);
                    let mut prod = prod.lock().unwrap();
                    if prod.push(pair).is_ok() {
                        count += 1;
                        pushed.fetch_add(1, Ordering::Relaxed);
                    } else {
                        std::thread::yield_now();
                    }
                }
            }));
        }

        // Consumer: drain repeatedly and assert the L/R invariant after every
        // drain until all pairs have been installed and the channel is empty.
        // Each loop iteration is one audio callback: the per-quantum structural
        // budget resets to zero (T2.5).
        while pushed.load(Ordering::Acquire) < TOTAL || !rx.as_ref().unwrap().is_empty() {
            structural_applied = 0;
            drain_slimmable_models(
                &mut rx,
                &mut deferred,
                &mut structural_applied,
                &mut model_l,
                &mut model_r,
                &mut gc_p,
                &mut parking_lot,
                &parking_lot_dirty,
                &gc_overflow,
                &flags,
            );
            let (l, r) = (model_l.as_ref().unwrap(), model_r.as_ref().unwrap());
            assert_eq!(
                l.channels(),
                r.channels(),
                "flood desynchronized active L/R channels"
            );
        }
        for h in handles {
            h.join().unwrap();
        }
        // Final drain to consume any tail.
        structural_applied = 0;
        drain_slimmable_models(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );
        let (l, r) = (model_l.as_ref().unwrap(), model_r.as_ref().unwrap());
        assert_eq!(l.channels(), r.channels());
        assert!(rx.as_ref().unwrap().is_empty());
    }

    // ── T2.5 Command Budgeting & Coalescing (F-RB-011) ──────────────────────

    /// The scalar parameter drain is bounded to `MAX_PARAM_BUDGET` per callback
    /// (T2.5). A channel overflowing the budget keeps its remainder for the
    /// next callback and raises `RT_STATUS_PARAM_QUEUE_BACKLOG` — the audio
    /// deadline is preserved, no command is lost.
    #[test]
    fn receive_commands_param_budget_bounds_pops_and_flags_backlog() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<ParamPayload>::new(64);
        let flags = Arc::new(RtStatusFlags::new());
        let mut deferred = None;
        let mut structural_applied = 0usize;

        // 40 scalar commands: 16 are consumed by the first callback.
        for i in 0..40u32 {
            prod.push(ParamPayload::InputGain(0.1 + i as f32 * 0.01))
                .unwrap();
        }

        let (input_gain, _, _, _, _) =
            run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);

        // The 16th command is the last one seen → latest-wins within the budget.
        assert!((input_gain - (0.1 + 15.0 * 0.01)).abs() < f32::EPSILON);
        assert_eq!(cons.slots(), 24, "the remaining commands stay queued");
        assert!(
            flags.check_flag(RT_STATUS_PARAM_QUEUE_BACKLOG),
            "backlog flag must be raised when the budget is exhausted"
        );
        assert_eq!(structural_applied, 0);

        // Drain again: the rest is consumed across the next callbacks. The
        // backlog flag is sticky (cleared by the main thread), so the third
        // callback must NOT re-raise it once the channel is empty.
        flags.clear_flag(RT_STATUS_PARAM_QUEUE_BACKLOG);
        run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);
        assert_eq!(cons.slots(), 8);

        flags.clear_flag(RT_STATUS_PARAM_QUEUE_BACKLOG);
        run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);
        assert!(cons.is_empty(), "all commands eventually consumed");
        assert!(
            !flags.check_flag(RT_STATUS_PARAM_QUEUE_BACKLOG),
            "an empty channel must not raise the backlog flag"
        );
    }

    /// Repeated scalar commands inside one quantum are coalesced latest-wins:
    /// only the last `InputGain`/`OutputGain`/`SlimOverride` is applied.
    #[test]
    fn receive_commands_scalar_latest_wins() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<ParamPayload>::new(8);
        let flags = Arc::new(RtStatusFlags::new());
        let mut deferred = None;
        let mut structural_applied = 0usize;

        prod.push(ParamPayload::InputGain(0.5)).unwrap();
        prod.push(ParamPayload::OutputGain(0.7)).unwrap();
        prod.push(ParamPayload::SlimOverride(SlimOverride::ForceFull))
            .unwrap();
        prod.push(ParamPayload::InputGain(0.9)).unwrap();
        prod.push(ParamPayload::OutputGain(1.3)).unwrap();
        prod.push(ParamPayload::SlimOverride(SlimOverride::ForceLite))
            .unwrap();

        let (input_gain, output_gain, slim_override, _, _) =
            run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);

        assert_eq!(input_gain, 0.9);
        assert_eq!(output_gain, 1.3);
        assert_eq!(slim_override, SlimOverride::ForceLite);
        assert!(!flags.check_flag(RT_STATUS_PARAM_QUEUE_BACKLOG));
        assert!(cons.is_empty());
    }

    /// Structural `LoadModel` commands are coalesced: an intermediate queued
    /// model is obsolete and its boxes are discarded to the GC cascade — only
    /// the latest one is installed (T2.5 latest-wins).
    #[test]
    fn receive_commands_load_model_coalesces_obsolete_to_gc() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<ParamPayload>::new(8);
        let flags = Arc::new(RtStatusFlags::new());
        let mut deferred = None;
        let mut structural_applied = 0usize;

        prod.push(load_model_payload(Some(fake_wavenet(4)), None))
            .unwrap();
        prod.push(load_model_payload(Some(fake_wavenet(8)), None))
            .unwrap();
        prod.push(load_model_payload(Some(fake_wavenet(16)), None))
            .unwrap();

        let (_, _, _, model_l, _) =
            run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);

        assert_eq!(
            model_l.as_ref().unwrap().channels(),
            16,
            "latest LoadModel must win"
        );
        assert_eq!(structural_applied, 1, "one structural apply per callback");
        assert!(deferred.is_none());
        assert!(cons.is_empty());
        assert!(
            flags.check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED),
            "obsolete intermediate models must be flagged superseded"
        );
    }

    /// A `LoadModel` drained after the shared structural budget was exhausted
    /// by another swap earlier in the callback is parked (deferred) — never
    /// applied out of budget and never lost.
    #[test]
    fn receive_commands_load_model_parked_when_budget_exhausted() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<ParamPayload>::new(8);
        let flags = Arc::new(RtStatusFlags::new());
        let mut deferred = None;
        let mut structural_applied = 1usize; // a resampler swap applied earlier

        prod.push(load_model_payload(Some(fake_wavenet(8)), None))
            .unwrap();

        let (_, _, _, model_l, _) =
            run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);

        assert!(model_l.is_none(), "must not apply out of budget");
        assert!(
            deferred.is_some(),
            "the model must be parked for the next callback"
        );
        assert!(flags.check_flag(RT_STATUS_STRUCTURAL_DEFERRED));
        assert!(cons.is_empty());
    }

    /// A parked `LoadModel` is resolved at the start of the next callback when
    /// the budget is fresh.
    #[test]
    fn receive_commands_deferred_model_resolved_next_callback() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<ParamPayload>::new(8);
        let flags = Arc::new(RtStatusFlags::new());
        let mut deferred = None;
        let mut structural_applied = 1usize;

        prod.push(load_model_payload(Some(fake_wavenet(8)), None))
            .unwrap();
        run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);
        assert!(deferred.is_some());

        // Next callback: fresh budget → the parked model is installed first.
        structural_applied = 0;
        let (_, _, _, model_l, _) =
            run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);
        assert_eq!(model_l.as_ref().unwrap().channels(), 8);
        assert!(deferred.is_none());
    }

    /// A parked `LoadModel` superseded by a newer queued model is discarded to
    /// the GC cascade — the newest command wins (latest-wins coalescing).
    #[test]
    fn receive_commands_deferred_model_superseded_by_queued() {
        let (mut prod, mut cons) = rtrb::RingBuffer::<ParamPayload>::new(8);
        let flags = Arc::new(RtStatusFlags::new());
        let mut deferred = None;
        let mut structural_applied = 1usize;

        // Park an 8-ch model from the previous callback.
        prod.push(load_model_payload(Some(fake_wavenet(8)), None))
            .unwrap();
        run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);
        assert!(deferred.is_some());

        // A newer 4-ch model is queued → the parked one is superseded.
        prod.push(load_model_payload(Some(fake_wavenet(4)), None))
            .unwrap();
        structural_applied = 0;
        let (_, _, _, model_l, _) =
            run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);
        assert_eq!(
            model_l.as_ref().unwrap().channels(),
            4,
            "the queued latest model must win"
        );
        assert!(deferred.is_none());
        assert!(flags.check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED));
        assert!(flags.check_flag(RT_STATUS_STRUCTURAL_DEFERRED));
    }

    /// T2.5 acceptance (F-RB-011): a closed-loop producer thread (push without
    /// yield) must never make the RT callback exceed its fixed budget or starve
    /// the audio processing — every simulated callback completes.
    #[test]
    fn receive_commands_soak_aggressive_producer_no_starvation() {
        const CALLBACKS: usize = 2_000;

        let (mut prod, cons) = rtrb::RingBuffer::<ParamPayload>::new(64);
        let flags = Arc::new(RtStatusFlags::new());
        let stop = Arc::new(AtomicBool::new(false));

        // Pre-fill beyond the budget so the first callback deterministically
        // overflows it (backlog flag) while the producer thread refills.
        for i in 0..40u32 {
            prod.push(ParamPayload::InputGain(0.1 + i as f32 * 0.01))
                .unwrap();
        }

        let stop_producer = Arc::clone(&stop);
        let producer = std::thread::spawn(move || {
            let mut i = 0u32;
            while !stop_producer.load(Ordering::Acquire) {
                if prod
                    .push(ParamPayload::InputGain(0.5 + (i % 100) as f32 * 0.001))
                    .is_ok()
                {
                    i += 1;
                }
            }
        });

        let mut deferred = None;
        let mut structural_applied;
        let mut cons = cons;
        let mut callbacks_ran = 0usize;
        while callbacks_ran < CALLBACKS {
            structural_applied = 0;
            run_receive_commands(&mut cons, &mut deferred, &mut structural_applied, &flags);
            callbacks_ran += 1;
        }
        assert_eq!(
            callbacks_ran, CALLBACKS,
            "the audio callback must never starve under an aggressive producer"
        );
        assert!(
            flags.check_flag(RT_STATUS_PARAM_QUEUE_BACKLOG),
            "the closed-loop producer must overflow the per-callback budget at least once"
        );

        stop.store(true, Ordering::Release);
        producer.join().unwrap();
    }

    /// T2.5 structural budget: with multiple current-generation pairs queued,
    /// exactly one structural swap applies per callback and the obsolete
    /// intermediate pairs are coalesced to the GC cascade (latest-wins).
    #[test]
    fn drain_slimmable_budget_applies_one_and_coalesces_backlog() {
        let (mut prod, cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(4);
        let mut rx = Some(cons);
        let mut model_l = Some(fake_wavenet(4));
        let mut model_r = Some(fake_wavenet(4));
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::<GcItem>::new(64);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();
        flags
            .requested_slimmable_generation
            .store(1, Ordering::Release);

        // Backlog: three current-generation pairs queued at once.
        prod.push(make_pair(1, 8, true)).unwrap();
        prod.push(make_pair(1, 4, true)).unwrap();
        prod.push(make_pair(1, 16, true)).unwrap();

        let mut deferred = None;
        let mut structural_applied = 0usize;
        drain_slimmable_models(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        // Only the latest pair was installed atomically.
        assert_eq!(model_l.as_ref().unwrap().channels(), 16);
        assert_eq!(model_r.as_ref().unwrap().channels(), 16);
        assert_eq!(
            structural_applied, 1,
            "at most one structural swap per callback"
        );
        assert!(deferred.is_none());
        assert!(rx.as_ref().unwrap().is_empty());
        assert!(flags.check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED));

        // GC: 2 superseded pairs (L+R each) + the replaced active pair = 6.
        let mut models = 0usize;
        while let Ok(item) = gc_c.pop() {
            assert_matches!(item, GcItem::Model(_));
            models += 1;
        }
        assert_eq!(models, 6);
    }

    /// When the shared structural budget is exhausted, the slimmable pair is
    /// parked and installed by the next callback (fresh budget).
    #[test]
    fn drain_slimmable_parked_when_budget_exhausted_resolved_next_callback() {
        let (mut prod, cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(4);
        let mut rx = Some(cons);
        let mut model_l = Some(fake_wavenet(4));
        let mut model_r = Some(fake_wavenet(4));
        let (mut gc_p, mut _gc_c) = rtrb::RingBuffer::<GcItem>::new(16);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();
        flags
            .requested_slimmable_generation
            .store(1, Ordering::Release);

        prod.push(make_pair(1, 8, true)).unwrap();

        let mut deferred = None;
        let mut structural_applied = 1usize; // another swap applied earlier
        drain_slimmable_models(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );
        assert_eq!(model_l.as_ref().unwrap().channels(), 4, "not installed");
        assert!(deferred.is_some(), "pair must be parked");
        assert!(flags.check_flag(RT_STATUS_STRUCTURAL_DEFERRED));

        // Next callback: fresh budget → the parked pair is installed.
        structural_applied = 0;
        drain_slimmable_models(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut model_l,
            &mut model_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );
        assert_eq!(model_l.as_ref().unwrap().channels(), 8);
        assert_eq!(model_r.as_ref().unwrap().channels(), 8);
        assert!(deferred.is_none());
    }

    /// T2.5 structural budget for OS engines: one pair applied per callback,
    /// obsolete intermediate pairs coalesced to GC (latest-wins).
    #[test]
    fn drain_os_budget_applies_one_and_coalesces_backlog() {
        let (mut prod, cons) = rtrb::RingBuffer::<Box<OsEnginePair>>::new(4);
        let mut rx = Some(cons);
        let mut os_l = Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap());
        let mut os_r = Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap());
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::<GcItem>::new(64);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        let pair = |f: OversampleFactor| {
            Box::new(OsEnginePair {
                l: Box::new(OversampleEngine::new(f, 64).unwrap()),
                r: Box::new(OversampleEngine::new(f, 64).unwrap()),
            })
        };
        prod.push(pair(OversampleFactor::X2)).unwrap();
        prod.push(pair(OversampleFactor::X4)).unwrap();

        let mut deferred = None;
        let mut structural_applied = 0usize;
        drain_os_engines(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut os_l,
            &mut os_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert_eq!(os_l.factor(), OversampleFactor::X4);
        assert_eq!(os_r.factor(), OversampleFactor::X4);
        assert_eq!(structural_applied, 1);
        assert!(deferred.is_none());
        assert!(rx.as_ref().unwrap().is_empty());
        assert!(flags.check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED));

        // GC: 1 superseded pair (2 engines) + 2 replaced active engines = 4.
        let mut engines = 0usize;
        while let Ok(item) = gc_c.pop() {
            assert_matches!(item, GcItem::Oversample(_));
            engines += 1;
        }
        assert_eq!(engines, 4);
    }

    /// OS engine pairs respect the shared structural budget: the excess is
    /// parked and applied by the next callback.
    #[test]
    fn drain_os_parked_when_budget_exhausted_resolved_next_callback() {
        let (mut prod, cons) = rtrb::RingBuffer::<Box<OsEnginePair>>::new(4);
        let mut rx = Some(cons);
        let mut os_l = Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap());
        let mut os_r = Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap());
        let (mut gc_p, mut _gc_c) = rtrb::RingBuffer::<GcItem>::new(16);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        prod.push(Box::new(OsEnginePair {
            l: Box::new(OversampleEngine::new(OversampleFactor::X2, 64).unwrap()),
            r: Box::new(OversampleEngine::new(OversampleFactor::X2, 64).unwrap()),
        }))
        .unwrap();

        let mut deferred = None;
        let mut structural_applied = 1usize;
        drain_os_engines(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut os_l,
            &mut os_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );
        assert_eq!(os_l.factor(), OversampleFactor::Off, "not installed");
        assert!(deferred.is_some());
        assert!(flags.check_flag(RT_STATUS_STRUCTURAL_DEFERRED));

        structural_applied = 0;
        drain_os_engines(
            &mut rx,
            &mut deferred,
            &mut structural_applied,
            &mut os_l,
            &mut os_r,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );
        assert_eq!(os_l.factor(), OversampleFactor::X2);
        assert_eq!(os_r.factor(), OversampleFactor::X2);
        assert!(deferred.is_none());
    }
}
