// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Capture DSP pipeline orchestration.

use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_IS_SILENT};
use neural_amp_modeler_rs::dsp::pipeline::{DspPipelineContext, StreamingDspBuffers};
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;

use super::process::PipeCabRuntime;

/// Outcome of one DSP pipeline run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PipelineResult {
    /// Post-resampler output frames delivered to the bridge/recording.
    pub n_pw: usize,
    /// Frames intentionally suppressed by the `Silence` gate with no cabinet
    /// tail to drain. The recording path turns this into a zero-frame marker so
    /// the capture duration stays continuous; transport loss never sets it.
    pub silence_frames: usize,
}

/// Executes the active capture DSP pipeline under either [`GateMode::Silence`] (legacy
/// block-RMS hysteresis with CPU skip and post-IR fade) or [`GateMode::Musical`]
/// (continuous reference-compatible quadratic expander with pre-model detector and pre-IR gain).
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Unified RT DSP pipeline supporting Silence and Musical gate topologies and exclusive cab slot"
)]
pub(crate) fn run_capture_dsp_pipeline(
    samples_l: &mut [f32],
    samples_r: &mut [f32],
    n_samples: usize,
    mut ctx: DspPipelineContext<'_>,
    stream_resample: &mut StreamingResampleBuffer,
    bufs: StreamingDspBuffers<'_>,
    current_host_rate: u32,
    user_input_gain_mult: f32,
    gate_mode: crate::standalone::cli::GateMode,
    musical_gate: &mut crate::dsp::musical_gate::MusicalNoiseGate,
    mut cab: PipeCabRuntime<'_>,
    capture_tsc: u64,
) -> PipelineResult {
    use neural_amp_modeler_rs::dsp::gate::GateState;
    use neural_amp_modeler_rs::dsp::gate_flags::report_gate_flags;
    use neural_amp_modeler_rs::dsp::pipeline::{
        MAX_RESAMP_BUF, apply_input_stage, apply_output_stage, run_inference_streaming,
        write_bridge_stamped,
    };

    let n = n_samples
        .min(samples_l.len())
        .min(samples_r.len())
        .min(MAX_RESAMP_BUF);
    if n != n_samples || bufs.resamp_out_l.len() < n || bufs.resamp_out_r.len() < n {
        ctx.rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        if gate_mode == crate::standalone::cli::GateMode::Musical {
            ctx.rt_status.clear_flag(RT_STATUS_IS_SILENT);
        } else {
            let safe_l = bufs.resamp_out_l.len().min(n);
            bufs.resamp_out_l[..safe_l].fill(0.0);
            let safe_r = bufs.resamp_out_r.len().min(n);
            bufs.resamp_out_r[..safe_r].fill(0.0);
        }
        *cab.tail_draining = false;
        if let Some(ref writer) = ctx.bridge_writer {
            writer.write_silence();
        }
        return PipelineResult::default();
    }
    if n == 0 {
        *cab.tail_draining = false;
        if let Some(ref writer) = ctx.bridge_writer {
            writer.write_silence();
        }
        return PipelineResult::default();
    }

    if gate_mode == crate::standalone::cli::GateMode::Musical {
        musical_gate.ensure_sample_rate(current_host_rate);
        // Musical Gate Detector: post user input gain, pre model input calibration (`cal_in`).
        // Keep both channels updated continuously across mono <-> stereo transitions.
        musical_gate.process_trigger_scaled(
            samples_l,
            samples_r,
            n,
            false,
            user_input_gain_mult as f64,
        );
    }

    // Take `conv_pair` upfront so `ctx` can be borrowed freely by Stages 1-2.
    let mut wav_pair = ctx.conv_pair.take();

    // Stage 1: Sanitize, input gain (+ model `cal_in`), mono hysteresis, denormal dither.
    let gate_state = apply_input_stage(samples_l, samples_r, n, &mut ctx);
    if gate_mode == crate::standalone::cli::GateMode::Silence {
        report_gate_flags(ctx.rt_status, gate_state);
        if gate_state == GateState::Closed {
            stream_resample.reset();
            bufs.os_in_l[..n].fill(0.0);
            cab.dry_delay
                .process_zeros(bufs.os_in_l, bufs.os_model_l, n, *ctx.process_mono);
            // Ring out the active cabinet (WAV IR or neural .nam/.namb) while its tail
            // budget remains; the two sources are mutually exclusive.
            let tail_active = !cab.target_bypass && !cab.bypass_state.was_steady_bypassed();
            let drained_wav = if tail_active
                && let Some(ref mut pair) = wav_pair
                && pair.remaining_tail_samples() > 0
            {
                bufs.resamp_out_l[..n].fill(0.0);
                bufs.resamp_out_r[..n].fill(0.0);
                pair.drain_tail_stereo(
                    &mut bufs.resamp_out_l[..n],
                    &mut bufs.resamp_out_r[..n],
                    Some(ctx.rt_status),
                );
                if *ctx.process_mono {
                    bufs.resamp_out_r[..n].copy_from_slice(&bufs.resamp_out_l[..n]);
                }
                true
            } else {
                false
            };
            let drained_nam = if !drained_wav
                && tail_active
                && let Some(nam) = cab.nam_slot.as_deref_mut()
                && nam.remaining_tail_samples() > 0
            {
                let drained = nam.drain_tail(
                    bufs.resamp_out_l,
                    bufs.resamp_out_r,
                    n,
                    *ctx.process_mono,
                    bufs.os_model_l,
                    bufs.os_model_r,
                );
                drained > 0
            } else {
                false
            };

            if drained_wav || drained_nam {
                if !*cab.tail_draining {
                    cab.tail_hysteresis.reset();
                    *cab.tail_draining = true;
                }
                apply_output_stage(
                    bufs.resamp_out_l,
                    bufs.resamp_out_r,
                    n,
                    ctx.output_gain_mult,
                    cab.tail_hysteresis,
                    ctx.rt_status,
                    *ctx.process_mono,
                    ctx.adaptive,
                    current_host_rate,
                );
                write_bridge_stamped(
                    bufs.resamp_out_l,
                    bufs.resamp_out_r,
                    n,
                    ctx.bridge_writer.as_ref(),
                    *ctx.process_mono,
                    capture_tsc,
                );
                return PipelineResult {
                    n_pw: n,
                    silence_frames: 0,
                };
            }
            *cab.tail_draining = false;
            if let Some(ref writer) = ctx.bridge_writer {
                writer.write_silence();
            }
            // Intentional gate suppression: report the processed frame count so
            // recording can preserve temporal continuity with zero frames.
            return PipelineResult {
                n_pw: 0,
                silence_frames: n,
            };
        }
    }
    *cab.tail_draining = false;

    // Stage 2: Oversampling, neural inference, and output resampling.
    let n_pw = run_inference_streaming(
        samples_l,
        samples_r,
        bufs.resamp_out_l,
        bufs.resamp_out_r,
        n,
        &mut ctx,
        stream_resample,
        bufs.os_in_l,
        bufs.os_in_r,
        bufs.os_model_l,
        bufs.os_model_r,
        bufs.crossfade_scratch_l,
        bufs.crossfade_scratch_r,
    );

    if gate_mode == crate::standalone::cli::GateMode::Musical {
        // Musical Gate Gain: applied post-model, pre-cabinet so cabinet tails ring naturally.
        musical_gate.apply_gain(
            bufs.resamp_out_l,
            bufs.resamp_out_r,
            n_pw,
            *ctx.process_mono,
        );
    }

    // Stage 3: Exclusive Cabinet slot (WAV UPOLS or 1-in/1-out NAM/NAMB) + constant-latency bypass.
    cab.bypass_state.process_stage(
        cab.target_bypass,
        wav_pair,
        cab.nam_slot,
        cab.dry_delay,
        bufs.os_in_l,
        bufs.os_in_r,
        bufs.resamp_out_l,
        bufs.resamp_out_r,
        bufs.os_model_l,
        bufs.os_model_r,
        n_pw,
        *ctx.process_mono,
        ctx.rt_status,
    );

    // Stage 4: Output gain, clipping detection, and adaptive compute state.
    apply_output_stage(
        bufs.resamp_out_l,
        bufs.resamp_out_r,
        n_pw,
        ctx.output_gain_mult,
        ctx.silence_hysteresis,
        ctx.rt_status,
        *ctx.process_mono,
        ctx.adaptive,
        current_host_rate,
    );

    // Stage 5: Deliver output to audio bridge.
    write_bridge_stamped(
        bufs.resamp_out_l,
        bufs.resamp_out_r,
        n_pw,
        ctx.bridge_writer.as_ref(),
        *ctx.process_mono,
        capture_tsc,
    );

    PipelineResult {
        n_pw,
        silence_frames: 0,
    }
}
