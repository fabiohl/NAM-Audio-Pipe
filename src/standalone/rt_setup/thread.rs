// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire-specific real-time thread bring-up glue.
//!
//! Process-wide setup (THP, `mlockall`), CPU affinity and `SCHED_FIFO`
//! promotion are the engine's host-agnostic `rt_hardening` API; this module
//! only supplies the PipeWire-specific pieces: the DSP data-thread name and the
//! RT-callback log mark, delegating everything else to the engine.

use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::rt_hardening::{
    SystemThreadConfigurator, ThreadConfigurator, promote_sched_fifo_with, set_cpu_affinity_with,
};
use std::sync::Arc;

/// Configures the process for real-time operation (process-wide).
///
/// Delegates to the engine's `rt_hardening::{disable_thp, mlockall_current}`.
/// Must be called from `main()` **after** all major heap allocations and
/// **before** starting the PipeWire DSP thread. Runs:
///
/// 1. **THP disable** — Disables Transparent Huge Pages via `prctl`, avoiding
///    background compaction latencies from khugepaged. The engine attempts the
///    modern `PR_THP_DISABLE_EXCEPT_ADVISED` mode (Linux 7.0+) first, falling
///    back to classic `PR_SET_THP_DISABLE` on older kernels.
/// 2. **mlockall** — Locks current and future memory in physical RAM, preventing
///    page faults in the DSP thread.
///
/// These operations were originally executed in the cold-path of the first DSP
/// frame, but were moved here to reduce jitter at the critical moment of the
/// first audio delivery.
pub fn configure_process_wide() {
    // Engine equivalents log + return Result with graceful fallback (no panic
    // on the setup path); outcome is identical, telemetry unchanged.
    let _ = neural_amp_modeler_rs::rt_hardening::disable_thp();
    match neural_amp_modeler_rs::rt_hardening::mlockall_current() {
        Ok(()) => log::info!("mlockall: ok"),
        Err(err) => {
            let errno = err.raw_os_error().unwrap_or(-1);
            log::warn!("mlockall: failed({errno})");
        }
    }
}

/// Sets the calling thread's name to `nam_pipe_dsp` (best-effort diagnostic
/// metadata for `htop`/`perf`; failures — e.g. permissions or length — are
/// non-fatal). The name is PipeWire-specific, so it stays outside the engine.
fn set_dsp_thread_name() {
    // SAFETY: `pthread_setname_np` expects a valid pthread id and a valid
    // NUL-terminated C string of at most 16 bytes; both hold here.
    unsafe {
        libc::pthread_setname_np(libc::pthread_self(), c"nam_pipe_dsp".as_ptr());
    }
}

/// Configures the current DSP thread for real-time operation using the provided
/// engine [`ThreadConfigurator`].
///
/// Thin PipeWire wrapper over the engine's opt-in `rt_hardening` module: it
/// applies the PipeWire-specific thread name and RT-callback mark, and delegates
/// affinity pinning (`set_cpu_affinity_with`) and scheduler promotion
/// (`promote_sched_fifo_with`) to the engine — which owns the honest-policy
/// semantics (keep FIFO/RR, elevate `SCHED_OTHER` to FIFO 88, record errno in
/// `rt_sched_err` / `rt_affinity_err` without panicking) and publishes the
/// identical `rt_status` telemetry.
///
/// Executed on the RT data thread, inside its **first** `process()` quantum
/// (one-time, `#[cold]`). The PipeWire `state_changed` / `param_changed`
/// listeners run on the PipeWire thread-loop (main-loop) thread, while
/// `process()` runs on a dedicated data thread ("data-loop.0"); the first
/// `process()` invocation is therefore the **only** consumer-owned hook on the
/// RT data thread, and the one-time setup must stay there — moving it to
/// `state_changed` would configure the wrong thread. The setup cost is bounded,
/// one-time and already hoisted off the RT path wherever the target is not the
/// data thread (`configure_process_wide` runs in `main()`).
///
/// After configuring, the engine publishes the result via `rt_status` atomics
/// (`rt_is_fifo`, `rt_policy`, `rt_priority` / `confirmed_priority`, `rt_tid`,
/// `rt_cpu`).
#[cold]
#[inline(never)]
pub fn configure_realtime_thread_with<C: ThreadConfigurator>(
    target_cpu: usize,
    rt_status: &RtStatusFlags,
    cfg: &C,
) {
    cfg.set_daz_ftz();
    set_dsp_thread_name();

    let _ = set_cpu_affinity_with(target_cpu, rt_status, cfg);
    let _ = promote_sched_fifo_with(88, rt_status, cfg);

    // Arm the RT-callback-thread mark consulted by the PipeWire log handler
    // (`pw_host::log_redirect`): the first `process()` quantum is the only
    // consumer-owned hook on the data thread, so arming here — regardless of
    // the honest-policy outcome (FIFO, RR or fallback OTHER) — remains the
    // faithful identity point for "this thread must offload its log events off
    // its context". The mark dies with the thread; the next instance's fresh
    // data thread re-arms in its own first quantum. One TLS store, once, on the
    // cold setup path: no cost on the steady-state hot path.
    let _ = crate::standalone::rt_setup::rt_log_mark::rt_mark_current_thread();
}

/// Configures the current DSP thread for real-time operation using the default
/// engine `SystemThreadConfigurator`.
#[cold]
#[inline(never)]
pub fn configure_realtime_thread(target_cpu: usize, rt_status: Arc<RtStatusFlags>) {
    configure_realtime_thread_with(target_cpu, &rt_status, &SystemThreadConfigurator);
}
