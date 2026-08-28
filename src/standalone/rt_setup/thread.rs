// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Thread and process configuration for real-time operation.
//!
//! Applies CPU affinity, SCHED_FIFO, mlockall, DAZ/FTZ and THP disabling
//! to ensure deterministic execution of the DSP thread.

use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// `PR_THP_DISABLE_EXCEPT_ADVISED` (value 2) — introduced in Linux 7.0.
/// Not yet available in libc 0.2.186; defined locally for forward compatibility.
const PR_THP_DISABLE_EXCEPT_ADVISED: libc::c_ulong = 2;

/// Configures the process for real-time operation (process-wide).
///
/// Must be called from `main()` **after** all major heap allocations and
/// **before** starting the PipeWire DSP thread. Runs:
///
/// 1. **THP disable** — Disables Transparent Huge Pages via `prctl`, avoiding
///    background compaction latencies from khugepaged. Attempts the modern
///    `PR_THP_DISABLE_EXCEPT_ADVISED` mode (Linux 7.0+) first, falling back to
///    classic `PR_SET_THP_DISABLE` on older kernels.
/// 2. **mlockall** — Locks current and future memory in physical RAM, preventing
///    page faults in the DSP thread.
///
/// These operations were originally executed in the cold-path of the first DSP frame,
/// but were moved here to reduce jitter at the critical moment of the first
/// audio delivery.
pub fn configure_process_wide() {
    // 1. THP disable — tries the modern `PR_THP_DISABLE_EXCEPT_ADVISED`
    //    (Linux 7.0+) which allows pages explicitly marked with MADV_HUGEPAGE
    //    to use THP (e.g., hot-swapped models). Falls back gracefully to the
    //    classic global `PR_SET_THP_DISABLE` on older kernels.
    unsafe {
        let ret = libc::prctl(
            libc::PR_SET_THP_DISABLE,
            1,
            PR_THP_DISABLE_EXCEPT_ADVISED,
            0,
            0,
        );
        if ret == -1 && *libc::__errno_location() == libc::EINVAL {
            let err = std::io::Error::last_os_error();
            log::info!(
                "Kernel does not support PR_THP_DISABLE_EXCEPT_ADVISED (errno={}: {}) — \
                 falling back to classic PR_SET_THP_DISABLE.",
                err.raw_os_error().unwrap_or(-1),
                err,
            );
            let classic_ret = libc::prctl(libc::PR_SET_THP_DISABLE, 1, 0, 0, 0);
            if classic_ret == -1 {
                let fallback_err = std::io::Error::last_os_error();
                log::warn!(
                    "Classic PR_SET_THP_DISABLE also failed (errno={}: {}). \
                     THP may remain active — background compaction latencies possible.",
                    fallback_err.raw_os_error().unwrap_or(-1),
                    fallback_err,
                );
            } else {
                log::info!(
                    "Transparent Huge Pages globally disabled (classic fallback). \
                     Only MADV_HUGEPAGE regions may use THP."
                );
            }
        } else if ret == -1 {
            let err = std::io::Error::last_os_error();
            log::warn!(
                "prctl(PR_SET_THP_DISABLE) failed with unexpected errno={}: {}. \
                 THP state unknown — background compaction latencies possible.",
                err.raw_os_error().unwrap_or(-1),
                err,
            );
        }
    }

    let ret_mlock = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };

    if ret_mlock != 0 {
        let err = std::io::Error::last_os_error();
        log::warn!(
            "mlockall() failed ({}). Audio may experience dropouts if the system swaps.\n  Hint: Verify the 'memlock' limit in ulimits.",
            err
        );
    } else {
        log::info!("🔒 Memory Protection: Locked in physical RAM to prevent dropouts (mlockall).");
    }
}

/// Configures the current DSP thread for real-time operation.
///
/// Executed **only once** in the cold-path of the first `process()` callback frame,
/// before the data flow actually begins. Applies:
///
/// 1. **DAZ/FTZ** — Enables Denormals-Are-Zero and Flush-To-Zero in the MXCSR register
///    to avoid FPU penalties on silence blocks ("death spiral").
/// 2. **Core Affinity** — Pins the thread to the ideal physical core via
///    `pthread_setaffinity_np`, avoiding core migration and L1/L2 cache misses.
/// 3. **SCHED_FIFO** — Elevates priority to real-time scheduling (prio 90).
///
/// The process-wide operations (THP disable, mlockall) have been moved to
/// `configure_process_wide()`, called from `main()` before PipeWire.
///
/// After configuring, publishes the result via `rt_status` (atomic flags):
/// - `rt_is_fifo`: `true` if `SCHED_FIFO` was confirmed by `pthread_getschedparam`.
/// - `rt_priority`: effective priority granted by the kernel (or `0` if FIFO not obtained).
///
/// The main loop in `run_pipewire_host` reads these flags and emits an auditable
/// confirmation log (or warning) — **zero additional I/O inside the RT callback**.
#[cold]
#[inline(never)]
pub fn configure_realtime_thread(target_cpu: usize, rt_status: Arc<RtStatusFlags>) {
    unsafe {
        neural_amp_modeler_rs::math::common::set_daz_ftz();
    }

    let thread_id = unsafe { libc::pthread_self() };

    unsafe {
        let name = b"nam_pipe_dsp\0";
        libc::pthread_setname_np(thread_id, name.as_ptr() as *const libc::c_char);
    }

    pin_thread_affinity(thread_id, target_cpu, &rt_status);

    let mut actual_policy = 0i32;
    let mut actual_param = libc::sched_param { sched_priority: 0 };
    let ret_getsched =
        unsafe { libc::pthread_getschedparam(thread_id, &mut actual_policy, &mut actual_param) };

    let actual_cpu = unsafe { libc::sched_getcpu() };
    rt_status.rt_cpu.store(actual_cpu, Ordering::Relaxed);

    if ret_getsched == 0 {
        let mut base_policy = actual_policy & !0x40000000i32;

        if base_policy != libc::SCHED_FIFO {
            let param = libc::sched_param { sched_priority: 90 };

            let ret_sched =
                unsafe { libc::pthread_setschedparam(thread_id, libc::SCHED_FIFO, &param) };

            if ret_sched != 0 {
                rt_status.rt_sched_err.store(ret_sched, Ordering::Relaxed);
            } else {
                base_policy = libc::SCHED_FIFO;
                actual_param.sched_priority = 90;
            }
        }

        let confirmed_fifo = base_policy == libc::SCHED_FIFO;

        if confirmed_fifo {
            rt_status.set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
        } else {
            rt_status.clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
        }

        rt_status
            .rt_priority
            .store(actual_param.sched_priority, Ordering::Relaxed);
        rt_status
            .confirmed_priority
            .store(actual_param.sched_priority, Ordering::Relaxed);
        rt_status.rt_policy.store(base_policy, Ordering::Relaxed);
    } else {
        rt_status.clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
        rt_status.rt_priority.store(0, Ordering::Relaxed);
        rt_status.confirmed_priority.store(-1, Ordering::Relaxed);
        rt_status.rt_policy.store(-1, Ordering::Relaxed);

        rt_status
            .rt_getsched_err
            .store(ret_getsched, Ordering::Relaxed);
    }
}

/// Builds the `cpu_set_t` affinity mask that pins a thread to `target_cpu`.
///
/// Returns `None` when `target_cpu` is outside the `[0, CPU_SETSIZE)` index
/// range supported by `pthread_setaffinity_np`.
pub(crate) fn build_cpu_affinity_mask(target_cpu: usize) -> Option<libc::cpu_set_t> {
    if target_cpu >= libc::CPU_SETSIZE as usize {
        return None;
    }

    // SAFETY: on the supported Linux targets (glibc/musl) `cpu_set_t` is a C
    // bitmask whose all-zero bit pattern denotes the empty CPU set, so a
    // zero-initialized `cpu_set_t` is a fully valid value — no Rust reference
    // is formed over uninitialized storage.
    let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };

    // SAFETY: `CPU_ZERO`/`CPU_SET` only mutate the already-initialized bitmask
    // in place; the bounds check above guarantees libc's
    // `cpu / (8 * size_of::<u64>())` index stays within `cpu_set_t`'s
    // `[u64; 16]` storage.
    unsafe {
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(target_cpu, &mut cpuset);
    }

    Some(cpuset)
}

/// Pins `thread_id` to `target_cpu`, recording the outcome atomically in
/// `rt_status` (RT-safe: no logging or allocation on this path).
///
/// Out-of-range CPUs are rejected before any syscall and recorded as
/// `rt_affinity_err = -1`; kernel rejections record the errno.
fn pin_thread_affinity(thread_id: libc::pthread_t, target_cpu: usize, rt_status: &RtStatusFlags) {
    let Some(cpuset) = build_cpu_affinity_mask(target_cpu) else {
        rt_status.rt_affinity_err.store(-1, Ordering::Relaxed);
        rt_status
            .rt_target_cpu
            .store(target_cpu as i32, Ordering::Relaxed);
        return;
    };

    // SAFETY: `cpuset` is a fully initialized `cpu_set_t` (built above); the
    // mask size matches the type and the kernel consumes it synchronously,
    // never retaining the pointer.
    let ret_aff = unsafe {
        libc::pthread_setaffinity_np(thread_id, std::mem::size_of::<libc::cpu_set_t>(), &cpuset)
    };

    if ret_aff != 0 {
        rt_status.rt_affinity_err.store(ret_aff, Ordering::Relaxed);
        rt_status
            .rt_target_cpu
            .store(target_cpu as i32, Ordering::Relaxed);
    }
}
