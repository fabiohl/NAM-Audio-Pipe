// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(target_arch = "x86_64")]

//! Utilities for real-time and hardware configuration and monitoring.
//!
//! CPU affinity/topology/selection and the generic housekeeping pinning helpers
//! are the engine's canonical, host-agnostic `rt_hardening` API and are
//! re-exported here (single source of truth); only PipeWire-specific glue
//! (thread naming and RT-callback mark, hardware-sink probe, telemetry) stays
//! local.

pub mod pm_qos;
pub mod rt_log_mark;
pub mod telemetry;
pub mod thread;

pub use neural_amp_modeler_rs::common::tsc;
// Engine-canonical affinity API, re-exported at the `rt_setup` root and as the
// `rt_setup::affinity` module so both `rt_setup::<item>` and
// `rt_setup::affinity::<item>` paths resolve to the engine.
pub use neural_amp_modeler_rs::rt_hardening::affinity;
pub use neural_amp_modeler_rs::rt_hardening::{
    CpuCoreTopology, CpuSelectionReason, CpuSelectionReceipt, PinThreadError, SysfsTopologySource,
    SystemSysfsSource, apply_housekeeping_affinity, build_cpu_affinity_mask,
    build_housekeeping_affinity_mask, get_allowed_cpus, parse_cpu_list, parse_interrupts_per_cpu,
    parse_proc_interrupts, pin_current_thread, select_cpu_with_source, select_optimal_cpu,
    select_optimal_cpu_with_receipt,
};
pub use pm_qos::*;
pub use rt_log_mark::*;
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
