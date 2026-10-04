// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unified and safe installation of service termination signal handlers.
//!
//! A single staged C-ABI handler is installed for both `SIGINT` (Ctrl+C) and
//! `SIGTERM` (the default shutdown command of `systemd`/`systemctl stop`,
//! containers and orchestrators). The first signal flips the process-global
//! cooperative [`SHUTDOWN`](neural_amp_modeler_rs::common::spsc::SHUTDOWN)
//! flag so the main control loop in `pw_host::run` can stop the PipeWire loop,
//! drain the recording ring and finalize WAV headers.
//!
//! ## Grace window for near-simultaneous signals
//!
//! Shutdown orchestration frequently delivers **two** termination signals a
//! few milliseconds apart (e.g. `systemctl stop` racing an interactive Ctrl+C
//! in the same terminal). Signals arriving inside a small grace window after
//! the first delivery are therefore swallowed — they carry no new intent — so
//! exactly one graceful shutdown runs to completion and a recording in
//! progress always finalizes its WAV header. Only a signal arriving after the
//! grace window expired — meaning the graceful teardown is stuck — escalates
//! immediately to `_exit(1)`, preserving the operator's terminal-control
//! guarantee without sacrificing recording integrity.
//!
//! The handler itself touches only atomics, `clock_gettime(CLOCK_MONOTONIC)`
//! and `_exit` — every one of them async-signal-safe — and never allocates,
//! locks or logs. `SA_RESETHAND` is deliberately not used: resetting the
//! disposition to `SIG_DFL` would hand the kernel an unconditional hard kill on
//! the next delivery and bypass the grace window exactly in the burst case it
//! exists to protect.

use neural_amp_modeler_rs::common::spsc::SHUTDOWN;
use std::sync::atomic::Ordering;

/// Grace window opened by the first termination signal (in milliseconds).
///
/// A second `SIGINT`/`SIGTERM` arriving strictly inside this window is
/// swallowed; one arriving at or after the window boundary escalates to
/// [`libc::_exit`] (the graceful teardown had 500 ms — three CPU-time orders
/// of magnitude above a WAV header rewrite — and is evidently stuck).
const GRACE_WINDOW_MS: u64 = 500;

/// Handler stages packed into the high bits of one atomic word, with the
/// monotonic timestamp of the stage entry in the data bits.
///
/// Keeping stage and timestamp in a single word makes the stage transition
/// race-free without multi-variable ordering: one `compare_exchange` both
/// publishes the stage and freezes the grace-window anchor for every other
/// delivery.
const STAGE_IDLE: u64 = 0;
const STAGE_GRACE: u64 = 1 << 62;
const STAGE_FORCE_EXIT: u64 = 2 << 62;
/// Data bits holding the monotonic timestamp (bounded to the stage bits above;
/// `u64` milliseconds overflow only after ~292 million years of uptime).
const TIMESTAMP_MASK: u64 = (1u64 << 62) - 1;

/// Packed handler state: stage in the two high bits, monotonic
/// milliseconds of the stage transition in the data bits.
static TERMINATION_STATE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(STAGE_IDLE);

/// The staged, async-signal-safe termination handler for `SIGINT`/`SIGTERM`.
///
/// First signal: cooperatively store `SHUTDOWN = true` (Release) so the main
/// control loop observes the request and performs a graceful teardown, opening
/// the [`GRACE_WINDOW_MS`] grace window. Further signals inside the window are
/// swallowed (exactly one graceful shutdown runs, e.g. `systemctl stop`
/// racing an interactive Ctrl+C). A signal at or after the window boundary
/// means the graceful teardown is stuck: hard `_exit(1)` without running
/// destructors — the kernel reclaims all open resources.
extern "C" fn termination_signal_handler(_sig: libc::c_int) {
    let now = monotonic_ms();
    loop {
        let state = TERMINATION_STATE.load(Ordering::Relaxed);
        if state == STAGE_IDLE {
            // Publish the grace window anchor and the stage transition in one
            // atomic word: the successful exchange is the only writer of the
            // anchor, and both stores are single-variable anyway — one
            // `compare_exchange` ends every concurrent race.
            let opened = STAGE_GRACE | now.min(TIMESTAMP_MASK);
            match TERMINATION_STATE.compare_exchange_weak(
                STAGE_IDLE,
                opened,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Release pairs with the Acquire loads in `pw_host::run` and
                    // the panic hook.
                    SHUTDOWN.store(true, Ordering::Release);
                    return;
                }
                Err(_) => continue,
            }
        }
        // Grace window already open: decide by elapsed time.
        let opened_ms = state & TIMESTAMP_MASK;
        if now.saturating_sub(opened_ms) >= GRACE_WINDOW_MS {
            // Diagnostic marker for core/crash triage; `_exit` follows.
            TERMINATION_STATE.store(STAGE_FORCE_EXIT, Ordering::Relaxed);
            // SAFETY: `_exit` is async-signal-safe and terminates the process
            // immediately; it never runs destructors, so no locks, allocations
            // or logging are touched from the signal context.
            unsafe {
                libc::_exit(1);
            }
        }
        // Inside the grace window: swallow the repeated delivery — the
        // cooperative shutdown already carries this exact intent.
        return;
    }
}

/// Monotonic milliseconds for the grace-window arithmetic.
///
/// Reads `CLOCK_MONOTONIC` via `clock_gettime` (vDSO on Linux — no syscall, no
/// lock, no allocation), the textbook async-signal-safe timestamp source.
/// Saturating arithmetic; the value is clamped below the stage bits so it can
/// never leak into the stage field.
fn monotonic_ms() -> u64 {
    // SAFETY: `ts` is a valid, fully initialized output buffer for
    // `clock_gettime`; the monotonic clock read never fails on Linux targets.
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    let secs_ms = (ts.tv_sec as u64).saturating_mul(1_000);
    let ns_ms: u64 = (ts.tv_nsec as u64) / 1_000_000;
    secs_ms.saturating_add(ns_ms).min(TIMESTAMP_MASK)
}

/// Restores the handler state machine to `STAGE_IDLE` (test-only).
///
/// The handler state is process-global and outlives every test; each
/// handler-touching test resets it under [`crate::standalone::SHUTDOWN_TEST_LOCK`]
/// so the next test starts from a pristine idle stage.
#[cfg(test)]
pub(crate) fn reset_termination_stage_for_tests() {
    TERMINATION_STATE.store(STAGE_IDLE, Ordering::Release);
}

/// Saves the current disposition for `sig` (test-only; shared by the
/// process-global-state tests of `signals.rs` and `disk.rs`).
#[cfg(test)]
pub(crate) fn saved_sigaction(sig: libc::c_int) -> libc::sigaction {
    // SAFETY: all-zero bytes form a valid `sigaction` (SIG_DFL); the call only
    // reads the current disposition, never replaces it.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: null `act` makes this a read-only query; `current` is a valid
    // fully initialized output buffer.
    let ret = unsafe { libc::sigaction(sig, std::ptr::null(), &mut current) };
    assert_eq!(
        ret,
        0,
        "sigaction({sig}) query failed: {}",
        std::io::Error::last_os_error()
    );
    current
}

/// Restores a previously saved disposition for `sig` (test-only).
#[cfg(test)]
pub(crate) fn restore_sigaction(sig: libc::c_int, saved: libc::sigaction) {
    // SAFETY: `saved` was produced by a successful query and is a valid
    // disposition to reinstall; null `oldact` writes nothing back.
    let ret = unsafe { libc::sigaction(sig, &saved, std::ptr::null_mut()) };
    assert_eq!(
        ret,
        0,
        "restore of sigaction({sig}) failed: {}",
        std::io::Error::last_os_error()
    );
}

/// RAII restore of everything a handler-touching test touches: the packed
/// handler stage (reset to idle), the cooperative `SHUTDOWN` value and the
/// `SIGINT`/`SIGTERM` dispositions.
///
/// The signal surface is process-global and shared by every test in the
/// binary (serialized by [`crate::standalone::SHUTDOWN_TEST_LOCK`]); the guard
/// guarantees restore also through a panicking fail-closed assertion, so one
/// test's failure can never poison the sibling tests' invariants.
#[cfg(test)]
pub(crate) struct HandlerRestoreGuard {
    shutdown: bool,
    saved_int: libc::sigaction,
    saved_term: libc::sigaction,
}

#[cfg(test)]
impl HandlerRestoreGuard {
    /// Resets the stage to idle, then captures the current `SHUTDOWN` value
    /// and `SIGINT`/`SIGTERM` dispositions for restoration on drop.
    pub(crate) fn capture() -> Self {
        reset_termination_stage_for_tests();
        Self {
            shutdown: SHUTDOWN.load(Ordering::Acquire),
            saved_int: saved_sigaction(libc::SIGINT),
            saved_term: saved_sigaction(libc::SIGTERM),
        }
    }
}

#[cfg(test)]
impl Drop for HandlerRestoreGuard {
    fn drop(&mut self) {
        reset_termination_stage_for_tests();
        SHUTDOWN.store(self.shutdown, Ordering::Release);
        restore_sigaction(libc::SIGINT, self.saved_int);
        restore_sigaction(libc::SIGTERM, self.saved_term);
    }
}

/// Reads back the packed handler state (stage + timestamp; test-only).
#[cfg(test)]
pub(crate) fn termination_stage_snapshot_for_tests() -> u64 {
    TERMINATION_STATE.load(Ordering::Acquire)
}

/// Installs the unified [`termination_signal_handler`] for `SIGINT` and `SIGTERM`.
///
/// Every `libc::sigaction` call is formally validated: a non-zero return code
/// aborts initialization with an [`anyhow::Error`] carrying the last OS error,
/// never leaving the process with inconsistent signal dispositions.
///
/// # Errors
///
/// Returns an error if the kernel rejects the installation for either signal.
pub fn install_termination_signal_handlers() -> anyhow::Result<()> {
    // A zeroed `sigaction` is a fully-defined, valid disposition (SIG_DFL with
    // an empty mask); the handler pointer and SA_RESTART are set immediately
    // below, before any syscall, so no partially-configured state is exposed.
    // SAFETY: all-zero bytes form a valid `sigaction` on this target (SIG_DFL).
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    // `termination_signal_handler` has the 1-arg signature expected by the
    // kernel when SA_SIGINFO is not set; SA_RESTART alone triggers the 1-arg
    // handler path. The cast to `sighandler_t` (a pointer-sized integer) is an
    // ABI-compatible no-op conversion on this target.
    sa.sa_sigaction = termination_signal_handler as *const () as libc::sighandler_t;
    sa.sa_flags = libc::SA_RESTART;

    // SAFETY: `sa` is fully initialized before either call and `install_one`
    // never retains the pointer across the call.
    unsafe {
        install_one(libc::SIGINT, &sa)?;
        install_one(libc::SIGTERM, &sa)?;
    }
    log::info!("Installed SIGINT/SIGTERM termination handlers (SA_RESTART)");
    Ok(())
}

/// Installs `sa` as the disposition for `sig`, formally checking the syscall.
///
/// # Safety
///
/// `sa` must be fully initialized and valid for the target signal.
unsafe fn install_one(sig: libc::c_int, sa: &libc::sigaction) -> anyhow::Result<()> {
    // SAFETY: caller guarantees `sa` is fully initialized; a null `oldact`
    // means the kernel writes nothing back through this pointer.
    let ret = unsafe { libc::sigaction(sig, sa, std::ptr::null_mut()) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(anyhow::anyhow!(
            "sigaction({sig}) failed to install termination handler: {err}"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "signals_test.rs"]
mod tests;
