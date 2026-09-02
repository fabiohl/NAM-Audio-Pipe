// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Backend state machine and thread-safe shared status for the PipeWire host.
//!
//! Every fatal loss of backend connectivity (stream `StreamState::Error`,
//! post-streaming `StreamState::Unconnected`) transitions the shared
//! [`SharedBackendStatus`] to `Failed` through the stream-state observers
//! installed on the capture and playback streams. The main control loop in
//! `run.rs` polls [`SharedBackendStatus::is_failed`] every iteration and, on
//! failure, either enters the bounded reconnect cycle (via
//! [`SharedBackendStatus::begin_reconnect`]) or tears the host down
//! observably (RT loop stop, GC drain, recording teardown) and returns an
//! error — the process never survives as a functionally-dead zombie with no
//! audio, and never reconnects unboundedly.
//!
//! Exception: a post-streaming `Unconnected` observed while the process-global
//! `SHUTDOWN` flag is raised (SIGINT/SIGTERM) is the expected teardown of the
//! streams by `thread_loop.stop()`, not a daemon crash. It is logged at `info!`
//! and does **not** transition the backend to `Failed`, so a graceful termination
//! never raises a false "daemon crash" alarm.

use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::wakeup::ControlPlaneWakeup;
use neural_amp_modeler_rs::common::spsc::{RtStatusFlags, SHUTDOWN};
use pipewire as pw;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Typed lifecycle state of the PipeWire backend.
///
/// `Failed` is **sticky**: once the backend failed, no subsequent transition
/// (`Running` / `Degraded`) can overwrite the failure — the control loop must
/// observe it and either enter the bounded reconnect cycle or terminate the host.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BackendState {
    /// Host initialized but no stream reached an operational state yet.
    #[default]
    Starting,
    /// At least one stream is streaming and audio is expected.
    Running,
    /// Backend alive but degraded (e.g. SPA format contract violated — audio muted).
    Degraded { reason: String },
    /// A bounded reconnect cycle is in progress: the backend lost connectivity
    /// and the host is waiting the backoff before the next stream re-instantiation attempt.
    Reconnecting {
        /// 1-based attempt index of the current cycle.
        attempt: u32,
        /// Maximum attempts configured for the cycle.
        total_attempts: u32,
        /// Backoff duration before the next reconnect attempt.
        next_backoff: Duration,
    },
    /// A fatal error occurred on `stream` (e.g. PipeWire daemon disconnected or
    /// fatal stream error).
    Failed {
        /// Stream where the error originated (`capture`, `playback`, or `core`).
        stream: &'static str,
        /// Diagnostic message emitted by PipeWire.
        reason: String,
    },
    /// Host shut down cleanly (control loop exited).
    Terminated,
}

impl BackendState {
    /// Invariant: returns `true` if `is_failed == true` is consistent with this
    /// state. `Failed` must always match `is_failed == true`; all other states
    /// require `is_failed == false`, except `Terminated` which can follow a
    /// failed teardown.
    pub fn matches_failed_flag(&self, failed: bool) -> bool {
        match self {
            BackendState::Failed { .. } => failed,
            BackendState::Starting
            | BackendState::Running
            | BackendState::Degraded { .. }
            | BackendState::Reconnecting { .. } => !failed,
            // A terminated backend can originate from a clean shutdown or a
            // failed teardown, so both flag values are legal.
            BackendState::Terminated => true,
        }
    }
}

/// Immutable failure detail captured when the backend failed (sticky).
#[derive(Debug, Clone, PartialEq, Eq)]
struct BackendFailureDetail {
    stream: &'static str,
    reason: String,
}

/// A coherent snapshot of the backend status (model checking).
///
/// Read under a single acquisition of the state lock, so `failed`, `state`
/// and `failure` can never describe different instants — the model-check
/// gate in `tests/rt_metrics.rs` asserts the machine invariants on these
/// snapshots under 16 concurrent writers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendStatusSnapshot {
    /// Mirror of [`SharedBackendStatus::is_failed`] read atomically with the
    /// rest of the snapshot.
    pub failed: bool,
    /// Detailed lifecycle state at the snapshot instant.
    pub state: BackendState,
    /// Sticky failure detail when `failed`; `None` while healthy.
    pub failure: Option<(&'static str, String)>,
}

impl BackendStatusSnapshot {
    /// Whether the snapshot satisfies the coherent-snapshot invariants of the
    /// backend machine (model checking).
    ///
    /// `Failed` must be accompanied by the published failure flag and detail;
    /// every pre-terminal state (`Starting`, `Running`, `Degraded`,
    /// `Reconnecting`) must be healthy; `Terminated` is terminal and accepts
    /// both a clean and a failed-teardown outcome. Single source of truth for
    /// the unit regression (`status_test.rs`) and the long-suite model-check
    /// gate (`tests/rt_metrics.rs`).
    pub fn invariants_hold(&self) -> bool {
        self.state.matches_failed_flag(self.failed)
    }
}

/// Thread-safe, observable backend status shared between the PipeWire stream
/// listeners (capture/playback state handlers, cold path) and the main control
/// loop in `run.rs`.
///
/// A [`Mutex`] protects the detailed [`BackendState`] (off-RT only — the state
/// handlers run on the PipeWire `ThreadLoop` thread, never on the RT data
/// thread), while an [`AtomicBool`] gives the main loop a lock-free fast-path
/// [`SharedBackendStatus::is_failed`] poll every control iteration.
#[derive(Default)]
pub struct SharedBackendStatus {
    failed: AtomicBool,
    capture_active: AtomicBool,
    playback_active: AtomicBool,
    state: Mutex<BackendState>,
    failure_detail: Mutex<Option<BackendFailureDetail>>,
    rt_status: Option<Arc<RtStatusFlags>>,
    wakeup: Option<ControlPlaneWakeup>,
}

impl std::fmt::Debug for SharedBackendStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBackendStatus")
            .field("failed", &self.failed)
            .field("capture_active", &self.capture_active)
            .field("playback_active", &self.playback_active)
            .field("state", &self.lock_state())
            .field("has_rt_status", &self.rt_status.is_some())
            .field("has_wakeup", &self.wakeup.is_some())
            .finish()
    }
}

impl SharedBackendStatus {
    /// Creates a new status in the [`BackendState::Starting`] state without an RT status latch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a new status in the [`BackendState::Starting`] state bound to an RT status latch.
    pub fn with_rt_status(rt_status: Arc<RtStatusFlags>) -> Self {
        // Initial state before PipeWire streams reach `Streaming` is inactive (muted).
        crate::standalone::pw_host::output_pw::mark_stream_active(&rt_status, "capture", false);
        crate::standalone::pw_host::output_pw::mark_stream_active(&rt_status, "playback", false);
        Self {
            rt_status: Some(rt_status),
            ..Self::default()
        }
    }

    /// Binds an RT status latch to an existing backend status instance.
    pub fn bind_rt_status(&mut self, rt_status: Arc<RtStatusFlags>) {
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &rt_status,
            "capture",
            self.capture_active.load(Ordering::Acquire),
        );
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &rt_status,
            "playback",
            self.playback_active.load(Ordering::Acquire),
        );
        self.rt_status = Some(rt_status);
    }

    /// Binds an event-driven wakeup mechanism for the main control plane.
    pub fn bind_wakeup(&mut self, wakeup: ControlPlaneWakeup) {
        self.wakeup = Some(wakeup);
    }

    /// Wakes up the waiting main control plane loop immediately.
    pub fn notify_wakeup(&self) {
        if let Some(ref wakeup) = self.wakeup {
            wakeup.notify();
        }
    }

    /// Lock-free fast-path poll used by the main control loop.
    ///
    /// `Acquire` pairs with the `Release` store in [`Self::mark_failed`]: a
    /// consumer that observes `true` is guaranteed to also observe the fully
    /// published failure detail.
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Snapshot of the detailed backend lifecycle state.
    pub fn state(&self) -> BackendState {
        self.lock_state().clone()
    }

    /// Returns the sticky failure detail `(stream, reason)` once the backend
    /// failed; `None` while the backend is healthy.
    pub fn failure(&self) -> Option<(&'static str, String)> {
        if !self.is_failed() {
            return None;
        }
        self.lock_failure_detail()
            .as_ref()
            .map(|d| (d.stream, d.reason.clone()))
    }

    /// Coherent status snapshot for diagnostics and the concurrency
    /// model-check gate.
    ///
    /// All three fields are read under one acquisition of the state lock, so
    /// the snapshot is internally consistent even while transition writers
    /// (`mark_*`, `begin_reconnect`) run concurrently.
    pub fn snapshot(&self) -> BackendStatusSnapshot {
        let state_guard = self.lock_state();
        let failed = self.failed.load(Ordering::Acquire);
        let state = state_guard.clone();
        let failure = if failed {
            self.lock_failure_detail()
                .as_ref()
                .map(|d| (d.stream, d.reason.clone()))
        } else {
            None
        };
        BackendStatusSnapshot {
            failed,
            state,
            failure,
        }
    }

    /// Updates active state for a stream (`capture` or `playback`).
    pub fn set_stream_active(&self, stream: &'static str, active: bool) {
        if stream == "capture" {
            self.capture_active.store(active, Ordering::Release);
        } else if stream == "playback" {
            self.playback_active.store(active, Ordering::Release);
        }
        if let Some(ref rt) = self.rt_status {
            crate::standalone::pw_host::output_pw::mark_stream_active(rt, stream, active);
        }
        let cap = self.capture_active.load(Ordering::Acquire);
        let pb = self.playback_active.load(Ordering::Acquire);
        let mut guard = self.lock_state();
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        if cap && pb {
            if matches!(
                *guard,
                BackendState::Starting | BackendState::Reconnecting { .. }
            ) {
                *guard = BackendState::Running;
                drop(guard);
                self.notify_wakeup();
            }
        } else if matches!(*guard, BackendState::Running) {
            *guard = BackendState::Starting;
            drop(guard);
            self.notify_wakeup();
        }
    }

    /// Transitions to [`BackendState::Running`].
    ///
    /// A no-op once the backend failed — `Failed` is sticky so the control loop
    /// always observes the terminal condition.
    pub fn mark_running(&self) {
        let mut guard = self.lock_state();
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        *guard = BackendState::Running;
        drop(guard);
        self.notify_wakeup();
    }

    /// Transitions to [`BackendState::Degraded`] with a diagnostic reason
    /// (e.g. SPA format contract violation). A no-op once the backend failed.
    pub fn mark_degraded(&self, reason: impl Into<String>) {
        let mut guard = self.lock_state();
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        *guard = BackendState::Degraded {
            reason: reason.into(),
        };
        drop(guard);
        self.notify_wakeup();
    }

    /// Transitions to [`BackendState::Failed`] for `stream` and records the
    /// sticky failure detail.
    pub fn mark_failed(&self, stream: &'static str, reason: impl Into<String>) {
        let reason = reason.into();
        let mut guard = self.lock_state();
        *guard = BackendState::Failed {
            stream,
            reason: reason.clone(),
        };
        *self.lock_failure_detail() = Some(BackendFailureDetail { stream, reason });
        self.failed.store(true, Ordering::Release);
        drop(guard);
        self.notify_wakeup();
    }

    /// Marks the backend [`BackendState::Terminated`] after teardown finished.
    pub fn mark_terminated(&self) {
        *self.lock_state() = BackendState::Terminated;
        self.capture_active.store(false, Ordering::Release);
        self.playback_active.store(false, Ordering::Release);
        if let Some(ref rt) = self.rt_status {
            crate::standalone::pw_host::output_pw::mark_stream_active(rt, "capture", false);
            crate::standalone::pw_host::output_pw::mark_stream_active(rt, "playback", false);
        }
        self.notify_wakeup();
    }

    /// Enters the bounded reconnect cycle.
    pub fn begin_reconnect(&self, attempt: u32, total_attempts: u32, next_backoff: Duration) {
        let mut guard = self.lock_state();
        self.failed.store(false, Ordering::Release);
        self.capture_active.store(false, Ordering::Release);
        self.playback_active.store(false, Ordering::Release);
        if let Some(ref rt) = self.rt_status {
            crate::standalone::pw_host::output_pw::mark_stream_active(rt, "capture", false);
            crate::standalone::pw_host::output_pw::mark_stream_active(rt, "playback", false);
        }
        *self.lock_failure_detail() = None;
        *guard = BackendState::Reconnecting {
            attempt,
            total_attempts,
            next_backoff,
        };
        drop(guard);
        self.notify_wakeup();
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, BackendState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_failure_detail(&self) -> std::sync::MutexGuard<'_, Option<BackendFailureDetail>> {
        self.failure_detail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Maps a PipeWire stream-state transition to the backend state machine.
pub fn observe_stream_state(
    stream: &'static str,
    old: pw::stream::StreamState,
    new: pw::stream::StreamState,
    backend: &SharedBackendStatus,
) {
    match new {
        pw::stream::StreamState::Error(err) => {
            log::error!(
                "{} Critical PipeWire {stream} stream failure: {err}",
                "💥".red(),
            );
            backend.set_stream_active(stream, false);
            backend.mark_failed(stream, err);
        }
        pw::stream::StreamState::Unconnected if was_connected(&old) => {
            backend.set_stream_active(stream, false);
            // Exception: a post-streaming disconnect while the process is
            // shutting down cooperatively (SIGINT/SIGTERM raised `SHUTDOWN`) is
            // the streams being torn down by `thread_loop.stop()` — expected,
            // so it is logged below `ERROR` and the sticky `Failed` transition
            // is skipped. Without `SHUTDOWN` the strict behavior is kept: an
            // `error!` signaling an unexpected drop or a daemon restart/crash.
            if SHUTDOWN.load(Ordering::Acquire) {
                log::info!(
                    "{} PipeWire {stream} stream disconnected cooperatively during shutdown.",
                    "🔌".yellow(),
                );
            } else {
                log::error!(
                    "{} PipeWire {stream} stream disconnected from the audio backend \
                     (daemon restart or crash) — bounded reconnect or fail-fast teardown follows.",
                    "🔌".red(),
                );
                backend.mark_failed(stream, "stream disconnected from the audio backend");
            }
        }
        pw::stream::StreamState::Paused => {
            backend.set_stream_active(stream, false);
            if old == pw::stream::StreamState::Streaming {
                log::info!(
                    "{} Audio disconnected or node switch ({stream} stream).",
                    "⏸️".yellow(),
                );
            }
        }
        pw::stream::StreamState::Streaming => {
            backend.set_stream_active(stream, true);
            if old == pw::stream::StreamState::Paused {
                log::info!(
                    "{} Audio captured ({stream} connection established).",
                    "▶️".green(),
                );
            }
        }
        _ => {}
    }
}

/// Observes the RT panic-captured latch and transitions the
/// backend to [`BackendState::Failed`] when a panic was contained inside an RT
/// callback closure.
///
/// Called by the main control loop on every poll iteration (< 100 ms): a
/// contained panic must never become an `abort` — the backend machine drives
/// the ordered teardown (thread-loop stop, GC drain,
/// `RecordingWorkerGuard::shutdown` with the WAV finalized) exactly like any
/// other fatal backend failure.
///
/// Returns `true` when the panic flag was observed (the caller breaks the
/// control loop into the teardown path).
pub fn observe_rt_panic(rt_status: &RtStatusFlags, backend: &SharedBackendStatus) -> bool {
    if rt_status.check_flag(super::rt_callback::RT_STATUS_PANIC_CAPTURED) {
        log::error!(
            "{} Panic captured inside an RT callback closure — contained, \
             ordered teardown follows (no abort, capture will be finalized).",
            "🔥".red(),
        );
        backend.mark_failed(
            "rt_callback",
            "panic captured in an RT callback closure (contained — no abort, ordered teardown follows)",
        );
        true
    } else {
        false
    }
}

/// Whether `old` indicates the stream previously held an established
/// connection (`Paused` or `Streaming`), i.e. `Unconnected` now means a real
/// disconnect rather than the initial not-yet-connected state.
fn was_connected(old: &pw::stream::StreamState) -> bool {
    matches!(
        old,
        pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming
    )
}

#[cfg(test)]
#[path = "status_test.rs"]
mod tests;
