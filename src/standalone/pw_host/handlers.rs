// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Handlers for SPSC non-RT communication and dynamic parameter rebuilds.
//!
//! Handles resampler, buffer quantum logging, CabSim IR rebuild,
//! slimmable WaveNet slicing, and oversampling engine reconfiguration.

#[path = "handlers/cabsim.rs"]
mod cabsim;
#[path = "handlers/faults.rs"]
mod faults;
#[path = "handlers/resampler.rs"]
mod resampler;
#[path = "handlers/slimmable_os.rs"]
mod slimmable_os;

pub(super) use cabsim::{handle_cabinet_nam_rebuild, handle_cabsim_rebuild};
#[cfg(all(test, feature = "testing"))]
pub(super) use faults::testing::{os_fault, resampler_fault, slimmable_fault};
pub(super) use resampler::{handle_quantum_log, handle_resampler_rebuild};
pub(super) use slimmable_os::{handle_oversample_rebuild, handle_slimmable_rebuild};

/// Main-thread failure latches for off-RT rebuilds.
///
/// `RtStatusFlags` (owned by the `NeuralAmpModeler-rs` crate) already tracks
/// `resampler_failed_generation` because the RT callback reads it for the
/// fail-open unmute guard. Oversampling and slimmable rebuilds have no RT-side
/// reader, so their failed generations live here — plain main-thread state
/// owned by the control loop. It mirrors the resampler semantics exactly: a
/// build failure for generation N suppresses further attempts until a newer
/// request (higher generation) arrives, capping the retry storm at one
/// allocation + one `log::error!` per generation.
#[derive(Debug, Default)]
pub(super) struct RebuildFailureTracker {
    /// Last oversample build generation that failed (`0` = none).
    pub(super) os_failed_generation: u64,
    /// Last slimmable slice generation that failed (`0` = none).
    pub(super) slimmable_failed_generation: u64,
}

#[cfg(test)]
fn rearm_rebuild_if_superseded(
    rt_status: &neural_amp_modeler_rs::common::spsc::RtStatusFlags,
    generation: u64,
) {
    faults::rearm_if_superseded(
        rt_status,
        neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD,
        &rt_status.requested_rate_generation,
        generation,
    );
}

#[cfg(test)]
fn rearm_cabsim_if_superseded(
    rt_status: &neural_amp_modeler_rs::common::spsc::RtStatusFlags,
    generation: u64,
) {
    faults::rearm_if_superseded(
        rt_status,
        neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_CABSIM_REBUILD,
        &rt_status.requested_cabsim_generation,
        generation,
    );
}

#[cfg(test)]
fn rearm_slimmable_if_superseded(
    rt_status: &neural_amp_modeler_rs::common::spsc::RtStatusFlags,
    generation: u64,
) {
    faults::rearm_if_superseded(
        rt_status,
        neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD,
        &rt_status.requested_slimmable_generation,
        generation,
    );
}

#[cfg(test)]
fn rearm_os_if_superseded(
    rt_status: &neural_amp_modeler_rs::common::spsc::RtStatusFlags,
    generation: u64,
) {
    faults::rearm_if_superseded(
        rt_status,
        neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_OS_REBUILD,
        &rt_status.requested_os_generation,
        generation,
    );
}

#[cfg(test)]
#[path = "handlers_test.rs"]
mod tests;
