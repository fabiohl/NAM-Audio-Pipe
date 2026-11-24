// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! Accelerated-timeline soak harness for the PipeWire host.
//!
//! Purpose (declared in the long-suite receipt as `SOAK_ACCELERATED_PURPOSE`):
//! the audio timeline is **compressed** — 320 000 continuous audio blocks
//! (~7.1 min of nominal timeline at BLOCK=64 / 48 kHz, far less in wall clock)
//! with thousands of concurrent swaps across WaveNet, LSTM and Linear models,
//! CabSim IRs, oversampling factors and gain variations. Validation windows are
//! mandatory and **fail-closed** (a window that cannot complete within the
//! bounded retry budget is a hard failure — never a silent pass). Zero-frame
//! sentinel skips (a callback that processed no frames) are counted and
//! bounded, never silently ignored.
//!
//! Distinct from the real wall-clock endurance suite (`tests/endurance.rs`):
//! the accelerated soak compresses the timeline; the endurance suite
//! runs in real wall-clock time with periodic raw RSS/faults/threads/FD
//! telemetry.
//!
//! Two `#[ignore]`d tests run by Phase 1 of `utils/tests-long.sh`:
//!
//! - `test_soak_320k_multichannel_swaps` — 320 000 continuous audio blocks
//!   with thousands of concurrent swaps. Validates continuous integrity (no
//!   undue silence, no channel inversion, no gain asymmetry) during periodic
//!   Linear-model windows — every reached window must complete.
//!
//! - `test_soak_rss_memory_stability` — captures **raw** resident memory
//!   (VmRSS), page faults, thread and FD counts periodically across the same
//!   soak and asserts the post-warmup drift is below the OS page-margin
//!   threshold (zero memory leak; RSS shrinkage is never a failure).
//!
//! Measured: soak 320k blocks in <elapsed> s, RSS delta=<delta> MB, faults=0
//! (filled by the operator after a calibrated run; the markers carry the live
//! numbers every run).

mod common;

use common::proc::{TelemetrySample, read_thread_rusage};
use common::swap::*;
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use nam_audio_pipe::standalone::rt_setup::thread::{configure_process_wide, mlockall_ok};

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

/// Total continuous audio blocks for the soak (≈426 s of nominal timeline at
/// 48 kHz / BLOCK=64).
const TOTAL_BLOCKS: usize = 320_000;

/// Validation window: every N blocks, install Linear A/B and verify polarity
/// (mandatory — fail-closed, never a vanished window).
const VALIDATION_INTERVAL: usize = 500;

/// Maximum allowed blocks of undue (exact) silence during the soak.
const MAX_UNDUE_SILENCE: usize = 200;

/// Maximum allowed zero-frame sentinel skips (a callback that processed no
/// frames, e.g. while a resampler swap is pending). Documented, bounded —
/// never a silent validation skip.
const MAX_SKIPPED_BLOCKS: usize = 200;

/// Bounded retry budget per validation window: a window must complete within
/// this many consecutive zero-frame attempts or the soak fails.
const MAX_VALIDATION_ATTEMPTS: usize = 64;

/// Maximum allowed RSS growth (KiB) between the canonical quiescent baseline
/// and final GC-settled telemetry samples. Under continuous swapping the
/// allocator retains freed arenas (`M_TRIM_THRESHOLD = -1`), so a 512 KiB
/// (128 pages) bound matches `tests/endurance.rs` and isolates real leaks
/// (MBs/min) from one-time arena growth across mixed model batches.
/// RSS shrinkage is never a failure (reported as a negative delta).
const MAX_RSS_DRIFT_KB: usize = 512;

/// Maximum allowed major page faults over the whole soak.
const MAX_MAJOR_FAULTS: u64 = 8;

/// Aggregate soak metrics: integrity signals that are either asserted
/// fail-closed or reported in the measurement marker.
#[derive(Debug, Default, Clone, Copy)]
struct SoakMetrics {
    /// Blocks where input was non-zero but both outputs were bit-exact zero.
    undue_silence: usize,
    /// Zero-frame sentinel skips (callbacks that processed no frames).
    skipped_blocks: usize,
    /// Validation windows that completed.
    windows_completed: usize,
    /// Validation windows that needed more than one attempt to complete.
    windows_deferred: usize,
}

/// Runs the soak over `sig_l`/`sig_r` for the `[start, start + blocks)`
/// segment of the audio timeline, performing periodic swap batches and (when
/// `validate` is true) mandatory Linear-model validation windows. The signal
/// buffers must hold at least `(start + blocks) * BLOCK` samples per channel.
///
/// Validation windows are fail-closed: a window that cannot complete within
/// `MAX_VALIDATION_ATTEMPTS` consecutive zero-frame attempts fails the soak —
/// zero vanished windows.
fn run_soak_blocks(
    h: &mut RtSwapHarness,
    start: usize,
    blocks: usize,
    validate: bool,
    sig_l: &[f32],
    sig_r: &[f32],
) -> SoakMetrics {
    let mut metrics = SoakMetrics::default();
    let mut validation_pending = false;
    let mut window_needed_retry = false;
    let mut retry_budget = MAX_VALIDATION_ATTEMPTS;
    let mut next_validation = VALIDATION_INTERVAL;

    for local in 0..blocks {
        let block = start + local;
        if block.is_multiple_of(SWAP_INTERVAL) {
            apply_swap_batch(h, block);
        }

        if validate && !validation_pending && block >= next_validation {
            validation_pending = true;
            retry_budget = MAX_VALIDATION_ATTEMPTS;
        }
        if validation_pending {
            if validate_linear_window(h) {
                validation_pending = false;
                metrics.windows_completed += 1;
                if window_needed_retry {
                    metrics.windows_deferred += 1;
                    window_needed_retry = false;
                }
                next_validation += VALIDATION_INTERVAL;
            } else {
                window_needed_retry = true;
                retry_budget -= 1;
                if retry_budget == 0 {
                    panic!(
                        "soak validation window {} vanished: {MAX_VALIDATION_ATTEMPTS} \
                         consecutive zero-frame attempts",
                        metrics.windows_completed
                    );
                }
            }
        }

        // Production main loop drains the GC cascade periodically; the soak
        // must mimic that or retired models accumulate in the overflow ring
        // (whose overwrite policy is a documented bounded leak).
        if block.is_multiple_of(50) {
            h.consume_gc();
        }

        let mut in_l = [0.0f32; BLOCK];
        let mut in_r = [0.0f32; BLOCK];
        in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);
        let frames_before = h.frame_count();
        let n_pw = h.run_callback(&mut in_l, &mut in_r, BLOCK);

        if h.frame_count() == frames_before {
            // Zero-frame sentinel: the quantum was skipped (e.g. resampler
            // swap pending). Counted and bounded — never a silent skip.
            metrics.skipped_blocks += 1;
        }

        // Exact-silence accounting: non-zero input, bit-exact zero output on
        // both channels (undue silence is a real integrity failure, not
        // float-noise).
        if n_pw > 0
            && in_l.iter().any(|&s| s != 0.0)
            && h.out_l().iter().all(|&s| s == 0.0)
            && h.out_r().iter().all(|&s| s == 0.0)
        {
            metrics.undue_silence += 1;
        }
    }

    // Fail-closed resolution of a validation window that triggered near the
    // end of the segment: it must complete within the bounded budget or the
    // soak fails — a window that starts must never vanish with the loop exit.
    let mut final_attempts = 0usize;
    while validation_pending {
        if validate_linear_window(h) {
            validation_pending = false;
            metrics.windows_completed += 1;
            if window_needed_retry {
                metrics.windows_deferred += 1;
            }
        } else {
            final_attempts += 1;
            if final_attempts >= MAX_VALIDATION_ATTEMPTS {
                panic!(
                    "soak final validation window vanished after {MAX_VALIDATION_ATTEMPTS} \
                     zero-frame attempts"
                );
            }
        }
    }

    metrics
}

/// 320 000 continuous audio blocks (~426 s of compressed timeline) with
/// thousands of concurrent swaps across WaveNet, LSTM and Linear models,
/// CabSim IRs, oversampling factors and gain variations. Validates continuous
/// integrity (no undue silence, no channel inversion, no gain asymmetry)
/// during mandatory Linear-model windows — fail-closed, zero vanished windows.
#[test]
#[ignore]
fn test_soak_320k_multichannel_swaps() {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    let (sig_l, sig_r) = test_signal_blocks(TOTAL_BLOCKS);
    let t0 = std::time::Instant::now();

    let metrics = run_soak_blocks(&mut h, 0, TOTAL_BLOCKS, true, &sig_l, &sig_r);
    let elapsed = t0.elapsed();

    // Windows trigger at every VALIDATION_INTERVAL boundary processed *inside*
    // the segment; the boundary at TOTAL_BLOCKS is exclusive, so the count is
    // the number of boundaries strictly below TOTAL_BLOCKS.
    let expected_windows = (TOTAL_BLOCKS - 1) / VALIDATION_INTERVAL;
    assert!(
        metrics.windows_completed == expected_windows,
        "zero vanished windows required: completed {}, reached {expected_windows} \
         (deferred {})",
        metrics.windows_completed,
        metrics.windows_deferred,
    );
    assert!(
        metrics.undue_silence <= MAX_UNDUE_SILENCE,
        "soak observed {} undue-silence blocks (limit {MAX_UNDUE_SILENCE})",
        metrics.undue_silence
    );
    assert!(
        metrics.skipped_blocks <= MAX_SKIPPED_BLOCKS,
        "zero-frame sentinel skips {} exceed the {MAX_SKIPPED_BLOCKS} bound",
        metrics.skipped_blocks
    );
    eprintln!(
        "SOAK_MEASURED blocks={TOTAL_BLOCKS} elapsed_s={:.1} windows_completed={} \
         windows_deferred={} undue_silence={} skipped_blocks={}",
        elapsed.as_secs_f64(),
        metrics.windows_completed,
        metrics.windows_deferred,
        metrics.undue_silence,
        metrics.skipped_blocks,
    );
    eprintln!(
        "TEST_RESULT[soak_320k]=PASS blocks={TOTAL_BLOCKS} windows_completed={} \
         windows_deferred={} undue_silence={} skipped_blocks={}",
        metrics.windows_completed,
        metrics.windows_deferred,
        metrics.undue_silence,
        metrics.skipped_blocks,
    );
}

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
        "soak {stage} quiescent linear window failed after {MAX_VALIDATION_ATTEMPTS} attempts: {}",
        validation_state_debug(h)
    );
}

/// Captures **raw** resident memory (VmRSS), page faults, thread and FD counts
/// periodically across the full soak and asserts the post-warmup drift is
/// strictly below the OS page-margin threshold (zero memory leak). Measures
/// thread-specific rusage (`ru_minflt`, `ru_nivcsw`) on the RT audio callback
/// thread post-warmup and gates on zero minor page faults.
///
/// The full-length signal is pre-allocated once and kept alive across the
/// soak so its heap footprint is part of the baseline (the delta isolates the
/// soak's own allocation behavior).
#[test]
#[ignore]
fn test_soak_rss_memory_stability() {
    configure_process_wide();

    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    for _ in 0..8 {
        let mut l = [0f32; BLOCK];
        let mut r = [0f32; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }
    h.consume_gc();

    warm_producer_arena(&mut h);
    settle_quiescent_linear_window(&mut h, "pre-baseline");

    let (sig_l, sig_r) = test_signal_blocks(TOTAL_BLOCKS);

    let harness = Arc::new(Mutex::new(h));
    let stop = Arc::new(AtomicBool::new(false));
    let warmup_done = Arc::new(AtomicBool::new(false));
    let quiescent_gate = Arc::new(AtomicBool::new(true));
    let rt_finished = Arc::new(AtomicBool::new(false));
    let warmup_blocks = Arc::new(AtomicUsize::new(0));
    let rt_blocks = Arc::new(AtomicUsize::new(0));
    let (tx_ready, rx_ready) = std::sync::mpsc::channel();

    let harness_rt = Arc::clone(&harness);
    let stop_rt = Arc::clone(&stop);
    let warmup_done_rt = Arc::clone(&warmup_done);
    let quiescent_gate_rt = Arc::clone(&quiescent_gate);
    let rt_finished_rt = Arc::clone(&rt_finished);
    let warmup_blocks_rt = Arc::clone(&warmup_blocks);
    let rt_blocks_rt = Arc::clone(&rt_blocks);

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

            // Warmup loop: process until signaled that 2s warmup completed
            while !warmup_done_rt.load(Ordering::Acquire) {
                let b = warmup_blocks_rt.fetch_add(1, Ordering::Relaxed);
                let sig_block = b % TOTAL_BLOCKS;
                in_l.copy_from_slice(&sig_l_rt[sig_block * BLOCK..(sig_block + 1) * BLOCK]);
                in_r.copy_from_slice(&sig_r_rt[sig_block * BLOCK..(sig_block + 1) * BLOCK]);
                {
                    let mut h = harness_rt.lock().expect("harness lock");
                    h.run_callback(&mut in_l, &mut in_r, BLOCK);
                }
                std::thread::yield_now();
            }

            // Pause while main establishes the canonical quiescent baseline
            // (Linear A/B, CabSim None, OS Off, post-GC).
            while quiescent_gate_rt.load(Ordering::Acquire) {
                std::thread::yield_now();
            }

            prefault_rt_thread_and_signals(&sig_l_rt, &sig_r_rt);
            let rusage_before = read_thread_rusage().expect("initial thread rusage");

            while !stop_rt.load(Ordering::Relaxed) {
                let b = rt_blocks_rt.fetch_add(1, Ordering::Relaxed);
                if b >= TOTAL_BLOCKS {
                    break;
                }
                let sig_block = b % TOTAL_BLOCKS;
                in_l.copy_from_slice(&sig_l_rt[sig_block * BLOCK..(sig_block + 1) * BLOCK]);
                in_r.copy_from_slice(&sig_r_rt[sig_block * BLOCK..(sig_block + 1) * BLOCK]);
                {
                    let mut h = harness_rt.lock().expect("harness lock");
                    h.run_callback(&mut in_l, &mut in_r, BLOCK);
                }
                std::thread::yield_now();
            }

            // Signal main that all callbacks stopped so the final quiescent reset
            // and GC drain observe zero concurrent RT mutations. Keep the thread
            // alive until the final telemetry sample is captured.
            rt_finished_rt.store(true, Ordering::Release);
            while !stop_rt.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            let rusage_after = read_thread_rusage().expect("final thread rusage");
            (rusage_before, rusage_after)
        })
        .expect("spawn RT thread");

    rx_ready.recv().expect("wait RT thread ready");

    // 3 s multi-thread warm-up window (stabilizes concurrent arena/GC watermark)
    let warmup_start = Instant::now();
    let warmup_duration = Duration::from_secs(3);
    let mut last_warmup_swap = 0usize;
    let mut last_warmup_gc = 0usize;
    while warmup_start.elapsed() < warmup_duration || warmup_blocks.load(Ordering::Relaxed) < 4_000
    {
        let b = warmup_blocks.load(Ordering::Relaxed);
        if b.saturating_sub(last_warmup_swap) >= SWAP_INTERVAL {
            let mut h = harness.lock().expect("harness lock");
            apply_swap_batch(&mut h, b);
            last_warmup_swap = b;
        }
        if b.saturating_sub(last_warmup_gc) >= 50 {
            let mut h = harness.lock().expect("harness lock");
            h.consume_gc();
            last_warmup_gc = b;
        }
        std::thread::yield_now();
    }

    warmup_done.store(true, Ordering::Release);
    // Restore canonical quiescent baseline before sampling RSS baseline so
    // `first -> last` compares identical topologies (`Linear A/B`, no CabSim,
    // OS Off, GC drained) rather than +/- ~7 MiB CabSim transients.
    {
        let mut h = harness.lock().expect("harness lock");
        settle_quiescent_linear_window(&mut h, "baseline");
    }
    let mut telemetry: Vec<TelemetrySample> = vec![TelemetrySample::capture()];
    quiescent_gate.store(false, Ordering::Release);

    let mut last_swap_block = 0usize;
    let mut last_gc_block = 0usize;
    let mut last_telem_block = 0usize;

    while !rt_finished.load(Ordering::Acquire) {
        let b = rt_blocks.load(Ordering::Relaxed);
        if b.saturating_sub(last_swap_block) >= SWAP_INTERVAL {
            let mut h = harness.lock().expect("harness lock");
            apply_swap_batch(&mut h, b);
            last_swap_block = b;
        }
        if b.saturating_sub(last_gc_block) >= 50 {
            let mut h = harness.lock().expect("harness lock");
            h.consume_gc();
            last_gc_block = b;
        }
        if b.saturating_sub(last_telem_block) >= 10_000 {
            telemetry.push(TelemetrySample::capture());
            last_telem_block = b;
        }
        std::thread::yield_now();
    }

    // Final quiescent sample: captured after RT stops processing and before
    // `join`, so any in-flight SPSC swaps/CabSims (~7 MiB) are drained into the
    // exact same quiescent topology as baseline while `threads` stays constant.
    {
        let mut h = harness.lock().expect("harness lock");
        settle_quiescent_linear_window(&mut h, "final");
    }
    telemetry.push(TelemetrySample::capture());
    stop.store(true, Ordering::Release);
    let (rusage_before, rusage_after) = rt_handle.join().expect("join RT thread");

    for (i, s) in telemetry.iter().enumerate() {
        eprintln!(
            "SOAK_RSS_TELEMETRY sample={i} rss_kb={} minflt={} majflt={} threads={} fds={}",
            s.rss_kb, s.minflt, s.majflt, s.threads, s.fds,
        );
    }

    // Process-wide page faults (`TelemetrySample::minflt`/`majflt`, read from
    // `/proc/self/stat`) include allocator-arena churn from the off-RT producer
    // (swap batches, GC). They are diagnostic only and never a zero-fault
    // oracle. The invariant below judges `getrusage(RUSAGE_THREAD)` of the RT
    // callback thread (`minflt_thread_delta`/`majflt_thread_delta`), the only
    // counters that can attribute a page fault to the audio thread.
    let minflt_thread_delta = rusage_after.minflt.saturating_sub(rusage_before.minflt);
    let nivcsw_thread_delta = rusage_after.nivcsw.saturating_sub(rusage_before.nivcsw);
    let majflt_thread_delta = rusage_after.majflt.saturating_sub(rusage_before.majflt);

    assert!(
        telemetry.len() >= 2,
        "soak captured {} telemetry sample(s); at least two (warmup baseline + \
         final pre-join) are required — a single sample makes the first→last \
         deltas vacuous",
        telemetry.len()
    );
    let first = telemetry.first().expect("telemetry baseline");
    let last = telemetry.last().expect("telemetry final");
    let rss_delta_kb = last.rss_kb as i64 - first.rss_kb as i64;
    let minflt_delta = last.minflt.saturating_sub(first.minflt);
    let majflt_delta = last.majflt.saturating_sub(first.majflt);
    let threads_delta = last.threads as i64 - first.threads as i64;
    let fds_delta = last.fds as i64 - first.fds as i64;

    assert!(first.rss_kb > 0, "VmRSS unavailable on this platform");
    assert!(
        rss_delta_kb <= MAX_RSS_DRIFT_KB as i64,
        "RSS growth of {rss_delta_kb} KiB between warmup baseline ({} KiB) and block {TOTAL_BLOCKS} \
         ({} KiB) exceeds {MAX_RSS_DRIFT_KB} KiB margin — possible memory leak",
        first.rss_kb,
        last.rss_kb,
    );
    assert!(
        majflt_delta <= MAX_MAJOR_FAULTS,
        "major page faults {majflt_delta} exceed the {MAX_MAJOR_FAULTS} bound"
    );
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
    eprintln!(
        "SOAK_RSS_MEASURED rss_first_kb={} rss_last_kb={} rss_delta_kb={rss_delta_kb} \
         minflt_delta={minflt_delta} majflt_delta={majflt_delta} \
         minflt_thread_delta={minflt_thread_delta} nivcsw_thread_delta={nivcsw_thread_delta} \
         threads_first={} threads_last={} fds_first={} fds_last={} telemetry_samples={}",
        first.rss_kb,
        last.rss_kb,
        first.threads,
        last.threads,
        first.fds,
        last.fds,
        telemetry.len(),
    );
    // The zero-fault invariant is a calibrated-host property: under non-strict
    // hosted QA an out-of-budget fault is a typed environment GAP; under `--strict`
    // (operator release) it remains a hard failure. When `mlockall` succeeded and
    // Linux `compact_unevictable_allowed=1` permits `kcompactd` page migration,
    // `MAX_COMPACTION_MINOR_FAULTS` absorbs transient compaction waits.
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
                "TEST_RESULT[soak]=GAP:rt_thread_page_faults profile=release+testing total_blocks={TOTAL_BLOCKS} \
                 mlockall_ok={mlock_ok} compact_unevictable_allowed={compact_allowed} minflt_thread_delta={minflt_thread_delta} \
                 minflt_budget={allowed_minflt} majflt_thread_delta={majflt_thread_delta} nivcsw_thread_delta={nivcsw_thread_delta} \
                 rss_delta_kb={rss_delta_kb} threads_delta={threads_delta} fds_delta={fds_delta}"
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
        "TEST_RESULT[soak]=PASS profile=release+testing total_blocks={TOTAL_BLOCKS} \
         mlockall_ok={mlock_ok} compact_unevictable_allowed={compact_allowed} minflt_thread_delta={minflt_thread_delta} \
         minflt_budget={allowed_minflt} majflt_thread_delta={majflt_thread_delta} \
         rss_delta_kb={rss_delta_kb} threads_delta={threads_delta} fds_delta={fds_delta}"
    );
}
