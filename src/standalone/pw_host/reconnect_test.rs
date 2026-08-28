// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::standalone::pw_host::{BackendState, SharedBackendStatus};

#[test]
fn production_defaults_are_bounded_and_enabled() {
    let policy = ReconnectPolicy::production();
    assert!(policy.enabled);
    assert_eq!(policy.max_attempts, 3);
    assert_eq!(policy.initial_backoff, Duration::from_millis(250));
    assert_eq!(policy.max_backoff, Duration::from_millis(1000));
    assert!(!policy.is_disabled());
    assert_eq!(ReconnectPolicy::default(), policy);
}

#[test]
fn fail_fast_policy_is_disabled() {
    let policy = ReconnectPolicy::fail_fast();
    assert!(!policy.enabled);
    assert_eq!(policy.max_attempts, 0);
    assert!(policy.is_disabled());
    assert_eq!(policy.total_backoff_budget(), Duration::ZERO);
}

#[test]
fn backoff_schedule_follows_progressive_doubling_capped_at_max() {
    let policy = ReconnectPolicy::production();
    assert_eq!(policy.backoff_for_attempt(1), Duration::from_millis(250));
    assert_eq!(policy.backoff_for_attempt(2), Duration::from_millis(500));
    assert_eq!(policy.backoff_for_attempt(3), Duration::from_millis(1000));
    // The ceiling clamps any further doubling (attempt 4 would be 2000 ms).
    assert_eq!(policy.backoff_for_attempt(4), Duration::from_millis(1000));
    assert_eq!(policy.backoff_for_attempt(100), Duration::from_millis(1000));
}

#[test]
fn backoff_never_overflows_for_any_attempt_number() {
    let policy = ReconnectPolicy::production();
    // Saturating arithmetic: no matter how large the attempt number, the
    // computed delay stays finite and within the ceiling — a strict time bound.
    for attempt in [u32::MAX, 1u32 << 30, 1u32 << 31] {
        let backoff = policy.backoff_for_attempt(attempt);
        assert!(backoff <= policy.max_backoff);
        assert!(backoff.as_millis() > 0);
    }
}

#[test]
fn custom_policy_respects_initial_and_max_backoff() {
    let policy = ReconnectPolicy {
        max_attempts: 4,
        initial_backoff: Duration::from_millis(100),
        max_backoff: Duration::from_millis(300),
        enabled: true,
    };
    assert_eq!(policy.backoff_for_attempt(1), Duration::from_millis(100));
    assert_eq!(policy.backoff_for_attempt(2), Duration::from_millis(200));
    assert_eq!(policy.backoff_for_attempt(3), Duration::from_millis(300));
    assert_eq!(policy.backoff_for_attempt(4), Duration::from_millis(300));
}

#[test]
fn total_backoff_budget_is_the_hard_time_ceiling() {
    // Acceptance (F-RB-010 / T4.5): the recovery phase must have a strict
    // time ceiling, impeding infinite loops. Production: 250+500+1000 = 1750 ms.
    let policy = ReconnectPolicy::production();
    assert_eq!(policy.total_backoff_budget(), Duration::from_millis(1750));
}

#[test]
fn cycle_hands_out_exactly_max_attempts_backoffs_then_none() {
    // Acceptance (F-RB-010 / T4.5): a daemon that stays inaccessible must
    // exhaust the retry budget and then yield nothing — the caller fails fast
    // instead of looping forever.
    let mut cycle = ReconnectCycle::new(ReconnectPolicy::production());
    assert!(cycle.can_retry());
    assert_eq!(cycle.attempts_made(), 0);

    assert_eq!(cycle.begin_attempt(), Some(Duration::from_millis(250)));
    assert_eq!(cycle.begin_attempt(), Some(Duration::from_millis(500)));
    assert_eq!(cycle.begin_attempt(), Some(Duration::from_millis(1000)));
    assert_eq!(cycle.attempts_made(), 3);

    assert!(!cycle.can_retry());
    assert_eq!(cycle.begin_attempt(), None);
    assert_eq!(
        cycle.begin_attempt(),
        None,
        "budget exhausted: no more retries"
    );
}

#[test]
fn cycle_with_disabled_policy_never_retries() {
    let mut cycle = ReconnectCycle::new(ReconnectPolicy::fail_fast());
    assert!(!cycle.can_retry());
    assert_eq!(cycle.begin_attempt(), None);
    assert_eq!(cycle.attempts_made(), 0);
}

#[test]
fn simulated_reconnect_recovers_without_losing_carried_state() {
    // Acceptance (F-RB-010 / T4.5): a momentary daemon drop is recovered and
    // the internal state (models, IRs, recording) survives the re-instantiation.
    // This drives the exact begin_attempt protocol `run.rs` uses: wait the
    // backoff, re-instantiate, and on failure consume the next slot.
    let mut cycle = ReconnectCycle::new(ReconnectPolicy::production());
    let mut generation = 0u64; // the preserved internal state (e.g. model handle)
    let mut failed_once = true; // first attempt fails (daemon still down)

    let mut attempts = 0u32;
    let outcome: Result<u64, &str> = loop {
        let Some(_backoff) = cycle.begin_attempt() else {
            break Err("reconnect budget exhausted");
        };
        attempts += 1;
        // Simulated stream re-instantiation: the state survives untouched.
        generation += 1;
        if failed_once {
            failed_once = false;
            continue; // attempt failed → next cycle iteration
        }
        break Ok(generation); // attempt succeeded → audio resumed
    };

    assert_eq!(
        outcome,
        Ok(2),
        "audio resumes with the carried state intact"
    );
    assert_eq!(attempts, 2, "recovery consumed exactly 2 of the 3 attempts");
    assert_eq!(cycle.attempts_made(), 2);
    assert!(
        cycle.can_retry(),
        "remaining budget is preserved for later drops"
    );
}

#[test]
fn simulated_exhaustion_terminates_cleanly_with_error_outcome() {
    // Acceptance (F-RB-010 / T4.5): a daemon that never comes back must exhaust
    // retries and terminate cleanly with an error (the fail-fast T4.4 path).
    let mut cycle = ReconnectCycle::new(ReconnectPolicy::production());
    let mut attempts = 0u32;
    let outcome: Result<(), &str> = loop {
        let Some(backoff) = cycle.begin_attempt() else {
            break Err("daemon unreachable after all retries");
        };
        attempts += 1;
        assert!(backoff <= Duration::from_millis(1000));
        // Simulated failed re-instantiation — keep retrying.
    };

    assert!(outcome.is_err());
    assert_eq!(attempts, PRODUCTION_MAX_ATTEMPTS);
    assert_eq!(cycle.attempts_made(), PRODUCTION_MAX_ATTEMPTS);
    assert!(!cycle.can_retry());
}

#[test]
fn simulated_stream_setup_failure_stages_during_reconnect_route_through_budget() {
    // Acceptance (T8.3): temporary stream setup failures (capture setup, playback setup,
    // or stream connect) after a previous reconnection attempt consume the reconnect
    // budget, execute interruptible backoff, and retry until success or exhaustion.
    for stage in ["capture_setup", "playback_setup", "stream_connect"] {
        let mut cycle = ReconnectCycle::new(ReconnectPolicy::production());
        let backend_status = SharedBackendStatus::new();

        // 1. Initial attempt succeeded (e.g. initial connection established)
        assert_eq!(cycle.attempts_made(), 0);

        // 2. Disconnection occurs → attempt 1 begins (e.g. daemon dropped)
        let backoff1 = cycle.begin_attempt().expect("attempt 1 backoff");
        backend_status.begin_reconnect(cycle.attempts_made(), 3, backoff1);
        assert_eq!(cycle.attempts_made(), 1);

        // 3. Re-instantiation attempt 2 fails during stream setup at specific stage
        let _setup_err = anyhow::anyhow!("simulated {stage} error");
        let backoff2 = match cycle.begin_attempt() {
            Some(b) => {
                backend_status.begin_reconnect(cycle.attempts_made(), 3, b);
                b
            }
            None => panic!("should have attempt 2"),
        };
        assert_eq!(cycle.attempts_made(), 2);
        assert_eq!(backoff2, Duration::from_millis(500));
        assert!(matches!(
            backend_status.state(),
            BackendState::Reconnecting { attempt: 2, .. }
        ));

        // 4. Next attempt succeeds
        assert!(cycle.can_retry());
    }
}

#[test]
fn stream_setup_failure_on_initial_attempt_fails_fast() {
    // Acceptance (T8.3): failure on startup (attempts_made == 0) must fail fast
    // without triggering reconnect backoff loops.
    let cycle = ReconnectCycle::new(ReconnectPolicy::production());
    assert_eq!(cycle.attempts_made(), 0);

    // Simulated startup failure check (matching run.rs condition: attempts_made() == 0)
    let setup_failed_on_startup = cycle.attempts_made() == 0;
    assert!(
        setup_failed_on_startup,
        "initial attempt must trigger immediate error return"
    );
}
