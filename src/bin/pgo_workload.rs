// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PGO profiling workload — exercises the full DSP pipeline over a **coverage
//! matrix** with a deterministic synthetic stress signal, without requiring a
//! PipeWire daemon.
//!
//! Generates `.profraw` profiles representative of the RT callback hot-path
//! (gate → resampler → inference → oversample → cabsim → bridge → recording).
//!
//! # Coverage matrix
//!
//! | Dimension            | Values                     | Weight (typical use)                       |
//! |----------------------|----------------------------|--------------------------------------------|
//! | Host rate            | 44.1 / 48 / 96 kHz         | 0.25 / **0.50** / 0.25 (PipeWire default)  |
//! | Quantum (frames)     | 32 / 64 / 128 / 256 / 512 / 1024 | **0.40** / 0.10 / 0.15 / 0.20 / 0.10 / 0.05 (64 dominant; 128 is the PipeWire-common quantum, 32/1024 close the tail coverage gap) |
//! | Topology             | A1 / A2 / LSTM (WaveNet)   | 1/3 each (mandatory)                       |
//! | Oversampling         | Off / 2× / 4×              | **0.60** / 0.20 / 0.20 (Live first)        |
//! | CabSim               | IR / bypass                | **0.70** / 0.30 (IR is the default path)   |
//! | Recording            | on / off (`--record`)      | 0.50 / 0.50 (both halves are profiled)     |
//! | Gate                 | on / off (`--gate`)        | 0.50 / 0.50 (both halves are profiled)     |
//!
//! The total simulation budget is distributed by weight across every cell of
//! the cross product (models × rates × quantums × oversampling × CabSim ×
//! recording × gate), with a per-cell floor (`--min-blocks`) so each combination is
//! **provably exercised** (fail-closed coverage, no silently skipped stage).
//!
//! # Fail-closed contract
//!
//! The workload is a profiling *gate*, not a best-effort sampler:
//!
//! * Any I/O error, JSON parse failure, model construction failure (WaveNet /
//!   LSTM) or resampler allocation failure terminates immediately with
//!   exit code 1 — there are no tolerant `continue` paths.
//! * The CabSim IR fixture [`cabsim_ir_pgo.wav`](tests/fixtures/models/cabsim_ir_pgo.wav)
//!   is **mandatory**: if it cannot be loaded, or if the stereo convolution is
//!   not executed on at least the `IR` cells of every model, the workload
//!   aborts. The `bypass` half of the matrix proves the no-IR path is also
//!   representative (zero-cost CabSim).
//! * All three oversampling modes (`Off`, `2x`, `4x`), all host rates
//!   (44.1/48/96 kHz) and all quantums (32/64/128/256/512/1024, 64 dominant)
//!   must be exercised.
//! * The recording path (`--record` halves) must push every processed block
//!   into the production recording transport (pool transport) and report a typed
//!   overrun count — an overrun is recorded, never silently dropped.
//! * The receipt proves **per-group, per-topology minimum progress** (frames
//!   and samples advanced through resampler / inference / oversample / cabsim
//!   / bridge / recording), never an aggregated global number.
//!   `no_stage_skipped` is only `true` when every mandatory gate above passed.
//!
//! # Deterministic CabSim IR fixture
//!
//! `tests/fixtures/models/cabsim_ir_pgo.wav` is a synthetic, public-domain
//! impulse response: 48 kHz, mono, 512 samples, PCM16, peak normalized to 0.95.
//! Sample `n` (t = n / 48000) follows:
//!
//! ```text
//! v(t) = exp(-400t)·sin(2π·1800·t)
//!      + 0.35·exp(-280t)·sin(2π·3200·t)
//!      - 0.12·exp(-200t)·sin(2π·450·t)
//! ```
//!
//! then normalized so the absolute peak equals 0.95 and quantized to 16-bit
//! PCM. The unit tests in `pgo_workload_test.rs` re-derive this formula and
//! assert the committed fixture matches it within quantization tolerance.

use neural_amp_modeler_rs::common::params::AdaptiveComputeMode;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::adaptive::AdaptiveCompute;
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::cabsim::loader::CabSimIr;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::dsp::pipeline::{
    DspBridge, DspBridgeWriter, DspPipelineContext, MAX_RESAMP_BUF, StreamingDspBuffers,
    capture_dsp_pipeline_streaming,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::loader::dispatcher::build_model;
use neural_amp_modeler_rs::loader::nam_json::parse_nam_json;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use neural_amp_modeler_rs::testing::stress::generate_stress_signal_v2_default;

use nam_audio_pipe::recording::transport::{RecordingReceiver, RecordingSender};
#[cfg(test)]
use nam_audio_pipe::testing::json::JsonValue;

use std::path::{Path, PathBuf};
use std::process;

#[path = "pgo_workload/models.rs"]
mod models;
#[path = "pgo_workload/receipt.rs"]
mod receipt;

use models::{
    TOPOLOGY_LSTM, TOPOLOGY_WAVENET_A1, TOPOLOGY_WAVENET_A2, resolve_ir_path,
    resolve_workload_models,
};
#[cfg(test)]
use models::{Topology, classify_model_file};
use receipt::{
    CABSIM_BYPASS_WEIGHT, CABSIM_IR_WEIGHT, CellRecord, Coverage, GATE_OFF, GATE_OFF_WEIGHT,
    GATE_ON, GATE_ON_WEIGHT, MODE_2X, MODE_4X, MODE_OFF, OS_WEIGHTS, ProgressReport,
    QUANTUM_WEIGHTS, QUANTUMS, RATE_WEIGHTS, RATES_HZ, REC_NO, REC_YES, RECORDING_NO_WEIGHT,
    RECORDING_YES_WEIGHT, WorkloadReceipt, write_receipt,
};

/// Default total simulation budget (seconds of model audio) per matrix half.
/// Distributed across every cell by weight.
const DEFAULT_TOTAL_SECONDS: f64 = 10.0;
/// Per-cell block floor: every combination of the matrix must run at least
/// this many blocks so its hot-path instructions are provably in the profile.
const DEFAULT_MIN_BLOCKS_PER_CELL: u64 = 4;

// ── Recording helper (production transport) ───────────────────────────────────

/// The `--record` halves of the matrix push every processed block through the
/// production recording transport. The consumer side is drained synchronously
/// on the same thread so the pool never exhausts (no artificial overruns) and
/// both the producer (`try_acquire`/`fill_planar`/`publish`) and consumer
/// (`try_pop`/`release`) instruction streams appear in the profile.
struct RecordingPipe {
    sender: RecordingSender,
    receiver: RecordingReceiver,
    accepted: u64,
    overruns: u64,
    frames: u64,
}

impl RecordingPipe {
    fn new() -> Self {
        let (sender, receiver) = nam_audio_pipe::recording::transport::create_recording_transport();
        Self {
            sender,
            receiver,
            accepted: 0,
            overruns: 0,
            frames: 0,
        }
    }

    /// Pushes the post-pipeline output frames (the exact buffers the production
    /// `send_recording_audio` records — `resamp_out_l/r[..n_pw]`), then drains
    /// one block synchronously.
    fn push(&mut self, out_l: &[f32], out_r: &[f32], n_pw: usize) {
        if n_pw == 0 {
            return;
        }
        if self.sender.try_push_audio(&out_l[..n_pw], &out_r[..n_pw]) {
            self.accepted += 1;
            self.frames += n_pw as u64;
        } else {
            self.overruns += 1;
        }
        self.drain_one();
    }

    /// Drains exactly one published block (or control barrier) so the pool
    /// stays available. The worker's exactly-once release path is exercised.
    fn drain_one(&mut self) {
        if let Some(in_flight) = self.receiver.pool_consumer_mut().try_pop() {
            let _ = in_flight.release();
        }
    }

    fn finish(&mut self) {
        // Push the terminal token, then drop the producer half so the
        // abandoned+drained terminal condition arms (production semantics).
        // The pool channels can then be drained integrally.
        let _ = self.sender.try_push_stream_stop();
        self.sender = RecordingSender::none();
        loop {
            if self.receiver.control_consumer_mut().pop().is_ok() {
                continue;
            }
            if let Some(in_flight) = self.receiver.pool_consumer_mut().try_pop() {
                let _ = in_flight.release();
                continue;
            }
            if self.receiver.is_fully_drained() {
                break;
            }
            std::hint::spin_loop();
        }
    }
}

// ── Per-cell DSP runner ───────────────────────────────────────────────────────

/// One cell of the coverage matrix.
#[derive(Debug, Clone, Copy)]
struct CellConfig {
    rate: u32,
    quantum: usize,
    os_mode: &'static str,
    cabsim_ir: bool,
    recording: bool,
    gate: bool,
    blocks: u64,
}

/// Progress advanced through each DSP group during one cell.
#[derive(Debug, Clone, Copy, Default)]
struct CellProgress {
    blocks: u64,
    /// Pipeline output frames (`n_pw`) summed over all blocks (post-resampler).
    frames: u64,
    /// Input frames advanced through the resampler (`blocks × quantum`).
    resampler_frames: u64,
    /// Frames advanced through the oversample engine (2×/4× cells only).
    oversample_frames: u64,
    /// Frames advanced through the CabSim convolution (IR cells only).
    cabsim_frames: u64,
    /// Frames advanced through the recording transport (recording cells only).
    recording_frames: u64,
    recording_accepted: u64,
    recording_overruns: u64,
}

impl CellProgress {
    fn samples(&self) -> u64 {
        self.frames.saturating_mul(2)
    }
}

/// Builds the mandatory stereo-decoupled CabSim pair (fail-closed).
fn build_cabsim_pair(ir_path: &Path, target_rate: u32, block_size: usize) -> CabSimPair {
    let ir = match CabSimIr::load(ir_path, target_rate, true) {
        Ok(ir) => ir,
        Err(e) => {
            eprintln!("pgo_workload: FATAL: IR load failed: {e}");
            process::exit(1);
        }
    };

    let build_side = |label: &str| -> Box<CabSimAdapter> {
        let engine = match ConvEngine::new(&ir.samples, block_size) {
            Ok(engine) => engine,
            Err(e) => {
                eprintln!("pgo_workload: FATAL: ConvEngine({label}) init failed: {e}");
                process::exit(1);
            }
        };
        match CabSimAdapter::new(Box::new(engine)) {
            Ok(adapter) => Box::new(adapter),
            Err(e) => {
                eprintln!("pgo_workload: FATAL: CabSimAdapter({label}) init failed: {e:?}");
                process::exit(1);
            }
        }
    };

    CabSimPair {
        l: build_side("L"),
        r: build_side("R"),
        sample_rate: target_rate,
    }
}

/// Converts a dBFS threshold to the squared linear amplitude expected by
/// [`DspPipelineContext`], whose gate thresholds are compared against the
/// block mean-square energy.
fn gate_thresholds_sq(open_db: f32, close_db: f32) -> (f32, f32) {
    let open = 10.0_f32.powf(open_db / 20.0);
    let close = 10.0_f32.powf(close_db / 20.0);
    (open * open, close * close)
}

/// Runs one matrix cell: `cfg.blocks` blocks through the full DSP pipeline at
/// the cell's host rate / quantum / oversampling / CabSim / recording
/// configuration. Fail-closed: any construction failure aborts the process.
///
/// The neural models (the expensive construction) are built once per model by
/// the caller and reused across every cell of that model; only the per-cell
/// machinery (resampler, oversample engines, CabSim, buffers) is rebuilt.
fn run_cell(
    model_l: &mut Option<Box<StaticModel>>,
    model_r: &mut Option<Box<StaticModel>>,
    model_sr: u32,
    stress_signal: &[f32],
    stereo_offset: usize,
    cfg: &CellConfig,
    ir_path: &Path,
) -> CellProgress {
    let quantum = cfg.quantum;
    let blocks = cfg.blocks;

    let mut resampler = match NamResampler::new_simple(cfg.rate, model_sr) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "pgo_workload: FATAL: failed to create resampler {}→{}: {e}",
                cfg.rate, model_sr
            );
            process::exit(1);
        }
    };
    // Production streaming adapter, sized exactly like `CaptureState::init`
    // (`StreamingResampleBuffer::new(host_rate, model_rate, MAX_RESAMP_BUF)`).
    let mut stream_resample = match StreamingResampleBuffer::new(cfg.rate, model_sr, MAX_RESAMP_BUF)
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "pgo_workload: FATAL: streaming resampler {}→{model_sr} failed: {e}",
                cfg.rate
            );
            process::exit(1);
        }
    };

    let target_rate = model_sr.max(cfg.rate);
    let mut conv = if cfg.cabsim_ir {
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

    let mut bridge = DspBridge::new_boxed();

    let mut resamp_out_l = vec![0.0; MAX_RESAMP_BUF];
    let mut resamp_out_r = vec![0.0; MAX_RESAMP_BUF];

    let (threshold_open_sq, threshold_close_sq) = if cfg.gate {
        gate_thresholds_sq(
            gate_params.threshold_open_db,
            gate_params.threshold_close_db,
        )
    } else {
        (0.0f32, 0.0f32)
    };

    let mut signal_offset: usize = 0;

    let opt_model_l = model_l;
    let opt_model_r = model_r;

    let os_factor = match cfg.os_mode {
        MODE_OFF => OversampleFactor::Off,
        MODE_2X => OversampleFactor::X2,
        MODE_4X => OversampleFactor::X4,
        other => {
            eprintln!("pgo_workload: FATAL: unknown oversampling mode {other:?}");
            process::exit(1);
        }
    };
    let mut os_l = new_os_engine(os_factor, "L");
    let mut os_r = new_os_engine(os_factor, "R");

    let os_scratch_len = OversampleEngine::required_scratch_len(os_factor, MAX_RESAMP_BUF);
    let mut os_in_l = vec![0.0f32; os_scratch_len];
    let mut os_in_r = vec![0.0f32; os_scratch_len];
    let mut os_model_l = vec![0.0f32; os_scratch_len];
    let mut os_model_r = vec![0.0f32; os_scratch_len];
    let mut xfd_l = vec![0.0f32; MAX_RESAMP_BUF];
    let mut xfd_r = vec![0.0f32; MAX_RESAMP_BUF];
    let mut samples_l = vec![0.0f32; quantum];
    let mut samples_r = vec![0.0f32; quantum];

    let mut recording = cfg.recording.then(RecordingPipe::new);
    let mut progress = CellProgress {
        blocks,
        resampler_frames: blocks.saturating_mul(quantum as u64),
        ..CellProgress::default()
    };

    for _ in 0..blocks {
        for j in 0..quantum {
            let idx = (signal_offset + j) % stress_signal.len();
            samples_l[j] = stress_signal[idx];
            samples_r[j] = stress_signal[(idx + stereo_offset) % stress_signal.len()];
        }
        signal_offset = (signal_offset + quantum) % stress_signal.len();
        // SAFETY: `bridge` is exclusively owned by this benchmarking thread; `bridge_writer`
        // narrows writes strictly to the inactive back-buffer without concurrent reader conflicts.
        let bridge_writer = unsafe { Some(DspBridgeWriter::new(&raw mut *bridge)) };

        let ctx = DspPipelineContext {
            resampler: &mut resampler,
            os_l: &mut os_l,
            os_r: &mut os_r,
            active_model_l: opt_model_l,
            active_model_r: opt_model_r,
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

        let bufs = StreamingDspBuffers {
            resamp_out_l: &mut resamp_out_l,
            resamp_out_r: &mut resamp_out_r,
            os_in_l: &mut os_in_l,
            os_in_r: &mut os_in_r,
            os_model_l: &mut os_model_l,
            os_model_r: &mut os_model_r,
            crossfade_scratch_l: &mut xfd_l,
            crossfade_scratch_r: &mut xfd_r,
        };

        let n_pw = capture_dsp_pipeline_streaming(
            &mut samples_l,
            &mut samples_r,
            quantum,
            ctx,
            &mut stream_resample,
            bufs,
            model_sr,
        );
        progress.frames += n_pw as u64;
        if cfg.os_mode != MODE_OFF {
            progress.oversample_frames += n_pw as u64;
        }
        if cfg.cabsim_ir {
            progress.cabsim_frames += n_pw as u64;
        }
        if let Some(rec) = recording.as_mut() {
            rec.push(&resamp_out_l, &resamp_out_r, n_pw);
        }
    }

    if let Some(mut rec) = recording {
        rec.finish();
        progress.recording_frames = rec.frames;
        progress.recording_accepted = rec.accepted;
        progress.recording_overruns = rec.overruns;
    }

    drop(conv);
    drop(resampler);
    drop(bridge);

    progress
}

fn new_os_engine(factor: OversampleFactor, label: &str) -> OversampleEngine {
    match OversampleEngine::new(factor, MAX_RESAMP_BUF) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pgo_workload: FATAL: OversampleEngine({label}) init failed: {e}");
            process::exit(1);
        }
    }
}

const DEFAULT_RECEIPT_PATH: &str = "target/logs/pgo-workload-receipt.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordArg {
    No,
    Yes,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateArg {
    On,
    Off,
    Both,
}

/// Manual CLI parsing (keeps the profiling harness dependency-free).
#[derive(Debug)]
struct Cli {
    record: RecordArg,
    gate: GateArg,
    seconds: f64,
    min_blocks: u64,
    receipt_path: PathBuf,
}

impl Default for Cli {
    fn default() -> Self {
        Self {
            record: RecordArg::Both,
            gate: GateArg::Both,
            seconds: DEFAULT_TOTAL_SECONDS,
            min_blocks: DEFAULT_MIN_BLOCKS_PER_CELL,
            receipt_path: PathBuf::from(DEFAULT_RECEIPT_PATH),
        }
    }
}

const USAGE: &str = "usage: pgo_workload [--record|--no-record] [--gate on|off] [--seconds S] \
                     [--min-blocks N] [--receipt PATH]";

/// Parses the CLI arguments into a [`Cli`].
///
/// Returns a typed usage error (never panics) so the caller can report it and
/// exit with the conventional usage status `2`.
fn parse_cli_from<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--record" => cli.record = RecordArg::Yes,
            "--no-record" => cli.record = RecordArg::No,
            "--gate" => {
                let val = args
                    .next()
                    .ok_or_else(|| "--gate <on|off> requires a value".to_string())?;
                match val.as_str() {
                    "on" => cli.gate = GateArg::On,
                    "off" => cli.gate = GateArg::Off,
                    other => return Err(format!("--gate must be on|off, got {other:?}")),
                }
            }
            "--seconds" => {
                cli.seconds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--seconds requires a numeric value <S>".to_string())?;
            }
            "--min-blocks" => {
                cli.min_blocks = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| "--min-blocks requires a numeric value <N>".to_string())?;
            }
            "--receipt" => {
                cli.receipt_path = PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--receipt <path> requires a value".to_string())?,
                );
            }
            "--help" | "-h" => {
                eprintln!("{USAGE}");
                process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if !cli.seconds.is_finite() || cli.seconds <= 0.0 {
        return Err("--seconds must be a finite value > 0".to_string());
    }
    if cli.min_blocks < 1 {
        return Err("--min-blocks must be >= 1".to_string());
    }
    Ok(cli)
}

/// Parses the process arguments, exiting with status `2` and the usage block on
/// a malformed invocation (never panicking on bad argv).
fn parse_cli() -> Cli {
    match parse_cli_from(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(msg) => {
            eprintln!("pgo_workload: {msg}");
            eprintln!("{USAGE}");
            process::exit(2);
        }
    }
}

fn main() {
    let cli = parse_cli();
    let models = resolve_workload_models();
    eprintln!(
        "pgo_workload: found {} models for multi-topological PGO profiling",
        models.len()
    );
    for (i, model) in models.iter().enumerate() {
        eprintln!(
            "  [{}] {} (topology={})",
            i + 1,
            model.path.display(),
            model.topology.label()
        );
    }

    let recording_modes: &[(&str, bool, f64)] = match cli.record {
        RecordArg::No => &[(REC_NO, false, 1.0)],
        RecordArg::Yes => &[(REC_YES, true, 1.0)],
        RecordArg::Both => &[
            (REC_NO, false, RECORDING_NO_WEIGHT),
            (REC_YES, true, RECORDING_YES_WEIGHT),
        ],
    };

    let gate_modes: &[(&str, bool, f64)] = match cli.gate {
        GateArg::Off => &[(GATE_OFF, false, 1.0)],
        GateArg::On => &[(GATE_ON, true, 1.0)],
        GateArg::Both => &[
            (GATE_ON, true, GATE_ON_WEIGHT),
            (GATE_OFF, false, GATE_OFF_WEIGHT),
        ],
    };

    eprintln!(
        "pgo_workload: matrix={} rates × {} quantums × {} os × 2 cabsim × {} recording × {} gate → {} cells/model",
        RATES_HZ.len(),
        QUANTUMS.len(),
        OS_WEIGHTS.len(),
        recording_modes.len(),
        gate_modes.len(),
        RATES_HZ.len()
            * QUANTUMS.len()
            * OS_WEIGHTS.len()
            * 2
            * recording_modes.len()
            * gate_modes.len()
    );

    neural_amp_modeler_rs::dsp::pipeline::DISABLE_GATE
        .store(false, std::sync::atomic::Ordering::Relaxed);

    let ir_path = resolve_ir_path();

    let mut receipt = WorkloadReceipt::new();
    receipt.ir = ir_path.display().to_string();
    receipt.gate_disabled = false;

    // Uniform per-model weight keeps every topology far above the mandatory
    // block gate regardless of NAM_MODEL/fallback additions.
    let model_weight = 1.0 / models.len() as f64;

    // Per-topology provisional accumulators. `min_blocks`/`min_frames` start at
    // u64::MAX and are tightened per cell (every topology runs cells). Group
    // minima start at 0 and are tightened only by cells that exercised the
    // group (`if > 0` guard below) — an unexercised group stays 0 and fails
    // the fail-closed gate when it is mandatory for the invocation.
    let mut progress = ProgressReport::default();
    for topology in [TOPOLOGY_WAVENET_A1, TOPOLOGY_WAVENET_A2, TOPOLOGY_LSTM] {
        progress.min_blocks_per_topology.insert(topology, u64::MAX);
        progress.min_frames_per_topology.insert(topology, u64::MAX);
        for group in [
            "resampler",
            "inference",
            "oversample",
            "cabsim",
            "bridge",
            "recording",
        ] {
            progress
                .groups
                .entry(group)
                .or_default()
                .min_frames_per_topology
                .insert(topology, 0);
        }
    }

    let mut total_frames: u64 = 0;
    let mut cabsim_total_frames: u64 = 0;

    for (idx, model) in models.iter().enumerate() {
        let topo_label: &'static str = model.topology.label();
        eprintln!(
            "\npgo_workload: [{}/{}] processing model {} (topology {})",
            idx + 1,
            models.len(),
            model.path.display(),
            topo_label
        );

        let json_data = match std::fs::read_to_string(&model.path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "pgo_workload: FATAL: failed to read model file {}: {e}",
                    model.path.display()
                );
                process::exit(1);
            }
        };
        let model_data = match parse_nam_json(&json_data) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "pgo_workload: FATAL: failed to parse model JSON {}: {e}",
                    model.path.display()
                );
                process::exit(1);
            }
        };

        // The neural models are the expensive construction; build once per
        // model and reuse across every cell of the coverage matrix.
        let model_sr = model_data.sample_rate.unwrap_or(48_000.0) as u32;
        let mut opt_model_l: Option<Box<StaticModel>> = match build_model(&model_data) {
            Ok(mut m) => {
                m.prewarm(2048);
                Some(m)
            }
            Err(e) => {
                eprintln!(
                    "pgo_workload: FATAL: failed to build model L for {}: {e}",
                    model.path.display()
                );
                process::exit(1);
            }
        };
        let mut opt_model_r: Option<Box<StaticModel>> = match build_model(&model_data) {
            Ok(mut m) => {
                m.prewarm(2048);
                Some(m)
            }
            Err(e) => {
                eprintln!(
                    "pgo_workload: FATAL: failed to build model R for {}: {e}",
                    model.path.display()
                );
                process::exit(1);
            }
        };
        let stress_signal = generate_stress_signal_v2_default(model_sr);
        // Phase-shifted right channel keeps the stereo (decorrelated) detection
        // open so the CabSim pair actually convolves both channels every block.
        let stereo_offset = if (model_sr as usize).is_multiple_of(stress_signal.len()) {
            1
        } else {
            model_sr as usize % stress_signal.len()
        };

        for &(rec_label, rec_on, rec_weight) in recording_modes {
            for &(gate_label, gate_on, gate_weight) in gate_modes {
                for (ri, &rate) in RATES_HZ.iter().enumerate() {
                    for (qi, &quantum) in QUANTUMS.iter().enumerate() {
                        for &(os_mode, os_weight) in &OS_WEIGHTS {
                            for &(cabsim_ir, cabsim_weight) in
                                &[(true, CABSIM_IR_WEIGHT), (false, CABSIM_BYPASS_WEIGHT)]
                            {
                                let cell_seconds = cli.seconds
                                    * model_weight
                                    * RATE_WEIGHTS[ri]
                                    * QUANTUM_WEIGHTS[qi]
                                    * os_weight
                                    * cabsim_weight
                                    * rec_weight
                                    * gate_weight;
                                let blocks = (((rate as f64 * cell_seconds) / quantum as f64)
                                    .round() as u64)
                                    .max(cli.min_blocks);

                                let cfg = CellConfig {
                                    rate,
                                    quantum,
                                    os_mode,
                                    cabsim_ir,
                                    recording: rec_on,
                                    gate: gate_on,
                                    blocks,
                                };

                                eprintln!(
                                    "  cell: {rate} Hz / {quantum} fr / os={os_mode} / cabsim={} / rec={rec_label} / gate={gate_label} → {blocks} blocks ({cell_seconds:.3}s)",
                                    if cabsim_ir { "ir" } else { "bypass" }
                                );

                                let cell = run_cell(
                                    &mut opt_model_l,
                                    &mut opt_model_r,
                                    model_sr,
                                    &stress_signal,
                                    stereo_offset,
                                    &cfg,
                                    &ir_path,
                                );

                                // Fail-closed per-cell gate: the pipeline must have
                                // advanced real frames on every cell (with CabSim
                                // attached, `n_pw` is the post-resampler count).
                                if cell.blocks == 0 || cell.frames == 0 {
                                    eprintln!(
                                        "pgo_workload: FATAL: cell {rate}/{quantum}/{os_mode}/{}/{rec_label}/{gate_label} advanced 0 frames.",
                                        if cabsim_ir { "ir" } else { "bypass" }
                                    );
                                    process::exit(1);
                                }
                                if rec_on && cell.recording_accepted != cell.blocks {
                                    eprintln!(
                                        "pgo_workload: FATAL: recording cell accepted {} of {} blocks (overruns={}).",
                                        cell.recording_accepted,
                                        cell.blocks,
                                        cell.recording_overruns
                                    );
                                    process::exit(1);
                                }

                                total_frames += cell.frames;
                                if cabsim_ir {
                                    cabsim_total_frames += cell.cabsim_frames;
                                }

                                // Coverage buckets.
                                Coverage::bump(
                                    &mut receipt.coverage.rates,
                                    &rate.to_string(),
                                    cell.blocks,
                                );
                                Coverage::bump(
                                    &mut receipt.coverage.quantums,
                                    &quantum.to_string(),
                                    cell.blocks,
                                );
                                Coverage::bump(
                                    &mut receipt.coverage.topologies,
                                    topo_label,
                                    cell.blocks,
                                );
                                Coverage::bump(
                                    &mut receipt.coverage.oversampling,
                                    os_mode,
                                    cell.blocks,
                                );
                                Coverage::bump(
                                    &mut receipt.coverage.cabsim,
                                    if cabsim_ir { "ir" } else { "bypass" },
                                    cell.blocks,
                                );
                                Coverage::bump(
                                    &mut receipt.coverage.recording,
                                    rec_label,
                                    cell.blocks,
                                );
                                Coverage::bump(&mut receipt.coverage.gate, gate_label, cell.blocks);

                                // Per-topology minimum progress (frames/blocks) and
                                // per-group minima (only over cells that exercised
                                // the group). `tighten_min` assumes the slot starts
                                // at u64::MAX (every topology runs cells);
                                // `tighten_group` slots start at 0 and must be set
                                // by the first exercised cell before narrowing.
                                let tighten_min = |slot: &mut u64, v: u64| {
                                    *slot = (*slot).min(v);
                                };
                                let tighten_group = |slot: &mut u64, v: u64| {
                                    if *slot == 0 || v < *slot {
                                        *slot = v;
                                    }
                                };
                                tighten_min(
                                    progress
                                        .min_blocks_per_topology
                                        .get_mut(topo_label)
                                        .expect("min blocks bucket"),
                                    cell.blocks,
                                );
                                tighten_min(
                                    progress
                                        .min_frames_per_topology
                                        .get_mut(topo_label)
                                        .expect("min frames bucket"),
                                    cell.frames,
                                );
                                tighten_group(
                                    progress
                                        .groups
                                        .get_mut("resampler")
                                        .expect("resampler")
                                        .min_frames_per_topology
                                        .get_mut(topo_label)
                                        .expect("resampler bucket"),
                                    cell.resampler_frames,
                                );
                                tighten_group(
                                    progress
                                        .groups
                                        .get_mut("inference")
                                        .expect("inference")
                                        .min_frames_per_topology
                                        .get_mut(topo_label)
                                        .expect("inference bucket"),
                                    cell.frames,
                                );
                                tighten_group(
                                    progress
                                        .groups
                                        .get_mut("bridge")
                                        .expect("bridge")
                                        .min_frames_per_topology
                                        .get_mut(topo_label)
                                        .expect("bridge bucket"),
                                    cell.frames,
                                );
                                if cell.oversample_frames > 0 {
                                    tighten_group(
                                        progress
                                            .groups
                                            .get_mut("oversample")
                                            .expect("oversample")
                                            .min_frames_per_topology
                                            .get_mut(topo_label)
                                            .expect("oversample bucket"),
                                        cell.oversample_frames,
                                    );
                                }
                                if cell.cabsim_frames > 0 {
                                    tighten_group(
                                        progress
                                            .groups
                                            .get_mut("cabsim")
                                            .expect("cabsim")
                                            .min_frames_per_topology
                                            .get_mut(topo_label)
                                            .expect("cabsim bucket"),
                                        cell.cabsim_frames,
                                    );
                                }
                                if cell.recording_frames > 0 {
                                    tighten_group(
                                        progress
                                            .groups
                                            .get_mut("recording")
                                            .expect("recording")
                                            .min_frames_per_topology
                                            .get_mut(topo_label)
                                            .expect("recording bucket"),
                                        cell.recording_frames,
                                    );
                                }

                                receipt.cells.push(CellRecord {
                                    rate,
                                    quantum,
                                    topology: topo_label,
                                    os_mode,
                                    cabsim_ir,
                                    recording: rec_on,
                                    gate: gate_on,
                                    progress: cell,
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    progress.total_frames = total_frames;
    progress.total_samples = total_frames.saturating_mul(2);
    receipt.progress = progress;
    receipt.cabsim_total_frames = cabsim_total_frames;

    // Global fail-closed gates: every mandatory topology and every dimension
    // value must have advanced real progress; every DSP group must have run at
    // least once for every mandatory topology.
    let mut failures: Vec<String> = Vec::new();
    for topology in [TOPOLOGY_WAVENET_A1, TOPOLOGY_WAVENET_A2, TOPOLOGY_LSTM] {
        let blocks = receipt
            .coverage
            .topologies
            .get(topology)
            .copied()
            .unwrap_or(0);
        if blocks == 0 {
            failures.push(format!("topology '{topology}' was not profiled"));
        }
        if receipt
            .progress
            .min_frames_per_topology
            .get(topology)
            .copied()
            .unwrap_or(0)
            == 0
        {
            failures.push(format!("topology '{topology}' advanced 0 frames"));
        }
        for group in ["resampler", "inference", "oversample", "cabsim", "bridge"] {
            let min = receipt
                .progress
                .groups
                .get(group)
                .and_then(|g| g.min_frames_per_topology.get(topology))
                .copied()
                .unwrap_or(0);
            if min == 0 {
                failures.push(format!(
                    "DSP group '{group}' advanced 0 frames for topology '{topology}'"
                ));
            }
        }
        // The recording group only applies when recording cells ran in this
        // invocation (`--record` or the default both halves).
        if cli.record != RecordArg::No {
            let min = receipt
                .progress
                .groups
                .get("recording")
                .and_then(|g| g.min_frames_per_topology.get(topology))
                .copied()
                .unwrap_or(0);
            if min == 0 {
                failures.push(format!(
                    "DSP group 'recording' advanced 0 frames for topology '{topology}'"
                ));
            }
        }
    }
    let required_recording: &[&str] = match cli.record {
        RecordArg::No => &[REC_NO],
        RecordArg::Yes => &[REC_YES],
        RecordArg::Both => &[REC_NO, REC_YES],
    };
    let required_gate: &[&str] = match cli.gate {
        GateArg::Off => &[GATE_OFF],
        GateArg::On => &[GATE_ON],
        GateArg::Both => &[GATE_ON, GATE_OFF],
    };
    for (dim, required) in [
        ("rates", &["44100", "48000", "96000"] as &[&str]),
        (
            "quantums",
            &["32", "64", "128", "256", "512", "1024"] as &[&str],
        ),
        ("oversampling", &[MODE_OFF, MODE_2X, MODE_4X] as &[&str]),
        ("cabsim", &["ir", "bypass"] as &[&str]),
        ("recording", required_recording),
        ("gate", required_gate),
    ] {
        let counts = match dim {
            "rates" => &receipt.coverage.rates,
            "quantums" => &receipt.coverage.quantums,
            "oversampling" => &receipt.coverage.oversampling,
            "cabsim" => &receipt.coverage.cabsim,
            "recording" => &receipt.coverage.recording,
            "gate" => &receipt.coverage.gate,
            _ => unreachable!("dim {dim}"),
        };
        for value in required {
            let count = counts.get(*value).copied().unwrap_or(0);
            if count == 0 {
                failures.push(format!(
                    "matrix dimension '{dim}' value '{value}' was not exercised"
                ));
            }
        }
    }

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("pgo_workload: FATAL: {f}");
        }
        eprintln!("pgo_workload: the PGO profile is not representative; aborting.");
        process::exit(1);
    }

    receipt.no_stage_skipped = true;

    write_receipt(&cli.receipt_path, &receipt);

    eprintln!(
        "pgo_workload: completed successfully across {} cells. \
         (topologies: {}, total frames: {}, CabSim frames: {})",
        receipt.cells.len(),
        receipt.coverage.topologies.len(),
        total_frames,
        cabsim_total_frames
    );
}

#[cfg(test)]
#[path = "pgo_workload_test.rs"]
mod tests;
