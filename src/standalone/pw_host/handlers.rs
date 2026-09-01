// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Handlers for SPSC non-RT communication and dynamic parameter rebuilds.
//!
//! Handles resampler, buffer quantum logging, CabSim IR rebuild,
//! slimmable WaveNet slicing, and oversampling engine reconfiguration.

use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{
    self, CabSimSwapPayload, ResamplerSwapPayload, RtStatusFlags, SlimModelPair,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::oversample::{OsEnginePair, OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::models::StaticModel;
use neural_amp_modeler_rs::models::slimmable::slice_wavenet_model;
use std::sync::atomic::Ordering;

/// Handles dynamic resampler rebuild requested by the audio thread.
///
/// Builds a consistent "photograph" of the pending request: the generation is
/// captured with Acquire (which orders the rate stores published before the
/// Release increment in `sync_rate`), then the resampler is built and pushed
/// inside a [`ResamplerSwapPayload`] stamped with that generation. The rebuild
/// request is only cleared if no newer generation was published while the build
/// was in flight — otherwise `NEEDS_RESAMPLER_REBUILD` is re-armed so the next
/// control-loop iteration rebuilds for the most recent request (F-RB-004).
pub(super) fn handle_resampler_rebuild(
    rt_status: &RtStatusFlags,
    _sys: &SystemSnapshot,
    resampler_producer: &mut rtrb::Producer<Box<ResamplerSwapPayload>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD) {
        return;
    }
    let generation = rt_status.requested_rate_generation.load(Ordering::Acquire);
    let target_host_rate = rt_status.requested_host_rate.load(Ordering::Relaxed);
    let target_nam_rate = rt_status.requested_nam_rate.load(Ordering::Relaxed);

    if target_host_rate != 0 && target_nam_rate != 0 {
        match (
            NamResampler::new(target_host_rate, target_nam_rate, 2048),
            StreamingResampleBuffer::new(target_host_rate, target_nam_rate, MAX_RESAMP_BUF),
        ) {
            (Ok(new_rs), Ok(new_stream)) => {
                rt_status
                    .resampler_failed_generation
                    .store(0, Ordering::Release);

                log::info!(
                    "{} Sample rate updated: PW={} Hz, NAM={} Hz (bypass={})",
                    "🔄".cyan(),
                    target_host_rate,
                    target_nam_rate,
                    new_rs.is_bypass()
                );

                let payload = Box::new(ResamplerSwapPayload {
                    generation,
                    resampler: Box::new(new_rs),
                    stream: Box::new(new_stream),
                });
                if resampler_producer.push(payload).is_err() {
                    // Fail-closed: the replacement was built but could not reach
                    // the RT callback. Keep NEEDS_RESAMPLER_REBUILD set so the
                    // next main-loop iteration retries the delivery. Clearing
                    // NEEDS here (or setting REBUILD_FAILED) would either strand
                    // RESAMP_SWAP_PENDING (permanent mute) or unmute with the
                    // stale resampler (wrong rate).
                    // Sprint 6 / T6.1: concise runtime warning, no support block.
                    log::warn!(
                        "[E2201 | RESAMPLER_CHANNEL_FULL] Resampler channel full — rebuild will \
                         be retried; the audio engine is overloaded (PW={} Hz, NAM={} Hz). \
                         The swap is retried automatically until delivery succeeds.",
                        target_host_rate,
                        target_nam_rate
                    );
                    return;
                }
                rearm_rebuild_if_superseded(rt_status, generation);
            }
            (Err(e), _) => {
                log::error!(
                    "[E2200 | RESAMPLER_BUILD_FAILED] Failed to rebuild resampler for PW={} Hz \
                     and NAM={} Hz ({e}) — audio will continue with the previous resampler; if \
                     the sample rate is incorrect, restart NAM-Audio-Pipe.",
                    target_host_rate,
                    target_nam_rate
                );

                rt_status
                    .resampler_failed_generation
                    .store(generation, Ordering::Release);
                rt_status.clear_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
            }
            (_, Err(e)) => {
                log::error!(
                    "[E2200 | RESAMPLER_BUILD_FAILED] Failed to create streaming resample buffer \
                     for PW={} Hz and NAM={} Hz ({e:?}) — audio will continue with the previous \
                     resampler; if the sample rate is incorrect, restart NAM-Audio-Pipe.",
                    target_host_rate,
                    target_nam_rate
                );

                rt_status
                    .resampler_failed_generation
                    .store(generation, Ordering::Release);
                rt_status.clear_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
            }
        }
    }
}

/// Lost-wakeup guard (F-RB-004) for the main-thread side of a resampler rebuild.
///
/// Clears `NEEDS_RESAMPLER_REBUILD` and re-arms it if the request generation
/// advanced past the generation the just-completed build was stamped with. The
/// clear runs *first* and the check *after* it: if the RT thread publishes a
/// new request between the clear and the load, the load observes the advanced
/// generation and the re-arm below restores the bit; if the publish happens
/// after the load, the RT's own `set_flag` lands on the already-cleared bit and
/// sticks. Either interleaving leaves the request visible — the request can
/// never be erased by a stale build completion.
#[inline(always)]
fn rearm_rebuild_if_superseded(rt_status: &RtStatusFlags, generation: u64) {
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
    if rt_status.requested_rate_generation.load(Ordering::Acquire) != generation {
        rt_status.set_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
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

/// Handles CabSim IR dynamic rebuild (quantum and rate calibration, F-RB-006).
///
/// The cab-sim stage runs at the applied host output rate, so the preserved
/// original IR (`ir_raw_samples` at `ir_source_rate`) is resampled
/// specifically for the requested host rate before building a
/// stereo-decoupled [`CabSimPair`] (independent L/R adapters, identical IR).
/// The pair is stamped with the host rate it was calibrated for so the RT
/// can detect drift again.
///
/// Lost-wakeup guard (F-RB-004 pattern): the request generation is captured
/// with Acquire before building; the flag is only cleared via
/// [`rearm_cabsim_if_superseded`] if no newer generation was published while
/// the build was in flight.
///
/// Rollback: on build failure the handler delivers `None` — safe cab-sim
/// bypass — instead of letting the RT run an IR calibrated for a divergent
/// rate. The RT re-requests while `active == None`, so transient failures
/// recover automatically.
pub(super) fn handle_cabsim_rebuild(
    rt_status: &RtStatusFlags,
    ir_raw_samples: Option<&[f32]>,
    ir_source_rate: u32,
    _sys: &SystemSnapshot,
    cabsim_producer: &mut rtrb::Producer<Box<CabSimSwapPayload>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD) {
        return;
    }
    let generation = rt_status
        .requested_cabsim_generation
        .load(Ordering::Acquire);
    let requested_partition = rt_status
        .requested_cabsim_partition_size
        .load(Ordering::Relaxed) as usize;
    let target_host_rate = rt_status.requested_cabsim_host_rate.load(Ordering::Relaxed);
    if requested_partition == 0 || target_host_rate == 0 {
        return;
    }
    // Fail-closed partition bound (G-RB-003 / T6.2): the RT-requested partition
    // must lie in [16, MAX_RESAMP_BUF]. A spurious quantum outside that domain
    // is clamped before any `ConvEngine` is instantiated, so no oversized FFT
    // structure is ever allocated off-RT.
    let partition_size = requested_partition.clamp(16, MAX_RESAMP_BUF);
    if partition_size != requested_partition {
        log::warn!(
            "Requested cabsim partition_size {} clamped to {}",
            requested_partition,
            partition_size
        );
    }
    let Some(raw_samples) = ir_raw_samples else {
        rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
        return;
    };

    match build_cabsim_pair(
        raw_samples,
        ir_source_rate,
        target_host_rate,
        partition_size,
    ) {
        Ok(pair) => {
            log::info!(
                "{} Cab-sim IR rebuilt: rate={} Hz, partition_size={} ({} partitions, FFT={})",
                "🔄".cyan(),
                target_host_rate,
                partition_size,
                pair.l.num_partitions(),
                pair.l.engine().fft_size(),
            );
            // Box::new runs exclusively on this (non-RT) main thread: the RT
            // swap then moves the same allocation into the GC (F-RB-007).
            let payload = Box::new(CabSimSwapPayload {
                generation,
                pair: Some(Box::new(pair)),
            });
            if cabsim_producer.push(payload).is_err() {
                // Fail-closed: keep NEEDS_CABSIM_REBUILD so the next
                // main-loop iteration retries. Clearing NEEDS here
                // would lock the RT on the stale partition/rate.
                // Sprint 6 / T6.1: concise runtime warning, no support block.
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] Cab-sim rebuild channel full — rebuild will \
                     be retried; the audio engine is overloaded (partition={}, PW={} Hz). \
                     The swap is retried automatically until delivery succeeds.",
                    partition_size,
                    target_host_rate
                );
                return;
            }
            rearm_cabsim_if_superseded(rt_status, generation);
        }
        Err(e) => {
            log::error!(
                "Failed to rebuild Cab-sim IR ({} -> {} Hz, partition={}): {e:#} — bypassing cab-sim",
                ir_source_rate,
                target_host_rate,
                partition_size,
            );
            let payload = Box::new(CabSimSwapPayload {
                generation,
                pair: None,
            });
            if cabsim_producer.push(payload).is_err() {
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] Cab-sim bypass channel full — rebuild will \
                     be retried; the audio engine is overloaded. The swap is retried \
                     automatically until delivery succeeds."
                );
                return;
            }
            rearm_cabsim_if_superseded(rt_status, generation);
        }
    }
}

/// Builds a stereo-decoupled [`CabSimPair`] from the preserved original IR,
/// resampled for the applied host output rate. Off-RT only (allocates).
fn build_cabsim_pair(
    raw_samples: &[f32],
    ir_source_rate: u32,
    target_host_rate: u32,
    partition_size: usize,
) -> anyhow::Result<CabSimPair> {
    if raw_samples.is_empty() {
        anyhow::bail!("IR has no samples");
    }
    let resampled: Option<Vec<f32>> = if ir_source_rate != 0 && ir_source_rate != target_host_rate {
        Some(
            CabSimIr::resample(raw_samples, ir_source_rate, target_host_rate).map_err(|e| {
                anyhow::anyhow!("IR resample ({ir_source_rate} -> {target_host_rate} Hz): {e}")
            })?,
        )
    } else {
        None
    };
    let samples: &[f32] = resampled.as_deref().unwrap_or(raw_samples);

    let build_adapter = || {
        ConvEngine::new(samples, partition_size)
            .map_err(|e| anyhow::anyhow!("Cab-sim engine: {e}"))
            .and_then(|engine| {
                CabSimAdapter::new(Box::new(engine))
                    .map_err(|e| anyhow::anyhow!("Cab-sim adapter: {e:?}"))
            })
    };
    let l = build_adapter()?;
    let r = build_adapter()?;
    Ok(CabSimPair {
        l: Box::new(l),
        r: Box::new(r),
        sample_rate: target_host_rate,
    })
}

/// Lost-wakeup guard (F-RB-004 pattern) for the main-thread side of a
/// cab-sim rebuild.
///
/// Clears `NEEDS_CABSIM_REBUILD` and re-arms it if the cabsim generation
/// advanced past the generation the just-completed build was stamped with.
/// The clear runs *first* and the check *after* it, so a rebuild request
/// published during the resample/build cannot be erased by the stale
/// completion.
#[inline(always)]
fn rearm_cabsim_if_superseded(rt_status: &RtStatusFlags, generation: u64) {
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
    if rt_status
        .requested_cabsim_generation
        .load(Ordering::Acquire)
        != generation
    {
        rt_status.set_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
    }
}

/// Handles WaveNet slimmable channel slicing rebuild (F-RB-005).
///
/// Slices and prewarms the L and (if stereo) R channel models **before** any
/// delivery, then pushes both in a single [`SlimModelPair`] envelope over the
/// SPSC channel. The RT drain consumes the pair with one `pop()` and swaps L
/// and R in the same logical block — an all-or-nothing transaction. If the
/// channel is full, neither channel is delivered and
/// `RT_STATUS_NEEDS_SLIMMABLE_REBUILD` stays armed for a full retry in the next
/// main-loop iteration.
pub(super) fn handle_slimmable_rebuild(
    rt_status: &RtStatusFlags,
    full_wavenet_model_l: Option<&StaticModel>,
    full_wavenet_model_r: Option<&StaticModel>,
    has_model_r: bool,
    _sys: &SystemSnapshot,
    slimmable_producer: &mut rtrb::Producer<Box<SlimModelPair>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD) {
        return;
    }
    let target_ch = rt_status.requested_slimmable_ch.load(Ordering::Relaxed) as usize;
    if target_ch < 4 {
        return;
    }
    let Some(m_l) = full_wavenet_model_l else {
        return;
    };
    let StaticModel::WavenetDyn(w_l) = m_l else {
        return;
    };

    let generation = rt_status
        .requested_slimmable_generation
        .load(Ordering::Acquire);

    // Build L channel model from full_wavenet_model_l.
    let model_l = match slice_wavenet_model(w_l.as_ref(), target_ch) {
        Ok(mut slimmed) => {
            slimmed.prewarm();
            Box::new(StaticModel::WavenetDyn(Box::new(slimmed)))
        }
        Err(_) => {
            rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
            return;
        }
    };

    // Build R channel model from full_wavenet_model_r for stereo configurations.
    let model_r = if has_model_r {
        let Some(m_r) = full_wavenet_model_r else {
            rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
            return;
        };
        let StaticModel::WavenetDyn(w_r) = m_r else {
            rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
            return;
        };
        match slice_wavenet_model(w_r.as_ref(), target_ch) {
            Ok(mut slimmed) => {
                slimmed.prewarm();
                Some(Box::new(StaticModel::WavenetDyn(Box::new(slimmed))))
            }
            Err(_) => {
                rt_status.set_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED);
                return;
            }
        }
    } else {
        None
    };

    let pair = Box::new(SlimModelPair {
        generation,
        channels: target_ch,
        l: Some(model_l),
        r: model_r,
    });
    if slimmable_producer.push(pair).is_err() {
        // Fail-closed (F-RB-005): neither channel is delivered; keep NEEDS so
        // the next cycle retries the whole pair instead of delivering a
        // half-swap that would desynchronize L/R generations.
        // Sprint 6 / T6.1: concise runtime warning, no support block.
        log::warn!(
            "[E3100 | PARAM_CHANNEL_FULL] Slimmable model channel full — rebuild will be \
             retried; the audio engine is overloaded (target_ch={target_ch}). The swap is \
             retried automatically until delivery succeeds."
        );
        return;
    }
    rearm_slimmable_if_superseded(rt_status, generation);
}

/// Lost-wakeup guard (F-RB-004 pattern) for the main-thread side of a
/// slimmable rebuild.
///
/// Clears `NEEDS_SLIMMABLE_REBUILD` and re-arms it if the slimmable generation
/// advanced past the generation the just-completed pair was stamped with. The
/// clear runs *first* and the check *after* it, so a rebuild request published
/// during the slice/prewarm cannot be erased by the stale completion.
#[inline(always)]
fn rearm_slimmable_if_superseded(rt_status: &RtStatusFlags, generation: u64) {
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
    if rt_status
        .requested_slimmable_generation
        .load(Ordering::Acquire)
        != generation
    {
        rt_status.set_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
    }
}

/// Handles oversampling engine dynamic rebuild.
pub(super) fn handle_oversample_rebuild(
    rt_status: &RtStatusFlags,
    _sys: &SystemSnapshot,
    os_producer: &mut rtrb::Producer<Box<OsEnginePair>>,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_OS_REBUILD) {
        return;
    }
    let generation = rt_status.requested_os_generation.load(Ordering::Acquire);
    let factor_val = rt_status.requested_os_factor.load(Ordering::Relaxed);
    let factor = OversampleFactor::from_f32(factor_val as f32);
    match (
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
        OversampleEngine::new(factor, MAX_RESAMP_BUF),
    ) {
        (Ok(os_l), Ok(os_r)) => {
            let pair = Box::new(OsEnginePair {
                generation,
                l: Box::new(os_l),
                r: Box::new(os_r),
            });
            log::info!(
                "{} Oversampling factor changed to {:?}",
                "🔄".cyan(),
                factor,
            );
            if os_producer.push(pair).is_err() {
                // Sprint 6 / T6.1: concise runtime warning, no support block.
                log::warn!(
                    "[E3100 | PARAM_CHANNEL_FULL] OS engine channel full — rebuild will be \
                     retried; the audio engine is overloaded. The oversampling swap is \
                     retried automatically until delivery succeeds."
                );
                return;
            }
            rearm_os_if_superseded(rt_status, generation);
        }
        (Err(e), _) | (_, Err(e)) => {
            log::error!(
                "[E5000 | OUT_OF_MEMORY] Failed to rebuild oversample engine ({e}) — audio \
                 will continue with the previous oversampling state."
            );
        }
    }
}

/// Lost-wakeup guard (F-RB-004 pattern) for the main-thread side of an
/// oversampling rebuild.
///
/// Clears `NEEDS_OS_REBUILD` and re-arms it if the oversample generation
/// advanced past the generation the just-completed pair was stamped with. The
/// clear runs *first* and the check *after* it, so an oversample request published
/// during engine construction cannot be erased by the stale completion.
#[inline(always)]
fn rearm_os_if_superseded(rt_status: &RtStatusFlags, generation: u64) {
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_OS_REBUILD);
    if rt_status.requested_os_generation.load(Ordering::Acquire) != generation {
        rt_status.set_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD);
    }
}

#[cfg(test)]
#[path = "handlers_test.rs"]
mod tests;
