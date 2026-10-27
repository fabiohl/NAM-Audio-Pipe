// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Shared real-time environment calibration probe for integration harnesses.
//!
//! Single source of truth for "is this host a certified RT environment?"
//! (`Cpus_allowed_list`, `/sys/devices/system/cpu/isolated`, `sched_getscheduler`).
//! `tests/rt_metrics.rs` and `tests/pw_integration.rs` must never drift apart on
//! what counts as a calibrated (pinned single isolated CPU + `SCHED_FIFO`) host,
//! because the tight E2E gate is only allowed to `PASS` there.
//!
//! Kept self-contained (rather than re-exporting the engine's
//! `testing::host::rt_env`) because `pw_integration` builds without the
//! `testing` feature, which is what gates the engine's host probes.
//!
//! Host topology policy: the process affinity decides the measured CPU
//! (`NAM_BENCH_CORE` → first online isolated CPU → first permitted CPU);
//! there is no host-specific core preference and no `cpuN` probe here.

/// Reads the process's `Cpus_allowed_list` from `/proc/self/status`.
pub fn cpus_allowed_list() -> Vec<u32> {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
            return parse_cpu_ranges(rest.trim());
        }
    }
    Vec::new()
}

/// Parses the kernel's `/sys/devices/system/cpu/isolated` CPU ids.
pub fn isolated_cpus() -> Vec<u32> {
    let s = std::fs::read_to_string("/sys/devices/system/cpu/isolated").unwrap_or_default();
    parse_cpu_ranges(s.trim())
}

/// Expands "0,2-4" style CPU ranges into a flat id list.
pub fn parse_cpu_ranges(s: &str) -> Vec<u32> {
    let mut cpus = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u32 = lo.trim().parse().unwrap_or(0);
            let hi: u32 = hi.trim().parse().unwrap_or(lo);
            cpus.extend(lo..=hi);
        } else if let Ok(c) = part.parse::<u32>() {
            cpus.push(c);
        }
    }
    cpus
}

/// Tries to set `SCHED_FIFO` with the given priority on the current thread.
pub fn try_promote_sched_fifo(priority: i32) -> bool {
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: `param` is a fully initialized `sched_param`; the syscall only
    // reads it and targets the calling thread (`0`).
    unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0 }
}

/// Reads the cpufreq scaling_governor for the given CPU or cpu0.
pub fn read_cpu_governor(cpu: Option<u32>) -> String {
    let path = match cpu {
        Some(id) => format!("/sys/devices/system/cpu/cpu{id}/cpufreq/scaling_governor"),
        None => "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor".to_string(),
    };
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Calibration facts of the measurement environment (probed once per gate).
#[derive(Debug, Clone)]
pub struct RtCalibration {
    /// The process is pinned to exactly one CPU (`Cpus_allowed_list`).
    pub pinned_single_cpu: bool,
    /// The pinned CPU id, if pinned to a single core.
    pub pinned_cpu: Option<u32>,
    /// The pinned CPU is in the kernel's isolcpus set.
    pub cpu_isolated: bool,
    /// `sched_getscheduler` reports `SCHED_FIFO`.
    pub sched_fifo: bool,
    /// Scaling governor of the pinned CPU or system.
    pub governor: String,
    /// `NAM_RT_STRICT=1`: promote every GAP condition to a hard failure.
    pub strict: bool,
}

impl RtCalibration {
    /// Probes the calibration facts once.
    pub fn probe() -> Self {
        let allowed = cpus_allowed_list();
        let pinned_single_cpu = allowed.len() == 1;
        let pinned_cpu = if pinned_single_cpu {
            allowed.first().copied()
        } else {
            None
        };
        let isolated = isolated_cpus();
        let cpu_isolated = pinned_cpu.is_some_and(|c| isolated.contains(&c));
        // SAFETY: `sched_getscheduler` only reads the scheduling policy of the
        // calling thread (`0`) and returns an integer; no pointers involved.
        let sched_fifo = unsafe { libc::sched_getscheduler(0) } == libc::SCHED_FIFO;
        Self {
            pinned_single_cpu,
            pinned_cpu,
            cpu_isolated,
            sched_fifo,
            governor: read_cpu_governor(pinned_cpu),
            strict: std::env::var("NAM_RT_STRICT").as_deref() == Ok("1"),
        }
    }

    /// A certified realtime environment: exclusive CPU affinity on an isolated
    /// core plus a FIFO scheduler.
    pub fn calibrated(&self) -> bool {
        self.pinned_single_cpu && self.cpu_isolated && self.sched_fifo
    }

    /// Formats the calibration evidence string for `TEST_RESULT[...]` markers.
    pub fn format_env(&self) -> String {
        let pinned = if self.pinned_single_cpu { 1 } else { 0 };
        let isolated = if self.cpu_isolated { 1 } else { 0 };
        let fifo = if self.sched_fifo { 1 } else { 0 };
        let cpu_str = match self.pinned_cpu {
            Some(id) => id.to_string(),
            None => "none".to_string(),
        };
        format!(
            "env=pinned:{pinned},isolated:{isolated},fifo:{fifo},governor:{},cpu:{cpu_str}",
            self.governor
        )
    }

    /// Best-effort SCHED_FIFO promotion for the current thread.
    pub fn promote_current_thread(priority: i32) -> bool {
        try_promote_sched_fifo(priority)
    }
}
