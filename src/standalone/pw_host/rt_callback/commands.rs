// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! COMMAND RECEPTION (SPSC Channel) — engine `RtSwapDrain` integration.
//! Processes commands from the command-line interface or control system (volume, model, noise gate).
//!
//! # Command Budgeting
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
//! # T9.5 (F-PERF-20) — canonical structural-swap scheduler
//!
//! The hand-rolled 3-phase protocol previously duplicated here (and in the
//! resampler/cabsim/slimmable/OS drains) was migrated to the engine's
//! canonical scheduler (`NeuralAmpModeler-rs/common/spsc/swap.rs`):
//!
//! - The four dedicated boxed channels (`Consumer<Box<P>>` for resampler /
//!   cab-sim / slimmable / oversample) instantiate [`RtSwapDrain`] directly
//!   with per-family [`RtSwapHandler`]s — the ring types match `SwapRing`
//!   exactly, so no reimplementation remains.
//! - The mixed `Consumer<ParamPayload>` ring adopts the contract: this module
//!   implements the canonical 3-phase protocol verbatim (Phase 0
//!   deferred-resolution → Phase 1 bounded drain with latest-wins coalescing →
//!   Phase 2 budgeted apply-or-park), preserving the historical tuning (1
//!   structural swap/callback shared budget, 16-pop scalar window, backlog
//!   flag). Contract adoption (not direct instantiation) because `ParamPayload`
//!   is carried unboxed — wrapping pops into `Box` would allocate on the RT
//!   path.
//!
//! ## Empirical Composite Bound
//!
//! Measured under continuous simultaneous saturation across all 5 RT drains
//! (resampler, cabsim, parameters, slimmable, OS):
//! // Measured: pops/callback p99=32, max=32 (nominal ceiling 48), duration p99=0.94 us (0.28% of 333 us deadline)
//! As the p99 drain execution time (0.94 µs) is far below 10% of the 333 µs deadline
//! at quantum=16 (33.3 µs threshold), the nominal ceiling of ~48 pops per callback
//! is safe without requiring an additional global shared drain budget.

use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, GcSink, ParamPayload, RT_STATUS_NEEDS_OS_REBUILD,
    RT_STATUS_PARAM_QUEUE_BACKLOG, RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED,
    RtStatusFlags, RtSwapDrain, RtSwapHandler, SlimModelPair, SwapBudget, SwapTunables,
};
use neural_amp_modeler_rs::dsp::adaptive::{AdaptiveCompute, SlimOverride};
use neural_amp_modeler_rs::dsp::gate::GateParams;
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine};
use neural_amp_modeler_rs::models::StaticModel;

use rtrb::Consumer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum number of structural swaps applied per audio callback, shared across
/// every RT swap drain (resampler, cab-sim, model pair, oversampling). Heavy
/// state movement is bounded to one transaction per audio quantum.
pub const STRUCTURAL_SWAPS_PER_CALLBACK: usize = 1;

/// Maximum number of payloads popped from a single structural SPSC channel per
/// audio callback (coalescing window). Bounds the drain loop even when the
/// producer refills the ring continuously. The channels have capacity 4, so
/// this window covers the full queue plus producer refills.
pub const STRUCTURAL_POPS_PER_CALLBACK: usize = 8;

/// Maximum number of `ParamPayload` commands consumed per audio callback —
/// the scalar parameter budget. Scalar commands inside the budget are
/// coalesced latest-wins; the excess is left in the ring for the next callback
/// and flagged via `RT_STATUS_PARAM_QUEUE_BACKLOG`.
pub const MAX_PARAM_BUDGET: usize = 16;

/// Raises `RT_STATUS_STRUCTURAL_DEFERRED` (+ monotonic counter, `Relaxed`) —
/// pipe-side mirror of the engine scheduler's internal `flag_deferred`.
#[inline(always)]
fn flag_structural_deferred(rt_status: &RtStatusFlags) {
    rt_status.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
    rt_status
        .structural_deferred_total
        .fetch_add(1, Ordering::Relaxed);
}

/// Raises `RT_STATUS_STRUCTURAL_SUPERSEDED` (+ monotonic counter, `Relaxed`) —
/// pipe-side mirror of the engine scheduler's internal `flag_superseded`.
#[inline(always)]
fn flag_structural_superseded(rt_status: &RtStatusFlags) {
    rt_status.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
    rt_status
        .structural_superseded_total
        .fetch_add(1, Ordering::Relaxed);
}

/// [`SwapTunables`] for the four dedicated structural swap drains
/// (resampler, cab-sim, slimmable, oversample): historical NAM-Audio-Pipe
/// tuning preserved through the T9.5 migration (1 shared swap/callback,
/// 8-pop coalescing window, no backlog flag — the pop-cap truncation flag
/// reports saturation instead).
pub(crate) const fn structural_swap_tunables() -> SwapTunables {
    SwapTunables {
        pops_per_callback: STRUCTURAL_POPS_PER_CALLBACK,
        swaps_per_callback: STRUCTURAL_SWAPS_PER_CALLBACK,
        backlog_flag: false,
    }
}

/// COMMAND RECEPTION (SPSC Channel)
/// Processes commands from the command-line interface or control system (volume, model, noise gate).
///
/// T9.5 contract adoption of the engine's canonical 3-phase protocol
/// ([`RtSwapDrain`], adopted verbatim; see the module docs): scalar parameters
/// are coalesced latest-wins inside the [`MAX_PARAM_BUDGET`] window (handler
/// side — the canonical "handler owns scalar-coalescing policy" pattern, with
/// the flush after the drain), `LoadModel` structural swaps obey the shared
/// [`STRUCTURAL_SWAPS_PER_CALLBACK`] budget and park in `deferred` when
/// exhausted (the budget-exhausted structural head stays queued — FIFO
/// intact), and a non-empty channel after the budget raises
/// `RT_STATUS_PARAM_QUEUE_BACKLOG`.
///
/// Returns `(param_changed, pops)` — the coalesced-parameter signal plus the
/// exact number of `ParamPayload` payloads consumed in this callback.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn receive_commands(
    consumer: &mut rtrb::Consumer<ParamPayload>,
    deferred: &mut Option<ParamPayload>,
    budget: &mut SwapBudget,
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
    // Handler-side coalescing locals (canonical "handler owns scalar-coalescing
    // policy" pattern; flushed after the drain).
    let mut pending_input_gain: Option<f32> = None;
    let mut pending_output_gain: Option<f32> = None;
    let mut pending_gate: Option<GateParams> = None;
    let mut pending_slim_override: Option<SlimOverride> = None;
    let mut param_changed = false;

    let mut gc = GcSink {
        producer: gc_producer,
        parking_lot,
        overflow: gc_overflow_for_process,
        rt_status: rt_status_for_process,
        parking_lot_dirty: Some(parking_lot_dirty),
    };

    // Phase 0 — resolve a `LoadModel` deferred by the previous callback. It is
    // causally before everything still in the ring, so it applies first; a
    // newer `LoadModel` already queued supersedes it (latest-wins coalescing).
    // `LoadModel` carries no request generation — it is never stale.
    if let Some(payload) = deferred.take() {
        let head_same_key = consumer
            .peek()
            .is_ok_and(|head| matches!(head, ParamPayload::LoadModel { .. }));
        if head_same_key {
            // A newer same-key command is already queued (latest-wins): the
            // parked command is obsolete and cascades to GC.
            discard_load_model(payload, &mut gc, rt_status_for_process);
        } else if budget.can_apply() {
            install_load_model(
                payload,
                model_input_mult_adj,
                model_output_mult_adj,
                current_nam_rate,
                active_model_l,
                active_model_r,
                &mut gc,
                rt_status_for_process,
                adaptive,
            );
            budget.consume();
            param_changed = true;
        } else {
            // Budget exhausted and nothing newer queued: re-park.
            *deferred = Some(payload);
            flag_structural_deferred(rt_status_for_process);
        }
    }

    // Phase 1 — bounded drain with latest-wins coalescing (canonical
    // `bounded_drain` for the mixed ring): light scalars install inline into
    // handler locals (never parked), `LoadModel` payloads collapse into the
    // single window candidate (same-key coalescing is free and proceeds even
    // when the budget is exhausted). The budget-exhausted structural head
    // stays queued (FIFO intact) and the drain stops — everything behind it is
    // causally after it.
    let mut candidate: Option<ParamPayload> = None;
    let mut pops = 0usize;
    while pops < MAX_PARAM_BUDGET {
        // Classify the head without detaching it from the ring (rtrb peek is
        // `Result`-based; the ring is never empty-mid-loop for a parked
        // payload — an unresolvable structural head stays queued).
        let Ok(head) = consumer.peek() else {
            break;
        };
        if !matches!(head, ParamPayload::LoadModel { .. }) {
            // Light scalar: applies inline, never parked, never coalesced by
            // the scheduler.
            if let Ok(payload) = consumer.pop() {
                pops += 1;
                match payload {
                    ParamPayload::InputGain(mult) => pending_input_gain = Some(mult),
                    ParamPayload::OutputGain(mult) => pending_output_gain = Some(mult),
                    ParamPayload::GateConfig(params) => pending_gate = Some(params),
                    ParamPayload::SlimOverride(ov) => pending_slim_override = Some(ov),
                    // Lightweight request (atomics only): the actual engine
                    // swap is budgeted when the delivered engines are drained
                    // in `drain_os_engines`. Latest value wins by overwrite.
                    ParamPayload::SetOversample(factor) => {
                        rt_status_for_process
                            .requested_os_factor
                            .store(factor.to_f32() as u32, Ordering::Relaxed);
                        rt_status_for_process
                            .requested_os_generation
                            .fetch_add(1, Ordering::Release);
                        rt_status_for_process.set_flag_release(RT_STATUS_NEEDS_OS_REBUILD);
                    }
                    ParamPayload::LoadModel { .. } => unreachable!("classified above"),
                }
            }
            continue;
        }
        // Structural: same-key coalescing happens even when the budget is
        // exhausted (free coalescing — no budget, no slot).
        if candidate.is_some() {
            if let Ok(payload) = consumer.pop() {
                pops += 1;
                // An intermediate current `LoadModel` is obsolete — its boxes
                // are discarded to the GC cascade (latest-wins).
                if let Some(older) = candidate.replace(payload) {
                    discard_load_model(older, &mut gc, rt_status_for_process);
                }
            }
            continue;
        }
        if !budget.can_apply() {
            // Budget exhausted with a structural at the head: it is deferred
            // (stays queued, FIFO intact) and the drain stops.
            flag_structural_deferred(rt_status_for_process);
            break;
        }
        // Budget available: pop and make this payload the single candidate.
        // (The canonical flush-install arm for a pending different-key
        // candidate is unreachable in this single-key family — structurally
        // faithful rather than special-cased away.)
        if let Ok(payload) = consumer.pop() {
            pops += 1;
            candidate = Some(payload);
        }
    }

    // End-of-drain telemetry: the channel still held commands after the fixed
    // budget — the remainder is drained by the next callback.
    if !consumer.is_empty() {
        rt_status_for_process.set_flag(RT_STATUS_PARAM_QUEUE_BACKLOG);
    }

    // Phase 2 — resolve the window candidate under the shared budget; a
    // budget-exhausted candidate parks in the deferred slot for the next
    // callback (latest-wins against anything queued behind it).
    if let Some(payload) = candidate {
        if budget.can_apply() {
            install_load_model(
                payload,
                model_input_mult_adj,
                model_output_mult_adj,
                current_nam_rate,
                active_model_l,
                active_model_r,
                &mut gc,
                rt_status_for_process,
                adaptive,
            );
            budget.consume();
            param_changed = true;
        } else {
            *deferred = Some(payload);
            flag_structural_deferred(rt_status_for_process);
        }
    }

    // after_drain — apply the coalesced scalar parameters (latest-wins).
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
    gc: &mut GcSink<'_>,
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
            gc.retire(GcItem::Model(m));
        }
    }
}

/// Discards an obsolete `LoadModel` payload to the GC cascade (command
/// coalescing — latest-wins). Its model boxes drop off-RT; the payload is
/// never applied and never dropped on the audio thread.
#[cold]
fn discard_load_model(
    payload: ParamPayload,
    gc: &mut GcSink<'_>,
    rt_status_for_process: &Arc<RtStatusFlags>,
) {
    let ParamPayload::LoadModel {
        model_l, model_r, ..
    } = payload
    else {
        unreachable!("only LoadModel reaches discard_load_model");
    };
    for model in [model_l, model_r].into_iter().flatten() {
        gc.retire(GcItem::Model(model));
    }
    flag_structural_superseded(rt_status_for_process);
}

/// Signals the main thread to rebuild WaveNet models with a reduced channel count.
///
/// The audio thread ONLY sets the atomic flag, target channel count, and the
/// rebuild generation. All allocation, prewarm, and mmap happen on the main
/// thread. The generation is bumped with `Release` before the flag is armed so
/// the main thread's `Acquire` capture observes the full request ordering pattern
/// and the RT drain can discard stale in-flight pairs.
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
        install_os_pair(payload, self.os_l, self.os_r, gc, self.rt_status);
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
        rt_status,
    };
    drain.drain(&mut handler, budget, &mut gc);
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
    gc.retire(GcItem::OsEnginePair(pair));
}

/// Discards an obsolete OS engine pair to the GC cascade as a single moved `Box<OsEnginePair>`.
#[cold]
fn discard_os_pair(pair: Box<OsEnginePair>, gc: &mut GcSink<'_>) {
    gc.retire(GcItem::OsEnginePair(pair));
}

#[cfg(test)]
#[path = "commands_test.rs"]
mod commands_test;
