// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Helper functions executed inside the capture stream's `process()` RT callback.
//!
//! All functions in this module follow the absolute callback rules:
//! - Zero heap allocation
//! - Zero I/O
//! - Zero mutexes

use neural_amp_modeler_rs::common::spsc::RtStatusFlags;

mod cabsim_swap;
mod commands;
mod process;
mod rate_sync;
mod resampler_swap;

/// Offline RT swap-stress harness (T2.6 / ER-2), available only under the
/// `testing` feature. Reproduces the full capture-callback drain sequence with
/// no PipeWire daemon so integration tests can soak concurrent swaps and run
/// the zero-allocation heap audit.
#[cfg(feature = "testing")]
pub mod harness;

pub use cabsim_swap::drain_cabsims;
pub use commands::{
    drain_os_engines, drain_slimmable_models, receive_commands, try_slimmable_rebuild,
};
pub use process::process_dsp_buffer;
pub(crate) use process::{handle_spa_pair_fail_closed, silence_available_datas};
pub use rate_sync::sync_rate;
pub use resampler_swap::drain_resamplers;

/// RT fatal flag raised when a panic is captured inside an RT callback closure
/// (F-RB-020 / T3.2).
///
/// Bit 31 is **not** defined by `NeuralAmpModeler-rs`'s `RtStatusFlags` (bits
/// `0..=30` are in use); this local constant aliases the free bit in the shared
/// `status_bits` `AtomicU64`, keeping the RT→Main silent signaling on the same
/// latch as every other RT status flag. The main control loop observes it
/// (`status::observe_rt_panic`) and drives the ordered teardown — never an
/// `abort` with a corrupted capture.
pub(crate) const RT_STATUS_PANIC_CAPTURED: u64 = 1 << 31;

/// Executes the body of an RT callback closure under `catch_unwind`, containing
/// any panic before it reaches the `pipewire` crate's `extern "C"` trampoline
/// (F-RB-020 / T3.2).
///
/// On a captured panic the fatal [`RT_STATUS_PANIC_CAPTURED`] flag is raised
/// and `false` is returned — the closure returns without processed audio and
/// the control loop observes the flag (< 100 ms), transitions the backend to
/// `Failed` and runs the ordered shutdown (thread-loop stop, GC drain,
/// `RecordingWorkerGuard::shutdown` finalizing the WAV). The unwind never
/// crosses the FFI boundary, so the process never aborts with a corrupt
/// capture.
///
/// `catch_unwind` itself performs **zero heap allocations on the success path**
/// (verified by the heap-audit gate); the closure body must uphold the RT
/// zero-alloc/zero-IO/zero-lock contract.
#[inline]
pub(crate) fn run_rt_callback_body<F>(rt_status: &RtStatusFlags, body: F) -> bool
where
    F: FnOnce() + std::panic::UnwindSafe,
{
    match std::panic::catch_unwind(body) {
        Ok(()) => true,
        Err(_) => {
            rt_status.set_flag(RT_STATUS_PANIC_CAPTURED);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neural_amp_modeler_rs::common::spsc::RtStatusFlags;

    #[test]
    #[cfg(feature = "testing")]
    fn rt_panic_captured_sets_fatal_flag_no_abort() {
        // F-RB-020 / T3.2: a panic inside an RT callback body is contained by
        // `catch_unwind` — it never propagates to the `pipewire` C trampoline
        // (no `abort`) and raises the fatal `RT_STATUS_PANIC_CAPTURED` latch
        // for the control loop to run the ordered teardown.
        let rt = RtStatusFlags::default();
        let ok = run_rt_callback_body(
            &rt,
            std::panic::AssertUnwindSafe(|| {
                panic!("injected RT callback panic");
            }),
        );
        assert!(!ok, "panicking body must report failure");
        assert!(
            rt.check_flag(RT_STATUS_PANIC_CAPTURED),
            "panic must raise the fatal RT flag"
        );
    }

    #[test]
    fn rt_callback_body_completes_without_flag() {
        let rt = RtStatusFlags::default();
        let mut executed = false;
        let ok = run_rt_callback_body(
            &rt,
            std::panic::AssertUnwindSafe(|| {
                executed = true;
            }),
        );
        assert!(ok, "non-panicking body must report success");
        assert!(executed, "body must run exactly once");
        assert!(
            !rt.check_flag(RT_STATUS_PANIC_CAPTURED),
            "no panic must not raise the fatal flag"
        );
    }

    #[test]
    #[cfg(all(feature = "testing", feature = "heap-audit"))]
    fn rt_callback_catch_unwind_wrapper_zero_alloc() {
        use neural_amp_modeler_rs::common::alloc_audit::{
            TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
        };
        let rt = RtStatusFlags::default();
        let (allocs, deallocs, reallocs) = {
            let _guard = TrackingGuard::new();
            for _ in 0..1000 {
                let ok = run_rt_callback_body(&rt, std::panic::AssertUnwindSafe(|| {}));
                assert!(ok);
            }
            (get_alloc_count(), get_dealloc_count(), get_realloc_count())
        };
        // Measured: alloc=0, dealloc=0, realloc=0 (1000 catch_unwind cycles).
        assert_eq!(allocs, 0, "catch_unwind wrapper allocated on RT: {allocs}");
        assert_eq!(
            deallocs, 0,
            "catch_unwind wrapper deallocated on RT: {deallocs}"
        );
        assert_eq!(
            reallocs, 0,
            "catch_unwind wrapper reallocated on RT: {reallocs}"
        );
    }
}
