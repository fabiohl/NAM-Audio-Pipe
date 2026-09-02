// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.2. COMMAND RECEPTION (SPSC Channel)
//! Processes commands from the command-line interface or control system (volume, model, noise gate).
//!
//! # Command Budgeting (F-RB-011 / T2.5 / T1.5)
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
//!
//! ## Empirical Composite Bound (T1.5)
//!
//! Measured under continuous simultaneous saturation across all 5 RT drains
//! (resampler, cabsim, parameters, slimmable, OS):
//! // Medido: pops/callback p99=32, max=32 (teto nominal 48), duracao p99=0.94 us (0.28% do deadline de 333 us)
//! As the p99 drain execution time (0.94 µs) is far below 10% of the 333 µs deadline
//! at quantum=16 (33.3 µs threshold), the nominal ceiling of ~48 pops per callback
//! is safe without requiring an additional global shared drain budget.

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
///
/// Returns `(param_changed, pops)` — the coalesced-parameter signal plus the
/// exact number of `ParamPayload` payloads consumed in this callback (T5.3
/// swap accounting).
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
) -> (bool, usize) {
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
                rt_status_for_process
                    .requested_os_generation
                    .fetch_add(1, Ordering::Release);
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

    (param_changed, pops)
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
            // Stale while parked: discard whole to GC — never installed (F-RB-005).
            discard_pair_whole(
                pending,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
        } else if head_is_current {
            // A newer same-generation pair is already queued (latest-wins).
            discard_pair_whole(
                pending,
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
                pending,
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
            // to the GC cascade.
            discard_pair_whole(
                pair,
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
                older,
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
                pair,
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
                older,
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
/// R models (if any) are swapped into the envelope and cascade to GC as a single
/// moved `Box<SlimModelPair>`.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
fn install_pair(
    mut pair: Box<SlimModelPair>,
    active_model_l: &mut Option<Box<StaticModel>>,
    active_model_r: &mut Option<Box<StaticModel>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    std::mem::swap(&mut pair.l, active_model_l);
    if pair.r.is_some() {
        std::mem::swap(&mut pair.r, active_model_r);
    }
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::SlimModelPair(pair)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
}

/// Discards a whole pair to the GC cascade as a single moved `Box<SlimModelPair>`
/// — never applied (F-RB-005).
#[inline(always)]
fn discard_pair_whole(
    pair: Box<SlimModelPair>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::SlimModelPair(pair)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
}

/// Drains oversampling engines delivered by the main thread via SPSC.
/// Swaps both L and R engines and sends the obsolete envelope to the GC cascade.
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

    let current_gen = rt_status.requested_os_generation.load(Ordering::Acquire);

    // Phase 0 — resolve an engine pair deferred by the previous callback.
    if let Some(pending) = deferred.take() {
        if pending.generation != current_gen {
            // Superseded while parked in the deferred slot: discard to GC cascade
            // without applying or clearing the pending bit (F-RB-005).
            discard_os_pair(
                pending,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
        } else if rx.peek().is_ok_and(|head| head.generation == current_gen) {
            // A newer pair of the same generation is already queued (latest-wins):
            // the deferred pair is obsolete and its envelope cascades to GC.
            discard_os_pair(
                pending,
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
                pending,
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

    // Phase 1 — bounded drain with coalescing and stale-generation filtering (F-RB-011 / T2.5).
    let mut candidate: Option<Box<OsEnginePair>> = None;
    let mut pops = 0usize;
    while pops < STRUCTURAL_POPS_PER_CALLBACK {
        let Some(pair) = rx.pop().ok() else {
            break;
        };
        pops += 1;
        if pair.generation != current_gen {
            // Stale rebuild guard (F-RB-005 / T2.3): a newer oversample change
            // superseded this pair before delivery. Cascade the entire envelope
            // directly to GC without touching the active engines.
            discard_os_pair(
                pair,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow,
                rt_status,
            );
            continue;
        }
        if let Some(older) = candidate.replace(pair) {
            discard_os_pair(
                older,
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
                pair,
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
                older,
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

/// Swaps both active OS engines into the envelope and cascades the replaced pair to GC.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
fn install_os_pair(
    mut pair: Box<OsEnginePair>,
    os_l: &mut Box<OversampleEngine>,
    os_r: &mut Box<OversampleEngine>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    rt_status
        .applied_os_generation
        .store(pair.generation, Ordering::Release);
    std::mem::swap(&mut pair.l, os_l);
    std::mem::swap(&mut pair.r, os_r);
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::OsEnginePair(pair)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
}

/// Discards an obsolete OS engine pair to the GC cascade as a single moved `Box<OsEnginePair>`.
#[inline(always)]
fn discard_os_pair(
    pair: Box<OsEnginePair>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
) {
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::OsEnginePair(pair)),
        gc_producer,
        parking_lot,
        gc_overflow,
        rt_status,
    );
}

#[cfg(test)]
#[path = "commands_test.rs"]
mod commands_test;
