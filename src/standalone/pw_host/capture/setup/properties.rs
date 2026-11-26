// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Capture Virtual Sink property construction.

use pipewire as pw;
use pw::properties::properties;

/// Assembles PipeWire property attributes for the capture Virtual Sink node.
pub fn create_capture_properties(buffer_size: u32) -> pw::properties::PropertiesBox {
    let pid_str = std::process::id().to_string();
    let mut capture_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Duplex",
        *pw::keys::MEDIA_ROLE => "DSP",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => crate::standalone::pw_host::identity::PW_CAPTURE_NODE_NAME,
        *pw::keys::NODE_DESCRIPTION => crate::standalone::pw_host::identity::PW_CAPTURE_NODE_DESC,
        *pw::keys::NODE_VIRTUAL => "true",
        *pw::keys::PRIORITY_SESSION => "2000",
        *pw::keys::PRIORITY_DRIVER => "2000",
        "audio.position" => "FL,FR",
        "node.group" => crate::standalone::pw_host::identity::PW_NODE_GROUP,
        "node.link-group" => crate::standalone::pw_host::identity::PW_LINK_GROUP,
        "application.process.id" => pid_str.as_str(),
    };

    if buffer_size > 0 {
        // PipeWire `node.latency` property contract:
        // Expressed as a rational fraction ("quantum/rate", e.g. "128/48000").
        // PipeWire treats this as a requested duration in seconds: tau = quantum / rate.
        // When connecting to a graph running at a different rate R, PipeWire dynamically
        // reschedules the quantum to round(tau * R). Thus "{buffer_size}/48000" requests
        // exactly `buffer_size` frames of latency at 48kHz, dynamically rescaled by PipeWire.
        let latency_str = format!("{}/48000", buffer_size);
        capture_props.insert("node.latency", latency_str.as_str());
    }

    capture_props
}
