// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Optimal CPU core selection and interrupt analysis.
//!
//! Implements heuristics to pin the DSP thread to the core with the highest
//! processing capacity and lowest IRQ load, respecting
//! kernel affinity masks (isolcpus, cgroups).

/// Selects the ideal CPU core to pin the RT thread (core affinity).
///
/// The heuristic prioritizes:
/// 1. **Affinity Filter** — Respects the cores authorized by the OS (isolcpus, cgroups).
/// 2. **Maximum Capacity** — On hybrid architectures (e.g. big.LITTLE), prefers high-performance cores.
/// 3. **Lowest IRQ Load** — Minimizes jitter caused by hardware interrupts (network, disk, etc.).
/// 4. **Numeric Index** — Deterministic tiebreaker.
pub fn select_optimal_cpu() -> Option<usize> {
    use std::fs;

    // 1. Gets the cores the operating system has allowed for this process.
    let allowed_cpus = get_allowed_cpus();
    if allowed_cpus.is_empty() {
        log::warn!("Could not detect allowed CPUs via sched_getaffinity. Using total fallback.");
    }

    // Scans /sys to find all available logical cores.
    let cpu_dir = fs::read_dir("/sys/devices/system/cpu").ok()?;
    let mut cpus: Vec<usize> = cpu_dir
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            let name = name.to_str()?;
            // Filters only directories in the 'cpuX' format.
            let cpu_idx = name.strip_prefix("cpu")?.parse::<usize>().ok()?;

            // If we have the allowed list, we filter by it.
            if !allowed_cpus.is_empty() && !allowed_cpus.contains(&cpu_idx) {
                return None;
            }

            Some(cpu_idx)
        })
        .collect();

    if cpus.is_empty() {
        return None;
    }
    cpus.sort_unstable();

    // Gets the processing capacity of each core.
    let capacities: Vec<(usize, u64)> = cpus
        .iter()
        .map(|&cpu| {
            let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity");
            let cap = fs::read_to_string(path)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(1024); // Default kernel fallback.
            (cpu, cap)
        })
        .collect();

    // Totals the interrupts handled per core to detect "noisy neighbors".
    let irq_totals = parse_interrupts_per_cpu();

    capacities
        .iter()
        .map(|&(cpu, cap)| {
            let irqs = irq_totals.get(&cpu).copied().unwrap_or(u64::MAX);
            (cpu, cap, irqs)
        })
        .max_by(|a, b| {
            // Sorting Heuristic:
            // 1. Highest capacity (cap)
            // 2. Lowest interrupt count (irqs) — inverted in cmp
            // 3. Highest CPU index (cpu)
            a.1.cmp(&b.1)
                .then_with(|| b.2.cmp(&a.2))
                .then_with(|| a.0.cmp(&b.0))
        })
        .map(|(cpu, _, _)| cpu)
}

/// Gets the list of CPUs allowed for the current process via `sched_getaffinity`.
///
/// Respects CPU isolation (isolcpus), cgroups and affinity masks
/// imposed by the operating system or the user (e.g. taskset).
pub fn get_allowed_cpus() -> Vec<usize> {
    let mut allowed = Vec::new();
    unsafe {
        let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut cpuset);

        // Kernel call to get the affinity mask of the current process (pid 0)
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut cpuset) == 0 {
            for i in 0..libc::CPU_SETSIZE as usize {
                if libc::CPU_ISSET(i, &cpuset) {
                    allowed.push(i);
                }
            }
        }
    }
    allowed
}

/// Parses /proc/interrupts to extract the interrupt load per physical core.
///
/// Returns a map from physical CPU index (extracted from the header) to the total
/// number of numeric interrupts serviced by that CPU.
pub fn parse_interrupts_per_cpu() -> std::collections::HashMap<usize, u64> {
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let mut totals: HashMap<usize, u64> = HashMap::new();

    // Streaming Refactoring: Avoids monolithic string allocation for /proc/interrupts.
    // Essential for systems with high CPU counts where the file can be large.
    let file = match File::open("/proc/interrupts") {
        Ok(f) => f,
        Err(_) => return totals,
    };
    let mut reader = BufReader::new(file);

    // 1. Parse header to extract physical CPU IDs (e.g. CPU0, CPU1, CPU4)
    let mut header = String::new();
    if reader.read_line(&mut header).is_err() {
        return totals;
    }
    let cpu_ids: Vec<usize> = header
        .split_whitespace()
        .filter_map(|tok| tok.strip_prefix("CPU")?.parse::<usize>().ok())
        .collect();

    if cpu_ids.is_empty() {
        return totals;
    }
    for &id in &cpu_ids {
        totals.insert(id, 0);
    }

    // 2. Accumulate IRQ counts keyed by physical CPU ID
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim_start();
        // Locates the interrupt name delimiter (e.g. "7: ...")
        let irq_end = trimmed.find(':').unwrap_or(0);
        if irq_end == 0 {
            continue;
        }

        // Filters only numeric interrupts (ignores NMI, LOC, etc. for simplicity).
        if !trimmed[..irq_end]
            .trim()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }

        let after_colon = match trimmed.get(irq_end + 1..) {
            Some(s) => s,
            None => continue,
        };

        // Accumulates counters for each core present in the line.
        for (&cpu_id, token) in cpu_ids.iter().zip(after_colon.split_whitespace()) {
            if let Ok(count) = token.parse::<u64>() {
                *totals.entry(cpu_id).or_insert(0) += count;
            } else {
                // Stops at the first non-numeric token (usually the driver/device name).
                break;
            }
        }
    }

    totals
}
