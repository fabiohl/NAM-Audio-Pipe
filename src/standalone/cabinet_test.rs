// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::SystemSnapshot;
use std::path::Path;

fn linear_cab_json(sample_rate: u32, gain: f32, gear_type: Option<&str>) -> String {
    let meta = match gear_type {
        Some(gt) => format!(
            r#","metadata":{{"gear_type":"{gt}","gear_make":"TestMake","gear_model":"TestCab","input_level_dbu":12.0,"output_level_dbu":6.0,"loudness":-6.0}}"#
        ),
        None => String::new(),
    };
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"Linear",
            "sample_rate":{sample_rate},
            "config":{{"receptive_field":1,"bias":false}},
            "weights":[{gain}]
            {meta}
        }}"#
    )
}

#[test]
fn disambiguates_parent_directory_and_detects_kind() {
    assert_eq!(
        disambiguate_cab_path(Path::new("/cabs/studio_a/4x12.wav")),
        "studio_a/4x12.wav"
    );
    assert_eq!(disambiguate_cab_path(Path::new("4x12.nam")), "4x12.nam");
    assert_eq!(
        CabAssetKind::detect(Path::new("x.wav"), b"").unwrap(),
        CabAssetKind::WavIr
    );
    assert_eq!(
        CabAssetKind::detect(Path::new("x.nam"), b"{}").unwrap(),
        CabAssetKind::NamModel
    );
    assert_eq!(
        CabAssetKind::detect(Path::new("x.namb"), b"NAMB...").unwrap(),
        CabAssetKind::NambModel
    );
    assert!(CabAssetKind::detect(Path::new("x.txt"), b"hello").is_err());
}

#[test]
fn validates_1x1_and_gear_tags_without_calibration_or_head() {
    let sys = SystemSnapshot::capture();
    for gt in ["Amp", "Pedal", "IR", "FullRig"] {
        let json = linear_cab_json(48_000, 0.5, Some(gt));
        let mut slot = build_cab_nam_slot(
            json.as_bytes(),
            CabAssetKind::NamModel,
            "cab.nam",
            48_000,
            &sys,
        )
        .expect("any 1x1 gear_type accepted");
        assert_eq!(slot.latency_samples(), 0);
        let mut l = [1.0f32; 8];
        let mut r = [-1.0f32; 8];
        let mut sl = [0.0f32; 8];
        let mut sr = [0.0f32; 8];
        slot.process_block(&mut l, &mut r, 8, false, &mut sl, &mut sr);
        // Output must reflect pure weight 0.5 * input (no amp calibration or loudness offset applied).
        let expected_r = if cfg!(feature = "dual-mono") {
            -0.5
        } else {
            0.5
        };
        for (vl, vr) in l.iter().zip(r.iter()) {
            assert!((vl - 0.5).abs() < 1e-6);
            assert!((vr - expected_r).abs() < 1e-6);
        }
    }

    let wavenet_head = r#"{
        "version":"0.6.0","architecture":"WaveNet","sample_rate":48000,
        "config":{
            "layers":[{"input_size":1,"condition_size":1,"channels":1,"head_size":1,"kernel_size":1,"dilations":[1],"activation":"ReLU","gated":false,"head_bias":false}],
            "head_scale":1.0,
            "head":{"in_channels":1,"channels":1,"activation":"Tanh","out_channels":1,"kernel_sizes":[1]}
        },
        "weights":[1.0,0.0,1.0,1.0,0.0,1.0,1.0,0.0]
    }"#;
    let err = parse_and_validate_cab_nam(wavenet_head.as_bytes(), CabAssetKind::NamModel)
        .unwrap_err()
        .to_string();
    assert!(err.contains("post-stack 'head'"), "got {err}");

    let stereo_lin = r#"{
        "version":"0.6.0","architecture":"Linear","sample_rate":48000,
        "config":{"receptive_field":1,"bias":false,"in_channels":2,"out_channels":2},
        "weights":[1.0,0.0,0.0,1.0]
    }"#;
    let ch_err = parse_and_validate_cab_nam(stereo_lin.as_bytes(), CabAssetKind::NamModel)
        .unwrap_err()
        .to_string();
    assert!(ch_err.contains("1-in/1-out"), "got {ch_err}");
}
