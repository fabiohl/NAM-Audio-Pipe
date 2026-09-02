// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the PGO workload (`pgo_workload.rs`).
//!
//! Validates the deterministic CabSim IR fixture against its documented
//! generation formula, the topology classification table, the coverage-matrix
//! weights, the per-group/per-topology minimum progress aggregation and
//! the structured receipt (schema v2).

use super::*;

const FIXTURE_IR: &str = "tests/fixtures/models/cabsim_ir_pgo.wav";

fn ideal_ir_sample(i: usize, rate: f64) -> f64 {
    let t = i as f64 / rate;
    (-400.0 * t).exp() * (2.0 * std::f64::consts::PI * 1800.0 * t).sin()
        + 0.35 * (-280.0 * t).exp() * (2.0 * std::f64::consts::PI * 3200.0 * t).sin()
        - 0.12 * (-200.0 * t).exp() * (2.0 * std::f64::consts::PI * 450.0 * t).sin()
}

fn ideal_ir_quantized(sample_count: usize, rate: f64) -> Vec<i16> {
    let raw: Vec<f64> = (0..sample_count)
        .map(|i| ideal_ir_sample(i, rate))
        .collect();
    let peak = raw.iter().fold(0.0f64, |acc, &v| acc.max(v.abs()));
    raw.iter()
        .map(|&v| (0.95 * v / peak * 32767.0).round() as i16)
        .collect()
}

#[test]
fn ir_fixture_is_deterministic_mono_48k_512() {
    let ir = CabSimIr::load(Path::new(FIXTURE_IR), 48_000, false)
        .expect("fixture IR must load with the engine's own parser");
    assert_eq!(ir.sample_rate, 48_000, "fixture must be 48 kHz");
    assert_eq!(ir.original_rate, 48_000, "fixture must be native 48 kHz");
    assert_eq!(ir.samples.len(), 512, "fixture must contain 512 samples");
    assert!(
        !ir.normalized,
        "fixture checked with normalize=false must stay raw"
    );

    let expected = ideal_ir_quantized(512, 48_000.0);
    assert_eq!(expected.len(), ir.samples.len());

    // PCM16 quantization: the committed fixture must match the documented
    // formula within one 16-bit LSB (Python `round` vs Rust `round` may differ
    // only on exact half-LSB ties, which are not representable here).
    let mut max_lsb_deviation = 0i32;
    for (i, (&loaded, &ideal)) in ir.samples.iter().zip(expected.iter()).enumerate() {
        let loaded_pcm = (loaded * 32767.0).round() as i32;
        let deviation = (loaded_pcm - i32::from(ideal)).abs();
        max_lsb_deviation = max_lsb_deviation.max(deviation);
        assert!(
            deviation <= 1,
            "sample {i}: loaded {loaded} deviates from the documented formula by {deviation} LSB"
        );
    }
    assert!(
        max_lsb_deviation <= 1,
        "max LSB deviation must stay within 1 LSB"
    );

    let peak = ir.samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
    assert!(
        (peak - 0.95).abs() < 0.01,
        "fixture must be normalized to 0.95 peak (got {peak})"
    );

    // Representative exponential decay: the tail must be far below the peak.
    let tail_peak = ir.samples[ir.samples.len() - 16..]
        .iter()
        .fold(0.0f32, |acc, &s| acc.max(s.abs()));
    assert!(
        tail_peak < 0.06,
        "fixture tail must decay exponentially (tail_peak={tail_peak})"
    );
}

#[test]
fn ir_fixture_ir_loads_with_normalize() {
    let ir = CabSimIr::load(Path::new(FIXTURE_IR), 48_000, true)
        .expect("fixture IR must load with normalization for the convolution path");
    assert_eq!(ir.samples.len(), 512);
    let peak = ir.samples.iter().fold(0.0f32, |acc, &s| acc.max(s.abs()));
    assert!(
        (peak - 1.0).abs() < 1e-3,
        "normalized fixture must peak at 1.0 (got {peak})"
    );
}

#[test]
fn classify_fixture_topologies() {
    assert_eq!(
        classify_model_file(Path::new("tests/fixtures/models/wavenet_a1_standard.nam")),
        Topology::WavenetA1
    );
    assert_eq!(
        classify_model_file(Path::new("tests/fixtures/models/a2_example.nam")),
        Topology::WavenetA2
    );
    assert_eq!(
        classify_model_file(Path::new("tests/fixtures/models/lstm.nam")),
        Topology::Lstm
    );
}

#[test]
fn json_emitter_escapes_strings_and_sorts_keys() {
    let mut obj = JsonValue::new_obj();
    obj.insert("b", JsonValue::Int(2));
    obj.insert("a", JsonValue::Int(1));
    obj.insert("quote", JsonValue::Str("a\"b\\c\nd".to_string()));
    obj.insert(
        "arr",
        JsonValue::Arr(vec![JsonValue::Bool(true), JsonValue::Bool(false)]),
    );
    obj.insert("ctl", JsonValue::Str("\u{01}".to_string()));
    let json = obj.to_json_string();

    assert!(
        json.contains(r#"{"a":1,"arr":[true,false],"b":2,"ctl":"\u0001","quote":"a\"b\\c\nd"#),
        "keys must be sorted and strings escaped: {json}"
    );
    assert!(
        json.contains(r#"[true,false]"#),
        "arrays must render: {json}"
    );
}

// ── Coverage matrix tests ───────────────────────────────────────────────────

#[test]
fn matrix_weights_sum_to_one_per_dimension() {
    assert!(
        (RATE_WEIGHTS.iter().sum::<f64>() - 1.0).abs() < 1e-9,
        "rate weights must sum to 1 (typical use distribution)"
    );
    assert!(
        (QUANTUM_WEIGHTS.iter().sum::<f64>() - 1.0).abs() < 1e-9,
        "quantum weights must sum to 1"
    );
    let os_sum: f64 = OS_WEIGHTS.iter().map(|(_, w)| *w).sum();
    assert!(
        (os_sum - 1.0).abs() < 1e-9,
        "oversampling weights must sum to 1"
    );
    assert!(
        (CABSIM_IR_WEIGHT + CABSIM_BYPASS_WEIGHT - 1.0).abs() < 1e-9,
        "CabSim weights must sum to 1"
    );
    assert!(
        (RECORDING_NO_WEIGHT + RECORDING_YES_WEIGHT - 1.0).abs() < 1e-9,
        "recording weights must sum to 1"
    );
    assert!(
        (GATE_ON_WEIGHT + GATE_OFF_WEIGHT - 1.0).abs() < 1e-9,
        "gate weights must sum to 1"
    );
    // Documented typical-use dominance: 48 kHz and 64-frame quantum are the
    // most-weighted cells (low-latency default first).
    const {
        assert!(RATE_WEIGHTS[1] > RATE_WEIGHTS[0]);
        assert!(RATE_WEIGHTS[1] > RATE_WEIGHTS[2]);
        assert!(QUANTUM_WEIGHTS[0] > QUANTUM_WEIGHTS[1]);
        assert!(QUANTUM_WEIGHTS[0] > QUANTUM_WEIGHTS[2]);
    }
}

#[test]
fn matrix_dimensions_cover_all_mandatory_values() {
    assert_eq!(RATES_HZ, [44_100, 48_000, 96_000]);
    assert_eq!(QUANTUMS, [64, 256, 512]);
    let os: Vec<&str> = OS_WEIGHTS.iter().map(|(m, _)| *m).collect();
    assert_eq!(os, [MODE_OFF, MODE_2X, MODE_4X]);
}

#[test]
fn per_topology_min_progress_aggregates_group_minima() {
    // Emulate the aggregation over a small set of cells for one topology:
    // resampler/inference/bridge run on every cell, oversample only on 2x/4x,
    // cabsim only on ir, recording only on recording cells.
    let topo = TOPOLOGY_WAVENET_A1;
    let mut progress = ProgressReport::default();
    progress.min_blocks_per_topology.insert(topo, u64::MAX);
    progress.min_frames_per_topology.insert(topo, u64::MAX);
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
            .insert(topo, 0);
    }

    let tighten_min = |slot: &mut u64, v: u64| *slot = (*slot).min(v);
    let tighten_group = |slot: &mut u64, v: u64| {
        if *slot == 0 || v < *slot {
            *slot = v;
        }
    };

    // Cells: (blocks, frames, os_on, ir, rec_on)
    for (blocks, frames, os_on, ir, rec) in [
        (4, 256, false, true, false),
        (4, 128, true, true, false),
        (4, 512, true, false, true),
        (4, 64, false, false, true),
    ] {
        let resampler = blocks * 64;
        tighten_min(
            progress.min_blocks_per_topology.get_mut(topo).unwrap(),
            blocks,
        );
        tighten_min(
            progress.min_frames_per_topology.get_mut(topo).unwrap(),
            frames,
        );
        tighten_group(
            progress
                .groups
                .get_mut("resampler")
                .unwrap()
                .min_frames_per_topology
                .get_mut(topo)
                .unwrap(),
            resampler,
        );
        tighten_group(
            progress
                .groups
                .get_mut("inference")
                .unwrap()
                .min_frames_per_topology
                .get_mut(topo)
                .unwrap(),
            frames,
        );
        tighten_group(
            progress
                .groups
                .get_mut("bridge")
                .unwrap()
                .min_frames_per_topology
                .get_mut(topo)
                .unwrap(),
            frames,
        );
        if os_on {
            tighten_group(
                progress
                    .groups
                    .get_mut("oversample")
                    .unwrap()
                    .min_frames_per_topology
                    .get_mut(topo)
                    .unwrap(),
                frames,
            );
        }
        if ir {
            tighten_group(
                progress
                    .groups
                    .get_mut("cabsim")
                    .unwrap()
                    .min_frames_per_topology
                    .get_mut(topo)
                    .unwrap(),
                frames,
            );
        }
        if rec {
            tighten_group(
                progress
                    .groups
                    .get_mut("recording")
                    .unwrap()
                    .min_frames_per_topology
                    .get_mut(topo)
                    .unwrap(),
                frames,
            );
        }
    }

    assert_eq!(progress.min_blocks_per_topology.get(topo).copied(), Some(4));
    assert_eq!(
        progress.min_frames_per_topology.get(topo).copied(),
        Some(64),
        "min frames is the smallest cell"
    );
    // Oversample only ran on the os cells (128, 512) → min 128.
    assert_eq!(
        progress
            .groups
            .get("oversample")
            .unwrap()
            .min_frames_per_topology
            .get(topo)
            .copied(),
        Some(128)
    );
    // Cabsim only ran on ir cells (256, 128) → min 128.
    assert_eq!(
        progress
            .groups
            .get("cabsim")
            .unwrap()
            .min_frames_per_topology
            .get(topo)
            .copied(),
        Some(128)
    );
    // Recording only ran on rec cells (512, 64) → min 64.
    assert_eq!(
        progress
            .groups
            .get("recording")
            .unwrap()
            .min_frames_per_topology
            .get(topo)
            .copied(),
        Some(64)
    );
    // Resampler ran on every cell → min over {256,128,512,64} → 256.
    assert_eq!(
        progress
            .groups
            .get("resampler")
            .unwrap()
            .min_frames_per_topology
            .get(topo)
            .copied(),
        Some(256)
    );
    // The ProgressReport renders min_samples_per_topology as 2× min_frames.
    let report_json = progress.to_json().to_json_string();
    assert!(
        report_json.contains(r#""wavenet_a1":128"#),
        "min_samples_per_topology must double the min frames: {report_json}"
    );
}

#[test]
fn receipt_json_contains_matrix_fields() {
    let mut receipt = WorkloadReceipt::new();
    receipt.ir = "tests/fixtures/models/cabsim_ir_pgo.wav".to_string();
    receipt.gate_disabled = true;
    receipt.no_stage_skipped = true;
    Coverage::bump(&mut receipt.coverage.topologies, TOPOLOGY_WAVENET_A1, 2400);
    Coverage::bump(&mut receipt.coverage.topologies, TOPOLOGY_WAVENET_A2, 2400);
    Coverage::bump(&mut receipt.coverage.topologies, TOPOLOGY_LSTM, 2400);
    Coverage::bump(&mut receipt.coverage.oversampling, MODE_OFF, 3600);
    Coverage::bump(&mut receipt.coverage.oversampling, MODE_2X, 1800);
    Coverage::bump(&mut receipt.coverage.oversampling, MODE_4X, 1800);
    for rate in [44_100, 48_000, 96_000] {
        Coverage::bump(&mut receipt.coverage.rates, &rate.to_string(), 2400);
    }
    for q in [64, 256, 512] {
        Coverage::bump(&mut receipt.coverage.quantums, &q.to_string(), 2400);
    }
    Coverage::bump(&mut receipt.coverage.cabsim, "ir", 3600);
    Coverage::bump(&mut receipt.coverage.cabsim, "bypass", 3600);
    Coverage::bump(&mut receipt.coverage.recording, REC_NO, 3600);
    Coverage::bump(&mut receipt.coverage.recording, REC_YES, 3600);
    Coverage::bump(&mut receipt.coverage.gate, GATE_ON, 3600);
    Coverage::bump(&mut receipt.coverage.gate, GATE_OFF, 3600);

    for topo in [TOPOLOGY_WAVENET_A1, TOPOLOGY_WAVENET_A2, TOPOLOGY_LSTM] {
        receipt.progress.min_blocks_per_topology.insert(topo, 2400);
        receipt
            .progress
            .min_frames_per_topology
            .insert(topo, 153600);
        for group in [
            "resampler",
            "inference",
            "oversample",
            "cabsim",
            "bridge",
            "recording",
        ] {
            receipt
                .progress
                .groups
                .entry(group)
                .or_default()
                .min_frames_per_topology
                .insert(topo, 153600);
        }
    }
    receipt.progress.total_frames = 460800;
    receipt.progress.total_samples = 921600;
    receipt.cabsim_total_frames = 460800;

    receipt.cells.push(CellRecord {
        rate: 48_000,
        quantum: 64,
        topology: TOPOLOGY_WAVENET_A1,
        os_mode: MODE_OFF,
        cabsim_ir: true,
        recording: true,
        gate: true,
        progress: CellProgress {
            blocks: 2400,
            frames: 153600,
            resampler_frames: 153600,
            oversample_frames: 0,
            cabsim_frames: 153600,
            recording_frames: 153600,
            recording_accepted: 2400,
            recording_overruns: 0,
        },
    });

    let json = receipt.to_json().to_json_string();
    for needle in [
        r#""schema_version":2"#,
        r#""tool":"pgo_workload""#,
        r#""rates_hz":[44100,48000,96000]"#,
        r#""quantums_frames":[64,256,512]"#,
        r#""gate_modes":["on","off"]"#,
        r#""no_stage_skipped":true"#,
        r#""disabled":true"#,
        r#""min_blocks_per_topology"#,
        r#""min_frames_per_topology"#,
        r#""min_samples_per_topology"#,
        r#""topology_blocks":{"lstm":2400,"wavenet_a1":2400,"wavenet_a2":2400}"#,
        r#""oversampling_blocks":{"2x":1800,"4x":1800,"Off":3600}"#,
        r#""recording":{"no":3600,"yes":3600}"#,
        r#""gate":{"off":3600,"on":3600}"#,
    ] {
        assert!(
            json.contains(needle),
            "receipt JSON missing {needle}: {json}"
        );
    }
}

#[test]
fn mandatory_gate_sets_cover_all_families() {
    // The receipt matrix metadata must enumerate the exact mandatory values so
    // a future edit cannot silently drop one dimension value.
    assert_eq!(RATES_HZ.len(), 3);
    assert_eq!(QUANTUMS.len(), 3);
    assert_eq!(OS_WEIGHTS.len(), 3);
    let coverage = Coverage::default();
    assert!(coverage.topologies.is_empty());
    assert!(coverage.recording.is_empty());
    assert!(coverage.gate.is_empty());
}
