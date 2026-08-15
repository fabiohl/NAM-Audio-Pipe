// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire playback stream configuration (`Stream/Output/Audio`) — reads from
//! `DspBridge` and delivers processed audio to the hardware.

use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::identity;
use crate::standalone::pw_host::output_pw::{build_spa_format_pod, playback_dsp_cycle};
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::pipeline::{BridgeRef, DspBridgeReader};

use pipewire as pw;
use pw::properties::properties;
use std::sync::Arc;
use std::sync::atomic::Ordering;

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

    let playback_listener = playback_stream
        .add_local_listener::<()>()
        .process(move |stream: &pw::stream::Stream, _info| {
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
            );
        })
        .register()?;

    let mut playback_audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    playback_audio_info.set_format(pw::spa::param::audio::AudioFormat::F32P);
    playback_audio_info.set_channels(2);

    let mut playback_format_buf = [0u8; 1024];
    let playback_format_pod =
        unsafe { build_spa_format_pod(&playback_audio_info, &mut playback_format_buf)? };

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
