// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::GcOverflowBuffer;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};

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

#[test]
fn active_cabinet_latency_follows_the_single_installed_source() {
    let sys = SystemSnapshot::capture();
    let json = linear_cab_json(48_000, 0.5, None);
    let slot = build_cab_nam_slot(
        json.as_bytes(),
        CabAssetKind::NamModel,
        "cab.nam",
        44_100,
        &sys,
    )
    .expect("linear cabinet builds at a mismatched host rate");
    assert_eq!(active_cabinet_latency(None, None), 0);
    assert_eq!(
        active_cabinet_latency(None, Some(&slot)),
        slot.latency_samples()
    );
}

#[test]
fn failed_neural_rebuild_latches_key_and_keeps_last_safe_state() {
    let (mut prod, mut cons) = rtrb::RingBuffer::<PipeCabNamPayload>::new(4);
    prod.push(PipeCabNamPayload::failed(
        7,
        CabAssetKind::NamModel,
        44_100,
        64,
    ))
    .unwrap();
    let requested = AtomicU64::new(7);
    let applied = AtomicU64::new(0);
    let mut wav: Option<Box<CabSimPair>> = None;
    let mut nam: Option<CabNamSlot> = None;
    let mut failed: Option<(u32, usize)> = None;
    let (mut gc_p, _gc_c) = rtrb::RingBuffer::<GcItem>::new(8);
    let mut parking_lot: [Option<GcItem>; 16] = [const { None }; 16];
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut gc = GcSink {
        producer: &mut gc_p,
        parking_lot: &mut parking_lot,
        overflow: &gc_overflow,
        rt_status: &flags,
        parking_lot_dirty: Some(&parking_lot_dirty),
    };

    drain_cab_nam_payloads(
        &mut cons,
        &requested,
        &applied,
        &mut wav,
        &mut nam,
        &|_: u32| {},
        Some(&mut failed),
        &mut gc,
    );

    assert_eq!(failed, Some((44_100, 64)));
    assert!(nam.is_none());
    assert!(wav.is_none());
    assert_eq!(applied.load(Ordering::Acquire), 7);
}

#[test]
fn successful_neural_install_clears_failed_latch() {
    let sys = SystemSnapshot::capture();
    let json = linear_cab_json(48_000, 0.5, None);
    let slot = build_cab_nam_slot(
        json.as_bytes(),
        CabAssetKind::NamModel,
        "cab.nam",
        48_000,
        &sys,
    )
    .expect("linear cabinet builds");
    let (mut prod, mut cons) = rtrb::RingBuffer::<PipeCabNamPayload>::new(4);
    prod.push(PipeCabNamPayload::from_slot(8, slot)).unwrap();
    let requested = AtomicU64::new(8);
    let applied = AtomicU64::new(0);
    let mut wav: Option<Box<CabSimPair>> = None;
    let mut nam: Option<CabNamSlot> = None;
    let mut failed: Option<(u32, usize)> = Some((48_000, 64));
    let (mut gc_p, _gc_c) = rtrb::RingBuffer::<GcItem>::new(8);
    let mut parking_lot: [Option<GcItem>; 16] = [const { None }; 16];
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut gc = GcSink {
        producer: &mut gc_p,
        parking_lot: &mut parking_lot,
        overflow: &gc_overflow,
        rt_status: &flags,
        parking_lot_dirty: Some(&parking_lot_dirty),
    };

    drain_cab_nam_payloads(
        &mut cons,
        &requested,
        &applied,
        &mut wav,
        &mut nam,
        &|_: u32| {},
        Some(&mut failed),
        &mut gc,
    );

    assert_eq!(failed, None);
    assert!(nam.is_some());
    assert_eq!(applied.load(Ordering::Acquire), 8);
}

#[test]
fn neural_reengage_drains_prewarm_then_crossfades_to_wet() {
    let sys = SystemSnapshot::capture();
    let json = linear_cab_json(48_000, 0.5, None);
    let mut slot = build_cab_nam_slot(
        json.as_bytes(),
        CabAssetKind::NamModel,
        "cab.nam",
        48_000,
        &sys,
    )
    .expect("linear cabinet builds");
    let mut bypass = CabBypassState::new(true);
    let mut delay = CabDryDelay::new(0);
    let flags = RtStatusFlags::new();
    let n = 8;
    let mut dry_l = vec![0.0f32; n];
    let mut dry_r = vec![0.0f32; n];
    let mut wet_l = vec![1.0f32; n];
    let mut wet_r = vec![1.0f32; n];
    let mut scratch_l = vec![0.0f32; n];
    let mut scratch_r = vec![0.0f32; n];
    let mut last = 0.0f32;
    for _ in 0..32 {
        wet_l[..n].fill(1.0);
        wet_r[..n].fill(1.0);
        let active = bypass.process_stage(
            false,
            None,
            Some(&mut slot),
            &mut delay,
            &mut dry_l,
            &mut dry_r,
            &mut wet_l,
            &mut wet_r,
            &mut scratch_l,
            &mut scratch_r,
            n,
            false,
            &flags,
        );
        assert!(active);
        last = wet_l[n - 1];
    }
    assert!((last - 0.5).abs() < 1e-5, "settled at {last}");
}
