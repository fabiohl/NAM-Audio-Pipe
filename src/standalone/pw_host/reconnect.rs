// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Bounded reconnect policy and recovery cycle (F-RB-010 / T4.5).
//!
//! When the PipeWire daemon is restarted by the package manager, by the user
//! (`systemctl --user restart pipewire`) or by a USB interface reconnect, the
//! host must not abort immediately — a *bounded* retry policy re-establishes
//! the link with the daemon while every piece of internal state (models, IRs,
//! recording worker) stays intact. The retry budget is **strictly limited** in
//! number of attempts and in total time:
//!
//! * [`ReconnectPolicy`] — the configurable budget (production default: 3
//!   attempts with progressive 250 ms → 500 ms → 1000 ms exponential backoff,
//!   disabled entirely under `--fail-fast` or in unit-test environments);
//! * [`ReconnectCycle`] — the state machine driving the bounded retry loop in
//!   `run.rs`. It hands out at most [`ReconnectPolicy::max_attempts`] attempts
//!   and the exact backoff to wait before each one, so the caller can never
//!   loop unboundedly; `total_backoff_budget` is the hard cumulative time
//!   ceiling of the whole recovery cycle.
//!
//! The invariant is: **attempts are strictly bounded in number and time** — no
//! infinite reconnect loop can exist by construction. When the budget is
//! exhausted the host falls back to the fail-fast teardown path established in
//! [T4.4], finalizing recordings on disk and exiting with a non-zero status.

use std::time::Duration;

/// Production default: maximum number of reconnect attempts per session.
pub const PRODUCTION_MAX_ATTEMPTS: u32 = 3;
/// Production default: backoff before the first reconnect attempt (ms).
pub const PRODUCTION_INITIAL_BACKOFF_MS: u64 = 250;
/// Production default: exponential-backoff ceiling (ms).
pub const PRODUCTION_MAX_BACKOFF_MS: u64 = 1000;

/// Configurable bounded-reconnect policy (F-RB-010 / T4.5).
///
/// Reconnect is enabled by default with a conservative budget. It is disabled
/// when `--fail-fast` is present on the CLI or when running in a unit-test
/// environment, in which case the first backend failure triggers the T4.4
/// fail-fast teardown immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Maximum number of reconnect attempts per session (`0` = no retries).
    pub max_attempts: u32,
    /// Backoff to wait before the first reconnect attempt.
    pub initial_backoff: Duration,
    /// Ceiling for the exponential backoff growth.
    pub max_backoff: Duration,
    /// Whether reconnection is allowed at all.
    pub enabled: bool,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::production()
    }
}

impl ReconnectPolicy {
    /// Production defaults: 3 attempts with progressive exponential backoff
    /// (250 ms, 500 ms, 1000 ms) and reconnection enabled.
    pub fn production() -> Self {
        Self {
            max_attempts: PRODUCTION_MAX_ATTEMPTS,
            initial_backoff: Duration::from_millis(PRODUCTION_INITIAL_BACKOFF_MS),
            max_backoff: Duration::from_millis(PRODUCTION_MAX_BACKOFF_MS),
            enabled: true,
        }
    }

    /// Fail-fast policy: reconnection disabled (`--fail-fast` on the CLI, or
    /// unit-test environment). The first backend failure immediately triggers
    /// the T4.4 observable teardown with a non-zero exit code.
    pub fn fail_fast() -> Self {
        Self {
            max_attempts: 0,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            enabled: false,
        }
    }

    /// Whether this policy performs any reconnect attempt at all.
    pub fn is_disabled(&self) -> bool {
        !self.enabled || self.max_attempts == 0
    }

    /// Exponential backoff to wait **before** reconnect attempt `attempt`
    /// (1-based): `initial_backoff × 2^(attempt-1)`, clamped to `max_backoff`.
    ///
    /// The doubling is saturating: for any attempt number the result is finite
    /// and ≤ `max_backoff`, so the schedule can never hang nor wrap to zero.
    pub fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        debug_assert!(attempt >= 1, "attempts are 1-based");
        let initial_ms = self.initial_backoff.as_millis() as u64;
        let max_ms = self.max_backoff.as_millis() as u64;
        let shift = attempt.saturating_sub(1).min(63);
        // `1 << shift` is the exponential multiplier; for shift == 63 it still
        // fits u64, and `saturating_mul` clamps the product instead of
        // wrapping — a huge attempt number can never yield 0 ns.
        let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        let doubled = initial_ms.saturating_mul(multiplier);
        let capped = doubled.min(max_ms);
        Duration::from_millis(capped)
    }

    /// Worst-case cumulative backoff time of a full bounded recovery cycle —
    /// the hard time ceiling of the retry phase (sleeps only; per-attempt
    /// connection timeouts are additive and bounded by PipeWire's own
    /// handshake deadlines).
    pub fn total_backoff_budget(&self) -> Duration {
        let mut total_ms = 0u64;
        for attempt in 1..=self.max_attempts {
            total_ms =
                total_ms.saturating_add(self.backoff_for_attempt(attempt).as_millis() as u64);
        }
        Duration::from_millis(total_ms)
    }
}

/// Bounded reconnect-cycle state machine (F-RB-010 / T4.5).
///
/// The control loop in `run.rs` owns one [`ReconnectCycle`] per host session
/// and consults it after every backend failure. [`ReconnectCycle::begin_attempt`]
/// returns the backoff to wait before the next stream re-instantiation, and
/// returns `None` once the policy budget is exhausted — at which point the
/// caller must fall back to the T4.4 fail-fast path. The budget is never
/// reset: a session performs at most `max_attempts` reconnects total, so the
/// recovery phase is strictly bounded in number and time by construction.
#[derive(Debug, Clone)]
pub struct ReconnectCycle {
    policy: ReconnectPolicy,
    attempts_made: u32,
}

impl ReconnectCycle {
    /// Creates a cycle under `policy` with zero attempts made.
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self {
            policy,
            attempts_made: 0,
        }
    }

    /// The policy driving this cycle.
    pub fn policy(&self) -> &ReconnectPolicy {
        &self.policy
    }

    /// Reconnect attempts already consumed by this session.
    pub fn attempts_made(&self) -> u32 {
        self.attempts_made
    }

    /// Whether another reconnect attempt may still start under the policy.
    pub fn can_retry(&self) -> bool {
        !self.policy.is_disabled() && self.attempts_made < self.policy.max_attempts
    }

    /// Starts one reconnect attempt: returns the backoff to wait **before**
    /// re-instantiating the streams, or `None` when the bounded budget is
    /// exhausted (caller must fail fast via T4.4).
    ///
    /// Each call atomically consumes one slot of the budget; consecutive
    /// calls return monotonically non-decreasing backoffs until `max_attempts`
    /// is reached and `None` forever after — the loop cannot spin.
    pub fn begin_attempt(&mut self) -> Option<Duration> {
        if !self.can_retry() {
            return None;
        }
        self.attempts_made += 1;
        Some(self.policy.backoff_for_attempt(self.attempts_made))
    }
}

#[cfg(test)]
#[path = "reconnect_test.rs"]
mod tests;
