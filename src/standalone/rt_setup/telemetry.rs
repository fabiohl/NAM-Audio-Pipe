// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Telemetry and RT status monitoring for the audio engine.
//!
//! Translates atomic signals from the DSP thread into diagnostic logs for
//! the main loop, acting as the "dashboard" of NAM-Audio-Pipe.

use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Mutable state for `poll_rt_status`, replacing function-scoped statics
/// to make the function testable and re-entrant.
#[derive(Debug, Clone, Default)]
pub struct PollState {
    pub hugepage_synced: bool,
    pub telemetry_throttle: u32,
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
    // Reads error flags set atomically by configure_realtime_thread in the DSP callback
    // and emits the corresponding diagnostic messages from the main thread.
    // On full success, prints the classic thread optimization confirmation.

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

        rt_status.rt_priority.store(-1, Ordering::Relaxed);

        if is_fifo {
            let cpu = rt_status.rt_cpu.load(Ordering::Relaxed);
            log::info!(
                "{} Thread Optimization: Dedicated core {} with Real-Time priority (FIFO, Prio={})",
                "🔍".blue(),
                cpu.to_string().cyan(),
                prio.to_string().green()
            );
        } else {
            NamDiagnostic::new(NamErrorCode::RtPriorityDenied, sys)
                .message(format!(
                    "DSP thread is NOT in SCHED_FIFO (priority = {}). \
                     Audio may experience jitter and xruns.",
                    prio
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
            let p50 = rt_status.latency_hist.get_percentile(0.50) / 1000;
            let p99 = rt_status.latency_hist.get_percentile(0.99) / 1000;
            let exact = rt_status.latency_hist.take_exact_max() / 1000;
            let total_calls = rt_status.latency_hist.total_count();

            log::info!(
                "{} DSP Telemetry (10s): {}µs (Median) | {}µs (P99) | {}µs (Exact Max) [{} blocks]",
                "📊".bright_blue(),
                p50,
                p99,
                exact,
                total_calls
            );
            rt_status.latency_hist.reset();

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
