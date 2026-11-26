// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Backend teardown latch and RAII guard.

use super::machine::SharedBackendStatus;
use std::sync::atomic::Ordering;

impl SharedBackendStatus {
    /// Returns `true` if backend teardown or stream destruction is currently in progress.
    pub fn is_teardown_in_progress(&self) -> bool {
        self.teardown_in_progress.load(Ordering::Acquire)
    }

    /// Sets the teardown-in-progress latch.
    pub fn set_teardown_in_progress(&self, in_progress: bool) {
        self.teardown_in_progress
            .store(in_progress, Ordering::Release);
    }

    /// Enters the teardown phase, raising the `teardown_in_progress` latch
    /// and returning an RAII [`TeardownGuard`] that clears the latch on drop.
    pub fn enter_teardown(&self) -> TeardownGuard<'_> {
        self.set_teardown_in_progress(true);
        TeardownGuard { backend: self }
    }
}

/// RAII guard that manages the lifetime of the backend teardown latch.
///
/// While this guard is alive, [`SharedBackendStatus::is_teardown_in_progress`]
/// evaluates to `true`, preventing spurious `Unconnected` state transitions
/// (triggered synchronously by `pw_stream_destroy()`) from poisoning
/// subsequent reconnection attempts with premature `Failed` markers.
///
/// Dropping this guard (including during stack unwinding or normal function exit)
/// resets the teardown latch to `false`.
#[derive(Debug)]
pub struct TeardownGuard<'a> {
    backend: &'a SharedBackendStatus,
}

impl<'a> Drop for TeardownGuard<'a> {
    fn drop(&mut self) {
        self.backend.set_teardown_in_progress(false);
    }
}
