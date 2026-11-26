// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire stream-state and RT-panic observers.

use super::machine::SharedBackendStatus;
use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::spsc::{RtStatusFlags, SHUTDOWN};
use pipewire as pw;
use std::sync::atomic::Ordering;

/// Maps a PipeWire stream-state transition to the backend state machine.
pub fn observe_stream_state(
    stream: &'static str,
    old: pw::stream::StreamState,
    new: pw::stream::StreamState,
    backend: &SharedBackendStatus,
) {
    match new {
        pw::stream::StreamState::Error(err) => {
            log::error!(
                "{} Critical PipeWire {stream} stream failure: {err}",
                "💥".red(),
            );
            backend.set_stream_active(stream, false);
            backend.mark_failed(stream, err);
        }
        pw::stream::StreamState::Unconnected if was_connected(&old) => {
            backend.set_stream_active(stream, false);
            // Exception: a post-streaming disconnect while the process is
            // shutting down cooperatively (SIGINT/SIGTERM raised `SHUTDOWN`) is
            // the streams being torn down by `thread_loop.stop()` — expected,
            // so it is logged below `ERROR` and the sticky `Failed` transition
            // is skipped.
            //
            // If teardown/reconnect is in progress
            // (e.g. `pw_stream_destroy()` destroying streams while reconnecting),
            // this synchronous `Unconnected` event is the direct consequence of
            // host teardown and must not poison the upcoming reconnection attempt
            // with a premature `mark_failed`.
            if SHUTDOWN.load(Ordering::Acquire) {
                log::info!(
                    "{} PipeWire {stream} stream disconnected cooperatively during shutdown.",
                    "🔌".yellow(),
                );
            } else if backend.is_teardown_in_progress() {
                log::debug!(
                    "PipeWire {stream} stream disconnected during host teardown/reconnect (ignored)."
                );
            } else {
                log::error!(
                    "{} PipeWire {stream} stream disconnected from the audio backend \
                     (daemon restart or crash) — bounded reconnect or fail-fast teardown follows.",
                    "🔌".red(),
                );
                backend.mark_failed(stream, "stream disconnected from the audio backend");
            }
        }
        pw::stream::StreamState::Paused => {
            backend.set_stream_active(stream, false);
            if old == pw::stream::StreamState::Streaming {
                log::info!(
                    "{} Audio disconnected or node switch ({stream} stream).",
                    "⏸️".yellow(),
                );
            }
        }
        pw::stream::StreamState::Streaming => {
            backend.set_stream_active(stream, true);
            if old == pw::stream::StreamState::Paused {
                let action = match stream {
                    "playback" => "Playback active",
                    "capture" => "Audio captured",
                    _ => "Audio stream active",
                };
                log::info!(
                    "{} {} ({stream} connection established).",
                    "▶️".green(),
                    action,
                );
            }
        }
        _ => {}
    }
}

/// Observes the RT panic-captured latch and transitions the
/// backend to [`crate::standalone::pw_host::status::BackendState::Failed`] when a panic was contained inside an RT
/// callback closure.
///
/// Called by the main control loop on every poll iteration (< 100 ms): a
/// contained panic must never become an `abort` — the backend machine drives
/// the ordered teardown (thread-loop stop, GC drain,
/// `RecordingWorkerGuard::shutdown` with the WAV finalized) exactly like any
/// other fatal backend failure.
///
/// Returns `true` when the panic flag was observed (the caller breaks the
/// control loop into the teardown path).
pub fn observe_rt_panic(rt_status: &RtStatusFlags, backend: &SharedBackendStatus) -> bool {
    if rt_status.check_flag(super::super::rt_callback::RT_STATUS_PANIC_CAPTURED) {
        log::error!(
            "{} Panic captured inside an RT callback closure — contained, \
             ordered teardown follows (no abort, capture will be finalized).",
            "🔥".red(),
        );
        backend.mark_failed(
            "rt_callback",
            "panic captured in an RT callback closure (contained — no abort, ordered teardown follows)",
        );
        true
    } else {
        false
    }
}

/// Whether `old` indicates the stream previously held an established
/// connection (`Paused` or `Streaming`), i.e. `Unconnected` now means a real
/// disconnect rather than the initial not-yet-connected state.
fn was_connected(old: &pw::stream::StreamState) -> bool {
    matches!(
        old,
        pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming
    )
}
