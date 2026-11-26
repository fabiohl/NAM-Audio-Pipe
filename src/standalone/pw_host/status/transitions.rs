// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Backend lifecycle transitions and stream-health evaluation.

use super::machine::{BackendFailureDetail, BackendState, SharedBackendStatus};
use std::sync::atomic::Ordering;
use std::time::Duration;

impl SharedBackendStatus {
    /// Evaluates stream activity and format contract validity to update backend lifecycle state.
    ///
    /// Must be called with `guard` holding `self.lock_state()`.
    /// `Failed` is sticky and terminal — no-op if `self.failed` is true.
    /// Transition to `Running` requires BOTH streams active AND both format contracts valid.
    fn update_stream_health_locked(&self, guard: &mut BackendState) {
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        let cap_active = self.capture_active.load(Ordering::Acquire);
        let pb_active = self.playback_active.load(Ordering::Acquire);
        let cap_format = self.stream_status.capture_format_ok.load(Ordering::Acquire) != 0;
        let pb_format = self
            .stream_status
            .playback_format_ok
            .load(Ordering::Acquire)
            != 0;

        let both_active = cap_active && pb_active;
        let both_format_ok = cap_format && pb_format;

        if both_active && both_format_ok {
            if matches!(
                *guard,
                BackendState::Starting
                    | BackendState::Reconnecting { .. }
                    | BackendState::Degraded { .. }
            ) {
                self.set_state_locked(guard, BackendState::Running);
                self.notify_wakeup();
            }
        } else if matches!(*guard, BackendState::Running) {
            if !both_active {
                self.set_state_locked(guard, BackendState::Starting);
                self.notify_wakeup();
            } else if !both_format_ok {
                self.set_state_locked(
                    guard,
                    BackendState::Degraded {
                        reason: "format contract violation on stream".into(),
                    },
                );
                self.notify_wakeup();
            }
        } else if matches!(*guard, BackendState::Degraded { .. }) && both_format_ok {
            // Formats are valid, but streams are not both active yet -> transition to Starting
            self.set_state_locked(guard, BackendState::Starting);
            self.notify_wakeup();
        }
    }

    /// Updates active state for a stream (`capture` or `playback`).
    pub fn set_stream_active(&self, stream: &'static str, active: bool) {
        if stream == "capture" {
            self.capture_active.store(active, Ordering::Release);
        } else if stream == "playback" {
            self.playback_active.store(active, Ordering::Release);
        }
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &self.stream_status,
            stream,
            active,
        );
        let mut guard = self.lock_state();
        self.update_stream_health_locked(&mut guard);
    }

    /// Marks the raw audio format contract valid for `stream` (`capture` or `playback`).
    ///
    /// Re-arms the RT mute guard and updates the backend lifecycle state:
    /// if both streams are active AND both formats are valid, transitions to [`BackendState::Running`].
    /// If both formats are valid but one stream is not yet active, transitions from [`BackendState::Degraded`]
    /// to [`BackendState::Starting`].
    ///
    /// Sticky [`BackendState::Failed`] is strictly preserved (terminal).
    pub fn mark_format_ok(&self, stream: &'static str) {
        crate::standalone::pw_host::output_pw::mark_format_contract_ok(&self.stream_status, stream);
        let mut guard = self.lock_state();
        self.update_stream_health_locked(&mut guard);
    }

    /// Transitions to [`BackendState::Running`].
    ///
    /// A no-op once the backend failed — `Failed` is sticky so the control loop
    /// always observes the terminal condition.
    #[cfg(any(test, feature = "testing"))]
    pub fn mark_running(&self) {
        let mut guard = self.lock_state();
        if self.failed.load(Ordering::Acquire) {
            return;
        }
        self.set_state_locked(&mut guard, BackendState::Running);
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
        self.set_state_locked(
            &mut guard,
            BackendState::Degraded {
                reason: reason.into(),
            },
        );
        drop(guard);
        self.notify_wakeup();
    }

    /// Transitions to [`BackendState::Failed`] for `stream` and records the
    /// sticky failure detail.
    pub fn mark_failed(&self, stream: &'static str, reason: impl Into<String>) {
        let reason = reason.into();
        let mut guard = self.lock_state();
        self.set_state_locked(
            &mut guard,
            BackendState::Failed {
                stream,
                reason: reason.clone(),
            },
        );
        *self.lock_failure_detail() = Some(BackendFailureDetail { stream, reason });
        self.failed.store(true, Ordering::Release);
        drop(guard);
        self.notify_wakeup();
    }

    /// Marks the backend [`BackendState::Terminated`] after teardown finished.
    pub fn mark_terminated(&self) {
        let mut guard = self.lock_state();
        self.set_state_locked(&mut guard, BackendState::Terminated);
        drop(guard);
        self.capture_active.store(false, Ordering::Release);
        self.playback_active.store(false, Ordering::Release);
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &self.stream_status,
            "capture",
            false,
        );
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &self.stream_status,
            "playback",
            false,
        );
        self.notify_wakeup();
    }

    /// Enters the bounded reconnect cycle.
    pub fn begin_reconnect(&self, attempt: u32, total_attempts: u32, next_backoff: Duration) {
        let mut guard = self.lock_state();
        self.failed.store(false, Ordering::Release);
        self.capture_active.store(false, Ordering::Release);
        self.playback_active.store(false, Ordering::Release);
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &self.stream_status,
            "capture",
            false,
        );
        crate::standalone::pw_host::output_pw::mark_stream_active(
            &self.stream_status,
            "playback",
            false,
        );
        *self.lock_failure_detail() = None;
        self.set_state_locked(
            &mut guard,
            BackendState::Reconnecting {
                attempt,
                total_attempts,
                next_backoff,
            },
        );
        drop(guard);
        self.notify_wakeup();
    }
}
