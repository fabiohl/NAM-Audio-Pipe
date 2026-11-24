// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Off-RT resampler rebuild and PipeWire quantum logging handlers.

use super::faults::rearm_if_superseded;
#[cfg(feature = "testing")]
use super::faults::testing::resampler_fault;
use crate::standalone::cli::BUFFER_SIZE_MIN;
use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::{NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::{self, ResamplerSwapPayload, RtStatusFlags};
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use std::sync::atomic::Ordering;

fn build_resampler_pair(
    host_rate: u32,
    nam_rate: u32,
    _generation: u64,
    _rt_status: &RtStatusFlags,
) -> (
    Result<NamResampler, NamErrorCode>,
    Result<StreamingResampleBuffer, NamErrorCode>,
) {
    #[cfg(feature = "testing")]
    if resampler_fault::inject(_rt_status, _generation) {
        return (
            Err(NamErrorCode::OutOfMemory),
            Err(NamErrorCode::OutOfMemory),
        );
    }
    (
        NamResampler::new_simple_typed(host_rate, nam_rate),
        StreamingResampleBuffer::new(host_rate, nam_rate, MAX_RESAMP_BUF),
    )
}

/// Handles dynamic resampler rebuild requested by the audio thread.
pub(in crate::standalone::pw_host) fn handle_resampler_rebuild(
    rt_status: &RtStatusFlags,
    _sys: &SystemSnapshot,
    resampler_producer: &mut rtrb::Producer<Box<ResamplerSwapPayload>>,
) -> Option<u32> {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD) {
        return None;
    }
    let generation = rt_status.requested_rate_generation.load(Ordering::Acquire);
    let target_host_rate = rt_status.requested_host_rate.load(Ordering::Relaxed);
    let target_nam_rate = rt_status.requested_nam_rate.load(Ordering::Relaxed);

    if target_host_rate == 0 || target_nam_rate == 0 {
        return None;
    }

    match build_resampler_pair(target_host_rate, target_nam_rate, generation, rt_status) {
        (Ok(new_rs), Ok(new_stream)) => {
            rt_status
                .resampler_failed_generation
                .store(0, Ordering::Release);

            let latency_samples = if new_rs.is_bypass() {
                0
            } else {
                new_stream.latency_samples()
            };

            log::info!(
                "{} Sample rate updated: PW={} Hz, NAM={} Hz (bypass={}, latency={} samples)",
                "🔄".cyan(),
                target_host_rate,
                target_nam_rate,
                new_rs.is_bypass(),
                latency_samples,
            );

            let payload = Box::new(ResamplerSwapPayload {
                generation,
                resampler: Box::new(new_rs),
                stream: Box::new(new_stream),
            });
            if resampler_producer.push(payload).is_err() {
                log::warn!(
                    "[E2201 | RESAMPLER_CHANNEL_FULL] Resampler channel full — rebuild will \
                     be retried; the audio engine is overloaded (PW={} Hz, NAM={} Hz). \
                     The swap is retried automatically until delivery succeeds.",
                    target_host_rate,
                    target_nam_rate
                );
                return None;
            }
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD,
                &rt_status.requested_rate_generation,
                generation,
            );
            Some(latency_samples)
        }
        (Err(e), _) => {
            log::error!(
                "Failed to rebuild resampler for PW={} Hz and NAM={} Hz ({e}) — audio will \
                 continue with the previous resampler; if the sample rate is incorrect, \
                 restart NAM-Audio-Pipe.",
                target_host_rate,
                target_nam_rate
            );
            rt_status
                .resampler_failed_generation
                .store(generation, Ordering::Release);
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD,
                &rt_status.requested_rate_generation,
                generation,
            );
            Some(0)
        }
        (_, Err(e)) => {
            log::error!(
                "Failed to create streaming resample buffer for PW={} Hz and NAM={} Hz \
                 ({e:?}) — audio will continue with the previous resampler; if the sample \
                 rate is incorrect, restart NAM-Audio-Pipe.",
                target_host_rate,
                target_nam_rate
            );
            rt_status
                .resampler_failed_generation
                .store(generation, Ordering::Release);
            rearm_if_superseded(
                rt_status,
                spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD,
                &rt_status.requested_rate_generation,
                generation,
            );
            Some(0)
        }
    }
}

/// Logs PipeWire quantum renegotiation updates, clearly distinguishing
/// requested quantum, effective quantum, and DSP algorithmic latency.
pub(in crate::standalone::pw_host) fn handle_quantum_log(
    rt_status: &RtStatusFlags,
    requested_quantum: u32,
    dsp_latency_samples: u32,
    host_rate: u32,
) {
    if !rt_status.check_flag_acquire(spsc::RT_STATUS_NEEDS_QUANTUM_LOG) {
        return;
    }
    let effective_quantum = rt_status.requested_buffer_frames.load(Ordering::Relaxed);
    let old_quantum = rt_status.previous_buffer_frames.load(Ordering::Relaxed);
    if effective_quantum != 0 && effective_quantum != old_quantum {
        if effective_quantum < BUFFER_SIZE_MIN {
            log::warn!(
                "PipeWire quantum {} samples is below the supported minimum {}; CabSim installation is rejected and the dry path keeps running.",
                effective_quantum,
                BUFFER_SIZE_MIN
            );
        }
        let rate = if host_rate > 0 { host_rate } else { 48_000 };
        let eff_ms = effective_quantum as f64 * 1000.0 / rate as f64;
        let req_str = if requested_quantum > 0 {
            format!("{requested_quantum} samples")
        } else {
            "graph default".to_string()
        };
        let dsp_ms = dsp_latency_samples as f64 * 1000.0 / rate as f64;
        log::info!(
            "{} PipeWire quantum updated: requested={} | effective={} samples ({:.2} ms @{}Hz) | DSP processing latency={} samples ({:.2} ms)",
            "🔄".cyan(),
            req_str,
            effective_quantum,
            eff_ms,
            rate,
            dsp_latency_samples,
            dsp_ms,
        );
        rt_status
            .previous_buffer_frames
            .store(effective_quantum, Ordering::Relaxed);
    }
    rt_status.clear_flag_relaxed(spsc::RT_STATUS_NEEDS_QUANTUM_LOG);
}
