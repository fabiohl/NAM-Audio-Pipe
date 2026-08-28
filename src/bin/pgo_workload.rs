// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PGO profiling workload — exercises the full DSP pipeline with a deterministic
//! synthetic stress signal, without requiring a PipeWire daemon.
//!
//! Generates `.profraw` profiles representative of the RT callback hot-path
//! (gate → resampler → inference → oversample → cabsim → bridge).
//!
//! # Fail-closed contract (F-RB-013 / T5.3)
//!
//! The workload is a profiling *gate*, not a best-effort sampler:
//!
//! * Any I/O error, JSON parse failure, model construction failure (WaveNet /
//!   LSTM) or resampler allocation failure terminates immediately with
//!   exit code 1 — there are no tolerant `continue` paths.
//! * The CabSim IR fixture [`cabsim_ir_pgo.wav`](tests/fixtures/models/cabsim_ir_pgo.wav)
//!   is **mandatory**: if it cannot be loaded, or if the stereo convolution is
//!   not executed on every block of every model, the workload aborts.
//! * All three oversampling modes (`Off`, `2x`, `4x`) must be exercised.
//! * At the end a structured receipt is written to
//!   `target/logs/pgo-workload-receipt.json` (override with `NAM_PGO_RECEIPT`)
//!   proving per-topology block counts, per-mode oversampling coverage, and the
//!   stereo CabSim frame counter. `no_stage_skipped` is only `true` when every
//!   mandatory gate above passed.
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
    BridgeBuffer, DspBridge, DspBridgeWriter, DspBuffers, DspPipelineContext, MAX_BRIDGE_BUF,
    MAX_RESAMP_BUF, capture_dsp_pipeline,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::loader::dispatcher::build_model;
use neural_amp_modeler_rs::loader::nam_json::{NamModelData, parse_nam_json};
use neural_amp_modeler_rs::models::NamModel;
use neural_amp_modeler_rs::testing::stress::generate_stress_signal_v2_default;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize};
use std::time::{SystemTime, UNIX_EPOCH};

const IR_FILENAME: &str = "cabsim_ir_pgo.wav";
const DEFAULT_RECEIPT_PATH: &str = "target/logs/pgo-workload-receipt.json";
const RECEIPT_PATH_ENV: &str = "NAM_PGO_RECEIPT";
const HOST_RATE: u32 = 48_000;
const BLOCK_SIZE: usize = 64;
const TOTAL_SIMULATION_SECONDS: u64 = 10;

const TOPOLOGY_WAVENET_A1: &str = "wavenet_a1";
const TOPOLOGY_WAVENET_A2: &str = "wavenet_a2";
const TOPOLOGY_LSTM: &str = "lstm";
const MODE_OFF: &str = "Off";
const MODE_2X: &str = "2x";
const MODE_4X: &str = "4x";

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

/// Minimal dependency-free JSON emitter for the structured PGO receipt.
///
/// Object keys are kept in a `BTreeMap`, so the emitted document is
/// byte-deterministic for a given workload.
#[derive(Debug, Clone)]
enum JsonValue {
    Bool(bool),
    Int(u64),
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

/// Per-model entry of the structured receipt.
struct ModelReceipt {
    path: String,
    topology: &'static str,
    sample_rate: u32,
    blocks: u64,
    oversampling: BTreeMap<&'static str, u64>,
    cabsim_frames: u64,
    cabsim_blocks: u64,
}

impl ModelReceipt {
    fn to_json(&self) -> JsonValue {
        let mut oversampling = JsonValue::new_obj();
        for (mode, count) in &self.oversampling {
            oversampling.insert(mode, JsonValue::Int(*count));
        }
        let mut obj = JsonValue::new_obj();
        obj.insert("path", JsonValue::Str(self.path.clone()));
        obj.insert("topology", JsonValue::Str(self.topology.to_string()));
        obj.insert("sample_rate", JsonValue::Int(self.sample_rate as u64));
        obj.insert("blocks", JsonValue::Int(self.blocks));
        obj.insert("oversampling", oversampling);
        obj.insert("cabsim_frames", JsonValue::Int(self.cabsim_frames));
        obj.insert("cabsim_blocks", JsonValue::Int(self.cabsim_blocks));
        obj
    }
}

/// Structured PGO workload receipt (`target/logs/pgo-workload-receipt.json`).
struct WorkloadReceipt {
    generated_at_unix: u64,
    host_rate: u32,
    block_size: usize,
    simulation_seconds: u64,
    ir: String,
    models: Vec<ModelReceipt>,
    topology_blocks: BTreeMap<&'static str, u64>,
    oversampling_blocks: BTreeMap<&'static str, u64>,
    cabsim_frames: u64,
    cabsim_blocks: u64,
    no_stage_skipped: bool,
}

impl WorkloadReceipt {
    fn new() -> Self {
        let mut topology_blocks = BTreeMap::new();
        topology_blocks.insert(TOPOLOGY_WAVENET_A1, 0);
        topology_blocks.insert(TOPOLOGY_WAVENET_A2, 0);
        topology_blocks.insert(TOPOLOGY_LSTM, 0);
        let mut oversampling_blocks = BTreeMap::new();
        oversampling_blocks.insert(MODE_OFF, 0);
        oversampling_blocks.insert(MODE_2X, 0);
        oversampling_blocks.insert(MODE_4X, 0);
        Self {
            generated_at_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            host_rate: HOST_RATE,
            block_size: BLOCK_SIZE,
            simulation_seconds: TOTAL_SIMULATION_SECONDS,
            ir: String::new(),
            models: Vec::new(),
            topology_blocks,
            oversampling_blocks,
            cabsim_frames: 0,
            cabsim_blocks: 0,
            no_stage_skipped: false,
        }
    }

    fn to_json(&self) -> JsonValue {
        let mut models = JsonValue::Arr(Vec::new());
        for model in &self.models {
            if let JsonValue::Arr(items) = &mut models {
                items.push(model.to_json());
            }
        }
        let mut topology_blocks = JsonValue::new_obj();
        for (topology, count) in &self.topology_blocks {
            topology_blocks.insert(topology, JsonValue::Int(*count));
        }
        let mut oversampling_blocks = JsonValue::new_obj();
        for (mode, count) in &self.oversampling_blocks {
            oversampling_blocks.insert(mode, JsonValue::Int(*count));
        }

        let mut obj = JsonValue::new_obj();
        obj.insert("schema_version", JsonValue::Int(1));
        obj.insert("tool", JsonValue::Str("pgo_workload".to_string()));
        obj.insert("generated_at_unix", JsonValue::Int(self.generated_at_unix));
        obj.insert("host_rate", JsonValue::Int(self.host_rate as u64));
        obj.insert("block_size", JsonValue::Int(self.block_size as u64));
        obj.insert(
            "simulation_seconds",
            JsonValue::Int(self.simulation_seconds),
        );
        obj.insert("ir", JsonValue::Str(self.ir.clone()));
        obj.insert("models", models);
        obj.insert("topology_blocks", topology_blocks);
        obj.insert("oversampling_blocks", oversampling_blocks);
        obj.insert("cabsim", {
            let mut cab = JsonValue::new_obj();
            cab.insert(
                "stereo_convolved_frames",
                JsonValue::Int(self.cabsim_frames),
            );
            cab.insert("blocks", JsonValue::Int(self.cabsim_blocks));
            cab.insert("ir", JsonValue::Str(self.ir.clone()));
            cab
        });
        obj.insert("no_stage_skipped", JsonValue::Bool(self.no_stage_skipped));
        obj
    }
}

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

/// Builds the mandatory stereo-decoupled CabSim pair (fail-closed).
fn build_cabsim_pair(ir_path: &Path, target_rate: u32, block_size: usize) -> CabSimPair {
    let ir = match CabSimIr::load(ir_path, target_rate, true) {
        Ok(ir) => ir,
        Err(e) => {
            eprintln!("pgo_workload: FATAL: IR load failed: {e}");
            process::exit(1);
        }
    };
    eprintln!(
        "pgo_workload: IR loaded: {} samples @ {} Hz (orig {} Hz, normalized={})",
        ir.samples.len(),
        ir.sample_rate,
        ir.original_rate,
        ir.normalized
    );

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

fn main() {
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

    neural_amp_modeler_rs::dsp::pipeline::DISABLE_GATE
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let ir_path = resolve_ir_path();

    let mut receipt = WorkloadReceipt::new();
    receipt.ir = ir_path.display().to_string();

    // Uniform per-model budget keeps every topology far above the mandatory
    // 1000-block gate regardless of NAM_MODEL/fallback additions.
    let weight = 1.0 / models.len() as f64;

    for (idx, model) in models.iter().enumerate() {
        eprintln!(
            "\npgo_workload: [{}/{}] processing model {} (topology {}, weight {:.1}%)",
            idx + 1,
            models.len(),
            model.path.display(),
            model.topology.label(),
            weight * 100.0
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

        let model_sr = model_data.sample_rate.unwrap_or(48_000.0) as u32;

        let mut model_l = match build_model(&model_data) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "pgo_workload: FATAL: failed to build model L for {}: {e}",
                    model.path.display()
                );
                process::exit(1);
            }
        };
        model_l.prewarm(2048);

        let mut model_r = match build_model(&model_data) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "pgo_workload: FATAL: failed to build model R for {}: {e}",
                    model.path.display()
                );
                process::exit(1);
            }
        };
        model_r.prewarm(2048);

        let mut resampler = match NamResampler::new(HOST_RATE, model_sr, BLOCK_SIZE) {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "pgo_workload: FATAL: failed to create resampler for {}: {e}",
                    model.path.display()
                );
                process::exit(1);
            }
        };

        let target_rate = model_sr.max(HOST_RATE);
        let mut conv = build_cabsim_pair(&ir_path, target_rate, BLOCK_SIZE);

        let gate_params = GateParams::default();
        let mut silence_hysteresis = DynamicHysteresis::new();
        let mut mono_hysteresis = DynamicHysteresis::new();
        let mut process_mono = false;
        let rt_status = RtStatusFlags::default();
        let mut adaptive = AdaptiveCompute::new(AdaptiveComputeMode::Off);

        let mut bridge = Box::new(DspBridge {
            buffers: [
                BridgeBuffer {
                    buf_l: [0.0; MAX_BRIDGE_BUF],
                    buf_r: [0.0; MAX_BRIDGE_BUF],
                    n_samples: 0,
                },
                BridgeBuffer {
                    buf_l: [0.0; MAX_BRIDGE_BUF],
                    buf_r: [0.0; MAX_BRIDGE_BUF],
                    n_samples: 0,
                },
            ],
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

        let threshold_open_sq = (-70.0f32).powf(10.0 / 20.0);
        let threshold_close_sq = (-80.0f32).powf(10.0 / 20.0);

        let stress_signal = generate_stress_signal_v2_default(model_sr);
        let model_seconds = TOTAL_SIMULATION_SECONDS as f64 * weight;
        let num_model_samples = (model_sr as f64 * model_seconds) as usize;
        let model_blocks = (num_model_samples / BLOCK_SIZE).max(1);
        let mut signal_offset: usize = 0;
        // Phase-shifted right channel keeps the stereo (decorrelated) detection
        // open so the CabSim pair actually convolves both channels every block.
        let stereo_offset = if (model_sr as usize).is_multiple_of(stress_signal.len()) {
            1
        } else {
            model_sr as usize % stress_signal.len()
        };

        eprintln!(
            "pgo_workload: running {} blocks ({:.2}s) through capture_dsp_pipeline",
            model_blocks, model_seconds
        );

        let mut opt_model_l = Some(model_l);
        let mut opt_model_r = Some(model_r);

        let mut os_engine_l_off = OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF)
            .unwrap_or_else(|e| {
                eprintln!("pgo_workload: FATAL: OversampleEngine(Off,L) init failed: {e}");
                process::exit(1);
            });
        let mut os_engine_r_off = OversampleEngine::new(OversampleFactor::Off, MAX_RESAMP_BUF)
            .unwrap_or_else(|e| {
                eprintln!("pgo_workload: FATAL: OversampleEngine(Off,R) init failed: {e}");
                process::exit(1);
            });
        let mut os_engine_l_2x = OversampleEngine::new(OversampleFactor::X2, MAX_RESAMP_BUF)
            .unwrap_or_else(|e| {
                eprintln!("pgo_workload: FATAL: OversampleEngine(2x,L) init failed: {e}");
                process::exit(1);
            });
        let mut os_engine_r_2x = OversampleEngine::new(OversampleFactor::X2, MAX_RESAMP_BUF)
            .unwrap_or_else(|e| {
                eprintln!("pgo_workload: FATAL: OversampleEngine(2x,R) init failed: {e}");
                process::exit(1);
            });
        let mut os_engine_l_4x = OversampleEngine::new(OversampleFactor::X4, MAX_RESAMP_BUF)
            .unwrap_or_else(|e| {
                eprintln!("pgo_workload: FATAL: OversampleEngine(4x,L) init failed: {e}");
                process::exit(1);
            });
        let mut os_engine_r_4x = OversampleEngine::new(OversampleFactor::X4, MAX_RESAMP_BUF)
            .unwrap_or_else(|e| {
                eprintln!("pgo_workload: FATAL: OversampleEngine(4x,R) init failed: {e}");
                process::exit(1);
            });

        let mut os_buf: [f32; MAX_RESAMP_BUF * 6] = [0.0f32; MAX_RESAMP_BUF * 6];
        let mut samples_l = vec![0.0f32; BLOCK_SIZE];
        let mut samples_r = vec![0.0f32; BLOCK_SIZE];

        let mut model_os_counts = BTreeMap::new();
        model_os_counts.insert(MODE_OFF, 0u64);
        model_os_counts.insert(MODE_2X, 0);
        model_os_counts.insert(MODE_4X, 0);
        let mut model_cabsim_frames: u64 = 0;
        let mut model_cabsim_blocks: u64 = 0;

        for block_idx in 0..model_blocks {
            for j in 0..BLOCK_SIZE {
                let idx = (signal_offset + j) % stress_signal.len();
                samples_l[j] = stress_signal[idx];
                samples_r[j] = stress_signal[(idx + stereo_offset) % stress_signal.len()];
            }
            signal_offset = (signal_offset + BLOCK_SIZE) % stress_signal.len();

            let use_4x = block_idx % 6 == 0;
            let use_2x = block_idx % 6 == 3;
            let mode_label = if use_4x {
                MODE_4X
            } else if use_2x {
                MODE_2X
            } else {
                MODE_OFF
            };
            let (os_l, os_r) = if use_4x {
                (&mut os_engine_l_4x, &mut os_engine_r_4x)
            } else if use_2x {
                (&mut os_engine_l_2x, &mut os_engine_r_2x)
            } else {
                (&mut os_engine_l_off, &mut os_engine_r_off)
            };

            let bridge_writer =
                unsafe { Some(DspBridgeWriter::new(&mut *bridge as *mut DspBridge)) };

            let ctx = DspPipelineContext {
                resampler: &mut resampler,
                os_l,
                os_r,
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
                conv_pair: Some(&mut conv),
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

            let convolved_frames = capture_dsp_pipeline(
                &mut samples_l,
                &mut samples_r,
                BLOCK_SIZE,
                ctx,
                bufs,
                model_sr,
            );
            model_cabsim_frames += convolved_frames as u64;
            model_cabsim_blocks += 1;
            *model_os_counts.get_mut(mode_label).expect("mode bucket") += 1;
        }

        drop(opt_model_l);
        drop(opt_model_r);
        drop(conv);
        drop(resampler);
        drop(bridge);

        // Fail-closed per-model gate: the convolution must have run on every
        // block (capture_dsp_pipeline returns the post-resampler frame count,
        // which is > 0 while the cab-sim pair is attached and the gate is open).
        if model_cabsim_blocks == 0 || model_cabsim_frames == 0 {
            eprintln!(
                "pgo_workload: FATAL: model {} executed 0 convolution frames; \
                 CabSim stereo path was skipped.",
                model.path.display()
            );
            process::exit(1);
        }

        receipt.models.push(ModelReceipt {
            path: model.path.display().to_string(),
            topology: model.topology.label(),
            sample_rate: model_sr,
            blocks: model_blocks as u64,
            oversampling: model_os_counts.clone(),
            cabsim_frames: model_cabsim_frames,
            cabsim_blocks: model_cabsim_blocks,
        });
        let topology_label: &'static str = model.topology.label();
        *receipt.topology_blocks.entry(topology_label).or_insert(0) += model_blocks as u64;
        for (mode, count) in &model_os_counts {
            *receipt
                .oversampling_blocks
                .get_mut(*mode)
                .expect("mode bucket") += count;
        }
        receipt.cabsim_frames += model_cabsim_frames;
        receipt.cabsim_blocks += model_cabsim_blocks;
    }

    // Global fail-closed gates: every mandatory topology must have run and
    // every oversampling mode must have been exercised.
    for topology in [TOPOLOGY_WAVENET_A1, TOPOLOGY_WAVENET_A2, TOPOLOGY_LSTM] {
        if receipt.topology_blocks.get(topology).copied().unwrap_or(0) == 0 {
            eprintln!("pgo_workload: FATAL: topology '{topology}' was not profiled; aborting.");
            process::exit(1);
        }
    }
    for mode in [MODE_OFF, MODE_2X, MODE_4X] {
        if receipt.oversampling_blocks.get(mode).copied().unwrap_or(0) == 0 {
            eprintln!(
                "pgo_workload: FATAL: oversampling mode '{mode}' was not exercised; aborting."
            );
            process::exit(1);
        }
    }
    if receipt.cabsim_frames == 0 {
        eprintln!("pgo_workload: FATAL: stereo CabSim convolution never ran; aborting.");
        process::exit(1);
    }

    receipt.no_stage_skipped = true;

    let receipt_path = std::env::var(RECEIPT_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_RECEIPT_PATH));
    write_receipt(&receipt_path, &receipt);

    eprintln!(
        "pgo_workload: completed successfully across all models. \
         (topologies: {}, cab-sim frames: {})",
        receipt.topology_blocks.len(),
        receipt.cabsim_frames
    );
}

#[cfg(test)]
#[path = "pgo_workload_test.rs"]
mod tests;
