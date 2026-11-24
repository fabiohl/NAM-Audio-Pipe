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

#[path = "telemetry/state.rs"]
mod state;

pub use state::{
    GATE_LOG_MIN_INTERVAL, GateTransitionThrottle, LatchedSignal, LatencyHistogramView,
    LatencySummary, PollState, TelemetryLatches, latency_summary,
};

use super::affinity::CpuSelectionReason;
use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::StreamStatusFlags;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_GC_CORRUPTED, RT_STATUS_GC_OVERFLOW, RT_STATUS_GC_TIER3, RT_STATUS_HAS_CLIPPED,
    RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_HUGEPAGE_OK, RT_STATUS_IS_FADING,
    RT_STATUS_IS_SILENT, RT_STATUS_NON_FINITE_INPUT_DETECTED, RT_STATUS_PARAM_QUEUE_BACKLOG,
    RT_STATUS_RT_IS_FIFO, RT_STATUS_SLIMMABLE_RESET_FAILED, RT_STATUS_SLIMMABLE_SLICE_FAILED,
    RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED, RT_STATUS_THP_ACTIVE,
    RtStatusFlags,
};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn poll_gc_and_command_flags(rt_status: &RtStatusFlags, latches: &mut TelemetryLatches) {
    if latches
        .gc_overflow
        .observe(rt_status.check_and_clear_flag(RT_STATUS_GC_OVERFLOW))
    {
        log::warn!(
            "[E3101 | GC_OVERFLOW] Garbage Collection (GC) channel overflow detected — \
             the audio thread had to leak memory to avoid dropouts in the hot-path; \
             NAM-Audio-Pipe will drain the buffer aggressively now."
        );
    }

    if latches
        .gc_tier3
        .observe(rt_status.check_and_clear_flag(RT_STATUS_GC_TIER3))
    {
        log::warn!(
            "[E3101 | GC_OVERFLOW] GC cascade reached Tier 3 (SPSC channel and parking lot \
             both full) — items are being parked in the overflow buffer; \
             NAM-Audio-Pipe will drain the overflow buffer aggressively now."
        );
    }

    if latches
        .gc_corrupted
        .observe(rt_status.check_and_clear_flag(RT_STATUS_GC_CORRUPTED))
    {
        log::error!(
            "[E3102 | GC_CORRUPTED] Garbage Collection overflow buffer corruption detected — \
             a GC slot had inconsistent type/pointer data; the pointer was leaked to avoid \
             undefined behavior. This should never happen — report it."
        );
    }

    if latches
        .param_backlog
        .observe(rt_status.check_and_clear_flag(RT_STATUS_PARAM_QUEUE_BACKLOG))
    {
        log::warn!(
            "[E3100 | PARAM_CHANNEL_FULL] Command queue backlog: the scalar parameter drain \
             budget (16/callback) was exhausted — a producer (CLI/UI/automation) filled the \
             queue faster than it can drain; the remainder is processed by the next callback, \
             preserving the audio deadline."
        );
    }

    if latches
        .structural_deferred
        .observe(rt_status.check_and_clear_flag(RT_STATUS_STRUCTURAL_DEFERRED))
    {
        log::info!(
            "Structural command deferred to the next callback (structural budget 1/callback) — FIFO order preserved."
        );
    }

    if latches
        .structural_superseded
        .observe(rt_status.check_and_clear_flag(RT_STATUS_STRUCTURAL_SUPERSEDED))
    {
        log::info!(
            "Deferred structural command superseded by a newer same-kind command; obsolete resources discarded off-RT (coalescing)."
        );
    }

    if latches
        .slimmable_slice_failed
        .observe(rt_status.check_and_clear_flag(RT_STATUS_SLIMMABLE_SLICE_FAILED))
    {
        log::error!(
            "[E3104 | SLIMMABLE_SLICE_FAILED] WaveNet slimmable slice_channels rebuild failed — \
             model may run in reduced state. Check that the active channel count is within the \
             model's supported slimmable range."
        );
    }

    if latches
        .slimmable_reset_failed
        .observe(rt_status.check_and_clear_flag(RT_STATUS_SLIMMABLE_RESET_FAILED))
    {
        log::error!(
            "[E3105 | SLIMMABLE_RESET_FAILED] ContainerModel submodel reset failed — \
             model may run in previous state. This is a transient RT signal; if persistent, \
             check model graph integrity."
        );
    }
}

fn poll_rate_clip_and_hugepages(rt_status: &RtStatusFlags, state: &mut PollState) {
    let rate_notif = rt_status.active_rate_changed.swap(0, Ordering::Relaxed);
    if rate_notif != 0 {
        log::info!(
            "{} RT callback activated resampler with rate = {} Hz",
            "✅".green(),
            rate_notif
        );
    }

    if state
        .latches
        .clipping
        .observe(rt_status.check_and_clear_flag(RT_STATUS_HAS_CLIPPED))
    {
        log::warn!(
            "{} Clipping detected! Consider reducing the input and/or output gain, \
             or checking whether the model itself carries a large DC offset.",
            "🔥".bright_red().bold()
        );
    }

    if state
        .latches
        .non_finite_input
        .observe(rt_status.check_flag(RT_STATUS_NON_FINITE_INPUT_DETECTED))
    {
        log::error!(
            "[E2305 | NON_FINITE_INPUT_CONTAINED] Non-finite audio (NaN/Inf) detected on the \
             capture path — inputs were sanitized and the output is being sanitized until the \
             signal recovers (engine containment barrier). Check the PipeWire graph for a \
             source emitting corrupt floats."
        );
    }

    if !state.hugepage_synced {
        neural_amp_modeler_rs::dsp::mirror_buf::sync_huge_page_flag(rt_status);
        state.hugepage_synced = true;
    }
    if rt_status.check_and_clear_flag(RT_STATUS_HUGEPAGE_OK) {
        log::info!(
            "{} HugeTLB explicit 2 MB pages active — reduced TLB pressure on DSP thread.",
            "✅".green()
        );
    }
    if rt_status.check_and_clear_flag(RT_STATUS_THP_ACTIVE) {
        log::info!(
            "{} Transparent Huge Pages (THP) advice active — kernel may promote to 2 MB.",
            "ℹ️".green()
        );
    }
}

fn poll_rt_thread_setup(rt_status: &RtStatusFlags, state: &PollState) {
    let aff_err = rt_status.rt_affinity_err.swap(0, Ordering::Relaxed);
    let sched_err = rt_status.rt_sched_err.swap(0, Ordering::Relaxed);
    let getsched_err = rt_status.rt_getsched_err.swap(0, Ordering::Relaxed);
    let target_cpu = rt_status.rt_target_cpu.swap(-1, Ordering::Relaxed);
    let tid = rt_status.rt_tid.load(Ordering::Relaxed);

    if aff_err == -1 {
        log::error!(
            "CPU {} is out of bounds (CPU_SETSIZE={}). NAM-Audio-Pipe will continue running without CPU affinity.\n\
             [E2306 | CPU_OUT_OF_BOUNDS] cpu={} max={}",
            target_cpu,
            libc::CPU_SETSIZE,
            target_cpu,
            libc::CPU_SETSIZE - 1,
        );
    } else if aff_err > 0 {
        log::error!(
            "\n  ⚡ Failed to set CPU affinity to core {} (errno={}).\n  💡 NAM-Audio-Pipe will continue running, but may suffer jitter due to Core Migration.\n\
             [E2301 | CPU_AFFINITY_FAILED] cpu={} errno={}\n",
            target_cpu,
            aff_err,
            target_cpu,
            aff_err
        );
    }

    if sched_err > 0 {
        log::error!(
            "⚠️ pthread_setschedparam failed (errno={}, TID={tid}).\n\
             [E2308 | RT_SCHED_FAILED] Check ulimit -r and rtkit permissions.\n",
            sched_err
        );
    }

    if getsched_err > 0 {
        log::error!(
            "  [E2303 | RT_GETSCHED_FAILED] pthread_getschedparam failed (ret={}).\n",
            getsched_err
        );
    }

    let prio = rt_status.rt_priority.load(Ordering::Relaxed);
    if prio != -1 {
        let is_fifo = rt_status.check_flag(RT_STATUS_RT_IS_FIFO);
        let policy = rt_status.rt_policy.load(Ordering::Relaxed);
        let cpu = rt_status.rt_cpu.load(Ordering::Relaxed);

        rt_status.rt_priority.store(-1, Ordering::Relaxed);

        if is_fifo || policy == libc::SCHED_RR {
            let policy_name = if policy == libc::SCHED_RR {
                "RR"
            } else {
                "FIFO"
            };

            let is_dedicated = state.cpu_receipt.as_ref().is_some_and(|r| r.is_dedicated);

            if is_dedicated {
                log::info!(
                    "{} Real-Time Priority: Active ({}, Prio={}, Dedicated Core {}, TID={})",
                    "⚡".yellow(),
                    policy_name,
                    prio.to_string().green(),
                    cpu.to_string().cyan(),
                    tid
                );
            } else {
                let reason_str = state.cpu_receipt.as_ref().map_or(
                    "Conservative heuristic / unverified topology",
                    |r| match &r.reason {
                        CpuSelectionReason::ConservativeHeuristic { explanation, .. } => {
                            *explanation
                        }
                        CpuSelectionReason::ExplicitCli { .. } => {
                            "Explicit CLI pinning (non-isolated)"
                        }
                        CpuSelectionReason::FullyIsolated { .. } => "Fully isolated core",
                        _ => "Unclassified CPU selection reason",
                    },
                );

                log::info!(
                    "{} Real-Time Priority: Active ({}, Prio={}, Core {}, TID={}) [Affinity: {}]",
                    "⚡".yellow(),
                    policy_name,
                    prio.to_string().green(),
                    cpu.to_string().cyan(),
                    tid,
                    reason_str
                );
            }
        } else {
            let policy_str = match policy {
                libc::SCHED_OTHER => "OTHER",
                libc::SCHED_BATCH => "BATCH",
                libc::SCHED_IDLE => "IDLE",
                _ => "NON-RT",
            };
            log::warn!(
                "[E2300 | RT_PRIORITY_DENIED] DSP thread is NOT in a Real-Time scheduling \
                 policy (policy = {policy_str}, priority = {prio}, TID={tid}) — audio may \
                 experience jitter and xruns under CPU load. Ensure PipeWire RT module or rtkit is configured."
            );
        }
    }
}

fn poll_xruns_and_playback_health(
    rt_status: &RtStatusFlags,
    stream_status: Option<&StreamStatusFlags>,
    state: &mut PollState,
) {
    let overloads = rt_status.dsp_overloads.swap(0, Ordering::Relaxed);
    if overloads > 0 {
        rt_status.xruns.fetch_add(overloads, Ordering::Relaxed);
    }
    if state.latches.cpu_overload.observe(overloads > 0) {
        log::warn!(
            "{} CPU overload ({} buffers). Consider using a lighter model or a faster processor.",
            "🚨".red(),
            overloads
        );
    }

    let input_buffer_miss = rt_status.input_buffer_miss.swap(0, Ordering::Relaxed);
    if state
        .latches
        .input_buffer_miss
        .observe(input_buffer_miss > 0)
    {
        log::warn!(
            "{} PipeWire capture buffer miss ({} xruns). Check system load or buffer size.",
            "📻".yellow(),
            input_buffer_miss
        );
    }

    let output_buffer_miss = rt_status.output_buffer_miss.swap(0, Ordering::Relaxed);
    if state
        .latches
        .output_buffer_miss
        .observe(output_buffer_miss > 0)
    {
        log::warn!(
            "{} PipeWire playback buffer miss ({} xruns). Check system load or buffer size.",
            "📢".yellow(),
            output_buffer_miss
        );
    }

    if state
        .latches
        .contract_violation
        .observe(rt_status.check_and_clear_flag(RT_STATUS_HOST_CONTRACT_VIOLATION))
    {
        log::error!(
            "[E2304 | SPA_FORMAT_CONTRACT_VIOLATION] Audio host violated the strict SPA format \
             contract (F32P planar stereo, 2 channels FL/FR) — check that the PipeWire graph is \
             not forcing a mono, interleaved, S16 or surround negotiation onto the NAM streams \
             (e.g. via WirePlumber rules)."
        );
    }

    let pw_log_dropped_now = crate::standalone::pw_host::rt_log_ring::pw_log_dropped();
    let pw_log_dropped_delta = pw_log_dropped_now.saturating_sub(state.pw_log_dropped_last);
    state.pw_log_dropped_last = pw_log_dropped_now;
    if state
        .latches
        .pw_log_dropped
        .observe(pw_log_dropped_delta > 0)
    {
        log::warn!(
            "PipeWire log ring saturated: {} PW log event(s) discarded off-RT this episode \
             (marked RT thread stayed deadline-clean; PW logging at this rate is a \
             diagnostic storm — audit the PW level with PIPEWIRE_DEBUG).",
            pw_log_dropped_delta
        );
    }

    let current_starvation = stream_status
        .map(|s| s.playback_bridge_starvation.load(Ordering::Relaxed))
        .unwrap_or(0);
    let delta_starvation = current_starvation.saturating_sub(state.last_starvation);
    state.last_starvation = current_starvation;
    if state
        .latches
        .playback_starvation
        .observe(delta_starvation > 0)
    {
        log::info!(
            "{} Playback delivered {} silence block(s) under bridge starvation (no stale audio repeated).",
            "🔇".blue(),
            delta_starvation
        );
    }

    let current_early = stream_status
        .map(|s| s.playback_early_callbacks.load(Ordering::Relaxed))
        .unwrap_or(0);
    let delta_early = current_early.saturating_sub(state.last_early);
    state.last_early = current_early;
    if state.latches.playback_early.observe(delta_early > 0) {
        log::info!(
            "{} Playback callback fired ahead of the capture quantum {} time(s); classified separately from starvation.",
            "⏱",
            delta_early
        );
    }

    let current_muted = stream_status
        .map(|s| s.playback_muted_callbacks.load(Ordering::Relaxed))
        .unwrap_or(0);
    let delta_muted = current_muted.saturating_sub(state.last_muted);
    state.last_muted = current_muted;
    if state.latches.playback_muted.observe(delta_muted > 0) {
        log::info!(
            "{} Playback delivered {} fail-closed silence block(s) while the format contract was broken.",
            "🔇".blue(),
            delta_muted
        );
    }
}

fn poll_clock_drift(
    stream_status: Option<&StreamStatusFlags>,
    dropped_frames: u32,
    state: &mut PollState,
) {
    let quanta = stream_status
        .map(|s| s.playback_quantums.load(Ordering::Relaxed))
        .unwrap_or(u32::MAX);
    if quanta < state.last_bringup_quanta {
        state.last_bringup_quanta = quanta;
        state.bringup_suppressed_polls = 0;
        state.has_polled_in_bringup = false;
        state.startup_drops_drained = false;
        state.bringup_completed = false;
    } else if quanta != state.last_bringup_quanta {
        state.last_bringup_quanta = quanta;
        state.bringup_suppressed_polls = 0;
    }

    let mut bringing_up = false;
    if !state.bringup_completed {
        if quanta < crate::standalone::pw_host::PLAYBACK_BRINGUP_QUANTUMS {
            bringing_up = state.bringup_suppressed_polls
                < crate::standalone::pw_host::MAX_BRINGUP_SUPPRESSED_POLLS;
            state.has_polled_in_bringup = true;
        } else if !state.startup_drops_drained {
            bringing_up = true;
            state.startup_drops_drained = true;
            state.bringup_completed = true;
        } else {
            state.bringup_completed = true;
        }
    }

    if bringing_up {
        state.bringup_suppressed_polls = state.bringup_suppressed_polls.saturating_add(1);
    }
    let drops = dropped_frames;
    if bringing_up && drops > 0 {
        state.startup_drops_drained = true;
    }
    if !bringing_up {
        if let Some(s) = stream_status.filter(|_| drops > 0) {
            s.bridge_dropped_frames.fetch_add(drops, Ordering::Relaxed);
        }
        if state.latches.drift_drops.observe(drops > 0) {
            log::warn!(
                "{} Drifting detected: {} audio blocks discarded (capture > playback).",
                "⚠️".yellow(),
                drops
            );
        }
    }
}

fn emit_throttled_latency_and_timing(
    rt_status: &RtStatusFlags,
    stream_status: Option<&StreamStatusFlags>,
    state: &mut PollState,
) {
    state.telemetry_throttle = state.telemetry_throttle.wrapping_add(1);
    if state.telemetry_throttle.wrapping_rem(100) != 0 {
        return;
    }

    let cap = stream_status
        .map(|ss| latency_summary(&ss.capture_hist))
        .unwrap_or_default();
    let dsp = latency_summary(&rt_status.latency_hist);
    let rec = stream_status
        .map(|ss| latency_summary(&ss.record_hist))
        .unwrap_or_default();
    let pb = stream_status
        .map(|ss| latency_summary(&ss.playback_hist))
        .unwrap_or_default();
    let e2e = stream_status
        .map(|ss| latency_summary(&ss.e2e_hist))
        .unwrap_or_default();

    let total_calls = rt_status.latency_hist.total_count();

    log::info!(
        "{} RT Latency Breakdown (10s) [{} blocks]:\n\
         ├─ 1. Capture Total:        min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs\n\
         ├─ 2. DSP Core:             min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs\n\
         ├─ 3. Record Enqueue:       min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs\n\
         ├─ 4. Playback Total:       min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs\n\
         └─ 5. Capture↦Playback E2E: min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs",
        "📊".bright_blue(),
        total_calls,
        cap.min_ns / 1000,
        cap.mean_ns / 1000,
        cap.p50_ns / 1000,
        cap.p99_ns / 1000,
        cap.p999_ns / 1000,
        cap.max_ns / 1000,
        dsp.min_ns / 1000,
        dsp.mean_ns / 1000,
        dsp.p50_ns / 1000,
        dsp.p99_ns / 1000,
        dsp.p999_ns / 1000,
        dsp.max_ns / 1000,
        rec.min_ns / 1000,
        rec.mean_ns / 1000,
        rec.p50_ns / 1000,
        rec.p99_ns / 1000,
        rec.p999_ns / 1000,
        rec.max_ns / 1000,
        pb.min_ns / 1000,
        pb.mean_ns / 1000,
        pb.p50_ns / 1000,
        pb.p99_ns / 1000,
        pb.p999_ns / 1000,
        pb.max_ns / 1000,
        e2e.min_ns / 1000,
        e2e.mean_ns / 1000,
        e2e.p50_ns / 1000,
        e2e.p99_ns / 1000,
        e2e.p999_ns / 1000,
        e2e.max_ns / 1000,
    );

    if let Some(ss) = stream_status {
        ss.capture_hist.reset();
        ss.record_hist.reset();
        ss.playback_hist.reset();
        ss.e2e_hist.reset();
        #[cfg(any(feature = "testing", feature = "heap-audit"))]
        ss.e2e_exact.reset();
    }
    rt_status.latency_hist.reset();

    let Some(ss) = stream_status else {
        return;
    };
    let cap_ticks = ss.capture_host_ticks.load(Ordering::Relaxed);
    let pb_ticks = ss.playback_host_ticks.load(Ordering::Relaxed);
    if cap_ticks == 0 || pb_ticks == 0 {
        return;
    }

    let cap_now = ss.capture_host_now.load(Ordering::Relaxed);
    let pb_now = ss.playback_host_now.load(Ordering::Relaxed);
    let cap_delay = ss.capture_host_delay.load(Ordering::Relaxed);
    let pb_delay = ss.playback_host_delay.load(Ordering::Relaxed);

    let tick_delta = pb_ticks.wrapping_sub(cap_ticks);
    let time_delta_us = if pb_now > 0 && cap_now > 0 && pb_now > cap_now {
        ((pb_now - cap_now) / 1000) as u64
    } else {
        0
    };

    let rate = rt_status.active_rate.load(Ordering::Relaxed);
    let samples = rt_status.last_n_samples.load(Ordering::Relaxed);
    let quantum_us = if rate > 0 && samples > 0 {
        (samples as u64 * 1_000_000) / rate as u64
    } else {
        0
    };
    let cap_delay_us = if rate > 0 {
        (cap_delay.max(0) as u64 * 1_000_000) / rate as u64
    } else {
        0
    };
    let pb_delay_us = if rate > 0 {
        (pb_delay.max(0) as u64 * 1_000_000) / rate as u64
    } else {
        0
    };

    log::info!(
        "{} PW Stream Timing: cap↦pb gap={} µs | tick_delta={} | cap_ticks={} pb_ticks={} | cap_delay={} µs pb_delay={} µs | quantum={} µs",
        "⏱️".bright_blue(),
        time_delta_us,
        tick_delta,
        cap_ticks,
        pb_ticks,
        cap_delay_us,
        pb_delay_us,
        quantum_us,
    );
}

fn check_audio_deadline(rt_status: &RtStatusFlags, duration: Duration, state: &mut PollState) {
    let rate_val = rt_status.active_rate.load(Ordering::Relaxed);
    let samples_val = rt_status.last_n_samples.load(Ordering::Relaxed);

    if rate_val > 0 && samples_val > 0 {
        let budget_us = (samples_val as f64 / rate_val as f64) * 1_000_000.0;
        let elapsed_us = duration.as_micros() as f64;

        let deadline_exceeded = elapsed_us > budget_us;
        if state.latches.deadline_exceeded.observe(deadline_exceeded) {
            log::error!(
                "[E2001 | PROCESSING_OVERLOAD] Audio deadline exceeded (possible xrun): \
                 exec_time_us={} budget_us={} n_samples={} rate={} — verify model \
                 topology or reduce system load.",
                elapsed_us as u64,
                budget_us as u64,
                samples_val,
                rate_val
            );
        }
    }
}

fn log_gate_transition(throttle: &mut GateTransitionThrottle, message: &str) {
    if let Some(count) = throttle.observe(Instant::now()) {
        if count > 1 {
            log::info!("{message} (×{count})");
        } else {
            log::info!("{message}");
        }
    }
}

fn poll_gate_transitions(
    rt_status: &RtStatusFlags,
    was_silent: bool,
    was_fading: bool,
    state: &mut PollState,
) -> (bool, bool) {
    let current_silent = rt_status.check_flag(RT_STATUS_IS_SILENT);

    if current_silent != was_silent {
        let message = if current_silent {
            format!(
                "{} Silent Mode: Input below threshold (Gate Closed).",
                "🔇".blue()
            )
        } else {
            format!(
                "{} Audio Signal Detected: DSP processing resumed.",
                "🔊".green()
            )
        };
        log_gate_transition(&mut state.gate_throttle, &message);
    }

    let current_fading = rt_status.check_flag(RT_STATUS_IS_FADING);

    if current_fading != was_fading && current_fading {
        let message = format!("{} Signal Transition: Gate in Fade-In/Out.", "🌓".yellow());
        log_gate_transition(&mut state.gate_throttle, &message);
    }

    (current_silent, current_fading)
}

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
