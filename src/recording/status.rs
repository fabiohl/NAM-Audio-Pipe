// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Observable worker state, startup handshake and failure propagation for the
//! WAV recording subsystem (F-RB-009 / T3.3).
//!
//! The `--record` disk worker used to be fired in background without any
//! synchronization: if the `io_uring` runtime failed to initialize, the output
//! directory was not writable, or the disk filled up, the worker only printed
//! `log::error!` and exited silently while the main thread kept starting
//! PipeWire with `--record`, discarding all audio into the ring. This module
//! provides:
//!
//! * [`RecordingStatus`] — the observable state machine of the worker
//!   (`Starting → Active | Failed`, then `Stopped` on graceful shutdown),
//!   shared with the main thread through an [`SharedRecordingStatus`] handle.
//! * [`RecordingInit`] — the startup payload handed to the worker: the status
//!   handle, a `tokio::sync::oneshot` handshake channel, an RT-observable
//!   atomic failure flag, the output directory and an injectable `io_uring`
//!   probe (so tests can force the unavailable-kernel verdict without a real
//!   kernel change).
//! * [`wait_for_recording_init`] — the main-thread side of the handshake:
//!   blocks (bounded by [`RECORDING_INIT_TIMEOUT`]) until the worker confirms
//!   it is ready or reports a startup failure.
//! * [`record_failure`] — the single choke point that transitions the status
//!   to `Failed` and raises the atomic failure flag the RT callback polls to
//!   suspend enqueueing without panics.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Maximum time the main thread waits for the recording worker's startup
/// handshake before aborting the process (fail-fast gate, F-RB-009 / T3.3).
pub const RECORDING_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Observable state machine of the disk-recording worker.
///
/// Written by the worker thread, polled by the main thread (never read on the
/// RT callback — the RT path observes only the atomic [`RecordingInit::failed_flag`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingStatus {
    /// The worker was spawned but has not completed the startup handshake yet.
    Starting,
    /// The worker initialized the `io_uring` runtime and confirmed the output
    /// directory; `path` is the directory, later updated to the actual capture
    /// file path once the first WAV is created.
    Active {
        /// Output directory (startup) or currently-open capture file path.
        path: PathBuf,
    },
    /// A fatal error occurred (startup or runtime: `EIO`, `ENOSPC`, missing
    /// directory, unavailable `io_uring`, ...). Recording is suspended.
    Failed {
        /// Human-readable reason for the failure.
        reason: String,
    },
    /// The worker drained everything and exited cleanly.
    Stopped,
}

/// Shared handle to the observable [`RecordingStatus`] (worker writes, main
/// polls). The mutex is never touched by the RT callback.
pub type SharedRecordingStatus = Arc<Mutex<RecordingStatus>>;

/// Failure of the main-thread side of the recording startup handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingStartupError {
    /// The worker reported an explicit startup failure (`io_uring` unavailable,
    /// output directory missing/not writable, disk full, ...).
    Failed {
        /// Human-readable reason reported by the worker.
        reason: String,
    },
    /// The worker did not complete the handshake within the given timeout.
    Timeout {
        /// The timeout that was exceeded.
        timeout: std::time::Duration,
    },
    /// The worker thread vanished without sending a verdict (the handshake
    /// sender was dropped — e.g. thread spawn failure or an early panic).
    WorkerGone,
}

impl std::fmt::Display for RecordingStartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordingStartupError::Failed { reason } => {
                write!(f, "recording worker failed to start: {reason}")
            }
            RecordingStartupError::Timeout { timeout } => {
                write!(
                    f,
                    "recording worker did not confirm readiness within {timeout:?}"
                )
            }
            RecordingStartupError::WorkerGone => {
                write!(
                    f,
                    "recording worker disappeared before the startup handshake completed"
                )
            }
        }
    }
}

impl std::error::Error for RecordingStartupError {}

/// Startup payload handed to the recording worker (and mirrored to the main
/// thread through the handshake receiver).
///
/// Everything the worker needs to run, plus the three synchronization handles
/// that turn the old "log and exit silently" failure into an observable,
/// fail-fast, RT-suspending one.
#[derive(Debug)]
pub struct RecordingInit {
    /// Observable worker state (worker writes, main polls).
    pub status: SharedRecordingStatus,
    /// Startup handshake: the worker sends `Ok(output_dir)` once the
    /// `io_uring` runtime is up and the output directory is confirmed
    /// writable, or `Err(anyhow)` to fail fast.
    pub handshake: tokio::sync::oneshot::Sender<anyhow::Result<PathBuf>>,
    /// RT-observable failure flag: set with `Release` on any fatal error so the
    /// audio callback suspends enqueueing of new blocks without panics.
    pub failed_flag: Arc<AtomicBool>,
    /// Output directory for capture files.
    pub base_dir: PathBuf,
    /// Optional `io_uring` probe override. Tests inject a fake verdict here to
    /// exercise the unavailable-kernel fail-fast path without a real kernel
    /// change; `None` uses the real [`crate::recording::probe::probe_io_uring`].
    pub io_uring_probe: Option<fn() -> crate::recording::probe::IoUringSupport>,
}

impl RecordingInit {
    /// Builds a startup payload for the recording worker.
    ///
    /// `handshake` is the sender half of a `tokio::sync::oneshot` channel whose
    /// receiver the main thread feeds to [`wait_for_recording_init`].
    pub fn new(
        status: SharedRecordingStatus,
        handshake: tokio::sync::oneshot::Sender<anyhow::Result<PathBuf>>,
        failed_flag: Arc<AtomicBool>,
        base_dir: PathBuf,
    ) -> Self {
        Self {
            status,
            handshake,
            failed_flag,
            base_dir,
            io_uring_probe: None,
        }
    }
}

/// Transitions the observable status to `Failed { reason }` and raises the
/// RT-observable failure flag (Release), suspending new enqueues.
///
/// Idempotent and lock-tolerant: if the status mutex is poisoned the flag is
/// still raised, so the RT path always observes the failure.
pub fn record_failure(status: &SharedRecordingStatus, failed_flag: &AtomicBool, reason: &str) {
    if let Ok(mut guard) = status.lock() {
        *guard = RecordingStatus::Failed {
            reason: reason.to_string(),
        };
    }
    failed_flag.store(true, Ordering::Release);
}

/// Main-thread side of the recording startup handshake (F-RB-009 / T3.3).
///
/// Blocks — bounded by `timeout` — until the worker either confirms readiness
/// (returning the output directory) or reports a startup failure. On timeout or
/// a vanished worker it returns the corresponding [`RecordingStartupError`];
/// the caller must abort **before** connecting any audio stream.
pub fn wait_for_recording_init(
    rx: tokio::sync::oneshot::Receiver<anyhow::Result<PathBuf>>,
    timeout: std::time::Duration,
) -> Result<PathBuf, RecordingStartupError> {
    let mut rx = rx;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(Ok(path)) => return Ok(path),
            Ok(Err(e)) => {
                return Err(RecordingStartupError::Failed {
                    reason: format!("{e:#}"),
                });
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return Err(RecordingStartupError::Timeout { timeout });
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                return Err(RecordingStartupError::WorkerGone);
            }
        }
    }
}

#[cfg(test)]
#[path = "status_test.rs"]
mod status_test;
