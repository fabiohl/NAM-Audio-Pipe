// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unified and safe installation of service termination signal handlers.
//!
//! A single staged C-ABI handler is installed for `SIGINT` (Ctrl+C), `SIGTERM`
//! (the default shutdown command of `systemd`/`systemctl stop`, containers and
//! orchestrators), `SIGHUP` (the terminal is closed, e.g. a `Terminal=true`
//! launcher window is dismissed — the default action would otherwise kill the
//! process with no graceful teardown and no WAV finalization) and `SIGQUIT`.
//! The first signal flips the process-global cooperative
//! [`SHUTDOWN`] flag so the main
//! control loop in `pw_host::run` can stop the PipeWire loop, drain the
//! recording ring and finalize WAV headers.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Default grace window opened by the first termination signal (milliseconds).
///
/// A second `SIGINT`/`SIGTERM` arriving strictly inside the window is swallowed;
/// one arriving at or after the boundary escalates to [`libc::_exit`]. The
/// default (500 ms) is three CPU-time orders of magnitude above a WAV header
/// rewrite, so it is ample when no recording is in flight.
pub const DEFAULT_GRACE_WINDOW_MS: u64 = 500;

/// Extended grace window used while a WAV recording is active (milliseconds).
///
/// The graceful teardown of an active recording includes draining the SPSC ring,
/// rewriting the header and `fsync`ing the capture file, which on HDD/USB/NFS
/// can exceed 500 ms (F-UX-04). With this window a second Ctrl+C arriving while
/// the file is finalizing is swallowed instead of force-exiting mid-`fsync` and
/// corrupting the take. The window is restored to
/// [`DEFAULT_GRACE_WINDOW_MS`] as soon as the recording worker's shutdown
/// completes.
pub const RECORDING_GRACE_WINDOW_MS: u64 = 5000;

/// Live grace window (milliseconds).
///
/// Read by the async-signal-safe handler with `Relaxed` and written by the main
/// thread: `RECORDING_GRACE_WINDOW_MS` is armed once the recording handshake
/// succeeds (`main`) and disarmed when the worker's shutdown completes
/// (`recording::guard`). Only an atomic is touched — no lock, no allocation —
/// so the handler stays async-signal-safe.
static GRACE_WINDOW_MS: AtomicU64 = AtomicU64::new(DEFAULT_GRACE_WINDOW_MS);

/// Sets the live grace window (milliseconds). Clamped to at least `1` so a
/// misconfigured `0` can never make the window instantaneous.
pub fn set_grace_window(ms: u64) {
    GRACE_WINDOW_MS.store(ms.max(1), Ordering::Relaxed);
}

/// Returns the current live grace window (milliseconds).
pub fn grace_window_ms() -> u64 {
    GRACE_WINDOW_MS.load(Ordering::Relaxed)
}

/// Whether the recording grace window is currently armed.
///
/// [`disarm_recording_grace_window`] only restores the default window when a
/// recording actually armed it, so a recording-less teardown (e.g. a bare
/// `RecordingWorkerGuard` in a unit test) can never clobber a window another
/// context is deliberately holding.
static RECORDING_WINDOW_ARMED: AtomicBool = AtomicBool::new(false);

/// Widens the grace window to [`RECORDING_GRACE_WINDOW_MS`] and marks it armed.
///
/// Called by `main` once the recording handshake succeeds.
pub fn arm_recording_grace_window() {
    GRACE_WINDOW_MS.store(RECORDING_GRACE_WINDOW_MS, Ordering::Relaxed);
    RECORDING_WINDOW_ARMED.store(true, Ordering::Relaxed);
}

/// Restores [`DEFAULT_GRACE_WINDOW_MS`] — but only if a recording armed the
/// window, so it is a no-op when no recording grace window was taken.
///
/// Called from the recording worker's teardown once the capture is finalized.
pub fn disarm_recording_grace_window() {
    if RECORDING_WINDOW_ARMED.swap(false, Ordering::Relaxed) {
        GRACE_WINDOW_MS.store(DEFAULT_GRACE_WINDOW_MS, Ordering::Relaxed);
    }
}

/// Static, async-signal-safe message written to `stderr` on the forced
/// `_exit(1)` path when the graceful teardown (possibly finalizing a capture)
/// is abandoned. A literal byte slice — no formatting, no allocation.
const FORCED_EXIT_MESSAGE: &[u8] = b"forced exit: capture may be incomplete\n";

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

/// The staged, async-signal-safe termination handler for
/// `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT`.
///
/// First signal: cooperatively store `SHUTDOWN = true` (Release) so the main
/// control loop observes the request and performs a graceful teardown, opening
/// the live [`grace_window_ms`] grace window. Further signals inside the window
/// are swallowed (exactly one graceful shutdown runs, e.g. `systemctl stop`
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
        // Grace window already open: decide by elapsed time. The window length
        // is read from the live atomic (Relaxed — async-signal-safe): it is
        // widened while a recording finalizes and restored afterwards.
        let opened_ms = state & TIMESTAMP_MASK;
        if now.saturating_sub(opened_ms) >= GRACE_WINDOW_MS.load(Ordering::Relaxed) {
            // Diagnostic marker for core/crash triage; `_exit` follows.
            TERMINATION_STATE.store(STAGE_FORCE_EXIT, Ordering::Relaxed);
            // Async-signal-safe best effort: a raw static literal on stderr so the
            // operator learns the teardown (and possibly a capture) was abandoned.
            // SAFETY: `write(2, ...)` is async-signal-safe; the pointer/length
            // describe a `'static` byte slice that outlives the process.
            unsafe {
                libc::write(
                    2,
                    FORCED_EXIT_MESSAGE.as_ptr().cast::<libc::c_void>(),
                    FORCED_EXIT_MESSAGE.len(),
                );
            }
            // SAFETY: `_exit` is async-signal-safe and terminates the process
            // immediately; it never runs destructors, so no locks, allocations
            // or logging are touched from the signal context. The generic code
            // is a compile-time constant (no runtime work in the handler).
            unsafe {
                libc::_exit(crate::standalone::exit::EXIT_GENERIC);
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
    // SAFETY: `ts` is a valid, writable `libc::timespec` and `CLOCK_MONOTONIC`
    // always exists on the Linux targets of this daemon; the return value is
    // intentionally ignored (the monotonic read never fails).
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
/// `SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT` dispositions.
///
/// The signal surface is process-global and shared by every test in the
/// binary (serialized by [`crate::standalone::SHUTDOWN_TEST_LOCK`]); the guard
/// guarantees restore also through a panicking fail-closed assertion, so one
/// test's failure can never poison the sibling tests' invariants.
#[cfg(test)]
pub(crate) struct HandlerRestoreGuard {
    shutdown: bool,
    grace_window_ms: u64,
    recording_window_armed: bool,
    saved: [(libc::c_int, libc::sigaction); 4],
}

#[cfg(test)]
impl HandlerRestoreGuard {
    /// Resets the stage to idle, then captures the current `SHUTDOWN` value,
    /// the live grace window, the armed flag and the termination-signal
    /// dispositions for restoration on drop.
    pub(crate) fn capture() -> Self {
        reset_termination_stage_for_tests();
        let sigs = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];
        Self {
            shutdown: SHUTDOWN.load(Ordering::Acquire),
            grace_window_ms: grace_window_ms(),
            recording_window_armed: RECORDING_WINDOW_ARMED.load(Ordering::Relaxed),
            saved: sigs.map(|sig| (sig, saved_sigaction(sig))),
        }
    }
}

#[cfg(test)]
impl Drop for HandlerRestoreGuard {
    fn drop(&mut self) {
        reset_termination_stage_for_tests();
        SHUTDOWN.store(self.shutdown, Ordering::Release);
        set_grace_window(self.grace_window_ms);
        RECORDING_WINDOW_ARMED.store(self.recording_window_armed, Ordering::Relaxed);
        for (sig, saved) in self.saved {
            restore_sigaction(sig, saved);
        }
    }
}

/// Reads back the packed handler state (stage + timestamp; test-only).
#[cfg(test)]
pub(crate) fn termination_stage_snapshot_for_tests() -> u64 {
    TERMINATION_STATE.load(Ordering::Acquire)
}

/// Installs the unified `termination_signal_handler` for `SIGINT`, `SIGTERM`,
/// `SIGHUP` and `SIGQUIT`.
///
/// Every `libc::sigaction` call is formally validated: a non-zero return code
/// aborts initialization with an [`anyhow::Error`] carrying the last OS error,
/// never leaving the process with inconsistent signal dispositions.
///
/// # Errors
///
/// Returns an error if the kernel rejects the installation for any signal.
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
        install_one(libc::SIGHUP, &sa)?;
        install_one(libc::SIGQUIT, &sa)?;
    }
    log::info!("Installed SIGINT/SIGTERM/SIGHUP/SIGQUIT termination handlers (SA_RESTART)");
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
