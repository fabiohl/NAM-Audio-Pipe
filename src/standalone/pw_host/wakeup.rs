// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Event-driven wakeup mechanism for the PipeWire main control plane.
//!
//! Replaces unconditional sleeps in the control loop with a condition variable wait,
//! waking up instantly on format/rate changes, stream state transitions, or shutdown,
//! while retaining a bounded health-poll timeout (≤ 100 ms) as fallback.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Event-driven notification mechanism for the main control plane loop.
///
/// Wraps a condition variable and mutex pair in an [`Arc`], allowing off-RT
/// and cold-path listeners (e.g. PipeWire `ThreadLoop` format negotiation handlers)
/// to wake the main loop immediately upon receiving events.
#[derive(Debug, Clone, Default)]
pub struct ControlPlaneWakeup {
    inner: Arc<(Mutex<()>, Condvar)>,
}

impl ControlPlaneWakeup {
    /// Creates a new control plane wakeup instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wakes up the waiting control plane thread immediately.
    ///
    /// Must only be called from off-RT or cold-path threads (e.g. PipeWire ThreadLoop,
    /// backend state handlers, CLI/signal controllers). Zero syscalls on RT path.
    pub fn notify(&self) {
        self.inner.1.notify_one();
    }

    /// Waits on the condition variable for up to `timeout`.
    ///
    /// Returns `true` if woken by a notification or `false` if timed out.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        if let Ok(guard) = self.inner.0.lock() {
            let (guard, result) = match self.inner.1.wait_timeout(guard, timeout) {
                Ok(res) => res,
                Err(poisoned) => poisoned.into_inner(),
            };
            drop(guard);
            !result.timed_out()
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn wakeup_notifies_immediately_without_waiting_full_timeout() {
        let wakeup = ControlPlaneWakeup::new();
        let wakeup_clone = wakeup.clone();

        let t0 = Instant::now();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            wakeup_clone.notify();
        });

        let notified = wakeup.wait_timeout(Duration::from_millis(500));
        let elapsed = t0.elapsed();

        handle.join().unwrap();
        assert!(notified, "should have been woken by notification");
        assert!(
            elapsed < Duration::from_millis(250),
            "wakeup took {elapsed:?}, expected immediate return (< 250 ms)"
        );
    }

    #[test]
    fn wakeup_times_out_when_no_notification() {
        let wakeup = ControlPlaneWakeup::new();
        let t0 = Instant::now();
        let notified = wakeup.wait_timeout(Duration::from_millis(20));
        let elapsed = t0.elapsed();

        assert!(!notified, "should report timed out");
        assert!(
            elapsed >= Duration::from_millis(18),
            "timeout returned too early: {elapsed:?}"
        );
    }
}
