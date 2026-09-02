// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire playback stream configuration (`Stream/Output/Audio`) — reads from
//! `DspBridge` and delivers processed audio to the hardware.

use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::identity;
use crate::standalone::pw_host::output_pw::{
    SpaPodStorage, build_spa_format_pod, check_negotiated_rate_mismatch, mark_format_contract_ok,
    playback_dsp_cycle, reject_negotiated_format_violation, validate_audio_raw_format,
};
use crate::standalone::pw_host::rt_callback;
use crate::standalone::pw_host::{SharedBackendStatus, observe_stream_state};
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::pipeline::{BridgeRef, DspBridgeReader};

use pipewire as pw;
use pw::properties::properties;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Handles playback stream state changes — feeds the backend state machine
/// (F-RB-010 / T4.4).
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
    observe_stream_state("playback", old, new, backend);
}

/// Handles playback stream `param_changed` events (format negotiation).
///
/// Enforces the strict SPA format contract (G-RB-001 / T4.3) through the
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
/// a warning ("Sincronização de Sample Rate").
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
            rt_status
                .playback_negotiated_rate
                .store(rate, Ordering::Release);
            mark_format_contract_ok(rt_status, "playback");
            check_negotiated_rate_mismatch(rt_status);
            backend.notify_wakeup();
        }
        Err(violation) => {
            let violation_msg = violation.to_string();
            reject_negotiated_format_violation(rt_status, "playback", violation);
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
pub fn setup_playback_stream<'c>(
    core: &'c pw::core::Core,
    bridge_ptr: BridgeRef,
    buffer_size: u32,
    latency_str: &str,
    rt_status: Arc<RtStatusFlags>,
    backend_status: Arc<SharedBackendStatus>,
) -> anyhow::Result<(pw::stream::StreamBox<'c>, pw::stream::StreamListener<()>)> {
    let bridge_ptr_playback = unsafe { DspBridgeReader::new(bridge_ptr.as_ptr()) };
    let rt_status_playback = rt_status.clone();

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
        playback_props.insert("node.latency", latency_str);
    }

    let playback_stream =
        pw::stream::StreamBox::new(core, identity::PW_PLAYBACK_STREAM_NAME, playback_props)?;

    let mut last_bridge_gen: u64 = 0;
    let mut pb_frame_count: u32 = 0;

    let rt_status_for_params = rt_status_playback.clone();
    let backend_for_state = backend_status.clone();
    let backend_for_params = backend_status.clone();

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
            // F-RB-020 / T3.2: contain RT-callback panics here — they never
            // reach the `pipewire` crate's `extern "C"` trampoline (which would
            // `abort`); the fatal `RT_STATUS_PANIC_CAPTURED` latch drives the
            // ordered teardown from the main control loop.
            rt_callback::run_rt_callback_body(
                &rt_status_playback,
                std::panic::AssertUnwindSafe(|| {
                    if (pb_frame_count & 0x3F) == 0
                        && let Ok(pw_time) = stream.time()
                    {
                        rt_status_playback
                            .playback_host_now
                            .store(pw_time.now(), Ordering::Relaxed);
                        rt_status_playback
                            .playback_host_ticks
                            .store(pw_time.ticks(), Ordering::Relaxed);
                        rt_status_playback
                            .playback_host_delay
                            .store(pw_time.delay(), Ordering::Relaxed);
                    }
                    pb_frame_count = pb_frame_count.wrapping_add(1);

                    playback_dsp_cycle(
                        stream,
                        bridge_ptr_playback,
                        &mut last_bridge_gen,
                        &rt_status_playback,
                        pb_frame_count,
                    );
                }),
            );
        })
        .register()?;

    let mut playback_audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    playback_audio_info.set_format(pw::spa::param::audio::AudioFormat::F32P);
    playback_audio_info.set_channels(2);

    let mut playback_format_storage = SpaPodStorage::new();
    let playback_format_pod =
        unsafe { build_spa_format_pod(&playback_audio_info, &mut playback_format_storage)? };

    playback_stream.connect(
        pw::spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut [playback_format_pod],
    )?;

    log::info!(
        "{} Playback stream connected to PipeWire (Stream/Output/Audio, F32P Planar Stereo).",
        "🔊".bright_blue()
    );

    Ok((playback_stream, playback_listener))
}
