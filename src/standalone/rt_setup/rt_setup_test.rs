// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Pipe-specific unit tests for the `rt_setup` module.
//!
//! The affinity mask construction, typed pinning errors and scheduler-policy
//! invariants live in the engine's `rt_hardening` test suite (single source of
//! truth); this file keeps only the PipeWire-specific integration points:
//! the threaded bring-up wrapper and the RT-callback log mark.

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
    // The function must not panic and must record the rejection atomically
    // (rt_affinity_err = -1 sentinel, rt_target_cpu preserved).
    configure_realtime_thread(2048, rt_status.clone());
    assert_eq!(
        rt_status
            .rt_affinity_err
            .load(std::sync::atomic::Ordering::Relaxed),
        -1
    );
    assert_eq!(
        rt_status
            .rt_target_cpu
            .load(std::sync::atomic::Ordering::Relaxed),
        2048
    );
}

#[test]
fn test_configure_realtime_thread_cpu_zero_pins() {
    // SAFETY: `pthread_self` has no preconditions and queries the current thread ID.
    let thread_id = unsafe { libc::pthread_self() };
    // SAFETY: zeroing `cpu_set_t` bitmask is well-defined and represents an empty CPU set.
    let mut allowed = unsafe { std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed().assume_init() };
    // SAFETY: `thread_id` is the current thread and `allowed` points to a valid `cpu_set_t`.
    let ret = unsafe {
        libc::pthread_getaffinity_np(
            thread_id,
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut allowed,
        )
    };
    assert_eq!(
        ret, 0,
        "pthread_getaffinity_np must succeed on the test thread"
    );
    // Skip when the environment (e.g. a restricted cpuset) forbids CPU 0.
    // SAFETY: `allowed` is fully initialized and CPU index 0 is within bounds.
    if !unsafe { libc::CPU_ISSET(0, &allowed) } {
        return;
    }

    let rt_status = std::sync::Arc::new(neural_amp_modeler_rs::common::spsc::RtStatusFlags::new());
    configure_realtime_thread(0, rt_status.clone());
    assert_eq!(
        rt_status
            .rt_affinity_err
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "pinning to CPU 0 must succeed when the cpuset allows it"
    );

    // SAFETY: zeroing `cpu_set_t` bitmask is well-defined and represents an empty CPU set.
    let mut after = unsafe { std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed().assume_init() };
    // SAFETY: `thread_id` is the current thread and `after` points to a valid `cpu_set_t`.
    let ret = unsafe {
        libc::pthread_getaffinity_np(
            thread_id,
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut after,
        )
    };
    assert_eq!(ret, 0);
    // SAFETY: `after` is fully initialized and CPU index 0 is within bounds.
    assert!(
        unsafe { libc::CPU_ISSET(0, &after) },
        "thread must be pinned to CPU 0"
    );
}

#[test]
fn test_configure_realtime_thread_max_cpu_boundary() {
    let max_cpu = libc::CPU_SETSIZE as usize - 1;
    let rt_status = std::sync::Arc::new(neural_amp_modeler_rs::common::spsc::RtStatusFlags::new());
    // The boundary index is representable in the affinity mask; on hosts with
    // fewer than CPU_SETSIZE online CPUs the kernel rejects the mask with
    // EINVAL (or EPERM when the cpuset forbids it) — the function must record
    // the outcome without panicking or corrupting the mask.
    configure_realtime_thread(max_cpu, rt_status.clone());
    let err = rt_status
        .rt_affinity_err
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        err == 0 || err == libc::EINVAL || err == libc::EPERM,
        "boundary CPU pinning must succeed or be rejected with EINVAL/EPERM, got errno {err}"
    );
}

#[test]
fn test_configure_realtime_thread_arms_rt_callback_mark() {
    let rt_status = std::sync::Arc::new(neural_amp_modeler_rs::common::spsc::RtStatusFlags::new());

    // The thread running the configuration hook is the RT callback thread:
    // the log handler consults the mark to route this thread's PW log events
    // off its context. Arming follows the scheduler step and is independent of
    // the policy outcome.
    rt_log_mark::rt_unmark_current_thread();
    configure_realtime_thread(2, rt_status);
    assert!(rt_log_mark::rt_is_current_thread_marked());

    // Cleanup: leave no mark residue on the shared runner thread.
    rt_log_mark::rt_unmark_current_thread();
    assert!(!rt_log_mark::rt_is_current_thread_marked());
}
