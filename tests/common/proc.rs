// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Shared `/proc` telemetry readers for the soak and endurance harnesses
//! (T5.3 / G-PERF-004).
//!
//! Single source of truth for the raw RSS / page-fault / thread / FD probes so
//! `tests/soak_extended.rs` and `tests/endurance.rs` never drift apart — a
//! kernel-format or field-index fix lands here exactly once.

/// Reads the current process resident set size in KiB from `/proc/self/status`.
pub fn read_rss_kb() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
        }
    }
    0
}

/// Reads minor/major page fault totals from `/proc/self/stat` (fields 10/12).
pub fn read_page_faults() -> (u64, u64) {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let Some(tail) = stat.rfind(')') else {
        return (0, 0);
    };
    let fields: Vec<&str> = stat[tail + 1..].split_whitespace().collect();
    let minflt = fields
        .get(7)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let majflt = fields
        .get(9)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    (minflt, majflt)
}

/// Reads the thread count (`Threads:` line of `/proc/self/status`).
pub fn read_thread_count() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Threads:") {
            return rest.trim().parse::<usize>().unwrap_or(0);
        }
    }
    0
}

/// Reads the number of open file descriptors (`/proc/self/fd` entries).
pub fn read_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|it| it.count())
        .unwrap_or(0)
}

/// One periodic telemetry sample (raw values — never `saturating_sub`).
#[derive(Debug, Clone, Copy)]
pub struct TelemetrySample {
    pub rss_kb: usize,
    pub minflt: u64,
    pub majflt: u64,
    pub threads: usize,
    pub fds: usize,
}

impl TelemetrySample {
    pub fn capture() -> Self {
        let (minflt, majflt) = read_page_faults();
        Self {
            rss_kb: read_rss_kb(),
            minflt,
            majflt,
            threads: read_thread_count(),
            fds: read_fd_count(),
        }
    }
}
