// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! Real wall-clock endurance suite (T5.3 / G-PERF-004).
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
//! Medido: soak 320k blocos em <elapsed> s, RSS delta=<delta> MB, faults=0
//! (filled by the operator after a calibrated run; the marker carries the live
//! numbers every run).

mod common;

use common::proc::{
    TelemetrySample, read_fd_count, read_page_faults, read_rss_kb, read_thread_count,
};
use common::swap::*;
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;

use std::time::{Duration, Instant};

/// Validation window cadence: every N blocks a mandatory Linear A/B polarity
/// window must complete (fail-closed — see the acceptance "zero janelas
/// desaparecidas").
const VALIDATION_INTERVAL: usize = 500;

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
/// samples. Under continuous swapping the allocator's steady state wobbles
/// ±~50 KiB (model sizes vary per swap batch), so 256 KiB (64 pages) is the
/// leak-sensitive margin: a real leak grows by MBs per minute, far above it.
/// RSS shrinkage is never a failure (reported as a negative delta).
const MAX_RSS_DRIFT_KB: usize = 256;

/// Maximum allowed major page faults over the whole endurance (registered and
/// bounded; fresh allocator pages fault minor, never major).
const MAX_MAJOR_FAULTS: u64 = 8;

/// Default wall-clock window in seconds (overridable via `NAM_ENDURANCE_SECONDS`).
const DEFAULT_WINDOW_SECS: u64 = 30;

/// Minimum wall-clock window: below this the suite is a vacuous pass and fails.
const MIN_WINDOW_SECS: u64 = 5;

/// Wall-clock window from `NAM_ENDURANCE_SECONDS` (default 30 s, bounded).
fn endurance_window_seconds() -> u64 {
    let raw = std::env::var("NAM_ENDURANCE_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_WINDOW_SECS);
    raw.clamp(MIN_WINDOW_SECS, 600)
}

/// Real wall-clock endurance (T5.3 / G-PERF-004).
///
/// Runs continuous audio + periodic swap batches for a real wall-clock window,
/// with mandatory fail-closed validation windows every `VALIDATION_INTERVAL`
/// blocks and periodic raw RSS/faults/threads/FD telemetry. Every reached
/// window must complete (`windows_completed == blocks / VALIDATION_INTERVAL`);
/// a window that cannot complete is a hard failure.
#[test]
#[ignore = "Real wall-clock endurance: fail-closed validation windows + periodic RSS/faults/threads/FD telemetry — long suite only (tests-long.sh Phase 6)"]
fn test_endurance_real_wall_clock_windows_fail_closed() {
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

    // Pre-warm every model kind the swap batches build so the timed window
    // starts from a resident working set (the baseline RSS sample includes the
    // preallocated signal buffers).
    for _ in 0..4 {
        apply_swap_batch(&mut h, 0);
        let (sig_l, sig_r) = test_signal_blocks(8);
        let mut in_l = [0f32; BLOCK];
        let mut in_r = [0f32; BLOCK];
        for block in 0..8 {
            in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
            in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);
            h.run_callback(&mut in_l, &mut in_r, BLOCK);
        }
        h.consume_gc();
    }

    let max_blocks = (window.as_secs() * SAMPLE_RATE as u64 / BLOCK as u64) as usize + BLOCK;
    let (sig_l, sig_r) = test_signal_blocks(max_blocks);
    let mut in_l = [0f32; BLOCK];
    let mut in_r = [0f32; BLOCK];

    let start = Instant::now();
    let mut blocks = 0usize;
    let mut undue_silence = 0usize;
    let mut skipped_blocks = 0usize;
    let mut windows_completed = 0usize;
    let mut windows_deferred = 0usize;
    let mut window_needed_retry = false;
    let mut validation_pending = false;
    let mut retry_budget = MAX_VALIDATION_ATTEMPTS;
    let mut next_validation = VALIDATION_INTERVAL;
    let mut telemetry: Vec<TelemetrySample> = Vec::new();
    let mut last_telemetry_block = 0usize;

    while start.elapsed() < window {
        let block = blocks;
        if block.is_multiple_of(SWAP_INTERVAL) {
            apply_swap_batch(&mut h, block);
        }

        // The offline harness processes far faster than real-time, so the
        // deterministic signal is cycled (the nominal timeline is virtual —
        // the wall-clock requirement is the loop window, not the sample rate).
        let sig_block = block % max_blocks;
        in_l.copy_from_slice(&sig_l[sig_block * BLOCK..(sig_block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[sig_block * BLOCK..(sig_block + 1) * BLOCK]);
        let frames_before = h.frame_count();
        let n_pw = h.run_callback(&mut in_l, &mut in_r, BLOCK);
        blocks += 1;

        if h.frame_count() == frames_before {
            skipped_blocks += 1;
        }

        // Exact-silence accounting: non-zero input, bit-exact zero output on
        // both channels (undue silence is a real integrity failure, not
        // float-noise).
        if n_pw > 0
            && in_l.iter().any(|&s| s != 0.0)
            && h.out_l().iter().all(|&s| s == 0.0)
            && h.out_r().iter().all(|&s| s == 0.0)
        {
            undue_silence += 1;
        }

        // Mandatory validation windows (fail-closed).
        if !validation_pending && block >= next_validation {
            validation_pending = true;
            retry_budget = MAX_VALIDATION_ATTEMPTS;
        }
        if validation_pending {
            if validate_linear_window(&mut h) {
                validation_pending = false;
                windows_completed += 1;
                next_validation += VALIDATION_INTERVAL;
                if window_needed_retry {
                    windows_deferred += 1;
                    window_needed_retry = false;
                }
            } else {
                window_needed_retry = true;
                retry_budget -= 1;
                if retry_budget == 0 {
                    panic!(
                        "endurance validation window {windows_completed} vanished: \
                         {MAX_VALIDATION_ATTEMPTS} consecutive zero-frame attempts"
                    );
                }
            }
        }

        if blocks >= TELEMETRY_WARMUP_BLOCKS
            && (telemetry.is_empty() || blocks - last_telemetry_block >= TELEMETRY_INTERVAL)
        {
            telemetry.push(TelemetrySample::capture());
            last_telemetry_block = blocks;
        }

        if block.is_multiple_of(50) {
            h.consume_gc();
        }
    }

    // Resolve a validation window that started before the clock stopped —
    // fail-closed: it must complete within the budget or the suite fails.
    let mut final_attempts = 0usize;
    while validation_pending {
        if validate_linear_window(&mut h) {
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
    h.consume_gc();

    // Windows trigger at every VALIDATION_INTERVAL boundary processed inside
    // the wall-clock loop; the boundary at `blocks` itself is exclusive, so
    // the expected count is the number of boundaries strictly below `blocks`.
    let expected_windows = (blocks.saturating_sub(1)) / VALIDATION_INTERVAL;
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
        blocks > 0,
        "endurance processed no audio blocks — vacuous pass"
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
    assert!(
        rss_delta_kb <= MAX_RSS_DRIFT_KB as i64,
        "RSS growth of {rss_delta_kb} KiB (first {} → last {}) exceeds \
         the {MAX_RSS_DRIFT_KB} KiB margin — possible memory leak",
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
        "TEST_RESULT[endurance_real]=PASS profile=release+testing window_secs={window_secs} \
         elapsed_ms={} dsp_quantums={blocks} blocks={blocks} windows_completed={windows_completed} \
         windows_deferred={windows_deferred} rss_first_kb={} rss_last_kb={} rss_delta_kb={rss_delta_kb} \
         minflt_delta={minflt_delta} majflt_delta={majflt_delta} threads_first={} threads_last={} \
         threads_delta={threads_delta} fds_first={} fds_last={} fds_delta={fds_delta} \
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
    assert!(
        minflt1 >= minflt0 && majflt1 >= majflt0,
        "page-fault counters must be monotonic: {minflt0}/{majflt0} → {minflt1}/{majflt1}"
    );
    assert!(
        read_thread_count() >= threads && read_fd_count() >= fds,
        "thread/FD counters must be monotonic"
    );
}
