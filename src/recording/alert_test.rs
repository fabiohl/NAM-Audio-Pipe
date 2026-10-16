// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the recording alert state machine (F-UX-02a): the first
//! observation is emitted once, reminders only after
//! [`super::ALERT_REMINDER_INTERVAL`], a changed reason re-emits immediately and
//! a resolved condition clears the latch. The clock is injected so the cadence
//! is proven without sleeping.

use super::*;
use crate::recording::status::RecordingStatus;
use std::sync::{Arc, Mutex};

/// Manually advanced clock shared with the test body.
#[derive(Clone)]
struct ManualClock {
    now: Arc<Mutex<Instant>>,
}

impl ManualClock {
    fn new() -> Self {
        Self {
            now: Arc::new(Mutex::new(Instant::now())),
        }
    }

    fn advance(&self, delta: Duration) {
        let mut guard = self.now.lock().unwrap();
        *guard += delta;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        *self.now.lock().unwrap()
    }
}

fn status_with(state: RecordingStatus) -> SharedRecordingStatus {
    Arc::new(Mutex::new(state))
}

fn active() -> RecordingStatus {
    RecordingStatus::Active {
        path: std::path::PathBuf::from("/tmp/captures"),
        space_warning: None,
    }
}

#[test]
fn failure_emits_once_then_reminds_at_interval() {
    let clock = ManualClock::new();
    let mut alert = RecordingAlert::with_clock(clock.clone());
    let status = status_with(active());

    // Healthy Active: nothing to report.
    assert_eq!(alert.poll(&status), None);

    *status.lock().unwrap() = RecordingStatus::Failed {
        reason: "No space left on device (ENOSPC)".to_string(),
    };

    let first = alert.poll(&status).expect("first failure must alert");
    assert_eq!(first.kind, AlertKind::Failure);
    assert!(!first.reminder);
    assert!(first.reason.contains("ENOSPC"));

    // Before the interval: silent.
    clock.advance(ALERT_REMINDER_INTERVAL - Duration::from_secs(1));
    assert_eq!(alert.poll(&status), None);

    // At the interval: exactly one reminder.
    clock.advance(Duration::from_secs(1));
    let reminder = alert.poll(&status).expect("reminder must be due");
    assert!(reminder.reminder);
    assert_eq!(reminder.kind, AlertKind::Failure);

    // Immediately after: silent again.
    assert_eq!(alert.poll(&status), None);
}

#[test]
fn space_warning_emits_and_reminds_like_a_failure() {
    let clock = ManualClock::new();
    let mut alert = RecordingAlert::with_clock(clock.clone());
    let status = status_with(active());

    *status.lock().unwrap() = RecordingStatus::Active {
        path: std::path::PathBuf::from("/tmp/captures/capture_1.wav"),
        space_warning: Some("0.8 GiB free (~3 min of recording left)".to_string()),
    };

    let first = alert.poll(&status).expect("space warning must alert");
    assert_eq!(first.kind, AlertKind::SpaceLow);
    assert!(!first.reminder);

    clock.advance(ALERT_REMINDER_INTERVAL);
    let reminder = alert.poll(&status).expect("space reminder must be due");
    assert!(reminder.reminder);
    assert_eq!(reminder.kind, AlertKind::SpaceLow);
}

#[test]
fn changed_reason_re_emits_immediately_as_first() {
    let clock = ManualClock::new();
    let mut alert = RecordingAlert::with_clock(clock.clone());
    let status = status_with(active());

    *status.lock().unwrap() = RecordingStatus::Failed {
        reason: "first".to_string(),
    };
    assert!(!alert.poll(&status).unwrap().reminder);

    // A different reason is a new condition: emit as first, not as reminder.
    *status.lock().unwrap() = RecordingStatus::Failed {
        reason: "second".to_string(),
    };
    let second = alert.poll(&status).unwrap();
    assert!(!second.reminder);
    assert_eq!(second.reason, "second");
}

#[test]
fn resolved_condition_clears_the_latch() {
    let clock = ManualClock::new();
    let mut alert = RecordingAlert::with_clock(clock.clone());
    let status = status_with(active());

    *status.lock().unwrap() = RecordingStatus::Failed {
        reason: "boom".to_string(),
    };
    assert!(alert.poll(&status).is_some());

    *status.lock().unwrap() = RecordingStatus::Stopped;
    assert_eq!(alert.poll(&status), None);

    // Recurrence after resolution is a fresh first alert.
    *status.lock().unwrap() = RecordingStatus::Failed {
        reason: "boom".to_string(),
    };
    let again = alert.poll(&status).unwrap();
    assert!(!again.reminder);
}

#[test]
fn render_includes_reason_and_is_multiline() {
    let alert = Alert {
        kind: AlertKind::Failure,
        reason: "EIO while writing".to_string(),
        reminder: false,
    };
    let rendered = alert.render();
    assert!(rendered.contains("Recording FAILED"));
    assert!(rendered.contains("EIO while writing"));
    assert!(rendered.contains('\n'));
}

#[test]
fn poll_never_blocks_on_a_held_lock() {
    // The worker holds the status mutex for the whole poll: the alert must
    // return promptly with `None` (try_lock), never block the control loop.
    let clock = ManualClock::new();
    let mut alert = RecordingAlert::with_clock(clock);
    let status = status_with(active());
    let _held = status.lock().unwrap();
    assert_eq!(alert.poll(&status), None);
}
