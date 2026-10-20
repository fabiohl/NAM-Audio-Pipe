// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Visible, de-duplicated recording alerts for the main control loop (F-UX-02a).
//!
//! A runtime recording failure (disk full `ENOSPC`, `EIO`) used to be silent
//! until shutdown: the worker transitioned [`RecordingStatus`] to `Failed` and
//! raised the RT-observable flag, but the main control loop never looked at it,
//! so the musician kept playing believing the take was being recorded. The same
//! applied to the disk-space pre-warning, which is published through the
//! `Active.space_warning` slot to unify the output path.
//!
//! [`RecordingAlert`] closes that gap. The control loop polls the shared status
//! with `try_lock` (never blocking the 100 ms health loop, never touching the RT
//! callback) and gets:
//!
//! * [`Alert::First`]-like emission the moment a failure or a new low-space
//!   warning is observed; and
//! * a [`Alert`] reminder every [`ALERT_REMINDER_INTERVAL`] while the condition
//!   persists — a non-spam cadence so a long session does not drown in logs.
//!
//! The clock is injectable ([`Clock`]) so the reminder cadence is proven
//! deterministically without sleeping.

use std::time::{Duration, Instant};

use crate::recording::status::{RecordingStatus, SharedRecordingStatus};
use crate::standalone::colors::{Colorize, color_enabled};

/// Minimum interval between repeated reminders while a condition persists.
pub const ALERT_REMINDER_INTERVAL: Duration = Duration::from_secs(30);

/// Injectable monotonic clock, so tests advance time without sleeping.
pub trait Clock {
    /// Returns the current monotonic instant.
    fn now(&self) -> Instant;
}

/// Production clock backed by [`std::time::Instant`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Class of a recording alert, selecting the remediation hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    /// The recording stopped fatally (`Failed`): `ENOSPC`, `EIO`, missing
    /// directory, `io_uring` loss, ...
    Failure,
    /// Disk space is running low but the recording is still live.
    SpaceLow,
}

/// A user-facing recording alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Whether this is a failure or a low-space pre-warning.
    pub kind: AlertKind,
    /// Human-readable reason (worker error text or the low-space summary).
    pub reason: String,
    /// `true` for a repeat of a still-active condition, `false` for the first
    /// observation (or the first observation of a *changed* reason).
    pub reminder: bool,
}

impl Alert {
    /// Renders the colored, multi-line message the control loop emits.
    ///
    /// The message is deliberately loud (a failure must be impossible to miss)
    /// but keeps the audio contract explicit: monitoring never stops, only the
    /// capture does.
    pub fn render(&self) -> String {
        let tag = if self.reminder {
            if color_enabled() {
                "(reminder) ".yellow().bold()
            } else {
                "(reminder) ".to_string()
            }
        } else {
            String::new()
        };
        match self.kind {
            AlertKind::Failure => {
                let head = if color_enabled() {
                    format!("{} {tag}Recording FAILED", "🛑".bright_red().bold())
                } else {
                    format!("🛑 {tag}Recording FAILED")
                };
                format!(
                    "{head}: {reason}\n\
                     \x20  Audio keeps playing, but nothing more is being captured.\n\
                     \x20  💡 Check free disk space (`df -h`) and the capture directory \
                     permissions; the partial file is preserved for recovery.",
                    reason = self.reason,
                )
            }
            AlertKind::SpaceLow => {
                let head = if color_enabled() {
                    format!("{} {tag}Recording disk space low", "⚠️".yellow().bold())
                } else {
                    format!("⚠️ {tag}Recording disk space low")
                };
                format!(
                    "{head}: {reason}\n\
                     \x20  Free space on the capture volume; recording will fail when it fills.",
                    reason = self.reason,
                )
            }
        }
    }
}

/// State machine turning status polls into de-duplicated, rate-limited alerts.
pub struct RecordingAlert<C: Clock = SystemClock> {
    clock: C,
    /// Reason of the condition currently being reported, if any. Drives the
    /// "changed condition" detection: a new failure or a new low-space warning
    /// re-emits immediately as a fresh first alert.
    active_reason: Option<String>,
    /// Kind of the condition currently being reported.
    active_kind: Option<AlertKind>,
    /// When the current alert was last emitted.
    last_emit: Option<Instant>,
}

impl RecordingAlert<SystemClock> {
    /// Builds an alert tracker using the real monotonic clock.
    pub fn new() -> Self {
        Self::with_clock(SystemClock)
    }
}

impl Default for RecordingAlert<SystemClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> RecordingAlert<C> {
    /// Builds an alert tracker with an explicit clock (tests).
    pub fn with_clock(clock: C) -> Self {
        Self {
            clock,
            active_reason: None,
            active_kind: None,
            last_emit: None,
        }
    }

    /// Polls the shared status without blocking and returns an alert when due.
    ///
    /// `try_lock` is mandatory: the control loop must never block on the
    /// worker's status mutex, and the RT callback never appears here.
    pub fn poll(&mut self, status: &SharedRecordingStatus) -> Option<Alert> {
        // Ignore the poll entirely while the worker holds the lock; the next
        // 100 ms iteration will see the updated state.
        let reason = match status.try_lock() {
            Ok(guard) => match &*guard {
                RecordingStatus::Failed { reason } => Some((AlertKind::Failure, reason.clone())),
                RecordingStatus::Active {
                    space_warning: Some(w),
                    ..
                } => Some((AlertKind::SpaceLow, w.clone())),
                _ => None,
            },
            Err(_) => return None,
        };

        let now = self.clock.now();
        match reason {
            None => {
                // Condition resolved (or never started): clear the latches so a
                // future recurrence is reported again as a first alert.
                self.active_reason = None;
                self.active_kind = None;
                self.last_emit = None;
                None
            }
            Some((kind, reason)) => {
                let changed = self.active_kind != Some(kind)
                    || self.active_reason.as_deref() != Some(reason.as_str());
                if changed {
                    self.active_kind = Some(kind);
                    self.active_reason = Some(reason.clone());
                    self.last_emit = Some(now);
                    Some(Alert {
                        kind,
                        reason,
                        reminder: false,
                    })
                } else {
                    let due = self.last_emit.is_none_or(|t| {
                        now.checked_duration_since(t).unwrap_or_default() >= ALERT_REMINDER_INTERVAL
                    });
                    if due {
                        self.last_emit = Some(now);
                        Some(Alert {
                            kind,
                            reason,
                            reminder: true,
                        })
                    } else {
                        None
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "alert_test.rs"]
mod alert_test;
