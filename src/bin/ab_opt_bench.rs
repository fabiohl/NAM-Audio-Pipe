// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! A/B optimization benchmark — measures the DSP hot-path envelope of the
//! **current binary** so `plain → PGO → PGO+BOLT` builds can be compared.
//!
//! # What it measures
//!
//! Runs a deterministic, fixed DSP workload (the same cell configuration across
//! every measured binary) in a tight loop and retains, per run:
//!
//! * **Per-block cycles** (serialized `RDTSC`) — min/mean/p50/p99/p999/max in
//!   cycles and nanoseconds. The **p99 and max** are the tail latency the
//!   promotion rule is judged on.
//! * **PMU counters** (via `perf_event_open`, when the kernel/paranoid level
//!   permits): `cycles`, `instructions`, **iTLB misses** and **I-cache
//!   misses**, accumulated over the whole measured window. Typed
//!   availability: an unsupported/permission-denied event is recorded as
//!   `"unavailable"` with the cause — never silently zero.
//!
//! The harness itself is compiled three ways by
//! [`utils/ab-opt-ceremony.sh`](../utils/ab-opt-ceremony.sh) (plain, PGO,
//! PGO+BOLT) and each emits a per-variant receipt; the ceremony compares them
//! and declares whether BOLT **proves** a gain over PGO (rollback:
//! PGO-ONLY is the explicit fallback when BOLT does not).
//!
//! # Running
//!
//! ```sh
//! cargo run --features testing --release --bin ab_opt_bench \
//!   -- --variant plain --runs 3 --blocks 20000
//! ```
//!
//! Receipt: `target/logs/ab-opt-receipt-<variant>.json` (+ `.txt`), override
//! with `NAM_AB_OPT_RECEIPT`.
//!
//! > **Dev-environment caveat (per project guidance):** micro-benchmarks on a
//! > shared host are noisy. The harness truncates a warmup prefix of every run
//! > and pins to the optimal CPU (best-effort); the promotion rule still
//! > requires a reproducible gain across all runs before BOLT is announced.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use neural_amp_modeler_rs::common::params::AdaptiveComputeMode;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::{
    BridgeBuffer, DspBridge, DspBridgeWriter, DspBuffers, DspPipelineContext, MAX_RESAMP_BUF,
    capture_dsp_pipeline,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::loader::dispatcher::build_model;
use neural_amp_modeler_rs::loader::nam_json::parse_nam_json;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use neural_amp_modeler_rs::testing::stress::generate_stress_signal_v2_default;

use nam_audio_pipe::standalone::rt_setup::affinity::select_optimal_cpu;

const IR_FILENAME: &str = "cabsim_ir_pgo.wav";

const DEFAULT_RECEIPT_JSON: &str = "target/logs/ab-opt-receipt.json";
const RECEIPT_ENV: &str = "NAM_AB_OPT_RECEIPT";

const DEFAULT_RATE: u32 = 48_000;
const DEFAULT_QUANTUM: usize = 64;
const DEFAULT_OS: &str = "Off";
const DEFAULT_BLOCKS: u64 = 20_000;
const DEFAULT_WARMUP: u64 = 2_000;
const DEFAULT_RUNS: usize = 3;

const TOPOLOGY_WAVENET_A1: &str = "wavenet_a1";
const TOPOLOGY_WAVENET_A2: &str = "wavenet_a2";
const TOPOLOGY_LSTM: &str = "lstm";
const MODE_OFF: &str = "Off";
const MODE_2X: &str = "2x";
const MODE_4X: &str = "4x";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Topology {
    WavenetA1,
    WavenetA2,
    Lstm,
}

impl Topology {
    fn label(self) -> &'static str {
        match self {
            Self::WavenetA1 => TOPOLOGY_WAVENET_A1,
            Self::WavenetA2 => TOPOLOGY_WAVENET_A2,
            Self::Lstm => TOPOLOGY_LSTM,
        }
    }
}

/// Resolves the mandatory A1/A2/LSTM fixture set (same fail-closed resolution
/// as the PGO workload).
fn resolve_models() -> Vec<(PathBuf, Topology)> {
    let search_dirs = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.push(PathBuf::from("tests/fixtures/models"));
        dirs
    };
    let mut resolved: Vec<(PathBuf, Topology)> = Vec::new();
    for (name, topo) in [
        ("wavenet_a1_standard.nam", Topology::WavenetA1),
        ("a2_example.nam", Topology::WavenetA2),
        ("lstm.nam", Topology::Lstm),
    ] {
        let mut found = false;
        for dir in &search_dirs {
            let path = dir.join(name);
            if path.is_file() {
                resolved.push((path, topo));
                found = true;
                break;
            }
        }
        if !found {
            eprintln!("ab_opt_bench: FATAL: mandatory fixture {name} not found in {search_dirs:?}");
            std::process::exit(1);
        }
    }
    resolved
}

fn resolve_ir_path() -> PathBuf {
    for dir in [
        std::env::var("NAM_FIXTURES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::new()),
        PathBuf::from("tests/fixtures/models"),
    ] {
        if dir.is_dir() {
            let path = dir.join(IR_FILENAME);
            if path.is_file() {
                return path;
            }
        }
    }
    eprintln!("ab_opt_bench: FATAL: mandatory CabSim IR fixture {IR_FILENAME} not found.");
    std::process::exit(1);
}

// ── Perf-event counters (raw syscall; libc lacks the perf binding) ──────────

const PERF_EVENT_IOC_ENABLE: libc::c_ulong = 0x2400;
const PERF_EVENT_IOC_DISABLE: libc::c_ulong = 0x2401;
const PERF_EVENT_IOC_RESET: libc::c_ulong = 0x2403;
const PERF_FLAG_FD_CLOEXEC: u64 = 0x8;
const PERF_TYPE_HARDWARE: u32 = 0;
const PERF_TYPE_HW_CACHE: u32 = 3;
const PERF_COUNT_HW_CPU_CYCLES: u64 = 0;
const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
const PERF_COUNT_HW_CACHE_ITLB: u64 = 8;
const PERF_COUNT_HW_CACHE_ICACHE: u64 = 5;
const PERF_COUNT_HW_CACHE_OP_READ: u64 = 0;
const PERF_COUNT_HW_CACHE_RESULT_MISS: u64 = 1;
// __NR_perf_event_open on x86-64 Linux (not exported by libc).
const NR_PERF_EVENT_OPEN: libc::c_long = 298;

/// Linux `perf_event_attr` (first 112 bytes — enough for type/size/config and
/// the `disabled`/`exclude_kernel`/`exclude_hv` control flags).
#[repr(C)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    sample_period: u64,
    sample_type: u64,
    read_format: u64,
    flags: u64,
    wakeup_events: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
}

impl PerfEventAttr {
    fn hardware(config: u64) -> Self {
        Self {
            type_: PERF_TYPE_HARDWARE,
            size: std::mem::size_of::<Self>() as u32,
            config,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            // disabled=1 (bit 0), exclude_kernel=1 (bit 5), exclude_hv=1 (bit 6).
            flags: 1 | (1 << 5) | (1 << 6),
            wakeup_events: 0,
            bp_type: 0,
            config1: 0,
            config2: 0,
            branch_sample_type: 0,
            sample_regs_user: 0,
            sample_stack_user: 0,
            clockid: 0,
        }
    }

    fn cache(config: u64) -> Self {
        Self {
            type_: PERF_TYPE_HW_CACHE,
            ..Self::hardware(config)
        }
    }
}

/// One best-effort PMU counter. The fd is `None` when the event could not be
/// opened (typed: `unavailable` + reason).
struct Pmc {
    name: &'static str,
    fd: Option<i32>,
    reason: Option<String>,
}

impl Pmc {
    fn open(name: &'static str, attr: PerfEventAttr) -> Self {
        // SAFETY: `attr` is a fully initialized value of the correct layout;
        // the syscall only reads it and returns an fd (or -1). No memory is
        // retained beyond the copy the kernel makes.
        let fd = unsafe {
            libc::syscall(
                NR_PERF_EVENT_OPEN,
                &attr as *const PerfEventAttr,
                0,  // pid: current thread
                -1, // cpu: any (pinned to one CPU by the harness)
                -1, // group_fd: none
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd >= 0 {
            Self {
                name,
                fd: Some(fd as i32),
                reason: None,
            }
        } else {
            Self {
                name,
                fd: None,
                reason: Some(std::io::Error::last_os_error().to_string()),
            }
        }
    }

    fn ioctl(&self, request: libc::c_ulong) {
        if let Some(fd) = self.fd {
            // SAFETY: `fd` is a valid perf fd owned by this counter.
            unsafe {
                libc::ioctl(fd, request, 0);
            }
        }
    }

    fn read(&self) -> u64 {
        let Some(fd) = self.fd else {
            return 0;
        };
        let mut val: u64 = 0;
        // SAFETY: `fd` is a valid perf fd and `val` is a writable u64 large
        // enough for a single counter value.
        let n = unsafe {
            libc::read(
                fd,
                &mut val as *mut u64 as *mut libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
        if n != std::mem::size_of::<u64>() as isize {
            return 0;
        }
        val
    }
}

impl Drop for Pmc {
    fn drop(&mut self) {
        if let Some(fd) = self.fd {
            // SAFETY: `fd` is a valid fd owned by this counter.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

/// The PMU counter set retained in the receipt: cycles, instructions, iTLB
/// misses and I-cache misses, each with typed availability.
struct PmcSet {
    cycles: Pmc,
    instructions: Pmc,
    itlb_misses: Pmc,
    icache_misses: Pmc,
}

impl PmcSet {
    fn open() -> Self {
        Self {
            cycles: Pmc::open("cycles", PerfEventAttr::hardware(PERF_COUNT_HW_CPU_CYCLES)),
            instructions: Pmc::open(
                "instructions",
                PerfEventAttr::hardware(PERF_COUNT_HW_INSTRUCTIONS),
            ),
            itlb_misses: Pmc::open(
                "itlb_misses",
                PerfEventAttr::cache(
                    PERF_COUNT_HW_CACHE_ITLB
                        | (PERF_COUNT_HW_CACHE_OP_READ << 8)
                        | (PERF_COUNT_HW_CACHE_RESULT_MISS << 16),
                ),
            ),
            icache_misses: Pmc::open(
                "icache_misses",
                PerfEventAttr::cache(
                    PERF_COUNT_HW_CACHE_ICACHE
                        | (PERF_COUNT_HW_CACHE_OP_READ << 8)
                        | (PERF_COUNT_HW_CACHE_RESULT_MISS << 16),
                ),
            ),
        }
    }

    fn reset(&self) {
        for pmc in [
            &self.cycles,
            &self.instructions,
            &self.itlb_misses,
            &self.icache_misses,
        ] {
            pmc.ioctl(PERF_EVENT_IOC_RESET);
        }
    }

    fn enable(&self) {
        for pmc in [
            &self.cycles,
            &self.instructions,
            &self.itlb_misses,
            &self.icache_misses,
        ] {
            pmc.ioctl(PERF_EVENT_IOC_ENABLE);
        }
    }

    fn disable(&self) {
        for pmc in [
            &self.cycles,
            &self.instructions,
            &self.itlb_misses,
            &self.icache_misses,
        ] {
            pmc.ioctl(PERF_EVENT_IOC_DISABLE);
        }
    }

    fn counts(&self) -> BTreeMap<&'static str, u64> {
        let mut m = BTreeMap::new();
        for pmc in [
            &self.cycles,
            &self.instructions,
            &self.itlb_misses,
            &self.icache_misses,
        ] {
            m.insert(pmc.name, pmc.read());
        }
        m
    }

    fn availability(&self) -> Vec<(String, Option<String>)> {
        let mut v = Vec::new();
        for pmc in [
            &self.cycles,
            &self.instructions,
            &self.itlb_misses,
            &self.icache_misses,
        ] {
            v.push((pmc.name.to_string(), pmc.reason.clone()));
        }
        v
    }
}

// ── Statistics ───────────────────────────────────────────────────────────────

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

// ── TSC helpers ─────────────────────────────────────────────────────────────

#[inline(always)]
fn rdtsc_cycles() -> u64 {
    // SAFETY: `_mm_lfence` + `_rdtsc` are available on all x86-64 CPUs; they
    // perform no memory access and have no side effects beyond reading TSC.
    unsafe {
        core::arch::x86_64::_mm_lfence();
        core::arch::x86_64::_rdtsc()
    }
}

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

// ── DSP machinery ────────────────────────────────────────────────────────────

fn build_cabsim_pair(ir_path: &Path, target_rate: u32, block_size: usize) -> CabSimPair {
    let ir = CabSimIr::load(ir_path, target_rate, true).unwrap_or_else(|e| {
        eprintln!("ab_opt_bench: FATAL: IR load failed: {e}");
        std::process::exit(1);
    });
    let build_side = |label: &str| -> Box<CabSimAdapter> {
        let engine = ConvEngine::new(&ir.samples, block_size).unwrap_or_else(|e| {
            eprintln!("ab_opt_bench: FATAL: ConvEngine({label}) init failed: {e}");
            std::process::exit(1);
        });
        match CabSimAdapter::new(Box::new(engine)) {
            Ok(a) => Box::new(a),
            Err(e) => {
                eprintln!("ab_opt_bench: FATAL: CabSimAdapter({label}) init failed: {e:?}");
                std::process::exit(1);
            }
        }
    };
    CabSimPair {
        l: build_side("L"),
        r: build_side("R"),
        sample_rate: target_rate,
    }
}

/// The DSP cell configuration the A/B harness measures (fixed across every
/// measured binary so plain → PGO → PGO+BOLT compare the same workload).
#[derive(Debug, Clone, Copy)]
struct CellCfg {
    rate: u32,
    quantum: usize,
    os: &'static str,
    cabsim_ir: bool,
}

/// Runs `blocks` DSP blocks for one `CellCfg`, returning the per-block
/// serialized cycle counts.
///
/// Only the block loop is timed; the one-time machinery construction is
/// outside the measured window (it would pollute the tail-latency envelope).
fn run_measured(
    path: &Path,
    cfg: &CellCfg,
    warmup: u64,
    blocks: u64,
    ir_path: &Path,
) -> (Vec<u64>, u64) {
    let rate = cfg.rate;
    let quantum = cfg.quantum;
    let os_mode = cfg.os;
    let cabsim_ir = cfg.cabsim_ir;
    let json = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("ab_opt_bench: FATAL: cannot read {path:?}: {e}");
        std::process::exit(1);
    });
    let model_data = parse_nam_json(&json).unwrap_or_else(|e| {
        eprintln!("ab_opt_bench: FATAL: cannot parse {path:?}: {e}");
        std::process::exit(1);
    });
    let model_sr = model_data.sample_rate.unwrap_or(48_000.0) as u32;

    let mut opt_model_l: Option<Box<StaticModel>> = match build_model(&model_data) {
        Ok(mut m) => {
            m.prewarm(2048);
            Some(m)
        }
        Err(e) => {
            eprintln!("ab_opt_bench: FATAL: model L build failed: {e}");
            std::process::exit(1);
        }
    };
    let mut opt_model_r: Option<Box<StaticModel>> = match build_model(&model_data) {
        Ok(mut m) => {
            m.prewarm(2048);
            Some(m)
        }
        Err(e) => {
            eprintln!("ab_opt_bench: FATAL: model R build failed: {e}");
            std::process::exit(1);
        }
    };

    let mut resampler = NamResampler::new(rate, model_sr, quantum).unwrap_or_else(|e| {
        eprintln!("ab_opt_bench: FATAL: resampler {rate}→{model_sr} failed: {e}");
        std::process::exit(1);
    });
    let target_rate = model_sr.max(rate);
    let mut conv = if cabsim_ir {
        Some(build_cabsim_pair(ir_path, target_rate, quantum))
    } else {
        None
    };

    let gate_params = GateParams::default();
    let mut silence_hysteresis = DynamicHysteresis::new();
    let mut mono_hysteresis = DynamicHysteresis::new();
    let mut process_mono = false;
    let rt_status = RtStatusFlags::default();
    let mut adaptive = AdaptiveCompute::new(AdaptiveComputeMode::Off);
    let mut bridge = Box::new(DspBridge {
        buffers: [BridgeBuffer::new(), BridgeBuffer::new()],
        active_read_idx: AtomicUsize::new(0),
        generation: AtomicU64::new(0),
        consumed_gen: AtomicU64::new(0),
        dropped_frames: AtomicU32::new(0),
    });

    let mut resamp_mid_l = vec![0.0; MAX_RESAMP_BUF];
    let mut resamp_mid_r = vec![0.0; MAX_RESAMP_BUF];
    let mut resamp_out_l = vec![0.0; MAX_RESAMP_BUF];
    let mut resamp_out_r = vec![0.0; MAX_RESAMP_BUF];
    let mut model_out_l = vec![0.0; MAX_RESAMP_BUF];
    let mut model_out_r = vec![0.0; MAX_RESAMP_BUF];
    let mut os_buf: [f32; MAX_RESAMP_BUF * 6] = [0.0f32; MAX_RESAMP_BUF * 6];

    let (mut os_l, mut os_r) = match os_mode {
        MODE_OFF => (new_os(OversampleFactor::Off), new_os(OversampleFactor::Off)),
        MODE_2X => (new_os(OversampleFactor::X2), new_os(OversampleFactor::X2)),
        MODE_4X => (new_os(OversampleFactor::X4), new_os(OversampleFactor::X4)),
        other => {
            eprintln!("ab_opt_bench: FATAL: unknown oversampling mode {other:?}");
            std::process::exit(1);
        }
    };

    let threshold_open_sq = (-70.0f32).powf(10.0 / 20.0);
    let threshold_close_sq = (-80.0f32).powf(10.0 / 20.0);

    let stress_signal = generate_stress_signal_v2_default(model_sr);
    let stereo_offset = if (model_sr as usize).is_multiple_of(stress_signal.len()) {
        1
    } else {
        model_sr as usize % stress_signal.len()
    };
    let mut signal_offset: usize = 0;
    let mut samples_l = vec![0.0f32; quantum];
    let mut samples_r = vec![0.0f32; quantum];

    let total = warmup + blocks;
    let mut cycle_samples = Vec::with_capacity(blocks as usize);
    let mut total_frames: u64 = 0;

    for i in 0..total {
        for j in 0..quantum {
            let idx = (signal_offset + j) % stress_signal.len();
            samples_l[j] = stress_signal[idx];
            samples_r[j] = stress_signal[(idx + stereo_offset) % stress_signal.len()];
        }
        signal_offset = (signal_offset + quantum) % stress_signal.len();

        let bridge_writer = unsafe { Some(DspBridgeWriter::new(&mut *bridge as *mut DspBridge)) };
        let ctx = DspPipelineContext {
            resampler: &mut resampler,
            os_l: &mut os_l,
            os_r: &mut os_r,
            active_model_l: &mut opt_model_l,
            active_model_r: &mut opt_model_r,
            input_gain_mult: 1.0,
            output_gain_mult: 1.0,
            gate_params: &gate_params,
            silence_hysteresis: &mut silence_hysteresis,
            mono_hysteresis: &mut mono_hysteresis,
            threshold_open_sq,
            threshold_close_sq,
            process_mono: &mut process_mono,
            rt_status: &rt_status,
            adaptive: &mut adaptive,
            bridge_writer,
            conv: None,
            conv_pair: conv.as_mut(),
        };
        let (os_in_l_slice, rest) = os_buf.split_at_mut(MAX_RESAMP_BUF);
        let (os_in_r_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
        let (os_model_l_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
        let (os_model_r_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
        let (xfd_l, xfd_r) = rest.split_at_mut(MAX_RESAMP_BUF);
        let bufs = DspBuffers {
            resamp_mid_l: &mut resamp_mid_l,
            resamp_mid_r: &mut resamp_mid_r,
            resamp_out_l: &mut resamp_out_l,
            resamp_out_r: &mut resamp_out_r,
            model_out_l: &mut model_out_l,
            model_out_r: &mut model_out_r,
            os_in_l: os_in_l_slice,
            os_in_r: os_in_r_slice,
            os_model_l: os_model_l_slice,
            os_model_r: os_model_r_slice,
            crossfade_scratch_l: xfd_l,
            crossfade_scratch_r: xfd_r,
        };

        let t0 = rdtsc_cycles();
        let n_pw =
            capture_dsp_pipeline(&mut samples_l, &mut samples_r, quantum, ctx, bufs, model_sr);
        let t1 = rdtsc_cycles();
        black_box(&samples_l);
        black_box(&samples_r);
        total_frames += n_pw as u64;
        if i >= warmup {
            cycle_samples.push(t1 - t0);
        }
    }

    black_box(&bridge);
    (cycle_samples, total_frames)
}

fn new_os(factor: OversampleFactor) -> OversampleEngine {
    OversampleEngine::new(factor, MAX_RESAMP_BUF).unwrap_or_else(|e| {
        eprintln!("ab_opt_bench: FATAL: OversampleEngine init failed: {e}");
        std::process::exit(1);
    })
}

// ── Dependency-free JSON ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum JsonValue {
    Int(u64),
    Num(f64),
    Str(String),
    Obj(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    fn render(&self, out: &mut String) {
        match self {
            Self::Int(n) => out.push_str(&n.to_string()),
            Self::Num(n) => out.push_str(&format!("{n:.2}")),
            Self::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
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
            Self::Obj(map) => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
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
    fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.render(&mut out);
        out
    }
}

fn kv(key: &str, value: JsonValue) -> (String, JsonValue) {
    (key.to_string(), value)
}
fn obj(items: Vec<(String, JsonValue)>) -> JsonValue {
    JsonValue::Obj(items.into_iter().collect())
}

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopoFilter {
    All,
    A1,
    A2,
    Lstm,
}

struct Cfg {
    variant: String,
    runs: usize,
    blocks: u64,
    warmup: u64,
    rate: u32,
    quantum: usize,
    os: &'static str,
    cabsim_ir: bool,
    topology: TopoFilter,
    receipt_json: PathBuf,
    cpu: Option<usize>,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            variant: "unset".to_string(),
            runs: DEFAULT_RUNS,
            blocks: DEFAULT_BLOCKS,
            warmup: DEFAULT_WARMUP,
            rate: DEFAULT_RATE,
            quantum: DEFAULT_QUANTUM,
            os: DEFAULT_OS,
            cabsim_ir: true,
            topology: TopoFilter::All,
            receipt_json: PathBuf::from(DEFAULT_RECEIPT_JSON),
            cpu: None,
        }
    }
}

fn parse_args() -> Cfg {
    let mut cfg = Cfg::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--variant" => cfg.variant = args.next().expect("--variant <name>"),
            "--runs" => {
                cfg.runs = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--runs <N>")
            }
            "--blocks" => {
                cfg.blocks = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--blocks <N>")
            }
            "--warmup" => {
                cfg.warmup = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--warmup <N>")
            }
            "--rate" => {
                cfg.rate = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--rate <Hz>")
            }
            "--quantum" => {
                cfg.quantum = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--quantum <N>")
            }
            "--oversample" => {
                cfg.os = match args.next().as_deref() {
                    Some("Off") => MODE_OFF,
                    Some("2x") => MODE_2X,
                    Some("4x") => MODE_4X,
                    other => panic!("--oversample must be Off|2x|4x, got {other:?}"),
                };
            }
            "--cabsim" => {
                cfg.cabsim_ir = match args.next().as_deref() {
                    Some("ir") => true,
                    Some("bypass") => false,
                    other => panic!("--cabsim must be ir|bypass, got {other:?}"),
                };
            }
            "--topology" => {
                cfg.topology = match args.next().as_deref() {
                    Some("all") => TopoFilter::All,
                    Some("a1") => TopoFilter::A1,
                    Some("a2") => TopoFilter::A2,
                    Some("lstm") => TopoFilter::Lstm,
                    other => panic!("--topology must be all|a1|a2|lstm, got {other:?}"),
                };
            }
            "--cpu" => {
                cfg.cpu = args.next().and_then(|v| v.parse().ok());
            }
            "--receipt" => {
                cfg.receipt_json = PathBuf::from(args.next().expect("--receipt <path>"));
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: ab_opt_bench [--variant NAME] [--runs N] [--blocks N] [--warmup N] \
                     [--rate Hz] [--quantum N] [--oversample Off|2x|4x] [--cabsim ir|bypass] \
                     [--topology all|a1|a2|lstm] [--cpu N] [--receipt PATH]"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument {other} (see --help)"),
        }
    }
    if let Ok(path) = std::env::var(RECEIPT_ENV) {
        cfg.receipt_json = PathBuf::from(path);
    }
    assert!(cfg.runs >= 1, "--runs must be >= 1");
    assert!(cfg.blocks >= 1000, "--blocks must be >= 1000");
    if cfg.cpu.is_none() {
        cfg.cpu = select_optimal_cpu();
    }
    cfg
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    neural_amp_modeler_rs::dsp::pipeline::DISABLE_GATE
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let cfg = parse_args();
    pin_thread(cfg.cpu);
    let freq = measure_freq_ghz_x1000();
    let models = resolve_models();
    let ir_path = resolve_ir_path();

    let scope: Vec<&(PathBuf, Topology)> = models
        .iter()
        .filter(|(_, t)| match cfg.topology {
            TopoFilter::All => true,
            TopoFilter::A1 => *t == Topology::WavenetA1,
            TopoFilter::A2 => *t == Topology::WavenetA2,
            TopoFilter::Lstm => *t == Topology::Lstm,
        })
        .collect();
    if scope.is_empty() {
        eprintln!("ab_opt_bench: FATAL: no topology in scope");
        return ExitCode::FAILURE;
    }

    eprintln!(
        "NAM A/B opt bench — variant={} runs={} blocks={} quantum={} rate={} os={} cabsim={} freq={:.3} GHz",
        cfg.variant,
        cfg.runs,
        cfg.blocks,
        cfg.quantum,
        cfg.rate,
        cfg.os,
        if cfg.cabsim_ir { "ir" } else { "bypass" },
        freq as f64 / 1000.0
    );
    if let Some(c) = cfg.cpu {
        eprintln!("  cpu = {c}");
    }

    let pmcs = PmcSet::open();
    let mut jsonl_lines: Vec<JsonValue> = Vec::new();
    let mut txt = String::new();
    txt.push_str(&format!(
        "NAM-Audio-Pipe A/B optimization bench\n\
         =====================================\n\
         variant={} runs={} blocks={} quantum={} rate={} os={} cabsim={}\n",
        cfg.variant,
        cfg.runs,
        cfg.blocks,
        cfg.quantum,
        cfg.rate,
        cfg.os,
        if cfg.cabsim_ir { "ir" } else { "bypass" }
    ));

    let cell_cfg = CellCfg {
        rate: cfg.rate,
        quantum: cfg.quantum,
        os: cfg.os,
        cabsim_ir: cfg.cabsim_ir,
    };

    for (path, topo) in &scope {
        for run in 0..cfg.runs {
            pmcs.reset();
            // Warmup runs outside the PMU window; the measured block loop is
            // bracketed by enable/disable so the counters attribute only to it.
            let _warm = run_measured(path, &cell_cfg, cfg.warmup, 0, &ir_path);
            pmcs.enable();
            let (cycles, total_frames) = run_measured(path, &cell_cfg, 0, cfg.blocks, &ir_path);
            pmcs.disable();
            let counts = pmcs.counts();
            let st = Stats::of(cycles);
            let to_ns = |c: u64| c * 1000 / freq;

            let mut run_obj = vec![
                kv("event", JsonValue::Str("run".into())),
                kv("variant", JsonValue::Str(cfg.variant.clone())),
                kv("topology", JsonValue::Str(topo.label().into())),
                kv("run", JsonValue::Int(run as u64 + 1)),
                kv("blocks", JsonValue::Int(cfg.blocks)),
                kv("frames_processed", JsonValue::Int(total_frames)),
                kv(
                    "cycles",
                    obj(vec![
                        kv("min", JsonValue::Int(st.min)),
                        kv("mean", JsonValue::Num(st.mean)),
                        kv("p50", JsonValue::Int(st.p50)),
                        kv("p99", JsonValue::Int(st.p99)),
                        kv("p999", JsonValue::Int(st.p999)),
                        kv("max", JsonValue::Int(st.max)),
                    ]),
                ),
                kv(
                    "tail_latency_ns",
                    obj(vec![
                        kv("p99", JsonValue::Int(to_ns(st.p99))),
                        kv("p999", JsonValue::Int(to_ns(st.p999))),
                        kv("max", JsonValue::Int(to_ns(st.max))),
                        kv("mean", JsonValue::Num(st.mean * 1000.0 / freq as f64)),
                    ]),
                ),
                kv(
                    "pmu",
                    obj(vec![
                        kv(
                            "cycles",
                            JsonValue::Int(counts.get("cycles").copied().unwrap_or(0)),
                        ),
                        kv(
                            "instructions",
                            JsonValue::Int(counts.get("instructions").copied().unwrap_or(0)),
                        ),
                        kv(
                            "itlb_misses",
                            JsonValue::Int(counts.get("itlb_misses").copied().unwrap_or(0)),
                        ),
                        kv(
                            "icache_misses",
                            JsonValue::Int(counts.get("icache_misses").copied().unwrap_or(0)),
                        ),
                    ]),
                ),
            ];

            let avail = pmcs.availability();
            run_obj.push(kv(
                "pmu_availability",
                obj(avail
                    .into_iter()
                    .map(|(name, reason)| {
                        kv(
                            &name,
                            if let Some(r) = reason {
                                JsonValue::Str(format!("unavailable: {r}"))
                            } else {
                                JsonValue::Str("ok".into())
                            },
                        )
                    })
                    .collect()),
            ));

            jsonl_lines.push(obj(run_obj));

            txt.push_str(&format!(
                "  run {} {}: p99={} cyc ({} ns), max={} cyc ({} ns), mean={:.0} cyc | cycles={} instr={} itlb={} icache={}\n",
                topo.label(),
                run + 1,
                st.p99,
                to_ns(st.p99),
                st.max,
                to_ns(st.max),
                st.mean,
                counts.get("cycles").copied().unwrap_or(0),
                counts.get("instructions").copied().unwrap_or(0),
                counts.get("itlb_misses").copied().unwrap_or(0),
                counts.get("icache_misses").copied().unwrap_or(0),
            ));
        }
    }

    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut header = vec![
        kv("event", JsonValue::Str("header".into())),
        kv("suite", JsonValue::Str("ab-opt".into())),
        kv("variant", JsonValue::Str(cfg.variant.clone())),
        kv("runs", JsonValue::Int(cfg.runs as u64)),
        kv("blocks", JsonValue::Int(cfg.blocks)),
        kv("rate_hz", JsonValue::Int(cfg.rate as u64)),
        kv("quantum", JsonValue::Int(cfg.quantum as u64)),
        kv("oversample", JsonValue::Str(cfg.os.into())),
        kv(
            "cabsim",
            JsonValue::Str(if cfg.cabsim_ir { "ir" } else { "bypass" }.into()),
        ),
        kv(
            "tsc_ghz",
            JsonValue::Str(format!("{:.3}", freq as f64 / 1000.0)),
        ),
        kv("ts", JsonValue::Int(started)),
    ];
    if let Ok(s) = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid") {
        header.push(kv("perf_event_paranoid", JsonValue::Str(s.trim().into())));
    }
    if let Ok(s) = std::fs::read_to_string("/proc/loadavg") {
        header.push(kv("loadavg", JsonValue::Str(s.trim().into())));
    }

    let mut jsonl = String::new();
    jsonl.push_str(&obj(header).to_json_string());
    jsonl.push('\n');
    for line in &jsonl_lines {
        jsonl.push_str(&line.to_json_string());
        jsonl.push('\n');
    }

    if let Some(dir) = cfg.receipt_json.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&cfg.receipt_json, &jsonl) {
        eprintln!(
            "error: cannot write receipt {}: {e}",
            cfg.receipt_json.display()
        );
        return ExitCode::FAILURE;
    }
    let txt_path = cfg.receipt_json.with_extension("txt");
    let _ = std::fs::write(&txt_path, &txt);
    eprintln!("receipt: {}", cfg.receipt_json.display());
    eprintln!("verdict retained for the ceremony comparison (plain → PGO → PGO+BOLT)");
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_percentiles_are_correct() {
        let v: Vec<u64> = (0..1000).collect();
        let s = Stats::of(v);
        assert_eq!(s.n, 1000);
        assert_eq!(s.min, 0);
        assert_eq!(s.p50, 499);
        assert_eq!(s.p99, 989);
        assert_eq!(s.max, 999);
    }

    #[test]
    fn stats_empty_is_default() {
        assert_eq!(Stats::of(Vec::<u64>::new()), Stats::default());
    }

    #[test]
    fn json_emitter_escapes_and_sorts_keys() {
        let v = obj(vec![
            kv("a", JsonValue::Int(1)),
            kv("b", JsonValue::Int(42)),
            kv("c", JsonValue::Str("x\"\n".into())),
            kv("d", JsonValue::Num(1.5)),
        ]);
        assert_eq!(v.to_json_string(), r#"{"a":1,"b":42,"c":"x\"\n","d":1.50}"#);
    }
}
