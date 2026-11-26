// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! State structures, episode latches, and latency summaries for RT telemetry.

use crate::standalone::rt_setup::affinity::CpuSelectionReceipt;
use neural_amp_modeler_rs::dsp::telemetry::{AtomicHistogram, LatencyHistogram};
use std::time::{Duration, Instant};

/// Read-only view of the engine latency-histogram surface.
pub trait LatencyHistogramView {
    fn percentile(&self, p: f32) -> u64;
    fn mean(&self) -> u64;
    fn exact_min(&self) -> u64;
    fn exact_max(&self) -> u64;
}

impl LatencyHistogramView for AtomicHistogram {
    fn percentile(&self, p: f32) -> u64 {
        self.get_percentile(p)
    }

    fn mean(&self) -> u64 {
        self.get_mean()
    }

    fn exact_min(&self) -> u64 {
        self.get_exact_min()
    }

    fn exact_max(&self) -> u64 {
        self.get_exact_max()
    }
}

impl LatencyHistogramView for LatencyHistogram {
    fn percentile(&self, p: f32) -> u64 {
        self.get_percentile(p)
    }

    fn mean(&self) -> u64 {
        self.get_mean()
    }

    fn exact_min(&self) -> u64 {
        self.get_exact_min()
    }

    fn exact_max(&self) -> u64 {
        self.get_exact_max()
    }
}

/// Latency summary emitted by the RT telemetry dashboard.
///
/// Engine percentiles report the upper edge of the matched bucket (≤ 12.5 %
/// error for the log-linear histogram), which can numerically exceed the exact
/// maximum when the tail sample sits inside that bucket. Consumers therefore
/// clamp every percentile to `max_ns` without touching the engine histogram's
/// bucket resolution, preserving `p50 <= p99 <= p999 <= max_ns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LatencySummary {
    /// Exact minimum latency (ns).
    pub min_ns: u64,
    /// Mean latency (ns).
    pub mean_ns: u64,
    /// Median latency (ns), clamped to `max_ns`.
    pub p50_ns: u64,
    /// 99th percentile latency (ns), clamped to `max_ns`.
    pub p99_ns: u64,
    /// 99.9th percentile latency (ns), clamped to `max_ns`.
    pub p999_ns: u64,
    /// Exact maximum latency observed (ns).
    pub max_ns: u64,
}

/// Summarises a latency histogram with monotonic percentiles clamped to the
/// exact maximum: `p50 <= p99 <= p999 <= max`.
pub fn latency_summary(hist: &impl LatencyHistogramView) -> LatencySummary {
    let max_ns = hist.exact_max();
    let p50_ns = hist.percentile(0.50).min(max_ns);
    let p99_ns = hist.percentile(0.99).min(max_ns).max(p50_ns);
    let p999_ns = hist.percentile(0.999).min(max_ns).max(p99_ns);
    LatencySummary {
        min_ns: hist.exact_min(),
        mean_ns: hist.mean(),
        p50_ns,
        p99_ns,
        p999_ns,
        max_ns,
    }
}

/// Minimum spacing between gate-transition `info` lines.
///
/// A gate threshold sitting near the guitar's noise floor can flap at the poll
/// rate (≤ 10 Hz), burying real errors under a burst of "Silent Mode"/"Audio
/// Signal Detected" lines. Gate transitions are therefore rate-limited to one
/// line per second; suppressed transitions are counted and folded into the next
/// emitted line as "(×N)".
pub const GATE_LOG_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Rate limiter for gate-transition logs (deterministic; clock injected as the
/// `now` argument so tests advance time without sleeping).
#[derive(Debug, Clone, Default)]
pub struct GateTransitionThrottle {
    last_emit: Option<Instant>,
    suppressed: u32,
}

impl GateTransitionThrottle {
    /// Records a gate transition observed at `now`.
    ///
    /// Returns `Some(count)` when the line should be logged — `count` is the
    /// number of transitions folded into this line (`1` = no suppression) — or
    /// `None` when the transition is suppressed by the rate limit.
    pub fn observe(&mut self, now: Instant) -> Option<u32> {
        let allowed = match self.last_emit {
            None => true,
            Some(prev) => now.duration_since(prev) >= GATE_LOG_MIN_INTERVAL,
        };
        if allowed {
            self.last_emit = Some(now);
            let count = self.suppressed.saturating_add(1);
            self.suppressed = 0;
            Some(count)
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }
}

/// Per-signal episode latch.
///
/// A condition observed continuously by the control loop (starvation, queue
/// saturation, rate churn, clipping, …) emits **at most once per episode**:
/// the first poll that observes it after it has cleared. The latch re-arms
/// only when a poll observes the condition absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatchedSignal {
    pub(super) active: bool,
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
    /// Non-finite input containment (`RT_STATUS_NON_FINITE_INPUT_DETECTED`):
    /// the engine detected NaN/Inf entering the pipeline and
    /// sanitized both the inputs and the delivered outputs.
    pub non_finite_input: LatchedSignal,
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
    /// Early playback callbacks counter (`playback_early_callbacks`): no new
    /// bridge generation because the callback fired ahead of schedule.
    pub playback_early: LatchedSignal,
    /// Fail-closed mute deliveries (`playback_muted_callbacks`).
    pub playback_muted: LatchedSignal,
    /// Clock-drift dropped-frames counter (`DspBridge::dropped_frames`).
    pub drift_drops: LatchedSignal,
    /// Audio deadline exceeded (`dsp_cycle_time > quantum budget`).
    pub deadline_exceeded: LatchedSignal,
    /// PW log ring saturation (`pw_host::rt_log_ring::pw_log_dropped`): PW
    /// log events discarded with the SPSC ring full (off-RT offload budget).
    pub pw_log_dropped: LatchedSignal,
    /// Playback callbacks running on a thread other than the capture-marked RT
    /// data thread (`StreamStatusFlags::playback_thread_divergence`).
    pub playback_thread_divergence: LatchedSignal,
    /// Installed cabinet slot bypassed because its rate differs from the host
    /// quantum rate (`StreamStatusFlags::cab_rate_stale`).
    pub cab_rate_stale: LatchedSignal,
}

/// Mutable state for `poll_rt_status`, replacing function-scoped statics
/// to make the function testable and re-entrant.
#[derive(Debug, Clone, Default)]
pub struct PollState {
    pub hugepage_synced: bool,
    pub telemetry_throttle: u32,
    pub cpu_receipt: Option<CpuSelectionReceipt>,
    /// Per-signal episode latches.
    pub latches: TelemetryLatches,
    /// Last observed cumulative starvation count.
    pub last_starvation: u32,
    /// Last observed cumulative early-playback-callback count.
    pub last_early: u32,
    /// Last observed cumulative fail-closed-mute delivery count.
    pub last_muted: u32,
    /// Last observed cumulative PW-log ring discard count
    /// (`rt_log_ring::pw_log_dropped`, `pw_log_dropped_last`).
    pub pw_log_dropped_last: u64,
    /// Last observed cumulative playback-thread divergence count.
    pub playback_thread_divergence_last: u64,
    /// Last observed `playback_quantums` value; used to re-arm the bridge
    /// bring-up suppression budget whenever playback makes progress.
    pub last_bringup_quanta: u32,
    /// Number of consecutive polls spent suppressing bridge drift drops in the
    /// current bring-up window. Caps the window so a stalled playback stream
    /// cannot suppress (and discard) drift drops indefinitely.
    pub bringup_suppressed_polls: u32,
    /// Whether the monitor loop has observed and polled during `quanta < PLAYBACK_BRINGUP_QUANTUMS`.
    pub has_polled_in_bringup: bool,
    /// Whether the initial startup drops (ramp / pre-roll silence skew) have been drained.
    pub startup_drops_drained: bool,
    /// Whether the bring-up transient has been drained and closed for the current
    /// streaming session.
    pub bringup_completed: bool,
    /// Gate-transition log rate limiter.
    pub gate_throttle: GateTransitionThrottle,
}

impl PollState {
    /// Creates a default `PollState`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a `PollState` carrying a CPU selection receipt for honest telemetry reporting.
    pub fn with_cpu_receipt(receipt: Option<CpuSelectionReceipt>) -> Self {
        Self {
            hugepage_synced: false,
            telemetry_throttle: 0,
            cpu_receipt: receipt,
            latches: TelemetryLatches::default(),
            last_starvation: 0,
            last_early: 0,
            last_muted: 0,
            pw_log_dropped_last: 0,
            playback_thread_divergence_last: 0,
            last_bringup_quanta: 0,
            bringup_suppressed_polls: 0,
            has_polled_in_bringup: false,
            startup_drops_drained: false,
            bringup_completed: false,
            gate_throttle: GateTransitionThrottle::default(),
        }
    }
}
