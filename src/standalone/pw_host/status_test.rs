// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use crate::standalone::pw_host::output_pw;
use neural_amp_modeler_rs::common::spsc::{RtStatusFlags, SHUTDOWN};
use pipewire::stream::StreamState;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// RAII guard restoring the process-global `SHUTDOWN` flag (same pattern as
/// `signals_test.rs`) so the SLA test leaves the flag pristine.
struct ShutdownRestore(bool);

impl ShutdownRestore {
    fn capture() -> Self {
        Self(SHUTDOWN.load(Ordering::Acquire))
    }
}

impl Drop for ShutdownRestore {
    fn drop(&mut self) {
        SHUTDOWN.store(self.0, Ordering::Release);
    }
}

#[test]
fn new_defaults_to_starting_and_healthy() {
    let backend = SharedBackendStatus::new();
    assert_eq!(backend.state(), BackendState::Starting);
    assert!(!backend.is_failed());
    assert_eq!(backend.failure(), None);
}

#[test]
fn mark_running_and_degraded_are_observable() {
    let backend = SharedBackendStatus::new();
    backend.mark_running();
    assert_eq!(backend.state(), BackendState::Running);
    assert!(!backend.is_failed());

    backend.mark_degraded("SPA format contract violated on capture: NotStereo(1)");
    assert_eq!(
        backend.state(),
        BackendState::Degraded {
            reason: "SPA format contract violated on capture: NotStereo(1)".into(),
        }
    );
    assert!(!backend.is_failed());
}

#[test]
fn mark_failed_is_sticky_and_exposes_detail() {
    let backend = SharedBackendStatus::new();
    backend.mark_failed("capture", "daemon disconnected");
    assert!(backend.is_failed());
    assert_eq!(
        backend.failure(),
        Some(("capture", "daemon disconnected".to_string()))
    );
    assert_eq!(
        backend.state(),
        BackendState::Failed {
            stream: "capture",
            reason: "daemon disconnected".into(),
        }
    );

    // Sticky: neither Running nor Degraded can erase the failure.
    backend.mark_running();
    backend.mark_degraded("late recovery attempt");
    assert!(backend.is_failed());
    assert_eq!(
        backend.failure(),
        Some(("capture", "daemon disconnected".to_string()))
    );
}

#[test]
fn mark_terminated_closes_lifecycle() {
    let backend = SharedBackendStatus::new();
    backend.mark_running();
    backend.mark_terminated();
    assert_eq!(backend.state(), BackendState::Terminated);
}

#[test]
fn begin_reconnect_clears_failure_and_publishes_reconnecting() {
    // F-RB-010 / T4.5: entering the bounded reconnect cycle must clear the
    // sticky failure so the fresh instance's `mark_running` is not a no-op,
    // and must publish the observable `Reconnecting` transition.
    let backend = SharedBackendStatus::new();
    backend.mark_failed("capture", "daemon restart");
    assert!(backend.is_failed());

    backend.begin_reconnect(1, 3, Duration::from_millis(250));
    assert!(!backend.is_failed(), "sticky failure cleared for recovery");
    assert_eq!(
        backend.failure(),
        None,
        "failure detail cleared for recovery"
    );
    assert_eq!(
        backend.state(),
        BackendState::Reconnecting {
            attempt: 1,
            total_attempts: 3,
            next_backoff: Duration::from_millis(250),
        }
    );
}

#[test]
fn successful_reconnect_returns_backend_to_running() {
    // The fresh instance reaches `Streaming` → `mark_running` must transition
    // `Reconnecting` → `Running` (the recovery completed observably).
    let backend = SharedBackendStatus::new();
    backend.mark_failed("playback", "stream disconnected from the audio backend");
    backend.begin_reconnect(1, 3, Duration::from_millis(250));
    backend.mark_running();
    assert_eq!(backend.state(), BackendState::Running);
    assert!(!backend.is_failed());
}

#[test]
fn failure_after_successful_reconnect_is_observable_again() {
    // A daemon that dies a second time after a recovered reconnect must be
    // observed as a fresh failure — the cycle budget governs how many more
    // attempts are allowed, but never swallows the event.
    let backend = SharedBackendStatus::new();
    backend.begin_reconnect(1, 3, Duration::from_millis(250));
    backend.mark_running();
    observe_stream_state(
        "capture",
        StreamState::Streaming,
        StreamState::Error("daemon died again".into()),
        &backend,
    );
    assert!(backend.is_failed());
    assert_eq!(
        backend.failure(),
        Some(("capture", "daemon died again".to_string()))
    );
}

#[test]
fn observe_error_transitions_to_failed() {
    let backend = SharedBackendStatus::new();
    observe_stream_state(
        "capture",
        StreamState::Streaming,
        StreamState::Error("connection reset by peer".into()),
        &backend,
    );
    assert!(backend.is_failed());
    assert_eq!(
        backend.failure(),
        Some(("capture", "connection reset by peer".to_string()))
    );
}

#[test]
fn observe_unconnected_after_streaming_marks_failed() {
    let backend = SharedBackendStatus::new();
    observe_stream_state(
        "playback",
        StreamState::Streaming,
        StreamState::Unconnected,
        &backend,
    );
    assert!(backend.is_failed());
    assert_eq!(
        backend.failure(),
        Some((
            "playback",
            "stream disconnected from the audio backend".to_string()
        ))
    );
}

#[test]
fn observe_unconnected_after_paused_marks_failed() {
    let backend = SharedBackendStatus::new();
    observe_stream_state(
        "capture",
        StreamState::Paused,
        StreamState::Unconnected,
        &backend,
    );
    assert!(backend.is_failed());
    assert!(backend.failure().is_some());
}

#[test]
fn observe_initial_unconnected_transition_is_not_fatal() {
    let backend = SharedBackendStatus::new();
    observe_stream_state(
        "capture",
        StreamState::Unconnected,
        StreamState::Connecting,
        &backend,
    );
    assert!(!backend.is_failed());
    assert_eq!(backend.state(), BackendState::Starting);
}

#[test]
fn observe_streaming_transition_marks_running() {
    let backend = SharedBackendStatus::new();
    observe_stream_state(
        "playback",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    observe_stream_state(
        "capture",
        StreamState::Paused,
        StreamState::Streaming,
        &backend,
    );
    assert_eq!(backend.state(), BackendState::Running);
    assert!(!backend.is_failed());
}

#[test]
fn observe_pause_transition_is_transient() {
    let backend = SharedBackendStatus::new();
    backend.mark_running();
    observe_stream_state(
        "capture",
        StreamState::Streaming,
        StreamState::Paused,
        &backend,
    );
    assert!(!backend.is_failed());
    assert_eq!(backend.state(), BackendState::Starting);
}

#[test]
fn failure_detection_terminates_control_loop_within_sla() {
    // Acceptance (F-RB-010 / T4.4): a fatal backend failure must be observed by
    // the control loop and lead to teardown + error return within < 200 ms —
    // zero zombie processes. This mirrors the exact poll pattern of the main
    // control loop in `run.rs` (SHUTDOWN condition + `is_failed` fast-path).
    let _shutdown = ShutdownRestore::capture();
    let backend = Arc::new(SharedBackendStatus::new());
    let loop_handle = {
        let backend = Arc::clone(&backend);
        std::thread::spawn(move || {
            let started = Instant::now();
            while !SHUTDOWN.load(Ordering::Acquire) {
                if backend.is_failed() {
                    return started.elapsed();
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            started.elapsed()
        })
    };

    // Let the loop run a few iterations, then kill the backend.
    std::thread::sleep(Duration::from_millis(20));
    backend.mark_failed("capture", "daemon restart");

    let detection_latency = loop_handle.join().expect("control-loop worker panicked");
    assert!(
        detection_latency < Duration::from_millis(200),
        "fail-fast detection exceeded the 200 ms acceptance SLA: {detection_latency:?}"
    );
    assert_eq!(
        backend.failure(),
        Some(("capture", "daemon restart".to_string()))
    );
}

#[test]
fn concurrent_transitions_never_leak_incoherent_snapshots() {
    // T6.5 model-check regression: concurrent `mark_failed` /
    // `mark_running` / `mark_degraded` / `begin_reconnect` writers must never
    // produce an internally inconsistent snapshot. Before the T6.5 hardening
    // this test caught the check-then-act race where an in-flight
    // `mark_running` could overwrite a `Failed` state while the failure flag
    // was already published (`failed == true` with `state == Running`).
    let backend = Arc::new(SharedBackendStatus::new());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let violation = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut workers = Vec::new();
    for w in 0..8 {
        let backend = Arc::clone(&backend);
        let stop = Arc::clone(&stop);
        workers.push(std::thread::spawn(move || {
            let mut iter = 0u64;
            while !stop.load(Ordering::Acquire) {
                match (w + iter as usize) % 6 {
                    0 => backend.mark_failed("capture", "simulated daemon restart"),
                    1 => backend.mark_running(),
                    2 => backend.mark_degraded("simulated SPA violation"),
                    3 => backend.begin_reconnect(1, 3, Duration::from_millis(1)),
                    4 => {
                        let _ = backend.state();
                    }
                    _ => {
                        let _ = backend.failure();
                    }
                }
                iter += 1;
            }
        }));
    }

    let checker = {
        let backend = Arc::clone(&backend);
        let stop = Arc::clone(&stop);
        let violation = Arc::clone(&violation);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if !backend.snapshot().invariants_hold() {
                    violation.store(true, Ordering::Release);
                    break;
                }
                std::thread::yield_now();
            }
        })
    };

    std::thread::sleep(Duration::from_millis(500));
    stop.store(true, Ordering::Release);
    for w in workers {
        w.join().expect("transition worker panicked");
    }
    checker.join().expect("snapshot checker panicked");

    assert!(
        !violation.load(Ordering::Acquire),
        "concurrent transitions produced an incoherent status snapshot"
    );
    assert!(
        backend.snapshot().invariants_hold(),
        "final status snapshot is incoherent"
    );
}

#[test]
fn single_stream_format_ok_does_not_unmute_or_mark_both_active() {
    let rt = RtStatusFlags::default();
    rt.capture_format_ok.store(0, Ordering::Relaxed);
    rt.playback_format_ok.store(0, Ordering::Relaxed);
    let backend = SharedBackendStatus::new();

    // Case 1: Capture valid format, playback not-yet-ok
    output_pw::mark_format_contract_ok(&rt, "capture");
    backend.set_stream_active("capture", true);

    // Audio MUST remain muted until all 4 conditions (capture_format_ok, playback_format_ok, capture_active, playback_active) are met
    assert!(!rt.is_audio_unmuted());
    assert_ne!(backend.state(), BackendState::Running);

    // Case 2: Playback valid format, capture not-yet-ok
    let rt2 = RtStatusFlags::default();
    rt2.capture_format_ok.store(0, Ordering::Relaxed);
    rt2.playback_format_ok.store(0, Ordering::Relaxed);
    let backend2 = SharedBackendStatus::new();
    output_pw::mark_format_contract_ok(&rt2, "playback");
    backend2.set_stream_active("playback", true);

    assert!(!rt2.is_audio_unmuted());
    assert_ne!(backend2.state(), BackendState::Running);

    // Case 3: Both format contracts ok AND both streams active -> is_audio_unmuted becomes true and state becomes Running
    output_pw::mark_format_contract_ok(&rt, "playback");
    backend.set_stream_active("playback", true);

    assert!(rt.is_audio_unmuted());
    assert_eq!(backend.state(), BackendState::Running);
}

#[test]
fn invalid_stream_format_rejection_prevents_unmute() {
    let rt = RtStatusFlags::default();
    let backend = SharedBackendStatus::new();

    // Valid capture format & active stream
    output_pw::mark_format_contract_ok(&rt, "capture");
    backend.set_stream_active("capture", true);

    // Rejected/invalid playback format
    output_pw::reject_negotiated_format_violation(
        &rt,
        "playback",
        output_pw::ContractViolation::NotStereo(1),
    );
    backend.set_stream_active("playback", true);

    // Audio must stay muted if playback format is invalid
    assert!(!rt.is_audio_unmuted());

    // Inverse: valid playback format & active stream, rejected capture format
    let rt2 = RtStatusFlags::default();
    let backend2 = SharedBackendStatus::new();

    output_pw::mark_format_contract_ok(&rt2, "playback");
    backend2.set_stream_active("playback", true);
    output_pw::reject_negotiated_format_violation(
        &rt2,
        "capture",
        output_pw::ContractViolation::NotStereo(1),
    );
    backend2.set_stream_active("capture", true);

    assert!(!rt2.is_audio_unmuted());
}
