// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

#[test]
fn test_get_allowed_cpus_not_empty() {
    let allowed = get_allowed_cpus();
    // On a normal Linux system, at least the current CPU should be allowed.
    assert!(
        !allowed.is_empty(),
        "The list of allowed CPUs must not be empty."
    );
}

#[test]
fn test_select_optimal_cpu_returns_something() {
    // select_optimal_cpu should return a valid core in the test environment.
    let cpu = select_optimal_cpu();
    assert!(cpu.is_some(), "Should be able to select an optimal core.");

    if let Some(cpu_idx) = cpu {
        let allowed = get_allowed_cpus();
        assert!(
            allowed.contains(&cpu_idx),
            "The selected core must be in the list of allowed CPUs."
        );
    }
}

#[test]
fn test_parse_interrupts_basic() {
    let irqs = parse_interrupts_per_cpu();
    if std::path::Path::new("/proc/interrupts").exists() {
        assert!(
            !irqs.is_empty(),
            "The map of parsed interrupts per CPU should not be empty on Linux."
        );
    }
}

#[test]
fn test_rdtsc_nanos_monotonic() {
    // Ensures time advances even in fallback
    let t1 = rdtsc_nanos();
    std::thread::sleep(std::time::Duration::from_millis(1));
    let t2 = rdtsc_nanos();
    assert!(t2 > t1, "Time did not advance: {} -> {}", t1, t2);
}

#[test]
fn test_rdtsc_nanos_significant() {
    // Ensures the returned time is not near zero (Instant::now().elapsed() issue)
    // Note: if the system is very fast, it can be small, but rdtsc_nanos
    // uses a static BOOT_TIME, so it should be at least a few microseconds
    // since the test binary started.
    std::thread::sleep(std::time::Duration::from_millis(5));
    let t = rdtsc_nanos();
    assert!(t > 1_000_000, "Reported time is too low: {} ns", t);
}

#[test]
fn test_configure_realtime_thread_invalid_cpu() {
    let rt_status = std::sync::Arc::new(neural_amp_modeler_rs::common::spsc::RtStatusFlags::new());
    // CPU_SETSIZE is typically 1024; 2048 is well out of bounds.
    // The function must not panic and must fall back to no-affinity.
    configure_realtime_thread(2048, rt_status);
}
