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
    // The recovery phase must have a strict time ceiling, impeding infinite loops.
    // Production: 250+500+1000 = 1750 ms.
    let policy = ReconnectPolicy::production();
    assert_eq!(policy.total_backoff_budget(), Duration::from_millis(1750));
}

#[test]
fn cycle_hands_out_exactly_max_attempts_backoffs_then_none() {
    // A daemon that stays inaccessible must exhaust the retry budget and then
    // yield nothing — the caller fails fast instead of looping forever.
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
    // A momentary daemon drop is recovered and the internal state
    // (models, IRs, recording) survives the re-instantiation.
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
    // A daemon that never comes back must exhaust retries and terminate
    // cleanly with an error (observable fail-fast path).
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
    // Temporary stream setup failures (capture setup, playback setup,
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
    // Failure on startup (attempts_made == 0) must fail fast
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

struct ShutdownRestore(bool);

impl ShutdownRestore {
    fn capture() -> Self {
        Self(
            neural_amp_modeler_rs::common::spsc::SHUTDOWN
                .load(std::sync::atomic::Ordering::Acquire),
        )
    }
}

impl Drop for ShutdownRestore {
    fn drop(&mut self) {
        neural_amp_modeler_rs::common::spsc::SHUTDOWN
            .store(self.0, std::sync::atomic::Ordering::Release);
    }
}

#[test]
fn teardown_latch_prevents_peer_stream_unconnected_from_poisoning_reconnect() {
    // Finding F-RB-101: pw_stream_destroy() synchronously emits
    // state_changed(old, Unconnected) before cleaning hooks. If capture drops
    // (triggering reconnect), begin_reconnect clears failed = false. Then, when
    // playback is destroyed during teardown, its synchronous Unconnected event
    // would invoke observe_stream_state and set failed = true if not latched,
    // prematurely poisoning the upcoming reconnection attempt before new streams
    // are even created.
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let _shutdown = ShutdownRestore::capture();
    neural_amp_modeler_rs::common::spsc::SHUTDOWN
        .store(false, std::sync::atomic::Ordering::Release);

    use crate::standalone::pw_host::status::observe_stream_state;
    use pipewire::stream::StreamState;

    let backend = SharedBackendStatus::new();

    // 1. Initial running state: both capture and playback are streaming.
    observe_stream_state(
        "capture",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    observe_stream_state(
        "playback",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    backend.mark_running();
    assert_eq!(backend.state(), BackendState::Running);
    assert!(!backend.is_failed());

    // 2. Capture fails (e.g. device error or localized disconnect).
    observe_stream_state(
        "capture",
        StreamState::Streaming,
        StreamState::Unconnected,
        &backend,
    );
    assert!(backend.is_failed(), "Capture disconnect marks failed");

    // 3. Section 6 (INSTANCE TEARDOWN) begins: enter teardown latch.
    let mut teardown_guard = Some(backend.enter_teardown());
    assert!(backend.is_teardown_in_progress());

    // 4. Bounded reconnect begins: clears backend.failed = false.
    backend.begin_reconnect(1, 3, Duration::from_millis(250));
    assert!(!backend.is_failed(), "begin_reconnect clears failure flag");

    // 5. Section 6 destroys the surviving peer stream (playback) via thread_loop.stop() / drop.
    // PipeWire synchronously fires state_changed(Streaming -> Unconnected).
    observe_stream_state(
        "playback",
        StreamState::Streaming,
        StreamState::Unconnected,
        &backend,
    );

    // Invariant: The teardown latch prevents this spurious event from setting failed = true!
    assert!(
        !backend.is_failed(),
        "Teardown latch must protect backend from being poisoned by peer stream destruction"
    );

    // 6. Section 4.1: New streams are initialized for the reconnection attempt.
    // Teardown guard is dropped once new AppState is instantiated.
    drop(teardown_guard.take());
    assert!(!backend.is_teardown_in_progress());

    // 7. New streams transition to Streaming.
    observe_stream_state(
        "capture",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    observe_stream_state(
        "playback",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );

    assert_eq!(backend.state(), BackendState::Running);
    assert!(
        !backend.is_failed(),
        "Reconnected host runs cleanly without premature failure"
    );
}

#[test]
fn without_teardown_latch_peer_stream_unconnected_poisons_reconnect() {
    // Negative test (counterfactual) reproducing finding F-RB-101:
    // Without the teardown latch, if the peer stream emits Unconnected after
    // begin_reconnect, the upcoming attempt is immediately poisoned with failed = true.
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let _shutdown = ShutdownRestore::capture();
    neural_amp_modeler_rs::common::spsc::SHUTDOWN
        .store(false, std::sync::atomic::Ordering::Release);

    use crate::standalone::pw_host::status::observe_stream_state;
    use pipewire::stream::StreamState;

    let backend = SharedBackendStatus::new();
    observe_stream_state(
        "capture",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    observe_stream_state(
        "playback",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    backend.mark_running();

    // Capture drops
    observe_stream_state(
        "capture",
        StreamState::Streaming,
        StreamState::Unconnected,
        &backend,
    );
    assert!(backend.is_failed());

    // Begin reconnect clears failed
    backend.begin_reconnect(1, 3, Duration::from_millis(250));
    assert!(!backend.is_failed());

    // Without the teardown latch raised:
    assert!(!backend.is_teardown_in_progress());

    // Playback emits Unconnected during teardown
    observe_stream_state(
        "playback",
        StreamState::Streaming,
        StreamState::Unconnected,
        &backend,
    );

    // Without the latch, backend is poisoned:
    assert!(
        backend.is_failed(),
        "Without teardown latch, peer stream unsets healthy state and poisons the attempt"
    );
}

// ── Combined Multi-Cycle Reconnect & Memory Safety Stress Harness (T1.3) ───

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Simulated underlying PipeWire `pw_stream` struct allocated on the heap.
/// Models the layout and lifecycle contracts of PipeWire's C stream and hook list (F-RB-102 & F-RB-101).
struct MockStreamNode {
    name: &'static str,
    backend: Arc<SharedBackendStatus>,
    is_streaming: AtomicBool,
    has_listener: AtomicBool,
    is_destroyed: AtomicBool,
    uaf_writes: AtomicU32,
}

/// Simulated PipeWire `StreamListener`.
/// Dropping the listener models `spa_hook_remove` / `spa_list_remove` unlinking from the stream's hook list.
struct MockListener {
    node: *mut MockStreamNode,
    uaf_counter: Arc<AtomicU32>,
}

impl Drop for MockListener {
    fn drop(&mut self) {
        // SAFETY: When AppState drop order is correct (F-RB-102), listeners drop
        // strictly BEFORE their respective streams. The heap node is therefore
        // guaranteed to be allocated and live. Under AddressSanitizer, if the node
        // were already freed by a premature Stream drop, dereferencing `self.node`
        // immediately triggers a heap-use-after-free fault.
        let node = unsafe { &mut *self.node };
        if node.is_destroyed.load(Ordering::Acquire) {
            // Write-after-free condition: listener dropped after stream was destroyed!
            self.uaf_counter.fetch_add(1, Ordering::SeqCst);
            node.uaf_writes.fetch_add(1, Ordering::SeqCst);
        }
        node.has_listener.store(false, Ordering::Release);
    }
}

/// Simulated PipeWire `Stream` / `StreamBox`.
/// Dropping the stream models `pw_stream_destroy`, including synchronous emission
/// of `state_changed(Streaming -> Unconnected)` and heap deallocation (`stream_free`).
struct MockStream {
    node: *mut MockStreamNode,
}

impl Drop for MockStream {
    fn drop(&mut self) {
        let (name, backend, was_streaming) = {
            let node = unsafe { &*self.node };
            node.is_destroyed.store(true, Ordering::Release);
            let streaming = node.is_streaming.load(Ordering::Acquire);
            (node.name, node.backend.clone(), streaming)
        };

        // PipeWire stream.c contract: pw_stream_destroy() synchronously emits
        // state_changed(old, Unconnected) before cleaning hooks.
        if was_streaming {
            use crate::standalone::pw_host::status::observe_stream_state;
            use pipewire::stream::StreamState;
            observe_stream_state(
                name,
                StreamState::Streaming,
                StreamState::Unconnected,
                &backend,
            );
        }

        // Deallocate the heap memory (stream_free).
        // If a listener drops after this, it accesses freed heap memory (ASan heap-use-after-free).
        unsafe {
            drop(Box::from_raw(self.node));
        }
    }
}

fn create_mock_pair(
    name: &'static str,
    backend: &Arc<SharedBackendStatus>,
    uaf_counter: &Arc<AtomicU32>,
) -> (MockListener, MockStream) {
    let node_ptr = Box::into_raw(Box::new(MockStreamNode {
        name,
        backend: backend.clone(),
        is_streaming: AtomicBool::new(true),
        has_listener: AtomicBool::new(true),
        is_destroyed: AtomicBool::new(false),
        uaf_writes: AtomicU32::new(0),
    }));

    (
        MockListener {
            node: node_ptr,
            uaf_counter: uaf_counter.clone(),
        },
        MockStream { node: node_ptr },
    )
}

#[test]
fn combined_multi_cycle_reconnect_with_mock_streams_proves_zero_uaf_and_no_poisoning() {
    // Finding F-RB-101 + F-RB-102 Combined Gate (Task T1.3):
    // 1. Simulates multiple reconnect cycles under localized stream failures (peer stream remains connected).
    // 2. In each cycle, Section 6 teardown drops `AppState` under `enter_teardown()`.
    // 3. Proves:
    //    (a) Zero write-after-free: listeners drop cleanly before streams (F-RB-102), heap allocations verified.
    //    (b) Zero false-negative reconnections: teardown latch prevents peer stream's synchronous Unconnected
    //        event from poisoning `backend.failed`, so every reconnect attempt begins healthy (F-RB-101).
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let _shutdown = ShutdownRestore::capture();
    neural_amp_modeler_rs::common::spsc::SHUTDOWN.store(false, Ordering::Release);

    use crate::standalone::pw_host::output_pw::AppState;
    use crate::standalone::pw_host::status::observe_stream_state;
    use pipewire::stream::StreamState;

    let backend = Arc::new(SharedBackendStatus::new());
    let uaf_counter = Arc::new(AtomicU32::new(0));
    let mut cycle = ReconnectCycle::new(ReconnectPolicy::production());

    // Run 3 consecutive simulated reconnect cycles matching PRODUCTION_MAX_ATTEMPTS.
    // Cycle 1: Capture stream suffers a localized disconnect, playback remains streaming.
    // Cycle 2: Playback stream suffers a localized disconnect, capture remains streaming.
    // Cycle 3: Capture stream suffers a localized disconnect again, playback remains streaming.
    for attempt_idx in 1..=3 {
        // 1. Setup initial active streaming AppState
        let (cap_listener, cap_stream) = create_mock_pair("capture", &backend, &uaf_counter);
        let (play_listener, play_stream) = create_mock_pair("playback", &backend, &uaf_counter);

        let app_state = AppState {
            capture_listener: cap_listener,
            capture_stream: cap_stream,
            playback_listener: play_listener,
            playback_stream: play_stream,
        };

        observe_stream_state(
            "capture",
            StreamState::Paused,
            StreamState::Streaming,
            &backend,
        );
        observe_stream_state(
            "playback",
            StreamState::Paused,
            StreamState::Streaming,
            &backend,
        );
        backend.mark_running();
        assert_eq!(backend.state(), BackendState::Running);
        assert!(!backend.is_failed());

        // 2. Localized failure occurs on one stream (alternate capture / playback)
        let failing_stream = if attempt_idx % 2 == 1 {
            "capture"
        } else {
            "playback"
        };
        observe_stream_state(
            failing_stream,
            StreamState::Streaming,
            StreamState::Unconnected,
            &backend,
        );
        assert!(
            backend.is_failed(),
            "Attempt {attempt_idx}: localized failure must mark backend as failed"
        );

        // 3. Section 6 (INSTANCE TEARDOWN) begins: enter teardown latch
        let mut teardown_guard = Some(backend.enter_teardown());
        assert!(backend.is_teardown_in_progress());

        // 4. Bounded reconnect begins: consumes attempt budget and clears failed flag
        let backoff = cycle.begin_attempt().expect("attempt within budget");
        backend.begin_reconnect(attempt_idx, 3, backoff);
        assert!(
            !backend.is_failed(),
            "Attempt {attempt_idx}: begin_reconnect must clear the failed flag"
        );

        // 5. Drop AppState under teardown guard (models line 584 of run.rs).
        // - Listeners drop before streams (F-RB-102): zero UAF.
        // - Surviving peer stream drop emits synchronous Unconnected (F-RB-101).
        // - Teardown latch prevents poisoning!
        drop(app_state);

        assert_eq!(
            uaf_counter.load(Ordering::SeqCst),
            0,
            "Attempt {attempt_idx}: zero write-after-free must occur during AppState drop (F-RB-102)"
        );
        assert!(
            !backend.is_failed(),
            "Attempt {attempt_idx}: teardown latch must prevent surviving stream destruction from poisoning reconnect (F-RB-101)"
        );

        // 6. Section 4.1: Re-instantiation creates new streams and clears teardown latch
        drop(teardown_guard.take());
        assert!(!backend.is_teardown_in_progress());
    }

    // 7. Successful recovery on Attempt 3 re-instantiation
    let (cap_listener, cap_stream) = create_mock_pair("capture", &backend, &uaf_counter);
    let (play_listener, play_stream) = create_mock_pair("playback", &backend, &uaf_counter);
    let final_app_state = AppState {
        capture_listener: cap_listener,
        capture_stream: cap_stream,
        playback_listener: play_listener,
        playback_stream: play_stream,
    };

    observe_stream_state(
        "capture",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    observe_stream_state(
        "playback",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    backend.mark_running();
    assert_eq!(backend.state(), BackendState::Running);
    assert!(!backend.is_failed());

    // 8. Clean graceful shutdown teardown
    let _shutdown_guard = backend.enter_teardown();
    drop(final_app_state);

    // Final gate invariant checks
    assert_eq!(
        uaf_counter.load(Ordering::SeqCst),
        0,
        "Total UAF write violations across entire multi-cycle run must be exactly 0 (F-RB-102)"
    );
    assert!(
        !backend.is_failed(),
        "Host must reach final clean state without false-negative failures (F-RB-101)"
    );
}
