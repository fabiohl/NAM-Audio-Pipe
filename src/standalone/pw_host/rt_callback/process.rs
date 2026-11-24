// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Real-time DSP logic
//! Acquires the raw system buffer and delegates the heavy lifting to the Audio Factory (pipeline).

use crate::recording::buffer::{AlignedBlock, MAX_BLOCK_SIZE};
use crate::recording::transport::RecordingSender;
use crate::standalone::pw_host::StreamStatusFlags;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_IS_SILENT, RtStatusFlags,
};
use neural_amp_modeler_rs::dsp::pipeline::{DspPipelineContext, StreamingDspBuffers};
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;

use super::recording_tx::{record_overrun, send_recording_audio, send_recording_metadata};
use super::spa_contract::{
    handle_spa_pair_fail_closed, resolve_capture_chunk_window, silence_available_datas, stamp_chunk,
};

use pipewire as pw;
use std::sync::atomic::{AtomicBool, Ordering};

/// Exclusive cabinet runtime state passed into [`run_capture_dsp_pipeline`].
pub struct PipeCabRuntime<'a> {
    pub nam_slot: Option<&'a mut crate::standalone::cabinet::CabNamSlot>,
    pub target_bypass: bool,
    pub dry_delay: &'a mut crate::standalone::cabinet::CabDryDelay,
    pub bypass_state: &'a mut crate::standalone::cabinet::CabBypassState,
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
) -> usize {
    use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateState};
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
        if let Some(ref writer) = ctx.bridge_writer {
            writer.write_silence();
        }
        return 0;
    }
    if n == 0 {
        if let Some(ref writer) = ctx.bridge_writer {
            writer.write_silence();
        }
        return 0;
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
                let mut tail_gate = DynamicHysteresis::new();
                apply_output_stage(
                    bufs.resamp_out_l,
                    bufs.resamp_out_r,
                    n,
                    ctx.output_gain_mult,
                    &mut tail_gate,
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
                return n;
            }
            if let Some(ref writer) = ctx.bridge_writer {
                writer.write_silence();
            }
            return 0;
        }
    }

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

    n_pw
}

/// Executes the DSP pipeline on the dequeued PipeWire audio buffer.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Core DSP kernel signature required by design; recording parameters are minimal additions"
)]
pub fn process_dsp_buffer(
    stream: &pw::stream::Stream,
    context: DspPipelineContext,
    stream_resample: &mut StreamingResampleBuffer,
    buffers: StreamingDspBuffers,
    current_host_rate: u32,
    user_input_gain_mult: f32,
    gate_mode: crate::standalone::cli::GateMode,
    musical_gate: &mut crate::dsp::musical_gate::MusicalNoiseGate,
    cab_rt: PipeCabRuntime<'_>,
    frame_count: &mut u32,
    rt_status_for_process: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
    recording_sender: &mut RecordingSender,
    recording_meta_sent: &mut bool,
    recording_meta_rate: &mut u32,
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
    t_cap_start: u64,
) {
    let mut _buf = match stream.dequeue_buffer() {
        Some(b) => b,
        None => {
            rt_status_for_process
                .input_buffer_miss
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    // Fail-closed mute: while the negotiated format contract is broken
    // (a divergent renegotiation was rejected by the param_changed listener),
    // the DSP pipeline must not run on potentially wrong-format input. The
    // dequeued buffer is recycled via drop and the bridge publishes no new
    // block — the playback side delivers deterministic silence.
    if stream_status.format_contract_ok.load(Ordering::Relaxed) == 0 {
        return;
    }

    let datas = _buf.datas_mut();
    if datas.len() < 2 {
        rt_status_for_process.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        silence_available_datas(datas);
        return;
    }
    let (left_datas, right_datas) = datas.split_at_mut(1);
    let (d_l, d_r) = match (left_datas.first_mut(), right_datas.first_mut()) {
        (Some(l), Some(r)) => (l, r),
        _ => {
            rt_status_for_process.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
            silence_available_datas(datas);
            return;
        }
    };

    // Reads the raw SPA descriptors as integers/pointers before forming any
    // reference to the audio bytes: `Data::chunk()` panics on a null chunk and
    // `Data::data()` would form `&mut [u8]` aliases when L/R share memory.
    let (ptr_l, max_l) = (d_l.as_raw().data as usize, d_l.as_raw().maxsize as usize);
    let (ptr_r, max_r) = (d_r.as_raw().data as usize, d_r.as_raw().maxsize as usize);
    let (chunk_l, chunk_r) = (d_l.as_raw().chunk, d_r.as_raw().chunk);

    // The host-declared valid-data window. Three cases are distinguished:
    // a *valid* window is used as declared; an *absent* chunk (null/misaligned
    // descriptor) yields `(0, 0)` and the consolidated harness below still rejects
    // the pair fail-closed via its own chunk-null/alignment proof; a *malformed*
    // non-null chunk (`stride != 4` or `SPA_CHUNK_FLAG_CORRUPTED`) raises `E2304`
    // here instead of silently degrading to `(0, 0)` — no reference is ever
    // formed from a null chunk, no panic reaches the C trampoline, and no
    // malformed path produces "silence with clean telemetry".
    let Some((offset_l, size_l)) =
        resolve_capture_chunk_window(chunk_l, ptr_l, max_l, ptr_r, max_r, rt_status_for_process)
    else {
        stamp_chunk(chunk_l, 0);
        stamp_chunk(chunk_r, 0);
        return;
    };
    let Some((offset_r, size_r)) =
        resolve_capture_chunk_window(chunk_r, ptr_l, max_l, ptr_r, max_r, rt_status_for_process)
    else {
        stamp_chunk(chunk_l, 0);
        stamp_chunk(chunk_r, 0);
        return;
    };

    // Consolidated fail-closed FFI/SPA validation: proves per-channel
    // alignment, bounds, cardinality, frame symmetry and strict pointer
    // disjunction before any mutable `f32` slice is formed. On any violation it
    // raises `RT_STATUS_HOST_CONTRACT_VIOLATION` and silences both channels.
    let Some((_n_bytes, n_samples)) = handle_spa_pair_fail_closed(
        ptr_l,
        max_l,
        chunk_l,
        offset_l,
        size_l,
        ptr_r,
        max_r,
        chunk_r,
        offset_r,
        size_r,
        rt_status_for_process,
    ) else {
        // Both chunks are stamped even when no region could be zeroed: a valid
        // descriptor gets `size = 0` (no stale data replayed), and an invalid
        // descriptor is left untouched because its validity is not established.
        stamp_chunk(chunk_l, 0);
        stamp_chunk(chunk_r, 0);
        return;
    };

    if n_samples > 0 {
        // SAFETY: `check_spa_buffer_pair` proved per-channel alignment, bounds,
        // frame symmetry and strict pointer disjunction, so the two `&mut [f32]`
        // below are well-formed and non-overlapping.
        let samples_l =
            unsafe { std::slice::from_raw_parts_mut((ptr_l + offset_l) as *mut f32, n_samples) };
        // SAFETY: `check_spa_buffer_pair` proved strict pointer disjunction, so this
        // right-channel mutable slice does not overlap `samples_l`.
        let samples_r =
            unsafe { std::slice::from_raw_parts_mut((ptr_r + offset_r) as *mut f32, n_samples) };

        send_recording_metadata(
            recording_sender,
            current_host_rate,
            recording_meta_sent,
            recording_meta_rate,
            recording_data_available,
            recording_failed,
        );

        let should_measure = (*frame_count & 0xF) == 0;
        *frame_count = frame_count.wrapping_add(1);

        // 1. CAPTURE TOTAL (callback start → end of SPA validation/dequeue)
        // Measured: TSC overhead =~15ns per sample (LFENCE+RDTSC), total < 0.05% of 333µs quantum
        if t_cap_start > 0 {
            stream_status
                .capture_start_tsc
                .store(t_cap_start, Ordering::Relaxed);
        }

        if should_measure && t_cap_start > 0 {
            let t_spa_valid = rt_setup::rdtsc_nanos();
            let cap_nanos = t_spa_valid.saturating_sub(t_cap_start);
            stream_status
                .capture_cycle_time
                .store(cap_nanos, Ordering::Relaxed);
            stream_status.capture_hist.record(cap_nanos);
        }

        // Denormal policy re-arm: this data-loop thread executes neural
        // inference whose math relies on FTZ/DAZ being active (constant-cost
        // denormal handling). The policy lives in per-thread CPU state, so it
        // must survive alongside the thread for its whole lifetime.
        if (*frame_count & 0x3FF) == 0 {
            // SAFETY: `set_daz_ftz` only sets the FTZ/DAZ bits of the calling
            // thread's MXCSR control word (x86_64 `stmxcsr`/`ldmxcsr` on a
            // stack local): it reads and writes no process memory, allocates
            // nothing, never unwinds, and the effect is confined to this
            // thread — sound to invoke in the audio callback. Idempotent (OR
            // of the control bits into the current MXCSR), so arming it again
            // here every 1024 quanta is a defensive re-assertion of the
            // denormal policy against foreign code running on this same
            // thread that might have reset MXCSR; repeated arming cannot
            // accumulate state.
            unsafe {
                neural_amp_modeler_rs::math::common::set_daz_ftz();
            }
        }

        // 2. DSP CORE (pre-DSP → post-DSP)
        let t_dsp_start = if should_measure {
            rt_setup::rdtsc_nanos()
        } else {
            0
        };

        // Streaming pipeline: the pipeline never reads
        // `resamp_mid_l/r` / `model_out_l/r` — pass the `StreamingDspBuffers`
        // working set (8 buffers) and let the engine's `Into<DspBuffers>`
        // adapter zero-slice the intermediates, removing 128 KiB of dead
        // per-instance allocations from `CaptureState`.
        let n_pw = run_capture_dsp_pipeline(
            samples_l,
            samples_r,
            n_samples,
            context,
            stream_resample,
            StreamingDspBuffers {
                resamp_out_l: &mut *buffers.resamp_out_l,
                resamp_out_r: &mut *buffers.resamp_out_r,
                os_in_l: &mut *buffers.os_in_l,
                os_in_r: &mut *buffers.os_in_r,
                os_model_l: &mut *buffers.os_model_l,
                os_model_r: &mut *buffers.os_model_r,
                crossfade_scratch_l: buffers.crossfade_scratch_l,
                crossfade_scratch_r: buffers.crossfade_scratch_r,
            },
            current_host_rate,
            user_input_gain_mult,
            gate_mode,
            musical_gate,
            cab_rt,
            t_cap_start,
        );

        let t_dsp_end = if should_measure {
            rt_setup::rdtsc_nanos()
        } else {
            0
        };

        if should_measure && t_dsp_start > 0 {
            let elapsed_nanos = t_dsp_end.saturating_sub(t_dsp_start);
            if rt_status_for_process
                .first_block_nanos
                .load(Ordering::Relaxed)
                == 0
            {
                rt_status_for_process
                    .first_block_nanos
                    .store(elapsed_nanos.max(1), Ordering::Relaxed);
            }
            rt_status_for_process
                .dsp_cycle_time
                .store(elapsed_nanos, Ordering::Relaxed);
            rt_status_for_process.latency_hist.record(elapsed_nanos);

            let elapsed_secs = elapsed_nanos as f64 / 1_000_000_000.0;
            let budget_secs = (n_samples as f64 / current_host_rate as f64) * 0.85;
            if elapsed_secs > budget_secs {
                rt_status_for_process
                    .dsp_overloads
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        // 3. RECORD ENQUEUE (pre-push → post-push)
        if *recording_meta_sent {
            let t_rec_start = if should_measure {
                rt_setup::rdtsc_nanos()
            } else {
                0
            };

            send_recording_audio(
                recording_sender,
                n_pw,
                &buffers.resamp_out_l[..],
                &buffers.resamp_out_r[..],
                recording_block,
                recording_data_available,
                recording_failed,
            );

            if should_measure && t_rec_start > 0 {
                let rec_nanos = rt_setup::rdtsc_nanos().saturating_sub(t_rec_start);
                stream_status
                    .record_cycle_time
                    .store(rec_nanos, Ordering::Relaxed);
                stream_status.record_hist.record(rec_nanos);
            }
        } else if recording_sender.has_producer()
            && n_pw > 0
            && !recording_failed.is_some_and(|f| f.load(Ordering::Acquire))
        {
            record_overrun(n_pw);
        }

        let n = n_samples as u32;
        if rt_status_for_process.last_n_samples.load(Ordering::Relaxed) != n {
            rt_status_for_process
                .last_n_samples
                .store(n, Ordering::Relaxed);
            rt_status_for_process
                .requested_buffer_frames
                .store(n, Ordering::Relaxed);
            rt_status_for_process
                .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_QUANTUM_LOG);
        }
    }
}

#[cfg(test)]
#[path = "process_test.rs"]
mod tests;
