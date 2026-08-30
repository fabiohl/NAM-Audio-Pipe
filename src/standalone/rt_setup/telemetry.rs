// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Telemetry and RT status monitoring for the audio engine.
//!
//! Translates atomic signals from the DSP thread into diagnostic logs for
//! the main loop, acting as the "dashboard" of NAM-Audio-Pipe.

use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Mutable state for `poll_rt_status`, replacing function-scoped statics
/// to make the function testable and re-entrant.
#[derive(Debug, Clone, Default)]
pub struct PollState {
    pub hugepage_synced: bool,
    pub telemetry_throttle: u32,
    pub cpu_receipt: Option<super::affinity::CpuSelectionReceipt>,
}

impl PollState {
    /// Creates a default `PollState`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a `PollState` carrying a CPU selection receipt for honest telemetry reporting.
    pub fn with_cpu_receipt(receipt: Option<super::affinity::CpuSelectionReceipt>) -> Self {
        Self {
            hugepage_synced: false,
            telemetry_throttle: 0,
            cpu_receipt: receipt,
        }
    }
}

/// Reads atomic RT status flags and emits monitoring logs to the user.
///
/// This function acts as the "dashboard" of NAM-Audio-Pipe. It is called periodically
/// to translate the technical signals coming from the audio thread (which is silent and ultra-fast)
/// into understandable messages, performance warnings, and latency telemetry.
///
/// Returns a tuple (current_silent, current_fading) for state control in the main loop.
pub fn poll_rt_status(
    rt_status: &RtStatusFlags,
    sys: &SystemSnapshot,
    was_silent: bool,
    was_fading: bool,
    bridge: &neural_amp_modeler_rs::dsp::pipeline::DspBridge,
    state: &mut PollState,
) -> (bool, bool) {
    let current_bits = rt_status.status_bits.load(Ordering::Relaxed);
    rt_status
        .flags_seen
        .fetch_or(current_bits, Ordering::Relaxed);

    // 1. MEMORY MANAGEMENT (Garbage Collection):
    // If the cleanup channel is full, it means we are swapping neural models
    // faster than the system can discard old ones. We prioritize audio
    // "leaking" memory temporarily to avoid clicks (drops) in the sound.
    if rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW) {
        NamDiagnostic::new(NamErrorCode::GcOverflow, sys)
            .message("Garbage Collection (GC) channel overflow detected.")
            .hint(
                "The audio thread had to leak memory to avoid dropouts in the hot-path. \
                   This can occur during rapid model swaps. \
                   NAM-Audio-Pipe will drain the buffer aggressively now.",
            )
            .emit_warning();
    }

    if rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_TIER3) {
        NamDiagnostic::new(NamErrorCode::GcOverflow, sys)
            .message("Garbage Collection (GC) cascade reached Tier 3 (overflow buffer).")
            .hint(
                "The SPSC channel and parking lot are both full — items are being parked \
                   in the overflow buffer. This indicates sustained GC pressure. \
                   NAM-Audio-Pipe will drain the overflow buffer aggressively now.",
            )
            .emit_warning();
    }

    if rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_CORRUPTED) {
        NamDiagnostic::new(NamErrorCode::GcCorrupted, sys)
            .message("Garbage Collection overflow buffer corruption detected.")
            .hint(
                "A GC slot had inconsistent type/pointer data. The pointer was leaked \
                   to avoid undefined behavior (Box::from_raw with wrong type). \
                   This should never happen — report it.",
            )
            .emit();
    }

    // Command Budgeting telemetry (F-RB-011 / T2.5): the RT callback drains
    // under fixed per-quantum budgets. These flags make saturation explicit —
    // no command is ever lost; the excess is deferred to the next callback.
    if rt_status
        .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PARAM_QUEUE_BACKLOG)
    {
        NamDiagnostic::new(NamErrorCode::ParamChannelFull, sys)
            .message("Command queue backlog: the scalar parameter drain budget was exhausted.")
            .hint(
                "A producer (CLI/UI/automation) filled the command queue faster than the \
                 per-callback budget (16) can drain. The remainder is processed by the \
                 next callback — audio deadline preserved.",
            )
            .emit_warning();
    }

    if rt_status
        .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_DEFERRED)
    {
        log::info!(
            "Structural command deferred to the next callback (structural budget 1/callback) — FIFO order preserved."
        );
    }

    if rt_status
        .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_SUPERSEDED)
    {
        log::info!(
            "Deferred structural command superseded by a newer same-kind command; obsolete resources discarded off-RT (coalescing)."
        );
    }

    if rt_status
        .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED)
    {
        log::error!(
            "WaveNet slimmable slice_channels rebuild failed — model may run in reduced state."
        );
    }

    if rt_status
        .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_SLIMMABLE_RESET_FAILED)
    {
        log::error!("ContainerModel submodel reset failed — model may run in previous state.");
    }

    // 2. RATE CHANGE (Sample Rate):
    // Warns when the audio server (PipeWire) changes the sampling frequency
    // (e.g. changed from 44.100 to 48.000 beats per second).
    let rate_notif = rt_status.active_rate_changed.swap(0, Ordering::Relaxed);
    if rate_notif != 0 {
        log::info!(
            "{} RT callback activated resampler with rate = {} Hz",
            "✅".green(),
            rate_notif
        );
    }

    // 3. DIGITAL DISTORTION (Clipping):
    // The equivalent of the "red LED" on mixing consoles. Indicates that the signal volume
    // exceeded the maximum limit of digital processing.
    if rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HAS_CLIPPED) {
        log::warn!(
            "{} Clipping detected! Consider reducing the input and/or output gain.",
            "🔥".bright_red().bold()
        );
    }

    // 3.5 HUGE PAGE STATUS:
    // Sync from mirror buffer global and log once.
    if !state.hugepage_synced {
        neural_amp_modeler_rs::dsp::mirror_buf::sync_huge_page_flag(rt_status);
        state.hugepage_synced = true;
    }
    if rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HUGEPAGE_OK) {
        log::info!(
            "{} HugeTLB explicit 2 MB pages active — reduced TLB pressure on DSP thread.",
            "✅".green()
        );
    }
    if rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_THP_ACTIVE) {
        log::info!(
            "{} Transparent Huge Pages (THP) advice active — kernel may promote to 2 MB.",
            "ℹ️".green()
        );
    }

    // 4. REAL-TIME PRIORITY & ATOMIC ERRORS:
    // Reads error flags set atomically by configure_realtime_thread during stream
    // state transition before readiness (T3.1 / F-RES-003) and emits the corresponding
    // diagnostic messages from the main thread. On full success, prints the classic
    // thread optimization confirmation.

    let aff_err = rt_status.rt_affinity_err.swap(0, Ordering::Relaxed);
    let sched_err = rt_status.rt_sched_err.swap(0, Ordering::Relaxed);
    let getsched_err = rt_status.rt_getsched_err.swap(0, Ordering::Relaxed);
    let target_cpu = rt_status.rt_target_cpu.swap(-1, Ordering::Relaxed);

    if aff_err == -1 {
        log::error!(
            "CPU {} is out of bounds (CPU_SETSIZE={}). NAM-Audio-Pipe will continue running without CPU affinity.\n\
             [E2301 | CPU_OUT_OF_BOUNDS] cpu={} max={}",
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
            "⚠️ pthread_setschedparam(SCHED_FIFO, 90) failed (errno={}).\n\
             [E2302 | RT_SCHED_FAILED] Check ulimit -r and rtkit permissions.\n",
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
        let is_fifo =
            rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
        let policy = rt_status.rt_policy.load(Ordering::Relaxed);
        let tid = rt_status.rt_tid.load(Ordering::Relaxed);
        let cpu = rt_status.rt_cpu.load(Ordering::Relaxed);

        rt_status.rt_priority.store(-1, Ordering::Relaxed);

        if is_fifo || policy == libc::SCHED_RR {
            let policy_name = if policy == libc::SCHED_RR {
                "RR"
            } else {
                "FIFO"
            };

            let is_dedicated = state
                .cpu_receipt
                .as_ref()
                .map(|r| r.is_dedicated)
                .unwrap_or(false);

            if is_dedicated {
                log::info!(
                    "{} Thread Optimization: Dedicated core {} with Real-Time priority ({}, Prio={}, TID={})",
                    "🔍".blue(),
                    cpu.to_string().cyan(),
                    policy_name,
                    prio.to_string().green(),
                    tid
                );
            } else {
                let reason_str = state
                    .cpu_receipt
                    .as_ref()
                    .map(|r| match &r.reason {
                        super::affinity::CpuSelectionReason::ConservativeHeuristic {
                            explanation,
                            ..
                        } => *explanation,
                        super::affinity::CpuSelectionReason::ExplicitCli { .. } => {
                            "Explicit CLI pinning (non-isolated)"
                        }
                        super::affinity::CpuSelectionReason::FullyIsolated { .. } => {
                            "Fully isolated core"
                        }
                    })
                    .unwrap_or("Conservative heuristic / unverified topology");

                log::info!(
                    "{} Thread Optimization: Conservative heuristic core {} with Real-Time priority ({}, Prio={}, TID={}) [Reason: {}]",
                    "🔍".blue(),
                    cpu.to_string().cyan(),
                    policy_name,
                    prio.to_string().green(),
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
            NamDiagnostic::new(NamErrorCode::RtPriorityDenied, sys)
                .message(format!(
                    "DSP thread is NOT in a Real-Time scheduling policy (policy = {}, priority = {}, TID = {}). \
                     Audio may experience jitter and xruns.",
                    policy_str, prio, tid
                ))
                .hint(
                    "Check if your user has RT permission (ulimit -r) \
                     or if the system has rtkit/PipeWire configured correctly.",
                )
                .emit_warning();
        }
    }

    // 5. PROCESSING OVERLOAD (Overloads):
    // Warns if the processor (CPU) is not fast enough to compute
    // the neural network before the next audio block is needed.
    let overloads = rt_status.dsp_overloads.swap(0, Ordering::Relaxed);
    if overloads > 0 {
        rt_status.xruns.fetch_add(overloads, Ordering::Relaxed);
        log::warn!(
            "{} CPU overload ({} buffers). Consider using a lighter model or a faster processor.",
            "🚨".red(),
            overloads
        );
    }

    // 5.5 BUFFER MISS (PipeWire):
    // PipeWire failed to provide a buffer — either on the capture or playback side.
    let input_buffer_miss = rt_status.input_buffer_miss.swap(0, Ordering::Relaxed);
    if input_buffer_miss > 0 {
        log::warn!(
            "{} PipeWire capture buffer miss ({} xruns). Check system load or buffer size.",
            "📻".yellow(),
            input_buffer_miss
        );
    }
    let output_buffer_miss = rt_status.output_buffer_miss.swap(0, Ordering::Relaxed);
    if output_buffer_miss > 0 {
        log::warn!(
            "{} PipeWire playback buffer miss ({} xruns). Check system load or buffer size.",
            "📢".yellow(),
            output_buffer_miss
        );
    }

    // 5.55 HOST SPA FORMAT CONTRACT VIOLATION (T4.3 / G-RB-001):
    // The audio host handed buffers or negotiated a format diverging from the
    // strict F32P planar stereo contract (raised by the RT harness or by the
    // param_changed listeners). The backend state machine acknowledges the
    // degraded/error state here with a structured diagnostic.
    if rt_status.check_and_clear_flag(RT_STATUS_HOST_CONTRACT_VIOLATION) {
        NamDiagnostic::new(NamErrorCode::SpaFormatContractViolation, sys)
            .message(
                "Audio host violated the strict SPA format contract (F32P planar stereo, \
                 2 channels FL/FR).",
            )
            .hint(
                "Check that the PipeWire graph is not forcing a mono, interleaved, S16 or \
                 surround negotiation onto the NAM streams (e.g. via WirePlumber rules).",
            )
            .emit();
    }

    // 5.6 PLAYBACK BRIDGE STARVATION (T4.2 / G-RB-001):
    // The bridge produced no new DSP block (capture paused, resampler rebuild
    // pending, clock drift or quantum miss). The playback callback delivered a
    // recycled buffer filled with deterministic silence instead of repeating
    // stale audio — expected behavior, surfaced as info telemetry.
    let playback_bridge_starvation = rt_status
        .playback_bridge_starvation
        .swap(0, Ordering::Relaxed);
    if playback_bridge_starvation > 0 {
        log::info!(
            "{} Playback delivered {} silence block(s) under bridge starvation (no stale audio repeated).",
            "🔇".blue(),
            playback_bridge_starvation
        );
    }

    // 6. CLOCK DRIFTING:
    // Occurs when you use different devices for input and output (e.g. USB Microphone
    // and P2 Headphones). If one is slightly faster than the other, the system needs to discard
    // some small audio chunks to maintain synchronization.
    let drops = bridge.drain_dropped_frames();
    if drops > 0 {
        log::warn!(
            "{} Drifting detected: {} audio blocks discarded (capture > playback).",
            "⚠️".yellow(),
            drops
        );
    }

    // 7. PERFORMANCE TELEMETRY (Latency):
    // Shows statistics of how long the CPU takes to process each block.
    // - Median (P50): The "common" processing time.
    // - P99: The worst case 99% of the time (indicates stability).
    // - Max: The highest delay peak ever recorded.
    let nanos = rt_status.dsp_cycle_time.load(Ordering::Relaxed);
    if nanos > 0 {
        let duration = Duration::from_nanos(nanos);
        state.telemetry_throttle = state.telemetry_throttle.wrapping_add(1);
        if state.telemetry_throttle.wrapping_rem(100) == 0 {
            let cap_min = rt_status.capture_hist.get_exact_min() / 1000;
            let cap_mean = rt_status.capture_hist.get_mean() / 1000;
            let cap_p50 = rt_status.capture_hist.get_percentile(0.50) / 1000;
            let cap_p99 = rt_status.capture_hist.get_percentile(0.99) / 1000;
            let cap_max = rt_status.capture_hist.take_exact_max() / 1000;

            let dsp_min = rt_status.latency_hist.get_exact_min() / 1000;
            let dsp_mean = rt_status.latency_hist.get_mean() / 1000;
            let dsp_p50 = rt_status.latency_hist.get_percentile(0.50) / 1000;
            let dsp_p99 = rt_status.latency_hist.get_percentile(0.99) / 1000;
            let dsp_max = rt_status.latency_hist.take_exact_max() / 1000;

            let rec_min = rt_status.record_hist.get_exact_min() / 1000;
            let rec_mean = rt_status.record_hist.get_mean() / 1000;
            let rec_p50 = rt_status.record_hist.get_percentile(0.50) / 1000;
            let rec_p99 = rt_status.record_hist.get_percentile(0.99) / 1000;
            let rec_max = rt_status.record_hist.take_exact_max() / 1000;

            let pb_min = rt_status.playback_hist.get_exact_min() / 1000;
            let pb_mean = rt_status.playback_hist.get_mean() / 1000;
            let pb_p50 = rt_status.playback_hist.get_percentile(0.50) / 1000;
            let pb_p99 = rt_status.playback_hist.get_percentile(0.99) / 1000;
            let pb_max = rt_status.playback_hist.take_exact_max() / 1000;

            let e2e_min = rt_status.e2e_hist.get_exact_min() / 1000;
            let e2e_mean = rt_status.e2e_hist.get_mean() / 1000;
            let e2e_p50 = rt_status.e2e_hist.get_percentile(0.50) / 1000;
            let e2e_p99 = rt_status.e2e_hist.get_percentile(0.99) / 1000;
            let e2e_max = rt_status.e2e_hist.take_exact_max() / 1000;

            let total_calls = rt_status.latency_hist.total_count();

            log::info!(
                "{} RT Latency Breakdown (10s) [{} blocks]:\n\
                 ├─ 1. Capture Total:        min={}µs | mean={}µs | p50={}µs | p99={}µs | max={}µs\n\
                 ├─ 2. DSP Core:             min={}µs | mean={}µs | p50={}µs | p99={}µs | max={}µs\n\
                 ├─ 3. Record Enqueue:       min={}µs | mean={}µs | p50={}µs | p99={}µs | max={}µs\n\
                 ├─ 4. Playback Total:       min={}µs | mean={}µs | p50={}µs | p99={}µs | max={}µs\n\
                 └─ 5. Capture↦Playback E2E: min={}µs | mean={}µs | p50={}µs | p99={}µs | max={}µs",
                "📊".bright_blue(),
                total_calls,
                cap_min,
                cap_mean,
                cap_p50,
                cap_p99,
                cap_max,
                dsp_min,
                dsp_mean,
                dsp_p50,
                dsp_p99,
                dsp_max,
                rec_min,
                rec_mean,
                rec_p50,
                rec_p99,
                rec_max,
                pb_min,
                pb_mean,
                pb_p50,
                pb_p99,
                pb_max,
                e2e_min,
                e2e_mean,
                e2e_p50,
                e2e_p99,
                e2e_max,
            );

            rt_status.capture_hist.reset();
            rt_status.latency_hist.reset();
            rt_status.record_hist.reset();
            rt_status.playback_hist.reset();
            rt_status.e2e_hist.reset();

            let cap_ticks = rt_status.capture_host_ticks.load(Ordering::Relaxed);
            let pb_ticks = rt_status.playback_host_ticks.load(Ordering::Relaxed);
            let cap_now = rt_status.capture_host_now.load(Ordering::Relaxed);
            let pb_now = rt_status.playback_host_now.load(Ordering::Relaxed);
            let cap_delay = rt_status.capture_host_delay.load(Ordering::Relaxed);
            let pb_delay = rt_status.playback_host_delay.load(Ordering::Relaxed);

            if cap_ticks > 0 && pb_ticks > 0 {
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
        }

        // 8. DEADLINE CHECK:
        // If execution time exceeds the "budget" given by the audio system,
        // we generate a diagnostic error explaining what failed.
        let rate_val = rt_status.active_rate.load(Ordering::Relaxed);
        let samples_val = rt_status.last_n_samples.load(Ordering::Relaxed);

        if rate_val > 0 && samples_val > 0 {
            let budget_us = (samples_val as f64 / rate_val as f64) * 1_000_000.0;
            let elapsed_us = duration.as_micros() as f64;

            if elapsed_us > budget_us {
                NamDiagnostic::new(NamErrorCode::ProcessingOverload, sys)
                    .message("Audio deadline exceeded (Possible Xrun detected)")
                    .hint("Verify model topology or reduce system load.")
                    .param("exec_time_us", elapsed_us as u64)
                    .param("budget_us", budget_us as u64)
                    .param("n_samples", samples_val)
                    .param("rate", rate_val)
                    .emit();
            }
        }
    }

    // Silence transition detection
    let current_silent =
        rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_IS_SILENT);

    if current_silent != was_silent {
        if current_silent {
            log::info!(
                "{} Silent Mode: Input below threshold (Gate Closed).",
                "🔇".blue()
            );
        } else {
            log::info!(
                "{} Audio Signal Detected: DSP processing resumed.",
                "🔊".green()
            );
        }
    }

    // Fading transition detection
    let current_fading =
        rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_IS_FADING);

    if current_fading != was_fading && current_fading {
        log::info!("{} Signal Transition: Gate in Fade-In/Out.", "🌓".yellow());
    }

    (current_silent, current_fading)
}

#[cfg(test)]
#[path = "telemetry_test.rs"]
mod telemetry_test;
