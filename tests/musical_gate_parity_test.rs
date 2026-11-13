// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration & parity tests for the optional musical noise gate (`--gate-mode musical`)
//! in `NAM-Audio-Pipe`.
//!
//! Verifies:
//! 1. Exact sample-by-sample parity against the `std::f64` C++ `NoiseGate.cpp` quadratic
//!    downward expander oracle across 44.1 / 48 / 96 kHz (`ESR < 1e-11`).
//! 2. Detector placement (post user input gain, pre model `cal_in`) and gain placement
//!    (post model inference, pre CabSim IR) preserving natural cabinet tail decay.
//! 3. `--gate off` (`GateConfig::Off`) bypass parity and stereo L/R channel independence.
//! 4. Continuous processing without Silence-mode cutouts (`n_pw == n_samples`,
//!    `RT_STATUS_IS_SILENT` cleared) compatible with continuous `--record` WAV capture.
//! 5. Dynamic runtime `GateConfig` command handling and zero-heap-allocation proof on RT.

#![cfg(feature = "testing")]

use nam_audio_pipe::dsp::musical_gate::{
    MAXIMUM_LOUDNESS_POWER, MINIMUM_LOUDNESS_DB, MINIMUM_LOUDNESS_POWER,
    MUSICAL_GATE_CLOSE_TIME_SEC, MUSICAL_GATE_HOLD_TIME_SEC, MUSICAL_GATE_OPEN_TIME_SEC,
    MUSICAL_GATE_RATIO, MUSICAL_GATE_TIME_SEC,
};
use nam_audio_pipe::standalone::calibration::{
    CalibrationConfig, ModelCalibrationMeta, OutputMode, compute_calibration_multipliers,
};
use nam_audio_pipe::standalone::cli::{GateConfig, GateMode};
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_IS_SILENT;
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::dsp::gate::GateParams;
use neural_amp_modeler_rs::loader::{LoadOptions, load_and_build_model};
use neural_amp_modeler_rs::testing::perceptual::compute_esr;
use std::path::PathBuf;

const BLOCK: usize = 128;

fn fixture_model(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/models")
        .join(name)
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

fn make_decay_cabsim(sample_rate: u32, len: usize, decay: f32) -> CabSimPair {
    let mut ir = Vec::with_capacity(len);
    let mut g = 1.0_f32;
    for _ in 0..len {
        ir.push(g);
        g *= decay;
    }
    CabSimPair {
        l: Box::new(CabSimAdapter::new(Box::new(ConvEngine::new(&ir, BLOCK).unwrap())).unwrap()),
        r: Box::new(CabSimAdapter::new(Box::new(ConvEngine::new(&ir, BLOCK).unwrap())).unwrap()),
        sample_rate,
    }
}

fn cpp_reference_gate_process(samples: &[f64], sample_rate: f64, threshold_db: f64) -> Vec<f64> {
    let alpha = 0.5_f64.powf(1.0 / (MUSICAL_GATE_TIME_SEC * sample_rate));
    let beta = 1.0 - alpha;
    let dt = 1.0 / sample_rate;
    let gr_at = |lvl_db: f64| -> f64 {
        if lvl_db < threshold_db {
            let d = lvl_db - threshold_db;
            -MUSICAL_GATE_RATIO * d * d
        } else {
            0.0
        }
    };
    let max_gr = gr_at(MINIMUM_LOUDNESS_DB);
    let d_open = -max_gr / (MUSICAL_GATE_OPEN_TIME_SEC * sample_rate);
    let d_close = max_gr / (MUSICAL_GATE_CLOSE_TIME_SEC * sample_rate);

    let mut level = MINIMUM_LOUDNESS_POWER;
    let mut last_gr = max_gr;
    let mut time_held = 0.0;
    let mut holding = false;
    let mut out = Vec::with_capacity(samples.len());

    for &s in samples {
        level =
            (alpha * level + beta * (s * s)).clamp(MINIMUM_LOUDNESS_POWER, MAXIMUM_LOUDNESS_POWER);
        let level_db = 10.0 * level.log10();

        let gain = if holding {
            last_gr = 0.0;
            if level_db < threshold_db {
                time_held += dt;
                if time_held >= MUSICAL_GATE_HOLD_TIME_SEC {
                    holding = false;
                }
            } else {
                time_held = 0.0;
            }
            1.0
        } else {
            let target_gr = gr_at(level_db);
            if target_gr > last_gr {
                let d_gain = (0.5 * (target_gr - last_gr)).clamp(0.0, d_open);
                last_gr += d_gain;
                if last_gr >= 0.0 {
                    last_gr = 0.0;
                    holding = true;
                    time_held = 0.0;
                    1.0
                } else {
                    10.0_f64.powf(last_gr / 10.0)
                }
            } else {
                if target_gr < last_gr {
                    let d_gain = (0.5 * (target_gr - last_gr)).clamp(d_close, 0.0);
                    last_gr = (last_gr + d_gain).max(max_gr);
                }
                10.0_f64.powf(last_gr / 10.0)
            }
        };
        out.push(s * gain);
    }
    out
}

fn load_model_into_harness(harness: &mut RtSwapHarness, model_name: &str, cal: CalibrationConfig) {
    let sys = SystemSnapshot::capture();
    let loaded = load_and_build_model(
        &fixture_model(model_name),
        &sys,
        true,
        LoadOptions::default(),
    )
    .expect("load model fixture");
    let meta = ModelCalibrationMeta {
        has_model: true,
        loudness: loaded.loudness(),
        input_level_dbu: loaded.input_level_dbu(),
        output_level_dbu: loaded.output_level_dbu(),
    };
    let (in_mult, out_mult) = compute_calibration_multipliers(meta, cal);
    harness.push_load_model(
        loaded.model_l,
        loaded.model_r,
        in_mult,
        out_mult,
        loaded.sample_rate,
    );
    harness.run_prologue();
}

#[test]
fn t01_musical_gate_cpp_oracle_parity_across_rates() {
    for sr in [44_100_u32, 48_000, 96_000] {
        let mut harness = RtSwapHarness::new_with_gate_config_and_mode(
            sr,
            sr,
            GateConfig::from_open_db(-52.0),
            GateMode::Musical,
        )
        .expect("harness init");
        assert_eq!(harness.gate_mode(), GateMode::Musical);

        let n_total = (sr as usize / 2) - ((sr as usize / 2) % BLOCK);
        let mut stim = Vec::with_capacity(n_total);
        for i in 0..n_total {
            let amp = if i < n_total / 5 {
                0.0001
            } else if i < (3 * n_total) / 5 {
                0.35
            } else {
                0.0005
            };
            let phase = (i as f64) * 2.0 * std::f64::consts::PI * 196.0 / f64::from(sr);
            stim.push((amp * phase.sin()) as f32);
        }

        let stim_f64: Vec<f64> = stim.iter().map(|&x| f64::from(x)).collect();
        let expected: Vec<f32> = cpp_reference_gate_process(&stim_f64, f64::from(sr), -52.0)
            .into_iter()
            .map(|y| y as f32)
            .collect();

        let mut actual = Vec::with_capacity(n_total);
        for chunk in stim.as_chunks::<BLOCK>().0 {
            let mut in_l = chunk.to_vec();
            let mut in_r = chunk.to_vec();
            let n_pw = harness.run_callback(&mut in_l, &mut in_r, BLOCK);
            assert_eq!(n_pw, BLOCK);
            actual.extend_from_slice(&harness.out_l()[..n_pw]);
        }

        let esr = compute_esr(&actual, &expected);
        let max_err = actual
            .iter()
            .zip(expected.iter())
            .map(|(&a, &e)| (a - e).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            esr < 1e-11,
            "Musical gate ESR vs C++ oracle too high at {sr} Hz: {esr:.3e}"
        );
        assert!(
            max_err < 1e-5,
            "Musical gate max_err too high at {sr} Hz: {max_err:.3e}"
        );
    }
}

#[test]
fn t02_detector_pre_cal_and_gain_pre_ir_preserve_ir_decay() {
    // 1. Pre-cal detector check: extreme +48 dBu attenuation scales `cal_in` down by -36 dB,
    //    but user raw input is -34 dBFS (> -45 dB open threshold), so detector MUST stay open.
    let mut harness_cal = RtSwapHarness::new_with_gate_config_and_mode(
        48_000,
        48_000,
        GateConfig::from_open_db(-45.0),
        GateMode::Musical,
    )
    .expect("harness init");
    load_model_into_harness(
        &mut harness_cal,
        "lstm.nam",
        CalibrationConfig {
            calibrate_input: true,
            input_calibration_level_dbu: 48.0,
            output_mode: OutputMode::Raw,
        },
    );

    let active_block: Vec<f32> = (0..BLOCK)
        .map(|i| 0.028 * ((i as f32) * 0.1).sin())
        .collect();
    for _ in 0..20 {
        let mut l = active_block.clone();
        let mut r = active_block.clone();
        let n_pw = harness_cal.run_callback(&mut l, &mut r, BLOCK);
        assert_eq!(n_pw, BLOCK);
    }
    assert!(
        !harness_cal.musical_gate_attenuating(),
        "Musical gate must detect post user gain and PRE cal_in attenuation"
    );

    // 2. Pre-IR gain placement + natural CabSim IR tail:
    //    Run loud guitar signal into lstm.nam + exponential decay IR (`len = 64`), then cut
    //    input to silence. First silent block must still emit natural IR decay tail (`rms > 1e-4`),
    //    and once the 10 ms half-life power detector settles (~250 ms), output decays > 30 dB.
    let mut harness_ir = RtSwapHarness::new_with_gate_config_and_mode(
        48_000,
        48_000,
        GateConfig::from_open_db(-40.0),
        GateMode::Musical,
    )
    .expect("harness init");
    load_model_into_harness(
        &mut harness_ir,
        "lstm.nam",
        CalibrationConfig {
            calibrate_input: false,
            input_calibration_level_dbu: 12.0,
            output_mode: OutputMode::Normalized,
        },
    );
    harness_ir.push_cabsim(Some(Box::new(make_decay_cabsim(48_000, 64, 0.85))));

    for _ in 0..40 {
        let mut l: Vec<f32> = (0..BLOCK)
            .map(|i| 0.35 * ((i as f32) * 0.12).sin())
            .collect();
        let mut r = l.clone();
        let n_pw = harness_ir.run_callback(&mut l, &mut r, BLOCK);
        assert_eq!(n_pw, BLOCK);
    }
    let active_rms = rms(&harness_ir.out_l()[..BLOCK]);

    let mut l_edge = vec![0.0_f32; BLOCK];
    let mut r_edge = vec![0.0_f32; BLOCK];
    let n_edge = harness_ir.run_callback(&mut l_edge, &mut r_edge, BLOCK);
    assert_eq!(n_edge, BLOCK);
    let edge_rms = rms(&harness_ir.out_l()[..n_edge]);
    assert!(
        edge_rms > 1e-4,
        "Expected smooth CabSim IR decay tail right after input cutoff, got {edge_rms}"
    );

    for _ in 0..120 {
        let mut l = vec![0.0_f32; BLOCK];
        let mut r = vec![0.0_f32; BLOCK];
        let _ = harness_ir.run_callback(&mut l, &mut r, BLOCK);
    }
    let settled_rms = rms(&harness_ir.out_l()[..BLOCK]);
    let atten_db = 20.0 * (active_rms / settled_rms.max(1e-12)).log10();
    assert!(
        atten_db > 30.0 && harness_ir.musical_gate_attenuating(),
        "Expected settled musical gate attenuation > 30 dB, got {atten_db:.1} dB"
    );
}

#[test]
fn t03_gate_off_bypasses_and_stereo_channels_are_independent() {
    // GateConfig::Off + GateMode::Musical must be bit-exact 1.0x bypass.
    let mut off_harness = RtSwapHarness::new_with_gate_config_and_mode(
        48_000,
        48_000,
        GateConfig::Off,
        GateMode::Musical,
    )
    .expect("off harness");
    let stim: Vec<f32> = (0..BLOCK)
        .map(|i| 0.0005 * ((i as f32) * 0.15).sin())
        .collect();
    for _ in 0..10 {
        let mut l = stim.clone();
        let mut r = stim.clone();
        let n_pw = off_harness.run_callback(&mut l, &mut r, BLOCK);
        assert_eq!(n_pw, BLOCK);
        assert!(!off_harness.musical_gate_attenuating());
        for (&y, &x) in off_harness.out_l()[..n_pw].iter().zip(stim.iter()) {
            assert!(
                (y - x).abs() < 1e-5,
                "GateConfig::Off expected 1.0x pass-through"
            );
        }
    }

    // Stereo independence: hot L, silent R -> L stays open, R settles attenuated.
    let mut stereo_harness = RtSwapHarness::new_with_gate_config_and_mode(
        48_000,
        48_000,
        GateConfig::from_open_db(-45.0),
        GateMode::Musical,
    )
    .expect("stereo harness");
    for _ in 0..80 {
        let mut in_l: Vec<f32> = (0..BLOCK)
            .map(|i| 0.35 * ((i as f32) * 0.1).sin())
            .collect();
        let mut in_r = vec![0.001_f32; BLOCK];
        let n_pw = stereo_harness.run_callback(&mut in_l, &mut in_r, BLOCK);
        assert_eq!(n_pw, BLOCK);
    }
    let rms_l = rms(&stereo_harness.out_l()[..BLOCK]);
    let rms_r = rms(&stereo_harness.out_r()[..BLOCK]);
    assert!(rms_l > 0.2, "Expected hot L open, got rms_l={rms_l}");
    #[cfg(feature = "dual-mono")]
    assert!(
        rms_r < 0.0001,
        "Expected quiet R attenuated under dual-mono, got rms_r={rms_r}"
    );
    #[cfg(not(feature = "dual-mono"))]
    assert!(
        (rms_r - rms_l).abs() < 1e-5,
        "Expected R to mirror open L in mono-only build, got rms_r={rms_r}, rms_l={rms_l}"
    );
}

#[test]
fn t04_continuous_output_without_silence_trim_and_status_flag() {
    let mut silence_harness = RtSwapHarness::new_with_gate_config_and_mode(
        48_000,
        48_000,
        GateConfig::from_open_db(-50.0),
        GateMode::Silence,
    )
    .expect("silence harness");
    let mut musical_harness = RtSwapHarness::new_with_gate_config_and_mode(
        48_000,
        48_000,
        GateConfig::from_open_db(-50.0),
        GateMode::Musical,
    )
    .expect("musical harness");

    let mut silence_frames_written = 0usize;
    let mut musical_frames_written = 0usize;
    let total_blocks = 40;
    for _ in 0..total_blocks {
        let mut sl = vec![0.0_f32; BLOCK];
        let mut sr = vec![0.0_f32; BLOCK];
        silence_frames_written += silence_harness.run_callback(&mut sl, &mut sr, BLOCK);

        let mut ml = vec![0.0_f32; BLOCK];
        let mut mr = vec![0.0_f32; BLOCK];
        musical_frames_written += musical_harness.run_callback(&mut ml, &mut mr, BLOCK);
    }

    assert!(
        silence_frames_written < total_blocks * BLOCK,
        "Silence mode must trim zero-energy blocks after hold+fade"
    );
    assert!(silence_harness.rt_status().check_flag(RT_STATUS_IS_SILENT));

    assert_eq!(
        musical_frames_written,
        total_blocks * BLOCK,
        "Musical mode must run continuously without trimming blocks"
    );
    assert!(!musical_harness.rt_status().check_flag(RT_STATUS_IS_SILENT));
}

#[test]
fn t05_runtime_gate_command_and_rt_zero_alloc_audit() {
    let mut harness = RtSwapHarness::new_with_gate_config_and_mode(
        48_000,
        48_000,
        GateConfig::Off,
        GateMode::Musical,
    )
    .expect("harness init");
    load_model_into_harness(&mut harness, "lstm.nam", CalibrationConfig::default());

    // Dynamically enable threshold at runtime over SPSC command queue.
    harness.push_gate(GateParams::new(-35.0, -45.0, 2048, 256, 1e-4));
    let mut warmup_l = vec![0.0_f32; BLOCK];
    let mut warmup_r = vec![0.0_f32; BLOCK];
    for _ in 0..80 {
        warmup_l.fill(0.0);
        warmup_r.fill(0.0);
        let n_pw = harness.run_callback(&mut warmup_l, &mut warmup_r, BLOCK);
        assert_eq!(n_pw, BLOCK);
    }
    assert!(
        harness.musical_gate_attenuating(),
        "Runtime GateParams push must enable musical expander"
    );
    assert!(!harness.rt_status().check_flag(RT_STATUS_IS_SILENT));

    #[cfg(feature = "heap-audit")]
    {
        use neural_amp_modeler_rs::common::alloc_audit::{
            TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
        };
        let mut in_l: Vec<f32> = (0..BLOCK).map(|i| 0.2 * ((i as f32) * 0.1).sin()).collect();
        let mut in_r = in_l.clone();
        let (allocs, deallocs, reallocs) = {
            let _guard = TrackingGuard::new();
            for _ in 0..200 {
                let n_pw = harness.run_callback(&mut in_l, &mut in_r, BLOCK);
                assert_eq!(n_pw, BLOCK);
            }
            (get_alloc_count(), get_dealloc_count(), get_realloc_count())
        };
        assert_eq!(
            (allocs, deallocs, reallocs),
            (0, 0, 0),
            "Musical gate RT path must allocate zero heap memory"
        );
    }
}
