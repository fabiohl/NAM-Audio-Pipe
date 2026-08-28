// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the PGO workload (`pgo_workload.rs`).
//!
//! Validates the deterministic CabSim IR fixture against its documented
//! generation formula, the topology classification table, the JSON receipt
//! emitter and the mandatory-stage gates (F-RB-013 / T5.3).

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

#[test]
fn receipt_json_contains_mandatory_stage_fields() {
    let mut receipt = WorkloadReceipt::new();
    receipt.ir = "tests/fixtures/models/cabsim_ir_pgo.wav".to_string();
    let mut os = BTreeMap::new();
    os.insert(MODE_OFF, 1200u64);
    os.insert(MODE_2X, 600);
    os.insert(MODE_4X, 600);
    receipt.models.push(ModelReceipt {
        path: "tests/fixtures/models/wavenet_a1_standard.nam".to_string(),
        topology: TOPOLOGY_WAVENET_A1,
        sample_rate: 48_000,
        blocks: 2400,
        oversampling: os,
        cabsim_frames: 2400 * 64,
        cabsim_blocks: 2400,
    });
    *receipt
        .topology_blocks
        .get_mut(TOPOLOGY_WAVENET_A1)
        .expect("bucket") = 2400;
    *receipt
        .oversampling_blocks
        .get_mut(MODE_OFF)
        .expect("bucket") = 1200;
    *receipt
        .oversampling_blocks
        .get_mut(MODE_2X)
        .expect("bucket") = 600;
    *receipt
        .oversampling_blocks
        .get_mut(MODE_4X)
        .expect("bucket") = 600;
    receipt.cabsim_frames = 2400 * 64;
    receipt.cabsim_blocks = 2400;
    receipt.no_stage_skipped = true;

    let json = receipt.to_json().to_json_string();
    for needle in [
        r#""schema_version":1"#,
        r#""tool":"pgo_workload""#,
        r#""topology_blocks":{"lstm":0,"wavenet_a1":2400,"wavenet_a2":0}"#,
        r#""oversampling_blocks":{"2x":600,"4x":600,"Off":1200}"#,
        r#""stereo_convolved_frames":153600"#,
        r#""no_stage_skipped":true"#,
    ] {
        assert!(
            json.contains(needle),
            "receipt JSON missing {needle}: {json}"
        );
    }
}

#[test]
fn mandatory_topology_gates_cover_all_families() {
    // The fail-closed gate set must list the three mandatory topologies so a
    // future edit cannot silently drop one from the validation loop.
    let receipt = WorkloadReceipt::new();
    assert_eq!(
        receipt.topology_blocks.len(),
        3,
        "receipt must track exactly the mandatory topologies"
    );
    for topology in [TOPOLOGY_WAVENET_A1, TOPOLOGY_WAVENET_A2, TOPOLOGY_LSTM] {
        assert!(
            receipt.topology_blocks.contains_key(topology),
            "mandatory topology {topology} missing from the receipt"
        );
    }
    for mode in [MODE_OFF, MODE_2X, MODE_4X] {
        assert!(
            receipt.oversampling_blocks.contains_key(mode),
            "oversampling mode {mode} missing from the receipt"
        );
    }
}
