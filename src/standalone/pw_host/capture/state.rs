// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! DSP state aggregated from the preamble of `setup_capture_stream`.
//!
//! Groups all locals initialized before the RT `process()` closure.
//! Large working buffers (>32 KB each) are boxed on the heap during `init`,
//! which runs in the main thread. The `Box` pointers remain on the stack,
//! preserving RT safety by avoiding dynamic allocation in the audio thread.

use crate::standalone::cli::GateConfig;
use crate::standalone::pw_host::StreamStatusFlags;
use crate::standalone::pw_host::rt_callback::{
    CabSimSwapDrain, OsSwapDrain, ResamplerSwapDrain, SlimmableSwapDrain,
};
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::common::params::AdaptiveComputeMode;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, ParamPayload};
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::math::dsp::gain_lut;
use rtrb::{Consumer, Producer};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

/// RT-side SPSC handles consumed by the capture `process()` closure.
///
/// Owned (heap-allocated) by `run_pipewire_host` and reached from the RT
/// callback through a raw pointer — never moved into the stream closures — so
/// the channels survive a bounded reconnect cycle. The four dedicated swap
/// channels live inside [`CaptureState`]'s engine `RtSwapDrain`s
/// (`resampler_drain` / `cabsim_drain` / `slimmable_drain` / `os_drain`) —
/// each drain owns its ring consumer and its deferred slot; the main
/// thread only touches its own separate `gc_overflow` handle (an [`Arc`]
/// clone) during the control loop, so there is never concurrent aliasing of
/// this struct between the RT callback (exclusive `&mut`) and the main
/// thread.
pub struct RtHostChannels {
    /// CLI→DSP parameter channel consumer (gain, model, etc.).
    pub param_consumer: Consumer<ParamPayload>,
    /// RT→main GC recycle producer (drop-delegation of obsolete models).
    pub gc_producer: Producer<GcItem>,
    /// Overflow fallback for GC items (read-only from the RT callback).
    pub gc_overflow: Arc<GcOverflowBuffer>,
}

/// Mutable real-time state owned exclusively by the PipeWire capture callback thread.
pub struct CaptureState {
    pub active_model_l: Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    pub active_model_r: Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    pub resampler: Box<NamResampler>,
    /// Pre-allocated bidirectional streaming adapter providing strict host cardinality.
    pub stream: Box<StreamingResampleBuffer>,
    pub os_l: Box<OversampleEngine>,
    pub os_r: Box<OversampleEngine>,
    /// Active stereo-decoupled cab-sim pair (`None` = bypass, zero cost).
    /// Transported `Box`ed end-to-end (state, SPSC channel, GC) so swaps move
    /// the same allocation without RT heap traffic.
    pub active_cabsim: Option<Box<CabSimPair>>,
    pub current_nam_rate: u32,
    pub resamp_out_l: Box<[f32; MAX_RESAMP_BUF]>,
    pub resamp_out_r: Box<[f32; MAX_RESAMP_BUF]>,
    /// Oversampled / cabinet-stage scratch buffers, sized to at least
    /// `MAX_RESAMP_BUF` (even when `OversampleFactor::Off`, because the silence
    /// gate and cabinet stage reuse `os_in_l/r` and `os_model_l/r` as
    /// `dry_l/r` and `scratch_l/r`). A runtime oversample swap that needs more
    /// scratch than allocated is rejected by `handle_oversample_rebuild`, so the
    /// RT pipeline never receives an engine pair the scratch cannot hold.
    pub os_in_l: Box<[f32]>,
    pub os_in_r: Box<[f32]>,
    pub os_model_l: Box<[f32]>,
    pub os_model_r: Box<[f32]>,
    /// WaveNet crossfade scratch buffers (engine 0.5.0 `DspBuffers`): second
    /// pass output used when processing is chunked (active resampler).
    pub xfd_scratch_l: Box<[f32; MAX_RESAMP_BUF]>,
    pub xfd_scratch_r: Box<[f32; MAX_RESAMP_BUF]>,
    pub user_input_gain_mult: f32,
    pub user_output_gain_mult: f32,
    pub model_input_mult_adj: f32,
    pub model_output_mult_adj: f32,
    pub input_gain_mult: f32,
    pub output_gain_mult: f32,
    pub gate_params: GateParams,
    pub gate_mode: crate::standalone::cli::GateMode,
    pub musical_gate: crate::dsp::musical_gate::MusicalNoiseGate,
    pub silence_hysteresis: DynamicHysteresis,
    pub mono_hysteresis: DynamicHysteresis,
    /// Persistent hysteresis for the gate-closed cabinet tail path. Constructed
    /// off-RT and reset when a tail drain begins, so the RT callback never
    /// builds a fresh FSM per quantum.
    pub tail_hysteresis: DynamicHysteresis,
    /// Whether the previous quantum was draining the cabinet tail. Used to
    /// reset [`Self::tail_hysteresis`] exactly at the start of the tail.
    pub tail_draining: bool,
    pub process_mono: bool,
    pub adaptive_compute: AdaptiveCompute,
    pub threshold_open_sq: f32,
    pub threshold_close_sq: f32,
    pub shared_target_rate: Arc<AtomicU32>,
    pub frame_count: u32,
    pub recording_meta_sent: bool,
    pub recording_meta_rate: u32,
    pub thread_configured: bool,
    /// Activation precision mode for the audio data thread.
    pub activation: neural_amp_modeler_rs::math::activations::ActivationPrecision,
    /// Rate synchronization state across callbacks (desired rate tracking, idempotency, retry bounds).
    pub rate_sync: crate::standalone::pw_host::rt_callback::RateSyncState,
    pub ir_raw_samples: Option<Vec<f32>>,
    /// Sample rate of `ir_raw_samples` (the IR file's native rate). The
    /// main-thread rebuild resamples the preserved original IR specifically
    /// for the applied host output rate.
    pub ir_source_rate: u32,
    /// Cab-sim UPOLS partition policy in samples (`--cabsim-partition`,
    /// exact set `{32, 64, 128, 256}`, default `128`). Sizes every pair
    /// installation: the RT callback publishes it in the first-install
    /// rebuild request instead of inheriting the host quantum, and the
    /// rate-only rebuilds preserve the active partition. Written once by the
    /// main thread before the RT handoff, read-only afterwards.
    pub cabsim_partition_policy: usize,
    /// Dedicated swap channels drained by the engine's generic scheduler:
    /// each `RtSwapDrain` owns its ring consumer AND the single
    /// deferred slot (formerly the `deferred_*` fields below). Wired in
    /// `run_pipewire_host` once per process (the rings and parked commands
    /// survive a bounded reconnect cycle); unwrapped by the RT callback.
    pub resampler_drain: Option<ResamplerSwapDrain>,
    pub cabsim_drain: Option<CabSimSwapDrain>,
    /// Deferred structural command slot for the mixed `Consumer<ParamPayload>`
    /// ring (contract adoption — the unboxed ring has no `RtSwapDrain`).
    ///
    /// Parks at most one `LoadModel` whose per-callback structural budget
    /// ([`STRUCTURAL_SWAPS_PER_CALLBACK`], shared across all swap drains) was
    /// exhausted. The parked command is resolved at the start of the next
    /// callback — applied if still current, superseded by a newer same-kind
    /// command already queued (latest-wins coalescing), or re-parked if the
    /// budget is still exhausted. Touched only by the RT callback; never
    /// allocates.
    pub deferred_model: Option<ParamPayload>,
    pub slimmable_drain: Option<SlimmableSwapDrain>,
    pub os_drain: Option<OsSwapDrain>,
    /// Cab-sim rebuild failure latch: stores the `(host_rate, partition_size)` of the last
    /// build attempt that failed and installed safe-bypass (`None`).
    /// Prevents RT callback from repeatedly requesting rebuilds for the same failing key.
    /// Cleared upon successful pair installation. Since IR is immutable per process in NAM-Audio-Pipe,
    /// recovery from a failed latch occurs when host rate or partition policy changes.
    pub cabsim_failed_key: Option<(u32, usize)>,
    /// Optional reference to the host stream status flags for publishing applied latency.
    pub stream_status: Option<Arc<StreamStatusFlags>>,
    /// Active neural cabinet runtime slot (mutually exclusive with `active_cabsim`).
    pub active_cab_nam: Option<crate::standalone::cabinet::CabNamSlot>,
    /// Dedicated unboxed SPSC consumer for neural cabinet slot installs/rebuilds.
    pub cab_nam_consumer: Option<rtrb::Consumer<crate::standalone::cabinet::PipeCabNamPayload>>,
    /// True when the startup `--cab` is a `.nam`/`.namb` neural cabinet (for rate rebuild gating).
    pub has_nam_cab: bool,
    /// Shared lock-free bypass flag for the cabinet stage (`--cab-bypass` or runtime tests).
    pub cab_bypass: Arc<std::sync::atomic::AtomicBool>,
    /// Pre-allocated circular dry delay line matching cabinet latency.
    pub cab_dry_delay: Box<crate::standalone::cabinet::CabDryDelay>,
    /// 64-sample raised-cosine crossfader & reset-before-engage controller.
    pub cab_bypass_state: crate::standalone::cabinet::CabBypassState,
}

pub(crate) struct CaptureRtView<'a> {
    pub resampler: &'a mut NamResampler,
    pub os_l: &'a mut OversampleEngine,
    pub os_r: &'a mut OversampleEngine,
    pub active_model_l: &'a mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    pub active_model_r: &'a mut Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    pub active_cabsim: &'a mut Option<Box<CabSimPair>>,
    pub active_cab_nam: &'a mut Option<crate::standalone::cabinet::CabNamSlot>,
    pub cab_bypass: &'a Arc<std::sync::atomic::AtomicBool>,
    pub cab_dry_delay: &'a mut crate::standalone::cabinet::CabDryDelay,
    pub cab_bypass_state: &'a mut crate::standalone::cabinet::CabBypassState,
    pub stream: &'a mut StreamingResampleBuffer,
    pub resamp_out_l: &'a mut [f32; MAX_RESAMP_BUF],
    pub resamp_out_r: &'a mut [f32; MAX_RESAMP_BUF],
    pub os_in_l: &'a mut [f32],
    pub os_in_r: &'a mut [f32],
    pub os_model_l: &'a mut [f32],
    pub os_model_r: &'a mut [f32],
    pub xfd_scratch_l: &'a mut [f32; MAX_RESAMP_BUF],
    pub xfd_scratch_r: &'a mut [f32; MAX_RESAMP_BUF],
    pub gate_params: &'a GateParams,
    pub input_gain_mult: f32,
    pub output_gain_mult: f32,
    pub threshold_open_sq: f32,
    pub threshold_close_sq: f32,
    pub silence_hysteresis: &'a mut DynamicHysteresis,
    pub mono_hysteresis: &'a mut DynamicHysteresis,
    pub process_mono: &'a mut bool,
    pub adaptive_compute: &'a mut AdaptiveCompute,
    pub user_input_gain_mult: f32,
    pub gate_mode: crate::standalone::cli::GateMode,
    pub musical_gate: &'a mut crate::dsp::musical_gate::MusicalNoiseGate,
    pub tail_hysteresis: &'a mut DynamicHysteresis,
    pub tail_draining: &'a mut bool,
    pub frame_count: &'a mut u32,
    pub recording_meta_sent: &'a mut bool,
    pub recording_meta_rate: &'a mut u32,
}

impl CaptureState {
    /// Pre-allocates all resampler, oversampling, cabinet, gate, and scratch buffers for RT capture.
    pub fn init(
        sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
        os: OversampleFactor,
        gate_config: GateConfig,
    ) -> Self {
        // The 48 kHz identity bypass re-instantiates the same resampler with
        // fixed valid parameters; the first fallible build already emitted a
        // diagnostic, so the second failure is treated as an invariant.
        #[expect(
            clippy::expect_used,
            reason = "48 kHz identity resampler bypass construction is infallible in practice"
        )]
        let resampler = NamResampler::new_simple_typed(48_000, 48_000).unwrap_or_else(|e| {
            NamDiagnostic::new(NamErrorCode::ResamplerBuildFailed, sys)
                .message("Failed to create initial NamResampler (using 48k bypass).")
                .hint("The engine remains in bypass mode. The resampler will be recreated upon receiving the actual rate from PipeWire.")
                .param("initial_rate", 48_000_u32)
                .param("detail", e)
                .emit_warning();
            NamResampler::new_simple_typed(48_000, 48_000).expect("bypass cannot fail")
        });

        #[expect(
            clippy::expect_used,
            reason = "48 kHz identity streaming buffer bypass construction is infallible in practice"
        )]
        let stream =
            StreamingResampleBuffer::new(48_000, 48_000, MAX_RESAMP_BUF).unwrap_or_else(|e| {
                NamDiagnostic::new(NamErrorCode::ResamplerBuildFailed, sys)
                    .message("Failed to create initial StreamingResampleBuffer (using 48k bypass).")
                    .hint("Falling back to bypass mode.")
                    .param("initial_rate", 48_000_u32)
                    .param("detail", e)
                    .emit_warning();
                StreamingResampleBuffer::new(48_000, 48_000, MAX_RESAMP_BUF)
                    .expect("bypass streaming buffer cannot fail")
            });

        // Resolves the polymorphic `--gate` configuration into the DSP gate
        // state. Off maps to zeroed linear thresholds (the mathematical
        // equivalent of `-inf dBFS`, so the FSM never closes); a Threshold
        // variant configures `gate_params` and converts both dBFS thresholds
        // through the gain LUT. Runs on the main thread (init), never on the
        // audio thread — the RT callback only reads the precomputed
        // `threshold_open_sq` / `threshold_close_sq` squares below, keeping the
        // zero-allocation invariant.
        let mut gate_params = GateParams::default();
        let lut = gain_lut::get_gain_lut();
        let (open_lin, close_lin) = match gate_config {
            GateConfig::Off => {
                log::info!("Noise gate disabled (--gate off) — pass-through mode.");
                (0.0, 0.0)
            }
            GateConfig::Threshold {
                threshold_open_db,
                threshold_close_db,
            } => {
                gate_params.threshold_open_db = threshold_open_db;
                gate_params.threshold_close_db = threshold_close_db;
                log::info!(
                    "Noise gate thresholds: open {threshold_open_db:.1} dBFS, \
                     close {threshold_close_db:.1} dBFS (Schmitt hysteresis)."
                );
                (
                    lut.db_to_linear(threshold_open_db),
                    lut.db_to_linear(threshold_close_db),
                )
            }
        };

        // Oversample engines resolve first (with the Off fallback) so the
        // scratch sizing below uses the *effective* factor, clamped to at least
        // `MAX_RESAMP_BUF`: `run_capture_dsp_pipeline` also reuses `os_in_l/r`
        // and `os_model_l/r` as dry-delay / cabinet-stage scratch even when
        // oversampling is `Off`.
        let effective_os = {
            match OversampleEngine::new(os, MAX_RESAMP_BUF) {
                Ok(_) => os,
                Err(_) => OversampleFactor::Off,
            }
        };
        let os_scratch_len =
            neural_amp_modeler_rs::dsp::oversample::OversampleEngine::required_scratch_len(
                effective_os,
                MAX_RESAMP_BUF,
            )
            .max(MAX_RESAMP_BUF);
        let alloc_os_scratch = || -> Box<[f32]> { vec![0.0f32; os_scratch_len].into_boxed_slice() };

        #[expect(
            clippy::expect_used,
            reason = "Off-mode oversample engine bypass construction is infallible"
        )]
        let os_l = Box::new(
            OversampleEngine::new(os, MAX_RESAMP_BUF).unwrap_or_else(|e| {
                NamDiagnostic::new(NamErrorCode::OutOfMemory, sys)
                    .message("Failed to create oversample engine (L).")
                    .hint("Falling back to bypass mode.")
                    .param("detail", e)
                    .emit_warning();
                OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF)
                    .expect("bypass oversample engine cannot fail")
            }),
        );
        #[expect(
            clippy::expect_used,
            reason = "Off-mode oversample engine bypass construction is infallible"
        )]
        let os_r = Box::new(
            OversampleEngine::new(os, MAX_RESAMP_BUF).unwrap_or_else(|e| {
                NamDiagnostic::new(NamErrorCode::OutOfMemory, sys)
                    .message("Failed to create oversample engine (R).")
                    .hint("Falling back to bypass mode.")
                    .param("detail", e)
                    .emit_warning();
                OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF)
                    .expect("bypass oversample engine cannot fail")
            }),
        );

        Self {
            active_model_l: None,
            active_model_r: None,
            resampler: Box::new(resampler),
            stream: Box::new(stream),
            os_l,
            os_r,
            active_cabsim: None,
            current_nam_rate: 48_000,
            resamp_out_l: Box::new([0.0f32; MAX_RESAMP_BUF]),
            resamp_out_r: Box::new([0.0f32; MAX_RESAMP_BUF]),
            os_in_l: alloc_os_scratch(),
            os_in_r: alloc_os_scratch(),
            os_model_l: alloc_os_scratch(),
            os_model_r: alloc_os_scratch(),
            xfd_scratch_l: Box::new([0.0f32; MAX_RESAMP_BUF]),
            xfd_scratch_r: Box::new([0.0f32; MAX_RESAMP_BUF]),
            user_input_gain_mult: 1.0,
            user_output_gain_mult: 1.0,
            model_input_mult_adj: 1.0,
            model_output_mult_adj: 1.0,
            input_gain_mult: 1.0,
            output_gain_mult: 1.0,
            gate_params,
            gate_mode: crate::standalone::cli::GateMode::Silence,
            musical_gate: crate::dsp::musical_gate::MusicalNoiseGate::new(48_000),
            silence_hysteresis: DynamicHysteresis::new(),
            mono_hysteresis: DynamicHysteresis::new(),
            tail_hysteresis: DynamicHysteresis::new(),
            tail_draining: false,
            process_mono: false,
            adaptive_compute: AdaptiveCompute::new(AdaptiveComputeMode::Conservative),
            threshold_open_sq: open_lin * open_lin,
            threshold_close_sq: close_lin * close_lin,
            shared_target_rate: Arc::new(AtomicU32::new(0)),
            frame_count: 0,
            recording_meta_sent: false,
            recording_meta_rate: 0,
            thread_configured: false,
            activation: neural_amp_modeler_rs::math::activations::ActivationPrecision::Standard,
            rate_sync: crate::standalone::pw_host::rt_callback::RateSyncState::new(48_000),
            ir_raw_samples: None,
            ir_source_rate: 0,
            cabsim_partition_policy: crate::standalone::cli::CABSIM_PARTITION_DEFAULT as usize,
            resampler_drain: None,
            cabsim_drain: None,
            deferred_model: None,
            slimmable_drain: None,
            os_drain: None,
            cabsim_failed_key: None,
            stream_status: None,
            active_cab_nam: None,
            cab_nam_consumer: None,
            has_nam_cab: false,
            cab_bypass: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cab_dry_delay: Box::new(crate::standalone::cabinet::CabDryDelay::new(0)),
            cab_bypass_state: crate::standalone::cabinet::CabBypassState::new(false),
        }
    }

    pub(crate) fn split_rt(&mut self) -> CaptureRtView<'_> {
        CaptureRtView {
            resampler: &mut self.resampler,
            os_l: &mut self.os_l,
            os_r: &mut self.os_r,
            active_model_l: &mut self.active_model_l,
            active_model_r: &mut self.active_model_r,
            active_cabsim: &mut self.active_cabsim,
            active_cab_nam: &mut self.active_cab_nam,
            cab_bypass: &self.cab_bypass,
            cab_dry_delay: &mut self.cab_dry_delay,
            cab_bypass_state: &mut self.cab_bypass_state,
            stream: &mut self.stream,
            resamp_out_l: &mut self.resamp_out_l,
            resamp_out_r: &mut self.resamp_out_r,
            os_in_l: &mut self.os_in_l,
            os_in_r: &mut self.os_in_r,
            os_model_l: &mut self.os_model_l,
            os_model_r: &mut self.os_model_r,
            xfd_scratch_l: &mut self.xfd_scratch_l,
            xfd_scratch_r: &mut self.xfd_scratch_r,
            gate_params: &self.gate_params,
            input_gain_mult: self.input_gain_mult,
            output_gain_mult: self.output_gain_mult,
            threshold_open_sq: self.threshold_open_sq,
            threshold_close_sq: self.threshold_close_sq,
            silence_hysteresis: &mut self.silence_hysteresis,
            mono_hysteresis: &mut self.mono_hysteresis,
            process_mono: &mut self.process_mono,
            adaptive_compute: &mut self.adaptive_compute,
            user_input_gain_mult: self.user_input_gain_mult,
            gate_mode: self.gate_mode,
            musical_gate: &mut self.musical_gate,
            tail_hysteresis: &mut self.tail_hysteresis,
            tail_draining: &mut self.tail_draining,
            frame_count: &mut self.frame_count,
            recording_meta_sent: &mut self.recording_meta_sent,
            recording_meta_rate: &mut self.recording_meta_rate,
        }
    }

    /// Configures the active gate topology (`Silence` vs `Musical`) and synchronizes thresholds.
    pub fn configure_gate_mode(&mut self, gate_mode: crate::standalone::cli::GateMode) {
        self.gate_mode = gate_mode;
        self.apply_gate_resolution(gain_lut::get_gain_lut());
    }

    /// Synchronizes silence-hysteresis squares and musical expander settings after initialization
    /// or runtime `GateParams` command updates.
    pub fn apply_gate_resolution(&mut self, lut: &gain_lut::GainLUT) {
        match self.gate_mode {
            crate::standalone::cli::GateMode::Silence => {
                let off = self.threshold_open_sq == 0.0 && self.threshold_close_sq == 0.0;
                let (open_sq, close_sq) = if off {
                    (0.0, 0.0)
                } else {
                    let open_lin = lut.db_to_linear(self.gate_params.threshold_open_db);
                    let close_lin = lut.db_to_linear(self.gate_params.threshold_close_db);
                    (open_lin * open_lin, close_lin * close_lin)
                };
                self.threshold_open_sq = open_sq;
                self.threshold_close_sq = close_sq;
                self.musical_gate
                    .set_threshold(self.gate_params.threshold_open_db as f64, false);
            }
            crate::standalone::cli::GateMode::Musical => {
                let threshold_configured =
                    !(self.threshold_open_sq == 0.0 && self.threshold_close_sq == 0.0);
                let enabled = threshold_configured && self.gate_params.threshold_open_db > -120.0;
                self.silence_hysteresis.reset();
                self.musical_gate
                    .set_threshold(self.gate_params.threshold_open_db as f64, enabled);
            }
        }
    }
}

#[cfg(test)]
#[path = "gate_property_test.rs"]
mod gate_property_test;

#[cfg(test)]
#[path = "state_test.rs"]
mod tests;
