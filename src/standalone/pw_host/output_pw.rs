// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire output DSP pipeline and host configuration (standalone).

mod config;
mod delivery;
mod playback;
mod silence;

pub use config::{AppState, PipewireHostConfig};
#[cfg(any(test, feature = "testing"))]
pub use delivery::deliver_playback_pair_fail_closed;
pub use delivery::deliver_playback_pair_fail_closed_with_limit;
pub use playback::playback_dsp_cycle;
pub use silence::deliver_silence_pair_fail_closed;

pub(crate) use super::spa_format::validate_built_pod;
pub use super::spa_format::{
    ContractViolation, SpaPodStorage, build_spa_format_pod, build_stereo_f32p_format_pod,
    check_negotiated_rate_mismatch, mark_format_contract_ok, mark_stream_active,
    negotiated_rate_mismatch, reject_negotiated_format_violation, validate_audio_raw_format,
};

#[cfg(test)]
use super::rt_callback::handle_spa_pair_fail_closed;
#[cfg(test)]
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
#[cfg(test)]
use pipewire as pw;
#[cfg(test)]
use std::sync::atomic::Ordering;

#[cfg(test)]
use silence::{
    SilenceReason, active_quantum_frames, classify_missing_bridge_block,
    classify_missing_bridge_block_at,
};

#[cfg(test)]
#[path = "output_pw_test.rs"]
mod output_pw_test;
