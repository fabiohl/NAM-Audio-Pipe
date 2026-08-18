// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Free functions extracted from the capture stream listener closures.
//!
//! These functions are called from thin wrapper closures in `setup_capture_stream`,
//! reducing the inline closure size while preserving the same capture semantics.

use crate::standalone::colors::Colorize;
use pipewire as pw;
use std::sync::atomic::{AtomicU32, Ordering};

/// Handles PipeWire stream state changes.
///
/// Note (Sprint C-01 / H-06): This handler executes on the PipeWire `ThreadLoop` thread,
/// NOT on the RT data processing thread that runs `process()`. Real-time thread setup
/// (DAZ/FTZ MXCSR, SCHED_FIFO, CPU affinity) must remain in the cold-path of `process()`.
pub fn state_changed_handler(
    _stream: &pw::stream::Stream,
    _user_data: &mut (),
    old: pw::stream::StreamState,
    new: pw::stream::StreamState,
) {
    match new {
        pw::stream::StreamState::Error(err) => {
            log::error!("{} Critical PW audio stream failure: {}", "💥".red(), err);
        }
        pw::stream::StreamState::Paused if old == pw::stream::StreamState::Streaming => {
            log::info!("{} Audio disconnected or node switch.", "⏸️".yellow());
        }
        pw::stream::StreamState::Streaming if old == pw::stream::StreamState::Paused => {
            log::info!("{} Audio captured (connection established)", "▶️".green());
        }
        _ => {}
    }
}

pub fn param_changed_handler(
    _stream: &pw::stream::Stream,
    _user_data: &mut (),
    id: u32,
    param: Option<&pw::spa::pod::Pod>,
    rate_for_param: &AtomicU32,
) {
    let Some(param) = param else { return };
    if id != pw::spa::param::ParamType::Format.as_raw() {
        return;
    }

    let (media_type, media_subtype) = match pw::spa::param::format_utils::parse_format(param) {
        Ok(v) => v,
        Err(_) => return,
    };

    if media_type != pw::spa::param::format::MediaType::Audio
        || media_subtype != pw::spa::param::format::MediaSubtype::Raw
    {
        return;
    }

    let mut format = pw::spa::param::audio::AudioInfoRaw::default();
    if format.parse(param).is_ok() {
        let rate = format.rate();
        rate_for_param.store(rate, Ordering::Release); // Pairs with Acquire swap in sync_rate (rt_callback/rate_sync.rs)
    }
}
