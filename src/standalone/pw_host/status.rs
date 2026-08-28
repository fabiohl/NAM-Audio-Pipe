// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Backend state machine and thread-safe shared status for the PipeWire host.
//!
//! F-RB-010 / T4.4: every fatal loss of backend connectivity (stream
//! `StreamState::Error`, post-streaming `StreamState::Unconnected`) transitions
//! the shared [`SharedBackendStatus`] to `Failed` through the stream-state
//! observers installed on the capture and playback streams. The main control
//! loop in `run.rs` polls [`SharedBackendStatus::is_failed`] every iteration
//! and, on failure, either enters the bounded reconnect cycle (F-RB-010 / T4.5,
//! via [`SharedBackendStatus::begin_reconnect`]) or tears the host down
//! observably (RT loop stop, GC drain, recording teardown) and returns an
//! error — the process never survives as a functionally-dead zombie with no
//! audio, and never reconnects unboundedly.

use crate::standalone::colors::Colorize;
use pipewire as pw;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Typed lifecycle state of the PipeWire backend.
///
/// `Failed` is **sticky**: once the backend failed, no subsequent transition
/// (`Running` / `Degraded`) can overwrite the failure — the control loop must
/// observe it and either enter the bounded reconnect cycle (F-RB-010 / T4.5)
/// or terminate the host.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BackendState {
    /// Host initialized but no stream reached an operational state yet.
    #[default]
    Starting,
    /// At least one stream is streaming and audio is expected.
    Running,
    /// Backend alive but degraded (e.g. SPA format contract violated — audio muted).
    Degraded { reason: String },
    /// A bounded reconnect cycle is in progress (F-RB-010 / T4.5): the backend
    /// lost connectivity and the host is waiting the backoff before the next
    /// stream re-instantiation attempt.
    Reconnecting {
        /// 1-based number of the reconnect attempt about to be made.
        attempt: u32,
        /// Total reconnect budget for this session (`max_attempts`).
        total_attempts: u32,
        /// Backoff being waited before the next attempt.
        next_backoff: Duration,
    },
    /// Fatal loss of connectivity on a specific stream.
    Failed {
        stream: &'static str,
        reason: String,
    },
    /// Teardown finished.
    Terminated,
}

/// Immutable failure detail captured when the backend failed (sticky).
#[derive(Debug, Clone, PartialEq, Eq)]
struct BackendFailureDetail {
    stream: &'static str,
    reason: String,
}

/// A coherent snapshot of the backend status (T6.5 / model checking).
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
    /// backend machine (T6.5 model checking).
    ///
    /// `Failed` must be accompanied by the published failure flag and detail;
    /// every pre-terminal state (`Starting`, `Running`, `Degraded`,
    /// `Reconnecting`) must be healthy; `Terminated` is terminal and accepts
    /// both a clean and a failed-teardown outcome. Single source of truth for
    /// the unit regression (`status_test.rs`) and the long-suite model-check
    /// gate (`tests/rt_metrics.rs`).
    pub fn invariants_hold(&self) -> bool {
        match &self.state {
            BackendState::Failed { .. } => self.failed && self.failure.is_some(),
            BackendState::Running
            | BackendState::Degraded { .. }
            | BackendState::Reconnecting { .. }
            | BackendState::Starting => !self.failed && self.failure.is_none(),
            // Terminated is terminal: it may follow either a clean run or a
            // failed teardown, so both flag values are legal.
            BackendState::Terminated => true,
        }
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
#[derive(Debug, Default)]
pub struct SharedBackendStatus {
    failed: AtomicBool,
    state: Mutex<BackendState>,
    failure_detail: Mutex<Option<BackendFailureDetail>>,
}

impl SharedBackendStatus {
    /// Creates a new status in the [`BackendState::Starting`] state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock-free fast-path poll used by the main control loop (F-RB-010 / T4.4).
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
    /// model-check gate (T6.5).
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

    /// Transitions to [`BackendState::Running`].
    ///
    /// A no-op once the backend failed — `Failed` is sticky so the control loop
    /// always observes the terminal condition. The sticky check and the state
    /// store happen under the same state-lock acquisition, so a concurrent
    /// [`Self::mark_failed`] can never be erased by an in-flight
    /// `mark_running` (T6.5 model-check invariant).
    pub fn mark_running(&self) {
        let mut guard = self.lock_state();
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        *guard = BackendState::Running;
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
    }

    /// Transitions to [`BackendState::Failed`] for `stream` and records the
    /// sticky failure detail (F-RB-010 / T4.4).
    ///
    /// The whole transaction — state store, failure detail and the `Release`
    /// store of the atomic flag — happens under one state-lock acquisition.
    /// This gives two guarantees under concurrent writers:
    ///
    /// * any `Acquire` poll that observes the flag also observes the fully
    ///   published `Failed` state and failure detail;
    /// * no concurrent [`Self::mark_running`]/[`Self::mark_degraded`] can
    ///   interleave between the state store and the flag store and erase the
    ///   failure (the check-then-act race closed by T6.5).
    pub fn mark_failed(&self, stream: &'static str, reason: impl Into<String>) {
        let reason = reason.into();
        let mut guard = self.lock_state();
        *guard = BackendState::Failed {
            stream,
            reason: reason.clone(),
        };
        *self.lock_failure_detail() = Some(BackendFailureDetail { stream, reason });
        self.failed.store(true, Ordering::Release);
    }

    /// Marks the backend [`BackendState::Terminated`] after teardown finished.
    pub fn mark_terminated(&self) {
        *self.lock_state() = BackendState::Terminated;
    }

    /// Enters the bounded reconnect cycle (F-RB-010 / T4.5).
    ///
    /// Clears the sticky failure (flag + detail) and publishes the observable
    /// [`BackendState::Reconnecting`] transition. Only the main control loop
    /// calls this, and only *after* `thread_loop.stop()` returned — the dying
    /// instance's state handlers can no longer fire, so no stale event can
    /// re-assert the failure while the fresh instance is being built.
    ///
    /// The whole transaction happens under one state-lock acquisition. The
    /// clear-before-publish ordering matters: clearing the `Acquire`-read
    /// failure flag first means a poll that sees `Reconnecting` never sees a
    /// stale `Failed`, and the fresh instance's `mark_running` on successful
    /// reconnection is not a no-op. Holding the lock across all three stores
    /// also makes the clear and the [`BackendState::Reconnecting`] publication
    /// atomic with respect to a concurrent [`Self::mark_failed`] (T6.5
    /// model-check invariant).
    pub fn begin_reconnect(&self, attempt: u32, total_attempts: u32, next_backoff: Duration) {
        let mut guard = self.lock_state();
        self.failed.store(false, Ordering::Release);
        *self.lock_failure_detail() = None;
        *guard = BackendState::Reconnecting {
            attempt,
            total_attempts,
            next_backoff,
        };
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
///
/// Installed on both the capture (`capture/listeners.rs`) and playback
/// (`playback.rs`) streams:
///
/// * `StreamState::Error(err)` → [`BackendState::Failed`] (fatal);
/// * `StreamState::Unconnected` after a previously connected (`Paused` /
///   `Streaming`) state → [`BackendState::Failed`] (daemon restart/crash);
/// * transition into `StreamState::Streaming` → [`BackendState::Running`]
///   (recovery from a node switch or from [`BackendState::Degraded`]).
///
/// Executes on the PipeWire `ThreadLoop` thread (cold path, never RT).
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
            backend.mark_failed(stream, err);
        }
        pw::stream::StreamState::Unconnected if was_connected(&old) => {
            log::error!(
                "{} PipeWire {stream} stream disconnected from the audio backend \
                 (daemon restart or crash) — bounded reconnect or fail-fast teardown follows.",
                "🔌".red(),
            );
            backend.mark_failed(stream, "stream disconnected from the audio backend");
        }
        pw::stream::StreamState::Paused if old == pw::stream::StreamState::Streaming => {
            log::info!(
                "{} Audio disconnected or node switch ({stream} stream).",
                "⏸️".yellow(),
            );
        }
        pw::stream::StreamState::Streaming => {
            backend.mark_running();
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
