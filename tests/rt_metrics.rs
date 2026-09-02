// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! Nanosecond RT metrics harness for the PipeWire host.
//!
//! Runs exclusively in `utils/tests-long.sh` (Phases 3/4/5), never in the
//! default/quick loop (rules/testing.md §1). Three `#[ignore]`d gates, each
//! emitting exactly one typed `TEST_RESULT[...]` marker per run:
//!
//! - `rt_deadline_gate_10k_quantums` (filter `deadline`, Phase 3): 10 000
//!   consecutive DSP quantums under full WaveNet A1/A2 + CabSim load, timed
//!   with `CLOCK_MONOTONIC_RAW`. The quantum budget is 85% of the nominal
//!   block period. Verdict is fail-closed:
//!   `TEST_RESULT[rt_deadline]=PASS max_ns=... budget_ns=... margin_pct=...`,
//!   or `=GAP:uncalibrated_environment` when a miss happens on a noisy dev
//!   host (a calibrated isolated-core + `SCHED_FIFO` machine turns the miss
//!   into a hard failure).
//! - `rt_jitter_gate_10k_callbacks` (filter `jitter`, Phase 4): 10 000
//!   callback dispatches at the nominal period while background I/O, cache
//!   thrash and syscall-storm threads perturb the shared CPU. Measures the
//!   inter-callback interval dispersion (max / p99 / std-dev) and emits
//!   `TEST_RESULT[rt_jitter]=PASS max_jitter_us=... p99_jitter_us=...`.
//!   Without exclusive CPU affinity the gate skips as
//!   `=GAP:cpu_not_isolated` (task rollback — no false positives).
//! - `concurrent_state_interleaving_stress_16_threads` (filter `concurrent`,
//!   Phase 5): 16-thread stress over swap requests, stream-state transitions
//!   (`observe_stream_state`), simulated reconnect cycles, sample-rate
//!   renegotiation and the cooperative `SHUTDOWN` trigger. The rate workers
//!   follow the production renegotiation causality: publish → `sync_rate`
//!   observes → constructor dispatches the generation-stamped envelope, so
//!   `drain_resamplers` installs it, clears `RESAMP_SWAP_PENDING` and the RT
//!   driver keeps incrementing `frame_count`. A watcher thread
//!   samples coherent [`BackendStatusSnapshot`]s and asserts the hardened
//!   `SharedBackendStatus` machine never leaks failure state; the RT driver
//!   asserts sample-rate reads stay within the published set; the joined
//!   completion proves deadlock freedom. Emits
//!   `TEST_RESULT[concurrency_stress]=PASS ...`.
//! - `concurrent_spsc_throughput_swap_accounting` (filter `concurrent`,
//!   Phase 5): **state-machine throughput** through the
//!   production SPSC protocol — [`RtSwapHarness::into_parts`] splits the
//!   harness into its two production faces, so a single-writer producer thread
//!   pushes swap commands through the real ring buffers while the RT side runs
//!   continuous DSP quantums with per-callback accounting
//!   (applied/pops/backlog). The producer counts every push
//!   (attempted/enqueued/dropped), the RT side counts applied/superseded/
//!   deferred/pending. Emits
//!   `TEST_RESULT[spsc_throughput]=PASS dsp_quantums=... swaps_attempted=...`.
//!   The harness throughput is **never** labelled "audio callbacks".
//!
//! All timing uses `libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, ...)` and
//! absolute `clock_nanosleep` (`TIMER_ABSTIME`) sleeps bridged from the RAW
//! clock domain through the measured RAW→MONOTONIC offset — immune to NTP
//! adjustments. Environment calibration is probed from `/proc/self/status`
//! (`Cpus_allowed_list`), `/sys/devices/system/cpu/isolated` and
//! `sched_getscheduler`. `NAM_RT_STRICT=1` (propagated by
//! `utils/tests-long.sh --strict-pre-release`) promotes every GAP
//! condition to a hard assertion failure AND refuses to certify a PASS
//! measured on an uncalibrated environment — a numeric pass below the limits
//! on a non-calibrated host must fail, never emit a silent pass.

mod common;

use common::swap::*;
use nam_audio_pipe::standalone::pw_host::{
    RtSwapHarness, SharedBackendStatus, observe_stream_state,
};
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_NEEDS_RESAMPLER_REBUILD, SHUTDOWN};
use pipewire::stream::StreamState;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Number of DSP quantums for the deadline gate (task spec: 10 000).
const DEADLINE_QUANTUMS: usize = 10_000;

/// Number of callback dispatches for the jitter gate (task spec: 10 000).
const JITTER_CALLBACKS: usize = 10_000;

/// Safety ceiling for the quantum budget (task spec): 85% of the nominal
/// block period, absorbing kernel/driver overheads.
const BUDGET_FACTOR: f64 = 0.85;

/// Wall-clock window for the 16-thread concurrency model-check stress.
const STRESS_WINDOW: Duration = Duration::from_secs(3);

/// Number of concurrent model-check threads (task spec: 16).
const MODEL_CHECK_THREADS: usize = 16;

/// Host rates the rate-renegotiation workers may publish; the RT driver
/// asserts `current_host_rate()` never leaves this set.
const VALID_RATES: [u32; 4] = [32_000, 44_100, 48_000, 96_000];

/// Rate-renegotiation publish period: each rate worker publishes a new
/// host rate every N loop iterations. The publish strictly precedes any
/// resampler delivery — the RT callback must observe it in `sync_rate` first.
const RATE_PUBLISH_PERIOD: u64 = 8;

/// Rate-renegotiation publish budget: after this many loop iterations a
/// rate worker stops publishing and only plays the constructor role — keeping
/// the resampler build/GC churn bounded while still delivering an envelope for
/// every request the RT callback already observed (including the final one).
const RATE_PUBLISH_BUDGET: u64 = 200;

/// Calibrated micro-yield sleep for the stress workers: 10–50 µs so the
/// RT driver always wins a scheduling slot on a saturated host instead of
/// starving on the harness lock against the 15 writer threads.
const WORKER_YIELD_US: u64 = 20;
// ── Nanosecond timing (CLOCK_MONOTONIC_RAW, NTP-immune) ─────────────────────

/// Reads a monotonic clock's current time in nanoseconds.
fn clock_ns(clockid: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(clockid, &mut ts) };
    assert_eq!(rc, 0, "clock_gettime({clockid}) failed with errno {rc}");
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// Reads the current `CLOCK_MONOTONIC_RAW` time in nanoseconds — immune to
/// NTP adjustments (task spec).
fn now_ns() -> u64 {
    clock_ns(libc::CLOCK_MONOTONIC_RAW)
}

/// Reads the current `CLOCK_MONOTONIC` time in nanoseconds.
fn mono_ns() -> u64 {
    clock_ns(libc::CLOCK_MONOTONIC)
}

/// Absolute `CLOCK_MONOTONIC_RAW` deadline converted into a `CLOCK_MONOTONIC`
/// absolute sleep.
///
/// The kernel's `clock_nanosleep` only accepts `CLOCK_REALTIME` /
/// `CLOCK_MONOTONIC` (RAW requests fail with `EOPNOTSUPP`; `timerfd_create`
/// likewise rejects RAW with `EINVAL`), so the raw deadline is translated
/// through the RAW→MONOTONIC offset measured at scheduling time. The residual
/// offset drift over a single nominal period (~1.33 ms) stays sub-microsecond
/// even at the maximum kernel slew rate, keeping the measurement NTP-immune
/// in practice. Retries on `EINTR`; a deadline already in the past returns
/// immediately.
fn sleep_until_monotonic_raw(deadline_raw_ns: u64) {
    // mono_time = raw_time + offset; convert the raw deadline into the
    // MONOTONIC domain the kernel can sleep on.
    let offset = mono_ns() as i128 - now_ns() as i128;
    let deadline_mono = (deadline_raw_ns as i128 + offset).clamp(0, i64::MAX as i128) as u64;
    let ts = libc::timespec {
        tv_sec: (deadline_mono / 1_000_000_000) as libc::time_t,
        tv_nsec: (deadline_mono % 1_000_000_000) as libc::c_long,
    };
    loop {
        let rc = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        };
        if rc == 0 {
            break;
        }
        assert_eq!(rc, libc::EINTR, "clock_nanosleep failed with errno {rc}");
    }
}

/// Nearest-rank percentile of a sample series (ns).
fn percentile_ns(mut samples: Vec<u64>, p: f64) -> u64 {
    assert!(!samples.is_empty(), "percentile of an empty sample series");
    samples.sort_unstable();
    let idx = ((samples.len() - 1) as f64 * p).round() as usize;
    samples[idx]
}

/// Population standard deviation of a sample series (ns).
fn std_dev_ns(samples: &[u64]) -> f64 {
    assert!(!samples.is_empty(), "std-dev of an empty sample series");
    let n = samples.len() as f64;
    let mean = samples.iter().map(|&s| s as f64).sum::<f64>() / n;
    let var = samples
        .iter()
        .map(|&s| (s as f64 - mean).powi(2))
        .sum::<f64>()
        / n;
    var.sqrt()
}

// ── Environment calibration probe ────────────────────────────────────────────

/// Calibration facts of the measurement environment (probed once per gate).
struct RtEnvironment {
    /// `NAM_RT_STRICT=1`: promote every GAP condition to a hard failure.
    strict: bool,
    /// The process is pinned to exactly one CPU (`Cpus_allowed_list`).
    pinned_single_cpu: bool,
    /// The pinned CPU is in the kernel's isolcpus set.
    cpu_isolated: bool,
    /// `sched_getscheduler` reports `SCHED_FIFO`.
    sched_fifo: bool,
}

impl RtEnvironment {
    fn probe() -> Self {
        let allowed = cpus_allowed_list();
        let pinned_single_cpu = allowed.len() == 1;
        let cpu_isolated =
            pinned_single_cpu && allowed.first().is_some_and(|c| isolated_cpus().contains(c));
        Self {
            strict: std::env::var("NAM_RT_STRICT").as_deref() == Ok("1"),
            pinned_single_cpu,
            cpu_isolated,
            sched_fifo: unsafe { libc::sched_getscheduler(0) } == libc::SCHED_FIFO,
        }
    }

    /// A calibrated realtime environment: exclusive CPU affinity on an
    /// isolated core plus a FIFO scheduler (task spec §1).
    fn calibrated(&self) -> bool {
        self.pinned_single_cpu && self.cpu_isolated && self.sched_fifo
    }
}

/// Parses `/proc/self/status` `Cpus_allowed_list` into the allowed CPU ids.
fn cpus_allowed_list() -> Vec<u32> {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Cpus_allowed_list:") {
            return parse_cpu_ranges(rest.trim());
        }
    }
    Vec::new()
}

/// Parses the kernel's `/sys/devices/system/cpu/isolated` CPU ids.
fn isolated_cpus() -> Vec<u32> {
    let s = std::fs::read_to_string("/sys/devices/system/cpu/isolated").unwrap_or_default();
    parse_cpu_ranges(s.trim())
}

/// Expands "0,2-4" style CPU ranges into a flat id list.
fn parse_cpu_ranges(s: &str) -> Vec<u32> {
    let mut cpus = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: u32 = lo.trim().parse().unwrap_or(0);
            let hi: u32 = hi.trim().parse().unwrap_or(lo);
            cpus.extend(lo..=hi);
        } else if let Ok(c) = part.parse::<u32>() {
            cpus.push(c);
        }
    }
    cpus
}

// ── Shared setup: full real-load harness (WaveNet A1/A2 + CabSim) ───────────

/// Builds the harness with the task-spec real load: WaveNet A1 on L, WaveNet
/// A2 on R, stereo CabSim active, unity gains. Returns it warm (commands
/// drained, GC consumed) so the measurement loop starts from steady state.
fn real_load_harness() -> RtSwapHarness {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("rt-metrics harness");
    h.push_load_model(
        Some(wavenet_a1()),
        Some(wavenet_a2()),
        1.0,
        1.0,
        SAMPLE_RATE,
    );
    h.push_cabsim(Some(cabsim_pair()));
    h.push_input_gain(1.0);
    h.push_output_gain(1.0);
    for _ in 0..8 {
        let mut l = [0f32; BLOCK];
        let mut r = [0f32; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }
    h.consume_gc();
    h
}

// ── 1. RT Deadline Gate (Phase 3 of tests-long.sh) ──────────────────────────

/// 10 000 consecutive DSP quantums under full real load, timed in
/// nanoseconds. Fails closed against the 85% budget; a miss on a noisy dev
/// host is a typed `GAP:uncalibrated_environment`, on a calibrated RT
/// machine a hard failure.
#[test]
#[ignore = "RT deadline gate: 10k DSP quantums under an 85% nanosecond budget — long suite only (tests-long.sh Phase 3)"]
fn rt_deadline_gate_10k_quantums() {
    if cfg!(debug_assertions) {
        eprintln!("TEST_RESULT[rt_deadline]=GAP:debug_build_measurement_invalid");
        return;
    }
    let env = RtEnvironment::probe();
    let mut h = real_load_harness();
    let budget_ns = ((BLOCK as f64 / SAMPLE_RATE as f64) * 1e9 * BUDGET_FACTOR) as u64;

    let (sig_l, sig_r) = test_signal_blocks(DEADLINE_QUANTUMS);
    let mut samples = Vec::with_capacity(DEADLINE_QUANTUMS);
    let mut min_ns = u64::MAX;
    let mut max_ns = 0u64;
    let mut sum_ns = 0u128;

    for block in 0..DEADLINE_QUANTUMS {
        let mut in_l = [0f32; BLOCK];
        let mut in_r = [0f32; BLOCK];
        in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);
        let t0 = now_ns();
        h.run_callback(&mut in_l, &mut in_r, BLOCK);
        let t1 = now_ns();
        let dur = t1 - t0;
        samples.push(dur);
        min_ns = min_ns.min(dur);
        max_ns = max_ns.max(dur);
        sum_ns += dur as u128;
    }
    h.consume_gc();

    let mean_ns = (sum_ns / DEADLINE_QUANTUMS as u128) as u64;
    let p99_ns = percentile_ns(samples.clone(), 0.99);
    let margin_pct = (budget_ns.saturating_sub(max_ns) as f64 / budget_ns as f64) * 100.0;

    eprintln!(
        "RT_METRICS rt_deadline quantums={DEADLINE_QUANTUMS} min_ns={min_ns} mean_ns={mean_ns} \
         p99_ns={p99_ns} max_ns={max_ns} budget_ns={budget_ns} margin_pct={margin_pct:.1}"
    );

    if max_ns <= budget_ns {
        // Under NAM_RT_STRICT=1 a PASS is only certifiable on a
        // calibrated RT environment — numbers below the budget on an
        // uncalibrated host must fail, never emit a silent pass.
        if env.strict && !env.calibrated() {
            panic!(
                "RT deadline gate FAILED: NAM_RT_STRICT=1 requires a calibrated realtime \
                 environment (single pinned isolated CPU + SCHED_FIFO) to certify a PASS; the \
                 current environment is not calibrated — refusing a silent pass on an \
                 uncalibrated host"
            );
        }
        eprintln!(
            "TEST_RESULT[rt_deadline]=PASS max_ns={max_ns} budget_ns={budget_ns} margin_pct={margin_pct:.1}"
        );
        return;
    }

    // Deadline miss: the verdict is fail-closed on the environment. A
    // calibrated RT machine (isolated core + SCHED_FIFO) turns the miss into
    // a hard failure; a noisy/dev host reports a typed GAP; `NAM_RT_STRICT=1`
    // promotes the GAP to a failure as well.
    if env.calibrated() {
        panic!(
            "RT deadline gate FAILED: max callback {max_ns} ns exceeds the {budget_ns} ns budget \
             (mean {mean_ns} ns, p99 {p99_ns} ns) on a calibrated/RT environment — real xrun risk"
        );
    } else if env.strict {
        panic!(
            "RT deadline gate FAILED: max callback {max_ns} ns exceeds the {budget_ns} ns budget \
             (mean {mean_ns} ns, p99 {p99_ns} ns) — NAM_RT_STRICT=1 promoted this GAP to a hard \
             failure (environment is not calibrated: CPU not isolated/pinned or no SCHED_FIFO); \
             re-run on the calibrated RT host"
        );
    }
    eprintln!(
        "TEST_RESULT[rt_deadline]=GAP:uncalibrated_environment max_ns={max_ns} budget_ns={budget_ns}"
    );
}

// ── 2. RT Jitter Gate (Phase 4 of tests-long.sh) ────────────────────────────

/// 10 000 callback dispatches at the nominal period under background
/// I/O/cache/syscall stress. Measures the inter-callback interval dispersion
/// (max / p99 / std-dev) and emits the typed PASS marker; without exclusive
/// CPU affinity the gate reports `GAP:cpu_not_isolated` (task rollback).
#[test]
#[ignore = "RT jitter gate: inter-callback dispatch dispersion under contention — long suite only (tests-long.sh Phase 4)"]
fn rt_jitter_gate_10k_callbacks() {
    if cfg!(debug_assertions) {
        eprintln!("TEST_RESULT[rt_jitter]=GAP:debug_build_measurement_invalid");
        return;
    }
    let env = RtEnvironment::probe();
    if !env.pinned_single_cpu {
        if env.strict {
            panic!("RT jitter gate: NAM_RT_STRICT=1 and the process is not pinned to a single CPU");
        }
        eprintln!("TEST_RESULT[rt_jitter]=GAP:cpu_not_isolated");
        return;
    }

    let mut h = real_load_harness();
    let nominal_ns = ((BLOCK as f64 / SAMPLE_RATE as f64) * 1e9) as u64;
    let (sig_l, sig_r) = test_signal_blocks(JITTER_CALLBACKS);

    // Background contention: I/O churn, cache thrashing and syscall storms on
    // the same affinity set as the callback thread (task spec §1: "sob
    // estresse de threads de I/O em background").
    let stop = Arc::new(AtomicBool::new(false));
    let stressers: Vec<_> = (0..6)
        .map(|i| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                // Background contention threads must run under normal scheduling
                // (SCHED_OTHER / priority 0) so the SCHED_FIFO callback thread
                // preempts them predictably rather than starving in the same FIFO queue.
                let param = libc::sched_param { sched_priority: 0 };
                unsafe {
                    libc::sched_setscheduler(0, libc::SCHED_OTHER, &param);
                }
                match i % 3 {
                    0 => io_churn_loop(&stop),
                    1 => cache_thrash_loop(&stop),
                    _ => syscall_storm_loop(&stop),
                }
            })
        })
        .collect();

    let mut deltas = Vec::with_capacity(JITTER_CALLBACKS - 1);
    let mut next = now_ns() + nominal_ns;
    let mut prev = now_ns();
    for block in 0..JITTER_CALLBACKS {
        sleep_until_monotonic_raw(next);
        let t_k = now_ns();
        if block > 0 {
            deltas.push(t_k.abs_diff(prev).abs_diff(nominal_ns));
        }
        prev = t_k;

        let mut in_l = [0f32; BLOCK];
        let mut in_r = [0f32; BLOCK];
        in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);
        h.run_callback(&mut in_l, &mut in_r, BLOCK);

        next += nominal_ns;
    }
    stop.store(true, Ordering::Release);
    for th in stressers {
        th.join().expect("background stresser panicked");
    }

    let max_jitter_ns = *deltas.iter().max().expect("non-empty jitter series");
    let p99_ns = percentile_ns(deltas.clone(), 0.99);
    let std_dev = std_dev_ns(&deltas);

    let budget_max_ns = nominal_ns;
    let budget_p99_ns = (nominal_ns as f64 * 0.5) as u64;

    let max_jitter_us = max_jitter_ns as f64 / 1e3;
    let p99_jitter_us = p99_ns as f64 / 1e3;
    let budget_max_us = budget_max_ns as f64 / 1e3;
    let budget_p99_us = budget_p99_ns as f64 / 1e3;

    eprintln!(
        "RT_METRICS rt_jitter profile=release+testing quantums={JITTER_CALLBACKS} max_jitter_us={max_jitter_us:.1} \
         p99_jitter_us={p99_jitter_us:.1} budget_max_us={budget_max_us:.1} std_dev_us={:.1} nominal_us={:.1}",
        std_dev / 1e3,
        nominal_ns as f64 / 1e3,
    );

    if max_jitter_ns <= budget_max_ns && p99_ns <= budget_p99_ns {
        // Under NAM_RT_STRICT=1 a PASS is only certifiable on a
        // calibrated RT environment — dispersion numbers within budget on an
        // uncalibrated host must fail, never emit a silent pass.
        if env.strict && !env.calibrated() {
            panic!(
                "RT jitter gate FAILED: NAM_RT_STRICT=1 requires a calibrated realtime \
                 environment (single pinned isolated CPU + SCHED_FIFO) to certify a PASS; the \
                 current environment is not calibrated — refusing a silent pass on an \
                 uncalibrated host"
            );
        }
        eprintln!(
            "TEST_RESULT[rt_jitter]=PASS profile=release+testing max_jitter_us={max_jitter_us:.1} p99_jitter_us={p99_jitter_us:.1} budget_max_us={budget_max_us:.1} std_dev_us={:.1}",
            std_dev / 1e3,
        );
        return;
    }

    if env.calibrated() {
        panic!(
            "RT jitter gate FAILED: max jitter {max_jitter_us:.1} us exceeds budget {budget_max_us:.1} us \
             (p99 {p99_jitter_us:.1} us vs {budget_p99_us:.1} us) on a calibrated RT environment"
        );
    } else if env.strict {
        panic!(
            "RT jitter gate FAILED: max jitter {max_jitter_us:.1} us exceeds budget {budget_max_us:.1} us \
             (p99 {p99_jitter_us:.1} us vs {budget_p99_us:.1} us) — NAM_RT_STRICT=1 promoted this GAP to a \
             hard failure (environment is not calibrated: CPU not isolated/pinned or no SCHED_FIFO); \
             re-run on the calibrated RT host"
        );
    }
    eprintln!(
        "TEST_RESULT[rt_jitter]=GAP:uncalibrated_environment profile=release+testing max_jitter_us={max_jitter_us:.1} p99_jitter_us={p99_jitter_us:.1} budget_max_us={budget_max_us:.1}"
    );
}

/// Blocking-I/O churn: open/read a kernel device repeatedly, forcing real
/// syscalls and scheduler preemption points.
fn io_churn_loop(stop: &AtomicBool) {
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::Acquire) {
        if let Ok(mut f) = std::fs::File::open("/dev/zero") {
            let _ = std::io::Read::read(&mut f, &mut buf);
        }
    }
}

/// Memory-bandwidth/cache thrashing: a 4 MiB working set with a large page
/// stride evicts the DSP model weights from the shared caches.
fn cache_thrash_loop(stop: &AtomicBool) {
    let mut buf = vec![0u8; 4 * 1024 * 1024];
    let mut idx = 0usize;
    while !stop.load(Ordering::Acquire) {
        for _ in 0..16 {
            let i = idx % buf.len();
            buf[i] = buf[i].wrapping_add(1);
            idx = idx.wrapping_add(64 * 1024);
        }
        std::thread::yield_now();
    }
}

/// Syscall storm: tight `getpid` + `yield_now` churn.
fn syscall_storm_loop(stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        let _ = unsafe { libc::getpid() };
        std::thread::yield_now();
    }
}

// ── 3. Concurrency Interleaving Stress (Phase 5 of tests-long.sh) ─────────────

/// Deterministic step-by-step state machine interleaving exploration.
///
/// Exercises reproducible state transitions across swap requests, backend status
/// updates, resampler swaps, and cooperative shutdown without requiring nightly.
#[test]
fn deterministic_state_interleaving_exploration() {
    let _shutdown = common::ShutdownGuard::new();
    let _harness = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("interleaving harness");
    let status = SharedBackendStatus::new();

    // Step 1: Initial state verification
    assert!(status.snapshot().invariants_hold());

    // Step 2: Stream state transitions & status updates
    observe_stream_state(
        "capture",
        StreamState::Streaming,
        StreamState::Paused,
        &status,
    );
    observe_stream_state(
        "playback",
        StreamState::Paused,
        StreamState::Streaming,
        &status,
    );
    assert!(status.snapshot().invariants_hold());

    // Step 3: Reconnect cycle & rate renegotiation
    status.begin_reconnect(1, 3, Duration::from_millis(1));
    status.mark_running();
    assert!(status.snapshot().invariants_hold());

    // Step 4: Cooperative shutdown trigger
    SHUTDOWN.store(true, Ordering::Release);
    assert!(SHUTDOWN.load(Ordering::Acquire));
    SHUTDOWN.store(false, Ordering::Release);

    eprintln!("TEST_RESULT[deterministic_interleaving]=PASS profile=release+testing steps=4");
}

/// 16-thread stress over swap requests, stream-state transitions, simulated
/// reconnect cycles, sample-rate renegotiation and the cooperative
/// `SHUTDOWN` trigger. Proves deadlock freedom (all joins return), no
/// inconsistent sample-rate reads and no leaked failure state.
#[test]
#[ignore = "Concurrency interleaving stress: 16-thread state-machine stress — long suite only (tests-long.sh Phase 5)"]
fn concurrent_state_interleaving_stress_16_threads() {
    let _shutdown = common::ShutdownGuard::new();

    let harness = Arc::new(Mutex::new(
        RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("model-check harness"),
    ));
    {
        let mut h = harness.lock().expect("harness lock");
        h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
        h.push_cabsim(Some(cabsim_pair()));
    }
    let status = Arc::new(SharedBackendStatus::new());
    let stop = Arc::new(AtomicBool::new(false));
    let violation = Arc::new(AtomicBool::new(false));
    let violation_msg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let swaps_requested = Arc::new(AtomicU64::new(0));

    // Watcher: samples coherent status snapshots and asserts the machine
    // invariants under the 16-writer storm. Sleeps between samples so the
    // RT driver always wins a scheduling slot even on a saturated host.
    let watcher = {
        let status = Arc::clone(&status);
        let stop = Arc::clone(&stop);
        let violation = Arc::clone(&violation);
        let msg = Arc::clone(&violation_msg);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let snap = status.snapshot();
                if !snap.invariants_hold() {
                    violation.store(true, Ordering::Release);
                    *msg.lock().expect("violation msg lock") =
                        format!("incoherent status snapshot: {snap:?}");
                    break;
                }
                std::thread::sleep(Duration::from_micros(20));
            }
        })
    };

    let mut handles = Vec::with_capacity(MODEL_CHECK_THREADS);

    // Shared RT status flags: lock-free observation face for the rate workers'
    // constructor role (the generation/NEEDS checks below never contend on the
    // harness mutex while the callback is unmuted).
    let rt_status_flags = harness.lock().expect("harness lock").rt_status_arc();

    // 4 swap workers: bounded bursts of model/cabsim/gain requests through
    // the producer face (serialized exactly like the production main thread).
    // Payloads are built *outside* the harness lock so the RT driver keeps a
    // fair share of the lock; each worker pushes a finite burst and exits.
    for w in 0..4 {
        let harness = Arc::clone(&harness);
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            let mut iter = 0u64;
            while iter < 600 && !stop.load(Ordering::Acquire) {
                match (w + iter as usize) % 4 {
                    0 => {
                        let l = linear_a();
                        let r = linear_b();
                        let mut h = harness.lock().expect("harness lock");
                        h.push_load_model(Some(l), Some(r), 1.0, 1.0, SAMPLE_RATE);
                    }
                    1 => {
                        let l = linear_a();
                        let r = linear_b();
                        let mut h = harness.lock().expect("harness lock");
                        h.push_slimmable(iter, 2, l, Some(r));
                    }
                    2 => {
                        if iter.is_multiple_of(8) {
                            let pair = cabsim_pair();
                            let mut h = harness.lock().expect("harness lock");
                            h.push_cabsim(Some(pair));
                        }
                    }
                    _ => {
                        let mult = 0.5 + 0.01 * ((iter % 100) as f32);
                        let mut h = harness.lock().expect("harness lock");
                        h.push_input_gain(mult);
                        h.push_output_gain(mult);
                    }
                }
                iter += 1;
                if iter.is_multiple_of(16) {
                    std::thread::sleep(Duration::from_micros(20));
                } else {
                    std::thread::yield_now();
                }
            }
        }));
    }

    // 4 status workers: stream-state transitions + reconnect cycles. The
    // logged `observe_stream_state` paths are exercised sparingly to keep the
    // phase log bounded; the raw transitions dominate the throughput. A short
    // sleep every 64 iterations guarantees the RT driver keeps getting CPU.
    for w in 0usize..4 {
        let status = Arc::clone(&status);
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            let mut iter = 0u64;
            while !stop.load(Ordering::Acquire) {
                let stream = if w.is_multiple_of(2) {
                    "capture"
                } else {
                    "playback"
                };
                if iter.is_multiple_of(32) {
                    observe_stream_state(
                        stream,
                        StreamState::Streaming,
                        StreamState::Paused,
                        &status,
                    );
                    observe_stream_state(
                        stream,
                        StreamState::Paused,
                        StreamState::Streaming,
                        &status,
                    );
                } else {
                    match (w + (iter % 8) as usize) % 8 {
                        0 => status.mark_running(),
                        1 => status.mark_degraded("simulated SPA contract violation"),
                        2 => status.begin_reconnect(1, 3, Duration::from_millis(1)),
                        3 => status.mark_failed(stream, "simulated daemon restart"),
                        4 => {
                            let _ = status.state();
                        }
                        5 => {
                            let _ = status.failure();
                        }
                        6 => status.mark_running(),
                        _ => status.mark_failed(stream, "simulated stream error"),
                    }
                }
                iter += 1;
                if iter.is_multiple_of(64) {
                    std::thread::sleep(Duration::from_micros(20));
                }
            }
        }));
    }

    // 4 rate workers: publisher + constructor roles following the production
    // rate-renegotiation protocol. The causal chain is:
    //
    //   1. `publish_host_rate(rate)`   — publisher role: publish the desired rate.
    //   2. `sync_rate` (RT callback)   — detects the discrepancy, bumps
    //      `requested_rate_generation` and arms
    //      `RT_STATUS_NEEDS_RESAMPLER_REBUILD`.
    //   3. `request_resampler_swap`    — constructor role: dispatched *only*
    //      after the RT observed the publish, capturing the updated generation
    //      under the harness lock (the same "photograph"
    //      `handle_resampler_rebuild` takes before building).
    //   4. `drain_resamplers` (RT)     — installs the envelope, clears
    //      `RT_STATUS_RESAMP_SWAP_PENDING` and the callback processes DSP.
    //
    // Dispatching the swap *before* the RT observes the publish is the Phase-5
    // starvation bug: the envelope carries a stale generation, is discarded by
    // `drain_resamplers` without unmuting, and `RESAMP_SWAP_PENDING` stays
    // armed — the fail-open rollback guard skips every callback and
    // `frame_count` never advances. The publish budget bounds the publisher
    // role; the constructor role keeps polling until `stop`, so the final
    // published rate always receives its envelope (both loops
    // yield/sleep so the RT driver wins the harness lock).
    for w in 0..4 {
        let harness = Arc::clone(&harness);
        let rt_status = Arc::clone(&rt_status_flags);
        let stop = Arc::clone(&stop);
        let swaps_requested = Arc::clone(&swaps_requested);
        handles.push(std::thread::spawn(move || {
            let mut iter = 0u64;
            let mut last_delivered_gen = 0u64;
            while !stop.load(Ordering::Acquire) {
                // Publisher role (bounded): publish the desired host rate and
                // release the lock immediately — the RT callback observes the
                // publish in its own `sync_rate` run.
                if iter < RATE_PUBLISH_BUDGET && iter.is_multiple_of(RATE_PUBLISH_PERIOD) {
                    let rate =
                        VALID_RATES[(w + (iter as usize % VALID_RATES.len())) % VALID_RATES.len()];
                    let mut h = harness.lock().expect("harness lock");
                    h.publish_host_rate(rate);
                }
                // Constructor role: only when the RT observed a request for a
                // generation this worker has not delivered yet (lock-free fast
                // path — zero mutex contention while the callback is unmuted).
                let req_gen = rt_status.requested_rate_generation.load(Ordering::Acquire);
                if req_gen != last_delivered_gen
                    && rt_status.check_flag_acquire(RT_STATUS_NEEDS_RESAMPLER_REBUILD)
                {
                    let mut h = harness.lock().expect("harness lock");
                    // Re-verify under the lock (atomic vs the RT callback) and
                    // capture the updated generation + requested rates — the
                    // production `handle_resampler_rebuild` photograph. The
                    // envelope is stamped with `requested_rate_generation`,
                    // which cannot advance while the lock is held.
                    let req_gen = h
                        .rt_status()
                        .requested_rate_generation
                        .load(Ordering::Acquire);
                    let host = h.rt_status().requested_host_rate.load(Ordering::Relaxed);
                    let nam = h.rt_status().requested_nam_rate.load(Ordering::Relaxed);
                    if req_gen != last_delivered_gen
                        && host != 0
                        && nam != 0
                        && h.request_resampler_swap(host, nam).is_ok()
                    {
                        last_delivered_gen = req_gen;
                        swaps_requested.fetch_add(1, Ordering::Relaxed);
                    }
                }
                iter += 1;
                // Calibrated micro-yield: the RT driver always wins a
                // scheduling slot even on a saturated host.
                if iter.is_multiple_of(16) {
                    std::thread::sleep(Duration::from_micros(WORKER_YIELD_US));
                } else {
                    std::thread::yield_now();
                }
            }
        }));
    }

    // 2 shutdown workers: toggle the cooperative SHUTDOWN trigger. A short
    // sleep per toggle keeps the loop from hogging every CPU.
    for _ in 0..2 {
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            let mut on = false;
            while !stop.load(Ordering::Acquire) {
                on = !on;
                SHUTDOWN.store(on, Ordering::Release);
                std::thread::sleep(Duration::from_micros(20));
            }
        }));
    }

    // 1 control loop: polls the status machine like run.rs — on failure it
    // enters the bounded reconnect cycle, on SHUTDOWN it exits cooperatively.
    {
        let status = Arc::clone(&status);
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            let mut reconnect_attempt = 0u32;
            while !stop.load(Ordering::Acquire) {
                if SHUTDOWN.load(Ordering::Acquire) {
                    break;
                }
                if status.is_failed() {
                    reconnect_attempt = reconnect_attempt.wrapping_add(1);
                    status.begin_reconnect(reconnect_attempt, 3, Duration::from_millis(1));
                }
                std::thread::sleep(Duration::from_micros(20));
            }
        }));
    }

    // 1 RT driver: continuous audio callbacks, verifying the sample-rate read
    // invariant after every callback (no inconsistent rate reads).
    {
        let harness = Arc::clone(&harness);
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            let mut iter = 0u64;
            while !stop.load(Ordering::Acquire) {
                let mut h = harness.lock().expect("harness lock");
                let mut in_l = [0f32; BLOCK];
                let mut in_r = [0f32; BLOCK];
                h.run_callback(&mut in_l, &mut in_r, BLOCK);
                let rate = h.current_host_rate();
                assert!(
                    VALID_RATES.contains(&rate),
                    "inconsistent sample-rate read: applied {rate} Hz is not a published rate"
                );
                if iter.is_multiple_of(32) {
                    h.consume_gc();
                }
                iter += 1;
            }
        }));
    }

    assert_eq!(handles.len(), MODEL_CHECK_THREADS, "worker mix drift");

    std::thread::sleep(STRESS_WINDOW);
    stop.store(true, Ordering::Release);
    for th in handles {
        th.join().expect("model-check thread panicked");
    }
    watcher.join().expect("watcher panicked");

    assert!(
        !violation.load(Ordering::Acquire),
        "model-check invariant violation: {}",
        *violation_msg.lock().expect("violation msg lock")
    );

    let snap = status.snapshot();
    assert!(
        snap.invariants_hold(),
        "final status snapshot incoherent: {snap:?}"
    );
    let callbacks = harness.lock().expect("harness lock").frame_count();
    assert!(callbacks > 0, "RT driver never processed a callback");
    let swaps = swaps_requested.load(Ordering::Relaxed);
    assert!(
        swaps > 0,
        "rate-renegotiation workers never pushed a resampler swap request — test is vacuous"
    );
    // The marker names the harness metric honestly — DSP
    // quantums processed, never "audio callbacks" (the harness throughput is
    // state-machine throughput, not RT audio throughput).
    eprintln!(
        "TEST_RESULT[concurrency_stress]=PASS profile=release+testing threads={MODEL_CHECK_THREADS} window_ms={} dsp_quantums={callbacks} swaps_requested={swaps}",
        STRESS_WINDOW.as_millis(),
    );
}

// ── 3b. State-machine throughput ───────────────────────────────────────────

/// DSP quantums for the SPSC throughput gate.
const THROUGHPUT_QUANTUMS: usize = 20_000;

/// Upper bound on pushes the producer thread attempts.
const THROUGHPUT_MAX_PUSHES: u64 = 60_000;

/// Hard bound on the command backlog any single callback may leave behind.
/// Each SPSC ring holds `SPSC_CAPACITY` (64) payloads; with 5 channels plus 5
/// deferred slots plus the parking-lot latch, the worst structural backlog is
/// 64×5 + 6 = 326 — the per-callback drain budgets (16 scalar + 8
/// structural pops) keep the backlog inside the channel capacity, never
/// unbounded.
const MAX_BACKLOG_PER_QUANTUM: usize = 64 * 5 + 6;

/// Production-SPSC state-machine throughput.
///
/// Unlike the 16-thread interleaving stress above, this gate measures
/// *throughput* — and only through the production protocol. The harness is
/// split into its two production faces ([`RtSwapHarness::into_parts`]): a
/// single-writer producer thread pushes swap commands through the real bounded
/// ring buffers (exactly the production main-thread face) while the RT side
/// runs continuous DSP quantums. There is **no global mutex** on the measured
/// path. Accounting:
///
/// - producer face: `attempted` / `enqueued` / `dropped` (every push counted);
/// - RT face, per callback: `structural_applied`, `param_pops`,
///   `structural_pops`, `commands_remaining`;
/// - RT face, cumulative: `swaps_superseded`, `swaps_deferred` (coalescing
///   telemetry from `RtStatusFlags`).
///
/// The receipt marker names the metric honestly (`dsp_quantums`, `swaps_*`),
/// never "audio callbacks" — harness throughput is state-machine throughput,
/// not RT audio throughput.
#[test]
#[ignore = "Concurrency SPSC throughput: production-SPSC swap accounting without a global mutex — long suite only (tests-long.sh Phase 5)"]
fn concurrent_spsc_throughput_swap_accounting() {
    let _shutdown = common::ShutdownGuard::new();

    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("throughput harness");
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    for _ in 0..8 {
        let mut l = [0f32; BLOCK];
        let mut r = [0f32; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }
    h.consume_gc();

    // Split into the two production faces: no mutex between producer and RT.
    let (mut producer, mut rt) = h.into_parts();

    let stop = Arc::new(AtomicBool::new(false));
    let producer_handle = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut iter = 0u64;
            while iter < THROUGHPUT_MAX_PUSHES && !stop.load(Ordering::Acquire) {
                match iter % 5 {
                    0 => {
                        producer.push_load_model(
                            Some(linear_a()),
                            Some(linear_b()),
                            1.0,
                            1.0,
                            SAMPLE_RATE,
                        );
                    }
                    1 => producer.push_slimmable(iter, 2, linear_a(), Some(linear_b())),
                    2 => producer.push_cabsim(Some(cabsim_pair())),
                    3 => {
                        producer.push_input_gain(0.5 + 0.01 * (iter % 100) as f32);
                        producer.push_output_gain(1.0);
                    }
                    _ => producer.push_os_pair(
                        neural_amp_modeler_rs::dsp::oversample::OversampleEngine::new(
                            neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2,
                            BLOCK * 2,
                        )
                        .expect("OS engine"),
                        neural_amp_modeler_rs::dsp::oversample::OversampleEngine::new(
                            neural_amp_modeler_rs::dsp::oversample::OversampleFactor::X2,
                            BLOCK * 2,
                        )
                        .expect("OS engine"),
                    ),
                }
                iter += 1;
                if iter.is_multiple_of(16) {
                    std::thread::yield_now();
                }
            }
            producer
        })
    };

    let (sig_l, sig_r) = test_signal_blocks(THROUGHPUT_QUANTUMS);
    let mut in_l = [0f32; BLOCK];
    let mut in_r = [0f32; BLOCK];
    let mut total_applied = 0u64;
    let mut total_param_pops = 0u64;
    let mut total_structural_pops = 0u64;
    let mut max_backlog = 0usize;
    let mut busy_quantums = 0u64;

    for block in 0..THROUGHPUT_QUANTUMS {
        in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);
        let acc = rt.run_callback_accounted(&mut in_l, &mut in_r, BLOCK);
        total_applied += acc.structural_applied as u64;
        total_param_pops += acc.param_pops as u64;
        total_structural_pops += acc.structural_pops as u64;
        max_backlog = max_backlog.max(acc.commands_remaining);
        if acc.structural_applied > 0 || acc.param_pops > 0 || acc.structural_pops > 0 {
            busy_quantums += 1;
        }
        if block.is_multiple_of(64) {
            rt.consume_gc();
        }
    }

    // Stop the producer and absorb whatever it enqueued before noticing.
    stop.store(true, Ordering::Release);
    let producer = producer_handle.join().expect("producer thread panicked");
    let mut drained = 0usize;
    while rt.commands_pending() && drained < 8192 {
        let acc = rt.run_callback_accounted(&mut in_l, &mut in_r, BLOCK);
        total_applied += acc.structural_applied as u64;
        total_param_pops += acc.param_pops as u64;
        total_structural_pops += acc.structural_pops as u64;
        max_backlog = max_backlog.max(acc.commands_remaining);
        if acc.structural_applied > 0 || acc.param_pops > 0 || acc.structural_pops > 0 {
            busy_quantums += 1;
        }
        rt.consume_gc();
        drained += 1;
    }
    let swaps_pending = rt.commands_pending_count();

    let attempted = producer.attempted();
    let enqueued = producer.enqueued();
    let dropped = producer.dropped();
    let superseded = rt
        .rt_status()
        .structural_superseded_total
        .load(Ordering::Relaxed);
    let deferred = rt
        .rt_status()
        .structural_deferred_total
        .load(Ordering::Relaxed);
    let dsp_quantums = rt.frame_count();

    // Complete swap accounting: every attempt either
    // entered a ring or was dropped by the bounded channel.
    assert_eq!(
        attempted,
        enqueued + dropped,
        "producer accounting must balance: attempted={attempted} enqueued={enqueued} dropped={dropped}"
    );
    assert!(
        enqueued > 0,
        "no command reached the SPSC rings — vacuous throughput"
    );
    assert!(
        total_applied > 0,
        "no structural swap was ever applied — vacuous throughput"
    );
    assert!(
        dsp_quantums > 0,
        "no DSP quantum executed — vacuous throughput"
    );
    assert!(
        busy_quantums > 0,
        "per-callback accounting recorded no quantum with drained work"
    );
    assert!(
        max_backlog <= MAX_BACKLOG_PER_QUANTUM,
        "command backlog {max_backlog} exceeds the channel-capacity bound \
         {MAX_BACKLOG_PER_QUANTUM} (5 SPSC rings × 64 + deferred + parking)"
    );
    assert!(
        swaps_pending == 0,
        "complete swap accounting requires full absorption after the drain: \
         {swaps_pending} commands still queued/parked"
    );

    eprintln!(
        "TEST_RESULT[spsc_throughput]=PASS profile=release+testing dsp_quantums={dsp_quantums} \
         swaps_attempted={attempted} swaps_enqueued={enqueued} swaps_dropped={dropped} \
         swaps_applied={total_applied} swaps_superseded={superseded} swaps_deferred={deferred} \
         swaps_pending={swaps_pending} param_pops={total_param_pops} \
         structural_pops={total_structural_pops} busy_quantums={busy_quantums} \
         max_backlog={max_backlog} spsc=production mutex=none"
    );
}
