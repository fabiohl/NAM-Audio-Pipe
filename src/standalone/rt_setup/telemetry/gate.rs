// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Gate transition telemetry.

use super::state::{GateTransitionThrottle, PollState};
use crate::standalone::colors::Colorize;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_IS_FADING, RT_STATUS_IS_SILENT, RtStatusFlags,
};
use std::time::Instant;

pub(super) fn log_gate_transition(throttle: &mut GateTransitionThrottle, message: &str) {
    if let Some(count) = throttle.observe(Instant::now()) {
        if count > 1 {
            log::info!("{message} (×{count})");
        } else {
            log::info!("{message}");
        }
    }
}

pub(super) fn poll_gate_transitions(
    rt_status: &RtStatusFlags,
    was_silent: bool,
    was_fading: bool,
    state: &mut PollState,
) -> (bool, bool) {
    let current_silent = rt_status.check_flag(RT_STATUS_IS_SILENT);

    if current_silent != was_silent {
        let message = if current_silent {
            format!(
                "{} Silent Mode: Input below threshold (Gate Closed).",
                "🔇".blue()
            )
        } else {
            format!(
                "{} Audio Signal Detected: DSP processing resumed.",
                "🔊".green()
            )
        };
        log_gate_transition(&mut state.gate_throttle, &message);
    }

    let current_fading = rt_status.check_flag(RT_STATUS_IS_FADING);

    if current_fading != was_fading && current_fading {
        let message = format!("{} Signal Transition: Gate in Fade-In/Out.", "🌓".yellow());
        log_gate_transition(&mut state.gate_throttle, &message);
    }

    (current_silent, current_fading)
}
