// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Thread marking of the PipeWire RT data-loop thread, consultable at O(1).
//!
//! The PipeWire log handler (`pw_host::log_redirect`) intercepts SPA log
//! events raised by PipeWire internals. Those events can fire on the RT data
//! thread, and dispatching them through `log::*` there would drag blocking
//! locks and horizon allocations into the audio callback (F-APRT-01). The
//! handler therefore needs a cheap, lock-free answer to "am I running on the
//! RT callback thread?".
//!
//! That answer is thread-local state: each thread owns its own mark, and the
//! consumer-owned hook on the data thread is the first `process()` quantum,
//! where `thread::configure_realtime_thread_with` applies the real-time setup
//! (DAZ/FTZ, affinity, scheduler promotion) and arms the mark right after the
//! scheduler step. Arming is deliberately tied to that hook regardless of the
//! promotion outcome: the honest-policy semantics record FIFO, RR or even a
//! fallback `SCHED_OTHER` without changing *which* thread runs the callback —
//! and it is exactly that thread whose log events must be offloaded.
//!
//! The mark dies with its thread: when the bounded-reconnect cycle stops one
//! `pw_thread_loop` (joining its data thread) and the next instance spawns a
//! fresh one, the new data thread starts unmarked and re-arms in its own
//! first quantum. There is never more than one live marked thread per host
//! instance, which is the single-producer premise the off-RT log ring relies
//! on.
//!
//! # Consultation contract
//!
//! `rt_is_current_thread_marked` is one TLS load plus a branch: no lock, no
//! allocation, no syscall — valid on the RT thread at any time. The TLS cell
//! is const-initialized (`Cell<bool>` has no `Drop`), so access compiles to a
//! direct thread-storage read without lazy-init machinery.

use std::cell::Cell;

thread_local! {
    /// Per-thread mark: `true` only on the thread that runs the RT audio
    /// callback. Const-initialized so access needs no lazy-init check.
    static RT_MARK: Cell<bool> = const { Cell::new(false) };
}

/// Marks the calling thread as the RT callback thread. Idempotent.
///
/// Returns the new mark state of the thread (always `true`). Arming a thread
/// that was already marked re-asserts the mark instead of signalling an
/// error: the configuration hook runs only once per data thread in
/// production, and test-harness reuse of the same thread re-arms harmlessly.
#[inline]
pub fn rt_mark_current_thread() -> bool {
    RT_MARK.with(|mark| mark.set(true));
    true
}

/// Clears the calling thread's RT mark. Idempotent.
///
/// Returns the new mark state (`false`). Production never needs this — the
/// mark dies together with the data thread — but tests and defensive
/// teardown paths use it to leave no residue on shared threads.
#[inline]
pub fn rt_unmark_current_thread() -> bool {
    RT_MARK.with(|mark| mark.set(false));
    false
}

/// O(1) consultation used by the PipeWire log handler.
///
/// `true` only on a thread previously marked by [`rt_mark_current_thread`].
/// Guaranteed free of locks, allocations and syscalls, so it can be evaluated
/// for every intercepted log event on the RT thread.
#[inline]
pub fn rt_is_current_thread_marked() -> bool {
    RT_MARK.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_unmark_round_trip_is_idempotent() {
        // The runner thread starts unmarked; clear defensively so the test
        // starts from a known state even if reused across test binaries'
        // re-runs on the same OS thread.
        rt_unmark_current_thread();
        assert!(!rt_is_current_thread_marked());

        assert!(rt_mark_current_thread());
        assert!(rt_is_current_thread_marked());
        // Re-arming is a re-assertion, not an error state.
        assert!(rt_mark_current_thread());
        assert!(rt_is_current_thread_marked());

        assert!(!rt_unmark_current_thread());
        assert!(!rt_is_current_thread_marked());
        // Clearing an already-clear mark stays a no-op.
        assert!(!rt_unmark_current_thread());
    }

    #[test]
    fn unmarked_thread_reports_false_and_marks_are_thread_local() {
        // Fresh threads are born unmarked: the consult that the log handler
        // would use answers `false`, which keeps such a thread on the
        // synchronous `log::*` path (the pre-marking behaviour).
        let handle = std::thread::spawn(|| {
            assert!(!rt_is_current_thread_marked());
            rt_mark_current_thread();
            assert!(rt_is_current_thread_marked());
        });
        handle.join().unwrap_or_else(|_| panic!("worker panicked"));

        // TLS isolation: another thread's mark never leaks here.
        assert!(!rt_is_current_thread_marked());
    }
}
