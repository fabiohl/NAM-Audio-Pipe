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
fn test_install_registers_all_termination_signals_with_sa_restart() {
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let _guard = HandlerRestoreGuard::capture();
    let saved: Vec<_> = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .iter()
        .map(|&sig| (sig, saved_sigaction(sig)))
        .collect();

    install_termination_signal_handlers().expect("termination handler installation must succeed");

    let expected = termination_signal_handler as *const () as libc::sighandler_t;
    // Cover all four dispositions wired to the unified handler (SIGINT, SIGTERM,
    // SIGHUP, SIGQUIT); SIGHUP matters because closing a `Terminal=true` launcher
    // window otherwise kills the process with no graceful teardown.
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
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

    for (sig, disp) in saved {
        restore_sigaction(sig, disp);
    }
}

/// `SIGHUP`/`SIGQUIT` must open the same cooperative grace stage as
/// `SIGINT`/`SIGTERM` (kernel delivery via `raise`, handler returns).
#[test]
fn sighup_and_sigquit_open_the_cooperative_grace_stage() {
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let guard = HandlerRestoreGuard::capture();
    install_termination_signal_handlers().expect("termination handler installation must succeed");

    for sig in [libc::SIGHUP, libc::SIGQUIT] {
        // Reset the packed stage between signals (the handler latches GRACE on
        // first delivery, and a second delivery would be swallowed otherwise).
        reset_termination_stage_for_tests();
        SHUTDOWN.store(false, Ordering::Release);

        // SAFETY: `raise` delivers to the calling thread; the installed handler
        // is the staged async-signal-safe handler under test.
        assert_eq!(unsafe { libc::raise(sig) }, 0);
        assert!(
            SHUTDOWN.load(Ordering::Acquire),
            "signal {sig} must cooperatively flip SHUTDOWN"
        );
        assert_eq!(
            stage_bits(termination_stage_snapshot_for_tests()),
            super::STAGE_GRACE >> 62,
            "signal {sig} must open the grace window (stage 1)"
        );
    }

    drop(guard);
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
    // SAFETY: `raise` delivers to the calling thread; the same staged
    // async-signal-safe handler covers this second in-burst signal.
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
        let expired_anchor = now.saturating_sub(super::grace_window_ms() + 60);
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
    assert!(
        stderr.contains("forced exit: capture may be incomplete"),
        "the forced `_exit(1)` path must write the static capture-loss warning \
         to stderr: {stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "grace-window expiry escalation must be a `_exit(1)`, got {stderr}"
    );
}

/// A recording widens the grace window to [`super::RECORDING_GRACE_WINDOW_MS`];
/// a second signal 700 ms after the first must then be swallowed (the graceful
/// teardown — draining the ring, rewriting the header, `fsync` — is allowed to
/// finish), exactly the F-UX-04 scenario that force-exited mid-finalization at
/// the 500 ms default.
///
/// Deterministic: the 700 ms interval is simulated by shifting the packed
/// anchor 700 ms into the past rather than sleeping.
#[test]
fn recording_grace_window_swallows_a_signal_700ms_after_the_first() {
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let guard = HandlerRestoreGuard::capture();
    install_termination_signal_handlers().expect("termination handler installation must succeed");

    // Arm the extended window exactly like `main` does under `--record`.
    set_grace_window(RECORDING_GRACE_WINDOW_MS);

    // First delivery opens the window.
    termination_signal_handler(libc::SIGINT);
    assert!(
        SHUTDOWN.load(Ordering::Acquire),
        "the first signal must cooperatively flip SHUTDOWN"
    );

    // Simulate 700 ms elapsed since the anchor.
    let now = monotonic_ms();
    let anchor_700ms = now.saturating_sub(700);
    TERMINATION_STATE.store(STAGE_GRACE | anchor_700ms, Ordering::Release);

    // 700 ms < 5000 ms: swallowed, the process survives (this line runs).
    termination_signal_handler(libc::SIGINT);
    assert_eq!(
        stage_bits(termination_stage_snapshot_for_tests()),
        STAGE_GRACE >> 62,
        "a signal 700 ms in with a 5000 ms window must stay in the grace stage"
    );

    drop(guard);
}

/// The live grace window is settable and readable, and `0` is clamped to `1`
/// (a zero window can never make the escalation instantaneous).
#[test]
fn grace_window_set_and_read_roundtrip_with_clamp() {
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    let guard = HandlerRestoreGuard::capture();

    set_grace_window(RECORDING_GRACE_WINDOW_MS);
    assert_eq!(grace_window_ms(), RECORDING_GRACE_WINDOW_MS);

    set_grace_window(0);
    assert_eq!(grace_window_ms(), 1, "a zero window must clamp to 1 ms");

    set_grace_window(DEFAULT_GRACE_WINDOW_MS);
    assert_eq!(grace_window_ms(), DEFAULT_GRACE_WINDOW_MS);

    drop(guard);
    assert_eq!(
        grace_window_ms(),
        DEFAULT_GRACE_WINDOW_MS,
        "the guard must restore the default window"
    );
}
