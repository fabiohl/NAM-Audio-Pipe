// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, GcSink, RT_STATUS_SPSC_DRAIN_TRUNCATED,
    RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED, RtStatusFlags, SwapBudget,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
fn validates_1x1_and_gear_tags_and_accepts_wavenet_head() {
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

    // A valid 1x1 WaveNet with a canonical multi-stage `config.head` is now
    // accepted in the cabinet slot: the engine 3.0.0 loader builds and validates
    // the `PostStackHead`.
    let head_fixture = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/models/wavenet_post_head_k13.nam"),
    )
    .expect("wavenet_post_head_k13 fixture must exist");
    let mut head_slot = build_cab_nam_slot(
        &head_fixture,
        CabAssetKind::NamModel,
        "cab_head.nam",
        48_000,
        &sys,
    )
    .expect("engine 3.0.0 must build a WaveNet 1x1 model with config.head in the cab slot");
    let mut cab_l = [0.2f32, -0.4, 0.8, 0.0, 0.1, -0.2, 0.3, 0.0];
    let mut cab_r = cab_l;
    let mut scratch_l = [0.0f32; 8];
    let mut scratch_r = [0.0f32; 8];
    head_slot.process_block(
        &mut cab_l,
        &mut cab_r,
        8,
        false,
        &mut scratch_l,
        &mut scratch_r,
    );
    assert!(
        cab_l.iter().chain(cab_r.iter()).all(|v| v.is_finite()),
        "a WaveNet head cabinet must produce finite output"
    );

    // A malformed post-stack head (out_channels: 2) is still rejected fail-closed
    // by the engine loader.
    let malformed_head = r#"{
        "version":"0.6.0","architecture":"WaveNet","sample_rate":48000,
        "config":{
            "layers":[{"input_size":1,"condition_size":1,"channels":1,"head_size":1,"kernel_size":1,"dilations":[1],"activation":"ReLU","gated":false,"head_bias":false}],
            "head_scale":1.0,
            "head":{"in_channels":1,"channels":1,"activation":"Tanh","out_channels":2,"kernel_sizes":[1]}
        },
        "weights":[1.0,0.0,1.0,1.0,0.0,1.0,1.0,0.0]
    }"#;
    let head_err = build_cab_nam_slot(
        malformed_head.as_bytes(),
        CabAssetKind::NamModel,
        "cab_head_invalid.nam",
        48_000,
        &sys,
    )
    .err()
    .expect("a malformed post-stack head must be rejected fail-closed by the engine");
    assert!(
        head_err.to_string().contains("Cabinet model load failed"),
        "got {head_err}"
    );

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
fn neural_cabinet_tail_budget_rearms_then_drains_to_zero() {
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
    assert!(slot.tail_budget >= 128, "budget={}", slot.tail_budget);
    assert_eq!(slot.remaining_tail_samples(), 0);

    slot.rearm_tail();
    assert_eq!(slot.remaining_tail_samples(), slot.tail_budget);

    let n = 64;
    let mut l = [0.0f32; 64];
    let mut r = [0.0f32; 64];
    let mut sl = [0.0f32; 64];
    let mut sr = [0.0f32; 64];
    let mut total_drained = 0usize;
    loop {
        let drained = slot.drain_tail(&mut l, &mut r, n, false, &mut sl, &mut sr);
        if drained == 0 {
            break;
        }
        total_drained += drained;
    }
    assert_eq!(total_drained, slot.tail_budget);
    assert_eq!(slot.remaining_tail_samples(), 0);
    assert!(l.iter().chain(r.iter()).all(|v| *v == 0.0));
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

    let mut budget = SwapBudget::new(1);
    let stats = drain_cab_nam_payloads(
        &mut cons,
        &mut budget,
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
    assert_eq!(budget.used(), 0);
    assert_eq!(stats.pops, 1);
    assert_eq!(stats.installed, 0);
    assert_eq!(stats.retired, 1);
    assert_eq!(stats.deferred, 0);
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

    let mut budget = SwapBudget::new(1);
    let stats = drain_cab_nam_payloads(
        &mut cons,
        &mut budget,
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
    assert_eq!(budget.used(), 1);
    assert_eq!(stats.pops, 1);
    assert_eq!(stats.installed, 1);
    assert_eq!(stats.retired, 0);
    assert_eq!(stats.deferred, 0);
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

#[test]
fn budget_exhausted_defers_valid_nam_via_peek_and_closes_accounting() {
    let sys = SystemSnapshot::capture();
    let json = linear_cab_json(48_000, 0.5, None);
    let make_slot = || {
        build_cab_nam_slot(
            json.as_bytes(),
            CabAssetKind::NamModel,
            "cab.nam",
            48_000,
            &sys,
        )
        .expect("linear cabinet builds")
    };

    let (mut prod, mut cons) = rtrb::RingBuffer::<PipeCabNamPayload>::new(8);
    // Queue 1 stale (gen 1), then 2 valid (gen 2, gen 3) with target_gen = 2.
    prod.push(PipeCabNamPayload::from_slot(1, make_slot()))
        .unwrap();
    prod.push(PipeCabNamPayload::from_slot(2, make_slot()))
        .unwrap();
    prod.push(PipeCabNamPayload::from_slot(3, make_slot()))
        .unwrap();

    let requested = AtomicU64::new(2);
    let applied = AtomicU64::new(0);
    let mut wav: Option<Box<CabSimPair>> = None;
    let mut nam: Option<CabNamSlot> = None;
    let mut failed: Option<(u32, usize)> = None;
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::<GcItem>::new(16);
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

    // Callback 1: shared budget already exhausted -> stale (gen 1) is popped & retired,
    // valid gen 2 & gen 3 remain untouched in the ring via peek()!
    let mut budget_1 = SwapBudget::new(1);
    budget_1.consume();
    let s1 = drain_cab_nam_payloads(
        &mut cons,
        &mut budget_1,
        &requested,
        &applied,
        &mut wav,
        &mut nam,
        &|_: u32| {},
        Some(&mut failed),
        &mut gc,
    );
    assert_eq!(s1.pops, 1, "only stale gen 1 popped");
    assert_eq!(s1.installed, 0);
    assert_eq!(s1.retired, 1);
    assert_eq!(s1.deferred, 1);
    assert_eq!(s1.pops, s1.installed + s1.retired);
    assert_eq!(cons.slots(), 2, "both valid payloads must remain queued");
    assert!(nam.is_none());
    assert!(flags.check_flag(RT_STATUS_STRUCTURAL_DEFERRED));
    assert_eq!(flags.structural_deferred_total.load(Ordering::Relaxed), 1);

    // Callback 2: fresh budget -> pops gen 2 & gen 3, coalesces gen 2 -> GC, installs gen 3.
    let mut budget_2 = SwapBudget::new(1);
    let s2 = drain_cab_nam_payloads(
        &mut cons,
        &mut budget_2,
        &requested,
        &applied,
        &mut wav,
        &mut nam,
        &|_: u32| {},
        Some(&mut failed),
        &mut gc,
    );
    assert_eq!(s2.pops, 2);
    assert_eq!(s2.installed, 1);
    assert_eq!(s2.retired, 1);
    assert_eq!(s2.deferred, 0);
    assert_eq!(s2.pops, s2.installed + s2.retired);
    assert_eq!(budget_2.used(), 1);
    assert_eq!(applied.load(Ordering::Acquire), 3);
    assert!(nam.is_some());
    assert!(cons.is_empty());
    assert!(flags.check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED));
    assert_eq!(flags.structural_superseded_total.load(Ordering::Relaxed), 1);

    // Total pushed (3) == total installed (1) + total coalesced/stale payloads retired (2).
    assert_eq!(s1.pops + s2.pops, 3);
    assert_eq!(s1.installed + s2.installed, 1);
    assert_eq!(s1.retired + s2.retired, 2);
    while gc_c.pop().is_ok() {}
}

#[cfg(feature = "testing")]
#[test]
fn simultaneous_resampler_wav_and_nam_enforce_single_structural_swap_per_callback() {
    use crate::standalone::pw_host::RtSwapHarness;
    use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
    use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;

    let sys = SystemSnapshot::capture();
    let json = linear_cab_json(48_000, 0.5, None);
    let nam_slot = build_cab_nam_slot(
        json.as_bytes(),
        CabAssetKind::NamModel,
        "cab.nam",
        48_000,
        &sys,
    )
    .expect("linear cabinet builds");

    let mut harness = RtSwapHarness::new(48_000, 48_000).expect("harness");
    let mut cab_nam_prod = harness.attach_cab_nam_channel();

    // Queue simultaneous resampler, WAV cabsim, and neural NAM cabsim swaps at the same generation!
    let cab_gen = harness.bump_cab_nam_generation();
    harness
        .request_resampler_swap(48_000, 48_000)
        .expect("queue resampler");
    let ir = [1.0f32, 0.5, 0.25];
    let make_adapter =
        || CabSimAdapter::new(Box::new(ConvEngine::new(&ir, 64).expect("conv"))).expect("adapter");
    harness.push_cabsim_with_gen(
        cab_gen,
        Some(Box::new(CabSimPair {
            l: Box::new(make_adapter()),
            r: Box::new(make_adapter()),
            sample_rate: 48_000,
        })),
    );
    cab_nam_prod
        .push(PipeCabNamPayload::from_slot(cab_gen, nam_slot))
        .expect("queue nam cab");

    let (_prod, mut rt) = harness.into_parts();
    let mut in_l = [0.2f32; 64];
    let mut in_r = [0.2f32; 64];

    // Callback 1: resampler swap applies (structural_applied == 1); WAV and NAM stay deferred.
    let acc1 = rt.run_callback_accounted(&mut in_l, &mut in_r, 64);
    assert_eq!(acc1.structural_applied, 1);
    assert!(rt.active_cabsim().is_none(), "WAV deferred on cb 1");
    assert!(rt.active_cab_nam().is_none(), "NAM deferred on cb 1");
    assert!(rt.commands_pending());

    // Callback 2: WAV cabsim applies (structural_applied == 1); NAM remains queued (not dropped!).
    let acc2 = rt.run_callback_accounted(&mut in_l, &mut in_r, 64);
    assert_eq!(acc2.structural_applied, 1);
    assert!(rt.active_cabsim().is_some(), "WAV applied on cb 2");
    assert!(rt.active_cab_nam().is_none(), "NAM deferred on cb 2");
    assert!(rt.commands_pending());

    // Callback 3: NAM cabsim applies (structural_applied == 1), retiring WAV to GC.
    let acc3 = rt.run_callback_accounted(&mut in_l, &mut in_r, 64);
    assert_eq!(acc3.structural_applied, 1);
    assert!(
        rt.active_cabsim().is_none(),
        "WAV retired by NAM exclusivity"
    );
    assert!(rt.active_cab_nam().is_some(), "NAM applied on cb 3");
    let acc4 = rt.run_callback_accounted(&mut in_l, &mut in_r, 64);
    assert_eq!(acc4.structural_applied, 0);
    rt.consume_gc();
    assert!(!rt.commands_pending());
}

#[test]
fn pop_cap_truncation_flags_and_preserves_remaining_payload() {
    let sys = SystemSnapshot::capture();
    let json = linear_cab_json(48_000, 0.5, None);
    let make_slot = || {
        build_cab_nam_slot(
            json.as_bytes(),
            CabAssetKind::NamModel,
            "cab.nam",
            48_000,
            &sys,
        )
        .expect("linear cabinet builds")
    };

    // Ring capacity 12 > STRUCTURAL_POPS_PER_CALLBACK (8): enqueue 10 stale payloads
    // followed by 1 valid payload (total 11).
    let (mut prod, mut cons) = rtrb::RingBuffer::<PipeCabNamPayload>::new(12);
    for g in 1..=10 {
        prod.push(PipeCabNamPayload::from_slot(g, make_slot()))
            .unwrap();
    }
    prod.push(PipeCabNamPayload::from_slot(11, make_slot()))
        .unwrap();

    let requested = AtomicU64::new(11);
    let applied = AtomicU64::new(0);
    let mut wav: Option<Box<CabSimPair>> = None;
    let mut nam: Option<CabNamSlot> = None;
    let mut failed: Option<(u32, usize)> = None;
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::<GcItem>::new(32);
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

    let mut b1 = SwapBudget::new(1);
    let s1 = drain_cab_nam_payloads(
        &mut cons,
        &mut b1,
        &requested,
        &applied,
        &mut wav,
        &mut nam,
        &|_: u32| {},
        Some(&mut failed),
        &mut gc,
    );
    assert_eq!(s1.pops, 8);
    assert_eq!(s1.retired, 8);
    assert_eq!(s1.installed, 0);
    assert_eq!(cons.slots(), 3);
    assert!(flags.check_flag(RT_STATUS_SPSC_DRAIN_TRUNCATED));

    let mut b2 = SwapBudget::new(1);
    let s2 = drain_cab_nam_payloads(
        &mut cons,
        &mut b2,
        &requested,
        &applied,
        &mut wav,
        &mut nam,
        &|_: u32| {},
        Some(&mut failed),
        &mut gc,
    );
    assert_eq!(s2.pops, 3);
    assert_eq!(s2.retired, 2);
    assert_eq!(s2.installed, 1);
    assert_eq!(b2.used(), 1);
    assert_eq!(applied.load(Ordering::Acquire), 11);
    assert!(nam.is_some());
    assert!(cons.is_empty());
    assert_eq!(s1.pops + s2.pops, 11);
    assert_eq!(s1.installed + s2.installed, 1);
    assert_eq!(s1.retired + s2.retired, 10);
    while gc_c.pop().is_ok() {}
}
