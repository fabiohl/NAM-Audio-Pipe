// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Rate, clipping, huge-page, and RT-thread telemetry.

use super::state::PollState;
use crate::standalone::colors::Colorize;
use crate::standalone::rt_setup::affinity::CpuSelectionReason;
use neural_amp_modeler_rs::common::spsc::{
    RT_STATUS_HAS_CLIPPED, RT_STATUS_HUGEPAGE_OK, RT_STATUS_NON_FINITE_INPUT_DETECTED,
    RT_STATUS_RT_IS_FIFO, RT_STATUS_THP_ACTIVE, RtStatusFlags,
};
use std::sync::atomic::Ordering;

pub(super) fn poll_rate_clip_and_hugepages(rt_status: &RtStatusFlags, state: &mut PollState) {
    let rate_notif = rt_status.active_rate_changed.swap(0, Ordering::Relaxed);
    if rate_notif != 0 {
        log::info!(
            "{} RT callback activated resampler with rate = {} Hz",
            "✅".green(),
            rate_notif
        );
    }

    if state
        .latches
        .clipping
        .observe(rt_status.check_and_clear_flag(RT_STATUS_HAS_CLIPPED))
    {
        log::warn!(
            "{} Clipping detected! Consider reducing the input and/or output gain, \
             or checking whether the model itself carries a large DC offset.",
            "🔥".bright_red().bold()
        );
    }

    if state
        .latches
        .non_finite_input
        .observe(rt_status.check_flag(RT_STATUS_NON_FINITE_INPUT_DETECTED))
    {
        log::error!(
            "[E2305 | NON_FINITE_INPUT_CONTAINED] Non-finite audio (NaN/Inf) detected on the \
             capture path — inputs were sanitized and the output is being sanitized until the \
             signal recovers (engine containment barrier). Check the PipeWire graph for a \
             source emitting corrupt floats."
        );
    }

    if !state.hugepage_synced {
        neural_amp_modeler_rs::dsp::mirror_buf::sync_huge_page_flag(rt_status);
        state.hugepage_synced = true;
    }
    if rt_status.check_and_clear_flag(RT_STATUS_HUGEPAGE_OK) {
        log::info!(
            "{} HugeTLB explicit 2 MB pages active — reduced TLB pressure on DSP thread.",
            "✅".green()
        );
    }
    if rt_status.check_and_clear_flag(RT_STATUS_THP_ACTIVE) {
        log::info!(
            "{} Transparent Huge Pages (THP) advice active — kernel may promote to 2 MB.",
            "ℹ️".green()
        );
    }
}

pub(super) fn poll_rt_thread_setup(rt_status: &RtStatusFlags, state: &PollState) {
    let aff_err = rt_status.rt_affinity_err.swap(0, Ordering::Relaxed);
    let sched_err = rt_status.rt_sched_err.swap(0, Ordering::Relaxed);
    let getsched_err = rt_status.rt_getsched_err.swap(0, Ordering::Relaxed);
    let target_cpu = rt_status.rt_target_cpu.swap(-1, Ordering::Relaxed);
    let tid = rt_status.rt_tid.load(Ordering::Relaxed);

    if aff_err == -1 {
        log::error!(
            "CPU {} is out of bounds (CPU_SETSIZE={}). NAM-Audio-Pipe will continue running without CPU affinity.\n\
             [E2306 | CPU_OUT_OF_BOUNDS] cpu={} max={}",
            target_cpu,
            libc::CPU_SETSIZE,
            target_cpu,
            libc::CPU_SETSIZE - 1,
        );
    } else if aff_err > 0 {
        log::error!(
            "\n  ⚡ Failed to set CPU affinity to core {} (errno={}).\n  💡 NAM-Audio-Pipe will continue running, but may suffer jitter due to Core Migration.\n\
             [E2301 | CPU_AFFINITY_FAILED] cpu={} errno={}\n",
            target_cpu,
            aff_err,
            target_cpu,
            aff_err
        );
    }

    if sched_err > 0 {
        log::error!(
            "⚠️ pthread_setschedparam failed (errno={}, TID={tid}).\n\
             [E2308 | RT_SCHED_FAILED] Check ulimit -r and rtkit permissions.\n",
            sched_err
        );
    }

    if getsched_err > 0 {
        log::error!(
            "  [E2303 | RT_GETSCHED_FAILED] pthread_getschedparam failed (ret={}).\n",
            getsched_err
        );
    }

    let prio = rt_status.rt_priority.load(Ordering::Relaxed);
    if prio != -1 {
        let is_fifo = rt_status.check_flag(RT_STATUS_RT_IS_FIFO);
        let policy = rt_status.rt_policy.load(Ordering::Relaxed);
        let cpu = rt_status.rt_cpu.load(Ordering::Relaxed);

        rt_status.rt_priority.store(-1, Ordering::Relaxed);

        if is_fifo || policy == libc::SCHED_RR {
            let policy_name = if policy == libc::SCHED_RR {
                "RR"
            } else {
                "FIFO"
            };

            let is_dedicated = state.cpu_receipt.as_ref().is_some_and(|r| r.is_dedicated);

            if is_dedicated {
                log::info!(
                    "{} Real-Time Priority: Active ({}, Prio={}, Dedicated Core {}, TID={})",
                    "⚡".yellow(),
                    policy_name,
                    prio.to_string().green(),
                    cpu.to_string().cyan(),
                    tid
                );
            } else {
                let reason_str = state.cpu_receipt.as_ref().map_or(
                    "Conservative heuristic / unverified topology",
                    |r| match &r.reason {
                        CpuSelectionReason::ConservativeHeuristic { explanation, .. } => {
                            *explanation
                        }
                        CpuSelectionReason::ExplicitCli { .. } => {
                            "Explicit CLI pinning (non-isolated)"
                        }
                        CpuSelectionReason::FullyIsolated { .. } => "Fully isolated core",
                        _ => "Unclassified CPU selection reason",
                    },
                );

                log::info!(
                    "{} Real-Time Priority: Active ({}, Prio={}, Core {}, TID={}) [Affinity: {}]",
                    "⚡".yellow(),
                    policy_name,
                    prio.to_string().green(),
                    cpu.to_string().cyan(),
                    tid,
                    reason_str
                );
            }
        } else {
            let policy_str = match policy {
                libc::SCHED_OTHER => "OTHER",
                libc::SCHED_BATCH => "BATCH",
                libc::SCHED_IDLE => "IDLE",
                _ => "NON-RT",
            };
            log::warn!(
                "[E2300 | RT_PRIORITY_DENIED] DSP thread is NOT in a Real-Time scheduling \
                 policy (policy = {policy_str}, priority = {prio}, TID={tid}) — audio may \
                 experience jitter and xruns under CPU load. Ensure PipeWire RT module or rtkit is configured."
            );
        }
    }
}
