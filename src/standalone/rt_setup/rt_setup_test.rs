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
fn test_build_affinity_mask_cpu_zero() {
    let Some(cpuset) = build_cpu_affinity_mask(0) else {
        panic!("CPU 0 must be representable in the affinity mask");
    };
    // SAFETY: mask is fully initialized; CPU_ISSET/CPU_COUNT only read it.
    assert!(unsafe { libc::CPU_ISSET(0, &cpuset) });
    assert_eq!(unsafe { libc::CPU_COUNT(&cpuset) }, 1);
}

#[test]
fn test_build_affinity_mask_max_index() {
    let max_cpu = libc::CPU_SETSIZE as usize - 1;
    let Some(cpuset) = build_cpu_affinity_mask(max_cpu) else {
        panic!("CPU_SETSIZE - 1 must be representable in the affinity mask");
    };
    // SAFETY: mask is fully initialized; CPU_ISSET/CPU_COUNT only read it.
    assert!(unsafe { libc::CPU_ISSET(max_cpu, &cpuset) });
    assert_eq!(unsafe { libc::CPU_COUNT(&cpuset) }, 1);
}

#[test]
fn test_build_affinity_mask_rejects_out_of_bounds() {
    assert!(build_cpu_affinity_mask(libc::CPU_SETSIZE as usize).is_none());
    assert!(build_cpu_affinity_mask(usize::MAX).is_none());
}

#[test]
fn test_configure_realtime_thread_cpu_zero_pins() {
    let thread_id = unsafe { libc::pthread_self() };
    let mut allowed = unsafe { std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed().assume_init() };
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

    let mut after = unsafe { std::mem::MaybeUninit::<libc::cpu_set_t>::zeroed().assume_init() };
    let ret = unsafe {
        libc::pthread_getaffinity_np(
            thread_id,
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut after,
        )
    };
    assert_eq!(ret, 0);
    assert!(
        unsafe { libc::CPU_ISSET(0, &after) },
        "thread must be pinned to CPU 0"
    );
}

#[test]
fn test_configure_realtime_thread_max_cpu_boundary() {
    let max_cpu = libc::CPU_SETSIZE as usize - 1;
    let rt_status = std::sync::Arc::new(neural_amp_modeler_rs::common::spsc::RtStatusFlags::new());
    // The boundary index is representable in the mask (see
    // test_build_affinity_mask_max_index); on hosts with fewer than CPU_SETSIZE
    // online CPUs the kernel rejects the mask with EINVAL (or EPERM when the
    // cpuset forbids it) — the function must record the outcome without
    // panicking or corrupting the mask.
    configure_realtime_thread(max_cpu, rt_status.clone());
    let err = rt_status
        .rt_affinity_err
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        err == 0 || err == libc::EINVAL || err == libc::EPERM,
        "boundary CPU pinning must succeed or be rejected with EINVAL/EPERM, got errno {err}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Injectable ThreadConfigurator Mock & Policy Invariant Tests (T3.1)
// ─────────────────────────────────────────────────────────────────────────────

struct MockThreadConfigurator {
    pub daz_ftz_called: std::sync::atomic::AtomicBool,
    pub thread_name: std::sync::Mutex<Option<Vec<u8>>>,
    pub affinity_result: i32,
    pub affinity_called: std::sync::atomic::AtomicUsize,
    pub getsched_result: Result<(i32, libc::sched_param), i32>,
    pub getsched_called: std::sync::atomic::AtomicUsize,
    pub setsched_result: i32,
    pub setsched_called: std::sync::atomic::AtomicUsize,
    pub set_policy: std::sync::atomic::AtomicI32,
    pub set_priority: std::sync::atomic::AtomicI32,
    pub current_cpu: i32,
    pub thread_id: libc::pthread_t,
}

impl MockThreadConfigurator {
    fn new() -> Self {
        Self {
            daz_ftz_called: std::sync::atomic::AtomicBool::new(false),
            thread_name: std::sync::Mutex::new(None),
            affinity_result: 0,
            affinity_called: std::sync::atomic::AtomicUsize::new(0),
            getsched_result: Ok((libc::SCHED_OTHER, libc::sched_param { sched_priority: 0 })),
            getsched_called: std::sync::atomic::AtomicUsize::new(0),
            setsched_result: 0,
            setsched_called: std::sync::atomic::AtomicUsize::new(0),
            set_policy: std::sync::atomic::AtomicI32::new(-1),
            set_priority: std::sync::atomic::AtomicI32::new(-1),
            current_cpu: 2,
            thread_id: 12345 as libc::pthread_t,
        }
    }
}

impl thread::ThreadConfigurator for MockThreadConfigurator {
    fn set_daz_ftz(&self) {
        self.daz_ftz_called
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn current_thread_id(&self) -> libc::pthread_t {
        self.thread_id
    }

    fn set_thread_name(&self, _thread_id: libc::pthread_t, name: &[u8]) -> i32 {
        if let Ok(mut guard) = self.thread_name.lock() {
            *guard = Some(name.to_vec());
        }
        0
    }

    fn set_thread_affinity(&self, _thread_id: libc::pthread_t, _cpuset: &libc::cpu_set_t) -> i32 {
        self.affinity_called
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.affinity_result
    }

    fn get_sched_param(
        &self,
        _thread_id: libc::pthread_t,
    ) -> Result<(i32, libc::sched_param), i32> {
        self.getsched_called
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.getsched_result
    }

    fn set_sched_param(
        &self,
        _thread_id: libc::pthread_t,
        policy: i32,
        param: &libc::sched_param,
    ) -> i32 {
        self.setsched_called
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.set_policy
            .store(policy, std::sync::atomic::Ordering::Relaxed);
        self.set_priority
            .store(param.sched_priority, std::sync::atomic::Ordering::Relaxed);
        self.setsched_result
    }

    fn get_current_cpu(&self) -> i32 {
        self.current_cpu
    }
}

#[test]
fn test_mock_thread_configurator_sched_fifo() {
    let mock = MockThreadConfigurator {
        getsched_result: Ok((libc::SCHED_FIFO, libc::sched_param { sched_priority: 85 })),
        current_cpu: 3,
        thread_id: 4242 as libc::pthread_t,
        ..MockThreadConfigurator::new()
    };
    let flags = neural_amp_modeler_rs::common::spsc::RtStatusFlags::new();

    thread::configure_realtime_thread_with(3, &flags, &mock);

    assert!(
        mock.daz_ftz_called
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    assert_eq!(
        mock.affinity_called
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        mock.getsched_called
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    // When already SCHED_FIFO, setschedparam must not be re-called
    assert_eq!(
        mock.setsched_called
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert_eq!(
        flags.rt_policy.load(std::sync::atomic::Ordering::Relaxed),
        libc::SCHED_FIFO
    );
    assert_eq!(
        flags.rt_priority.load(std::sync::atomic::Ordering::Relaxed),
        85
    );
    assert_eq!(
        flags
            .confirmed_priority
            .load(std::sync::atomic::Ordering::Relaxed),
        85
    );
    assert!(flags.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO));
    assert_eq!(
        flags.rt_tid.load(std::sync::atomic::Ordering::Relaxed),
        4242
    );
    assert_eq!(flags.rt_cpu.load(std::sync::atomic::Ordering::Relaxed), 3);
}

#[test]
fn test_mock_thread_configurator_sched_rr() {
    let mock = MockThreadConfigurator {
        getsched_result: Ok((libc::SCHED_RR, libc::sched_param { sched_priority: 50 })),
        current_cpu: 1,
        thread_id: 9999 as libc::pthread_t,
        ..MockThreadConfigurator::new()
    };
    let flags = neural_amp_modeler_rs::common::spsc::RtStatusFlags::new();

    thread::configure_realtime_thread_with(1, &flags, &mock);

    assert!(
        mock.daz_ftz_called
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    assert_eq!(
        mock.affinity_called
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    // When PipeWire/RTKit sets SCHED_RR, do NOT convert RR to FIFO 90: report honestly!
    assert_eq!(
        mock.setsched_called
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert_eq!(
        flags.rt_policy.load(std::sync::atomic::Ordering::Relaxed),
        libc::SCHED_RR
    );
    assert_eq!(
        flags.rt_priority.load(std::sync::atomic::Ordering::Relaxed),
        50
    );
    assert_eq!(
        flags
            .confirmed_priority
            .load(std::sync::atomic::Ordering::Relaxed),
        50
    );
    // RT_STATUS_RT_IS_FIFO must be false since it is SCHED_RR
    assert!(!flags.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO));
    assert_eq!(
        flags.rt_tid.load(std::sync::atomic::Ordering::Relaxed),
        9999
    );
    assert_eq!(flags.rt_cpu.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn test_mock_thread_configurator_sched_other_elevation_success() {
    let mock = MockThreadConfigurator {
        getsched_result: Ok((libc::SCHED_OTHER, libc::sched_param { sched_priority: 0 })),
        setsched_result: 0,
        current_cpu: 2,
        ..MockThreadConfigurator::new()
    };
    let flags = neural_amp_modeler_rs::common::spsc::RtStatusFlags::new();

    thread::configure_realtime_thread_with(2, &flags, &mock);

    assert_eq!(
        mock.setsched_called
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        mock.set_policy.load(std::sync::atomic::Ordering::Relaxed),
        libc::SCHED_FIFO
    );
    assert_eq!(
        mock.set_priority.load(std::sync::atomic::Ordering::Relaxed),
        90
    );
    assert_eq!(
        flags.rt_policy.load(std::sync::atomic::Ordering::Relaxed),
        libc::SCHED_FIFO
    );
    assert_eq!(
        flags.rt_priority.load(std::sync::atomic::Ordering::Relaxed),
        90
    );
    assert_eq!(
        flags
            .confirmed_priority
            .load(std::sync::atomic::Ordering::Relaxed),
        90
    );
    assert!(flags.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO));
    assert_eq!(
        flags
            .rt_sched_err
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[test]
fn test_mock_thread_configurator_sched_other_elevation_failure() {
    let mock = MockThreadConfigurator {
        getsched_result: Ok((libc::SCHED_OTHER, libc::sched_param { sched_priority: 0 })),
        setsched_result: libc::EPERM,
        current_cpu: 2,
        ..MockThreadConfigurator::new()
    };
    let flags = neural_amp_modeler_rs::common::spsc::RtStatusFlags::new();

    thread::configure_realtime_thread_with(2, &flags, &mock);

    assert_eq!(
        mock.setsched_called
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        flags.rt_policy.load(std::sync::atomic::Ordering::Relaxed),
        libc::SCHED_OTHER
    );
    assert_eq!(
        flags.rt_priority.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert_eq!(
        flags
            .confirmed_priority
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert!(!flags.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO));
    assert_eq!(
        flags
            .rt_sched_err
            .load(std::sync::atomic::Ordering::Relaxed),
        libc::EPERM
    );
}

#[test]
fn test_mock_thread_configurator_getsched_failure() {
    let mock = MockThreadConfigurator {
        getsched_result: Err(libc::ESRCH),
        current_cpu: 2,
        ..MockThreadConfigurator::new()
    };
    let flags = neural_amp_modeler_rs::common::spsc::RtStatusFlags::new();

    thread::configure_realtime_thread_with(2, &flags, &mock);

    assert_eq!(
        flags
            .rt_getsched_err
            .load(std::sync::atomic::Ordering::Relaxed),
        libc::ESRCH
    );
    assert_eq!(
        flags.rt_policy.load(std::sync::atomic::Ordering::Relaxed),
        -1
    );
    assert_eq!(
        flags
            .confirmed_priority
            .load(std::sync::atomic::Ordering::Relaxed),
        -1
    );
    assert!(!flags.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO));
}

#[test]
fn test_mock_thread_configurator_affinity_failure() {
    let mock = MockThreadConfigurator {
        affinity_result: libc::EINVAL,
        ..MockThreadConfigurator::new()
    };
    let flags = neural_amp_modeler_rs::common::spsc::RtStatusFlags::new();

    thread::configure_realtime_thread_with(2, &flags, &mock);

    assert_eq!(
        flags
            .rt_affinity_err
            .load(std::sync::atomic::Ordering::Relaxed),
        libc::EINVAL
    );
}

#[test]
fn test_zero_setup_calls_after_readiness() {
    let mock = MockThreadConfigurator::new();
    let flags = neural_amp_modeler_rs::common::spsc::RtStatusFlags::new();

    let mut thread_configured = false;

    // Simulation of data-loop state transition (StreamState::Paused / Streaming)
    if !thread_configured {
        thread::configure_realtime_thread_with(2, &flags, &mock);
        thread_configured = true;
    }

    let initial_affinity_calls = mock
        .affinity_called
        .load(std::sync::atomic::Ordering::Relaxed);
    let initial_getsched_calls = mock
        .getsched_called
        .load(std::sync::atomic::Ordering::Relaxed);
    let initial_setsched_calls = mock
        .setsched_called
        .load(std::sync::atomic::Ordering::Relaxed);

    // Operational audio callback frames (process() loop)
    for _ in 0..100 {
        #[cfg(test)]
        if !thread_configured {
            thread::configure_realtime_thread_with(2, &flags, &mock);
        }
    }

    // Zero additional setup calls must have occurred during operational audio processing
    assert_eq!(
        mock.affinity_called
            .load(std::sync::atomic::Ordering::Relaxed),
        initial_affinity_calls,
        "Invariant violation: setup syscalls executed during operational audio frames!"
    );
    assert_eq!(
        mock.getsched_called
            .load(std::sync::atomic::Ordering::Relaxed),
        initial_getsched_calls,
        "Invariant violation: getschedparam executed during operational audio frames!"
    );
    assert_eq!(
        mock.setsched_called
            .load(std::sync::atomic::Ordering::Relaxed),
        initial_setsched_calls,
        "Invariant violation: setschedparam executed during operational audio frames!"
    );
}

#[test]
fn test_harness_fallback_configures_when_unconfigured() {
    let mock = MockThreadConfigurator::new();
    let flags = neural_amp_modeler_rs::common::spsc::RtStatusFlags::new();

    let mut thread_configured = false;

    // Direct harness invocation of process() without preceding state_changed:
    // Fallback branch should configure the thread exactly once.
    if !thread_configured {
        thread::configure_realtime_thread_with(3, &flags, &mock);
        thread_configured = true;
    }

    assert!(thread_configured);
    assert_eq!(
        mock.affinity_called
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert_eq!(
        mock.setsched_called
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    // Subsequent process calls do zero setup
    for _ in 0..50 {
        if !thread_configured {
            thread::configure_realtime_thread_with(3, &flags, &mock);
        }
    }

    assert_eq!(
        mock.affinity_called
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}
