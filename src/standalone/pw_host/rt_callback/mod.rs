// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Helper functions executed inside the capture stream's `process()` RT callback.
//!
//! All functions in this module follow the absolute callback rules:
//! - Zero heap allocation
//! - Zero I/O
//! - Zero mutexes

mod cabsim_swap;
mod commands;
mod process;
mod rate_sync;
mod resampler_swap;

/// Offline RT swap-stress harness (T2.6 / ER-2), available only under the
/// `testing` feature. Reproduces the full capture-callback drain sequence with
/// no PipeWire daemon so integration tests can soak concurrent swaps and run
/// the zero-allocation heap audit.
#[cfg(feature = "testing")]
pub mod harness;

pub use cabsim_swap::drain_cabsims;
pub use commands::{
    drain_os_engines, drain_slimmable_models, receive_commands, try_slimmable_rebuild,
};
pub use process::process_dsp_buffer;
pub(crate) use process::{handle_spa_pair_fail_closed, silence_available_datas};
pub use rate_sync::sync_rate;
pub use resampler_swap::drain_resamplers;
