// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Telemetry and RT status monitoring for the audio engine.
//!
//! Translates atomic signals from the DSP thread into diagnostic logs for
//! the main loop, acting as the "dashboard" of NAM-Audio-Pipe.
//!
//! Sprint 6 / T6.1: runtime telemetry emits concise `log::*` lines
//! (`[Exxxx | MNEMONIC]` code + cause + recovery hint) **without** the
//! `DiagnosticBundle` support block — the `──── Recent Log Trace ────`
//! render is reserved for explicit `--diagnose`/`--diagnose-full` dumps and
//! crash/panic reports. Sprint 6 / T6.2: every recurrent signal is latched
//! ([`TelemetryLatches`]) so a continuous condition warns at most once per
//! episode instead of once per control-loop iteration.

use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Per-signal episode latch (Sprint 6 / T6.2).
///
/// A condition observed continuously by the control loop (starvation, queue
/// saturation, rate churn, clipping, …) emits **at most once per episode**:
/// the first poll that observes it after it has cleared. The latch re-arms
/// only when a poll observes the condition absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatchedSignal {
    active: bool,
}

impl LatchedSignal {
    /// Records one poll observation of a signal. Returns `true` exactly on
    /// the first observation of each episode — callers emit the log line
    /// only when `true`.
    #[inline]
    pub fn observe(&mut self, active_now: bool) -> bool {
        if active_now {
            let first = !self.active;
            self.active = true;
            first
        } else {
            self.active = false;
            false
        }
    }
}

/// Latching state for every recurrent signal translated by `poll_rt_status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TelemetryLatches {
    /// GC channel overflow (`RT_STATUS_GC_OVERFLOW`).
    pub gc_overflow: LatchedSignal,
    /// GC cascade Tier 3 (`RT_STATUS_GC_TIER3`).
    pub gc_tier3: LatchedSignal,
    /// GC overflow buffer corruption (`RT_STATUS_GC_CORRUPTED`).
    pub gc_corrupted: LatchedSignal,
    /// Scalar parameter queue backlog (`RT_STATUS_PARAM_QUEUE_BACKLOG`).
    pub param_backlog: LatchedSignal,
    /// Structural command deferred (`RT_STATUS_STRUCTURAL_DEFERRED`).
    pub structural_deferred: LatchedSignal,
    /// Deferred structural command superseded (`RT_STATUS_STRUCTURAL_SUPERSEDED`).
    pub structural_superseded: LatchedSignal,
    /// WaveNet slimmable slice rebuild failure (`RT_STATUS_SLIMMABLE_SLICE_FAILED`).
    pub slimmable_slice_failed: LatchedSignal,
    /// ContainerModel submodel reset failure (`RT_STATUS_SLIMMABLE_RESET_FAILED`).
    pub slimmable_reset_failed: LatchedSignal,
    /// Digital clipping (`RT_STATUS_HAS_CLIPPED`).
    pub clipping: LatchedSignal,
    /// SPA format contract violation (`RT_STATUS_HOST_CONTRACT_VIOLATION`).
    pub contract_violation: LatchedSignal,
    /// DSP CPU overload counter (`dsp_overloads`).
    pub cpu_overload: LatchedSignal,
    /// PipeWire capture buffer miss counter (`input_buffer_miss`).
    pub input_buffer_miss: LatchedSignal,
    /// PipeWire playback buffer miss counter (`output_buffer_miss`).
    pub output_buffer_miss: LatchedSignal,
    /// Playback bridge starvation counter (`playback_bridge_starvation`).
    pub playback_starvation: LatchedSignal,
    /// Clock-drift dropped-frames counter (`DspBridge::dropped_frames`).
    pub drift_drops: LatchedSignal,
    /// Audio deadline exceeded (`dsp_cycle_time > quantum budget`).
    pub deadline_exceeded: LatchedSignal,
}

/// Mutable state for `poll_rt_status`, replacing function-scoped statics
/// to make the function testable and re-entrant.
#[derive(Debug, Clone, Default)]
pub struct PollState {
    pub hugepage_synced: bool,
    pub telemetry_throttle: u32,
    pub cpu_receipt: Option<super::affinity::CpuSelectionReceipt>,
    /// Per-signal episode latches (Sprint 6 / T6.2).
    pub latches: TelemetryLatches,
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
            latches: TelemetryLatches::default(),
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
///
/// `_sys` is retained for API stability (callers/tests pass the startup snapshot);
/// runtime telemetry is intentionally bundle-free (Sprint 6 / T6.1).
pub fn poll_rt_status(
    rt_status: &RtStatusFlags,
    _sys: &SystemSnapshot,
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
    // (T6.2) sustained pressure is latched: one concise warning per episode.
    let gc_overflow =
        rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_OVERFLOW);
    if state.latches.gc_overflow.observe(gc_overflow) {
        log::warn!(
            "[E3101 | GC_OVERFLOW] Garbage Collection (GC) channel overflow detected — \
             the audio thread had to leak memory to avoid dropouts in the hot-path; \
             NAM-Audio-Pipe will drain the buffer aggressively now."
        );
    }

    let gc_tier3 =
        rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_TIER3);
    if state.latches.gc_tier3.observe(gc_tier3) {
        log::warn!(
            "[E3101 | GC_OVERFLOW] GC cascade reached Tier 3 (SPSC channel and parking lot \
             both full) — items are being parked in the overflow buffer; \
             NAM-Audio-Pipe will drain the overflow buffer aggressively now."
        );
    }

    let gc_corrupted =
        rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_GC_CORRUPTED);
    if state.latches.gc_corrupted.observe(gc_corrupted) {
        log::error!(
            "[E3102 | GC_CORRUPTED] Garbage Collection overflow buffer corruption detected — \
             a GC slot had inconsistent type/pointer data; the pointer was leaked to avoid \
             undefined behavior. This should never happen — report it."
        );
    }

    // Command Budgeting telemetry (F-RB-011 / T2.5): the RT callback drains
    // under fixed per-quantum budgets. These flags make saturation explicit —
    // no command is ever lost; the excess is deferred to the next callback.
    let param_backlog = rt_status
        .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_PARAM_QUEUE_BACKLOG);
    if state.latches.param_backlog.observe(param_backlog) {
        log::warn!(
            "[E3100 | PARAM_CHANNEL_FULL] Command queue backlog: the scalar parameter drain \
             budget (16/callback) was exhausted — a producer (CLI/UI/automation) filled the \
             queue faster than it can drain; the remainder is processed by the next callback, \
             preserving the audio deadline."
        );
    }

    let structural_deferred = rt_status
        .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_DEFERRED);
    if state
        .latches
        .structural_deferred
        .observe(structural_deferred)
    {
        log::info!(
            "Structural command deferred to the next callback (structural budget 1/callback) — FIFO order preserved."
        );
    }

    let structural_superseded = rt_status
        .check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_STRUCTURAL_SUPERSEDED);
    if state
        .latches
        .structural_superseded
        .observe(structural_superseded)
    {
        log::info!(
            "Deferred structural command superseded by a newer same-kind command; obsolete resources discarded off-RT (coalescing)."
        );
    }

    let slimmable_slice_failed = rt_status.check_and_clear_flag(
        neural_amp_modeler_rs::common::spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED,
    );
    if state
        .latches
        .slimmable_slice_failed
        .observe(slimmable_slice_failed)
    {
        log::error!(
            "WaveNet slimmable slice_channels rebuild failed — model may run in reduced state."
        );
    }

    let slimmable_reset_failed = rt_status.check_and_clear_flag(
        neural_amp_modeler_rs::common::spsc::RT_STATUS_SLIMMABLE_RESET_FAILED,
    );
    if state
        .latches
        .slimmable_reset_failed
        .observe(slimmable_reset_failed)
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
    // exceeded the maximum limit of digital processing. Latched: a continuously hot signal
    // warns once per episode instead of on every control-loop iteration.
    let has_clipped =
        rt_status.check_and_clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_HAS_CLIPPED);
    if state.latches.clipping.observe(has_clipped) {
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
    let tid = rt_status.rt_tid.load(Ordering::Relaxed);

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
            "⚠️ pthread_setschedparam failed (errno={}, TID={tid}).\n\
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
                    "{} Real-Time Priority: Active ({}, Prio={}, Dedicated Core {}, TID={})",
                    "⚡".yellow(),
                    policy_name,
                    prio.to_string().green(),
                    cpu.to_string().cyan(),
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

    // 5. PROCESSING OVERLOAD (Overloads):
    // Warns if the processor (CPU) is not fast enough to compute
    // the neural network before the next audio block is needed.
    // (T6.2) latched: a sustained overload episode warns once.
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

    // 5.5 BUFFER MISS (PipeWire):
    // PipeWire failed to provide a buffer — either on the capture or playback side.
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

    // 5.55 HOST SPA FORMAT CONTRACT VIOLATION (T4.3 / G-RB-001):
    // The audio host handed buffers or negotiated a format diverging from the
    // strict F32P planar stereo contract (raised by the RT harness or by the
    // param_changed listeners). The backend state machine acknowledges the
    // degraded/error state here with a concise error. The source-side listeners
    // already name the offending stream/violation (see output_pw.rs); this
    // loop covers the RT-raised path and is latched to warn once per episode.
    let contract_violation = rt_status.check_and_clear_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    if state.latches.contract_violation.observe(contract_violation) {
        log::error!(
            "[E2304 | SPA_FORMAT_CONTRACT_VIOLATION] Audio host violated the strict SPA format \
             contract (F32P planar stereo, 2 channels FL/FR) — check that the PipeWire graph is \
             not forcing a mono, interleaved, S16 or surround negotiation onto the NAM streams \
             (e.g. via WirePlumber rules)."
        );
    }

    // 5.6 PLAYBACK BRIDGE STARVATION (T4.2 / G-RB-001):
    // The bridge produced no new DSP block (capture paused, resampler rebuild
    // pending, clock drift or quantum miss). The playback callback delivered a
    // recycled buffer filled with deterministic silence instead of repeating
    // stale audio — expected behavior, surfaced as info telemetry.
    // (T6.2) latched: sustained starvation (e.g. paused capture) informs once
    // per episode instead of every control-loop iteration.
    let playback_bridge_starvation = rt_status
        .playback_bridge_starvation
        .swap(0, Ordering::Relaxed);
    if state
        .latches
        .playback_starvation
        .observe(playback_bridge_starvation > 0)
    {
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
    // (T6.2) latched: a sustained drift episode warns once.
    let drops = bridge.drain_dropped_frames();
    if state.latches.drift_drops.observe(drops > 0) {
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

            // (T6.2) latched: sustained deadline overruns report once per
            // episode instead of once per telemetry window. The observation
            // runs on every throttle window so the latch releases as soon as
            // the DSP stays within budget.
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
