// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Shared `/proc` telemetry readers for the soak and endurance harnesses.
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

/// Thread resource usage counters from `getrusage(RUSAGE_THREAD)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadRusage {
    pub minflt: u64,
    pub majflt: u64,
    pub nvcsw: u64,
    pub nivcsw: u64,
}

/// Reads resource usage counters for the calling thread via `getrusage(RUSAGE_THREAD)`.
///
/// Returns an error if the kernel/libc does not support `RUSAGE_THREAD`, allowing
/// callers to fall back to `/proc/thread-self` with a declared gap marker.
pub fn read_thread_rusage() -> Result<ThreadRusage, std::io::Error> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: `getrusage` writes up to `size_of::<libc::rusage>()` into the provided pointer.
    let ret = unsafe { libc::getrusage(libc::RUSAGE_THREAD, usage.as_mut_ptr()) };
    if ret == 0 {
        let usage = unsafe { usage.assume_init() };
        return Ok(ThreadRusage {
            minflt: usage.ru_minflt as u64,
            majflt: usage.ru_majflt as u64,
            nvcsw: usage.ru_nvcsw as u64,
            nivcsw: usage.ru_nivcsw as u64,
        });
    }

    eprintln!("GAP:thread_rusage_unavailable");
    read_thread_rusage_proc()
}

/// Fallback reader for thread resource usage using `/proc/thread-self/stat` and `/proc/thread-self/status`.
pub fn read_thread_rusage_proc() -> Result<ThreadRusage, std::io::Error> {
    let stat = std::fs::read_to_string("/proc/thread-self/stat")?;
    let tail = stat.rfind(')').ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid stat format")
    })?;
    let fields: Vec<&str> = stat[tail + 1..].split_whitespace().collect();
    let minflt = fields
        .get(7)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let majflt = fields
        .get(9)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let status = std::fs::read_to_string("/proc/thread-self/status").unwrap_or_default();
    let mut nvcsw = 0u64;
    let mut nivcsw = 0u64;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("voluntary_ctxt_switches:") {
            nvcsw = rest.trim().parse::<u64>().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
            nivcsw = rest.trim().parse::<u64>().unwrap_or(0);
        }
    }

    Ok(ThreadRusage {
        minflt,
        majflt,
        nvcsw,
        nivcsw,
    })
}
