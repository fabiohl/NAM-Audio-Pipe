// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration tests for Input Calibration and Output Modes in NAM-Audio-Pipe.
//!
//! Verifies:
//! 1. CLI defaults (`--calibrate-input off`, `--input-calibration-level 12.0`,
//!    `--output-mode normalized`) and strict startup metadata validation in
//!    `setup::try_load_initial_model` (explicit missing metadata fails fast;
//!    implicit `normalized` without `loudness` falls back to 1.0x with warning).
//! 2. Exact `f64` oracle parity across `raw`, `normalized`, `calibrated`,
//!    extreme dBu `[-60, +60]`, `.nam` and `.namb` v2 models.
//! 3. Pre-nonlinearity `calibrate_input` behavior on `lstm.nam` below/above saturation.
//! 4. Gate threshold evaluation on raw pre-calibration input and CabSim IR
//!    scaling isolation from amp calibration multipliers.

#![cfg(feature = "testing")]

use nam_audio_pipe::standalone::calibration::{
    CalibrationConfig, ModelCalibrationMeta, OutputMode, compute_calibration_multipliers,
    resolve_model_calibration,
};
use nam_audio_pipe::standalone::cli::GateConfig;
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use nam_audio_pipe::standalone::setup::{setup_communication_channels, try_load_initial_model};
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::ParamPayload;
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::loader::nam_json::{WeightsLayout, parse_nam_json};
use neural_amp_modeler_rs::loader::namb_encoder::encode_namb;
use neural_amp_modeler_rs::loader::{LoadOptions, load_and_build_model};
use neural_amp_modeler_rs::testing::perceptual::compute_esr;
use neural_amp_modeler_rs::testing::reference_oracle::{PrecisionConfig, oracle_forward};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK: usize = 128;

fn fixture_model(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/models")
        .join(name)
}

fn unique_tmp_path(tag: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "nam_pipe_cal_{tag}_{}_{nanos}.{ext}",
        std::process::id()
    ))
}

fn sine_wave(len: usize, freq_hz: f32, amp: f32, sample_rate: f64) -> Vec<f32> {
    (0..len)
        .map(|i| {
            let t = i as f64 / sample_rate;
            amp * (2.0 * std::f64::consts::PI * f64::from(freq_hz) * t).sin() as f32
        })
        .collect()
}

fn drive_harness(
    model_path: &Path,
    cal: CalibrationConfig,
    gate: GateConfig,
    cab_ir: Option<CabSimPair>,
    stimulus: &[f32],
) -> Vec<f32> {
    let sys = SystemSnapshot::capture();
    let loaded = load_and_build_model(model_path, &sys, true, LoadOptions::default())
        .expect("load_and_build_model");
    let meta = ModelCalibrationMeta {
        has_model: true,
        loudness: loaded.loudness(),
        input_level_dbu: loaded.input_level_dbu(),
        output_level_dbu: loaded.output_level_dbu(),
    };
    let (in_mult, out_mult) = compute_calibration_multipliers(meta, cal);

    let mut harness = RtSwapHarness::new_with_gate_config(48_000, 48_000, gate)
        .expect("RtSwapHarness::new_with_gate_config");
    harness.push_load_model(
        loaded.model_l,
        loaded.model_r,
        in_mult,
        out_mult,
        loaded.sample_rate,
    );
    if let Some(pair) = cab_ir {
        harness.push_cabsim(Some(Box::new(pair)));
    }

    let mut out = Vec::with_capacity(stimulus.len());
    for chunk in stimulus.chunks(BLOCK) {
        let mut in_l = chunk.to_vec();
        let mut in_r = chunk.to_vec();
        let n = harness.run_callback(&mut in_l, &mut in_r, chunk.len());
        out.extend_from_slice(&harness.out_l()[..n]);
    }
    out
}

fn oracle_calibrated_f32(
    model_path: &Path,
    input: &[f32],
    in_mult: f32,
    out_mult: f32,
) -> Vec<f32> {
    let json = std::fs::read_to_string(model_path).expect("read model json");
    let data = parse_nam_json(&json).expect("parse model json");
    let scaled_in: Vec<f64> = input
        .iter()
        .map(|&x| f64::from(x) * f64::from(in_mult))
        .collect();
    let out = oracle_forward(&data, &scaled_in, &PrecisionConfig::default());
    out.into_iter()
        .map(|y| (y * f64::from(out_mult)) as f32)
        .collect()
}

#[test]
fn startup_validation_rejects_missing_explicit_metadata_and_warns_on_implicit_normalized() {
    let sys = SystemSnapshot::capture();
    let base_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture_model("lstm.nam")).unwrap()).unwrap();

    // Missing all metadata
    let mut no_meta = base_json.clone();
    no_meta
        .as_object_mut()
        .unwrap()
        .insert("metadata".to_string(), serde_json::json!({}));
    let no_meta_path = unique_tmp_path("no_meta", "nam");
    std::fs::write(&no_meta_path, serde_json::to_vec(&no_meta).unwrap()).unwrap();

    // 1. Implicit normalized (output_mode_explicit = false): succeeds with fallback 1.0x
    let mut ch = setup_communication_channels().unwrap();
    let setup_res = try_load_initial_model(
        Some(&no_meta_path),
        &sys,
        CalibrationConfig::default(),
        false,
        &mut ch.param_producer,
    );
    assert!(
        setup_res.is_ok(),
        "implicit normalized lacking loudness should fall back cleanly: {:?}",
        setup_res.as_ref().err()
    );
    let payload = ch.param_consumer.pop().expect("LoadModel payload pushed");
    match payload {
        ParamPayload::LoadModel {
            input_mult_adj,
            output_mult_adj,
            ..
        } => {
            assert_eq!(input_mult_adj, 1.0);
            assert_eq!(output_mult_adj, 1.0);
        }
        _ => panic!("unexpected ParamPayload (expected LoadModel)"),
    }

    // 2. Explicit --output-mode normalized lacking loudness -> fatal error
    let mut ch2 = setup_communication_channels().unwrap();
    let err_norm = try_load_initial_model(
        Some(&no_meta_path),
        &sys,
        CalibrationConfig::default(),
        true,
        &mut ch2.param_producer,
    );
    assert!(err_norm.is_err());

    // 3. Explicit --calibrate-input on lacking input_level_dbu -> fatal error
    let mut ch3 = setup_communication_channels().unwrap();
    let err_cal_in = try_load_initial_model(
        Some(&no_meta_path),
        &sys,
        CalibrationConfig {
            calibrate_input: true,
            ..CalibrationConfig::default()
        },
        false,
        &mut ch3.param_producer,
    );
    assert!(err_cal_in.is_err());

    // 4. Explicit --output-mode calibrated lacking output_level_dbu -> fatal error
    let mut ch4 = setup_communication_channels().unwrap();
    let err_cal_out = try_load_initial_model(
        Some(&no_meta_path),
        &sys,
        CalibrationConfig {
            output_mode: OutputMode::Calibrated,
            ..CalibrationConfig::default()
        },
        true,
        &mut ch4.param_producer,
    );
    assert!(err_cal_out.is_err());

    // 5. Explicit --output-mode raw lacking metadata -> succeeds with (1.0, 1.0)
    let mut ch5 = setup_communication_channels().unwrap();
    let raw_ok = try_load_initial_model(
        Some(&no_meta_path),
        &sys,
        CalibrationConfig {
            output_mode: OutputMode::Raw,
            ..CalibrationConfig::default()
        },
        true,
        &mut ch5.param_producer,
    );
    assert!(raw_ok.is_ok());

    let _ = std::fs::remove_file(&no_meta_path);
}

#[test]
fn pipe_oracle_parity_across_modes_namb_and_saturation() {
    let model = fixture_model("lstm.nam");
    let stim = sine_wave(BLOCK * 24, 330.0, 0.25, 48_000.0);
    let meta = ModelCalibrationMeta {
        has_model: true,
        loudness: Some(-37.840_687),
        input_level_dbu: Some(18.3),
        output_level_dbu: Some(12.3),
    };

    for cal in [
        CalibrationConfig::new_project_default(),
        CalibrationConfig {
            calibrate_input: false,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Raw,
        },
        CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: -60.0,
            output_mode: OutputMode::Calibrated,
        },
        CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 60.0,
            output_mode: OutputMode::Calibrated,
        },
    ] {
        let produced = drive_harness(&model, cal, GateConfig::Off, None, &stim);
        let resolved = resolve_model_calibration(meta, cal, true).unwrap();
        let expected =
            oracle_calibrated_f32(&model, &stim, resolved.input_mult, resolved.output_mult);
        let esr = compute_esr(&expected[BLOCK * 8..], &produced[BLOCK * 8..]);
        assert!(
            esr < 1e-8,
            "Pipe oracle parity failed for {cal:?}: in={}, out={}, esr={esr:e}",
            resolved.input_mult,
            resolved.output_mult
        );
    }

    // Verify .namb v2 format yields identical output under calibrated settings.
    let data = parse_nam_json(&std::fs::read_to_string(&model).unwrap()).unwrap();
    let namb_bytes = encode_namb(&data, 2, WeightsLayout::Original).unwrap();
    let namb_path = unique_tmp_path("lstm", "namb");
    std::fs::write(&namb_path, &namb_bytes).unwrap();
    let namb_cal = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: 6.0,
        output_mode: OutputMode::Calibrated,
    };
    let out_nam = drive_harness(&model, namb_cal, GateConfig::Off, None, &stim);
    let out_namb = drive_harness(&namb_path, namb_cal, GateConfig::Off, None, &stim);
    let esr_namb = compute_esr(&out_nam[BLOCK * 6..], &out_namb[BLOCK * 6..]);
    assert!(esr_namb < 1e-12, ".nam vs .namb ESR={esr_namb:e}");
    let _ = std::fs::remove_file(&namb_path);

    // Pre-nonlinearity saturation test on lstm.nam (-6 dBu vs +30 dBu)
    let sat_stim = sine_wave(BLOCK * 20, 220.0, 0.65, 48_000.0);
    let out_low = drive_harness(
        &model,
        CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: -6.0,
            output_mode: OutputMode::Raw,
        },
        GateConfig::Off,
        None,
        &sat_stim,
    );
    let out_high = drive_harness(
        &model,
        CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 30.0,
            output_mode: OutputMode::Raw,
        },
        GateConfig::Off,
        None,
        &sat_stim,
    );
    let peak_low = out_low[BLOCK * 6..]
        .iter()
        .fold(0.0f32, |m, &x| m.max(x.abs()));
    let peak_high = out_high[BLOCK * 6..]
        .iter()
        .fold(0.0f32, |m, &x| m.max(x.abs()));
    let ratio = peak_high / peak_low.max(1e-9);
    let linear = 10.0_f32.powf(36.0 / 20.0);
    assert!(
        ratio < linear * 0.75,
        "expected nonlinear saturation compression: ratio={ratio:.2}, linear={linear:.2}"
    );
}

#[test]
fn pipe_gate_evaluates_raw_input_and_cabsim_stays_independent() {
    let model = fixture_model("lstm.nam");

    let mut ir_samples = vec![0.0f32; 256];
    ir_samples[0] = 1.0;
    let make_pair = || CabSimPair {
        l: Box::new(
            CabSimAdapter::new(Box::new(ConvEngine::new(&ir_samples, 128).unwrap())).unwrap(),
        ),
        r: Box::new(
            CabSimAdapter::new(Box::new(ConvEngine::new(&ir_samples, 128).unwrap())).unwrap(),
        ),
        sample_rate: 48_000,
    };

    // Wait past gate hold + fade + 256-sample IR drain on 0.002 (-54 dBFS)
    // with gate open at -40 dBFS: +60 dBu calibration must NOT re-open the gate.
    // Once DynamicHysteresis closes, capture_dsp_pipeline_streaming returns n=0
    // for idle gated blocks (gated_out stops growing while ungated stays loud).
    let quiet = sine_wave(BLOCK * 64, 440.0, 0.002, 48_000.0);
    let hot_cal = CalibrationConfig {
        calibrate_input: true,
        input_calibration_level_dbu: 60.0,
        output_mode: OutputMode::Normalized,
    };
    let gated_out = drive_harness(
        &model,
        hot_cal,
        GateConfig::from_open_db(-40.0),
        Some(make_pair()),
        &quiet,
    );
    assert!(
        gated_out.len() <= BLOCK * 32,
        "Pipe gate must evaluate raw pre-calibration input and close early (emitted {} of {} samples)",
        gated_out.len(),
        quiet.len()
    );

    let ungated_out = drive_harness(
        &model,
        hot_cal,
        GateConfig::Off,
        Some(make_pair()),
        &quiet[..BLOCK * 24],
    );
    let ungated_peak = ungated_out[BLOCK * 8..]
        .iter()
        .fold(0.0f32, |m, &x| m.max(x.abs()));
    assert!(
        ungated_peak > 0.1,
        "Without gate, +60 dBu calibration on 0.002 + CabSim should drive loud output, got {ungated_peak}"
    );
}
