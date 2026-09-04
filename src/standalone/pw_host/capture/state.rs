// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! DSP state aggregated from the preamble of `setup_capture_stream`.
//!
//! Groups all locals initialized before the RT `process()` closure.
//! Large working buffers (>32 KB each) are boxed on the heap during `init`,
//! which runs in the main thread. The `Box` pointers remain on the stack,
//! preserving RT safety by avoiding dynamic allocation in the audio thread.

use crate::recording::buffer::{AlignedBlock, MAX_BLOCK_SIZE};
use crate::standalone::cli::GateConfig;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::common::params::AdaptiveComputeMode;
use neural_amp_modeler_rs::common::spsc::{
    CabSimSwapPayload, GcItem, GcOverflowBuffer, ParamPayload, ResamplerSwapPayload, SlimModelPair,
};
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine, OversampleFactor};
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
/// the channels survive a bounded reconnect cycle. The `slimmable` and
/// `oversample` consumers live in [`CaptureState`] instead (`slimmable_rx` /
/// `os_rx`); the main thread only touches its own separate `gc_overflow`
/// handle (an [`Arc`] clone) during the control loop, so there is never
/// concurrent aliasing of this struct between the RT callback (exclusive
/// `&mut`) and the main thread.
pub struct RtHostChannels {
    /// CLI→DSP parameter channel consumer (gain, model, etc.).
    pub param_consumer: Consumer<ParamPayload>,
    /// RT→main GC recycle producer (drop-delegation of obsolete models).
    pub gc_producer: Producer<GcItem>,
    /// Overflow fallback for GC items (read-only from the RT callback).
    pub gc_overflow: Arc<GcOverflowBuffer>,
    /// Dedicated channel receiving pre-built resamplers from the main thread.
    pub resampler_consumer: Consumer<Box<ResamplerSwapPayload>>,
    /// Dedicated channel receiving pre-built cab-sim pairs from the main thread.
    pub cabsim_consumer: Consumer<Box<CabSimSwapPayload>>,
}

/// Max oversampled buffer size: MAX_RESAMP_BUF × 4 (for X4 oversampling).
const MAX_OS_BUF: usize = MAX_RESAMP_BUF * 4;

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
    pub resamp_mid_l: Box<[f32; MAX_RESAMP_BUF]>,
    pub resamp_out_l: Box<[f32; MAX_RESAMP_BUF]>,
    pub resamp_mid_r: Box<[f32; MAX_RESAMP_BUF]>,
    pub resamp_out_r: Box<[f32; MAX_RESAMP_BUF]>,
    pub model_out_l: Box<[f32; MAX_RESAMP_BUF]>,
    pub model_out_r: Box<[f32; MAX_RESAMP_BUF]>,
    pub os_in_l: Box<[f32; MAX_OS_BUF]>,
    pub os_in_r: Box<[f32; MAX_OS_BUF]>,
    pub os_model_l: Box<[f32; MAX_OS_BUF]>,
    pub os_model_r: Box<[f32; MAX_OS_BUF]>,
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
    pub silence_hysteresis: DynamicHysteresis,
    pub mono_hysteresis: DynamicHysteresis,
    pub process_mono: bool,
    pub adaptive_compute: AdaptiveCompute,
    pub threshold_open_sq: f32,
    pub threshold_close_sq: f32,
    pub shared_target_rate: Arc<AtomicU32>,
    pub frame_count: u32,
    pub recording_meta_sent: bool,
    pub recording_meta_rate: u32,
    pub recording_block: AlignedBlock<MAX_BLOCK_SIZE>,
    pub thread_configured: bool,
    pub ir_raw_samples: Option<Vec<f32>>,
    /// Sample rate of `ir_raw_samples` (the IR file's native rate). The
    /// main-thread rebuild resamples the preserved original IR specifically
    /// for the applied host output rate.
    pub ir_source_rate: u32,
    pub slimmable_rx: Option<Consumer<Box<neural_amp_modeler_rs::common::spsc::SlimModelPair>>>,
    pub os_rx: Option<Consumer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>>,
    /// Deferred structural command slots.
    ///
    /// Each slot parks at most one structural command whose per-callback
    /// structural budget (`STRUCTURAL_SWAPS_PER_CALLBACK`, shared across all
    /// swap drains) was exhausted. The parked command is resolved at the start
    /// of the next callback — applied if still current, superseded by a newer
    /// same-kind command already queued (latest-wins coalescing), or discarded
    /// if its request generation advanced while parked. Slots are only touched
    /// by the RT callback; they never allocate.
    pub deferred_resampler: Option<Box<ResamplerSwapPayload>>,
    pub deferred_cabsim: Option<Box<CabSimSwapPayload>>,
    pub deferred_model: Option<ParamPayload>,
    pub deferred_slimmable: Option<Box<SlimModelPair>>,
    pub deferred_os: Option<Box<OsEnginePair>>,
}

impl CaptureState {
    pub fn init(
        sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
        os: OversampleFactor,
        gate_config: GateConfig,
    ) -> Self {
        let resampler = NamResampler::new(48_000, 48_000, 2048).unwrap_or_else(|e| {
            NamDiagnostic::new(NamErrorCode::ResamplerBuildFailed, sys)
                .message("Failed to create initial NamResampler (using 48k bypass).")
                .hint("The engine remains in bypass mode. The resampler will be recreated upon receiving the actual rate from PipeWire.")
                .param("initial_rate", 48_000_u32)
                .param("detail", &e)
                .emit_warning();
            NamResampler::new(48_000, 48_000, 2048).expect("bypass cannot fail")
        });

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

        Self {
            active_model_l: None,
            active_model_r: None,
            resampler: Box::new(resampler),
            stream: Box::new(stream),
            os_l: Box::new(
                OversampleEngine::new(os, MAX_RESAMP_BUF).unwrap_or_else(|e| {
                    NamDiagnostic::new(NamErrorCode::OutOfMemory, sys)
                        .message("Failed to create oversample engine (L).")
                        .hint("Falling back to bypass mode.")
                        .param("detail", e)
                        .emit_warning();
                    OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF)
                        .expect("bypass oversample engine cannot fail")
                }),
            ),
            os_r: Box::new(
                OversampleEngine::new(os, MAX_RESAMP_BUF).unwrap_or_else(|e| {
                    NamDiagnostic::new(NamErrorCode::OutOfMemory, sys)
                        .message("Failed to create oversample engine (R).")
                        .hint("Falling back to bypass mode.")
                        .param("detail", e)
                        .emit_warning();
                    OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF)
                        .expect("bypass oversample engine cannot fail")
                }),
            ),
            active_cabsim: None,
            current_nam_rate: 48_000,
            resamp_mid_l: Box::new([0.0f32; MAX_RESAMP_BUF]),
            resamp_out_l: Box::new([0.0f32; MAX_RESAMP_BUF]),
            resamp_mid_r: Box::new([0.0f32; MAX_RESAMP_BUF]),
            resamp_out_r: Box::new([0.0f32; MAX_RESAMP_BUF]),
            model_out_l: Box::new([0.0f32; MAX_RESAMP_BUF]),
            model_out_r: Box::new([0.0f32; MAX_RESAMP_BUF]),
            os_in_l: Box::new([0.0f32; MAX_OS_BUF]),
            os_in_r: Box::new([0.0f32; MAX_OS_BUF]),
            os_model_l: Box::new([0.0f32; MAX_OS_BUF]),
            os_model_r: Box::new([0.0f32; MAX_OS_BUF]),
            xfd_scratch_l: Box::new([0.0f32; MAX_RESAMP_BUF]),
            xfd_scratch_r: Box::new([0.0f32; MAX_RESAMP_BUF]),
            user_input_gain_mult: 1.0,
            user_output_gain_mult: 1.0,
            model_input_mult_adj: 1.0,
            model_output_mult_adj: 1.0,
            input_gain_mult: 1.0,
            output_gain_mult: 1.0,
            gate_params,
            silence_hysteresis: DynamicHysteresis::new(),
            mono_hysteresis: DynamicHysteresis::new(),
            process_mono: false,
            adaptive_compute: AdaptiveCompute::new(AdaptiveComputeMode::Off),
            threshold_open_sq: open_lin * open_lin,
            threshold_close_sq: close_lin * close_lin,
            shared_target_rate: Arc::new(AtomicU32::new(0)),
            frame_count: 0,
            recording_meta_sent: false,
            recording_meta_rate: 0,
            recording_block: AlignedBlock::new(),
            thread_configured: false,
            ir_raw_samples: None,
            ir_source_rate: 0,
            slimmable_rx: None,
            os_rx: None,
            deferred_resampler: None,
            deferred_cabsim: None,
            deferred_model: None,
            deferred_slimmable: None,
            deferred_os: None,
        }
    }
}

#[cfg(test)]
#[path = "gate_property_test.rs"]
mod gate_property_test;
