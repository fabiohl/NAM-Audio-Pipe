// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Free functions extracted from the capture stream listener closures.
//!
//! These functions are called from thin wrapper closures in `setup_capture_stream`,
//! reducing the inline closure size while preserving the same capture semantics.

use crate::standalone::pw_host::output_pw::{
    check_negotiated_rate_mismatch, mark_format_contract_ok, reject_negotiated_format_violation,
    validate_audio_raw_format,
};
use crate::standalone::pw_host::{SharedBackendStatus, observe_stream_state};
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use pipewire as pw;
use std::sync::atomic::{AtomicU32, Ordering};

/// Handles capture stream state changes — feeds the backend state machine
/// (F-RB-010 / T4.4).
///
/// Transitions the shared [`SharedBackendStatus`] through the canonical
/// [`observe_stream_state`] mapping: a fatal `StreamState::Error` or a
/// post-streaming `StreamState::Unconnected` (daemon crash/restart) marks the
/// backend `Failed`, so the main control loop tears the host down observably.
///
/// Note (Sprint C-01 / H-06): This handler executes on the PipeWire `ThreadLoop` thread,
/// NOT on the RT data processing thread that runs `process()`. Real-time thread setup
/// (DAZ/FTZ MXCSR, SCHED_FIFO, CPU affinity) must remain in the cold-path of `process()`.
pub fn state_changed_handler(
    old: pw::stream::StreamState,
    new: pw::stream::StreamState,
    backend: &SharedBackendStatus,
) {
    observe_stream_state("capture", old, new, backend);
}

/// Handles capture stream `param_changed` events (format negotiation).
///
/// Enforces the strict SPA format contract (G-RB-001 / T4.3) through the
/// canonical [`validate_audio_raw_format`] gate: only `F32P` planar stereo is
/// accepted. A diverging renegotiation (mono, interleaved, S16, surround) is
/// rejected fail-closed — `RT_STATUS_HOST_CONTRACT_VIOLATION` is raised on
/// `rt_status`, the structured diagnostic is emitted, the backend is marked
/// `BackendState::Degraded` (audio muted), and the one-shot rate cell is left
/// untouched so the DSP keeps the previously applied rate. A subsequent valid
/// renegotiation restores the backend to `Running`.
///
/// Note: this handler executes on the PipeWire `ThreadLoop` thread (cold
/// path), never on the RT data thread. The rate store pairs with the `Acquire`
/// swap in `sync_rate` (`rt_callback/rate_sync.rs`).
pub fn param_changed_handler(
    _stream: &pw::stream::Stream,
    _user_data: &mut (),
    id: u32,
    param: Option<&pw::spa::pod::Pod>,
    rate_for_param: &AtomicU32,
    rt_status: &RtStatusFlags,
    backend: &SharedBackendStatus,
) {
    let Some(param) = param else { return };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }

    match validate_audio_raw_format(param) {
        Ok(rate) => {
            rate_for_param.store(rate, Ordering::Release); // Pairs with Acquire swap in sync_rate (rt_callback/rate_sync.rs)
            rt_status
                .capture_negotiated_rate
                .store(rate, Ordering::Release);
            mark_format_contract_ok(rt_status, "capture");
            check_negotiated_rate_mismatch(rt_status);
        }
        Err(violation) => {
            let violation_msg = violation.to_string();
            reject_negotiated_format_violation(rt_status, "capture", violation);
            backend.mark_degraded(format!(
                "SPA format contract violated on the capture stream: {violation_msg}"
            ));
        }
    }
}
