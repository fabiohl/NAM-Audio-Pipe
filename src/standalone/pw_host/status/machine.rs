// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Backend lifecycle state machine types and coherent snapshots.

use super::super::StreamStatusFlags;
use super::snapshot::BackendStatusSnapshot;
use crate::standalone::pw_host::wakeup::ControlPlaneWakeup;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
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
pub(super) struct BackendFailureDetail {
    pub(super) stream: &'static str,
    pub(super) reason: String,
}

/// Thread-safe, observable backend status shared between the PipeWire stream
/// listeners (capture/playback state handlers, cold path) and the main control
/// loop in `run.rs`.
///
/// A [`Mutex`] protects the detailed [`BackendState`] (off-RT only — the state
/// handlers run on the PipeWire `ThreadLoop` thread, never on the RT data
/// thread), while an [`AtomicBool`] gives the main loop a lock-free fast-path
/// [`SharedBackendStatus::is_failed`] poll every control iteration.
pub struct SharedBackendStatus {
    pub(super) failed: AtomicBool,
    pub(super) teardown_in_progress: AtomicBool,
    pub(super) capture_active: AtomicBool,
    pub(super) playback_active: AtomicBool,
    /// Lock-free mirror of `state == Running`, refreshed inside the state lock
    /// by every transition so the control loop can gate its reconnect
    /// stability feed without acquiring the mutex every iteration.
    pub(super) running: AtomicBool,
    pub(super) state: Mutex<BackendState>,
    pub(super) failure_detail: Mutex<Option<BackendFailureDetail>>,
    pub(super) rt_status: Option<Arc<RtStatusFlags>>,
    pub(super) stream_status: Arc<StreamStatusFlags>,
    pub(super) wakeup: Option<ControlPlaneWakeup>,
}

impl Default for SharedBackendStatus {
    fn default() -> Self {
        Self {
            failed: AtomicBool::new(false),
            teardown_in_progress: AtomicBool::new(false),
            capture_active: AtomicBool::new(false),
            playback_active: AtomicBool::new(false),
            running: AtomicBool::new(false),
            state: Mutex::new(BackendState::Starting),
            failure_detail: Mutex::new(None),
            rt_status: None,
            stream_status: Arc::new(StreamStatusFlags::new()),
            wakeup: None,
        }
    }
}

impl std::fmt::Debug for SharedBackendStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBackendStatus")
            .field("failed", &self.failed)
            .field("teardown_in_progress", &self.teardown_in_progress)
            .field("capture_active", &self.capture_active)
            .field("playback_active", &self.playback_active)
            .field("running", &self.running.load(Ordering::Relaxed))
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
    #[cfg(any(test, feature = "testing"))]
    pub fn with_rt_status(rt_status: Arc<RtStatusFlags>) -> Self {
        let this = Self {
            rt_status: Some(rt_status),
            ..Self::default()
        };
        // Initial state before PipeWire streams reach `Streaming` is inactive (muted).
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &this.stream_status,
            "capture",
            false,
        );
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &this.stream_status,
            "playback",
            false,
        );
        this
    }

    /// Creates a new status in the [`BackendState::Starting`] state bound to both
    /// an engine RT status latch and host stream status flags.
    pub fn with_rt_and_stream_status(
        rt_status: Arc<RtStatusFlags>,
        stream_status: Arc<StreamStatusFlags>,
    ) -> Self {
        let this = Self {
            rt_status: Some(rt_status),
            stream_status,
            ..Self::default()
        };
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &this.stream_status,
            "capture",
            false,
        );
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &this.stream_status,
            "playback",
            false,
        );
        this
    }

    /// Binds an RT status latch to an existing backend status instance.
    #[cfg(any(test, feature = "testing"))]
    pub fn bind_rt_status(&mut self, rt_status: Arc<RtStatusFlags>) {
        self.rt_status = Some(rt_status);
    }

    /// Returns a reference to the stream status flags.
    pub fn stream_status(&self) -> &Arc<StreamStatusFlags> {
        &self.stream_status
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

    /// Lock-free fast-path poll of the `Running` gate.
    ///
    /// Mirror of `state() == BackendState::Running`, refreshed inside the state
    /// lock by every transition (`set_state_locked`),
    /// so the main control loop can feed the reconnect stability window without
    /// acquiring the mutex on every iteration.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Assigns `state` under an already-held state lock and refreshes the
    /// lock-free [`is_running`](Self::is_running) mirror in the same critical
    /// section, so the fast-path can never drift from the detailed state.
    pub(super) fn set_state_locked(&self, guard: &mut BackendState, state: BackendState) {
        *guard = state;
        self.running
            .store(matches!(*guard, BackendState::Running), Ordering::Release);
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

    pub(super) fn lock_state(&self) -> std::sync::MutexGuard<'_, BackendState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn lock_failure_detail(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<BackendFailureDetail>> {
        self.failure_detail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
