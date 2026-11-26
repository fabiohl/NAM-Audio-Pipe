// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `setup_capture_stream` — configures the PipeWire capture stream (Virtual Sink)
//! and its RT listener, with process, param_changed, and state_changed closures.

mod connect;
mod listener;
mod properties;
mod stream;

#[cfg(test)]
use listener::cab_rate_diverged;
#[cfg(test)]
pub(crate) use properties::create_capture_properties;
pub use stream::setup_capture_stream;

#[cfg(test)]
pub(crate) use super::super::rt_callback::{
    adaptive_update_due, cabsim_partition_to_request, cabsim_rebuild_needed, nam_cab_rebuild_needed,
};
#[cfg(test)]
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;
#[cfg(test)]
use std::sync::atomic::Ordering;

#[cfg(test)]
use crate::standalone::pw_host::output_pw::{SpaPodStorage, build_stereo_f32p_format_pod};
#[cfg(test)]
use pipewire as pw;

#[cfg(test)]
#[path = "setup_test.rs"]
mod setup_test;
