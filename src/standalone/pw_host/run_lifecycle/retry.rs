// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Bounded reconnect retry handling and startup diagnostics.

use super::super::SharedBackendStatus;
use super::super::reconnect::ReconnectCycle;
use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode, SystemSnapshot};
use neural_amp_modeler_rs::common::spsc::SHUTDOWN;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Decision after handling a PipeWire `connect` or stream-setup error inside
/// the bounded-reconnect loop.
pub(crate) enum RetryDecision {
    Retry,
    BreakHost(Option<(&'static str, String)>),
}

/// Handles a startup or reconnect error uniformly for both `connect` and
/// `stream-setup`, preserving fail-fast on the initial connection and bounded
/// backoff on subsequent attempts.
pub(crate) fn handle_attempt_error<E: std::fmt::Display>(
    err: E,
    stage: &'static str,
    stream_label: &'static str,
    sys: &SystemSnapshot,
    reconnect: &mut ReconnectCycle,
    backend_status: &SharedBackendStatus,
) -> Result<RetryDecision, E> {
    if reconnect.attempts_made() == 0 {
        emit_startup_backend_diagnostic(sys, stage, &err);
        return Err(err);
    }
    match reconnect.begin_attempt() {
        Some(backoff) => {
            log::warn!(
                "{} {} failed during reconnect attempt {}/{} ({err}); retrying in {:?}.",
                "🔁".yellow(),
                if stage == "connect" {
                    "PipeWire daemon unreachable"
                } else {
                    "Stream setup"
                },
                reconnect.attempts_made(),
                reconnect.policy().max_attempts,
                backoff,
            );
            backend_status.begin_reconnect(
                reconnect.attempts_made(),
                reconnect.policy().max_attempts,
                backoff,
            );
            if sleep_interruptible(backoff) {
                Ok(RetryDecision::BreakHost(None))
            } else {
                Ok(RetryDecision::Retry)
            }
        }
        None => Ok(RetryDecision::BreakHost(Some((
            stream_label,
            format!("{stage} failed: {err}"),
        )))),
    }
}

/// Emits the first-connection backend diagnostic with a stage-specific hint.
pub(crate) fn emit_startup_backend_diagnostic(
    sys: &SystemSnapshot,
    stage: &'static str,
    error: &dyn std::fmt::Display,
) {
    let hint = match stage {
        "connect" => crate::standalone::exit::BACKEND_UNREACHABLE_HINT,
        _ => {
            "PipeWire is running but the audio streams could not be created: \
             verify the graph with `pw-cli info 0` and that a capture/playback \
             device is available."
        }
    };
    NamDiagnostic::new(NamErrorCode::BackendFailure, sys)
        .message(format!(
            "PipeWire first connection failed ({stage}): {error}"
        ))
        .hint(hint)
        .param("phase", "startup")
        .param("stage", stage)
        .emit();
}

/// Returns the pending backend failure that must survive a shutdown signal
/// observed while sleeping in a reconnect backoff.
///
/// Without this, an interrupted retry would leave `backend_failure` empty and
/// the process would report a clean exit even though the backend had already
/// failed.
pub(crate) fn interrupted_backoff_failure(
    stream: &'static str,
    reason: String,
) -> (&'static str, String) {
    (stream, reason)
}

/// Sleeps for `duration` in small slices, bailing out early when `SHUTDOWN` is raised.
pub(crate) fn sleep_interruptible(duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
    }
}
