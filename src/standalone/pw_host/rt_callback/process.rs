// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Real-time DSP logic
//! Acquires the raw system buffer and delegates the heavy lifting to the Audio Factory (pipeline).

use crate::recording::transport::RecordingSender;
use crate::standalone::pw_host::StreamStatusFlags;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::pipeline::{
    DspBridgeWriter, DspPipelineContext, StreamingDspBuffers,
};
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;

use super::recording_tx::{
    record_overrun, send_recording_audio, send_recording_metadata, send_recording_silence,
};
use super::spa_contract::{
    handle_spa_pair_fail_closed, resolve_capture_chunk_window, silence_available_datas, stamp_chunk,
};

use super::pipeline::run_capture_dsp_pipeline;
use crate::standalone::pw_host::capture::state::CaptureRtView;

use pipewire as pw;
use std::sync::atomic::{AtomicBool, Ordering};

/// Exclusive cabinet runtime state passed into [`run_capture_dsp_pipeline`].
pub struct PipeCabRuntime<'a> {
    pub nam_slot: Option<&'a mut crate::standalone::cabinet::CabNamSlot>,
    pub target_bypass: bool,
    pub dry_delay: &'a mut crate::standalone::cabinet::CabDryDelay,
    pub bypass_state: &'a mut crate::standalone::cabinet::CabBypassState,
    /// Persistent gate-closed tail envelope (see `CaptureState::tail_hysteresis`).
    pub tail_hysteresis: &'a mut neural_amp_modeler_rs::dsp::gate::DynamicHysteresis,
    /// Tracks tail-path entry so the envelope is reset exactly once per tail.
    pub tail_draining: &'a mut bool,
}

pub(crate) struct RtDspParts<'a> {
    pub context: DspPipelineContext<'a>,
    pub buffers: StreamingDspBuffers<'a>,
    pub cab: PipeCabRuntime<'a>,
    pub stream_resample: &'a mut StreamingResampleBuffer,
    pub current_host_rate: u32,
    pub user_input_gain_mult: f32,
    pub gate_mode: crate::standalone::cli::GateMode,
    pub musical_gate: &'a mut crate::dsp::musical_gate::MusicalNoiseGate,
    pub frame_count: &'a mut u32,
    pub rt_status: &'a RtStatusFlags,
    pub recording_meta_sent: &'a mut bool,
    pub recording_meta_rate: &'a mut u32,
}

pub(crate) fn assemble_rt_dsp<'a>(
    view: CaptureRtView<'a>,
    rt_status: &'a RtStatusFlags,
    bridge_writer: Option<DspBridgeWriter>,
    current_host_rate: u32,
) -> RtDspParts<'a> {
    let CaptureRtView {
        resampler,
        os_l,
        os_r,
        active_model_l,
        active_model_r,
        active_cabsim,
        active_cab_nam,
        cab_bypass,
        cab_dry_delay,
        cab_bypass_state,
        stream,
        resamp_out_l,
        resamp_out_r,
        os_in_l,
        os_in_r,
        os_model_l,
        os_model_r,
        xfd_scratch_l,
        xfd_scratch_r,
        gate_params,
        input_gain_mult,
        output_gain_mult,
        threshold_open_sq,
        threshold_close_sq,
        silence_hysteresis,
        mono_hysteresis,
        process_mono,
        adaptive_compute,
        user_input_gain_mult,
        gate_mode,
        musical_gate,
        tail_hysteresis,
        tail_draining,
        frame_count,
        recording_meta_sent,
        recording_meta_rate,
    } = view;

    let conv_pair = active_cabsim
        .as_deref_mut()
        .filter(|pair| pair.sample_rate == current_host_rate);
    let nam_slot = active_cab_nam
        .as_mut()
        .filter(|slot| slot.host_rate == current_host_rate);
    let (threshold_open_sq, threshold_close_sq) =
        if gate_mode == crate::standalone::cli::GateMode::Musical {
            (0.0, 0.0)
        } else {
            (threshold_open_sq, threshold_close_sq)
        };

    RtDspParts {
        context: DspPipelineContext {
            resampler,
            os_l,
            os_r,
            active_model_l,
            active_model_r,
            input_gain_mult,
            output_gain_mult,
            gate_params,
            silence_hysteresis,
            mono_hysteresis,
            threshold_open_sq,
            threshold_close_sq,
            process_mono,
            rt_status,
            adaptive: adaptive_compute,
            bridge_writer,
            conv: None,
            conv_pair,
        },
        buffers: StreamingDspBuffers {
            resamp_out_l: &mut *resamp_out_l,
            resamp_out_r: &mut *resamp_out_r,
            os_in_l: &mut *os_in_l,
            os_in_r: &mut *os_in_r,
            os_model_l: &mut *os_model_l,
            os_model_r: &mut *os_model_r,
            crossfade_scratch_l: &mut *xfd_scratch_l,
            crossfade_scratch_r: &mut *xfd_scratch_r,
        },
        cab: PipeCabRuntime {
            nam_slot,
            target_bypass: cab_bypass.load(Ordering::Acquire),
            dry_delay: cab_dry_delay,
            bypass_state: cab_bypass_state,
            tail_hysteresis,
            tail_draining,
        },
        stream_resample: stream,
        current_host_rate,
        user_input_gain_mult,
        gate_mode,
        musical_gate,
        frame_count,
        rt_status,
        recording_meta_sent,
        recording_meta_rate,
    }
}

/// Executes the DSP pipeline on the dequeued PipeWire audio buffer.
#[inline(always)]
pub fn process_dsp_buffer(
    stream: &pw::stream::Stream,
    rt: RtDspParts<'_>,
    stream_status: &StreamStatusFlags,
    recording_sender: &mut RecordingSender,
    recording_failed: Option<&AtomicBool>,
    t_cap_start: u64,
) {
    let RtDspParts {
        context,
        buffers,
        cab,
        stream_resample,
        current_host_rate,
        user_input_gain_mult,
        gate_mode,
        musical_gate,
        frame_count,
        rt_status: rt_status_for_process,
        recording_meta_sent,
        recording_meta_rate,
    } = rt;

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
    if stream_status.capture_format_ok.load(Ordering::Relaxed) == 0 {
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
            recording_failed,
        );

        let should_measure = (*frame_count & 0xF) == 0;
        *frame_count = frame_count.wrapping_add(1);

        if should_measure && t_cap_start > 0 {
            let t_spa_valid = rt_setup::rdtsc_nanos();
            let cap_nanos = t_spa_valid.saturating_sub(t_cap_start);
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
        let outcome = run_capture_dsp_pipeline(
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
            cab,
            t_cap_start,
        );
        let n_pw = outcome.n_pw;

        let t_dsp_end = if should_measure {
            rt_setup::rdtsc_nanos()
        } else {
            0
        };

        if should_measure && t_dsp_start > 0 {
            let elapsed_nanos = t_dsp_end.saturating_sub(t_dsp_start);
            rt_status_for_process
                .dsp_cycle_time
                .store(elapsed_nanos, Ordering::Relaxed);
            rt_status_for_process.latency_hist.record(elapsed_nanos);

            let elapsed_secs = elapsed_nanos as f64 / 1_000_000_000.0;
            let budget_secs = super::super::overload::dsp_overload_budget_secs(
                n_samples as u64,
                current_host_rate as u64,
            );
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

            if n_pw > 0 {
                send_recording_audio(
                    recording_sender,
                    n_pw,
                    &buffers.resamp_out_l[..],
                    &buffers.resamp_out_r[..],
                    recording_failed,
                );
            } else if outcome.silence_frames > 0 {
                send_recording_silence(recording_sender, outcome.silence_frames, recording_failed);
            }

            if should_measure && t_rec_start > 0 {
                let rec_nanos = rt_setup::rdtsc_nanos().saturating_sub(t_rec_start);
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
