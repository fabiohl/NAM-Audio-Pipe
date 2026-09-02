// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(target_arch = "x86_64")]

//! Offline RT swap-stress harness.
//!
//! `RtSwapHarness` reproduces, with no PipeWire daemon, the exact drain
//! sequence the capture `process()` callback executes (mirror of
//! `capture/setup.rs` closure):
//!
//! 1. GC parking-lot flush to the SPSC GC channel,
//! 2. budgeted drains — resamplers, cab-sims, `receive_commands` (scalar
//!    params + `LoadModel`), slimmable-rebuild request, slimmable model pairs,
//!    oversampling engines,
//! 3. rate synchronization (`sync_rate`),
//! 4. gain-multiplier recompute on parameter change,
//! 5. the `RESAMP_SWAP_PENDING` fail-open rollback guard,
//! 6. the full DSP pipeline (`capture_dsp_pipeline`) with the exact
//!    `DspPipelineContext`/`DspBuffers` the real callback builds.
//!
//! It owns the complete RT mutable state ([`CaptureState`]), every SPSC channel
//! (producer + consumer), the GC cascade artifacts (parking lot, dirty flag,
//! overflow buffer) and a `DspBridge` so the pipeline's writer is non-null.
//! Swap commands are injected through the producer face exactly as the main
//! thread does in production; `run_callback` then drains them inside a single
//! audio quantum, exactly as the RT thread does.
//!
//! The harness is **splittable** into its two production faces via
//! [`RtSwapHarness::into_parts`]:
//!
//! - [`SwapProducer`] — the main-thread producer face (the SPSC writers plus
//!   the shared rate signal). It counts every push (`attempted`/`enqueued`/
//!   `dropped`) so a concurrency-throughput harness can account for command
//!   loss through the bounded production ring.
//! - [`SwapRtSide`] — the RT side (consumers, `CaptureState`, GC cascade, DSP
//!   bridge). Its `run_callback_accounted` returns the per-quantum swap
//!   accounting (`applied`/`pops`/`commands_remaining`).
//!
//! This is the substrate for concurrency/soak and zero-allocation heap-audit
//! gates and for production-SPSC throughput measurement (no global mutex on the
//! measured path). It is compiled only under `feature = "testing"`.

use crate::standalone::pw_host::capture::state::CaptureState;
use crate::standalone::pw_host::rt_callback::{
    drain_cabsims, drain_os_engines, drain_resamplers, drain_slimmable_models, receive_commands,
    sync_rate, try_slimmable_rebuild,
};
use crate::standalone::rt_setup::compute_gain_multipliers;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{
    CabSimSwapPayload, GcItem, GcOverflowBuffer, ParamPayload, ResamplerSwapPayload, RtStatusFlags,
    SlimModelPair, setup_spsc,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::gate::GateParams;
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::{
    BridgeBuffer, BridgeRef, DspBridge, DspBridgeReader, DspBridgeWriter, DspBuffers,
    DspPipelineContext, MAX_RESAMP_BUF, capture_dsp_pipeline_streaming,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::math::dsp::gain_lut::{GainLUT, get_gain_lut};
use neural_amp_modeler_rs::models::StaticModel;

use rtrb::{Consumer, Producer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Maximum oversampled buffer size: `MAX_RESAMP_BUF × 4` (for X4 oversampling).
const MAX_OS_BUF: usize = MAX_RESAMP_BUF * 4;

/// Per-quantum swap accounting returned by [`SwapRtSide::run_callback_accounted`].
///
/// The concurrency-throughput harness records, per callback, how many
/// structural swaps were actually applied, how many payloads the drains
/// consumed (param vs structural channels) and how many commands are still
/// queued/parked after the quantum — the full swap accounting required by the
/// contract. The producer-side attempt counters (`attempted`/`enqueued`/`dropped`)
/// live in [`SwapProducer`].
#[derive(Debug, Clone, Copy, Default)]
pub struct CallbackAccounting {
    /// Valid output frames produced by the DSP pipeline (`0` when the quantum
    /// was skipped, e.g. a resampler swap is pending).
    pub n_pw: usize,
    /// Structural swaps installed during this quantum (shared structural budget).
    pub structural_applied: usize,
    /// `ParamPayload` payloads popped from the param SPSC this quantum.
    pub param_pops: usize,
    /// Structural payloads popped from the resampler/cabsim/slimmable/OS SPSC
    /// channels this quantum. Ring-pop lower bound: payloads parked in
    /// deferred slots are resolved without a ring pop and are reflected in
    /// `structural_applied` instead.
    pub structural_pops: usize,
    /// Commands still queued or parked after this quantum ([`SwapRtSide::commands_pending`]).
    pub commands_remaining: usize,
}

/// Main-thread producer face of the swap harness (production SPSC writers).
///
/// This handle is movable to a separate producer thread with **no global
/// mutex** — it mirrors the production single-writer main thread, pushing
/// commands through the same bounded ring buffers the real `process()`
/// callback drains. Every push is counted (`attempted`/`enqueued`/`dropped`)
/// so the throughput harness can prove full swap accounting.
pub struct SwapProducer {
    param_producer: Producer<ParamPayload>,
    resampler_producer: Producer<Box<ResamplerSwapPayload>>,
    cabsim_producer: Producer<Box<CabSimSwapPayload>>,
    slimmable_producer: Producer<Box<SlimModelPair>>,
    os_producer: Producer<Box<OsEnginePair>>,
    shared_target_rate: Arc<AtomicU32>,
    rt_status: Arc<RtStatusFlags>,
    /// Total push attempts (every `push_*` call).
    attempted: AtomicU64,
    /// Successful pushes (command entered the SPSC ring).
    enqueued: AtomicU64,
    /// Rejected pushes (ring full — the bounded channel dropped the command).
    dropped: AtomicU64,
}

impl SwapProducer {
    fn count_push<T>(&self, result: Result<(), rtrb::PushError<T>>) {
        self.attempted.fetch_add(1, Ordering::Relaxed);
        if result.is_ok() {
            self.enqueued.fetch_add(1, Ordering::Relaxed);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Pushes a `LoadModel` command (both channels transported atomically).
    pub fn push_load_model(
        &mut self,
        model_l: Option<Box<StaticModel>>,
        model_r: Option<Box<StaticModel>>,
        input_mult_adj: f32,
        output_mult_adj: f32,
        sample_rate: u32,
    ) {
        let result = self.param_producer.push(ParamPayload::LoadModel {
            model_l,
            model_r,
            input_mult_adj,
            output_mult_adj,
            sample_rate,
        });
        self.count_push(result);
    }

    /// Pushes a scalar input-gain command (latest-wins coalescing).
    pub fn push_input_gain(&mut self, mult: f32) {
        let result = self.param_producer.push(ParamPayload::InputGain(mult));
        self.count_push(result);
    }

    /// Pushes a scalar output-gain command (latest-wins coalescing).
    pub fn push_output_gain(&mut self, mult: f32) {
        let result = self.param_producer.push(ParamPayload::OutputGain(mult));
        self.count_push(result);
    }

    /// Pushes a gate-configuration command.
    pub fn push_gate(&mut self, params: GateParams) {
        let result = self.param_producer.push(ParamPayload::GateConfig(params));
        self.count_push(result);
    }

    /// Pushes an atomic slimmable L/R pair.
    pub fn push_slimmable(
        &mut self,
        generation: u64,
        channels: usize,
        l: Box<StaticModel>,
        r: Option<Box<StaticModel>>,
    ) {
        let pair = Box::new(SlimModelPair {
            generation,
            channels,
            l: Some(l),
            r,
        });
        self.attempted.fetch_add(1, Ordering::Relaxed);
        if self.slimmable_producer.push(pair).is_ok() {
            self.enqueued.fetch_add(1, Ordering::Relaxed);
        } else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Pushes a cab-sim pair with current requested_cabsim_generation; `None`
    /// clears/bypasses the cab-sim.
    pub fn push_cabsim(&mut self, pair: Option<Box<CabSimPair>>) {
        let generation = self
            .rt_status
            .requested_cabsim_generation
            .load(Ordering::Acquire);
        self.push_cabsim_with_gen(generation, pair);
    }

    /// Pushes a cab-sim pair with explicit generation timestamp.
    pub fn push_cabsim_with_gen(&mut self, generation: u64, pair: Option<Box<CabSimPair>>) {
        let result = self
            .cabsim_producer
            .push(Box::new(CabSimSwapPayload { generation, pair }));
        self.count_push(result);
    }

    /// Pushes an oversampling engine pair stamped with current requested OS generation.
    pub fn push_os_pair(&mut self, l: OversampleEngine, r: OversampleEngine) {
        let generation = self
            .rt_status
            .requested_os_generation
            .load(Ordering::Acquire);
        self.push_os_with_gen(generation, l, r);
    }

    /// Pushes an oversampling engine pair with explicit generation timestamp.
    pub fn push_os_with_gen(&mut self, generation: u64, l: OversampleEngine, r: OversampleEngine) {
        let result = self.os_producer.push(Box::new(OsEnginePair {
            generation,
            l: Box::new(l),
            r: Box::new(r),
        }));
        self.count_push(result);
    }

    /// Requests a resampler renegotiation and delivers the rebuilt resampler.
    ///
    /// Mirrors the main-thread protocol: the RT side raises
    /// `RT_STATUS_RESAMP_SWAP_PENDING` (+ generation bump via `sync_rate`),
    /// this pushes a generation-stamped envelope, and the next `run_callback`
    /// drains and installs it, clearing the pending flag.
    pub fn request_resampler_swap(&mut self, host_rate: u32, nam_rate: u32) -> anyhow::Result<()> {
        let generation = self
            .rt_status
            .requested_rate_generation
            .load(Ordering::Acquire);
        let resampler = Box::new(NamResampler::new(host_rate, nam_rate, 2048)?);
        let stream = Box::new(StreamingResampleBuffer::new(
            host_rate,
            nam_rate,
            MAX_RESAMP_BUF,
        )?);
        let result = self.resampler_producer.push(Box::new(ResamplerSwapPayload {
            generation,
            resampler,
            stream,
        }));
        self.count_push(result);
        Ok(())
    }

    /// Publishes a detected host rate so `sync_rate` requests a rebuild.
    pub fn publish_host_rate(&mut self, rate: u32) {
        self.shared_target_rate.store(rate, Ordering::Release);
    }

    /// Total push attempts made through this producer face.
    pub fn attempted(&self) -> u64 {
        self.attempted.load(Ordering::Relaxed)
    }

    /// Successful pushes (commands enqueued into the SPSC rings).
    pub fn enqueued(&self) -> u64 {
        self.enqueued.load(Ordering::Relaxed)
    }

    /// Rejected pushes (ring full — bounded channel dropped the command).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// RT side of the split harness: the complete RT mutable state plus the SPSC
/// consumers and the GC cascade — exclusively owned by the audio thread.
pub struct SwapRtSide {
    /// The complete RT mutable state (models, resampler, OS, cab-sim, gains,
    /// gate, hysteresis, adaptive, deferred slots, working buffers).
    pub state: CaptureState,
    param_consumer: Consumer<ParamPayload>,
    gc_producer: Producer<GcItem>,
    gc_consumer: Consumer<GcItem>,
    gc_overflow: Arc<GcOverflowBuffer>,
    resampler_consumer: Consumer<Box<ResamplerSwapPayload>>,
    cabsim_consumer: Consumer<Box<CabSimSwapPayload>>,
    parking_lot: [Option<GcItem>; 16],
    parking_lot_dirty: AtomicBool,
    rt_status: Arc<RtStatusFlags>,
    bridge: Box<DspBridge>,
    lut: &'static GainLUT,
    /// Last host rate applied by `run_callback`.
    current_host_rate: u32,
    /// `n_pw` of the most recent `run_callback` (valid output frames).
    last_n_pw: usize,
}

impl SwapRtSide {
    /// Executes one audio quantum: GC flush, all budgeted drains, rate sync,
    /// gain recompute, pending-resampler guard and the full DSP pipeline.
    ///
    /// `in_l`/`in_r` are the host input channels (processed in place by the
    /// pipeline); `n` is the frame count. Returns `n_pw` — the number of
    /// processed samples available in [`Self::out_l`]/[`Self::out_r`], or `0`
    /// when the callback was skipped (e.g. a resampler swap is in flight).
    ///
    /// This is the plain quantum used by the RT measurement gates (deadline,
    /// jitter, soak, endurance): the instruction profile mirrors the real
    /// callback exactly — no accounting reads. Use
    /// [`Self::run_callback_accounted`] only where the per-quantum swap
    /// accounting is actually consumed.
    ///
    /// Zero heap allocations: mirrors the real callback exactly.
    pub fn run_callback(&mut self, in_l: &mut [f32], in_r: &mut [f32], n: usize) -> usize {
        let rt_status = self.rt_status.clone();

        // 1. GC parking-lot flush (fast-path skip on clean lot).
        if self.parking_lot_dirty.load(Ordering::Acquire) {
            let mut any_remaining = false;
            for slot in self.parking_lot.iter_mut() {
                let Some(old) = slot.take() else { continue };
                if let Err(rtrb::PushError::Full(old_back)) = self.gc_producer.push(old) {
                    *slot = Some(old_back);
                    any_remaining = true;
                    break;
                }
            }
            if !any_remaining {
                self.parking_lot_dirty.store(false, Ordering::Release);
            }
        }

        // 2. Command budgeting: shared structural budget.
        let mut structural_applied = 0usize;

        // 3. Budgeted drains in production order.
        drain_resamplers(
            &mut self.resampler_consumer,
            &mut self.state.deferred_resampler,
            &mut structural_applied,
            &mut self.state.resampler,
            &mut self.state.stream,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
        );

        drain_cabsims(
            &mut self.cabsim_consumer,
            &mut self.state.deferred_cabsim,
            &mut structural_applied,
            &mut self.state.active_cabsim,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
        );

        let (param_changed, _param_pops) = receive_commands(
            &mut self.param_consumer,
            &mut self.state.deferred_model,
            &mut structural_applied,
            &mut self.state.model_input_mult_adj,
            &mut self.state.model_output_mult_adj,
            &mut self.state.current_nam_rate,
            &mut self.state.active_model_l,
            &mut self.state.active_model_r,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
            &mut self.state.user_input_gain_mult,
            &mut self.state.user_output_gain_mult,
            &mut self.state.gate_params,
            &mut self.state.threshold_open_sq,
            &mut self.state.threshold_close_sq,
            self.lut,
            &mut self.state.adaptive_compute,
        );

        try_slimmable_rebuild(&mut self.state.adaptive_compute, &rt_status);

        drain_slimmable_models(
            &mut self.state.slimmable_rx,
            &mut self.state.deferred_slimmable,
            &mut structural_applied,
            &mut self.state.active_model_l,
            &mut self.state.active_model_r,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
        );

        drain_os_engines(
            &mut self.state.os_rx,
            &mut self.state.deferred_os,
            &mut structural_applied,
            &mut self.state.os_l,
            &mut self.state.os_r,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
        );

        // 4. Rate synchronization.
        let current_host_rate = sync_rate(
            self.state.shared_target_rate.as_ref(),
            &self.state.resampler,
            self.state.current_nam_rate,
            &rt_status,
        );
        self.current_host_rate = current_host_rate;

        // 5. Gain recompute on parameter change.
        if param_changed {
            compute_gain_multipliers(
                self.state.user_input_gain_mult,
                self.state.user_output_gain_mult,
                self.state.model_input_mult_adj,
                self.state.model_output_mult_adj,
                &mut self.state.input_gain_mult,
                &mut self.state.output_gain_mult,
            );
        }

        // 6. Fail-open rollback guard.
        if rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING)
        {
            let failed_gen = rt_status
                .resampler_failed_generation
                .load(Ordering::Acquire);
            let requested_gen = rt_status.requested_rate_generation.load(Ordering::Acquire);

            if failed_gen != 0 && failed_gen == requested_gen {
                rt_status
                    .applied_rate_generation
                    .store(requested_gen, Ordering::Release);
                rt_status
                    .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);
                rt_status
                    .resampler_failed_generation
                    .store(0, Ordering::Release);
            } else {
                // The quantum was skipped: no output was produced. Publish the
                // skip through `last_n_pw = 0` so `current_n_pw()`/`out_l()`
                // reflect the skip and zero-frame sentinels fire instead
                // of exposing stale pre-swap output.
                self.last_n_pw = 0;
                return 0;
            }
        }

        // 7. DSP pipeline.
        let n_pw = self.process_dsp(in_l, in_r, n, current_host_rate);
        self.state.frame_count = self.state.frame_count.wrapping_add(1);
        self.last_n_pw = n_pw;
        n_pw
    }

    /// Same quantum as [`Self::run_callback`] but also returns the per-quantum
    /// swap accounting. The drains run in the exact
    /// production order; `structural_pops` is the ring-pop lower bound across
    /// the four structural channels, `param_pops` is the exact scalar pops.
    ///
    /// Only used where the accounting is consumed (the production-SPSC
    /// throughput gate) — the RT measurement gates keep using the plain
    /// [`Self::run_callback`] so their instruction profile stays a mirror of
    /// the production callback.
    pub fn run_callback_accounted(
        &mut self,
        in_l: &mut [f32],
        in_r: &mut [f32],
        n: usize,
    ) -> CallbackAccounting {
        let rt_status = self.rt_status.clone();

        // 1. GC parking-lot flush (fast-path skip on clean lot).
        if self.parking_lot_dirty.load(Ordering::Acquire) {
            let mut any_remaining = false;
            for slot in self.parking_lot.iter_mut() {
                let Some(old) = slot.take() else { continue };
                if let Err(rtrb::PushError::Full(old_back)) = self.gc_producer.push(old) {
                    *slot = Some(old_back);
                    any_remaining = true;
                    break;
                }
            }
            if !any_remaining {
                self.parking_lot_dirty.store(false, Ordering::Release);
            }
        }

        // 2. Command budgeting: shared structural budget.
        let mut structural_applied = 0usize;

        // 3. Budgeted drains in production order. Ring-pop accounting is
        //    captured through queue-occupancy deltas (single-threaded read of
        //    the consumers within the quantum).
        let resamp_before = self.resampler_consumer.slots();
        drain_resamplers(
            &mut self.resampler_consumer,
            &mut self.state.deferred_resampler,
            &mut structural_applied,
            &mut self.state.resampler,
            &mut self.state.stream,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
        );
        let resamp_after = self.resampler_consumer.slots();

        let cabsim_before = self.cabsim_consumer.slots();
        drain_cabsims(
            &mut self.cabsim_consumer,
            &mut self.state.deferred_cabsim,
            &mut structural_applied,
            &mut self.state.active_cabsim,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
        );
        let cabsim_after = self.cabsim_consumer.slots();

        let (param_changed, param_pops) = receive_commands(
            &mut self.param_consumer,
            &mut self.state.deferred_model,
            &mut structural_applied,
            &mut self.state.model_input_mult_adj,
            &mut self.state.model_output_mult_adj,
            &mut self.state.current_nam_rate,
            &mut self.state.active_model_l,
            &mut self.state.active_model_r,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
            &mut self.state.user_input_gain_mult,
            &mut self.state.user_output_gain_mult,
            &mut self.state.gate_params,
            &mut self.state.threshold_open_sq,
            &mut self.state.threshold_close_sq,
            self.lut,
            &mut self.state.adaptive_compute,
        );

        try_slimmable_rebuild(&mut self.state.adaptive_compute, &rt_status);

        let slimmable_before = self.state.slimmable_rx.as_ref().map_or(0, Consumer::slots);
        drain_slimmable_models(
            &mut self.state.slimmable_rx,
            &mut self.state.deferred_slimmable,
            &mut structural_applied,
            &mut self.state.active_model_l,
            &mut self.state.active_model_r,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
        );
        let slimmable_after = self.state.slimmable_rx.as_ref().map_or(0, Consumer::slots);

        let os_before = self.state.os_rx.as_ref().map_or(0, Consumer::slots);
        drain_os_engines(
            &mut self.state.os_rx,
            &mut self.state.deferred_os,
            &mut structural_applied,
            &mut self.state.os_l,
            &mut self.state.os_r,
            &mut self.gc_producer,
            &mut self.parking_lot,
            &self.parking_lot_dirty,
            &self.gc_overflow,
            &rt_status,
        );
        let os_after = self.state.os_rx.as_ref().map_or(0, Consumer::slots);

        // 4. Rate synchronization.
        let current_host_rate = sync_rate(
            self.state.shared_target_rate.as_ref(),
            &self.state.resampler,
            self.state.current_nam_rate,
            &rt_status,
        );
        self.current_host_rate = current_host_rate;

        // 5. Gain recompute on parameter change.
        if param_changed {
            compute_gain_multipliers(
                self.state.user_input_gain_mult,
                self.state.user_output_gain_mult,
                self.state.model_input_mult_adj,
                self.state.model_output_mult_adj,
                &mut self.state.input_gain_mult,
                &mut self.state.output_gain_mult,
            );
        }

        // 6. Fail-open rollback guard.
        if rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING)
        {
            let failed_gen = rt_status
                .resampler_failed_generation
                .load(Ordering::Acquire);
            let requested_gen = rt_status.requested_rate_generation.load(Ordering::Acquire);

            if failed_gen != 0 && failed_gen == requested_gen {
                rt_status
                    .applied_rate_generation
                    .store(requested_gen, Ordering::Release);
                rt_status
                    .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);
                rt_status
                    .resampler_failed_generation
                    .store(0, Ordering::Release);
            } else {
                // The quantum was skipped: publish the skip through
                // `last_n_pw = 0` so `current_n_pw()`/`out_l()` reflect it and
                // zero-frame sentinels fire instead of exposing stale
                // pre-swap output.
                self.last_n_pw = 0;
                return CallbackAccounting {
                    n_pw: 0,
                    structural_applied,
                    param_pops,
                    structural_pops: resamp_before
                        .saturating_sub(resamp_after)
                        .saturating_add(cabsim_before.saturating_sub(cabsim_after))
                        .saturating_add(slimmable_before.saturating_sub(slimmable_after))
                        .saturating_add(os_before.saturating_sub(os_after)),
                    commands_remaining: self.commands_pending_count(),
                };
            }
        }

        // 7. DSP pipeline.
        let n_pw = self.process_dsp(in_l, in_r, n, current_host_rate);
        self.state.frame_count = self.state.frame_count.wrapping_add(1);
        self.last_n_pw = n_pw;

        CallbackAccounting {
            n_pw,
            structural_applied,
            param_pops,
            structural_pops: resamp_before
                .saturating_sub(resamp_after)
                .saturating_add(cabsim_before.saturating_sub(cabsim_after))
                .saturating_add(slimmable_before.saturating_sub(slimmable_after))
                .saturating_add(os_before.saturating_sub(os_after)),
            commands_remaining: self.commands_pending_count(),
        }
    }

    /// Runs `capture_dsp_pipeline` with the exact context/buffers the real
    /// capture callback builds (see `capture/setup.rs` lines 301-334).
    fn process_dsp(&mut self, in_l: &mut [f32], in_r: &mut [f32], n: usize, rate: u32) -> usize {
        // SAFETY: `self.bridge` is owned by the RT side and outlives this call;
        // the writer is used only within `capture_dsp_pipeline` below.
        let bridge_ref = unsafe { BridgeRef::new(&mut *self.bridge as *mut DspBridge) };
        let writer = DspBridgeWriter::from_ref(bridge_ref)
            .expect("harness bridge pointer is non-null by construction");

        let conv_pair = self
            .state
            .active_cabsim
            .as_deref_mut()
            .filter(|pair| pair.sample_rate == rate);

        let ctx = DspPipelineContext {
            resampler: &mut self.state.resampler,
            os_l: &mut self.state.os_l,
            os_r: &mut self.state.os_r,
            active_model_l: &mut self.state.active_model_l,
            active_model_r: &mut self.state.active_model_r,
            input_gain_mult: self.state.input_gain_mult,
            output_gain_mult: self.state.output_gain_mult,
            gate_params: &self.state.gate_params,
            silence_hysteresis: &mut self.state.silence_hysteresis,
            mono_hysteresis: &mut self.state.mono_hysteresis,
            threshold_open_sq: self.state.threshold_open_sq,
            threshold_close_sq: self.state.threshold_close_sq,
            process_mono: &mut self.state.process_mono,
            rt_status: &self.rt_status,
            adaptive: &mut self.state.adaptive_compute,
            bridge_writer: Some(writer),
            conv: None,
            conv_pair,
        };
        let bufs = DspBuffers {
            resamp_mid_l: &mut *self.state.resamp_mid_l,
            resamp_mid_r: &mut *self.state.resamp_mid_r,
            resamp_out_l: &mut *self.state.resamp_out_l,
            resamp_out_r: &mut *self.state.resamp_out_r,
            model_out_l: &mut *self.state.model_out_l,
            model_out_r: &mut *self.state.model_out_r,
            os_in_l: &mut *self.state.os_in_l,
            os_in_r: &mut *self.state.os_in_r,
            os_model_l: &mut *self.state.os_model_l,
            os_model_r: &mut *self.state.os_model_r,
            crossfade_scratch_l: &mut *self.state.xfd_scratch_l,
            crossfade_scratch_r: &mut *self.state.xfd_scratch_r,
        };
        capture_dsp_pipeline_streaming(in_l, in_r, n, ctx, &mut self.state.stream, bufs, rate)
    }

    /// Drains and drops the GC channel (main-thread side). Returns the number
    /// of retired items — a progress signal that swaps actually cascade.
    pub fn consume_gc(&mut self) -> usize {
        let mut count = 0;
        while self.gc_consumer.pop().is_ok() {
            count += 1;
        }
        count
    }

    pub fn out_l(&self) -> &[f32] {
        let n = self.last_n_pw.min(MAX_RESAMP_BUF);
        &self.state.resamp_out_l[..n]
    }

    pub fn out_r(&self) -> &[f32] {
        let n = self.last_n_pw.min(MAX_RESAMP_BUF);
        &self.state.resamp_out_r[..n]
    }

    pub fn current_n_pw(&self) -> usize {
        self.last_n_pw.min(MAX_RESAMP_BUF)
    }

    pub fn active_model_l(&self) -> Option<&StaticModel> {
        self.state.active_model_l.as_deref()
    }

    pub fn active_model_r(&self) -> Option<&StaticModel> {
        self.state.active_model_r.as_deref()
    }

    pub fn active_cabsim(&self) -> Option<&CabSimPair> {
        self.state.active_cabsim.as_deref()
    }

    pub fn process_mono(&self) -> bool {
        self.state.process_mono
    }

    pub fn input_gain_mult(&self) -> f32 {
        self.state.input_gain_mult
    }

    pub fn output_gain_mult(&self) -> f32 {
        self.state.output_gain_mult
    }

    pub fn rt_status(&self) -> &RtStatusFlags {
        &self.rt_status
    }

    pub fn frame_count(&self) -> u32 {
        self.state.frame_count
    }

    pub fn current_host_rate(&self) -> u32 {
        self.current_host_rate
    }

    pub fn gc_pending(&self) -> usize {
        self.parking_lot.iter().filter(|s| s.is_some()).count() + self.gc_consumer.slots()
    }

    /// Number of retired items still queued for off-RT disposal, including
    /// both the SPSC GC channel and the 16-slot parking lot.
    pub fn gc_in_flight(&self) -> usize {
        self.gc_consumer.slots() + self.gc_pending()
    }

    /// `true` while any structural/scalar command is still queued or parked —
    /// the RT callback has not yet finished absorbing the current burst.
    pub fn commands_pending(&self) -> bool {
        !self.param_consumer.is_empty()
            || !self.resampler_consumer.is_empty()
            || !self.cabsim_consumer.is_empty()
            || self
                .state
                .slimmable_rx
                .as_ref()
                .is_some_and(|c| !c.is_empty())
            || self.state.os_rx.as_ref().is_some_and(|c| !c.is_empty())
            || self.state.deferred_resampler.is_some()
            || self.state.deferred_cabsim.is_some()
            || self.state.deferred_model.is_some()
            || self.state.deferred_slimmable.is_some()
            || self.state.deferred_os.is_some()
            || self.parking_lot_dirty.load(Ordering::Acquire)
    }

    /// Number of commands still queued or parked (the numeric form of
    /// [`Self::commands_pending`]) — queued SPSC payloads plus deferred slots,
    /// plus the parking-lot dirty latch.
    pub fn commands_pending_count(&self) -> usize {
        self.param_consumer.slots()
            + self.resampler_consumer.slots()
            + self.cabsim_consumer.slots()
            + self.state.slimmable_rx.as_ref().map_or(0, Consumer::slots)
            + self.state.os_rx.as_ref().map_or(0, Consumer::slots)
            + usize::from(self.state.deferred_resampler.is_some())
            + usize::from(self.state.deferred_cabsim.is_some())
            + usize::from(self.state.deferred_model.is_some())
            + usize::from(self.state.deferred_slimmable.is_some())
            + usize::from(self.state.deferred_os.is_some())
            + usize::from(self.parking_lot_dirty.load(Ordering::Acquire))
    }

    /// The shared RT status flags (main-thread read face for gates).
    pub fn rt_status_arc(&self) -> Arc<RtStatusFlags> {
        Arc::clone(&self.rt_status)
    }

    /// Returns a playback-side reader for the harness's internal `DspBridge`.
    ///
    /// Allows soak/heap-audit tests to exercise the lock-free bridge read path
    /// (capture writes → playback reads) without a live PipeWire graph.
    /// The reader only performs atomic reads; the bridge outlives the harness.
    pub fn bridge_reader(&self) -> DspBridgeReader {
        // SAFETY: `self.bridge` is a heap-immortal boxed `DspBridge` that
        // outlives the RT side. `DspBridgeReader` only performs read accesses
        // synchronized by the bridge atomics, so forming a raw pointer from a
        // shared reference is sound.
        unsafe { DspBridgeReader::new(self.bridge.as_ref() as *const DspBridge as *mut DspBridge) }
    }

    /// Changes the model's active NAM rate (drives `current_nam_rate`).
    pub fn set_nam_rate(&mut self, rate: u32) {
        self.state.current_nam_rate = rate;
    }

    /// Maximum working-buffer length for input slices (the pipeline clamps to
    /// `MAX_RESAMP_BUF` anyway).
    pub const MAX_BUF: usize = MAX_RESAMP_BUF;

    /// Oversampled working-buffer length.
    pub const MAX_OS_BUF: usize = MAX_OS_BUF;
}

/// Offline RT swap-stress harness — full drain sequence + DSP in one quantum.
///
/// Thin wrapper over the two production faces ([`SwapProducer`] + [`SwapRtSide`]).
/// Single-threaded callers keep the single-handle API; concurrency-throughput
/// tests split it via [`RtSwapHarness::into_parts`] to remove the global mutex
/// from the measured path.
pub struct RtSwapHarness {
    producer: SwapProducer,
    rt: SwapRtSide,
}

impl RtSwapHarness {
    /// Builds a harness pre-configured for `host_rate` ↔ `nam_rate` (both
    /// rates already consistent, so the first `run_callback` processes
    /// immediately without a resampler renegotiation) with custom `gate_enabled`.
    pub fn new_with_gate(
        host_rate: u32,
        nam_rate: u32,
        gate_enabled: bool,
    ) -> anyhow::Result<Self> {
        let sys = SystemSnapshot::capture();
        let mut state = CaptureState::init(&sys, OversampleFactor::Off, gate_enabled);
        state.resampler = Box::new(NamResampler::new(host_rate, nam_rate, 2048)?);
        state.current_nam_rate = nam_rate;
        state.shared_target_rate = Arc::new(std::sync::atomic::AtomicU32::new(host_rate));

        let spsc = setup_spsc(neural_amp_modeler_rs::common::spsc::SPSC_CAPACITY);
        state.slimmable_rx = Some(spsc.slimmable_consumer);
        state.os_rx = Some(spsc.os_consumer);

        let bridge = Box::new(DspBridge {
            buffers: [BridgeBuffer::new(), BridgeBuffer::new()],
            active_read_idx: Default::default(),
            generation: Default::default(),
            consumed_gen: Default::default(),
            dropped_frames: Default::default(),
        });

        let producer = SwapProducer {
            param_producer: spsc.param_producer,
            resampler_producer: spsc.resampler_producer,
            cabsim_producer: spsc.cabsim_producer,
            slimmable_producer: spsc.slimmable_producer,
            os_producer: spsc.os_producer,
            shared_target_rate: Arc::clone(&state.shared_target_rate),
            rt_status: Arc::clone(&spsc.rt_status),
            attempted: AtomicU64::new(0),
            enqueued: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        };
        let rt = SwapRtSide {
            state,
            param_consumer: spsc.param_consumer,
            gc_producer: spsc.gc_producer,
            gc_consumer: spsc.gc_consumer,
            gc_overflow: spsc.gc_overflow,
            resampler_consumer: spsc.resampler_consumer,
            cabsim_consumer: spsc.cabsim_consumer,
            parking_lot: Default::default(),
            parking_lot_dirty: AtomicBool::new(false),
            rt_status: spsc.rt_status,
            bridge,
            lut: get_gain_lut(),
            current_host_rate: host_rate,
            last_n_pw: 0,
        };

        Ok(Self { producer, rt })
    }

    /// Builds a harness pre-configured for `host_rate` ↔ `nam_rate` (both
    /// rates already consistent, so the first `run_callback` processes
    /// immediately without a resampler renegotiation) with default gate enabled.
    pub fn new(host_rate: u32, nam_rate: u32) -> anyhow::Result<Self> {
        Self::new_with_gate(host_rate, nam_rate, true)
    }

    /// Splits the harness into its two production faces.
    ///
    /// The producer face moves to the main-thread side (single writer, exact
    /// production SPSC protocol); the RT side is exclusively owned by the audio
    /// thread. No global mutex guards the measured path.
    pub fn into_parts(self) -> (SwapProducer, SwapRtSide) {
        (self.producer, self.rt)
    }

    // ── Producer face (main-thread side) ────────────────────────────────────

    /// Pushes a `LoadModel` command (both channels transported atomically).
    pub fn push_load_model(
        &mut self,
        model_l: Option<Box<StaticModel>>,
        model_r: Option<Box<StaticModel>>,
        input_mult_adj: f32,
        output_mult_adj: f32,
        sample_rate: u32,
    ) {
        self.producer.push_load_model(
            model_l,
            model_r,
            input_mult_adj,
            output_mult_adj,
            sample_rate,
        );
    }

    /// Pushes a scalar input-gain command (latest-wins coalescing).
    pub fn push_input_gain(&mut self, mult: f32) {
        self.producer.push_input_gain(mult);
    }

    /// Pushes a scalar output-gain command (latest-wins coalescing).
    pub fn push_output_gain(&mut self, mult: f32) {
        self.producer.push_output_gain(mult);
    }

    /// Pushes a gate-configuration command.
    pub fn push_gate(&mut self, params: GateParams) {
        self.producer.push_gate(params);
    }

    /// Pushes an atomic slimmable L/R pair.
    pub fn push_slimmable(
        &mut self,
        generation: u64,
        channels: usize,
        l: Box<StaticModel>,
        r: Option<Box<StaticModel>>,
    ) {
        self.producer.push_slimmable(generation, channels, l, r);
    }

    /// Pushes a cab-sim pair with current requested_cabsim_generation; `None` clears/bypasses the cab-sim.
    pub fn push_cabsim(&mut self, pair: Option<Box<CabSimPair>>) {
        self.producer.push_cabsim(pair);
    }

    /// Pushes a cab-sim pair with explicit generation timestamp.
    pub fn push_cabsim_with_gen(&mut self, generation: u64, pair: Option<Box<CabSimPair>>) {
        self.producer.push_cabsim_with_gen(generation, pair);
    }

    /// Pushes an oversampling engine pair stamped with current requested OS generation.
    pub fn push_os_pair(&mut self, l: OversampleEngine, r: OversampleEngine) {
        self.producer.push_os_pair(l, r);
    }

    /// Pushes an oversampling engine pair with explicit generation timestamp.
    pub fn push_os_with_gen(&mut self, generation: u64, l: OversampleEngine, r: OversampleEngine) {
        self.producer.push_os_with_gen(generation, l, r);
    }

    /// Requests a resampler renegotiation and delivers the rebuilt resampler.
    pub fn request_resampler_swap(&mut self, host_rate: u32, nam_rate: u32) -> anyhow::Result<()> {
        self.producer.request_resampler_swap(host_rate, nam_rate)
    }

    /// Publishes a detected host rate so `sync_rate` requests a rebuild.
    pub fn publish_host_rate(&mut self, rate: u32) {
        self.producer.publish_host_rate(rate);
    }

    /// Changes the model's active NAM rate (drives `current_nam_rate`).
    pub fn set_nam_rate(&mut self, rate: u32) {
        self.rt.set_nam_rate(rate);
    }

    // ── RT face ─────────────────────────────────────────────────────────────

    /// Executes one audio quantum (see [`SwapRtSide::run_callback`]).
    pub fn run_callback(&mut self, in_l: &mut [f32], in_r: &mut [f32], n: usize) -> usize {
        self.rt.run_callback(in_l, in_r, n)
    }

    /// Drains and drops the GC channel (main-thread side). Returns the number
    /// of retired items — a progress signal that swaps actually cascade.
    pub fn consume_gc(&mut self) -> usize {
        self.rt.consume_gc()
    }

    // ── Accessors (RT state inspection for tests) ───────────────────────────

    pub fn out_l(&self) -> &[f32] {
        self.rt.out_l()
    }

    pub fn out_r(&self) -> &[f32] {
        self.rt.out_r()
    }

    pub fn current_n_pw(&self) -> usize {
        self.rt.current_n_pw()
    }

    pub fn active_model_l(&self) -> Option<&StaticModel> {
        self.rt.active_model_l()
    }

    pub fn active_model_r(&self) -> Option<&StaticModel> {
        self.rt.active_model_r()
    }

    pub fn active_cabsim(&self) -> Option<&CabSimPair> {
        self.rt.active_cabsim()
    }

    pub fn process_mono(&self) -> bool {
        self.rt.process_mono()
    }

    pub fn input_gain_mult(&self) -> f32 {
        self.rt.input_gain_mult()
    }

    pub fn output_gain_mult(&self) -> f32 {
        self.rt.output_gain_mult()
    }

    pub fn rt_status(&self) -> &RtStatusFlags {
        self.rt.rt_status()
    }

    pub fn frame_count(&self) -> u32 {
        self.rt.frame_count()
    }

    pub fn current_host_rate(&self) -> u32 {
        self.rt.current_host_rate()
    }

    pub fn gc_pending(&self) -> usize {
        self.rt.gc_pending()
    }

    /// Number of retired items still queued for off-RT disposal, including
    /// both the SPSC GC channel and the 16-slot parking lot.
    pub fn gc_in_flight(&self) -> usize {
        self.rt.gc_in_flight()
    }

    /// `true` while any structural/scalar command is still queued or parked —
    /// the RT callback has not yet finished absorbing the current burst.
    pub fn commands_pending(&self) -> bool {
        self.rt.commands_pending()
    }

    /// Maximum working-buffer length for input slices (the pipeline clamps to
    /// `MAX_RESAMP_BUF` anyway).
    pub const MAX_BUF: usize = MAX_RESAMP_BUF;

    /// Oversampled working-buffer length.
    pub const MAX_OS_BUF: usize = MAX_OS_BUF;

    /// The shared RT status flags (main-thread read face for gates).
    pub fn rt_status_arc(&self) -> Arc<RtStatusFlags> {
        self.rt.rt_status_arc()
    }

    /// Returns a playback-side reader for the harness's internal `DspBridge`.
    pub fn bridge_reader(&self) -> DspBridgeReader {
        self.rt.bridge_reader()
    }
}
