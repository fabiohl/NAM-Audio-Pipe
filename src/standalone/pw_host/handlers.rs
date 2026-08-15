// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Handlers for SPSC non-RT communication and dynamic parameter rebuilds.
//!
//! Handles resampler, buffer quantum logging, CabSim IR rebuild,
//! slimmable WaveNet slicing, and oversampling engine reconfiguration.

use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::{self, RtStatusFlags};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::slice_wavenet_model;
use std::sync::atomic::Ordering;

/// Handles dynamic resampler rebuild requested by the audio thread.
pub(super) fn handle_resampler_rebuild(
    rt_status: &RtStatusFlags,
    sys: &SystemSnapshot,
    resampler_producer: &mut rtrb::Producer<Box<NamResampler>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD) {
        return;
    }
    let target_host_rate = rt_status.requested_host_rate.load(Ordering::Relaxed);
    let target_nam_rate = rt_status.requested_nam_rate.load(Ordering::Relaxed);

    if target_host_rate != 0 && target_nam_rate != 0 {
        match NamResampler::new(target_host_rate, target_nam_rate, 2048) {
            Ok(new_rs) => {
                rt_status.clear_flag_relaxed(spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED);

                log::info!(
                    "{} Sample rate updated: PW={} Hz, NAM={} Hz (bypass={})",
                    "🔄".cyan(),
                    target_host_rate,
                    target_nam_rate,
                    new_rs.is_bypass()
                );

                if resampler_producer.push(Box::new(new_rs)).is_err() {
                    // Fail-closed: the replacement was built but could not reach
                    // the RT callback. Keep NEEDS_RESAMPLER_REBUILD set so the
                    // next main-loop iteration retries the delivery. Clearing
                    // NEEDS here (or setting REBUILD_FAILED) would either strand
                    // RESAMP_SWAP_PENDING (permanent mute) or unmute with the
                    // stale resampler (wrong rate).
                    NamDiagnostic::new(NamErrorCode::ResamplerChannelFull, sys)
                        .message("Resampler channel full. Rebuild will be retried.")
                        .hint(
                            "The audio engine is overloaded. \
                             The resampler swap is retried automatically until delivery succeeds.",
                        )
                        .param("target_host_rate", target_host_rate)
                        .param("target_nam_rate", target_nam_rate)
                        .emit_warning();
                    return;
                }
            }
            Err(e) => {
                NamDiagnostic::new(NamErrorCode::ResamplerBuildFailed, sys)
                    .message(format!(
                        "Failed to rebuild resampler for PW={} Hz and NAM={} Hz.",
                        target_host_rate, target_nam_rate
                    ))
                    .hint(
                        "Audio will continue with the previous resampler. \
                         If the sample rate is incorrect, restart NAM-Audio-Pipe.",
                    )
                    .param("target_host_rate", target_host_rate)
                    .param("target_nam_rate", target_nam_rate)
                    .param("detail", e)
                    .emit();

                rt_status.set_flag(spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED);
            }
        }
        rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
    }
}

/// Logs PipeWire quantum renegotiation updates.
pub(super) fn handle_quantum_log(rt_status: &RtStatusFlags) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_QUANTUM_LOG) {
        return;
    }
    let new_quantum = rt_status.requested_buffer_frames.load(Ordering::Relaxed);
    let old_quantum = rt_status.previous_buffer_frames.load(Ordering::Relaxed);
    if new_quantum != 0 && new_quantum != old_quantum {
        log::info!(
            "{} PipeWire quantum renegotiated: {} -> {} samples ({}->{} ms @48kHz)",
            "🔄".cyan(),
            old_quantum,
            new_quantum,
            old_quantum as f64 * 1000.0 / 48_000.0,
            new_quantum as f64 * 1000.0 / 48_000.0,
        );
        rt_status
            .previous_buffer_frames
            .store(new_quantum, Ordering::Relaxed);
    }
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_QUANTUM_LOG);
}

/// Handles CabSim IR dynamic partition rebuild.
pub(super) fn handle_cabsim_rebuild(
    rt_status: &RtStatusFlags,
    ir_raw_samples: Option<&[f32]>,
    sys: &SystemSnapshot,
    cabsim_producer: &mut rtrb::Producer<Option<CabSimAdapter>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD) {
        return;
    }
    let partition_size = rt_status
        .requested_cabsim_partition_size
        .load(Ordering::Relaxed) as usize;
    if partition_size > 0 {
        if let Some(samples) = ir_raw_samples {
            match ConvEngine::new(samples, partition_size)
                .map_err(|e| anyhow::anyhow!("Cab-sim engine: {e}"))
                .and_then(|engine| {
                    CabSimAdapter::new(Box::new(engine))
                        .map_err(|e| anyhow::anyhow!("Cab-sim adapter: {e:?}"))
                }) {
                Ok(adapter) => {
                    log::info!(
                        "{} Cab-sim IR rebuilt: partition_size={} ({} partitions, FFT={})",
                        "🔄".cyan(),
                        partition_size,
                        adapter.num_partitions(),
                        adapter.engine().fft_size(),
                    );
                    if cabsim_producer.push(Some(adapter)).is_err() {
                        // Fail-closed: keep NEEDS_CABSIM_REBUILD so the next
                        // main-loop iteration retries. Clearing NEEDS here
                        // would lock the RT on the stale partition size.
                        NamDiagnostic::new(NamErrorCode::ParamChannelFull, sys)
                            .message("Cab-sim rebuild channel full. Rebuild will be retried.")
                            .hint(
                                "The audio engine is overloaded. \
                                 The cab-sim swap is retried automatically until delivery succeeds.",
                            )
                            .param("partition_size", partition_size)
                            .emit_warning();
                        return;
                    }
                }
                Err(e) => {
                    log::error!("Failed to rebuild Cab-sim IR: {e:#}");
                }
            }
        }
        rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
    }
}

/// Handles WaveNet slimmable channel slicing rebuild.
pub(super) fn handle_slimmable_rebuild(
    rt_status: &RtStatusFlags,
    full_wavenet_model: Option<&StaticModel>,
    slimmable_producer: &mut rtrb::Producer<Option<Box<StaticModel>>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD) {
        return;
    }
    let target_ch = rt_status.requested_slimmable_ch.load(Ordering::Relaxed) as usize;
    if target_ch >= 4
        && let Some(m) = full_wavenet_model
        && let StaticModel::WavenetDyn(w) = m
    {
        // Build L channel model
        match slice_wavenet_model(w.as_ref(), target_ch) {
            Ok(mut slimmed) => {
                slimmed.prewarm();
                let model_l = Box::new(StaticModel::WavenetDyn(Box::new(slimmed)));
                if slimmable_producer.push(Some(model_l)).is_err() {
                    // Fail-closed: keep NEEDS so the next cycle retries
                    // instead of locking quality on the previous channel count.
                    return;
                }
            }
            Err(_) => {
                rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
            }
        }
        // Build R channel model (same weights, same target_ch)
        match slice_wavenet_model(w.as_ref(), target_ch) {
            Ok(mut slimmed) => {
                slimmed.prewarm();
                let model_r = Box::new(StaticModel::WavenetDyn(Box::new(slimmed)));
                if slimmable_producer.push(Some(model_r)).is_err() {
                    return;
                }
            }
            Err(_) => {
                rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
            }
        }
    }
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
}

/// Handles oversampling engine dynamic rebuild.
pub(super) fn handle_oversample_rebuild(
    rt_status: &RtStatusFlags,
    sys: &SystemSnapshot,
    os_producer: &mut rtrb::Producer<Box<OsEnginePair>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_OS_REBUILD) {
        return;
    }
    let factor_val = rt_status.requested_os_factor.load(Ordering::Relaxed);
    let factor = OversampleFactor::from_f32(factor_val as f32);
    match (
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
    ) {
        (Ok(os_l), Ok(os_r)) => {
            let pair = Box::new(OsEnginePair {
                l: Box::new(os_l),
                r: Box::new(os_r),
            });
            log::info!(
                "{} Oversampling factor changed to {:?}",
                "🔄".cyan(),
                factor,
            );
            if os_producer.push(pair).is_err() {
                NamDiagnostic::new(NamErrorCode::ParamChannelFull, sys)
                    .message("OS engine channel full. Rebuild discarded.")
                    .hint(
                        "The audio engine is overloaded. \
                         If the problem persists, restart NAM-Audio-Pipe.",
                    )
                    .emit_warning();
            } else {
                rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_OS_REBUILD);
            }
        }
        (Err(e), _) | (_, Err(e)) => {
            NamDiagnostic::new(NamErrorCode::OutOfMemory, sys)
                .message("Failed to rebuild oversample engine (OOM).")
                .hint("Audio will continue with the previous oversampling state.")
                .param("detail", e)
                .emit();
        }
    }
}

#[cfg(test)]
#[path = "handlers_test.rs"]
mod tests;
