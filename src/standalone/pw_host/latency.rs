// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire graph latency calculation and publication.
//!
//! Provides algorithmic DSP processing latency accounting and publication to
//! the PipeWire graph via `SPA_PARAM_ProcessLatency` and `SPA_PARAM_Latency`
//! SPA PODs. All operations in this module run strictly outside the audio
//! callback (off-RT).

use super::output_pw::SpaPodStorage;
use crate::standalone::colors::Colorize;
use pipewire as pw;
use std::time::{Duration, Instant};

const RETRY_BASE_INTERVAL: Duration = Duration::from_millis(200);
const RETRY_MAX_INTERVAL: Duration = Duration::from_secs(2);
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(5);

/// Resolves the effective host sample rate from the RT active rate latch, falling
/// back to the last negotiated stream parameter rate when `active_rate == 0`, or
/// `48_000` Hz before either has been observed.
#[inline]
pub const fn resolve_effective_host_rate(active_rate: u32, param_rate: u32) -> u32 {
    if active_rate != 0 {
        active_rate
    } else if param_rate != 0 {
        param_rate
    } else {
        48_000
    }
}

/// Converts native oversampling latency samples (at `model_rate`) to equivalent host samples (at `host_rate`).
///
/// Uses integer ceiling conversion: `ceil(os_native * host_rate / model_rate)`.
/// When either rate is zero or oversampling is off (`os_native == 0`), returns `os_native`.
#[inline]
pub fn convert_os_latency_to_host(os_native: u32, host_rate: u32, model_rate: u32) -> u32 {
    if host_rate == 0 || model_rate == 0 || os_native == 0 {
        os_native
    } else {
        ((os_native as u64 * host_rate as u64).div_ceil(model_rate as u64)) as u32
    }
}

/// Computes total algorithmic DSP latency in samples and equivalent nanoseconds.
///
/// Total algorithmic latency is composed of:
/// 1. Cab-sim partitioned convolution latency (exactly `partition_size` samples
///    from `CabSimAdapter::latency_samples` when active, 0 when bypassed).
/// 2. Resampler filter chain group delay (from
///    `StreamingResampleBuffer::latency_samples`, 0 when host rate matches model rate).
/// 3. Oversampling half-band filter chain group delay (from
///    `OversampleEngine::latency_samples`, converted to host samples via [`convert_os_latency_to_host`]).
/// 4. Intra-cycle bridge latency (0 samples in intra-cycle lockstep execution).
///
/// Returns `(total_samples, latency_ns)`.
#[inline]
pub fn compute_dsp_latency(
    cabsim_samples: u32,
    resampler_samples: u32,
    oversample_samples: u32,
    host_rate: u32,
) -> (u32, i64) {
    let total_samples = cabsim_samples
        .saturating_add(resampler_samples)
        .saturating_add(oversample_samples);
    let latency_ns = if host_rate > 0 {
        ((i64::from(total_samples)) * 1_000_000_000) / i64::from(host_rate)
    } else {
        0
    };
    (total_samples, latency_ns)
}

/// Builds a synchronous SPA `ProcessLatency` POD (`quantum = 0.0`, `ns = 0`) for
/// PipeWire graph latency publication.
///
/// Per PipeWire's additive timing contract (`pw_stream_get_time_n`), sample-synchronous
/// DSP stages report their delay exclusively in `rate` (samples) with `ns = 0`; `ns` is
/// reserved for asynchronous wall-clock boundaries so the graph scheduler does not sum
/// the same delay twice.
///
/// The returned pod borrows `storage`; the caller must keep `storage` alive
/// and unmodified for as long as the pod is in use.
///
/// # Safety
/// The caller must not use the returned pod beyond the lifetime of `storage`.
pub unsafe fn build_process_latency_pod(
    latency_samples: u32,
    storage: &mut SpaPodStorage<1024>,
) -> anyhow::Result<&pw::spa::pod::Pod> {
    // SAFETY: zeroed builder struct is valid and initialized by `spa_pod_builder_init`.
    let mut builder: pw::spa::sys::spa_pod_builder = unsafe { std::mem::zeroed() };
    let rate_i32 = i32::try_from(latency_samples).unwrap_or(i32::MAX);
    let info = pw::spa::sys::spa_process_latency_info {
        quantum: 0.0,
        rate: rate_i32,
        ns: 0,
    };
    // SAFETY: `builder` is a zeroed struct initialized by `spa_pod_builder_init`
    // over `storage`'s live, 8-byte-aligned buffer; the built pod is validated
    // against that same storage before being returned.
    unsafe {
        pw::spa::sys::spa_pod_builder_init(
            &mut builder,
            storage.as_mut_slice().as_mut_ptr().cast(),
            storage.as_mut_slice().len() as u32,
        );
        let pod_ptr = pw::spa::sys::spa_process_latency_build(
            &mut builder,
            pw::spa::sys::SPA_PARAM_ProcessLatency,
            &info,
        );
        super::output_pw::validate_built_pod(storage, pod_ptr)
            .ok_or_else(|| anyhow::anyhow!("Failed to build SPA ProcessLatency POD"))
    }
}

/// Builds a synchronous SPA `Latency` POD (`min_ns = max_ns = 0`) for PipeWire
/// graph latency publication.
///
/// The returned pod borrows `storage`; the caller must keep `storage` alive
/// and unmodified for as long as the pod is in use.
///
/// # Safety
/// The caller must not use the returned pod beyond the lifetime of `storage`.
pub unsafe fn build_latency_pod(
    direction: pw::spa::sys::spa_direction,
    latency_samples: u32,
    storage: &mut SpaPodStorage<1024>,
) -> anyhow::Result<&pw::spa::pod::Pod> {
    // SAFETY: zeroed builder struct is valid and initialized by `spa_pod_builder_init`.
    let mut builder: pw::spa::sys::spa_pod_builder = unsafe { std::mem::zeroed() };
    let rate_i32 = i32::try_from(latency_samples).unwrap_or(i32::MAX);
    let info = pw::spa::sys::spa_latency_info {
        direction,
        min_quantum: 0.0,
        max_quantum: 0.0,
        min_rate: rate_i32,
        max_rate: rate_i32,
        min_ns: 0,
        max_ns: 0,
    };
    // SAFETY: `builder` is a zeroed struct initialized by `spa_pod_builder_init`
    // over `storage`'s live, 8-byte-aligned buffer; the built pod is validated
    // against that same storage before being returned.
    unsafe {
        pw::spa::sys::spa_pod_builder_init(
            &mut builder,
            storage.as_mut_slice().as_mut_ptr().cast(),
            storage.as_mut_slice().len() as u32,
        );
        let pod_ptr =
            pw::spa::sys::spa_latency_build(&mut builder, pw::spa::sys::SPA_PARAM_Latency, &info);
        super::output_pw::validate_built_pod(storage, pod_ptr)
            .ok_or_else(|| anyhow::anyhow!("Failed to build SPA Latency POD"))
    }
}

/// Updates synchronous latency parameters on a PipeWire stream outside the audio callback.
///
/// Builds `ProcessLatency` and `Latency` SPA PODs (`ns = 0`) using stack-local aligned
/// [`SpaPodStorage`] instances and updates the stream via `update_params`.
///
/// Must be invoked while holding the PipeWire ThreadLoop lock when called
/// from the main thread.
pub fn update_stream_latency(
    stream: &pw::stream::Stream,
    direction: pw::spa::sys::spa_direction,
    latency_samples: u32,
) -> anyhow::Result<()> {
    let mut storage_proc = SpaPodStorage::<1024>::new();
    let mut storage_lat = SpaPodStorage::<1024>::new();
    // SAFETY: storage_proc and storage_lat are stack-local and outlive the
    // synchronous `update_params` invocation.
    unsafe {
        let proc_pod = build_process_latency_pod(latency_samples, &mut storage_proc)?;
        let lat_pod = build_latency_pod(direction, latency_samples, &mut storage_lat)?;
        stream.update_params(&mut [proc_pod, lat_pod])?;
    }
    Ok(())
}

/// Identifies which host stream is the target of a latency POD publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencyStreamRole {
    /// Capture stream (`Audio/Sink`, `SPA_DIRECTION_INPUT`).
    Capture,
    /// Playback stream (`Stream/Output/Audio`, `SPA_DIRECTION_OUTPUT`).
    Playback,
}

impl LatencyStreamRole {
    /// Returns the SPA direction associated with this stream role.
    #[inline]
    pub const fn spa_direction(self) -> pw::spa::sys::spa_direction {
        match self {
            Self::Capture => pw::spa::sys::SPA_DIRECTION_INPUT,
            Self::Playback => pw::spa::sys::SPA_DIRECTION_OUTPUT,
        }
    }

    /// Returns a stable diagnostic label for off-RT logs.
    #[inline]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Capture => "capture",
            Self::Playback => "playback",
        }
    }
}

/// Off-RT seam for publishing synchronous DSP latency PODs to each host stream.
pub trait LatencyPublisher {
    /// Publishes `latency_samples` (with `ns = 0`) to `role`, returning an error
    /// if POD construction or the host `update_params` call failed.
    fn publish_latency(
        &mut self,
        role: LatencyStreamRole,
        latency_samples: u32,
    ) -> anyhow::Result<()>;
}

/// Production [`LatencyPublisher`] backed by the live PipeWire capture and playback streams.
pub struct PipewireLatencyPublisher<'a> {
    capture: &'a pw::stream::Stream,
    playback: &'a pw::stream::Stream,
}

impl<'a> PipewireLatencyPublisher<'a> {
    /// Wraps borrowed references to the instance's capture and playback streams.
    #[inline]
    pub fn new(capture: &'a pw::stream::Stream, playback: &'a pw::stream::Stream) -> Self {
        Self { capture, playback }
    }
}

impl LatencyPublisher for PipewireLatencyPublisher<'_> {
    fn publish_latency(
        &mut self,
        role: LatencyStreamRole,
        latency_samples: u32,
    ) -> anyhow::Result<()> {
        let stream = match role {
            LatencyStreamRole::Capture => self.capture,
            LatencyStreamRole::Playback => self.playback,
        };
        update_stream_latency(stream, role.spa_direction(), latency_samples)
    }
}

/// Desired or call-accepted latency state for a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyTarget {
    /// Total synchronous algorithmic DSP delay in host samples.
    pub samples: u32,
    /// Effective host sample rate used for rate-change tracking and diagnostics.
    pub host_rate: u32,
}

/// Per-stream publication state tracking desired vs. call-accepted latency targets.
#[derive(Debug, Clone)]
pub struct StreamPublicationState {
    desired: LatencyTarget,
    accepted: Option<LatencyTarget>,
    consecutive_failures: u32,
    next_retry_at: Option<Instant>,
    last_logged_error_at: Option<Instant>,
}

impl StreamPublicationState {
    fn new(initial: LatencyTarget) -> Self {
        Self {
            desired: initial,
            accepted: Some(initial),
            consecutive_failures: 0,
            next_retry_at: None,
            last_logged_error_at: None,
        }
    }

    /// Returns the currently desired latency target for this stream.
    #[inline]
    pub const fn desired(&self) -> LatencyTarget {
        self.desired
    }

    /// Returns the last target accepted by a `Ok(())` `update_stream_latency` call.
    #[inline]
    pub const fn accepted(&self) -> Option<LatencyTarget> {
        self.accepted
    }

    /// Returns the number of consecutive failed publication attempts since the last success.
    #[inline]
    pub const fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    fn note_desired(&mut self, target: LatencyTarget) {
        if self.desired != target {
            self.desired = target;
            // New desired parameters supersede any pending retry cooldown from an older target.
            self.next_retry_at = None;
        }
    }

    fn needs_attempt(&self, target: LatencyTarget, now: Instant) -> bool {
        if self.accepted == Some(target) {
            return false;
        }
        if self.desired != target {
            return true;
        }
        self.next_retry_at.is_none_or(|due| now >= due)
    }

    fn try_publish<P: LatencyPublisher>(
        &mut self,
        publisher: &mut P,
        role: LatencyStreamRole,
        now: Instant,
    ) -> bool {
        if self.accepted == Some(self.desired) {
            return false;
        }
        if let Some(due) = self.next_retry_at
            && now < due
        {
            return false;
        }
        match publisher.publish_latency(role, self.desired.samples) {
            Ok(()) => {
                let recovered = self.consecutive_failures > 0;
                let prior_failures = self.consecutive_failures;
                self.accepted = Some(self.desired);
                self.consecutive_failures = 0;
                self.next_retry_at = None;
                self.last_logged_error_at = None;
                if recovered {
                    log::info!(
                        "PipeWire {} stream latency publication recovered after {} failed attempt(s) ({} samples @{}Hz)",
                        role.label(),
                        prior_failures,
                        self.desired.samples,
                        self.desired.host_rate,
                    );
                }
                true
            }
            Err(err) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                let backoff = retry_backoff(self.consecutive_failures);
                self.next_retry_at = Some(now + backoff);
                let should_log = self
                    .last_logged_error_at
                    .is_none_or(|last| now.duration_since(last) >= ERROR_LOG_INTERVAL);
                if should_log {
                    self.last_logged_error_at = Some(now);
                    log::warn!(
                        "Failed to publish PipeWire {} stream latency ({} samples @{}Hz, attempt #{}): {err}; retrying in {} ms",
                        role.label(),
                        self.desired.samples,
                        self.desired.host_rate,
                        self.consecutive_failures,
                        backoff.as_millis(),
                    );
                }
                false
            }
        }
    }
}

#[inline]
fn retry_backoff(consecutive_failures: u32) -> Duration {
    let shift = consecutive_failures.saturating_sub(1).min(4);
    let scaled = RETRY_BASE_INTERVAL.saturating_mul(1u32 << shift);
    scaled.min(RETRY_MAX_INTERVAL)
}

/// Summary of one off-RT latency synchronization step across both host streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySyncOutcome {
    /// True if at least one stream transitioned to `accepted == Some(desired)` in this step.
    pub any_newly_accepted: bool,
    /// True if both capture and playback currently have `accepted == Some(desired)`.
    pub all_accepted: bool,
}

/// Off-RT tracker maintaining independent desired/accepted state and bounded retry
/// cadence for the capture and playback streams of a single host instance.
#[derive(Debug, Clone)]
pub struct LatencyPublicationTracker {
    capture: StreamPublicationState,
    playback: StreamPublicationState,
}

impl LatencyPublicationTracker {
    /// Creates a new tracker initialized with the latency and rate passed at stream `connect()`.
    pub fn new(initial_samples: u32, initial_host_rate: u32) -> Self {
        let initial = LatencyTarget {
            samples: initial_samples,
            host_rate: initial_host_rate,
        };
        Self {
            capture: StreamPublicationState::new(initial),
            playback: StreamPublicationState::new(initial),
        }
    }

    /// Returns the capture stream's publication state.
    #[inline]
    pub const fn capture(&self) -> &StreamPublicationState {
        &self.capture
    }

    /// Returns the playback stream's publication state.
    #[inline]
    pub const fn playback(&self) -> &StreamPublicationState {
        &self.playback
    }

    /// Returns `true` when either stream needs an update or retry at `now`.
    #[inline]
    pub fn needs_sync(&self, target: LatencyTarget, now: Instant) -> bool {
        self.capture.needs_attempt(target, now) || self.playback.needs_attempt(target, now)
    }

    /// Synchronizes both streams against `target` at timestamp `now`, updating
    /// each stream's `accepted` state strictly upon `Ok(())` and scheduling
    /// rate-limited off-RT retries on failure.
    pub fn sync_at<P: LatencyPublisher>(
        &mut self,
        publisher: &mut P,
        target: LatencyTarget,
        cabsim_samples: u32,
        resampler_samples: u32,
        oversample_samples: u32,
        now: Instant,
    ) -> LatencySyncOutcome {
        self.capture.note_desired(target);
        self.playback.note_desired(target);

        let cap_updated = self
            .capture
            .try_publish(publisher, LatencyStreamRole::Capture, now);
        let pb_updated = self
            .playback
            .try_publish(publisher, LatencyStreamRole::Playback, now);

        let any_newly_accepted = cap_updated || pb_updated;
        let all_accepted =
            self.capture.accepted == Some(target) && self.playback.accepted == Some(target);

        if any_newly_accepted && all_accepted {
            let (_, lat_ns) = compute_dsp_latency(
                cabsim_samples,
                resampler_samples,
                oversample_samples,
                target.host_rate,
            );
            log::info!(
                "{} Published updated DSP processing latency to PipeWire graph: {} samples ({:.2} ms @{}Hz) [cabsim={}, resampler={}, oversample={}]",
                "⏱️".cyan(),
                target.samples,
                lat_ns as f64 / 1_000_000.0,
                target.host_rate,
                cabsim_samples,
                resampler_samples,
                oversample_samples,
            );
        }

        LatencySyncOutcome {
            any_newly_accepted,
            all_accepted,
        }
    }
}

#[cfg(test)]
#[path = "latency_test.rs"]
mod tests;
