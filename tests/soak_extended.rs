// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! Accelerated-timeline soak harness for the PipeWire host (T6.4 / G-RB-002;
//! T5.3 / G-PERF-004).
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
//! Distinct from the real wall-clock endurance suite (`tests/endurance.rs`,
//! T5.3): the accelerated soak compresses the timeline; the endurance suite
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
//! Medido: soak 320k blocos em <elapsed> s, RSS delta=<delta> MB, faults=0
//! (filled by the operator after a calibrated run; the markers carry the live
//! numbers every run).

mod common;

use common::proc::TelemetrySample;
use common::swap::*;
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;

/// Total continuous audio blocks for the soak (≈426 s of nominal timeline at
/// 48 kHz / BLOCK=64 — the G-PERF-004 accelerated-timeline target).
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

/// Maximum allowed RSS growth (KiB) between the first and last telemetry
/// samples. Under continuous swapping the allocator's steady state wobbles
/// ±~50 KiB (model sizes vary per swap batch), so 256 KiB (64 pages) is the
/// leak-sensitive margin: a real leak grows by MBs per minute, far above it.
/// RSS shrinkage is never a failure (reported as a negative delta).
const MAX_RSS_DRIFT_KB: usize = 256;

/// Maximum allowed major page faults over the whole soak.
const MAX_MAJOR_FAULTS: u64 = 8;

/// Aggregate soak metrics (T5.3): integrity signals that are either asserted
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
}

/// Captures **raw** resident memory (VmRSS), page faults, thread and FD counts
/// periodically across the full soak and asserts the post-warmup drift is
/// strictly below the OS page-margin threshold (zero memory leak). RSS
/// shrinkage is never a failure; growth beyond the margin is.
///
/// The full-length signal is pre-allocated once and kept alive across the
/// soak so its heap footprint is part of the baseline (the delta isolates the
/// soak's own allocation behavior).
#[test]
#[ignore]
fn test_soak_rss_memory_stability() {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    let (sig_l, sig_r) = test_signal_blocks(TOTAL_BLOCKS);

    let _ = run_soak_blocks(&mut h, 0, 1_000, false, &sig_l, &sig_r);
    let mut telemetry: Vec<TelemetrySample> = vec![TelemetrySample::capture()];

    // Periodic raw telemetry across the rest of the soak (every 10k blocks).
    let mut remaining = 1_000;
    while remaining < TOTAL_BLOCKS {
        let next = (remaining + 10_000).min(TOTAL_BLOCKS);
        let _ = run_soak_blocks(&mut h, remaining, next - remaining, false, &sig_l, &sig_r);
        telemetry.push(TelemetrySample::capture());
        remaining = next;
    }

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
        "RSS growth of {rss_delta_kb} KiB between block 1 000 ({} KiB) and block {TOTAL_BLOCKS} \
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
         minflt_delta={minflt_delta} majflt_delta={majflt_delta} threads_first={} threads_last={} \
         fds_first={} fds_last={} telemetry_samples={}",
        first.rss_kb,
        last.rss_kb,
        first.threads,
        last.threads,
        first.fds,
        last.fds,
        telemetry.len(),
    );
}
