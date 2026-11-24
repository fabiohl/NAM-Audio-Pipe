// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Coverage matrix constants and structured v2 JSON receipt emitter for `pgo_workload`.

use nam_audio_pipe::testing::json::JsonValue;
use std::collections::BTreeMap;
use std::path::Path;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use super::CellProgress;
use super::models::{TOPOLOGY_LSTM, TOPOLOGY_WAVENET_A1, TOPOLOGY_WAVENET_A2};

pub(super) const MODE_OFF: &str = "Off";
pub(super) const MODE_2X: &str = "2x";
pub(super) const MODE_4X: &str = "4x";
pub(super) const REC_NO: &str = "no";
pub(super) const REC_YES: &str = "yes";
pub(super) const GATE_ON: &str = "on";
pub(super) const GATE_OFF: &str = "off";

pub(super) const RATES_HZ: [u32; 3] = [44_100, 48_000, 96_000];
pub(super) const RATE_WEIGHTS: [f64; 3] = [0.25, 0.50, 0.25];

pub(super) const QUANTUMS: [usize; 6] = [32, 64, 128, 256, 512, 1024];
pub(super) const QUANTUM_WEIGHTS: [f64; 6] = [0.10, 0.40, 0.15, 0.20, 0.10, 0.05];

pub(super) const OS_WEIGHTS: [(&str, f64); 3] =
    [(MODE_OFF, 0.60), (MODE_2X, 0.20), (MODE_4X, 0.20)];
pub(super) const CABSIM_IR_WEIGHT: f64 = 0.70;
pub(super) const CABSIM_BYPASS_WEIGHT: f64 = 0.30;
pub(super) const RECORDING_NO_WEIGHT: f64 = 0.50;
pub(super) const RECORDING_YES_WEIGHT: f64 = 0.50;
pub(super) const GATE_ON_WEIGHT: f64 = 0.50;
pub(super) const GATE_OFF_WEIGHT: f64 = 0.50;

/// One executed matrix cell, serialized to the receipt.
pub(super) struct CellRecord {
    pub(super) rate: u32,
    pub(super) quantum: usize,
    pub(super) topology: &'static str,
    pub(super) os_mode: &'static str,
    pub(super) cabsim_ir: bool,
    pub(super) recording: bool,
    pub(super) gate: bool,
    pub(super) progress: CellProgress,
}

impl CellRecord {
    pub(super) fn to_json(&self) -> JsonValue {
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
pub(super) struct GroupProgress {
    pub(super) min_frames_per_topology: BTreeMap<&'static str, u64>,
}

impl GroupProgress {
    pub(super) fn to_json(&self) -> JsonValue {
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

/// Aggregated per-topology minimum progress across the whole matrix.
#[derive(Debug, Clone, Default)]
pub(super) struct ProgressReport {
    pub(super) min_blocks_per_topology: BTreeMap<&'static str, u64>,
    pub(super) min_frames_per_topology: BTreeMap<&'static str, u64>,
    pub(super) groups: BTreeMap<&'static str, GroupProgress>,
    pub(super) total_frames: u64,
    pub(super) total_samples: u64,
}

impl ProgressReport {
    pub(super) fn to_json(&self) -> JsonValue {
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
pub(super) struct Coverage {
    pub(super) rates: BTreeMap<String, u64>,
    pub(super) quantums: BTreeMap<String, u64>,
    pub(super) topologies: BTreeMap<String, u64>,
    pub(super) oversampling: BTreeMap<String, u64>,
    pub(super) cabsim: BTreeMap<String, u64>,
    pub(super) recording: BTreeMap<String, u64>,
    pub(super) gate: BTreeMap<String, u64>,
}

impl Coverage {
    pub(super) fn bump(map: &mut BTreeMap<String, u64>, key: &str, blocks: u64) {
        *map.entry(key.to_string()).or_insert(0) += blocks;
    }

    pub(super) fn to_json(&self) -> JsonValue {
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

    pub(super) fn topologies_json(&self) -> JsonValue {
        let mut obj = JsonValue::new_obj();
        for (k, v) in &self.topologies {
            obj.insert(k, JsonValue::Int(*v));
        }
        obj
    }

    pub(super) fn oversampling_json(&self) -> JsonValue {
        let mut obj = JsonValue::new_obj();
        for (k, v) in &self.oversampling {
            obj.insert(k, JsonValue::Int(*v));
        }
        obj
    }
}

/// Structured PGO workload receipt (`target/logs/pgo-workload-receipt.json`).
pub(super) struct WorkloadReceipt {
    pub(super) generated_at_unix: u64,
    pub(super) ir: String,
    pub(super) cells: Vec<CellRecord>,
    pub(super) coverage: Coverage,
    pub(super) progress: ProgressReport,
    pub(super) cabsim_total_frames: u64,
    pub(super) no_stage_skipped: bool,
    pub(super) gate_disabled: bool,
}

impl WorkloadReceipt {
    pub(super) fn new() -> Self {
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

    pub(super) fn to_json(&self) -> JsonValue {
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
        obj.insert("topology_blocks", self.coverage.topologies_json());
        obj.insert("oversampling_blocks", self.coverage.oversampling_json());
        obj.insert("no_stage_skipped", JsonValue::Bool(self.no_stage_skipped));
        obj
    }
}

pub(super) fn matrix_weights_json() -> JsonValue {
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

pub(super) fn write_receipt(path: &Path, receipt: &WorkloadReceipt) {
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "pgo_workload: FATAL: cannot create receipt dir {}: {e}",
            parent.display()
        );
        process::exit(1);
    }
    let json = receipt.to_json().to_json_string_with_decimals(4);
    if let Err(e) = std::fs::write(path, json.as_bytes()) {
        eprintln!(
            "pgo_workload: FATAL: cannot write receipt {}: {e}",
            path.display()
        );
        process::exit(1);
    }
    eprintln!("pgo_workload: receipt written to {}", path.display());
}
