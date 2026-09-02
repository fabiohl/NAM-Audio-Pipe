// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Optimal CPU core selection, hardware topology inspection, and interrupt analysis.
//!
//! Implements topology parsing (package/core IDs, SMT thread siblings, cpuset,
//! kernel CPU isolation via isolcpus / sysfs, full tickless nohz_full) to pin
//! the RT audio thread to the most isolated, highest-capacity core, while confining
//! housekeeping and I/O worker threads away from the RT core.

use std::collections::HashMap;

/// Parses standard Linux CPU list syntax (e.g. "0-3,7,9-10") into a sorted, unique vector of CPU indices.
pub fn parse_cpu_list(text: &str) -> Vec<usize> {
    let mut cpus = Vec::new();
    for part in text.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((start_s, end_s)) = trimmed.split_once('-') {
            if let (Ok(start), Ok(end)) = (
                start_s.trim().parse::<usize>(),
                end_s.trim().parse::<usize>(),
            ) && start <= end
            {
                for cpu in start..=end {
                    cpus.push(cpu);
                }
            }
        } else if let Ok(cpu) = trimmed.parse::<usize>() {
            cpus.push(cpu);
        }
    }
    cpus.sort_unstable();
    cpus.dedup();
    cpus
}

/// Parses /proc/interrupts table from a reader, returning total numeric interrupts per CPU index.
pub fn parse_proc_interrupts<R: std::io::BufRead>(reader: R) -> HashMap<usize, u64> {
    let mut totals: HashMap<usize, u64> = HashMap::new();
    let mut lines = reader.lines();

    let header = match lines.next() {
        Some(Ok(h)) => h,
        _ => return totals,
    };

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

    for line_res in lines {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim_start();
        let irq_end = trimmed.find(':').unwrap_or(0);
        if irq_end == 0 {
            continue;
        }

        // Filters only numeric interrupts (ignores NMI, LOC, etc.).
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

        for (&cpu_id, token) in cpu_ids.iter().zip(after_colon.split_whitespace()) {
            if let Ok(count) = token.parse::<u64>() {
                *totals.entry(cpu_id).or_insert(0) += count;
            } else {
                break;
            }
        }
    }

    totals
}

/// Parses /proc/interrupts to extract the interrupt load per physical CPU.
pub fn parse_interrupts_per_cpu() -> HashMap<usize, u64> {
    use std::fs::File;
    use std::io::BufReader;

    let file = match File::open("/proc/interrupts") {
        Ok(f) => f,
        Err(_) => return HashMap::new(),
    };
    parse_proc_interrupts(BufReader::new(file))
}

/// Gets the list of CPUs allowed for the current process via `sched_getaffinity`.
///
/// Respects CPU isolation (isolcpus), cgroups and affinity masks
/// imposed by the operating system or the user (e.g. taskset).
pub fn get_allowed_cpus() -> Vec<usize> {
    let mut allowed = Vec::new();

    // SAFETY: on the supported Linux targets (glibc/musl) `cpu_set_t` is a C
    // bitmask whose all-zero bit pattern denotes the empty CPU set, so a
    // zero-initialized `cpu_set_t` is a fully valid value — no Rust reference
    // is formed over uninitialized storage.
    let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    // SAFETY: `CPU_ZERO` only mutates the already-initialized bitmask in place;
    // `sched_getaffinity` fills the same valid object. On failure the mask is
    // left untouched and the `ok` flag below keeps the loop from reading it.
    let ok = unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut cpuset) == 0
    };

    if ok {
        for i in 0..libc::CPU_SETSIZE as usize {
            // SAFETY: `cpuset` was filled by the successful `sched_getaffinity`
            // above; `CPU_ISSET` only reads bits within the initialized
            // `[u64; CPU_SETSIZE/64]` storage for indexes in `[0, CPU_SETSIZE)`.
            if unsafe { libc::CPU_ISSET(i, &cpuset) } {
                allowed.push(i);
            }
        }
    }
    allowed
}

/// Trait abstraction over sysfs and OS affinity queries for deterministic unit testing with fixtures.
pub trait SysfsTopologySource {
    /// Returns the list of discovered logical CPUs (e.g. from `/sys/devices/system/cpu/cpu*`).
    fn read_cpu_indices(&self) -> Vec<usize>;
    /// Reads a sysfs string from the given path.
    fn read_sysfs_string(&self, path: &str) -> Option<String>;
    /// Returns the list of allowed CPUs for the current process (from `sched_getaffinity`).
    fn get_allowed_cpus(&self) -> Vec<usize>;
    /// Returns the mapping of CPU index to cumulative IRQ count (from `/proc/interrupts`).
    fn get_irq_counts(&self) -> HashMap<usize, u64>;
}

/// Default system implementation querying the live Linux `/sys` and `/proc` filesystems.
pub struct SystemSysfsSource;

impl SysfsTopologySource for SystemSysfsSource {
    fn read_cpu_indices(&self) -> Vec<usize> {
        let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
            return Vec::new();
        };
        let mut cpus: Vec<usize> = entries
            .filter_map(|entry| {
                let name = entry.ok()?.file_name();
                let name = name.to_str()?;
                let idx = name.strip_prefix("cpu")?.parse::<usize>().ok()?;
                Some(idx)
            })
            .collect();
        cpus.sort_unstable();
        cpus
    }

    fn read_sysfs_string(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn get_allowed_cpus(&self) -> Vec<usize> {
        get_allowed_cpus()
    }

    fn get_irq_counts(&self) -> HashMap<usize, u64> {
        parse_interrupts_per_cpu()
    }
}

/// Discovered topology details for a single logical CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuCoreTopology {
    pub logical_id: usize,
    pub package_id: Option<usize>,
    pub core_id: Option<usize>,
    pub smt_siblings: Vec<usize>,
    pub capacity: u64,
    pub irq_count: u64,
    pub is_isolated: bool,
    pub is_nohz_full: bool,
    pub in_cpuset: bool,
}

/// Structured explanation of why a specific CPU was selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpuSelectionReason {
    /// Explicitly pinned via CLI `--cpu <N>`.
    ExplicitCli { cpu: usize, in_cpuset: bool },
    /// Selected core is proven isolated by the kernel (`/sys/devices/system/cpu/isolated`).
    FullyIsolated {
        cpu: usize,
        package_id: Option<usize>,
        core_id: Option<usize>,
        smt_siblings: Vec<usize>,
        nohz_full: bool,
    },
    /// Selected core was chosen via conservative heuristics (capacity, SMT, IRQs).
    ConservativeHeuristic {
        cpu: usize,
        capacity: u64,
        irq_count: u64,
        explanation: &'static str,
    },
}

/// Typed receipt of CPU selection and isolation proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuSelectionReceipt {
    pub selected_cpu: usize,
    pub is_dedicated: bool,
    pub package_id: Option<usize>,
    pub core_id: Option<usize>,
    pub smt_siblings: Vec<usize>,
    pub is_isolated: bool,
    pub is_nohz_full: bool,
    pub reason: CpuSelectionReason,
    pub housekeeping_cpus: Vec<usize>,
    pub topology: Vec<CpuCoreTopology>,
}

/// Selects the optimal CPU core given an explicit request and an injectable topology source.
pub fn select_cpu_with_source<S: SysfsTopologySource>(
    requested_cpu: Option<usize>,
    source: &S,
) -> CpuSelectionReceipt {
    let isolated_set = source
        .read_sysfs_string("/sys/devices/system/cpu/isolated")
        .map(|s| parse_cpu_list(&s))
        .unwrap_or_default();

    let nohz_set = source
        .read_sysfs_string("/sys/devices/system/cpu/nohz_full")
        .map(|s| parse_cpu_list(&s))
        .unwrap_or_default();

    let irqs = source.get_irq_counts();
    let mut allowed_cpus = source.get_allowed_cpus();
    let mut online_cpus = source.read_cpu_indices();

    if online_cpus.is_empty() {
        online_cpus = if allowed_cpus.is_empty() {
            vec![0]
        } else {
            allowed_cpus.clone()
        };
    }

    if allowed_cpus.is_empty() {
        allowed_cpus = online_cpus.clone();
    }

    let mut topology = Vec::with_capacity(online_cpus.len());
    for &cpu in &online_cpus {
        let pkg_path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/physical_package_id");
        let package_id = source
            .read_sysfs_string(&pkg_path)
            .and_then(|s| s.trim().parse::<usize>().ok());

        let core_path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_id");
        let core_id = source
            .read_sysfs_string(&core_path)
            .and_then(|s| s.trim().parse::<usize>().ok());

        let siblings_path =
            format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
        let core_cpus_path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_cpus_list");
        let smt_siblings = source
            .read_sysfs_string(&siblings_path)
            .or_else(|| source.read_sysfs_string(&core_cpus_path))
            .map(|s| parse_cpu_list(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![cpu]);

        let cap_path = format!("/sys/devices/system/cpu/cpu{cpu}/cpu_capacity");
        let capacity = source
            .read_sysfs_string(&cap_path)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(1024);

        let irq_count = irqs.get(&cpu).copied().unwrap_or(0);
        let is_isolated = isolated_set.contains(&cpu);
        let is_nohz_full = nohz_set.contains(&cpu);
        let in_cpuset = allowed_cpus.contains(&cpu);

        topology.push(CpuCoreTopology {
            logical_id: cpu,
            package_id,
            core_id,
            smt_siblings,
            capacity,
            irq_count,
            is_isolated,
            is_nohz_full,
            in_cpuset,
        });
    }

    // 1. Check if an explicit CLI CPU was requested
    if let Some(target) = requested_cpu {
        if allowed_cpus.contains(&target) {
            let target_topo = topology.iter().find(|t| t.logical_id == target);
            let is_isolated = target_topo.map(|t| t.is_isolated).unwrap_or(false);
            let is_nohz = target_topo.map(|t| t.is_nohz_full).unwrap_or(false);
            let pkg_id = target_topo.and_then(|t| t.package_id);
            let core_id = target_topo.and_then(|t| t.core_id);
            let siblings = target_topo
                .map(|t| t.smt_siblings.clone())
                .unwrap_or_else(|| vec![target]);

            let housekeeping_cpus: Vec<usize> = allowed_cpus
                .iter()
                .copied()
                .filter(|&c| c != target && !siblings.contains(&c))
                .collect();
            let final_housekeeping = if housekeeping_cpus.is_empty() {
                allowed_cpus.clone()
            } else {
                housekeeping_cpus
            };

            let receipt = CpuSelectionReceipt {
                selected_cpu: target,
                is_dedicated: is_isolated,
                package_id: pkg_id,
                core_id,
                smt_siblings: siblings,
                is_isolated,
                is_nohz_full: is_nohz,
                reason: CpuSelectionReason::ExplicitCli {
                    cpu: target,
                    in_cpuset: true,
                },
                housekeeping_cpus: final_housekeeping,
                topology,
            };

            log::info!(
                "🎯 CPU Selection: Explicit CLI core {} (cpuset allowed: true, isolated: {}, nohz_full: {})",
                receipt.selected_cpu,
                receipt.is_isolated,
                receipt.is_nohz_full
            );
            return receipt;
        } else {
            log::warn!(
                "⚠️ Requested CPU {} is NOT in the process cpuset allowed list ({:?}). Enforcing cpuset invariant and falling back to auto-selection.",
                target,
                allowed_cpus
            );
        }
    }

    // 2. Automatic Selection among candidates allowed by cpuset
    let candidates: Vec<&CpuCoreTopology> = topology.iter().filter(|t| t.in_cpuset).collect();

    if candidates.is_empty() {
        let fallback_cpu = online_cpus.first().copied().unwrap_or(0);
        let receipt = CpuSelectionReceipt {
            selected_cpu: fallback_cpu,
            is_dedicated: false,
            package_id: None,
            core_id: None,
            smt_siblings: vec![fallback_cpu],
            is_isolated: false,
            is_nohz_full: false,
            reason: CpuSelectionReason::ConservativeHeuristic {
                cpu: fallback_cpu,
                capacity: 1024,
                irq_count: 0,
                explanation: "No allowed CPU found in cpuset; total fallback to core 0",
            },
            housekeeping_cpus: vec![fallback_cpu],
            topology,
        };
        log::warn!(
            "⚠️ CPU Selection fallback: No candidates in cpuset, defaulting to CPU {}",
            fallback_cpu
        );
        return receipt;
    }

    // Look for proven isolated cores first
    let isolated_candidates: Vec<&CpuCoreTopology> = candidates
        .iter()
        .copied()
        .filter(|t| t.is_isolated)
        .collect();

    if let Some(&chosen) = isolated_candidates.iter().max_by(|a, b| {
        // Ranking for isolated cores:
        // 1. nohz_full priority (tickless reduces jitter)
        // 2. Primary SMT sibling priority
        // 3. Highest capacity
        // 4. Lowest IRQ load
        // 5. Index tiebreaker
        let a_primary = a.smt_siblings.first() == Some(&a.logical_id);
        let b_primary = b.smt_siblings.first() == Some(&b.logical_id);

        a.is_nohz_full
            .cmp(&b.is_nohz_full)
            .then_with(|| a_primary.cmp(&b_primary))
            .then_with(|| a.capacity.cmp(&b.capacity))
            .then_with(|| b.irq_count.cmp(&a.irq_count))
            .then_with(|| a.logical_id.cmp(&b.logical_id))
    }) {
        let housekeeping_cpus: Vec<usize> = allowed_cpus
            .iter()
            .copied()
            .filter(|&c| c != chosen.logical_id && !chosen.smt_siblings.contains(&c))
            .collect();
        let final_housekeeping = if housekeeping_cpus.is_empty() {
            allowed_cpus.clone()
        } else {
            housekeeping_cpus
        };

        let receipt = CpuSelectionReceipt {
            selected_cpu: chosen.logical_id,
            is_dedicated: true,
            package_id: chosen.package_id,
            core_id: chosen.core_id,
            smt_siblings: chosen.smt_siblings.clone(),
            is_isolated: true,
            is_nohz_full: chosen.is_nohz_full,
            reason: CpuSelectionReason::FullyIsolated {
                cpu: chosen.logical_id,
                package_id: chosen.package_id,
                core_id: chosen.core_id,
                smt_siblings: chosen.smt_siblings.clone(),
                nohz_full: chosen.is_nohz_full,
            },
            housekeeping_cpus: final_housekeeping,
            topology,
        };

        log::info!(
            "🧠 CPU Selection: Proven dedicated/isolated core {} (package: {:?}, core_id: {:?}, siblings: {:?}, nohz_full: {})",
            receipt.selected_cpu,
            receipt.package_id,
            receipt.core_id,
            receipt.smt_siblings,
            receipt.is_nohz_full
        );
        return receipt;
    }

    // Conservative Heuristic when no cores are proven isolated
    let chosen = candidates
        .iter()
        .copied()
        .max_by(|a, b| {
            // Ranking:
            // 1. Highest capacity (P-core vs E-core)
            // 2. Primary SMT sibling
            // 3. Lowest IRQ load
            // 4. Highest logical ID (deterministic)
            let a_primary = a.smt_siblings.first() == Some(&a.logical_id);
            let b_primary = b.smt_siblings.first() == Some(&b.logical_id);

            a.capacity
                .cmp(&b.capacity)
                .then_with(|| a_primary.cmp(&b_primary))
                .then_with(|| b.irq_count.cmp(&a.irq_count))
                .then_with(|| a.logical_id.cmp(&b.logical_id))
        })
        .cloned()
        .unwrap_or_else(|| candidates[0].clone());

    let housekeeping_cpus: Vec<usize> = allowed_cpus
        .iter()
        .copied()
        .filter(|&c| c != chosen.logical_id && !chosen.smt_siblings.contains(&c))
        .collect();
    let final_housekeeping = if housekeeping_cpus.is_empty() {
        allowed_cpus.clone()
    } else {
        housekeeping_cpus
    };

    let receipt = CpuSelectionReceipt {
        selected_cpu: chosen.logical_id,
        is_dedicated: false,
        package_id: chosen.package_id,
        core_id: chosen.core_id,
        smt_siblings: chosen.smt_siblings.clone(),
        is_isolated: false,
        is_nohz_full: chosen.is_nohz_full,
        reason: CpuSelectionReason::ConservativeHeuristic {
            cpu: chosen.logical_id,
            capacity: chosen.capacity,
            irq_count: chosen.irq_count,
            explanation: "Highest capacity with lowest IRQ load and SMT primary preference (non-isolated)",
        },
        housekeeping_cpus: final_housekeeping,
        topology,
    };

    log::info!(
        "🧠 CPU Selection: Conservative heuristic core {} (capacity: {}, irqs: {}, package: {:?}, core_id: {:?}, siblings: {:?})",
        receipt.selected_cpu,
        chosen.capacity,
        chosen.irq_count,
        receipt.package_id,
        receipt.core_id,
        receipt.smt_siblings
    );
    receipt
}

/// Selects the ideal CPU core to pin the RT thread (core affinity), returning the full selection receipt.
pub fn select_optimal_cpu_with_receipt(
    requested_cpu: Option<usize>,
) -> Option<CpuSelectionReceipt> {
    let source = SystemSysfsSource;
    Some(select_cpu_with_source(requested_cpu, &source))
}

/// Selects the ideal CPU core to pin the RT thread (core affinity).
///
/// Backward-compatible wrapper returning only the selected CPU index.
pub fn select_optimal_cpu() -> Option<usize> {
    select_optimal_cpu_with_receipt(None).map(|r| r.selected_cpu)
}

#[cfg(test)]
#[path = "affinity_test.rs"]
mod affinity_test;
