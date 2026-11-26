// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Telemetry and RT status monitoring for the audio engine.
//!
//! Translates atomic signals from the DSP thread into diagnostic logs for
//! the main loop, acting as the "dashboard" of NAM-Audio-Pipe.
//!
//! Runtime telemetry emits concise `log::*` lines
//! (`[Exxxx | MNEMONIC]` code + cause + recovery hint) **without** the
//! `DiagnosticBundle` support block — the `──── Recent Log Trace ────`
//! render is reserved for explicit `--diagnose`/`--diagnose-full` dumps and
//! crash/panic reports. Every recurrent signal is latched
//! ([`TelemetryLatches`]) so a continuous condition warns at most once per
//! episode instead of once per control-loop iteration.
//!
//! # Latency histogram reset protocol
//!
//! [`RtStatusFlags::latency_hist`] has a single writer (the RT callback, via
//! `record()`) and a single reader (this module). Each bucket and the
//! min/max/sum statistics are independent atomics advanced with `fetch_*`;
//! `AtomicHistogram::reset()` clears them with `swap` per bucket. Therefore no
//! bucket can be observed torn, neither side blocks, and a sample that races
//! the sweep may be dropped — an accepted best-effort reset. The reader only
//! calls `latency_summary()` before `reset()`, so the published percentiles are
//! always internally monotonic (`p50 <= p99 <= p999 <= max`).

#[path = "telemetry/state.rs"]
mod state;

mod gate;
mod gc;
mod health;
mod latency;
mod rt_health;

pub use state::{
    GATE_LOG_MIN_INTERVAL, GateTransitionThrottle, LatchedSignal, LatencyHistogramView,
    LatencySummary, PollState, TelemetryLatches, latency_summary,
};

use crate::standalone::pw_host::StreamStatusFlags;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gate::poll_gate_transitions;
use gc::poll_gc_and_command_flags;
use health::{check_audio_deadline, poll_clock_drift, poll_xruns_and_playback_health};
use latency::emit_throttled_latency_and_timing;
use rt_health::{poll_rate_clip_and_hugepages, poll_rt_thread_setup};

/// Reads atomic RT status flags and emits monitoring logs to the user.
///
/// This function acts as the "dashboard" of NAM-Audio-Pipe. It is called periodically
/// to translate the technical signals coming from the audio thread (which is silent and ultra-fast)
/// into understandable messages, performance warnings, and latency telemetry.
///
/// Returns a tuple (current_silent, current_fading) for state control in the main loop.
///
/// `_sys` is retained for API stability (callers/tests pass the startup snapshot);
/// runtime telemetry is intentionally bundle-free.
pub fn poll_rt_status(
    rt_status: &RtStatusFlags,
    stream_status: Option<&StreamStatusFlags>,
    _sys: &SystemSnapshot,
    was_silent: bool,
    was_fading: bool,
    dropped_frames: u32,
    state: &mut PollState,
) -> (bool, bool) {
    let current_bits = rt_status.status_bits.load(Ordering::Relaxed);
    rt_status
        .flags_seen
        .fetch_or(current_bits, Ordering::Relaxed);

    poll_gc_and_command_flags(rt_status, &mut state.latches);
    poll_rate_clip_and_hugepages(rt_status, state);
    poll_rt_thread_setup(rt_status, state);
    poll_xruns_and_playback_health(rt_status, stream_status, state);
    poll_clock_drift(stream_status, dropped_frames, state);

    let nanos = rt_status.dsp_cycle_time.load(Ordering::Relaxed);
    if nanos > 0 {
        emit_throttled_latency_and_timing(rt_status, stream_status, state);
        check_audio_deadline(rt_status, Duration::from_nanos(nanos), state);
    }

    poll_gate_transitions(rt_status, was_silent, was_fading, state)
}

#[cfg(test)]
#[path = "telemetry_test.rs"]
mod telemetry_test;
