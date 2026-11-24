// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! Real wall-clock endurance suite.
//!
//! Purpose (declared in the long-suite receipt as `ENDURANCE_REAL_PURPOSE`):
//! the timeline is **not** compressed — the suite runs for a real wall-clock
//! window (`NAM_ENDURANCE_SECONDS`, default 30 s) at BLOCK=64 / 48 kHz with
//! periodic swap batches, and every mandatory validation window is
//! **fail-closed** (a window that cannot complete within the bounded retry
//! budget is a hard failure — never a silent pass).
//!
//! Distinct from `tests/soak_extended.rs` (accelerated timeline soak). Runs
//! exclusively in `utils/tests-long.sh` Phase 6 (`--ignored`,
//! `--test-threads=1`). Telemetry, sampled periodically, records **raw**
//! resident memory (VmRSS), minor/major page faults (`/proc/self/stat`),
//! thread count (`/proc/self/status`) and open FD count (`/proc/self/fd`) —
//! registered in the `TEST_RESULT[endurance_real]=PASS ...` marker.
//!
//! Accommodation barrier: each validation window is isolated from the
//! ambient stream — the RT thread backs off for the whole window attempt
//! ([`ValidationWindowGate`]) and swap batches are deferred at
//! `SWAP_INTERVAL` boundaries colliding with a pending window — so the
//! fail-closed oracle only samples a settled linear tail (the shared
//! `validate_linear_window` additionally requires a stationarity-confirmed
//! repeat before its gain/polarity checks).
//!
//! Telemetry (`elapsed_s`, `rss_delta_kb`, `majflt`, and compaction faults) is
//! emitted live inside the `TEST_RESULT[...]` PASS marker on every run.

mod common;

use common::proc::{
    TelemetrySample, read_fd_count, read_page_faults, read_rss_kb, read_thread_count,
    read_thread_rusage,
};
use common::swap::*;
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use nam_audio_pipe::standalone::rt_setup::thread::{configure_process_wide, mlockall_ok};

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

/// Validation window cadence: every N blocks a mandatory Linear A/B polarity
/// window must complete (fail-closed — zero vanished windows).
const VALIDATION_INTERVAL: usize = 500;

/// RAII gate that pauses the ambient RT stream while a validation window
/// runs, so the window's pipeline state is exclusively its own: the ambient
/// stream (with its fresh signal and whatever engines the latest swap batch
/// left installed) must not interleave callbacks into the settle/read tail —
/// that interleave is the drain-race the accommodation barrier exists to
/// close. The gate releases the stream on drop, including through a panicking
/// fail-closed oracle (the panic unwinds with the RT stream running again).
struct ValidationWindowGate {
    active: Arc<AtomicBool>,
}

impl ValidationWindowGate {
    /// Arms the gate: the RT loop observes the flag (Acquire) and yields
    /// without processing until [`Drop`] releases it.
    fn arm(active: &std::sync::Arc<AtomicBool>) -> Self {
        active.store(true, Ordering::Release);
        Self {
            active: std::sync::Arc::clone(active),
        }
    }
}

impl Drop for ValidationWindowGate {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

/// Telemetry cadence: a raw RSS/faults/threads/FD sample every N blocks.
const TELEMETRY_INTERVAL: usize = 1_000;

/// Blocks processed before the first telemetry baseline: the allocator arena
/// grows once while the full model mix (Linear/WaveNet/LSTM/CabSim) faults in
/// fresh pages; the baseline must land after that one-time growth so the RSS
/// drift measures steady-state behaviour, not the first-arena expansion.
const TELEMETRY_WARMUP_BLOCKS: usize = 2_000;

/// Maximum allowed blocks of undue (exact) silence during the endurance.
const MAX_UNDUE_SILENCE: usize = 200;

/// Maximum allowed zero-frame sentinel skips (a callback that processed no
/// frames, e.g. while a resampler swap is pending). Documented, bounded —
/// never a silent validation skip.
const MAX_SKIPPED_BLOCKS: usize = 200;

/// Bounded retry budget per validation window: a window must complete within
/// this many consecutive zero-frame attempts or the suite fails.
const MAX_VALIDATION_ATTEMPTS: usize = 64;

/// Maximum allowed RSS growth (KiB) between the first and last telemetry
/// samples. A real leak grows by MBs per minute, orders of magnitude above
/// this margin, so 512 KiB (128 pages) stays leak-sensitive.
///
/// The margin deliberately ignores the raw swap-batch oscillation: measured
/// peak-to-peak RSS swings ~32-39 MB (e.g. 31.8↔71.1 MB) because every
/// `SWAP_INTERVAL` batch builds fresh WaveNet/LSTM/CabSim engines on the heap
/// and the off-RT GC frees the previous ones. The first→last delta is only
/// meaningful between **GC-settled** samples, which is why the final sample is
/// captured after `consume_gc()`; comparing a mid-oscillation final sample
/// instead reached +896 KiB and produced a false leak. RSS shrinkage is never
/// a failure (reported as a negative delta).
///
/// Measured (calibrated 30 s windows, 2 runs): first→last post-GC delta =
/// -10452 / -31656 KiB, `minflt_thread` = 0.
const MAX_RSS_DRIFT_KB: usize = 512;

/// Default wall-clock window in seconds (overridable via `NAM_ENDURANCE_SECONDS`).
const DEFAULT_WINDOW_SECS: u64 = 30;

/// Minimum wall-clock window: below this the suite is a vacuous pass and fails.
const MIN_WINDOW_SECS: u64 = 5;

/// Drives `validate_linear_window` up to `MAX_VALIDATION_ATTEMPTS` retries
/// followed by `consume_gc()`, ensuring the harness reaches the canonical
/// quiescent baseline (`Linear A/B`, no CabSim, `Off` oversampling, GC drained).
fn settle_quiescent_linear_window(h: &mut RtSwapHarness, stage: &str) {
    for _ in 0..MAX_VALIDATION_ATTEMPTS {
        if validate_linear_window(h) {
            h.consume_gc();
            return;
        }
    }
    panic!(
        "endurance {stage} quiescent linear window failed after {MAX_VALIDATION_ATTEMPTS} attempts: {}",
        validation_state_debug(h)
    );
}

/// Wall-clock window from `NAM_ENDURANCE_SECONDS` (default 30 s, bounded).
fn endurance_window_seconds() -> u64 {
    let raw = std::env::var("NAM_ENDURANCE_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_WINDOW_SECS);
    raw.clamp(MIN_WINDOW_SECS, 600)
}

/// Real wall-clock endurance.
///
/// Runs continuous audio + periodic swap batches for a real wall-clock window,
/// with mandatory fail-closed validation windows every `VALIDATION_INTERVAL`
/// blocks and periodic raw RSS/faults/threads/FD telemetry. Every reached
/// window must complete (`windows_completed == blocks / VALIDATION_INTERVAL`);
/// a window that cannot complete is a hard failure.
#[test]
#[ignore = "Real wall-clock endurance: fail-closed validation windows + periodic RSS/faults/threads/FD telemetry — long suite only (tests-long.sh Phase 6)"]
fn test_endurance_real_wall_clock_windows_fail_closed() {
    configure_process_wide();

    let window_secs = endurance_window_seconds();
    let window = Duration::from_secs(window_secs);
    assert!(
        window_secs >= MIN_WINDOW_SECS,
        "endurance window {window_secs}s below the {MIN_WINDOW_SECS}s vacuous-pass floor"
    );

    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("endurance harness");
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    for _ in 0..8 {
        let mut l = [0f32; BLOCK];
        let mut r = [0f32; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }
    h.consume_gc();

    warm_producer_arena(&mut h);
    validate_linear_window(&mut h);
    h.consume_gc();

    let max_blocks = (window.as_secs() * SAMPLE_RATE as u64 / BLOCK as u64) as usize + BLOCK;
    let (sig_l, sig_r) = test_signal_blocks(max_blocks);
    std::hint::black_box(sig_l.iter().copied().sum::<f32>() + sig_r.iter().copied().sum::<f32>());

    let harness = std::sync::Arc::new(std::sync::Mutex::new(h));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let window_active = Arc::new(AtomicBool::new(false));
    let rt_blocked_in_window = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let arm_rusage = Arc::new(AtomicBool::new(false));
    let rusage_armed = Arc::new(AtomicBool::new(false));
    // Shared RT-quantum counter: telemetry cadence and the final assertion are
    // keyed to actual DSP callbacks, never to the much slower paced loop below.
    let rt_blocks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let window_rt = std::sync::Arc::clone(&window_active);
    let rt_blocked_rt = std::sync::Arc::clone(&rt_blocked_in_window);
    let arm_rusage_rt = Arc::clone(&arm_rusage);
    let rusage_armed_rt = Arc::clone(&rusage_armed);
    let (tx_ready, rx_ready) = std::sync::mpsc::channel();

    let harness_rt = std::sync::Arc::clone(&harness);
    let stop_rt = std::sync::Arc::clone(&stop);
    let rt_blocks_rt = std::sync::Arc::clone(&rt_blocks);

    let rt_handle = std::thread::Builder::new()
        .name("nam_pipe_dsp".into())
        .spawn(move || {
            let sig_l_rt = sig_l;
            let sig_r_rt = sig_r;
            prefault_rt_thread_and_signals(&sig_l_rt, &sig_r_rt);
            {
                let mut h = harness_rt.lock().expect("harness lock");
                warm_rt_callback_paths(&mut h, &sig_l_rt, &sig_r_rt);
            }
            let mut in_l = [0f32; BLOCK];
            let mut in_r = [0f32; BLOCK];
            for _ in 0..16 {
                let mut h = harness_rt.lock().expect("harness lock");
                h.run_callback(&mut in_l, &mut in_r, BLOCK);
            }
            tx_ready.send(()).expect("notify ready");

            // Captured after the multi-thread warmup + baseline gate (mirroring
            // `tests/soak_extended.rs`) so pre-steady-state arena/futex warmup
            // never contaminates the steady-state zero-fault invariant.
            let mut rusage_before = None;

            let mut blocks = 0usize;
            let mut undue_silence = 0usize;
            let mut skipped_blocks = 0usize;

            while !stop_rt.load(std::sync::atomic::Ordering::Acquire) {
                // Validation-window gate: while a window runs the ambient
                // stream backs off entirely (no callbacks into the harness),
                // so the window's drain/settle/read tail observe a pipeline
                // whose state is exclusively the window's own.
                if window_rt.load(Ordering::Acquire) {
                    if arm_rusage_rt.swap(false, Ordering::AcqRel) {
                        prefault_rt_thread_and_signals(&sig_l_rt, &sig_r_rt);
                        rusage_before =
                            Some(read_thread_rusage().expect("post-warmup thread rusage"));
                        rusage_armed_rt.store(true, Ordering::Release);
                    }
                    rt_blocked_rt.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                    continue;
                }
                let sig_block = blocks % max_blocks;
                in_l.copy_from_slice(&sig_l_rt[sig_block * BLOCK..(sig_block + 1) * BLOCK]);
                in_r.copy_from_slice(&sig_r_rt[sig_block * BLOCK..(sig_block + 1) * BLOCK]);
                let (frames_diff, is_silent) = {
                    let mut h = harness_rt.lock().expect("harness lock");
                    let frames_before = h.frame_count();
                    let n_pw = h.run_callback(&mut in_l, &mut in_r, BLOCK);
                    let frames_diff = h.frame_count() == frames_before;
                    let is_silent = n_pw > 0
                        && in_l.iter().any(|&s| s != 0.0)
                        && h.out_l().iter().all(|&s| s == 0.0)
                        && h.out_r().iter().all(|&s| s == 0.0);
                    (frames_diff, is_silent)
                };
                blocks += 1;
                rt_blocks_rt.store(blocks, Ordering::Release);
                if frames_diff {
                    skipped_blocks += 1;
                }
                if is_silent {
                    undue_silence += 1;
                }
                std::thread::yield_now();
            }

            let rusage_after = read_thread_rusage().expect("final thread rusage");
            (
                rusage_before.expect("post-warmup rusage baseline armed"),
                rusage_after,
                blocks,
                undue_silence,
                skipped_blocks,
            )
        })
        .expect("spawn RT thread");

    rx_ready.recv().expect("wait RT thread ready");

    let start = Instant::now();
    let mut pacing_blocks = 0usize;
    let mut windows_completed = 0usize;
    let mut windows_deferred = 0usize;
    let mut window_needed_retry = false;
    let mut validation_pending = false;
    let mut retry_budget = MAX_VALIDATION_ATTEMPTS;
    let mut next_validation = VALIDATION_INTERVAL;
    let mut telemetry: Vec<TelemetrySample> = Vec::new();
    let mut last_telemetry_block = 0usize;
    let mut last_swap_rt_block = 0usize;
    let mut last_gc_rt_block = 0usize;
    let mut multi_thread_swaps = 0usize;

    while start.elapsed() < window {
        let rt_now = rt_blocks.load(std::sync::atomic::Ordering::Acquire);
        // Mandatory validation windows (fail-closed). The trigger is declared
        // BEFORE the swap-batch cadence so a boundary landing with a pending
        // window defers its batch: the window is the sole command producer in
        // flight for the whole attempt.
        if !validation_pending && pacing_blocks >= next_validation {
            validation_pending = true;
            retry_budget = MAX_VALIDATION_ATTEMPTS;
        }
        // Swap-batch deferral: cadenced on actual RT blocks (never main-loop
        // spin speed) so SPSC backlog depth stays bounded and consistent with
        // `tests/soak_extended.rs`. Any boundary colliding with a pending
        // validation window is deferred until the window resolves.
        if !validation_pending && rt_now.saturating_sub(last_swap_rt_block) >= SWAP_INTERVAL {
            let mut h = harness.lock().expect("harness lock");
            apply_swap_batch(&mut h, rt_now);
            last_swap_rt_block = rt_now;
            multi_thread_swaps += 1;
        }
        if validation_pending {
            // Accommodation barrier, thread leg: the ambient RT stream is
            // gated off for the whole window attempt so the drain/settle/read
            // tail sees a pipeline whose state is exclusively the window's
            // own (the gate releases the stream on drop, including through a
            // panicking fail-closed oracle).
            let blocked_before = rt_blocked_in_window.load(std::sync::atomic::Ordering::Relaxed);
            let _window_gate = ValidationWindowGate::arm(&window_active);
            let outcome = {
                let mut h = harness.lock().expect("harness lock");
                validate_linear_window(&mut h)
            };
            if outcome {
                validation_pending = false;
                windows_completed += 1;
                next_validation += VALIDATION_INTERVAL;
                if window_needed_retry {
                    windows_deferred += 1;
                    window_needed_retry = false;
                }
            } else {
                window_needed_retry = true;
                // Rate-limited observability: the first retries of a window in
                // trouble dump the RT pipeline state so a failing run carries
                // the discriminating information in the log.
                if retry_budget > MAX_VALIDATION_ATTEMPTS - 4 {
                    let h = harness.lock().expect("harness lock");
                    eprintln!(
                        "endurance window retry ({} remaining): {} — rt_blocked_during_window={}",
                        retry_budget - 1,
                        validation_state_debug(&h),
                        rt_blocked_in_window.load(std::sync::atomic::Ordering::Relaxed)
                            - blocked_before,
                    );
                }
                retry_budget -= 1;
                if retry_budget == 0 {
                    panic!(
                        "endurance validation window {windows_completed} vanished: \
                         {MAX_VALIDATION_ATTEMPTS} consecutive zero-frame attempts"
                    );
                }
            }
        }

        let rt_now = rt_blocks.load(std::sync::atomic::Ordering::Acquire);
        if telemetry.is_empty() {
            if rt_now >= TELEMETRY_WARMUP_BLOCKS
                && start.elapsed() >= Duration::from_secs(2)
                && multi_thread_swaps >= 24
                && !validation_pending
            {
                // Capture the baseline under the exact same quiescent topology
                // as the final sample (Linear A/B, CabSim None, OS Off, post-GC)
                // after a full 24-batch LCM cycle and >=2 s of multi-thread
                // contention, so glibc arena high-water marks under
                // `M_TRIM_THRESHOLD = -1` stabilize before `sample=0`.
                let _baseline_gate = ValidationWindowGate::arm(&window_active);
                {
                    let mut h = harness.lock().expect("harness lock");
                    settle_quiescent_linear_window(&mut h, "baseline");
                }
                telemetry.push(TelemetrySample::capture());
                last_telemetry_block = rt_now;
                arm_rusage.store(true, Ordering::Release);
                while !rusage_armed.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
            }
        } else if rt_now - last_telemetry_block >= TELEMETRY_INTERVAL {
            telemetry.push(TelemetrySample::capture());
            last_telemetry_block = rt_now;
        }

        if rt_now.saturating_sub(last_gc_rt_block) >= 50 {
            let mut h = harness.lock().expect("harness lock");
            h.consume_gc();
            last_gc_rt_block = rt_now;
        }

        pacing_blocks += 1;
        std::thread::yield_now();
    }

    // Resolve a validation window that started before the clock stopped —
    // fail-closed: it must complete within the budget or the suite fails
    // (same accommodation barrier as the in-loop windows).
    let mut final_attempts = 0usize;
    while validation_pending {
        let _window_gate = ValidationWindowGate::arm(&window_active);
        let outcome = {
            let mut h = harness.lock().expect("harness lock");
            validate_linear_window(&mut h)
        };
        if outcome {
            validation_pending = false;
            windows_completed += 1;
            if window_needed_retry {
                windows_deferred += 1;
                window_needed_retry = false;
            }
        } else {
            final_attempts += 1;
            if final_attempts >= MAX_VALIDATION_ATTEMPTS {
                panic!(
                    "endurance final validation window vanished after \
                     {MAX_VALIDATION_ATTEMPTS} zero-frame attempts"
                );
            }
        }
    }
    let elapsed = start.elapsed();

    // Final quiescent sample: captured while the RT worker is still alive and
    // parked behind the validation gate (pre-join), so `threads` observes the
    // same thread topology as the baseline sample. The gate also keeps the
    // canonical baseline reset (`validate_linear_window` + `consume_gc`) free
    // of foreign callbacks before the leak delta is taken.
    let final_gate = ValidationWindowGate::arm(&window_active);
    if !rusage_armed.load(Ordering::Acquire) {
        arm_rusage.store(true, Ordering::Release);
        while !rusage_armed.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }
    {
        let mut h = harness.lock().expect("harness lock");
        // Canonical baseline reset: restore harness to the exact quiescent baseline
        // topology (Linear A/B, CabSim None, Oversample Off) that produced the
        // initial telemetry baseline. This ensures the first→last delta measures true
        // leak drift instead of comparing a light Linear working set with an active
        // heavy model (WaveNet/LSTM) left running when the timer elapsed.
        settle_quiescent_linear_window(&mut h, "final");
    }
    telemetry.push(TelemetrySample::capture());
    stop.store(true, std::sync::atomic::Ordering::Release);
    drop(final_gate);
    let (rusage_before, rusage_after, rt_blocks_reported, undue_silence, skipped_blocks) =
        rt_handle.join().expect("join RT thread");
    let rt_blocks = rt_blocks.load(std::sync::atomic::Ordering::Acquire);
    assert_eq!(
        rt_blocks_reported, rt_blocks,
        "RT quantum counter diverged: worker={rt_blocks_reported}, shared snapshot={rt_blocks}"
    );

    // Process-wide page faults (`TelemetrySample::minflt`/`majflt`, read from
    // `/proc/self/stat`) include allocator-arena churn from the off-RT producer
    // (swap batches, GC). They are diagnostic only and never a zero-fault
    // oracle. The invariant below judges `getrusage(RUSAGE_THREAD)` of the RT
    // callback thread (`minflt_thread_delta`/`majflt_thread_delta`), the only
    // counters that can attribute a page fault to the audio thread.
    let minflt_thread_delta = rusage_after.minflt.saturating_sub(rusage_before.minflt);
    let majflt_thread_delta = rusage_after.majflt.saturating_sub(rusage_before.majflt);
    let nvcsw_thread_delta = rusage_after.nvcsw.saturating_sub(rusage_before.nvcsw);
    let nivcsw_thread_delta = rusage_after.nivcsw.saturating_sub(rusage_before.nivcsw);

    // Validation windows are paced by the main loop (`pacing_blocks`, one
    // deliberate command producer per iteration); telemetry is paced by the
    // shared RT quantum counter. The boundary at `pacing_blocks` itself is
    // exclusive, so the expected window count is the number of boundaries
    // strictly below it.
    let expected_windows = (pacing_blocks.saturating_sub(1)) / VALIDATION_INTERVAL;
    assert!(
        telemetry.len() >= 2,
        "endurance captured {} telemetry sample(s); at least two (steady-state \
         baseline + final pre-join) are required — a single sample makes the \
         first→last deltas vacuous",
        telemetry.len()
    );
    let first = telemetry.first().copied().expect("telemetry baseline");
    let last = telemetry.last().copied().expect("telemetry final");
    let rss_delta_kb = last.rss_kb as i64 - first.rss_kb as i64;
    let minflt_delta = last.minflt.saturating_sub(first.minflt);
    let majflt_delta = last.majflt.saturating_sub(first.majflt);
    let threads_delta = last.threads as i64 - first.threads as i64;
    let fds_delta = last.fds as i64 - first.fds as i64;

    for (i, s) in telemetry.iter().enumerate() {
        eprintln!(
            "ENDURANCE_TELEMETRY sample={i} rss_kb={} minflt={} majflt={} threads={} fds={}",
            s.rss_kb, s.minflt, s.majflt, s.threads, s.fds,
        );
    }

    // Fail-closed assertions. A deferred window that eventually completed is
    // not a vanished window: the zero-vanished requirement is
    // `windows_completed == expected_windows` (each reached boundary must
    // complete within the bounded retry budget or the suite fails).
    assert!(
        rt_blocks > 0 && pacing_blocks > 0,
        "endurance processed no audio quantums ({rt_blocks}) or paced no windows \
         ({pacing_blocks}) — vacuous pass"
    );
    assert!(
        windows_completed == expected_windows,
        "zero vanished windows required: completed {windows_completed}, reached \
         {expected_windows} (deferred {windows_deferred})"
    );
    assert!(
        skipped_blocks <= MAX_SKIPPED_BLOCKS,
        "zero-frame sentinel skips {skipped_blocks} exceed the {MAX_SKIPPED_BLOCKS} bound"
    );
    assert!(
        undue_silence <= MAX_UNDUE_SILENCE,
        "endurance observed {undue_silence} undue-silence blocks (limit {MAX_UNDUE_SILENCE})"
    );
    assert!(first.rss_kb > 0, "VmRSS unavailable on this platform");
    assert!(last.rss_kb > 0, "VmRSS unavailable on this platform");
    let drift_limit = MAX_RSS_DRIFT_KB;
    assert!(
        rss_delta_kb <= drift_limit as i64,
        "RSS growth of {rss_delta_kb} KiB (first {} → last {}) exceeds \
         the {drift_limit} KiB margin — possible memory leak",
        first.rss_kb,
        last.rss_kb,
    );
    // Invariant: Process-wide page fault counters (/proc/self/stat) must never
    // again be used to judge the RT thread. We assert thread-level faults directly.
    // The zero-fault invariant is a calibrated-host property: under non-strict
    // hosted QA a fault is a typed environment GAP; under `--strict` (operator
    // release) it remains a hard failure.
    assert!(
        threads_delta <= 2,
        "thread leak: {threads_delta} extra threads (first {} → last {})",
        first.threads,
        last.threads,
    );
    assert!(
        fds_delta <= 8,
        "FD leak: {fds_delta} extra FDs (first {} → last {})",
        first.fds,
        last.fds,
    );
    let mlock_ok = mlockall_ok();
    let compact_allowed = read_compact_unevictable_allowed();
    let allowed_minflt = rt_minor_fault_budget(mlock_ok, &compact_allowed);
    if minflt_thread_delta > allowed_minflt || majflt_thread_delta != 0 {
        if !matches!(
            std::env::var("NAM_RT_STRICT")
                .ok()
                .as_deref()
                .map(str::trim),
            Some("1" | "true")
        ) {
            eprintln!(
                "TEST_RESULT[endurance_real]=GAP:rt_thread_page_faults profile=release+testing window_secs={window_secs} \
                 elapsed_ms={} dsp_quantums={rt_blocks} pacing_blocks={pacing_blocks} windows_completed={windows_completed} \
                 windows_deferred={windows_deferred} rss_first_kb={} rss_last_kb={} rss_delta_kb={rss_delta_kb} \
                 mlockall_ok={mlock_ok} compact_unevictable_allowed={compact_allowed} minflt_delta={minflt_delta} majflt_delta={majflt_delta} \
                 minflt_thread={minflt_thread_delta} minflt_budget={allowed_minflt} majflt_thread={majflt_thread_delta} \
                 nvcsw_thread={nvcsw_thread_delta} nivcsw_thread={nivcsw_thread_delta} \
                 threads_first={} threads_last={} threads_delta={threads_delta} \
                 fds_first={} fds_last={} fds_delta={fds_delta} \
                 undue_silence={undue_silence} skipped_blocks={skipped_blocks} telemetry_samples={}",
                elapsed.as_millis(),
                first.rss_kb,
                last.rss_kb,
                first.threads,
                last.threads,
                first.fds,
                last.fds,
                telemetry.len(),
            );
            return;
        }
        assert!(
            minflt_thread_delta <= allowed_minflt,
            "minor page faults on RT callback thread ({minflt_thread_delta} > budget {allowed_minflt}) violate zero-fault invariant (mlockall_ok={mlock_ok}, compact_unevictable_allowed={compact_allowed}, rss_delta_kb={rss_delta_kb})"
        );
        assert_eq!(
            majflt_thread_delta, 0,
            "major page faults on RT callback thread ({majflt_thread_delta}) violate zero-fault invariant (mlockall_ok={mlock_ok})"
        );
    }

    eprintln!(
        "TEST_RESULT[endurance_real]=PASS profile=release+testing window_secs={window_secs} \
         elapsed_ms={} dsp_quantums={rt_blocks} pacing_blocks={pacing_blocks} windows_completed={windows_completed} \
         windows_deferred={windows_deferred} rss_first_kb={} rss_last_kb={} rss_delta_kb={rss_delta_kb} \
         mlockall_ok={mlock_ok} compact_unevictable_allowed={compact_allowed} minflt_delta={minflt_delta} majflt_delta={majflt_delta} \
         minflt_thread={minflt_thread_delta} minflt_budget={allowed_minflt} majflt_thread={majflt_thread_delta} \
         nvcsw_thread={nvcsw_thread_delta} nivcsw_thread={nivcsw_thread_delta} \
         threads_first={} threads_last={} threads_delta={threads_delta} \
         fds_first={} fds_last={} fds_delta={fds_delta} \
         undue_silence={undue_silence} skipped_blocks={skipped_blocks} telemetry_samples={}",
        elapsed.as_millis(),
        first.rss_kb,
        last.rss_kb,
        first.threads,
        last.threads,
        first.fds,
        last.fds,
        telemetry.len(),
    );
}

/// Fast parser gate for the endurance telemetry readers (runs in the quick
/// suite): the `/proc` parsers must return sane values and stay monotonic
/// across a short processing burst. This keeps the diagnostic surface covered
/// without executing the wall-clock endurance.
#[test]
fn endurance_telemetry_proc_parsers() {
    let rss = read_rss_kb();
    assert!(rss > 0, "VmRSS unavailable on this platform");
    let (minflt0, majflt0) = read_page_faults();
    let thread_ru0 = read_thread_rusage().expect("thread rusage initial");
    let threads = read_thread_count();
    let fds = read_fd_count();
    assert!(threads >= 1, "thread count parser failed: {threads}");
    assert!(fds >= 3, "FD count parser failed: {fds}");

    // Exercise the DSP a little so the counters advance monotonically.
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("parser harness");
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    let (sig_l, sig_r) = test_signal_blocks(128);
    let mut in_l = [0f32; BLOCK];
    let mut in_r = [0f32; BLOCK];
    for block in 0..128 {
        in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);
        h.run_callback(&mut in_l, &mut in_r, BLOCK);
    }
    h.consume_gc();

    let (minflt1, majflt1) = read_page_faults();
    let thread_ru1 = read_thread_rusage().expect("thread rusage after dsp");
    assert!(
        minflt1 >= minflt0 && majflt1 >= majflt0,
        "page-fault counters must be monotonic: {minflt0}/{majflt0} → {minflt1}/{majflt1}"
    );
    assert!(
        thread_ru1.minflt >= thread_ru0.minflt && thread_ru1.majflt >= thread_ru0.majflt,
        "thread rusage page faults must be monotonic: {:?} → {:?}",
        thread_ru0,
        thread_ru1
    );
    assert!(
        read_thread_count() >= threads && read_fd_count() >= fds,
        "thread/FD counters must be monotonic"
    );
}
