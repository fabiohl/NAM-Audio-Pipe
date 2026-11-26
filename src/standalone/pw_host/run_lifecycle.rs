// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Lifecycle helpers for [`super::run::run_pipewire_host_with_cabinet`].
//!
//! Splits stream setup, bounded-reconnect retry handling, single-iteration
//! control-loop orchestration, instance teardown, and final shutdown reporting
//! out of the top-level host loop so each phase stays small and auditable.

mod control;
mod retry;
mod status;
mod teardown;

#[cfg(test)]
pub(super) use control::drain_control_gc;
pub(super) use control::run_control_housekeeping;
pub(super) use retry::{
    RetryDecision, handle_attempt_error, interrupted_backoff_failure, sleep_interruptible,
};
pub(super) use status::snapshot_and_publish_initial_latencies;
pub(super) use teardown::{finalize_host_shutdown, reset_bridge_and_drain_gc};

#[cfg(test)]
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags};

#[cfg(test)]
#[path = "run_lifecycle_test.rs"]
mod tests;
