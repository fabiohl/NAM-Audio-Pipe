// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire playback stream configuration (`Stream/Output/Audio`) — reads from
//! `DspBridge` and delivers processed audio to the hardware.

use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::identity;
use crate::standalone::pw_host::latency;
use crate::standalone::pw_host::output_pw::{
    SpaPodStorage, build_stereo_f32p_format_pod, check_negotiated_rate_mismatch,
    playback_dsp_cycle, reject_negotiated_format_violation, validate_audio_raw_format,
};
use crate::standalone::pw_host::rt_callback;
use crate::standalone::pw_host::{SharedBackendStatus, observe_stream_state};
use anyhow::Context;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::pipeline::{BridgeRef, DspBridgeReader};

use pipewire as pw;
use pw::properties::properties;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Records whether the current thread is the capture-marked RT data thread.
///
/// Called once per playback quantum: one TLS load plus (on divergence) one
/// `Relaxed` atomic increment. No syscall, no lock, no allocation. The
/// playback thread is intentionally not promoted to SCHED_FIFO here — that
/// would require revisiting the single-producer invariant of the log ring.
#[inline]
pub(crate) fn observe_playback_thread(stream_status: &super::StreamStatusFlags) {
    stream_status.note_playback_thread_divergence(
        crate::standalone::rt_setup::rt_is_current_thread_marked(),
    );
}

/// Handles playback stream state changes — feeds the backend state machine.
///
/// Mirrors the capture `state_changed_handler`: a fatal `StreamState::Error` or
/// a post-streaming `StreamState::Unconnected` (daemon crash/restart) marks the
/// backend `Failed` through the canonical [`observe_stream_state`] mapping, so
/// the main control loop tears the host down observably.
///
/// Note: this handler executes on the PipeWire `ThreadLoop` thread (cold
/// path), never on the RT data thread that runs `playback_dsp_cycle`.
pub fn playback_state_changed_handler(
    old: pw::stream::StreamState,
    new: pw::stream::StreamState,
    backend: &SharedBackendStatus,
) {
    // Re-arm the bring-up window on every fresh entry into
    // `Streaming` so bridge drops during the startup ramp are attributed to
    // the pre-roll ramp, not to clock drift.
    if matches!(new, pw::stream::StreamState::Streaming)
        && !matches!(old, pw::stream::StreamState::Streaming)
    {
        backend.stream_status().reset_playback_quantums();
    }
    observe_stream_state("playback", old, new, backend);
}

/// Handles playback stream `param_changed` events (format negotiation).
///
/// Enforces the strict SPA format contract through the
/// canonical [`validate_audio_raw_format`] gate: only `F32P` planar stereo is
/// accepted. A diverging renegotiation (mono, interleaved, S16, surround) is
/// rejected fail-closed — `RT_STATUS_HOST_CONTRACT_VIOLATION` is raised on
/// `rt_status`, the structured diagnostic is emitted and the backend is marked
/// `BackendState::Degraded` (audio muted), so the backend state machine
/// observes the degraded state. A subsequent valid renegotiation restores the
/// backend to `Running`.
///
/// On a valid renegotiation the negotiated rate is recorded and compared
/// against the capture stream's negotiated rate; a discrepancy is surfaced as
/// a warning ("Sample Rate Synchronization").
///
/// Note: this handler executes on the PipeWire `ThreadLoop` thread (cold
/// path), never on the RT data thread that runs `playback_dsp_cycle`.
pub fn playback_param_changed_handler(
    _stream: &pw::stream::Stream,
    _user_data: &mut (),
    id: u32,
    param: Option<&pw::spa::pod::Pod>,
    rt_status: &RtStatusFlags,
    backend: &SharedBackendStatus,
) {
    let Some(param) = param else { return };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }

    match validate_audio_raw_format(param) {
        Ok(rate) => {
            backend
                .stream_status()
                .playback_negotiated_rate
                .store(rate, Ordering::Release);
            backend.mark_format_ok("playback");
            check_negotiated_rate_mismatch(backend.stream_status());
            backend.notify_wakeup();
        }
        Err(violation) => {
            let violation_msg = violation.to_string();
            reject_negotiated_format_violation(
                rt_status,
                backend.stream_status(),
                "playback",
                violation,
            );
            backend.mark_degraded(format!(
                "SPA format contract violated on the playback stream: {violation_msg}"
            ));
        }
    }
}

/// Configures the playback stream and its RT listener.
///
/// The `process()` closure reads from `DspBridge` (filled by the capture stream)
/// and delivers to hardware via `playback_dsp_cycle`.
#[expect(clippy::too_many_arguments)]
pub fn setup_playback_stream<'c>(
    core: &'c pw::core::Core,
    bridge_ptr: BridgeRef,
    buffer_size: u32,
    initial_latency_samples: u32,
    latency_str: &str,
    rt_status: Arc<RtStatusFlags>,
    backend_status: Arc<SharedBackendStatus>,
    output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
) -> anyhow::Result<(pw::stream::StreamBox<'c>, pw::stream::StreamListener<()>)> {
    // SAFETY: the PipeWire playback callback is the sole SPSC consumer on `bridge_ptr`.
    let bridge_ptr_playback = unsafe { DspBridgeReader::from_ref(bridge_ptr) }
        .context("BridgeRef cannot be null when setting up playback stream")?;
    let rt_status_playback = rt_status;
    let stream_status_playback = backend_status.stream_status().clone();

    let mut playback_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::MEDIA_CLASS => "Stream/Output/Audio",
        *pw::keys::NODE_NAME => identity::PW_PLAYBACK_NODE_NAME,
        *pw::keys::NODE_DESCRIPTION => identity::PW_PLAYBACK_NODE_DESC,
        "audio.position" => "FL,FR",
        "node.group" => identity::PW_NODE_GROUP,
        "node.link-group" => identity::PW_LINK_GROUP,
    };

    if buffer_size > 0 {
        // PipeWire `node.latency` property contract:
        // Expressed as a rational fraction ("quantum/rate", e.g. "128/48000").
        // PipeWire treats this as a requested duration in seconds: tau = quantum / rate.
        // When connecting to a graph running at a different rate R, PipeWire dynamically
        // reschedules the quantum to round(tau * R). Thus "{buffer_size}/48000" requests
        // exactly `buffer_size` frames of latency at 48kHz, dynamically rescaled by PipeWire.
        playback_props.insert("node.latency", latency_str);
    }

    let playback_stream =
        pw::stream::StreamBox::new(core, identity::PW_PLAYBACK_STREAM_NAME, playback_props)?;

    let mut last_bridge_gen: u64 = 0;
    let mut pb_callback_count: u32 = 0;

    let rt_status_for_params = rt_status_playback.clone();
    let backend_for_state = backend_status.clone();
    let backend_for_params = backend_status;

    let playback_listener = playback_stream
        .add_local_listener::<()>()
        .state_changed(move |_stream, _user_data, old, new| {
            playback_state_changed_handler(old, new, &backend_for_state)
        })
        .param_changed(move |stream, user_data, id, param| {
            playback_param_changed_handler(
                stream,
                user_data,
                id,
                param,
                &rt_status_for_params,
                &backend_for_params,
            )
        })
        .process(move |stream: &pw::stream::Stream, _info| {
            // Contain RT-callback panics here — they never
            // reach the `pipewire` crate's `extern "C"` trampoline (which would
            // `abort`); the fatal `RT_STATUS_PANIC_CAPTURED` latch drives the
            // ordered teardown from the main control loop.
            rt_callback::run_rt_callback_body(
                &rt_status_playback,
                std::panic::AssertUnwindSafe(|| {
                    observe_playback_thread(&stream_status_playback);
                    if (pb_callback_count & 0x3F) == 0
                        && let Ok(pw_time) = stream.time()
                    {
                        stream_status_playback
                            .playback_host_now
                            .store(pw_time.now(), Ordering::Relaxed);
                        stream_status_playback
                            .playback_host_ticks
                            .store(pw_time.ticks(), Ordering::Relaxed);
                        stream_status_playback
                            .playback_host_delay
                            .store(pw_time.delay(), Ordering::Relaxed);
                    }
                    pb_callback_count = pb_callback_count.wrapping_add(1);
                    // Quantum counter for the bring-up window.
                    // Saturate at the window size (no wrap) so a long-running
                    // stream can never re-open the suppression window; the
                    // counter is reset to 0 by the state handler on entering
                    // `Streaming`.
                    stream_status_playback.bump_playback_quantums(
                        crate::standalone::pw_host::PLAYBACK_BRINGUP_QUANTUMS,
                    );

                    playback_dsp_cycle(
                        stream,
                        &bridge_ptr_playback,
                        &mut last_bridge_gen,
                        &rt_status_playback,
                        &stream_status_playback,
                        pb_callback_count,
                        output_limit,
                    );
                }),
            );
        })
        .register()?;

    let mut playback_format_storage = SpaPodStorage::new();
    // Canonical stereo F32P pod with explicit FL/FR positions, shared with the
    // capture stream: an anonymous stereo pod would let a strict PipeWire graph
    // echo [0, 0] positions and trip the host's own E2304 contract validator.
    let playback_format_pod = build_stereo_f32p_format_pod(&mut playback_format_storage)?;

    let mut proc_storage = SpaPodStorage::new();
    let mut lat_storage = SpaPodStorage::new();
    // SAFETY: `proc_storage` and `lat_storage` remain valid and unmodified for the duration
    // of the synchronous `connect` call below.
    let proc_pod =
        unsafe { latency::build_process_latency_pod(initial_latency_samples, &mut proc_storage)? };
    // SAFETY: `lat_storage` remains valid and unmodified for the duration of the
    // synchronous `connect` call below (same contract as `proc_storage`).
    let lat_pod = unsafe {
        latency::build_latency_pod(
            pw::spa::sys::SPA_DIRECTION_OUTPUT,
            initial_latency_samples,
            &mut lat_storage,
        )?
    };

    playback_stream.connect(
        pw::spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS
            | pw::stream::StreamFlags::TRIGGER,
        &mut [playback_format_pod, proc_pod, lat_pod],
    )?;

    log::info!(
        "{} Playback stream connected to PipeWire (Stream/Output/Audio, F32P Planar Stereo).",
        "🔊".bright_blue()
    );

    Ok((playback_stream, playback_listener))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::standalone::rt_setup::{rt_mark_current_thread, rt_unmark_current_thread};
    use std::sync::Arc;

    #[test]
    fn unmarked_playback_thread_counts_divergence() {
        let status = Arc::new(super::super::StreamStatusFlags::new());
        let worker_status = Arc::clone(&status);
        let handle = std::thread::spawn(move || {
            rt_unmark_current_thread();
            for _ in 0..5 {
                observe_playback_thread(&worker_status);
            }
        });
        handle.join().unwrap_or_else(|_| panic!("worker panicked"));
        assert_eq!(
            status.playback_thread_divergence.load(Ordering::Relaxed),
            5,
            "an unmarked playback thread must be counted on every quantum"
        );
    }

    #[test]
    fn playback_on_the_marked_rt_thread_is_not_divergent() {
        let status = Arc::new(super::super::StreamStatusFlags::new());
        let worker_status = Arc::clone(&status);
        let handle = std::thread::spawn(move || {
            rt_mark_current_thread();
            for _ in 0..5 {
                observe_playback_thread(&worker_status);
            }
            rt_unmark_current_thread();
        });
        handle.join().unwrap_or_else(|_| panic!("worker panicked"));
        assert_eq!(
            status.playback_thread_divergence.load(Ordering::Relaxed),
            0,
            "the marked capture data thread must not report divergence"
        );
    }
}
