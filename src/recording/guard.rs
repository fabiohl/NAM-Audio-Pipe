// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! RAII custody of the `nam-recording-io` worker thread and its stop channel
//! (F-RB-009 / T3.5).
//!
//! The worker used to be handed to the PipeWire host as a bare
//! [`std::thread::JoinHandle`] whose join result was discarded
//! (`let _ = handle.join();`): a worker panic or `Err` returned by
//! [`crate::recording::disk::disk_writer_loop`] was silently swallowed and the
//! process still exited 0. Worse, an early `?` return inside
//! [`crate::standalone::pw_host::run_pipewire_host`] detached the handle and
//! left the I/O thread orphaned with an open WAV descriptor.
//!
//! [`RecordingWorkerGuard`] fixes both problems:
//!
//! * **RAII custody** — the guard owns the [`JoinHandle`] *and* the recording
//!   transport [`RecordingSender`] (the worker's stop channel: dropping it
//!   arms the "abandoned + drained" terminal condition, F-RB-009 / T3.4). On a
//!   premature drop — an error `?` return or a panic unwinding during host
//!   initialization — the guard pushes `StreamStop`, drops the sender and
//!   joins the worker with a bounded timeout, so no zombie thread or open file
//!   descriptor survives any exit path.
//! * **Observable join** — [`RecordingWorkerGuard::shutdown`] formally
//!   inspects the `JoinHandle` result (worker `Err`, panic payload, join
//!   timeout) and returns a [`RecordingWorkerOutcome`] that the caller
//!   propagates into the process exit code: recording failures can no longer
//!   masquerade as a successful run.

use crate::recording::transport::RecordingSender;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

/// Upper bound for waiting on the `nam-recording-io` thread during shutdown.
///
/// Shared by the explicit [`RecordingWorkerGuard::shutdown`] path and the
/// guard's `Drop` (premature-return cleanup). On timeout the outcome is
/// [`RecordingWorkerOutcome::TimedOut`] — never a silent success.
pub const RECORDING_IO_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bound for retrying `StreamStop` after the audio loop has already stopped.
pub const STREAM_STOP_RETRY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// Observable result of joining the `nam-recording-io` worker thread.
///
/// `Success` is the only variant that may be treated as a clean recording:
/// every failure variant must surface to the process exit code so scripts and
/// automation can tell a successful capture apart from a partial/failed one
/// (F-RB-009 / T3.5 rollback: never declare success, never discard the join
/// result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingWorkerOutcome {
    /// The worker drained every block, rewrote the WAV header with the final
    /// byte count and `fsync`ed before exiting cleanly.
    Success,
    /// The worker returned an explicit error — a fatal runtime failure in
    /// [`crate::recording::disk::disk_writer_loop`] (`EIO`, `ENOSPC`, failed
    /// header rewrite or `fsync`, ...).
    Failed {
        /// The error chain rendered as text.
        reason: String,
    },
    /// The worker thread panicked instead of returning.
    Panicked {
        /// The panic payload message.
        message: String,
    },
    /// The worker did not finish within the bounded join window. The thread is
    /// detached (it still terminates on its own once the producer is dropped),
    /// but the WAV header may be incomplete.
    TimedOut {
        /// The join timeout that was exceeded.
        timeout: std::time::Duration,
    },
}

impl std::fmt::Display for RecordingWorkerOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordingWorkerOutcome::Success => {
                write!(f, "recording completed successfully")
            }
            RecordingWorkerOutcome::Failed { reason } => {
                write!(f, "recording worker failed: {reason}")
            }
            RecordingWorkerOutcome::Panicked { message } => {
                write!(f, "recording worker panicked: {message}")
            }
            RecordingWorkerOutcome::TimedOut { timeout } => {
                write!(
                    f,
                    "recording worker did not finish within {timeout:?} \
                     (the WAV header may be incomplete)"
                )
            }
        }
    }
}

/// RAII custodian of the recording worker thread and its stop channel.
///
/// Owns, for the whole lifetime of the recording session:
///
/// * the [`JoinHandle`] of the `nam-recording-io` thread — so every exit path
///   (normal shutdown, early `?`, panic unwinding) formally joins it instead
///   of silently detaching; and
/// * the recording transport [`RecordingSender`] — the worker's stop channel.
///   Pushing [`StreamStop`](crate::recording::buffer::ControlPayload::StreamStop)
///   and then dropping the sender (which drops every producer half) arms the
///   worker's terminal "abandoned + drained" condition (F-RB-009 / T3.4), so a
///   premature drop terminates the worker in bounded time.
///
/// # Lifecycle
///
/// The guard is created by `main.rs` right after the startup handshake and
/// moved into [`crate::standalone::pw_host::run_pipewire_host`], which borrows
/// the sender slot (through a raw pointer for the RT callback) and calls
/// [`RecordingWorkerGuard::shutdown`] after the audio loop stopped. If the
/// host returns early via `?` — before the shutdown path runs — the guard is
/// dropped and `Drop` performs the same ordered teardown.
///
/// `Drop` cannot return the join result, so on a premature drop it logs a
/// warning when the teardown did not complete cleanly. The explicit
/// [`RecordingWorkerGuard::shutdown`] path returns the
/// [`RecordingWorkerOutcome`] for exit-code propagation.
pub struct RecordingWorkerGuard {
    /// Worker thread handle; `None` once the join was performed.
    handle: Option<JoinHandle<anyhow::Result<()>>>,
    /// Recording transport sender — the worker's stop channel. `Some` only
    /// under `--record`.
    sender: Option<RecordingSender>,
    /// RT-observable failure flag; `Some` only under `--record`. When raised,
    /// the worker already exited and the `StreamStop` push is skipped (the
    /// ring would never drain — F-RB-009 / T3.3).
    failed: Option<Arc<AtomicBool>>,
    /// Set once the ordered teardown ran, so a consumed guard never tears down
    /// twice (the `Drop` of a guard finished by [`RecordingWorkerGuard::shutdown`]
    /// must be a no-op).
    teardown_done: bool,
}

impl RecordingWorkerGuard {
    /// Wraps a freshly spawned recording worker.
    ///
    /// `sender` is the recording transport producer half (the stop channel) and
    /// `failed` the RT-observable failure flag — both `Some` only when the
    /// worker was actually spawned with `--record`.
    pub fn new(
        handle: JoinHandle<anyhow::Result<()>>,
        sender: Option<RecordingSender>,
        failed: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            handle: Some(handle),
            sender,
            failed,
            teardown_done: false,
        }
    }

    /// Stable `&mut` slot for the recording sender.
    ///
    /// Exposed to `run_pipewire_host` so the RT callback can push through a
    /// raw pointer (single-writer SPSC contract) while the guard keeps
    /// custody of the channel. The slot address is stable for the guard's
    /// lifetime — the guard must not be moved while the pointer is live. A
    /// guard created without a sender (recording disabled) lazily inserts a
    /// disabled [`RecordingSender::none`] so the caller can always deref the
    /// slot unconditionally.
    pub fn sender_slot(&mut self) -> &mut RecordingSender {
        self.sender.get_or_insert_with(RecordingSender::none)
    }

    /// RT-observable failure flag, for the RT callback path (clone before
    /// moving into the stream setup).
    pub fn failed_flag(&self) -> Option<&Arc<AtomicBool>> {
        self.failed.as_ref()
    }

    /// Ordered teardown of the recording worker (F-RB-009 / T3.4 + T3.5):
    ///
    /// 1. **`StreamStop`** — best-effort delivery (bounded retry) of the
    ///    terminal token through the transport's control channel. It is only
    ///    sent after `thread_loop.stop()` confirmed the RT loop stopped, so it
    ///    can never race a pending block. Skipped when the worker already
    ///    reported a fatal error (it has exited; the rings will never drain).
    /// 2. **Sender drop** — arms the worker's "abandoned **and** drained"
    ///    terminal condition, so finalization is guaranteed even if the token
    ///    push timed out on a full channel.
    /// 3. **Bounded join with formal result inspection** — the worker's
    ///    returned `Result<()>`, a panic payload or a join timeout become the
    ///    returned [`RecordingWorkerOutcome`].
    pub fn shutdown(mut self) -> RecordingWorkerOutcome {
        let outcome = teardown(&mut self.handle, &mut self.sender, self.failed.as_deref());
        self.teardown_done = true;
        outcome
    }
}

impl Drop for RecordingWorkerGuard {
    fn drop(&mut self) {
        if self.teardown_done {
            return;
        }
        // Premature drop: an error `?` return or a panic unwinding during host
        // initialization reached this frame before the explicit shutdown path
        // ran. Signal the worker (StreamStop → sender drop) and join with a
        // bounded timeout so no zombie thread or open WAV descriptor outlives
        // the guard (F-RB-009 / T3.5). The join result cannot be returned from
        // `Drop`; a non-clean teardown is logged for the diagnostics trace.
        let outcome = teardown(&mut self.handle, &mut self.sender, self.failed.as_deref());
        self.teardown_done = true;
        if !matches!(outcome, RecordingWorkerOutcome::Success) {
            log::warn!(
                "Recording worker teardown on premature drop did not complete \
                 cleanly: {outcome}"
            );
        }
    }
}

/// Runs the ordered recording teardown shared by the explicit shutdown and the
/// RAII drop path. See [`RecordingWorkerGuard::shutdown`] for the ordering
/// rationale.
fn teardown(
    handle: &mut Option<JoinHandle<anyhow::Result<()>>>,
    sender: &mut Option<RecordingSender>,
    failed: Option<&AtomicBool>,
) -> RecordingWorkerOutcome {
    let recording_failed_observed = failed.is_some_and(|f| f.load(Ordering::Acquire));
    if let Some(mut sender) = sender.take() {
        if !recording_failed_observed {
            push_stream_stop(&mut sender, STREAM_STOP_RETRY_TIMEOUT);
        }
        // Explicit drop: arms the worker's abandoned+drained terminal
        // condition so finalization happens even if the token never landed.
        drop(sender);
    }
    join_recording_io(handle, RECORDING_IO_JOIN_TIMEOUT)
}

/// Pushes `StreamStop` with a short retry. The audio callback is already
/// stopped, so the I/O thread is the only remaining consumer and should drain
/// capacity quickly. On timeout the token is dropped; the worker then
/// terminates through the sender-drop + drained-channels condition armed by
/// [`teardown`] (F-RB-009 / T3.4).
pub(crate) fn push_stream_stop(sender: &mut RecordingSender, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if sender.try_push_stream_stop() {
            return;
        }
        if !sender.has_producer() {
            // No channel exists (recording disabled / already taken) — nothing
            // to deliver.
            return;
        }
        if std::time::Instant::now() >= deadline {
            log::warn!(
                "StreamStop could not be delivered within {timeout:?}; \
                 finalization will be triggered by the producer drop."
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Bounded join for the `nam-recording-io` thread with formal result
/// inspection (F-RB-009 / T3.5).
///
/// Polls `is_finished()` and, once true, joins — which guarantees an immediate
/// return. The join result is never discarded:
///
/// * `Ok(Ok(()))` → [`RecordingWorkerOutcome::Success`];
/// * `Ok(Err(e))` → [`RecordingWorkerOutcome::Failed`] with the rendered error
///   chain;
/// * `Err(panic)` → [`RecordingWorkerOutcome::Panicked`] with the panic
///   message;
/// * deadline exceeded → [`RecordingWorkerOutcome::TimedOut`] with a detailed
///   diagnostic warning. The handle is detached — the worker still terminates
///   on its own because [`teardown`] already dropped the producer — but the
///   WAV header may be incomplete, so the outcome must never be reported as
///   success.
pub(crate) fn join_recording_io(
    handle: &mut Option<JoinHandle<anyhow::Result<()>>>,
    timeout: std::time::Duration,
) -> RecordingWorkerOutcome {
    let Some(handle) = handle.take() else {
        // Already joined (or no worker was spawned).
        return RecordingWorkerOutcome::Success;
    };
    let deadline = std::time::Instant::now() + timeout;
    while !handle.is_finished() {
        if std::time::Instant::now() >= deadline {
            // Rollback (F-RB-009): detailed diagnostic before detaching — the
            // process must never report a successful recording on a stall.
            log::warn!(
                "nam-recording-io did not finish within {timeout:?}; \
                 detaching — the WAV header may be incomplete and the process \
                 exit code will reflect the timeout (T3.5)."
            );
            return RecordingWorkerOutcome::TimedOut { timeout };
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    match handle.join() {
        Ok(Ok(())) => RecordingWorkerOutcome::Success,
        Ok(Err(e)) => RecordingWorkerOutcome::Failed {
            reason: format!("{e:#}"),
        },
        Err(panic) => RecordingWorkerOutcome::Panicked {
            message: panic_payload_message(&panic),
        },
    }
}

/// Extracts a human-readable message from a thread panic payload.
///
/// `panic!("literal")` may surface the payload as a `&str`, a `String`, or —
/// depending on the compiler — as a nested `Box<dyn Any + Send>`; the
/// recursive unwrap below covers all forms.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    let mut current = payload;
    loop {
        if let Some(s) = current.downcast_ref::<&str>() {
            return (*s).to_string();
        }
        if let Some(s) = current.downcast_ref::<String>() {
            return s.clone();
        }
        if let Some(inner) = current.downcast_ref::<Box<dyn std::any::Any + Send>>() {
            current = &**inner;
            continue;
        }
        return "non-string panic payload".to_string();
    }
}

#[cfg(test)]
#[path = "guard_test.rs"]
mod guard_test;
