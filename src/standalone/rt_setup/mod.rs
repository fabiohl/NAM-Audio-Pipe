// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(target_arch = "x86_64")]

//! Utilities for real-time and hardware configuration and monitoring.
//!
//! This module contains helper functions for managing CPU affinity,
//! SCHED_FIFO priorities, memory locking (mlockall), and telemetry
//! for the audio engine's RT status.

pub mod affinity;
pub mod pm_qos;
pub mod telemetry;
pub mod thread;
pub use neural_amp_modeler_rs::common::tsc;
// Engine RT-hardening equivalents (S4-T6 migration target). Local modules
// above delegate process/thread/PM-QoS setup to them; the affinity
// topology/selection API stays local because `PollState`/`telemetry` carry
// the local `CpuSelectionReceipt` type — engine re-exports are available as
// `neural_amp_modeler_rs::rt_hardening::*` for new code.
pub use affinity::*;
pub use pm_qos::*;
pub use telemetry::*;
pub use thread::*;
pub use tsc::*;

/// Computes the final multipliers combining user gain and model adjustments.
/// Ultra-fast operation (only linear multiplications) to keep the RT callback lightweight.
pub fn compute_gain_multipliers(
    u_in_mult: f32,
    u_out_mult: f32,
    m_in_mult: f32,
    m_out_mult: f32,
    out_in_mult: &mut f32,
    out_out_mult: &mut f32,
) {
    *out_in_mult = u_in_mult * m_in_mult;
    *out_out_mult = u_out_mult * m_out_mult;
}

#[cfg(test)]
#[path = "rt_setup_test.rs"]
mod rt_setup_test;
