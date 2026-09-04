// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Recording transport A/B benchmark: inline SPSC ring vs preallocated
//! pool + small-descriptor SPSC transport for the recording path.
//!
//! # What it measures
//!
//! Both transports run on the **same host, same CPU pair, same configuration**:
//!
//! * **Producer-side op** per quantum (copy `L/R` into the payload + enqueue):
//!   min/mean/p50/p99/p99.9/max, in cycles and nanoseconds.
//! * **Consumer-side op** per quantum (dequeue + consume into the WAV I/O
//!   buffer + return): same distribution.
//! * **Recording latency per quantum** = producer op + consumer op (SPSC FIFO
//!   aligns the samples by index) — the metric the promotion rule is
//!   judged on: promote only if the pool's **p99 recording latency** is
//!   reproducibly **≥ 5 % lower** than inline's across all runs.
//! * **bytes/cycles** — copy bytes per cycle on the producer hot path.
//! * **Cache proxy** — the analytical 64 B line footprint per quantum per
//!   transport plus a measured cold-vs-warm single-pass copy cost on this host
//!   (hardware counters are collected separately via `sudo perf stat`, see the
//!   receipt header).
//! * **Shutdown integrity** — every published quantum must be consumed and
//!   released (`overruns == 0`, `leaked == 0`); the exactly-once free-ring
//!   drain (no ABA / no double-return) is proven by the pool unit tests and
//!   re-verified at the end of each measured phase.
//!
//! # Running
//!
//! ```sh
//! cargo run --features testing --release --bin recording_ab_bench -- --runs 3
//! sudo perf stat -e cycles,instructions,l1-dcache-loads,l1-dcache-load-misses,\
//!     cache-references,cache-misses,l2_request_g1.all_no_prefetch \
//!     target/release/recording_ab_bench --quanta 50000
//! ```
//!
//! The receipt is written to `target/logs/recording-ab-receipt.jsonl`
//! (override with `NAM_AB_RECEIPT`) plus a human-readable
//! `target/logs/recording-ab-receipt.txt`.
//!
//! > **Dev-environment caveat (per project guidance):** micro-benchmarks on a
//! > shared host are noisy; the A/B interleaves transports per run
//! > (`A,B,A,B,A,B`), truncates the first `WARMUP_SAMPLES` of every phase, and
//! > only promotes on a **reproducible** gain (every run ≥ 5 %).

use std::hint::black_box;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nam_audio_pipe::recording::buffer::{
    AlignedBlock, MAX_BLOCK_SIZE, RING_CAPACITY, RingPayload, create_audio_ring_buffer,
};
use nam_audio_pipe::recording::pool::{POOL_CAPACITY, RecordingPool};
use nam_audio_pipe::standalone::rt_setup::affinity::{get_allowed_cpus, select_optimal_cpu};

const DEFAULT_RECEIPT_JSONL: &str = "target/logs/recording-ab-receipt.jsonl";
const DEFAULT_RECEIPT_TXT: &str = "target/logs/recording-ab-receipt.txt";
const RECEIPT_ENV: &str = "NAM_AB_RECEIPT";

const QUANTUM_SIZES: [usize; 5] = [64, 256, 512, 2048, 8192];
const DEFAULT_QUANTA: usize = 50_000;
const DEFAULT_RUNS: usize = 3;
/// Samples truncated from the head of every measured phase (steady-state
/// warmup: caches and ring/pool fill levels settle before the first kept
/// sample).
const WARMUP_SAMPLES: usize = 2_000;
/// Producer pacing between quanta (µs), emulating the audio clock that paces
/// the RT thread. Large enough that the I/O consumer always keeps up, so the
/// measured per-quantum op is the pure transport cost (no backpressure wait in
/// the timed window, no overruns).
const DEFAULT_THROTTLE_US: u64 = 100;

/// Analytical 64 B line accesses per quantum on the inline data path
/// (fill scratch W + push R+W + pop R+W + write I/O R+W = 7 accesses).
const INLINE_LINE_ACCESSES: u64 = 7;
/// Analytical 64 B line accesses per quantum on the pool data path
/// (fill slot in place W + read slot into I/O R+W = 3 accesses).
const POOL_LINE_ACCESSES: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportFilter {
    Both,
    InlineOnly,
    PoolOnly,
}

#[derive(Debug, Clone)]
struct Cfg {
    runs: usize,
    quanta: usize,
    producer_cpu: Option<usize>,
    consumer_cpu: Option<usize>,
    throttle_us: u64,
    transport: TransportFilter,
    receipt_jsonl: PathBuf,
    receipt_txt: PathBuf,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            runs: DEFAULT_RUNS,
            quanta: DEFAULT_QUANTA,
            producer_cpu: None,
            consumer_cpu: None,
            throttle_us: DEFAULT_THROTTLE_US,
            transport: TransportFilter::Both,
            receipt_jsonl: PathBuf::from(DEFAULT_RECEIPT_JSONL),
            receipt_txt: PathBuf::from(DEFAULT_RECEIPT_TXT),
        }
    }
}

// ── TSC helpers ─────────────────────────────────────────────────────────────

/// Serialized `RDTSC` (LFENCE + RDTSC) raw cycle count.
#[inline(always)]
fn rdtsc_cycles() -> u64 {
    // SAFETY: `_mm_lfence` + `_rdtsc` are available on all x86-64 CPUs; they
    // perform no memory access and have no side effects beyond reading TSC.
    unsafe {
        core::arch::x86_64::_mm_lfence();
        core::arch::x86_64::_rdtsc()
    }
}

/// Measures the current TSC rate as fixed-point GHz × 1000 (cycles per ns ×
/// 1000), matching `neural_amp_modeler_rs::common::tsc`'s calibration method.
fn measure_freq_ghz_x1000() -> u64 {
    let _ = rdtsc_cycles();
    std::thread::sleep(Duration::from_millis(5));
    let t0 = rdtsc_cycles();
    let wall = Instant::now();
    std::thread::sleep(Duration::from_millis(120));
    let t1 = rdtsc_cycles();
    let ns = wall.elapsed().as_nanos().max(1) as u64;
    let cyc = t1.saturating_sub(t0);
    (cyc * 1000) / ns
}

/// Pins the calling thread to `cpu` (best-effort; the A/B still runs without
/// pinning when the topology/cpuset forbids it).
fn pin_thread(cpu: Option<usize>) {
    let Some(cpu) = cpu else {
        return;
    };
    // SAFETY: `cpu_set_t` is zeroed before use; `sched_setaffinity` only
    // touches the supplied set and the calling thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            eprintln!("  warn: could not pin thread to cpu {cpu}");
        }
    }
}

// ── Statistics ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Stats {
    n: usize,
    min: u64,
    mean: f64,
    p50: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

impl Stats {
    fn of(mut samples: Vec<u64>) -> Self {
        let n = samples.len();
        if n == 0 {
            return Self::default();
        }
        samples.sort_unstable();
        let idx = |p: f64| {
            let i = ((n as f64) * p).ceil() as usize;
            samples[i.min(n).saturating_sub(1)]
        };
        let sum: u128 = samples.iter().map(|&c| c as u128).sum();
        Self {
            n,
            min: samples[0],
            mean: sum as f64 / n as f64,
            p50: idx(0.50),
            p99: idx(0.99),
            p999: idx(0.999),
            max: *samples.last().expect("n > 0"),
        }
    }
}

// ── Per-quantum sample record ───────────────────────────────────────────────

struct TransportSamples {
    producer_cycles: Vec<u64>,
    consumer_cycles: Vec<u64>,
    /// Consumer-side dequeue-to-data-ready cycles (cross-thread vs the
    /// producer's t0; informational only — invariant-TSC caveat).
    dequeue_cycles: Vec<u64>,
    overruns: u64,
    leaked: u64,
}

fn consume_into_io(block: &AlignedBlock<MAX_BLOCK_SIZE>, io_buf: &mut [f32; MAX_BLOCK_SIZE]) {
    let n = block.valid_len();
    // The read must survive optimization: mark the source opaque, then perform
    // the same read+write the WAV writer does (planar → contiguous I/O buffer).
    black_box(block.as_slice());
    io_buf[..n].copy_from_slice(block.as_slice());
}

// ── Inline transport driver ─────────────────────────────────────────────────

fn run_inline_phase(cfg: &Cfg, barrier: &Barrier, l: &[f32], r: &[f32]) -> TransportSamples {
    thread::scope(|scope| {
        let (mut producer, mut consumer) =
            create_audio_ring_buffer::<MAX_BLOCK_SIZE>(RING_CAPACITY);
        let mut recording_block = AlignedBlock::<MAX_BLOCK_SIZE>::new_uninit();

        let producer_cpu = cfg.producer_cpu;
        let consumer_cpu = cfg.consumer_cpu;
        let quanta = cfg.quanta;
        let throttle_us = cfg.throttle_us;

        let producer_handle = scope.spawn(move || {
            pin_thread(producer_cpu);
            barrier.wait();
            let mut producer_cycles = Vec::with_capacity(quanta);
            let mut overruns = 0u64;
            for _ in 0..quanta {
                // Backpressure: never overrun — wait until the consumer has
                // room (the RT thread is paced by the audio clock; the bench
                // emulates that by blocking instead of dropping blocks). The
                // wait happens OUTSIDE the timed window, so `t0..t1` measures
                // only the copy+enqueue cost when space is guaranteed.
                while producer.is_full() {
                    std::hint::spin_loop();
                }
                let t0 = rdtsc_cycles();
                let mut block = std::mem::replace(
                    &mut recording_block,
                    AlignedBlock::<MAX_BLOCK_SIZE>::new_uninit(),
                );
                block.fill_planar(l, r);
                if producer.push(RingPayload::Audio(block)).is_err() {
                    // Defensive: SPSC guarantees push succeeds after is_full
                    // returned false; a violation is counted fail-closed.
                    overruns += 1;
                }
                let t1 = rdtsc_cycles();
                producer_cycles.push(t1 - t0);
                // Pacing OUTSIDE the timed window (audio-clock emulation).
                std::thread::sleep(Duration::from_micros(throttle_us));
            }
            // StreamStop: the consumer's terminal marker (pushed after `quanta`).
            while producer.is_full() {
                std::hint::spin_loop();
            }
            let _ = producer.push(RingPayload::StreamStop);
            (producer_cycles, overruns)
        });

        let consumer_handle = scope.spawn(move || {
            pin_thread(consumer_cpu);
            barrier.wait();
            let mut io_buf = [0f32; MAX_BLOCK_SIZE];
            let mut consumer_cycles = Vec::with_capacity(quanta);
            let mut dequeue_cycles = Vec::with_capacity(quanta);
            loop {
                let t0 = rdtsc_cycles();
                if let Ok(payload) = consumer.pop() {
                    match payload {
                        RingPayload::Audio(block) => {
                            let t1 = rdtsc_cycles();
                            consume_into_io(&block, &mut io_buf);
                            let t2 = rdtsc_cycles();
                            consumer_cycles.push(t2 - t0);
                            dequeue_cycles.push(t1 - t0);
                            if consumer_cycles.len() == quanta {
                                break;
                            }
                        }
                        RingPayload::StreamStop => break,
                        RingPayload::Metadata(_) => {}
                    }
                } else if consumer.is_abandoned() && consumer.is_empty() {
                    break;
                } else {
                    std::hint::spin_loop();
                }
            }
            // Integral drain up to the terminal marker.
            let mut extra = 0u64;
            loop {
                match consumer.pop() {
                    Ok(RingPayload::Audio(_)) => extra += 1,
                    Ok(RingPayload::StreamStop) | Ok(RingPayload::Metadata(_)) => break,
                    Err(_) if consumer.is_abandoned() && consumer.is_empty() => break,
                    Err(_) => std::hint::spin_loop(),
                }
            }
            black_box(&io_buf);
            (consumer_cycles, dequeue_cycles, extra)
        });

        let (producer_cycles, overruns) = producer_handle.join().expect("inline producer panicked");
        let (consumer_cycles, dequeue_cycles, extra) =
            consumer_handle.join().expect("inline consumer panicked");
        assert_eq!(extra, 0, "unexpected trailing blocks in inline run");
        truncate(
            &producer_cycles,
            &consumer_cycles,
            &dequeue_cycles,
            |p, c, d| TransportSamples {
                producer_cycles: p,
                consumer_cycles: c,
                dequeue_cycles: d,
                overruns,
                leaked: 0,
            },
        )
    })
}

// ── Pool transport driver ───────────────────────────────────────────────────

fn run_pool_phase(cfg: &Cfg, barrier: &Barrier, l: &[f32], r: &[f32]) -> TransportSamples {
    thread::scope(|scope| {
        let pool = RecordingPool::<POOL_CAPACITY>::new();
        let (mut producer, mut consumer) = pool.split();

        let producer_cpu = cfg.producer_cpu;
        let consumer_cpu = cfg.consumer_cpu;
        let quanta = cfg.quanta;
        let throttle_us = cfg.throttle_us;

        let producer_handle = scope.spawn(move || {
            pin_thread(producer_cpu);
            barrier.wait();
            let mut producer_cycles = Vec::with_capacity(quanta);
            let mut overruns = 0u64;
            for _ in 0..quanta {
                let t0 = rdtsc_cycles();
                // Backpressure (see the inline driver): `try_acquire` returns
                // immediately in steady state — the throttle keeps the
                // consumer ahead — and only spins on a transient backlog.
                let mut slot = loop {
                    if let Some(slot) = producer.try_acquire() {
                        break slot;
                    }
                    std::hint::spin_loop();
                };
                {
                    let block = slot.block_mut();
                    block.fill_planar(l, r);
                }
                if !slot.publish() {
                    // Defensive: publish is infallible at capacity N; a
                    // violation is counted fail-closed.
                    overruns += 1;
                }
                let t1 = rdtsc_cycles();
                producer_cycles.push(t1 - t0);
                // Pacing OUTSIDE the timed window (audio-clock emulation).
                std::thread::sleep(Duration::from_micros(throttle_us));
            }
            drop(producer);
            (producer_cycles, overruns)
        });

        let consumer_handle = scope.spawn(move || {
            pin_thread(consumer_cpu);
            barrier.wait();
            let mut io_buf = [0f32; MAX_BLOCK_SIZE];
            let mut consumer_cycles = Vec::with_capacity(quanta);
            let mut dequeue_cycles = Vec::with_capacity(quanta);
            let mut leaked = 0u64;
            loop {
                let t0 = rdtsc_cycles();
                if let Some(in_flight) = consumer.try_pop() {
                    let t1 = rdtsc_cycles();
                    consume_into_io(in_flight.block(), &mut io_buf);
                    if !in_flight.release() {
                        leaked += 1;
                    }
                    let t2 = rdtsc_cycles();
                    consumer_cycles.push(t2 - t0);
                    dequeue_cycles.push(t1 - t0);
                    if consumer_cycles.len() == quanta {
                        break;
                    }
                } else if consumer.work_is_abandoned() && consumer.work_is_empty() {
                    break;
                } else {
                    std::hint::spin_loop();
                }
            }
            black_box(&io_buf);
            (consumer_cycles, dequeue_cycles, leaked)
        });

        let (producer_cycles, overruns) = producer_handle.join().expect("pool producer panicked");
        let (consumer_cycles, dequeue_cycles, leaked) =
            consumer_handle.join().expect("pool consumer panicked");

        truncate(
            &producer_cycles,
            &consumer_cycles,
            &dequeue_cycles,
            |p, c, d| TransportSamples {
                producer_cycles: p,
                consumer_cycles: c,
                dequeue_cycles: d,
                overruns,
                leaked,
            },
        )
    })
}

/// Drops the first `WARMUP_SAMPLES` of every cycle vector so producer and
/// consumer samples stay index-aligned after steady state is reached.
fn truncate<F, T>(p: &[u64], c: &[u64], d: &[u64], build: F) -> T
where
    F: FnOnce(Vec<u64>, Vec<u64>, Vec<u64>) -> T,
{
    let drop_n = WARMUP_SAMPLES.min(p.len().min(c.len()));
    build(
        p[drop_n..].to_vec(),
        c[drop_n..].to_vec(),
        d[drop_n..].to_vec(),
    )
}

// ── Cache proxy ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct CacheProxy {
    lines_per_quantum_inline: u64,
    lines_per_quantum_pool: u64,
    warm_p99_cycles: u64,
    cold_p99_cycles: u64,
    /// Estimated per-quantum L1/L2/LLC refill penalty: extra line accesses ×
    /// measured cold-vs-warm cost per line.
    est_miss_cost_inline_cycles: u64,
    est_miss_cost_pool_cycles: u64,
}

fn measure_cache_proxy(frames: usize) -> CacheProxy {
    let bytes = frames * 2 * 4;
    let lines_per_pass = bytes.div_ceil(64) as u64;

    let src = vec![0.5f32; frames * 2];
    let mut dst = vec![0f32; frames * 2];
    for _ in 0..1000 {
        dst.copy_from_slice(&src);
    }
    black_box(&dst);

    // Warm steady-state single-pass copy cost.
    let mut warm = Vec::with_capacity(2000);
    for _ in 0..2000 {
        let t0 = rdtsc_cycles();
        dst.copy_from_slice(&src);
        let t1 = rdtsc_cycles();
        warm.push(t1 - t0);
    }
    let warm_stats = Stats::of(warm);

    // Cold: flush every 64 B line of the destination before the copy, so the
    // pass pays the L1/L2/LLC refill penalty of this host's copy engine.
    let mut cold = Vec::with_capacity(2000);
    for _ in 0..2000 {
        for chunk in dst.as_chunks_mut::<16>().0 {
            // SAFETY: every element of `dst` is in-bounds, mutable memory;
            // `_mm_clflush` has no side effects beyond invalidating the cache
            // line containing the address.
            unsafe { core::arch::x86_64::_mm_clflush(chunk.as_mut_ptr() as *const u8) };
        }
        let t0 = rdtsc_cycles();
        dst.copy_from_slice(&src);
        let t1 = rdtsc_cycles();
        cold.push(t1 - t0);
    }
    let cold_stats = Stats::of(cold);
    black_box(&dst);

    let delta = cold_stats.p99.saturating_sub(warm_stats.p99);
    let cost_per_line = delta.div_ceil(lines_per_pass).max(1);

    CacheProxy {
        lines_per_quantum_inline: INLINE_LINE_ACCESSES * lines_per_pass,
        lines_per_quantum_pool: POOL_LINE_ACCESSES * lines_per_pass,
        warm_p99_cycles: warm_stats.p99,
        cold_p99_cycles: cold_stats.p99,
        est_miss_cost_inline_cycles: INLINE_LINE_ACCESSES * lines_per_pass * cost_per_line,
        est_miss_cost_pool_cycles: POOL_LINE_ACCESSES * lines_per_pass * cost_per_line,
    }
}

// ── Dependency-free JSON (receipt) ──────────────────────────────────────────

#[derive(Debug, Clone)]
enum JsonValue {
    Bool(bool),
    Int(u64),
    Num(f64),
    Str(String),
    Obj(Vec<(String, JsonValue)>),
}

impl JsonValue {
    fn render(&self, out: &mut String) {
        match self {
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(n) => out.push_str(&n.to_string()),
            Self::Num(n) => out.push_str(&format!("{n:.2}")),
            Self::Str(s) => {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Self::Obj(items) => {
                out.push('{');
                for (i, (key, value)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    Self::Str(key.clone()).render(out);
                    out.push(':');
                    value.render(out);
                }
                out.push('}');
            }
        }
    }
}

impl std::fmt::Display for JsonValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut out = String::new();
        self.render(&mut out);
        f.write_str(&out)
    }
}

fn kv(key: &str, value: JsonValue) -> (String, JsonValue) {
    (key.to_string(), value)
}

fn obj(items: Vec<(String, JsonValue)>) -> JsonValue {
    JsonValue::Obj(items)
}

// ── Per-size A/B report ─────────────────────────────────────────────────────

struct SizeReport<'a> {
    frames: usize,
    freq_ghz_x1000: u64,
    runs_inline: &'a [TransportSamples],
    runs_pool: &'a [TransportSamples],
    cache: CacheProxy,
}

impl SizeReport<'_> {
    fn to_ns(&self, cycles: u64) -> u64 {
        cycles * 1000 / self.freq_ghz_x1000
    }

    fn recording_latency_ns(&self, s: &TransportSamples) -> Vec<u64> {
        s.producer_cycles
            .iter()
            .zip(&s.consumer_cycles)
            .map(|(&p, &c)| self.to_ns(p + c))
            .collect()
    }

    fn bytes_per_quantum(&self) -> f64 {
        (self.frames * 2 * 4) as f64
    }
}

fn percent_delta(new: f64, old: f64) -> f64 {
    if old == 0.0 {
        0.0
    } else {
        (new - old) / old * 100.0
    }
}

fn host_context(freq_ghz_x1000: u64) -> Vec<(String, JsonValue)> {
    let mut ctx = vec![kv(
        "tsc_ghz",
        JsonValue::Str(format!("{:.3}", freq_ghz_x1000 as f64 / 1000.0)),
    )];
    for (key, file) in [
        (
            "perf_event_paranoid",
            "/proc/sys/kernel/perf_event_paranoid",
        ),
        ("loadavg", "/proc/loadavg"),
    ] {
        if let Ok(s) = std::fs::read_to_string(file) {
            ctx.push(kv(key, JsonValue::Str(s.trim().into())));
        }
    }
    if let Ok(s) = std::fs::read_to_string("/proc/cpuinfo")
        && let Some(line) = s.lines().find(|l| l.starts_with("model name"))
        && let Some((_, name)) = line.split_once(':')
    {
        ctx.push(kv("cpu", JsonValue::Str(name.trim().into())));
    }
    let n = std::thread::available_parallelism()
        .map(|v| v.get().to_string())
        .unwrap_or_else(|_| "?".into());
    ctx.push(kv("nproc", JsonValue::Str(n)));
    ctx
}

fn main() -> ExitCode {
    let cfg = parse_args();
    let freq = measure_freq_ghz_x1000();

    eprintln!(
        "NAM recording A/B — runs={} quanta={} freq={:.3} GHz transports=inline,pool",
        cfg.runs,
        cfg.quanta,
        freq as f64 / 1000.0
    );
    if let Some(c) = cfg.producer_cpu {
        eprintln!("  producer cpu = {c}");
    }
    if let Some(c) = cfg.consumer_cpu {
        eprintln!("  consumer cpu = {c}");
    }

    let mut all_ok = true;
    let mut jsonl_lines: Vec<JsonValue> = Vec::new();
    let mut txt = String::new();
    txt.push_str(&format!(
        "NAM-Audio-Pipe recording transport A/B\n\
         ======================================\n\
         runs={} quanta={} freq={:.3} GHz (measured)\n",
        cfg.runs,
        cfg.quanta,
        freq as f64 / 1000.0
    ));

    let mut per_size: Vec<(
        usize,
        Vec<TransportSamples>,
        Vec<TransportSamples>,
        CacheProxy,
    )> = Vec::new();

    for &frames in &QUANTUM_SIZES {
        let l: Vec<f32> = vec![0.5f32; frames];
        let r: Vec<f32> = vec![-0.5f32; frames];
        let cache = measure_cache_proxy(frames);

        let mut runs_inline = Vec::with_capacity(cfg.runs);
        let mut runs_pool = Vec::with_capacity(cfg.runs);
        for _ in 0..cfg.runs {
            // Interleaved per run so both transports share the same host noise
            // window. With `--transport` only the selected half runs (used to
            // attribute hardware counters per transport under `perf stat`).
            let (a, b) = match cfg.transport {
                TransportFilter::Both => {
                    let barrier = Arc::new(Barrier::new(2));
                    let a = run_inline_phase(&cfg, &barrier, &l, &r);
                    let barrier = Arc::new(Barrier::new(2));
                    let b = run_pool_phase(&cfg, &barrier, &l, &r);
                    (a, b)
                }
                TransportFilter::InlineOnly => {
                    let barrier = Arc::new(Barrier::new(2));
                    let a = run_inline_phase(&cfg, &barrier, &l, &r);
                    let barrier = Arc::new(Barrier::new(2));
                    let b = run_inline_phase(&cfg, &barrier, &l, &r);
                    (a, b)
                }
                TransportFilter::PoolOnly => {
                    let barrier = Arc::new(Barrier::new(2));
                    let a = run_pool_phase(&cfg, &barrier, &l, &r);
                    let barrier = Arc::new(Barrier::new(2));
                    let b = run_pool_phase(&cfg, &barrier, &l, &r);
                    (a, b)
                }
            };
            runs_inline.push(a);
            runs_pool.push(b);
        }
        per_size.push((frames, runs_inline, runs_pool, cache));
    }

    for (frames, runs_inline, runs_pool, cache) in &per_size {
        let report = SizeReport {
            frames: *frames,
            freq_ghz_x1000: freq,
            runs_inline,
            runs_pool,
            cache: *cache,
        };
        let ok = emit_size_report(&report, &mut jsonl_lines, &mut txt, cfg.transport);
        all_ok &= ok;
    }

    // Promotion rule (single, self-consistent): PROMOTE only when the A/B
    // ran both transports and every run at every quantum size met the gate —
    // zero overruns, zero leaked slots, and a >= 5 % p99 recording-latency gain
    // (`run_ok` in `emit_size_report` already encodes integrity + gain). Any
    // other case keeps the inline transport (invariant: the inline
    // ring is never replaced without a reproducible gain).
    let promote = cfg.transport == TransportFilter::Both && all_ok;
    let verdict = match cfg.transport {
        TransportFilter::Both if promote => {
            "PROMOTE: pool+descriptor with reproducible p99 gain >= 5% across all runs/sizes"
        }
        TransportFilter::Both => "KEEP_INLINE: gain not reproducible (keep inline transport)",
        TransportFilter::InlineOnly => "FILTERED: single transport (inline) — no promotion verdict",
        TransportFilter::PoolOnly => "FILTERED: single transport (pool) — no promotion verdict",
    };

    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut header = vec![
        kv("event", JsonValue::Str("header".into())),
        kv("suite", JsonValue::Str("recording-ab".into())),
        kv("verdict", JsonValue::Str(verdict.into())),
        kv("promote", JsonValue::Bool(promote)),
        kv("runs", JsonValue::Int(cfg.runs as u64)),
        kv("quanta", JsonValue::Int(cfg.quanta as u64)),
        kv(
            "transport",
            JsonValue::Str(
                match cfg.transport {
                    TransportFilter::Both => "both",
                    TransportFilter::InlineOnly => "inline",
                    TransportFilter::PoolOnly => "pool",
                }
                .into(),
            ),
        ),
        kv("ts", JsonValue::Int(started)),
    ];
    header.extend(host_context(freq));

    let mut jsonl = String::new();
    jsonl.push_str(&obj(header).to_string());
    jsonl.push('\n');
    for line in &jsonl_lines {
        jsonl.push_str(&line.to_string());
        jsonl.push('\n');
    }
    jsonl.push_str(
        &obj(vec![
            kv("event", JsonValue::Str("verdict".into())),
            kv("verdict", JsonValue::Str(verdict.into())),
            kv("promote", JsonValue::Bool(promote)),
            kv("ok", JsonValue::Bool(all_ok)),
        ])
        .to_string(),
    );
    jsonl.push('\n');

    txt.push_str(&format!("\nVERDICT: {verdict}\n"));

    if let Some(dir) = cfg.receipt_jsonl.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&cfg.receipt_jsonl, &jsonl) {
        eprintln!(
            "error: cannot write receipt {}: {e}",
            cfg.receipt_jsonl.display()
        );
        return ExitCode::FAILURE;
    }
    eprintln!("receipt: {}", cfg.receipt_jsonl.display());
    let _ = std::fs::write(&cfg.receipt_txt, &txt);

    eprintln!("VERDICT: {verdict}");
    eprintln!("PROMOTE={promote}");
    ExitCode::SUCCESS
}

fn emit_size_report(
    report: &SizeReport<'_>,
    lines: &mut Vec<JsonValue>,
    txt: &mut String,
    transport: TransportFilter,
) -> bool {
    let frames = report.frames;
    let bytes = report.bytes_per_quantum();
    let promotion_eligible = transport == TransportFilter::Both;
    let mut ok = true;

    let mut section = format!(
        "\n── quantum = {frames} frames ({:.0} KiB payload) ──────────────\n",
        bytes / 1024.0
    );

    let runs = report.runs_inline.len().min(report.runs_pool.len());
    for i in 0..runs {
        let inl = &report.runs_inline[i];
        let pool = &report.runs_pool[i];
        let inl_rec = Stats::of(report.recording_latency_ns(inl));
        let pool_rec = Stats::of(report.recording_latency_ns(pool));
        let inl_prod = Stats::of(
            inl.producer_cycles
                .iter()
                .map(|&c| report.to_ns(c))
                .collect(),
        );
        let pool_prod = Stats::of(
            pool.producer_cycles
                .iter()
                .map(|&c| report.to_ns(c))
                .collect(),
        );
        let inl_cons = Stats::of(
            inl.consumer_cycles
                .iter()
                .map(|&c| report.to_ns(c))
                .collect(),
        );
        let pool_cons = Stats::of(
            pool.consumer_cycles
                .iter()
                .map(|&c| report.to_ns(c))
                .collect(),
        );

        // Positive = pool faster. Reproducibility: this exact run must show
        // >= 5 % p99 gain with zero overruns and zero leaks (only meaningful
        // for the A/B; single-transport runs check integrity alone).
        let gain = -percent_delta(pool_rec.p99 as f64, inl_rec.p99 as f64);
        let integrity_ok =
            inl.overruns == 0 && pool.overruns == 0 && inl.leaked == 0 && pool.leaked == 0;
        let run_ok = integrity_ok && (!promotion_eligible || gain >= 5.0);
        ok &= run_ok;

        section.push_str(&format!(
            "  run {}: p99 recording-latency inline={} ns pool={} ns → ganho {gain:+.2}% ({})\n",
            i + 1,
            inl_rec.p99,
            pool_rec.p99,
            if run_ok { "ok" } else { "FAIL" }
        ));
        section.push_str(&format!(
            "     producer p99: inline={} ns pool={} ns | consumer p99: inline={} ns pool={} ns\n",
            inl_prod.p99, pool_prod.p99, inl_cons.p99, pool_cons.p99
        ));
        section.push_str(&format!(
            "     overruns: inline={} pool={} | leaked slots: pool={}\n",
            inl.overruns, pool.overruns, pool.leaked
        ));

        let inl_deq = Stats::of(
            inl.dequeue_cycles
                .iter()
                .map(|&c| report.to_ns(c))
                .collect(),
        );
        let pool_deq = Stats::of(
            pool.dequeue_cycles
                .iter()
                .map(|&c| report.to_ns(c))
                .collect(),
        );

        lines.push(obj(vec![
            kv("event", JsonValue::Str("size_run".into())),
            kv("quantum_frames", JsonValue::Int(frames as u64)),
            kv("run", JsonValue::Int(i as u64 + 1)),
            kv(
                "inline",
                obj(vec![
                    kv("p99_recording_ns", JsonValue::Int(inl_rec.p99)),
                    kv("p99_producer_ns", JsonValue::Int(inl_prod.p99)),
                    kv("p99_consumer_ns", JsonValue::Int(inl_cons.p99)),
                    kv("p99_dequeue_ns", JsonValue::Int(inl_deq.p99)),
                    kv("max_recording_ns", JsonValue::Int(inl_rec.max)),
                    kv("overruns", JsonValue::Int(inl.overruns)),
                ]),
            ),
            kv(
                "pool",
                obj(vec![
                    kv("p99_recording_ns", JsonValue::Int(pool_rec.p99)),
                    kv("p99_producer_ns", JsonValue::Int(pool_prod.p99)),
                    kv("p99_consumer_ns", JsonValue::Int(pool_cons.p99)),
                    kv("p99_dequeue_ns", JsonValue::Int(pool_deq.p99)),
                    kv("max_recording_ns", JsonValue::Int(pool_rec.max)),
                    kv("overruns", JsonValue::Int(pool.overruns)),
                    kv("leaked", JsonValue::Int(pool.leaked)),
                ]),
            ),
            kv("p99_gain_pct", JsonValue::Num(gain)),
            kv("ok", JsonValue::Bool(run_ok)),
        ]));
    }

    // bytes/cycles on the producer hot path (mean across runs).
    let inl_prod_mean_cyc: f64 = report
        .runs_inline
        .iter()
        .map(|s| Stats::of(s.producer_cycles.clone()).mean)
        .sum::<f64>()
        / report.runs_inline.len().max(1) as f64;
    let pool_prod_mean_cyc: f64 = report
        .runs_pool
        .iter()
        .map(|s| Stats::of(s.producer_cycles.clone()).mean)
        .sum::<f64>()
        / report.runs_pool.len().max(1) as f64;
    section.push_str(&format!(
        "  bytes/cycles (producer): inline={:.2} pool={:.2} (bytes={:.0})\n",
        bytes / inl_prod_mean_cyc,
        bytes / pool_prod_mean_cyc,
        bytes
    ));

    let c = &report.cache;
    section.push_str(&format!(
        "  cache proxy: 64B/quantum lines inline={} pool={} | 1-step copy p99 warm={} cycles cold={} cycles\n",
        c.lines_per_quantum_inline, c.lines_per_quantum_pool, c.warm_p99_cycles, c.cold_p99_cycles
    ));
    section.push_str(&format!(
        "  est. miss cost/quantum: inline≈{} cycles pool≈{} cycles (Δ={} cycles)\n",
        c.est_miss_cost_inline_cycles,
        c.est_miss_cost_pool_cycles,
        c.est_miss_cost_inline_cycles
            .saturating_sub(c.est_miss_cost_pool_cycles)
    ));

    lines.push(obj(vec![
        kv("event", JsonValue::Str("size_summary".into())),
        kv("quantum_frames", JsonValue::Int(frames as u64)),
        kv("bytes_per_quantum", JsonValue::Int(bytes as u64)),
        kv(
            "cache_proxy",
            obj(vec![
                kv("lines_inline", JsonValue::Int(c.lines_per_quantum_inline)),
                kv("lines_pool", JsonValue::Int(c.lines_per_quantum_pool)),
                kv("warm_p99_cycles", JsonValue::Int(c.warm_p99_cycles)),
                kv("cold_p99_cycles", JsonValue::Int(c.cold_p99_cycles)),
                kv(
                    "est_miss_cost_inline_cycles",
                    JsonValue::Int(c.est_miss_cost_inline_cycles),
                ),
                kv(
                    "est_miss_cost_pool_cycles",
                    JsonValue::Int(c.est_miss_cost_pool_cycles),
                ),
            ]),
        ),
        kv("ok", JsonValue::Bool(ok)),
    ]));

    txt.push_str(&section);
    ok
}

// ── CLI ─────────────────────────────────────────────────────────────────────

fn parse_args() -> Cfg {
    let mut cfg = Cfg::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => {
                cfg.runs = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--runs <N>");
            }
            "--quanta" => {
                cfg.quanta = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--quanta <N>");
            }
            "--producer-cpu" => {
                cfg.producer_cpu = args.next().and_then(|v| v.parse().ok());
            }
            "--consumer-cpu" => {
                cfg.consumer_cpu = args.next().and_then(|v| v.parse().ok());
            }
            "--throttle-us" => {
                cfg.throttle_us = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--throttle-us <µs>");
            }
            "--transport" => {
                cfg.transport = match args.next().as_deref() {
                    Some("inline") => TransportFilter::InlineOnly,
                    Some("pool") => TransportFilter::PoolOnly,
                    Some("both") => TransportFilter::Both,
                    other => panic!("--transport must be inline|pool|both, got {other:?}"),
                };
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: recording_ab_bench [--runs N] [--quanta N] \
                     [--producer-cpu N] [--consumer-cpu N] [--throttle-us µs] \
                     [--transport inline|pool|both]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument {other} (see --help)"),
        }
    }
    if let Ok(path) = std::env::var(RECEIPT_ENV) {
        cfg.receipt_jsonl = PathBuf::from(path);
        cfg.receipt_txt = cfg.receipt_jsonl.with_extension("txt");
    }
    if cfg.producer_cpu.is_none() {
        cfg.producer_cpu = select_optimal_cpu();
    }
    if cfg.consumer_cpu.is_none() {
        let allowed = get_allowed_cpus();
        cfg.consumer_cpu = allowed
            .first()
            .copied()
            .filter(|&c| Some(c) != cfg.producer_cpu)
            .or_else(|| allowed.get(1).copied());
    }
    assert!(cfg.runs >= 1, "--runs must be >= 1");
    assert!(cfg.quanta >= 1000, "--quanta must be >= 1000");
    cfg
}

#[cfg(test)]
#[path = "recording_ab_bench_test.rs"]
mod tests;
