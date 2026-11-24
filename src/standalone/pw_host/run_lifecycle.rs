// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Lifecycle helpers for [`super::run::run_pipewire_host_with_cabinet`].
//!
//! Splits stream setup, bounded-reconnect retry handling, single-iteration
//! control-loop orchestration, instance teardown, and final shutdown reporting
//! out of the top-level host loop so each phase stays small and auditable.

use super::capture::state::CaptureState;
use super::handlers;
use super::output_pw::AppState;
use super::reconnect::ReconnectCycle;
use super::{SharedBackendStatus, StreamStatusFlags};
use crate::recording::guard::{RecordingWorkerGuard, RecordingWorkerOutcome};
use crate::standalone::banner::{
    BannerInfo, BannerRequest, SessionSummary, render_banner, render_session_summary,
};
use crate::standalone::colors::{Colorize, color_enabled};
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, RtStatusFlags, SHUTDOWN, SlimModelPair,
};
use neural_amp_modeler_rs::dsp::oversample::OsEnginePair;
use neural_amp_modeler_rs::dsp::pipeline::DspBridge;
use neural_amp_modeler_rs::models::StaticModel;
use pipewire as pw;
use rtrb::{Consumer, Producer};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::latency;

/// Decision after handling a PipeWire `connect` or stream-setup error inside
/// the bounded-reconnect loop.
pub(super) enum RetryDecision {
    Retry,
    BreakHost(Option<(&'static str, String)>),
}

/// Handles a startup or reconnect error uniformly for both `connect` and
/// `stream-setup`, preserving fail-fast on the initial connection and bounded
/// backoff on subsequent attempts.
pub(super) fn handle_attempt_error<E: std::fmt::Display>(
    err: E,
    stage: &'static str,
    stream_label: &'static str,
    sys: &SystemSnapshot,
    reconnect: &mut ReconnectCycle,
    backend_status: &SharedBackendStatus,
) -> Result<RetryDecision, E> {
    if reconnect.attempts_made() == 0 {
        emit_startup_backend_diagnostic(sys, stage, &err);
        return Err(err);
    }
    match reconnect.begin_attempt() {
        Some(backoff) => {
            log::warn!(
                "{} {} failed during reconnect attempt {}/{} ({err}); retrying in {:?}.",
                "🔁".yellow(),
                if stage == "connect" {
                    "PipeWire daemon unreachable"
                } else {
                    "Stream setup"
                },
                reconnect.attempts_made(),
                reconnect.policy().max_attempts,
                backoff,
            );
            backend_status.begin_reconnect(
                reconnect.attempts_made(),
                reconnect.policy().max_attempts,
                backoff,
            );
            if sleep_interruptible(backoff) {
                Ok(RetryDecision::BreakHost(None))
            } else {
                Ok(RetryDecision::Retry)
            }
        }
        None => Ok(RetryDecision::BreakHost(Some((
            stream_label,
            format!("{stage} failed: {err}"),
        )))),
    }
}

/// Reads the currently installed DSP latencies from `state_ptr` (while the
/// PipeWire loops are stopped) and publishes them to `StreamStatusFlags`.
///
/// # Safety
/// Both PipeWire loops must be stopped so `state_ptr` has no concurrent RT
/// accessor.
pub(super) unsafe fn snapshot_and_publish_initial_latencies(
    state_ptr: *mut CaptureState,
    backend_status: &SharedBackendStatus,
) -> u32 {
    // SAFETY: caller guarantees PipeWire loops are stopped and `state_ptr` is exclusive.
    let (initial_resamp_lat, initial_cab_lat, initial_os_native, initial_nam_rate) = unsafe {
        let state = &*state_ptr;
        let resamp = state.stream.latency_samples();
        let cab = crate::standalone::cabinet::active_cabinet_latency(
            state.active_cabsim.as_deref(),
            state.active_cab_nam.as_ref(),
        );
        let os = state.os_l.latency_samples() as u32;
        (resamp, cab, os, state.current_nam_rate)
    };
    let initial_os_lat =
        latency::convert_os_latency_to_host(initial_os_native, 48_000, initial_nam_rate);
    let initial_latency_samples = initial_resamp_lat + initial_cab_lat + initial_os_lat;
    let ss = backend_status.stream_status();
    ss.applied_nam_rate
        .store(initial_nam_rate, Ordering::Release);
    ss.applied_resampler_latency
        .store(initial_resamp_lat, Ordering::Release);
    ss.applied_cabsim_latency
        .store(initial_cab_lat, Ordering::Release);
    ss.applied_oversample_latency
        .store(initial_os_native, Ordering::Release);
    ss.applied_latency_pair.store(
        StreamStatusFlags::pack_latency_pair(initial_resamp_lat, initial_cab_lat),
        Ordering::Release,
    );
    initial_latency_samples
}

/// Emits the consolidated status banner once the first RT quantum reports an
/// effective buffer size.
pub(super) fn maybe_emit_status_banner(
    banner_emitted: &mut bool,
    rt_status: &RtStatusFlags,
    banner: &BannerRequest,
    host_rate: u32,
    current_dsp_latency: u32,
) {
    let effective_quantum = rt_status.requested_buffer_frames.load(Ordering::Relaxed);
    if *banner_emitted || effective_quantum == 0 {
        return;
    }
    let policy = rt_status.rt_policy.load(Ordering::Relaxed);
    let policy_name = if policy == libc::SCHED_FIFO {
        "FIFO"
    } else if policy == libc::SCHED_RR {
        "RR"
    } else {
        "OTHER"
    };
    let banner_info = BannerInfo {
        request: banner.clone(),
        sample_rate: host_rate,
        quantum: effective_quantum,
        dsp_latency_samples: current_dsp_latency,
        rt_fifo: rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO),
        rt_policy: policy_name,
        rt_priority: rt_status.confirmed_priority.load(Ordering::Relaxed),
        mlockall_ok: rt_setup::mlockall_ok(),
    };
    log::info!(
        "{}",
        render_banner(&banner_info, color_enabled()).trim_end()
    );
    *banner_emitted = true;
}

/// Reads bridge diagnostic counters via raw field projections while RT
/// callbacks are live, returning any newly accumulated dropped frames.
pub(super) fn sample_bridge_diagnostics(
    bridge_ptr: neural_amp_modeler_rs::dsp::pipeline::BridgeRef,
) -> u32 {
    // SAFETY: `bridge_ptr` points to the process-lifetime `DspBridge`
    // allocation; the raw projections only read/swap atomic fields through
    // `&raw const` without materializing a `&DspBridge`.
    let bridge_raw = unsafe { bridge_ptr.as_ptr() }.cast_const();
    // SAFETY: `bridge_raw` is valid; both reads are atomic field projections.
    let (written_gen, consumed_gen) = unsafe {
        (
            DspBridge::written_gen_raw(bridge_raw),
            DspBridge::consumed_gen_raw(bridge_raw),
        )
    };
    log::trace!(
        "bridge generations: published={written_gen} consumed={consumed_gen} lag={}",
        written_gen.saturating_sub(consumed_gen)
    );
    // SAFETY: same projection contract as the generation snapshot above.
    unsafe { DspBridge::drain_dropped_frames_raw(bridge_raw) }
}

/// Performs one non-RT control-loop housekeeping cycle (rebuilds, latency sync,
/// telemetry polling, banner emission, and periodic GC drain).
#[expect(
    clippy::too_many_arguments,
    reason = "Aggregates the control-loop producers and trackers owned by run_pipewire_host_with_cabinet"
)]
pub(super) fn run_control_housekeeping(
    rt_status: &Arc<RtStatusFlags>,
    rate_for_param: &std::sync::atomic::AtomicU32,
    sys: &SystemSnapshot,
    resampler_producer: &mut Producer<
        Box<neural_amp_modeler_rs::common::spsc::ResamplerSwapPayload>,
    >,
    nam_cab_source: Option<&(Vec<u8>, crate::standalone::cabinet::CabAssetKind, String)>,
    cab_nam_producer: Option<&mut Producer<crate::standalone::cabinet::PipeCabNamPayload>>,
    ir_raw_samples: Option<&[f32]>,
    ir_source_rate: u32,
    ir_level_policy: crate::standalone::cli_domain::IrLevelPolicy,
    cabsim_producer: &mut Producer<Box<neural_amp_modeler_rs::common::spsc::CabSimSwapPayload>>,
    backend_status: &SharedBackendStatus,
    buffer_size: u32,
    latency_tracker: &mut latency::LatencyPublicationTracker,
    thread_loop: &pw::thread_loop::ThreadLoopBox,
    app_state: &AppState<
        pw::stream::StreamListener<()>,
        pw::stream::StreamBox<'_>,
        pw::stream::StreamListener<()>,
        pw::stream::StreamBox<'_>,
    >,
    full_wavenet_model_l: Option<&StaticModel>,
    full_wavenet_model_r: Option<&StaticModel>,
    has_model_r: bool,
    slimmable_producer: &mut Producer<Box<SlimModelPair>>,
    os_producer: &mut Producer<Box<OsEnginePair>>,
    rebuild_failures: &mut handlers::RebuildFailureTracker,
    os_scratch_len: usize,
    bridge_ptr: neural_amp_modeler_rs::dsp::pipeline::BridgeRef,
    was_silent: &mut bool,
    was_fading: &mut bool,
    poll_state: &mut rt_setup::PollState,
    banner_emitted: &mut bool,
    banner: &BannerRequest,
    gc_consumer: &mut Consumer<GcItem>,
    gc_overflow: &GcOverflowBuffer,
) {
    let active = rt_status.active_rate.load(Ordering::Relaxed);
    if active != 0 {
        neural_amp_modeler_rs::common::diagnostics::ACTIVE_SAMPLE_RATE
            .store(active, Ordering::Relaxed);
    }

    let host_rate =
        latency::resolve_effective_host_rate(active, rate_for_param.load(Ordering::Relaxed));

    let _ = handlers::handle_resampler_rebuild(rt_status, sys, resampler_producer);
    if let (Some((raw_bytes, kind, display_name)), Some(nam_prod)) =
        (nam_cab_source, cab_nam_producer)
    {
        let _ = handlers::handle_cabinet_nam_rebuild(
            rt_status,
            raw_bytes,
            *kind,
            display_name,
            sys,
            nam_prod,
        );
    } else {
        let _ = handlers::handle_cabsim_rebuild(
            rt_status,
            ir_raw_samples,
            ir_source_rate,
            ir_level_policy,
            sys,
            cabsim_producer,
        );
    }

    let (active_resampler_latency, active_cabsim_latency, active_os_native_latency) =
        backend_status.stream_status().read_applied_latencies();
    let applied_nam_rate = backend_status
        .stream_status()
        .applied_nam_rate
        .load(Ordering::Relaxed);
    let model_rate = if applied_nam_rate != 0 {
        applied_nam_rate
    } else {
        48_000
    };
    let active_os_latency =
        latency::convert_os_latency_to_host(active_os_native_latency, host_rate, model_rate);

    let current_dsp_latency = active_cabsim_latency + active_resampler_latency + active_os_latency;
    handlers::handle_quantum_log(rt_status, buffer_size, current_dsp_latency, host_rate);

    let latency_target = latency::LatencyTarget {
        samples: current_dsp_latency,
        host_rate,
    };
    let now = Instant::now();
    if latency_tracker.needs_sync(latency_target, now) {
        let _lock = thread_loop.lock();
        let mut publisher = latency::PipewireLatencyPublisher::new(
            &app_state.capture_stream,
            &app_state.playback_stream,
        );
        let _ = latency_tracker.sync_at(
            &mut publisher,
            latency_target,
            active_cabsim_latency,
            active_resampler_latency,
            active_os_latency,
            now,
        );
    }

    handlers::handle_slimmable_rebuild(
        rt_status,
        full_wavenet_model_l,
        full_wavenet_model_r,
        has_model_r,
        sys,
        slimmable_producer,
        rebuild_failures,
    );
    handlers::handle_oversample_rebuild(
        rt_status,
        sys,
        os_producer,
        rebuild_failures,
        os_scratch_len,
    );

    let dropped_frames = sample_bridge_diagnostics(bridge_ptr);
    (*was_silent, *was_fading) = rt_setup::poll_rt_status(
        rt_status,
        Some(backend_status.stream_status()),
        sys,
        *was_silent,
        *was_fading,
        dropped_frames,
        poll_state,
    );

    maybe_emit_status_banner(
        banner_emitted,
        rt_status,
        banner,
        host_rate,
        current_dsp_latency,
    );

    // While the loop runs, the parking lot is RT-owned — drain SPSC + overflow only.
    let mut rt_owned_lot: [Option<GcItem>; 16] = Default::default();
    let drained = neural_amp_modeler_rs::common::spsc::drain_gc_channels(
        gc_consumer,
        gc_overflow,
        &mut rt_owned_lot,
        rt_status,
    );
    rt_status
        .drains
        .fetch_add(drained as u32, Ordering::Relaxed);
}

/// Resets the DSP bridge to primed silence after streams/loops have stopped,
/// and drains the full GC cascade (including `rt_parking_lot`).
pub(super) fn reset_bridge_and_drain_gc(
    backend_status: &SharedBackendStatus,
    bridge_ptr: neural_amp_modeler_rs::dsp::pipeline::BridgeRef,
    buffer_size: u32,
    gc_consumer: &mut Consumer<GcItem>,
    gc_overflow: &GcOverflowBuffer,
    rt_parking_lot: &mut [Option<GcItem>; 16],
    rt_status: &RtStatusFlags,
) {
    // SAFETY: both PipeWire streams and loops (`pw_thread_loop` + `pw_data_loop`)
    // are stopped/dropped above, so the main thread is the sole accessor.
    let remaining_drops = unsafe { &*(bridge_ptr.as_ptr()) }.drain_dropped_frames();
    if remaining_drops > 0 {
        backend_status
            .stream_status()
            .bridge_dropped_frames
            .fetch_add(remaining_drops, Ordering::Relaxed);
    }

    // SAFETY: exclusive-access teardown window above.
    unsafe { &mut *bridge_ptr.as_ptr() }.reset_to_silence();
    // SAFETY: same exclusive-access contract as `reset_to_silence`.
    unsafe { &mut *bridge_ptr.as_ptr() }.prime_silence(buffer_size.max(1) as usize);

    let final_drained = neural_amp_modeler_rs::common::spsc::drain_gc_channels(
        gc_consumer,
        gc_overflow,
        rt_parking_lot,
        rt_status,
    );
    rt_status
        .drains
        .fetch_add(final_drained as u32, Ordering::Relaxed);
    if final_drained > 0 {
        log::debug!("nam-audio-pipe: instance GC drain released {final_drained} item(s) off-RT");
    }
}

/// Finalizes recording custody, emits either the terminal backend diagnostic or
/// the session summary, and marks the backend `Terminated`.
#[expect(
    clippy::too_many_arguments,
    reason = "Finalizes recording custody and renders either backend failure or session summary"
)]
pub(super) fn finalize_host_shutdown(
    backend_failure: Option<(&'static str, String)>,
    recording_enabled: bool,
    recording_worker: Option<RecordingWorkerGuard>,
    backend_status: &SharedBackendStatus,
    rt_state: &CaptureState,
    rt_status: &RtStatusFlags,
    buffer_size: u32,
    sys: &SystemSnapshot,
    reconnect: &ReconnectCycle,
    banner_emitted: bool,
    banner: &BannerRequest,
) -> anyhow::Result<Option<RecordingWorkerOutcome>> {
    if backend_failure.is_none() {
        if recording_enabled {
            log::warn!(
                "⏹ Shutting down… finalizing recording (a second Ctrl+C will \
                 force exit and may corrupt the capture)"
            );
        } else {
            log::info!("⏹ Shutting down…");
        }
    }

    let recording_outcome = recording_worker.map(RecordingWorkerGuard::shutdown);
    log::debug!(
        "PipeWire backend state at teardown: {:?}",
        backend_status.state()
    );

    if let Some((stream, reason)) = &backend_failure {
        let diag_sr = rt_state.shared_target_rate.load(Ordering::Relaxed);
        let diag_bs = {
            let observed = rt_status.last_n_samples.load(Ordering::Relaxed);
            if observed != 0 { observed } else { buffer_size }
        };
        NamDiagnostic::new(NamErrorCode::BackendFailure, sys)
            .message(format!(
                "PipeWire backend failed: the '{stream}' stream lost connectivity \
                 and no reconnect attempt is available."
            ))
            .hint(
                "Restart the PipeWire service (systemctl --user restart pipewire) \
                 and NAM-Audio-Pipe. The process exited with a non-zero code \
                 instead of remaining alive without audio.",
            )
            .param("host_name", "PipeWire")
            .param("sample_rate", diag_sr)
            .param("buffer_size", diag_bs)
            .param("channel_count", 2)
            .param("stream", *stream)
            .param("reason", reason)
            .param("reconnect_attempts", reconnect.attempts_made())
            .param("reconnect_max_attempts", reconnect.policy().max_attempts)
            .emit();

        if !matches!(
            recording_outcome,
            None | Some(RecordingWorkerOutcome::Success)
        ) {
            log::warn!(
                "Recording did not complete cleanly during the backend failure: \
                 {recording_outcome:?}"
            );
        }
        backend_status.mark_terminated();
        return Err(anyhow::anyhow!(
            "PipeWire backend failed ({stream}): {reason}"
        ));
    }

    if banner_emitted {
        let summary = SessionSummary {
            xruns: rt_status.xruns.load(Ordering::Relaxed),
            bridge_dropped_frames: backend_status
                .stream_status()
                .bridge_dropped_frames
                .load(Ordering::Relaxed),
            recording_dir: banner.recording_dir.clone(),
        };
        log::info!(
            "{}",
            render_session_summary(&summary, color_enabled()).trim_end()
        );
    }

    backend_status.mark_terminated();
    Ok(recording_outcome)
}

/// Emits the first-connection backend diagnostic with a stage-specific hint.
pub(super) fn emit_startup_backend_diagnostic(
    sys: &SystemSnapshot,
    stage: &'static str,
    error: &dyn std::fmt::Display,
) {
    let hint = match stage {
        "connect" => crate::standalone::exit::BACKEND_UNREACHABLE_HINT,
        _ => {
            "PipeWire is running but the audio streams could not be created: \
             verify the graph with `pw-cli info 0` and that a capture/playback \
             device is available."
        }
    };
    NamDiagnostic::new(NamErrorCode::BackendFailure, sys)
        .message(format!(
            "PipeWire first connection failed ({stage}): {error}"
        ))
        .hint(hint)
        .param("phase", "startup")
        .param("stage", stage)
        .emit();
}

/// Sleeps for `duration` in small slices, bailing out early when `SHUTDOWN` is raised.
pub(super) fn sleep_interruptible(duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
    }
}
