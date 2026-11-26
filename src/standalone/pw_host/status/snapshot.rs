// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Coherent backend status snapshots for diagnostics and model checking.

use super::machine::BackendState;

/// A coherent snapshot of the backend status (model checking).
///
/// Read under a single acquisition of the state lock, so `failed`, `state`
/// and `failure` can never describe different instants — the model-check
/// gate in `tests/rt_metrics.rs` asserts the machine invariants on these
/// snapshots under 16 concurrent writers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendStatusSnapshot {
    /// Mirror of [`crate::standalone::pw_host::status::SharedBackendStatus::is_failed`] read atomically with the
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
