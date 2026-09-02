// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PGO profiling workload — exercises the full DSP pipeline over a **coverage
//! matrix** with a deterministic synthetic stress signal, without requiring a
//! PipeWire daemon.
//!
//! Generates `.profraw` profiles representative of the RT callback hot-path
//! (gate → resampler → inference → oversample → cabsim → bridge → recording).
//!
//! # Coverage matrix (T5.2)
//!
//! | Dimension            | Values                     | Weight (typical use)                       |
//! |----------------------|----------------------------|--------------------------------------------|
//! | Host rate            | 44.1 / 48 / 96 kHz         | 0.25 / **0.50** / 0.25 (PipeWire default)  |
//! | Quantum (frames)     | 64 / 256 / 512             | **0.50** / 0.30 / 0.20 (low-latency first) |
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
//! # Fail-closed contract (F-RB-013 / T5.3, G-PERF-003)
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
//!   (44.1/48/96 kHz) and all quantums (64/256/512) must be exercised.
//! * The recording path (`--record` halves) must push every processed block
//!   into the production recording transport (T4.3 pool) and report a typed
//!   overrun count — an overrun is recorded, never silently dropped.
//! * The receipt proves **per-group, per-topology minimum progress** (frames
//!   and samples advanced through resampler / inference / oversample / cabsim
//!   / bridge / recording), never an aggregated global number (G-PERF-003).
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
    BridgeBuffer, DspBridge, DspBridgeWriter, DspBuffers, DspPipelineContext, MAX_RESAMP_BUF,
    capture_dsp_pipeline,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::loader::dispatcher::build_model;
use neural_amp_modeler_rs::loader::nam_json::{NamModelData, parse_nam_json};
use neural_amp_modeler_rs::models::{NamModel, StaticModel};
use neural_amp_modeler_rs::testing::stress::generate_stress_signal_v2_default;

use nam_audio_pipe::recording::transport::{RecordingReceiver, RecordingSender};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};
use std::time::{SystemTime, UNIX_EPOCH};

const IR_FILENAME: &str = "cabsim_ir_pgo.wav";
const DEFAULT_RECEIPT_PATH: &str = "target/logs/pgo-workload-receipt.json";
const RECEIPT_PATH_ENV: &str = "NAM_PGO_RECEIPT";

const TOPOLOGY_WAVENET_A1: &str = "wavenet_a1";
const TOPOLOGY_WAVENET_A2: &str = "wavenet_a2";
const TOPOLOGY_LSTM: &str = "lstm";
const MODE_OFF: &str = "Off";
const MODE_2X: &str = "2x";
const MODE_4X: &str = "4x";
const REC_NO: &str = "no";
const REC_YES: &str = "yes";
const GATE_ON: &str = "on";
const GATE_OFF: &str = "off";

// ── Coverage matrix (T5.2) ───────────────────────────────────────────────────

/// Host sample rates exercised (Hz). Weight documents typical use: 48 kHz is
/// the PipeWire default, 44.1 kHz is the music/DAW rate, 96 kHz is HQ.
const RATES_HZ: [u32; 3] = [44_100, 48_000, 96_000];
const RATE_WEIGHTS: [f64; 3] = [0.25, 0.50, 0.25];

/// Process quantums (frames/block) exercised. Weight documents typical use:
/// 64 is the low-latency default, 256 the common quantum, 512 the safe/large
/// margin.
const QUANTUMS: [usize; 3] = [64, 256, 512];
const QUANTUM_WEIGHTS: [f64; 3] = [0.50, 0.30, 0.20];

/// Oversampling modes exercised. Live mode (`Off`) dominates because it is the
/// default low-latency configuration; 2×/4× are the HQ/offline configurations.
const OS_WEIGHTS: [(&str, f64); 3] = [(MODE_OFF, 0.60), (MODE_2X, 0.20), (MODE_4X, 0.20)];

/// CabSim presence: the IR path is the default production path; the bypass
/// half proves the zero-cost CabSim route is also profiled.
const CABSIM_IR_WEIGHT: f64 = 0.70;
const CABSIM_BYPASS_WEIGHT: f64 = 0.30;

/// Recording: both halves of the matrix are profiled (with and without the
/// production recording transport).
const RECORDING_NO_WEIGHT: f64 = 0.50;
const RECORDING_YES_WEIGHT: f64 = 0.50;

/// Noise gate: both halves of the matrix are profiled (with thresholds active
/// and with thresholds zeroed/off).
const GATE_ON_WEIGHT: f64 = 0.50;
const GATE_OFF_WEIGHT: f64 = 0.50;

/// Default total simulation budget (seconds of model audio) per matrix half.
/// Distributed across every cell by weight.
const DEFAULT_TOTAL_SECONDS: f64 = 10.0;
/// Per-cell block floor: every combination of the matrix must run at least
/// this many blocks so its hot-path instructions are provably in the profile.
const DEFAULT_MIN_BLOCKS_PER_CELL: u64 = 4;

/// DSP topology family classification used by the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Topology {
    WavenetA1,
    WavenetA2,
    Lstm,
    Other,
}

impl Topology {
    fn label(self) -> &'static str {
        match self {
            Self::WavenetA1 => TOPOLOGY_WAVENET_A1,
            Self::WavenetA2 => TOPOLOGY_WAVENET_A2,
            Self::Lstm => TOPOLOGY_LSTM,
            Self::Other => "other",
        }
    }
}

/// A model resolved for profiling: its path plus its deterministic topology family.
struct WorkloadModel {
    path: PathBuf,
    topology: Topology,
}

// ── Recording helper (production transport, T4.3) ────────────────────────────

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
        match &mut self.receiver {
            RecordingReceiver::Pool { pool, .. } => {
                if let Some(in_flight) = pool.try_pop() {
                    let _ = in_flight.release();
                }
            }
            RecordingReceiver::Inline(consumer) => {
                if let Ok(payload) = consumer.pop() {
                    match payload {
                        nam_audio_pipe::recording::buffer::RingPayload::Audio(_)
                        | nam_audio_pipe::recording::buffer::RingPayload::Metadata(_)
                        | nam_audio_pipe::recording::buffer::RingPayload::StreamStop => {}
                    }
                }
            }
        }
    }

    fn finish(&mut self) {
        // Push the terminal token, then drop the producer half so the
        // abandoned+drained terminal condition arms (production semantics,
        // F-RB-009 / T3.5). The pool channels can then be drained integrally.
        let _ = self.sender.try_push_stream_stop();
        self.sender = RecordingSender::none();
        loop {
            match &mut self.receiver {
                RecordingReceiver::Pool { control, pool } => {
                    if control.pop().is_ok() {
                        continue;
                    }
                    if let Some(in_flight) = pool.try_pop() {
                        let _ = in_flight.release();
                        continue;
                    }
                }
                RecordingReceiver::Inline(consumer) => {
                    if consumer.pop().is_ok() {
                        continue;
                    }
                }
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

    let mut resampler = match NamResampler::new(cfg.rate, model_sr, quantum) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "pgo_workload: FATAL: failed to create resampler {}→{}: {e}",
                cfg.rate, model_sr
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

    let (threshold_open_sq, threshold_close_sq) = if cfg.gate {
        ((-70.0f32).powf(10.0 / 20.0), (-80.0f32).powf(10.0 / 20.0))
    } else {
        (0.0f32, 0.0f32)
    };

    let mut signal_offset: usize = 0;

    let opt_model_l = model_l;
    let opt_model_r = model_r;

    let (mut os_l, mut os_r) = match cfg.os_mode {
        MODE_OFF => (
            new_os_engine(OversampleFactor::Off, "Off,L"),
            new_os_engine(OversampleFactor::Off, "Off,R"),
        ),
        MODE_2X => (
            new_os_engine(OversampleFactor::X2, "2x,L"),
            new_os_engine(OversampleFactor::X2, "2x,R"),
        ),
        MODE_4X => (
            new_os_engine(OversampleFactor::X4, "4x,L"),
            new_os_engine(OversampleFactor::X4, "4x,R"),
        ),
        other => {
            eprintln!("pgo_workload: FATAL: unknown oversampling mode {other:?}");
            process::exit(1);
        }
    };

    let mut os_buf: [f32; MAX_RESAMP_BUF * 6] = [0.0f32; MAX_RESAMP_BUF * 6];
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

        let bridge_writer = unsafe { Some(DspBridgeWriter::new(&mut *bridge as *mut DspBridge)) };

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

        let (os_in_l_slice, rest) = os_buf.split_at_mut(MAX_RESAMP_BUF);
        let (os_in_r_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
        let (os_model_l_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
        let (os_model_r_slice, rest) = rest.split_at_mut(MAX_RESAMP_BUF);
        let (xfd_scratch_l_slice, xfd_scratch_r_slice) = rest.split_at_mut(MAX_RESAMP_BUF);

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
            crossfade_scratch_l: xfd_scratch_l_slice,
            crossfade_scratch_r: xfd_scratch_r_slice,
        };

        let n_pw =
            capture_dsp_pipeline(&mut samples_l, &mut samples_r, quantum, ctx, bufs, model_sr);
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

// ── Dependency-free JSON emitter ─────────────────────────────────────────────

/// Minimal dependency-free JSON emitter for the structured PGO receipt.
///
/// Object keys are kept in a `BTreeMap`, so the emitted document is
/// byte-deterministic for a given workload.
#[derive(Debug, Clone)]
enum JsonValue {
    Bool(bool),
    Int(u64),
    Num(f64),
    Str(String),
    Arr(Vec<JsonValue>),
    Obj(BTreeMap<String, JsonValue>),
}

impl JsonValue {
    fn new_obj() -> Self {
        Self::Obj(BTreeMap::new())
    }

    fn insert(&mut self, key: &str, value: JsonValue) {
        if let Self::Obj(map) = self {
            map.insert(key.to_string(), value);
        }
    }

    fn render(&self, out: &mut String) {
        match self {
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(n) => out.push_str(&n.to_string()),
            Self::Num(n) => out.push_str(&format!("{n:.4}")),
            Self::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => {
                            out.push_str(&format!("\\u{:04x}", c as u32));
                        }
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Self::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.render(out);
                }
                out.push(']');
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

/// One executed matrix cell, serialized to the receipt.
struct CellRecord {
    rate: u32,
    quantum: usize,
    topology: &'static str,
    os_mode: &'static str,
    cabsim_ir: bool,
    recording: bool,
    gate: bool,
    progress: CellProgress,
}

impl CellRecord {
    fn to_json(&self) -> JsonValue {
        let p = &self.progress;
        let mut obj = JsonValue::new_obj();
        obj.insert("rate_hz", JsonValue::Int(self.rate as u64));
        obj.insert("quantum_frames", JsonValue::Int(self.quantum as u64));
        obj.insert("topology", JsonValue::Str(self.topology.to_string()));
        obj.insert("oversampling", JsonValue::Str(self.os_mode.to_string()));
        obj.insert(
            "cabsim",
            JsonValue::Str(if self.cabsim_ir { "ir" } else { "bypass" }.to_string()),
        );
        obj.insert(
            "recording",
            JsonValue::Str(if self.recording { REC_YES } else { REC_NO }.to_string()),
        );
        obj.insert(
            "gate",
            JsonValue::Str(if self.gate { GATE_ON } else { GATE_OFF }.to_string()),
        );
        obj.insert("blocks", JsonValue::Int(p.blocks));
        obj.insert("frames", JsonValue::Int(p.frames));
        obj.insert("samples", JsonValue::Int(p.samples()));
        obj.insert("resampler_frames", JsonValue::Int(p.resampler_frames));
        obj.insert("inference_frames", JsonValue::Int(p.frames));
        obj.insert("oversample_frames", JsonValue::Int(p.oversample_frames));
        obj.insert("cabsim_frames", JsonValue::Int(p.cabsim_frames));
        obj.insert("bridge_frames", JsonValue::Int(p.frames));
        obj.insert("recording_frames", JsonValue::Int(p.recording_frames));
        obj.insert("recording_accepted", JsonValue::Int(p.recording_accepted));
        obj.insert("recording_overruns", JsonValue::Int(p.recording_overruns));
        obj
    }
}

/// Per-group minimum progress report (frames and samples), per topology.
#[derive(Debug, Clone, Default)]
struct GroupProgress {
    /// Minimum frames advanced over the cells of a topology that exercised the
    /// group (0 when the group never ran for that topology → fail-closed).
    min_frames_per_topology: BTreeMap<&'static str, u64>,
}

impl GroupProgress {
    fn to_json(&self) -> JsonValue {
        let mut obj = JsonValue::new_obj();
        let mut frames = JsonValue::new_obj();
        let mut samples = JsonValue::new_obj();
        for (topo, min) in &self.min_frames_per_topology {
            frames.insert(topo, JsonValue::Int(*min));
            samples.insert(topo, JsonValue::Int(min.saturating_mul(2)));
        }
        obj.insert("min_frames_per_topology", frames);
        obj.insert("min_samples_per_topology", samples);
        obj
    }
}

/// Aggregated per-topology minimum progress across the whole matrix
/// (G-PERF-003: never an aggregated global number).
#[derive(Debug, Clone, Default)]
struct ProgressReport {
    min_blocks_per_topology: BTreeMap<&'static str, u64>,
    min_frames_per_topology: BTreeMap<&'static str, u64>,
    groups: BTreeMap<&'static str, GroupProgress>,
    total_frames: u64,
    total_samples: u64,
}

impl ProgressReport {
    fn to_json(&self) -> JsonValue {
        let mut obj = JsonValue::new_obj();
        let mut min_blocks = JsonValue::new_obj();
        let mut min_frames = JsonValue::new_obj();
        let mut min_samples = JsonValue::new_obj();
        for (topo, v) in &self.min_blocks_per_topology {
            min_blocks.insert(topo, JsonValue::Int(*v));
        }
        for (topo, v) in &self.min_frames_per_topology {
            min_frames.insert(topo, JsonValue::Int(*v));
            min_samples.insert(topo, JsonValue::Int(v.saturating_mul(2)));
        }
        obj.insert("min_blocks_per_topology", min_blocks);
        obj.insert("min_frames_per_topology", min_frames);
        obj.insert("min_samples_per_topology", min_samples);
        let mut groups = JsonValue::new_obj();
        for (group, g) in &self.groups {
            groups.insert(group, g.to_json());
        }
        obj.insert("groups", groups);
        obj.insert("total_frames", JsonValue::Int(self.total_frames));
        obj.insert("total_samples", JsonValue::Int(self.total_samples));
        obj
    }
}

/// Per-dimension block counts of the executed matrix.
#[derive(Debug, Clone, Default)]
struct Coverage {
    rates: BTreeMap<String, u64>,
    quantums: BTreeMap<String, u64>,
    topologies: BTreeMap<String, u64>,
    oversampling: BTreeMap<String, u64>,
    cabsim: BTreeMap<String, u64>,
    recording: BTreeMap<String, u64>,
    gate: BTreeMap<String, u64>,
}

impl Coverage {
    fn bump(map: &mut BTreeMap<String, u64>, key: &str, blocks: u64) {
        *map.entry(key.to_string()).or_insert(0) += blocks;
    }

    fn to_json(&self) -> JsonValue {
        let dim = |m: &BTreeMap<String, u64>| -> JsonValue {
            let mut obj = JsonValue::new_obj();
            for (k, v) in m {
                obj.insert(k, JsonValue::Int(*v));
            }
            obj
        };
        let mut obj = JsonValue::new_obj();
        obj.insert("rates", dim(&self.rates));
        obj.insert("quantums", dim(&self.quantums));
        obj.insert("topologies", dim(&self.topologies));
        obj.insert("oversampling", dim(&self.oversampling));
        obj.insert("cabsim", dim(&self.cabsim));
        obj.insert("recording", dim(&self.recording));
        obj.insert("gate", dim(&self.gate));
        obj
    }

    /// Backward-compatible `topology_blocks` aggregate for build-release.sh.
    fn topologies_json(&self) -> JsonValue {
        let mut obj = JsonValue::new_obj();
        for (k, v) in &self.topologies {
            obj.insert(k, JsonValue::Int(*v));
        }
        obj
    }

    /// Backward-compatible `oversampling_blocks` aggregate for build-release.sh.
    fn oversampling_json(&self) -> JsonValue {
        let mut obj = JsonValue::new_obj();
        for (k, v) in &self.oversampling {
            obj.insert(k, JsonValue::Int(*v));
        }
        obj
    }
}

/// Structured PGO workload receipt (`target/logs/pgo-workload-receipt.json`).
struct WorkloadReceipt {
    generated_at_unix: u64,
    ir: String,
    cells: Vec<CellRecord>,
    coverage: Coverage,
    progress: ProgressReport,
    cabsim_total_frames: u64,
    no_stage_skipped: bool,
    gate_disabled: bool,
}

impl WorkloadReceipt {
    fn new() -> Self {
        Self {
            generated_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            ir: String::new(),
            cells: Vec::new(),
            coverage: Coverage::default(),
            progress: ProgressReport::default(),
            cabsim_total_frames: 0,
            no_stage_skipped: false,
            gate_disabled: false,
        }
    }

    fn to_json(&self) -> JsonValue {
        let mut cells = JsonValue::Arr(Vec::new());
        for cell in &self.cells {
            if let JsonValue::Arr(items) = &mut cells {
                items.push(cell.to_json());
            }
        }

        let mut matrix = JsonValue::new_obj();
        matrix.insert(
            "rates_hz",
            JsonValue::Arr(RATES_HZ.iter().map(|&r| JsonValue::Int(r as u64)).collect()),
        );
        matrix.insert(
            "quantums_frames",
            JsonValue::Arr(QUANTUMS.iter().map(|&q| JsonValue::Int(q as u64)).collect()),
        );
        matrix.insert(
            "topologies",
            JsonValue::Arr(
                [TOPOLOGY_WAVENET_A1, TOPOLOGY_WAVENET_A2, TOPOLOGY_LSTM]
                    .iter()
                    .map(|s| JsonValue::Str(s.to_string()))
                    .collect(),
            ),
        );
        matrix.insert(
            "oversampling_modes",
            JsonValue::Arr(
                [MODE_OFF, MODE_2X, MODE_4X]
                    .iter()
                    .map(|s| JsonValue::Str(s.to_string()))
                    .collect(),
            ),
        );
        matrix.insert(
            "cabsim_modes",
            JsonValue::Arr(
                ["ir", "bypass"]
                    .iter()
                    .map(|s| JsonValue::Str(s.to_string()))
                    .collect(),
            ),
        );
        matrix.insert(
            "recording_modes",
            JsonValue::Arr(
                [REC_NO, REC_YES]
                    .iter()
                    .map(|s| JsonValue::Str(s.to_string()))
                    .collect(),
            ),
        );
        matrix.insert(
            "gate_modes",
            JsonValue::Arr(
                [GATE_ON, GATE_OFF]
                    .iter()
                    .map(|s| JsonValue::Str(s.to_string()))
                    .collect(),
            ),
        );
        matrix.insert("weights", matrix_weights_json());

        let cabsim = {
            let mut cab = JsonValue::new_obj();
            cab.insert(
                "stereo_convolved_frames",
                JsonValue::Int(self.cabsim_total_frames),
            );
            cab.insert("ir", JsonValue::Str(self.ir.clone()));
            cab
        };

        let mut obj = JsonValue::new_obj();
        obj.insert("schema_version", JsonValue::Int(2));
        obj.insert("tool", JsonValue::Str("pgo_workload".to_string()));
        obj.insert("generated_at_unix", JsonValue::Int(self.generated_at_unix));
        obj.insert("matrix", matrix);
        obj.insert("gate", {
            let mut g = JsonValue::new_obj();
            g.insert("disabled", JsonValue::Bool(self.gate_disabled));
            g.insert(
                "note",
                JsonValue::Str(
                    "gate dimension exercises on (thresholds active) and off (thresholds zeroed) across the matrix; \
                     effective progress is proven by the receipt"
                        .to_string(),
                ),
            );
            g
        });
        obj.insert("ir", JsonValue::Str(self.ir.clone()));
        obj.insert("cells", cells);
        obj.insert("coverage", self.coverage.to_json());
        obj.insert("progress", self.progress.to_json());
        obj.insert("cabsim", cabsim);
        // Backward-compatible aggregate keys consumed by build-release.sh.
        obj.insert("topology_blocks", self.coverage.topologies_json());
        obj.insert("oversampling_blocks", self.coverage.oversampling_json());
        obj.insert("no_stage_skipped", JsonValue::Bool(self.no_stage_skipped));
        obj
    }
}

fn matrix_weights_json() -> JsonValue {
    let mut obj = JsonValue::new_obj();
    let mut rates = JsonValue::new_obj();
    for (i, rate) in RATES_HZ.iter().enumerate() {
        rates.insert(&rate.to_string(), JsonValue::Num(RATE_WEIGHTS[i]));
    }
    obj.insert("rates", rates);
    let mut quantums = JsonValue::new_obj();
    for (i, q) in QUANTUMS.iter().enumerate() {
        quantums.insert(&q.to_string(), JsonValue::Num(QUANTUM_WEIGHTS[i]));
    }
    obj.insert("quantums", quantums);
    let mut os = JsonValue::new_obj();
    for (mode, w) in OS_WEIGHTS {
        os.insert(mode, JsonValue::Num(w));
    }
    obj.insert("oversampling", os);
    let mut cabsim = JsonValue::new_obj();
    cabsim.insert("ir", JsonValue::Num(CABSIM_IR_WEIGHT));
    cabsim.insert("bypass", JsonValue::Num(CABSIM_BYPASS_WEIGHT));
    obj.insert("cabsim", cabsim);
    let mut recording = JsonValue::new_obj();
    recording.insert(REC_NO, JsonValue::Num(RECORDING_NO_WEIGHT));
    recording.insert(REC_YES, JsonValue::Num(RECORDING_YES_WEIGHT));
    obj.insert("recording", recording);
    let mut gate = JsonValue::new_obj();
    gate.insert(GATE_ON, JsonValue::Num(GATE_ON_WEIGHT));
    gate.insert(GATE_OFF, JsonValue::Num(GATE_OFF_WEIGHT));
    obj.insert("gate", gate);
    obj
}

// ── Topology classification & model resolution ───────────────────────────────

/// Deterministic topology classification.
///
/// The three in-repo fixture files map to fixed families (the `a2_example.nam`
/// fixture is a `SlimmableContainer` wrapping A2 sub-models, so it cannot be
/// classified by `architecture` alone). Unknown files fall back to the parsed
/// architecture signature.
fn classify_model_file(path: &Path) -> Topology {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        match name {
            "wavenet_a1_standard.nam" => return Topology::WavenetA1,
            "a2_example.nam" => return Topology::WavenetA2,
            "lstm.nam" => return Topology::Lstm,
            _ => {}
        }
    }
    let json = match std::fs::read_to_string(path) {
        Ok(json) => json,
        Err(e) => {
            eprintln!(
                "pgo_workload: FATAL: cannot classify model {} (unreadable): {e}",
                path.display()
            );
            process::exit(1);
        }
    };
    classify_parsed_model(&parse_nam_json(&json).unwrap_or_else(|e| {
        eprintln!(
            "pgo_workload: FATAL: cannot classify model {} (unparseable): {e}",
            path.display()
        );
        process::exit(1);
    }))
}

fn classify_parsed_model(data: &NamModelData) -> Topology {
    match data.architecture.as_str() {
        "LSTM" => Topology::Lstm,
        "WaveNet" if data.is_wavenet_a2() => Topology::WavenetA2,
        "WaveNet" => Topology::WavenetA1,
        _ => Topology::Other,
    }
}

/// Resolves the mandatory multi-topology model set (fail-closed).
///
/// The three mandatory topology families (WaveNet A1, WaveNet A2, LSTM) must
/// all resolve; a missing fixture aborts the workload with exit code 1 so the
/// PGO profile can never silently omit a DSP stage.
fn resolve_workload_models() -> Vec<WorkloadModel> {
    let mut resolved: Vec<WorkloadModel> = Vec::new();

    if let Ok(p) = std::env::var("NAM_MODEL") {
        let path = PathBuf::from(&p);
        if !path.is_file() {
            eprintln!("pgo_workload: FATAL: NAM_MODEL={p} not found");
            process::exit(1);
        }
        let topology = classify_model_file(&path);
        eprintln!(
            "pgo_workload: resolved NAM_MODEL override: {} (topology={})",
            path.display(),
            topology.label()
        );
        resolved.push(WorkloadModel { path, topology });
    }

    let search_dirs = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.push(PathBuf::from("tests/fixtures/models"));
        dirs
    };

    // License-safe in-repo fixtures only (tests/fixtures/models/).
    const CATEGORIES: &[(&str, Topology, &[&str])] = &[
        (
            "WaveNet A1 Standard",
            Topology::WavenetA1,
            &["wavenet_a1_standard.nam"],
        ),
        ("WaveNet A2", Topology::WavenetA2, &["a2_example.nam"]),
        ("LSTM", Topology::Lstm, &["lstm.nam"]),
    ];

    let mut missing = Vec::new();
    for (cat_name, topo, candidates) in CATEGORIES {
        let mut found_for_cat = false;
        for dir in &search_dirs {
            for name in *candidates {
                let path = dir.join(name);
                if path.is_file() && !resolved.iter().any(|m| m.path == path) {
                    eprintln!(
                        "pgo_workload: resolved fixture for category '{}': {}",
                        cat_name,
                        path.display()
                    );
                    resolved.push(WorkloadModel {
                        path,
                        topology: *topo,
                    });
                    found_for_cat = true;
                    break;
                }
            }
            if found_for_cat {
                break;
            }
        }
        if !found_for_cat {
            missing.push(*cat_name);
        }
    }

    if !missing.is_empty() {
        eprintln!(
            "pgo_workload: FATAL: mandatory topology categories missing: {}",
            missing.join(", ")
        );
        eprintln!(
            "pgo_workload:        the PGO profile would silently omit mandatory DSP stages; aborting."
        );
        process::exit(1);
    }

    // Deterministic fallback: any additional `.nam` files in the search dirs are
    // profiled as well, but never replace the mandatory families above.
    let mut extra = Vec::new();
    for dir in &search_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.ends_with(".nam"))
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            if resolved.iter().any(|m| m.path == path) {
                continue;
            }
            let topology = classify_model_file(&path);
            eprintln!(
                "pgo_workload: fallback model found: {} (topology={})",
                path.display(),
                topology.label()
            );
            extra.push(WorkloadModel { path, topology });
        }
    }
    resolved.extend(extra);

    if resolved.is_empty() {
        eprintln!(
            "pgo_workload: FATAL: no .nam model files found in any search location! \
             Set NAM_FIXTURES_DIR or NAM_MODEL."
        );
        process::exit(1);
    }

    resolved
}

/// Resolves the mandatory deterministic CabSim IR fixture (fail-closed).
fn resolve_ir_path() -> PathBuf {
    let search_dirs = {
        let mut dirs = Vec::new();
        if let Ok(dir) = std::env::var("NAM_FIXTURES_DIR") {
            dirs.push(PathBuf::from(dir));
        }
        dirs.push(PathBuf::from("tests/fixtures/models"));
        dirs
    };

    for dir in &search_dirs {
        let path = dir.join(IR_FILENAME);
        if path.is_file() {
            eprintln!("pgo_workload: resolved IR fixture: {}", path.display());
            return path;
        }
    }

    eprintln!(
        "pgo_workload: FATAL: mandatory CabSim IR fixture '{IR_FILENAME}' not found in search dirs."
    );
    eprintln!(
        "pgo_workload:        the PGO profile would skip the convolution hot-path; aborting."
    );
    process::exit(1);
}

// ── Receipt writing & CLI ────────────────────────────────────────────────────

fn write_receipt(path: &Path, receipt: &WorkloadReceipt) {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "pgo_workload: FATAL: cannot create receipt dir {}: {e}",
            parent.display()
        );
        process::exit(1);
    }
    let json = receipt.to_json().to_json_string();
    if let Err(e) = std::fs::write(path, json.as_bytes()) {
        eprintln!(
            "pgo_workload: FATAL: cannot write receipt {}: {e}",
            path.display()
        );
        process::exit(1);
    }
    eprintln!("pgo_workload: receipt written to {}", path.display());
}

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

fn parse_cli() -> Cli {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--record" => cli.record = RecordArg::Yes,
            "--no-record" => cli.record = RecordArg::No,
            "--gate" => {
                let val = args.next().expect("--gate <on|off>");
                match val.as_str() {
                    "on" => cli.gate = GateArg::On,
                    "off" => cli.gate = GateArg::Off,
                    other => {
                        eprintln!(
                            "pgo_workload: FATAL: invalid gate mode {other:?} (expected 'on' or 'off')"
                        );
                        process::exit(1);
                    }
                }
            }
            "--seconds" => {
                cli.seconds = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--seconds <f64>");
            }
            "--min-blocks" => {
                cli.min_blocks = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .expect("--min-blocks <u64>");
            }
            "--receipt" => {
                cli.receipt_path = PathBuf::from(args.next().expect("--receipt <path>"));
            }
            "--help" | "-h" => {
                eprintln!(
                    "usage: pgo_workload [--record|--no-record] [--gate on|off] [--seconds S] \
                     [--min-blocks N] [--receipt PATH]"
                );
                process::exit(0);
            }
            other => {
                eprintln!("pgo_workload: FATAL: unknown argument {other:?} (see --help)");
                process::exit(1);
            }
        }
    }
    assert!(cli.seconds > 0.0, "--seconds must be > 0");
    assert!(cli.min_blocks >= 1, "--min-blocks must be >= 1");
    if let Ok(path) = std::env::var(RECEIPT_PATH_ENV) {
        cli.receipt_path = PathBuf::from(path);
    }
    cli
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
    // least once for every mandatory topology (G-PERF-003).
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
        ("quantums", &["64", "256", "512"] as &[&str]),
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
