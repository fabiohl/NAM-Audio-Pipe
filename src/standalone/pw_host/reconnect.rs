// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Bounded reconnect policy and recovery cycle.
//!
//! When the PipeWire daemon is restarted by the package manager, by the user
//! (`systemctl --user restart pipewire`) or by a USB interface reconnect, the
//! host must not abort immediately — a *bounded* retry policy re-establishes
//! the link with the daemon while every piece of internal state (models, IRs,
//! recording worker) stays intact. The retry budget is **strictly limited** in
//! number of attempts and in total time:
//!
//! * [`ReconnectPolicy`] — the configurable budget (production default: 6
//!   attempts with progressive 250 ms → 4 s exponential backoff, ≈ 11.75 s of
//!   cumulative backoff; disabled entirely under `--fail-fast`);
//! * [`ReconnectCycle`] — the state machine driving the bounded retry loop in
//!   `run.rs`. It hands out at most [`ReconnectPolicy::max_attempts`] attempts
//!   and the exact backoff to wait before each one, so the caller can never
//!   loop unboundedly; `total_backoff_budget` is the hard cumulative time
//!   ceiling of a single recovery episode.
//!
//! ## Per-episode budget and stability reset (F-UX-09)
//!
//! A real PipeWire restart takes 1–4 s, and isolated drops (USB, suspend/resume)
//! are rare but repeated over a long session. A never-reset session budget would
//! therefore kill the host after a handful of *independent* drops even with long
//! healthy uptime between them. [`ReconnectCycle::note_stable`] closes that gap:
//! after [`STABLE_RESET`] of uninterrupted healthy streaming the per-episode
//! attempt budget is replenished. The limit is still **strictly per-episode** —
//! within a single incident the budget can never be reset (a failure always
//! clears the healthy window), so no infinite reconnect loop can exist by
//! construction. Setting [`STABLE_RESET`] to `Duration::MAX` disables
//! replenishment; the shorter pre-F-UX-09 budget (3 attempts, 250 → 1000 ms) is
//! restored through the [`PRODUCTION_MAX_ATTEMPTS`] / [`PRODUCTION_MAX_BACKOFF_MS`]
//! constants.
//!
//! The invariant is: **attempts are strictly bounded in number and time per
//! episode** — no infinite reconnect loop can exist by construction. When the
//! budget is exhausted the host falls back to the fail-fast teardown path,
//! finalizing recordings on disk and exiting with a non-zero status.

use std::time::{Duration, Instant};

use crate::recording::alert::{Clock, SystemClock};

/// Production default: maximum number of reconnect attempts per episode.
pub const PRODUCTION_MAX_ATTEMPTS: u32 = 6;
/// Production default: backoff before the first reconnect attempt (ms).
pub const PRODUCTION_INITIAL_BACKOFF_MS: u64 = 250;
/// Production default: exponential-backoff ceiling (ms).
pub const PRODUCTION_MAX_BACKOFF_MS: u64 = 4000;

/// Continuous healthy-streaming window that replenishes the per-episode retry
/// budget (F-UX-09). `Duration::MAX` disables replenishment entirely — the
/// documented rollback to the never-reset budget.
pub const STABLE_RESET: Duration = Duration::from_secs(60);

/// Configurable bounded-reconnect policy.
///
/// Reconnect is enabled by default with a conservative budget. It is disabled
/// when `--fail-fast` is present on the CLI or when running in a unit-test
/// environment, in which case the first backend failure triggers observable
/// teardown immediately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectPolicy {
    /// Maximum number of reconnect attempts per episode (`0` = no retries).
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
    /// Production defaults (F-UX-09): 6 attempts with progressive exponential
    /// backoff (250 ms, 500 ms, 1 s, 2 s, 4 s, 4 s — cumulative 11.75 s per
    /// episode) and reconnection enabled.
    pub fn production() -> Self {
        Self {
            max_attempts: PRODUCTION_MAX_ATTEMPTS,
            initial_backoff: Duration::from_millis(PRODUCTION_INITIAL_BACKOFF_MS),
            max_backoff: Duration::from_millis(PRODUCTION_MAX_BACKOFF_MS),
            enabled: true,
        }
    }

    /// Builds a production-shaped policy (250 ms initial, 4 s ceiling) whose
    /// cumulative per-episode backoff budget does not exceed `timeout`
    /// (`--reconnect-timeout <s>`).
    ///
    /// The attempt count is trimmed to the largest schedule that fits; at least
    /// one attempt is always kept, so a `timeout` smaller than the first
    /// backoff still performs a single retry. The stability reset
    /// ([`STABLE_RESET`]) is unaffected: the cap is per-episode.
    pub fn with_backoff_timeout(timeout: Duration) -> Self {
        let mut policy = Self::production();
        while policy.max_attempts > 1 && policy.total_backoff_budget() > timeout {
            policy.max_attempts -= 1;
        }
        policy
    }

    /// Fail-fast policy: reconnection disabled (`--fail-fast` on the CLI, or
    /// unit-test environment). The first backend failure immediately triggers
    /// observable teardown with a non-zero exit code.
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

    /// Worst-case cumulative backoff time of a full bounded recovery episode —
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

/// Outcome of a healthy control-loop poll ([`ReconnectCycle::note_stable`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StabilityEvent {
    /// No stability transition on this poll.
    None,
    /// Healthy streaming resumed after at least one reconnect attempt: the
    /// caller emits the restoration confirmation (`🎸 Audio restored ...`).
    Restored {
        /// Attempts consumed by the episode that just recovered.
        attempts: u32,
    },
    /// Uninterrupted healthy streaming for [`STABLE_RESET`] replenished the
    /// per-episode budget (`attempts_made` reset to `0`).
    Replenished,
}

/// Bounded reconnect-cycle state machine.
///
/// The control loop in `run.rs` owns one [`ReconnectCycle`] per host session
/// and consults it after every backend failure. [`ReconnectCycle::begin_attempt`]
/// returns the backoff to wait before the next stream re-instantiation, and
/// returns `None` once the policy budget is exhausted — at which point the
/// caller must fall back to the fail-fast path.
///
/// The budget is bounded **per episode**: within a single incident it can never
/// be reset, but [`ReconnectCycle::note_stable`] replenishes it after
/// [`STABLE_RESET`] of uninterrupted healthy streaming (F-UX-09), so isolated
/// drops spread over a long session never accumulate into an exhaustion.
#[derive(Debug, Clone)]
pub struct ReconnectCycle<C: Clock = SystemClock> {
    policy: ReconnectPolicy,
    attempts_made: u32,
    clock: C,
    /// Start of the current uninterrupted healthy-streaming window; `None`
    /// until the control loop observes a healthy iteration, and cleared by
    /// [`begin_attempt`](Self::begin_attempt) / [`note_unstable`](Self::note_unstable)
    /// — any break in continuity forces a fresh [`STABLE_RESET`].
    stable_since: Option<Instant>,
    /// Set once an attempt is consumed and cleared by the first healthy poll:
    /// drives the single [`StabilityEvent::Restored`] after a recovery.
    awaiting_restore: bool,
}

impl ReconnectCycle<SystemClock> {
    /// Creates a cycle under `policy` with zero attempts made and the real
    /// monotonic clock.
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self::with_clock(policy, SystemClock)
    }
}

impl<C: Clock> ReconnectCycle<C> {
    /// Creates a cycle under `policy` driven by an explicit clock (tests).
    pub fn with_clock(policy: ReconnectPolicy, clock: C) -> Self {
        Self {
            policy,
            attempts_made: 0,
            clock,
            stable_since: None,
            awaiting_restore: false,
        }
    }

    /// The policy driving this cycle.
    pub fn policy(&self) -> &ReconnectPolicy {
        &self.policy
    }

    /// Reconnect attempts already consumed by the current episode.
    pub fn attempts_made(&self) -> u32 {
        self.attempts_made
    }

    /// Whether another reconnect attempt may still start under the policy.
    pub fn can_retry(&self) -> bool {
        !self.policy.is_disabled() && self.attempts_made < self.policy.max_attempts
    }

    /// Starts one reconnect attempt: returns the backoff to wait **before**
    /// re-instantiating the streams, or `None` when the bounded budget is
    /// exhausted (caller must fail fast).
    ///
    /// Each call atomically consumes one slot of the episode budget; consecutive
    /// calls return monotonically non-decreasing backoffs until `max_attempts`
    /// is reached and `None` forever after — the loop cannot spin.
    pub fn begin_attempt(&mut self) -> Option<Duration> {
        if !self.can_retry() {
            return None;
        }
        self.attempts_made += 1;
        // A failed instance breaks the healthy-streaming window: the next
        // stability reset needs a fresh `STABLE_RESET` of continuous streaming,
        // and the upcoming recovery is announced exactly once.
        self.stable_since = None;
        self.awaiting_restore = true;
        Some(self.policy.backoff_for_attempt(self.attempts_made))
    }

    /// Reports one healthy, streaming control-loop iteration (F-UX-09).
    ///
    /// The first healthy poll after a recovery returns
    /// [`StabilityEvent::Restored`] (once); once the uninterrupted window
    /// reaches [`STABLE_RESET`] the per-episode budget is replenished and
    /// [`StabilityEvent::Replenished`] is returned. Every other poll returns
    /// [`StabilityEvent::None`]. The clock is injected, so the cadence is proven
    /// deterministically without sleeping.
    ///
    /// Must only be called while the backend is genuinely streaming; leaving
    /// that state must be reported through [`note_unstable`](Self::note_unstable)
    /// (or [`begin_attempt`](Self::begin_attempt)) so the window stays honest.
    pub fn note_stable(&mut self) -> StabilityEvent {
        let now = self.clock.now();
        let restored = if self.awaiting_restore {
            self.awaiting_restore = false;
            Some(StabilityEvent::Restored {
                attempts: self.attempts_made,
            })
        } else {
            None
        };
        match self.stable_since {
            None => {
                self.stable_since = Some(now);
                restored.unwrap_or(StabilityEvent::None)
            }
            Some(since) => {
                let elapsed = now.checked_duration_since(since).unwrap_or_default();
                if elapsed >= STABLE_RESET {
                    // Restart the window and replenish the episode budget.
                    self.stable_since = Some(now);
                    if self.attempts_made > 0 {
                        self.attempts_made = 0;
                        return StabilityEvent::Replenished;
                    }
                }
                restored.unwrap_or(StabilityEvent::None)
            }
        }
    }

    /// Reports that the backend is not (or no longer) streaming healthily: the
    /// continuous-streaming window is broken, so a later replenishment needs a
    /// fresh [`STABLE_RESET`] of uninterrupted streaming. Cheap and idempotent.
    pub fn note_unstable(&mut self) {
        self.stable_since = None;
    }
}

#[cfg(test)]
#[path = "reconnect_test.rs"]
mod tests;
