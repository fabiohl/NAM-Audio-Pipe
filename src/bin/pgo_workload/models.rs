// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Model topology classification and fixture discovery for `pgo_workload`.

use neural_amp_modeler_rs::loader::nam_json::{NamModelData, parse_nam_json};
use std::path::{Path, PathBuf};
use std::process;

pub(super) const TOPOLOGY_WAVENET_A1: &str = "wavenet_a1";
pub(super) const TOPOLOGY_WAVENET_A2: &str = "wavenet_a2";
pub(super) const TOPOLOGY_LSTM: &str = "lstm";

const IR_FILENAME: &str = "cabsim_ir_pgo.wav";

/// DSP topology family classification used by the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Topology {
    WavenetA1,
    WavenetA2,
    Lstm,
    Other,
}

impl Topology {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::WavenetA1 => TOPOLOGY_WAVENET_A1,
            Self::WavenetA2 => TOPOLOGY_WAVENET_A2,
            Self::Lstm => TOPOLOGY_LSTM,
            Self::Other => "other",
        }
    }
}

/// A model resolved for profiling: its path plus its deterministic topology family.
pub(super) struct WorkloadModel {
    pub(super) path: PathBuf,
    pub(super) topology: Topology,
}

/// Deterministic topology classification.
pub(super) fn classify_model_file(path: &Path) -> Topology {
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

pub(super) fn classify_parsed_model(data: &NamModelData) -> Topology {
    match data.architecture.as_str() {
        "LSTM" => Topology::Lstm,
        "WaveNet" if data.is_wavenet_a2() => Topology::WavenetA2,
        "WaveNet" => Topology::WavenetA1,
        _ => Topology::Other,
    }
}

/// Resolves the mandatory multi-topology model set (fail-closed).
pub(super) fn resolve_workload_models() -> Vec<WorkloadModel> {
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
pub(super) fn resolve_ir_path() -> PathBuf {
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
