// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Capture listener RT-thread and cabinet-rate helpers.

use super::super::state::CaptureState;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use std::sync::Arc;

#[cold]
#[inline(never)]
pub(super) fn configure_rt_thread_once(
    state: &mut CaptureState,
    target_cpu: usize,
    rt_status: &Arc<RtStatusFlags>,
) {
    rt_setup::configure_realtime_thread(target_cpu, Arc::clone(rt_status), state.activation);
    state.thread_configured = true;
}

/// Whether an installed cabinet slot cannot be used at the current host rate.
///
/// The callback bypasses a slot whose rate differs from the host quantum rate;
/// this helper makes that condition observable instead of silently ignoring
/// the cabinet.
#[inline]
pub(crate) fn cab_rate_diverged(
    host_rate: u32,
    cabsim_rate: Option<u32>,
    nam_cab_rate: Option<u32>,
) -> bool {
    cabsim_rate.is_some_and(|rate| rate != host_rate)
        || nam_cab_rate.is_some_and(|rate| rate != host_rate)
}
