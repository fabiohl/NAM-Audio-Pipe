// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::common::spsc::SHUTDOWN;
use std::sync::atomic::Ordering;

/// Stage field of a packed handler-state snapshot (the two high bits).
fn stage_bits(snapshot: u64) -> u64 {
    snapshot >> 62
}

#[test]
fn test_install_registers_sigint_and_sigterm_with_sa_restart() {
    let _guard = HandlerRestoreGuard::capture();
    let saved_int = saved_sigaction(libc::SIGINT);
    let saved_term = saved_sigaction(libc::SIGTERM);

    install_termination_signal_handlers().expect("termination handler installation must succeed");

    let expected = termination_signal_handler as *const () as libc::sighandler_t;
    for sig in [libc::SIGINT, libc::SIGTERM] {
        let current = saved_sigaction(sig);
        assert_eq!(
            current.sa_sigaction, expected,
            "signal {sig} must be wired to the unified termination handler"
        );
        assert_ne!(
            current.sa_flags & libc::SA_RESTART,
            0,
            "signal {sig} must be installed with SA_RESTART"
        );
    }

    restore_sigaction(libc::SIGINT, saved_int);
    restore_sigaction(libc::SIGTERM, saved_term);
}

#[test]
fn test_first_signal_sets_cooperative_shutdown_flag() {
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let guard = HandlerRestoreGuard::capture();
    assert!(
        !SHUTDOWN.load(Ordering::Acquire),
        "test assumes the process-global SHUTDOWN starts unset"
    );

    termination_signal_handler(libc::SIGINT);

    assert!(
        SHUTDOWN.load(Ordering::Acquire),
        "the first termination signal must cooperatively flip SHUTDOWN"
    );
    assert_eq!(
        stage_bits(termination_stage_snapshot_for_tests()),
        super::STAGE_GRACE >> 62,
        "the first delivery must open the grace window (stage 1)"
    );

    drop(guard);
    assert!(
        !SHUTDOWN.load(Ordering::Acquire),
        "guard must restore the previous SHUTDOWN value"
    );
}

/// In-burst double delivery (TERM followed by INT within milliseconds) must
/// produce exactly one graceful shutdown: `SHUTDOWN` flips once and the
/// process stays alive for WAV finalization.
///
/// Uses real kernel delivery (`raise` hands the signal to the calling thread
/// and returns after the handler ran) while the `SHUTDOWN_TEST_LOCK` keeps
/// every other handler-touching test out of the process-global state.
#[test]
fn burst_of_two_signals_within_grace_window_stays_cooperative() {
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let guard = HandlerRestoreGuard::capture();

    install_termination_signal_handlers().expect("termination handler installation must succeed");

    let burst_start = std::time::Instant::now();
    // SAFETY: `raise` delivers to the calling thread; the installed handler is
    // the staged async-signal-safe handler under test.
    assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
    assert!(
        SHUTDOWN.load(Ordering::Acquire),
        "first in-burst signal must flip SHUTDOWN synchronously"
    );
    assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
    let burst = burst_start.elapsed();
    assert!(
        burst < std::time::Duration::from_millis(10),
        "the two deliveries must land inside a single burst window, got {burst:?}"
    );

    // The process is alive (this assertion runs): the in-burst second delivery
    // was swallowed instead of escalating to `_exit`.
    assert_eq!(
        stage_bits(termination_stage_snapshot_for_tests()),
        super::STAGE_GRACE >> 62,
        "in-burst second delivery must stay inside the grace stage"
    );

    drop(guard);
}

/// A signal delivered after the grace window expired escalates to `_exit(1)`.
///
/// The child arm installs the handler, opens the grace window with a real
/// delivery, then shifts the monotonic anchor beyond `GRACE_WINDOW_MS`
/// (deterministic — no real sleep) and delivers again; the handler must
/// terminate the process with `_exit(1)` (exit code 1). The parent arm spawns
/// the same test binary pinned to this exact test and asserts the forced
/// termination.
#[test]
fn grace_window_expiry_escalates_to_forced_exit() {
    const ENV_KEY: &str = "NAM_SIGNAL_GRACE_EXPIRY_E2E";
    if std::env::var(ENV_KEY).is_ok() {
        let _guard = HandlerRestoreGuard::capture();
        install_termination_signal_handlers()
            .expect("termination handler installation must succeed");
        termination_signal_handler(libc::SIGTERM);
        assert!(
            SHUTDOWN.load(Ordering::Acquire),
            "first delivery must open the grace window"
        );

        // Simulate the required elapsed time by shifting the anchor behind
        // the window boundary; the next delivery computes the same predicate
        // the kernel-delivered late signal would.
        let now = super::monotonic_ms();
        let expired_anchor = now.saturating_sub(super::GRACE_WINDOW_MS + 60);
        super::TERMINATION_STATE.store(super::STAGE_GRACE | expired_anchor, Ordering::Release);

        // Must `_exit(1)` — if the escalation failed to fire, the process
        // survives and the marker below betrays it to the parent arm.
        termination_signal_handler(libc::SIGINT);
        eprintln!("GRACE_EXPIRY_TEST_NO_ESCAPE");
        return;
    }

    let exe = std::env::current_exe().expect("test executable path");
    // The substring filter is applied without `--exact` (libtest full paths
    // omit the crate prefix, which makes exact matching brittle across crates);
    // this test name is unique in the crate, so the child runs this one test.
    let filter = "grace_window_expiry_escalates_to_forced_exit";
    let output = std::process::Command::new(exe)
        .args([filter, "--nocapture"])
        .env(ENV_KEY, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("child test process must spawn");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
        "the child harness must run exactly the pinned escalation test"
    );
    assert!(
        !stderr.contains("GRACE_EXPIRY_TEST_NO_ESCAPE"),
        "the handler must terminate the process instead of returning: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "grace-window expiry escalation must be a `_exit(1)`, got {stderr}"
    );
}
