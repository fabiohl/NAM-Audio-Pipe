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

/// Configures the process for real-time operation (process-wide).
///
/// Delegates to the engine's `rt_hardening::{disable_thp, mlockall_current}`.
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
    // Engine equivalents log + return Result with graceful fallback (no panic
    // on the setup path); outcome is identical, telemetry unchanged.
    let _ = neural_amp_modeler_rs::rt_hardening::disable_thp();
    let _ = neural_amp_modeler_rs::rt_hardening::mlockall_current();
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
/// Delegates to the engine's opt-in `rt_hardening` module
/// (`neural_amp_modeler_rs::rt_hardening::{set_cpu_affinity, promote_sched_fifo}`)
/// for affinity + scheduler promotion, then applies the PipeWire-specific
/// thread name. The engine equivalents return `Result` where this wrapper
/// historically recorded errno silently — the outcome is identical (errno in
/// `rt_sched_err` / `rt_affinity_err`, no panic), now with log + graceful
/// fallback semantics owned by the engine.
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

    // Scheduler promotion via the engine (same honest-policy semantics:
    // keep FIFO/RR, elevate OTHER → FIFO 88, record errno without panic).
    // The engine helper re-reads DAZ/FTZ + CPU/TID (idempotent) and publishes
    // the identical `rt_status` telemetry, so delegate the policy block to it.
    let engine_cfg = EngineThreadAdapter { inner: cfg };
    let _ =
        neural_amp_modeler_rs::rt_hardening::promote_sched_fifo_with(88, rt_status, &engine_cfg);
}

/// Adapter bridging the local [`ThreadConfigurator`] to the engine's
/// `rt_hardening::ThreadConfigurator` (identical syscall surface minus the
/// PipeWire-specific thread naming, which stays local).
struct EngineThreadAdapter<'a, C: ThreadConfigurator> {
    inner: &'a C,
}

impl<C: ThreadConfigurator> neural_amp_modeler_rs::rt_hardening::ThreadConfigurator
    for EngineThreadAdapter<'_, C>
{
    fn set_daz_ftz(&self) {
        self.inner.set_daz_ftz();
    }
    fn current_thread_id(&self) -> libc::pthread_t {
        self.inner.current_thread_id()
    }
    fn set_thread_affinity(&self, thread_id: libc::pthread_t, cpuset: &libc::cpu_set_t) -> i32 {
        self.inner.set_thread_affinity(thread_id, cpuset)
    }
    fn get_sched_param(&self, thread_id: libc::pthread_t) -> Result<(i32, libc::sched_param), i32> {
        self.inner.get_sched_param(thread_id)
    }
    fn set_sched_param(
        &self,
        thread_id: libc::pthread_t,
        policy: i32,
        param: &libc::sched_param,
    ) -> i32 {
        self.inner.set_sched_param(thread_id, policy, param)
    }
    fn get_current_cpu(&self) -> i32 {
        self.inner.get_current_cpu()
    }
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
