// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Capture stream format/latency pod construction and connection.

use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::output_pw::{SpaPodStorage, build_stereo_f32p_format_pod};

use pipewire as pw;

pub(super) fn connect_capture_stream(
    capture_stream: &mut pw::stream::StreamBox<'_>,
    initial_latency_samples: u32,
) -> anyhow::Result<()> {
    let mut format_storage = SpaPodStorage::new();
    let format_pod = build_stereo_f32p_format_pod(&mut format_storage)?;

    let mut proc_storage = SpaPodStorage::new();
    let mut lat_storage = SpaPodStorage::new();
    // SAFETY: proc_storage and lat_storage remain valid and unmodified for the duration
    // of the synchronous `connect` call below.
    let proc_pod = unsafe {
        crate::standalone::pw_host::latency::build_process_latency_pod(
            initial_latency_samples,
            &mut proc_storage,
        )?
    };
    // SAFETY: `lat_storage` remains valid and unmodified for the duration of the
    // synchronous `connect` call below (same contract as `proc_storage`).
    let lat_pod = unsafe {
        crate::standalone::pw_host::latency::build_latency_pod(
            pw::spa::sys::SPA_DIRECTION_INPUT,
            initial_latency_samples,
            &mut lat_storage,
        )?
    };

    capture_stream.connect(
        pw::spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS
            | pw::stream::StreamFlags::EXCLUSIVE,
        &mut [format_pod, proc_pod, lat_pod],
    )?;

    log::info!(
        "{} Capture stream connected to PipeWire (Audio/Sink, F32P Planar Stereo).",
        "\u{1f3bc}".bright_blue()
    );

    Ok(())
}
