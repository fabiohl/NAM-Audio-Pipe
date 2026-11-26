// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Instance teardown, bridge reset, and shutdown reporting.

use super::super::SharedBackendStatus;
use super::super::reconnect::ReconnectCycle;
use crate::recording::guard::{RecordingWorkerGuard, RecordingWorkerOutcome};
use crate::standalone::banner::{BannerRequest, SessionSummary, render_session_summary};
use crate::standalone::colors::color_enabled;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags};
use rtrb::Consumer;
use std::sync::atomic::Ordering;

/// Resets the DSP bridge to primed silence after streams/loops have stopped,
/// and drains the full GC cascade (including `rt_parking_lot`).
pub(crate) fn reset_bridge_and_drain_gc(
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
pub(crate) fn finalize_host_shutdown(
    backend_failure: Option<(&'static str, String)>,
    recording_enabled: bool,
    recording_worker: Option<RecordingWorkerGuard>,
    backend_status: &SharedBackendStatus,
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
        let diag_sr = backend_status
            .stream_status()
            .last_known_rate
            .load(Ordering::Relaxed);
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
