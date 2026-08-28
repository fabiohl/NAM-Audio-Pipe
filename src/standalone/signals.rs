// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unified and safe installation of service termination signal handlers.
//!
//! A single C-ABI handler is installed for both `SIGINT` (Ctrl+C) and `SIGTERM`
//! (the default shutdown command of `systemd`/`systemctl stop`, containers and
//! orchestrators). The first signal flips the process-global cooperative
//! [`SHUTDOWN`](neural_amp_modeler_rs::common::spsc::SHUTDOWN) flag so the main
//! control loop in `pw_host::run` can stop the PipeWire loop, drain the
//! recording ring and finalize WAV headers; a second signal forces an immediate
//! `_exit(1)` so the operator always regains the terminal.

use neural_amp_modeler_rs::common::spsc::SHUTDOWN;
use std::sync::atomic::Ordering;

/// The unified, async-signal-safe termination handler for `SIGINT`/`SIGTERM`.
///
/// First signal: cooperatively store `SHUTDOWN = true` (Release) so the main
/// control loop observes the request and performs a graceful teardown. Second
/// signal (arriving while a shutdown is blocked or stuck): hard `_exit(1)`
/// without running destructors — the kernel reclaims all open resources.
extern "C" fn termination_signal_handler(_sig: libc::c_int) {
    if SHUTDOWN.load(Ordering::Acquire) {
        // SAFETY: `_exit` is async-signal-safe and terminates the process
        // immediately; it never runs destructors, so no locks, allocations or
        // logging are touched from the signal context.
        unsafe {
            libc::_exit(1);
        }
    }
    // Release pairs with the Acquire loads in `pw_host::run` and the panic hook.
    SHUTDOWN.store(true, Ordering::Release);
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
