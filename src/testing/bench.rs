// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cycle-timing, summary statistics, and best-effort thread pinning helpers
//! shared by A/B benchmark binaries.

use crate::standalone::rt_setup::{PinThreadError, pin_current_thread};
use std::time::{Duration, Instant};

/// Summary distribution (exact min/max, mean, nearest-rank percentiles) of
/// per-operation cycle or nanosecond samples.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stats {
    pub n: usize,
    pub min: u64,
    pub mean: f64,
    pub p50: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}

impl Stats {
    /// Computes summary statistics over `samples` (returns `Default` when empty).
    pub fn of(mut samples: Vec<u64>) -> Self {
        let n = samples.len();
        if n == 0 {
            return Self::default();
        }
        samples.sort_unstable();
        let idx = |p: f64| {
            let i = ((n as f64) * p).ceil() as usize;
            samples[i.min(n).saturating_sub(1)]
        };
        let sum: u128 = samples.iter().map(|&c| c as u128).sum();
        Self {
            n,
            min: samples[0],
            mean: sum as f64 / n as f64,
            p50: idx(0.50),
            p99: idx(0.99),
            p999: idx(0.999),
            max: *samples.last().expect("n > 0"),
        }
    }
}

/// Serialized TSC timestamp in cycles (`lfence; rdtsc` on `x86_64`, `0` elsewhere).
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn rdtsc_cycles() -> u64 {
    // SAFETY: `_mm_lfence` + `_rdtsc` are available on all x86-64 CPUs; they
    // perform no memory access and have no side effects beyond reading TSC.
    unsafe {
        core::arch::x86_64::_mm_lfence();
        core::arch::x86_64::_rdtsc()
    }
}

/// Serialized TSC timestamp in cycles (`0` on non-`x86_64` architectures).
#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
pub fn rdtsc_cycles() -> u64 {
    0
}

/// Measures the effective TSC frequency in GHz × 1000 over a ~120 ms window.
pub fn measure_freq_ghz_x1000() -> u64 {
    let _ = rdtsc_cycles();
    std::thread::sleep(Duration::from_millis(5));
    let t0 = rdtsc_cycles();
    let wall = Instant::now();
    std::thread::sleep(Duration::from_millis(120));
    let t1 = rdtsc_cycles();
    let ns = wall.elapsed().as_nanos().max(1) as u64;
    let cyc = t1.saturating_sub(t0);
    (cyc * 1000) / ns
}

/// Pins the calling thread to `cpu` best-effort: syscall failures warn and
/// continue, whereas out-of-bounds CPUs fail closed with [`PinThreadError`].
pub fn pin_thread(cpu: Option<usize>) -> Result<(), PinThreadError> {
    match pin_current_thread(cpu) {
        Ok(()) => Ok(()),
        Err(PinThreadError::SyscallFailed { cpu, .. }) => {
            eprintln!("  warn: could not pin thread to cpu {cpu}");
            Ok(())
        }
        Err(e @ PinThreadError::CpuOutOfBounds { .. }) => Err(e),
        Err(e) => {
            eprintln!("  warn: could not pin thread: {e}");
            Ok(())
        }
    }
}
