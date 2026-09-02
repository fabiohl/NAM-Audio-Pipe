// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::common::spsc::SHUTDOWN;
use std::sync::atomic::Ordering;

/// Reads back the current `sigaction` disposition for `sig`.
fn current_sigaction(sig: libc::c_int) -> libc::sigaction {
    // SAFETY: all-zero bytes form a valid `sigaction` on this target (SIG_DFL);
    // it is used only as an output buffer for the query below.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: `current` is a fully initialized (zeroed) output buffer and a
    // null `act` argument means we only read, never replace, the disposition.
    let ret = unsafe { libc::sigaction(sig, std::ptr::null(), &mut current) };
    assert_eq!(
        ret,
        0,
        "sigaction({sig}) query failed: {}",
        std::io::Error::last_os_error()
    );
    current
}

/// Restores a previously saved disposition for `sig`.
fn restore_sigaction(sig: libc::c_int, saved: libc::sigaction) {
    // SAFETY: `saved` was produced by a successful `sigaction` query, so it is
    // a valid disposition to reinstall; a null `oldact` writes nothing back.
    let ret = unsafe { libc::sigaction(sig, &saved, std::ptr::null_mut()) };
    assert_eq!(
        ret,
        0,
        "restore of sigaction({sig}) failed: {}",
        std::io::Error::last_os_error()
    );
}

/// RAII guard restoring the previous `SHUTDOWN` value on drop (same pattern as
/// the recording lifecycle tests) so a successful run leaves the flag pristine.
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
fn test_install_registers_sigint_and_sigterm_with_sa_restart() {
    let saved_int = current_sigaction(libc::SIGINT);
    let saved_term = current_sigaction(libc::SIGTERM);

    install_termination_signal_handlers().expect("termination handler installation must succeed");

    let expected = termination_signal_handler as *const () as libc::sighandler_t;
    for sig in [libc::SIGINT, libc::SIGTERM] {
        let current = current_sigaction(sig);
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
    let restore = ShutdownRestore::capture();
    assert!(
        !SHUTDOWN.load(Ordering::Acquire),
        "test assumes the process-global SHUTDOWN starts unset"
    );

    termination_signal_handler(libc::SIGINT);

    assert!(
        SHUTDOWN.load(Ordering::Acquire),
        "the first termination signal must cooperatively flip SHUTDOWN"
    );

    drop(restore);
    assert!(
        !SHUTDOWN.load(Ordering::Acquire),
        "guard must restore the previous SHUTDOWN value"
    );
}
