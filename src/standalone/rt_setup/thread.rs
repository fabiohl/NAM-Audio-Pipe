// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Thread and process configuration for real-time operation.
//!
//! Applies CPU affinity, SCHED_FIFO, mlockall, DAZ/FTZ and THP disabling
//! to ensure deterministic execution of the DSP thread.

use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use std::ffi::CStr;
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

/// Injectable system abstraction for thread real-time configuration.
pub trait ThreadConfigurator {
    /// Enables Denormals-Are-Zero and Flush-To-Zero.
    fn set_daz_ftz(&self);

    /// Obtains the current thread ID (`libc::pthread_t`).
    fn current_thread_id(&self) -> libc::pthread_t;

    /// Sets the thread name. `name` must be a NUL-terminated C string (the
    /// `&CStr` type enforces this at compile time).
    fn set_thread_name(&self, thread_id: libc::pthread_t, name: &CStr) -> i32;

    /// Sets thread CPU affinity.
    fn set_thread_affinity(&self, thread_id: libc::pthread_t, cpuset: &libc::cpu_set_t) -> i32;

    /// Gets scheduling policy and parameters.
    fn get_sched_param(&self, thread_id: libc::pthread_t) -> Result<(i32, libc::sched_param), i32>;

    /// Sets scheduling policy and parameters.
    fn set_sched_param(
        &self,
        thread_id: libc::pthread_t,
        policy: i32,
        param: &libc::sched_param,
    ) -> i32;

    /// Gets current running CPU core ID.
    fn get_current_cpu(&self) -> i32;
}

/// Default system-backed thread configurator using libc.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemThreadConfigurator;

impl ThreadConfigurator for SystemThreadConfigurator {
    fn set_daz_ftz(&self) {
        unsafe {
            neural_amp_modeler_rs::math::common::set_daz_ftz();
        }
    }

    fn current_thread_id(&self) -> libc::pthread_t {
        unsafe { libc::pthread_self() }
    }

    fn set_thread_name(&self, thread_id: libc::pthread_t, name: &CStr) -> i32 {
        unsafe { libc::pthread_setname_np(thread_id, name.as_ptr()) }
    }

    fn set_thread_affinity(&self, thread_id: libc::pthread_t, cpuset: &libc::cpu_set_t) -> i32 {
        unsafe {
            libc::pthread_setaffinity_np(thread_id, std::mem::size_of::<libc::cpu_set_t>(), cpuset)
        }
    }

    fn get_sched_param(&self, thread_id: libc::pthread_t) -> Result<(i32, libc::sched_param), i32> {
        let mut policy = 0i32;
        let mut param = libc::sched_param { sched_priority: 0 };
        let ret = unsafe { libc::pthread_getschedparam(thread_id, &mut policy, &mut param) };
        if ret == 0 {
            Ok((policy, param))
        } else {
            Err(ret)
        }
    }

    fn set_sched_param(
        &self,
        _thread_id: libc::pthread_t,
        policy: i32,
        param: &libc::sched_param,
    ) -> i32 {
        let ret = unsafe { libc::sched_setscheduler(0, policy, param) };
        if ret == -1 {
            unsafe { *libc::__errno_location() }
        } else {
            0
        }
    }

    fn get_current_cpu(&self) -> i32 {
        unsafe { libc::sched_getcpu() }
    }
}

/// Configures the current DSP thread for real-time operation using the provided configurator.
///
/// Executed off the audio hot-path during PipeWire data-loop state transition before declaring readiness.
/// Applies:
///
/// 1. **DAZ/FTZ** — Enables Denormals-Are-Zero and Flush-To-Zero in the MXCSR register
///    to avoid FPU penalties on silence blocks ("death spiral").
/// 2. **Core Affinity** — Pins the thread to the ideal physical core via
///    `pthread_setaffinity_np`, avoiding core migration and L1/L2 cache misses.
/// 3. **Scheduler Policy** — Inspects the existing scheduler policy honestly:
///    - `SCHED_FIFO`: keeps FIFO, records confirmed priority, sets `RT_STATUS_RT_IS_FIFO`.
///    - `SCHED_RR`: legitimate PipeWire / RTKit RT policy; keeps RR, records confirmed priority,
///      clears `RT_STATUS_RT_IS_FIFO` (it is RR, not FIFO), does NOT force elevation to FIFO 88.
///    - `SCHED_OTHER` (or other non-RT): attempts elevation to `SCHED_FIFO 88`. If elevation fails,
///      records errno in `rt_sched_err` and reports policy honestly without panicking.
///
/// After configuring, publishes the result via `rt_status` (atomic flags):
/// - `rt_is_fifo`: `true` if `SCHED_FIFO` was obtained.
/// - `rt_policy`: effective policy (`SCHED_FIFO`, `SCHED_RR`, or other).
/// - `rt_priority` / `confirmed_priority`: effective priority granted by the kernel.
/// - `rt_tid`: thread ID (kernel TID / pthread ID).
/// - `rt_cpu`: physical CPU core where the thread is running.
#[cold]
#[inline(never)]
pub fn configure_realtime_thread_with<C: ThreadConfigurator>(
    target_cpu: usize,
    rt_status: &RtStatusFlags,
    cfg: &C,
) {
    cfg.set_daz_ftz();

    let thread_id = cfg.current_thread_id();
    cfg.set_thread_name(thread_id, c"nam_pipe_dsp");

    pin_thread_affinity_with(thread_id, target_cpu, rt_status, cfg);

    let actual_cpu = cfg.get_current_cpu();
    rt_status.rt_cpu.store(actual_cpu, Ordering::Relaxed);
    rt_status.rt_tid.store(thread_id as i64, Ordering::Relaxed);

    let (actual_policy, actual_param) = match cfg.get_sched_param(thread_id) {
        Ok((p, param)) => {
            let base_policy = p & !0x40000000i32;
            if base_policy == libc::SCHED_FIFO || base_policy == libc::SCHED_RR {
                (base_policy, param)
            } else {
                // Thread is in SCHED_OTHER (or other non-RT). Attempt direct RT elevation to SCHED_FIFO 88.
                let target_param = libc::sched_param { sched_priority: 88 };
                let ret_set = cfg.set_sched_param(thread_id, libc::SCHED_FIFO, &target_param);
                if ret_set == 0 {
                    (libc::SCHED_FIFO, target_param)
                } else {
                    rt_status.rt_sched_err.store(ret_set, Ordering::Relaxed);
                    (base_policy, param)
                }
            }
        }
        Err(ret_getsched) => {
            rt_status
                .rt_getsched_err
                .store(ret_getsched, Ordering::Relaxed);
            (-1, libc::sched_param { sched_priority: -1 })
        }
    };

    if actual_policy == libc::SCHED_FIFO {
        rt_status.set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
    } else {
        rt_status.clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO);
    }

    rt_status.rt_priority.store(
        if actual_policy == -1 {
            0
        } else {
            actual_param.sched_priority
        },
        Ordering::Relaxed,
    );
    rt_status
        .confirmed_priority
        .store(actual_param.sched_priority, Ordering::Relaxed);
    rt_status.rt_policy.store(actual_policy, Ordering::Relaxed);
}

/// Configures the current DSP thread for real-time operation using the default `SystemThreadConfigurator`.
#[cold]
#[inline(never)]
pub fn configure_realtime_thread(target_cpu: usize, rt_status: Arc<RtStatusFlags>) {
    configure_realtime_thread_with(target_cpu, &rt_status, &SystemThreadConfigurator);
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

/// Pins `thread_id` to `target_cpu` using `cfg`, recording the outcome atomically in
/// `rt_status` (RT-safe: no logging or allocation on this path).
///
/// Out-of-range CPUs are rejected before any syscall and recorded as
/// `rt_affinity_err = -1`; kernel rejections record the errno.
pub(crate) fn pin_thread_affinity_with<C: ThreadConfigurator>(
    thread_id: libc::pthread_t,
    target_cpu: usize,
    rt_status: &RtStatusFlags,
    cfg: &C,
) {
    let Some(cpuset) = build_cpu_affinity_mask(target_cpu) else {
        rt_status.rt_affinity_err.store(-1, Ordering::Relaxed);
        rt_status
            .rt_target_cpu
            .store(target_cpu as i32, Ordering::Relaxed);
        return;
    };

    let ret_aff = cfg.set_thread_affinity(thread_id, &cpuset);

    if ret_aff != 0 {
        rt_status.rt_affinity_err.store(ret_aff, Ordering::Relaxed);
        rt_status
            .rt_target_cpu
            .store(target_cpu as i32, Ordering::Relaxed);
    }
}
