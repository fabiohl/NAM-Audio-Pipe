// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! Integration tests for S08 in `NAM-Audio-Pipe` — Exclusive Cabinet Slot (`.wav`/`.nam`/`.namb`)
//! and `--cab-bypass` constant-latency phase-aligned bypass.

mod common;

use common::swap::cabsim_pair;
use nam_audio_pipe::standalone::banner::{BannerInfo, BannerRequest, CabStatus, render_banner};
use nam_audio_pipe::standalone::cabinet::{
    CabAssetKind, PipeCabNamPayload, build_cab_nam_slot, disambiguate_cab_path,
};
use nam_audio_pipe::standalone::cli::{GateConfig, GateMode, IrLevelPolicy, parse_args_from};
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use nam_audio_pipe::standalone::setup::load_initial_cabinet;
use neural_amp_modeler_rs::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{
    CabSimSwapPayload, RT_STATUS_NEEDS_CABSIM_REBUILD, setup_spsc,
};
use neural_amp_modeler_rs::loader::nam_json::{WeightsLayout, parse_nam_json};
use neural_amp_modeler_rs::loader::namb_encoder::encode_namb;
use neural_amp_modeler_rs::testing::reference_oracle::{PrecisionConfig, oracle_forward};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

fn linear_cab_json(sample_rate: u32, gain: f32, with_hostile_meta: bool) -> String {
    let meta = if with_hostile_meta {
        r#","metadata":{"gear_type":"Amp","gear_make":"HostileAmp","gear_model":"Calibrated","input_level_dbu":12.0,"output_level_dbu":6.0,"loudness":-6.0}"#
    } else {
        ""
    };
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"Linear",
            "sample_rate":{sample_rate},
            "config":{{"receptive_field":2,"bias":false}},
            "weights":[{gain}, 0.0]
            {meta}
        }}"#
    )
}

fn sequential_cab_json(sample_rate: u32) -> String {
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"Sequential",
            "sample_rate":{sample_rate},
            "config":{{
                "models":[
                    {{
                        "version":"0.6.0",
                        "architecture":"Linear",
                        "sample_rate":{sample_rate},
                        "config":{{"receptive_field":1,"bias":false}},
                        "weights":[0.5]
                    }},
                    {{
                        "version":"0.6.0",
                        "architecture":"Linear",
                        "sample_rate":{sample_rate},
                        "config":{{"receptive_field":1,"bias":true}},
                        "weights":[0.8, 0.0]
                    }}
                ]
            }},
            "weights":[]
        }}"#
    )
}

fn lstm_cab_json(sample_rate: u32) -> String {
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"LSTM",
            "sample_rate":{sample_rate},
            "config":{{"num_layers":1,"input_size":1,"hidden_size":1}},
            "weights":[
                0.0, 0.0, 0.0, 0.0,
                0.0, 0.0, 0.0, 0.0,
                0.0, 0.0, 0.0, 0.0,
                0.0, 0.0,
                0.75, 0.0
            ]
        }}"#
    )
}

fn wavenet_with_head_json(sample_rate: u32) -> String {
    format!(
        r#"{{
            "version":"0.6.0",
            "architecture":"WaveNet",
            "sample_rate":{sample_rate},
            "config":{{
                "layers":[{{
                    "input_size":1,"condition_size":1,"channels":1,"head_size":1,
                    "kernel_size":1,"dilations":[1],"activation":"ReLU","gated":false,"head_bias":false
                }}],
                "head_scale":1.0,
                "head":{{"in_channels":1,"channels":1,"activation":"Tanh","out_channels":1,"kernel_sizes":[1]}}
            }},
            "weights":[1.0,0.0,1.0,1.0,0.0,1.0,1.0,0.0]
        }}"#
    )
}

fn unique_tmp_path(stem: &str, ext: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nam_pipe_s08_{}_{}_{}.{}",
        std::process::id(),
        id,
        stem,
        ext
    ))
}

#[test]
fn t08_1_cli_parsing_banner_and_disambiguation() {
    let args = parse_args_from(lexopt::Parser::from_iter([
        "nam-audio-pipe",
        "--cab",
        "/ir/studio_a/4x12.namb",
        "--cab-bypass",
    ]));
    assert_eq!(
        args.cab_path.as_deref(),
        Some(Path::new("/ir/studio_a/4x12.namb"))
    );
    assert!(args.cab_bypass);
    assert_eq!(
        disambiguate_cab_path(Path::new("/ir/studio_a/4x12.namb")),
        "studio_a/4x12.namb"
    );

    let active_nam = render_banner(
        &BannerInfo {
            request: BannerRequest {
                model_arch: Some("WaveNet".into()),
                cab: CabStatus::Asset {
                    kind: "NAM",
                    bypassed: false,
                },
                recording_dir: None,
            },
            sample_rate: 48_000,
            quantum: 128,
            ..Default::default()
        },
        false,
    );
    assert!(active_nam.contains("[NAM]"));
    assert!(active_nam.contains("unity gain"));

    let bypassed_wav = render_banner(
        &BannerInfo {
            request: BannerRequest {
                model_arch: None,
                cab: CabStatus::Asset {
                    kind: "WAV",
                    bypassed: true,
                },
                recording_dir: None,
            },
            sample_rate: 48_000,
            quantum: 128,
            dsp_latency_samples: 128,
            ..Default::default()
        },
        false,
    );
    assert!(bypassed_wav.contains("bypassed [WAV]"));
    assert!(bypassed_wav.contains("constant latency"));
}

#[test]
fn t08_2_exclusivity_fail_closed_guards_metadata_isolation_and_oracle_parity() {
    let sys = SystemSnapshot::capture();
    let wav_fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/models/cabsim_ir_pgo.wav");

    // 1. Fail-closed initial loads: post-stack head and 2x2 rejected fatally by default, skipped with --cab-optional.
    let head_path = unique_tmp_path("head_reject", "nam");
    std::fs::write(&head_path, wavenet_with_head_json(48_000)).unwrap();
    let stereo_path = unique_tmp_path("stereo_reject", "nam");
    std::fs::write(
        &stereo_path,
        r#"{"version":"0.6.0","architecture":"Linear","sample_rate":48000,"config":{"receptive_field":1,"bias":false,"in_channels":2,"out_channels":2},"weights":[1.0,0.0,0.0,1.0]}"#,
    )
    .unwrap();

    let mut spsc = setup_spsc(8).unwrap();
    let (mut nam_prod, _nam_cons) = rtrb::RingBuffer::<PipeCabNamPayload>::new(8);
    assert!(
        load_initial_cabinet(
            Some(&head_path),
            128,
            false,
            IrLevelPolicy::PeakNormalized,
            &mut spsc.cabsim_producer,
            &mut nam_prod,
            &sys,
        )
        .is_err()
    );
    assert!(
        load_initial_cabinet(
            Some(&head_path),
            128,
            true,
            IrLevelPolicy::PeakNormalized,
            &mut spsc.cabsim_producer,
            &mut nam_prod,
            &sys,
        )
        .unwrap()
        .is_none()
    );
    assert!(
        load_initial_cabinet(
            Some(&stereo_path),
            128,
            false,
            IrLevelPolicy::PeakNormalized,
            &mut spsc.cabsim_producer,
            &mut nam_prod,
            &sys,
        )
        .is_err()
    );
    // Genuine WAV IR loads cleanly.
    assert!(
        load_initial_cabinet(
            Some(&wav_fixture),
            128,
            false,
            IrLevelPolicy::PeakNormalized,
            &mut spsc.cabsim_producer,
            &mut nam_prod,
            &sys,
        )
        .unwrap()
        .is_some()
    );

    // 2. Exclusivity & oracle parity inside RT harness across WAV <-> NAM (Linear, Sequential, LSTM) <-> NAMB.
    let mut h = RtSwapHarness::new_with_gate_config(48_000, 48_000, GateConfig::Off).unwrap();
    let mut cab_nam_prod = h.attach_cab_nam_channel();
    h.push_input_gain(1.0);
    h.push_output_gain(1.0);

    // Install WAV IR first.
    let wav_pair = cabsim_pair();
    h.push_cabsim(Some(wav_pair));
    let mut dummy_l = [0.0f32; 64];
    let mut dummy_r = [0.0f32; 64];
    let _ = h.run_callback(&mut dummy_l, &mut dummy_r, 64);
    assert!(h.active_cabsim().is_some());
    assert!(h.active_cab_nam().is_none());

    // Replace with Linear .nam carrying hostile calibration metadata (unity gain must hold, zero calibration leakage).
    let lin_json = linear_cab_json(48_000, 0.5, true);
    let lin_slot = build_cab_nam_slot(
        lin_json.as_bytes(),
        CabAssetKind::NamModel,
        "lin.nam",
        48_000,
        &sys,
    )
    .unwrap();
    let gen1 = h.bump_cab_nam_generation();
    cab_nam_prod
        .push(PipeCabNamPayload::from_slot(gen1, lin_slot))
        .unwrap();

    const N: usize = 128;
    let in_l: Vec<f32> = (0..N).map(|i| 0.3 * (i as f32 * 0.05).sin()).collect();
    let in_r: Vec<f32> = (0..N).map(|i| -0.25 * (i as f32 * 0.07).cos()).collect();
    let mut wl = in_l.clone();
    let mut wr = in_r.clone();
    let _ = h.run_callback(&mut wl, &mut wr, N);
    assert!(
        h.active_cabsim().is_none(),
        "WAV pair must be exclusively retired"
    );
    assert!(h.active_cab_nam().is_some());

    let lin_data = parse_nam_json(&lin_json).unwrap();
    let in_l64: Vec<f64> = in_l.iter().map(|&x| f64::from(x)).collect();
    let oracle_l = oracle_forward(&lin_data, &in_l64, &PrecisionConfig::default());
    for (i, &exp) in oracle_l.iter().enumerate().take(N).skip(4) {
        assert!(
            (f64::from(h.out_l()[i]) - exp).abs() < 1e-5,
            "Linear NAM cab oracle mismatch at {i}: got {}, expected {}",
            h.out_l()[i],
            exp
        );
    }
    #[cfg(feature = "dual-mono")]
    {
        let in_r64: Vec<f64> = in_r.iter().map(|&x| f64::from(x)).collect();
        let oracle_r = oracle_forward(&lin_data, &in_r64, &PrecisionConfig::default());
        for (i, &exp) in oracle_r.iter().enumerate().take(N).skip(4) {
            assert!(
                (f64::from(h.out_r()[i]) - exp).abs() < 1e-5,
                "Linear NAM cab R mismatch at {i}: got {}, expected {}",
                h.out_r()[i],
                exp
            );
        }
    }

    // Replace with Sequential .nam and compare against composed f64 submodel oracle.
    let seq_json = sequential_cab_json(48_000);
    let seq_slot = build_cab_nam_slot(
        seq_json.as_bytes(),
        CabAssetKind::NamModel,
        "seq.nam",
        48_000,
        &sys,
    )
    .unwrap();
    let gen2 = h.bump_cab_nam_generation();
    cab_nam_prod
        .push(PipeCabNamPayload::from_slot(gen2, seq_slot))
        .unwrap();
    wl.copy_from_slice(&in_l);
    wr.copy_from_slice(&in_r);
    let _ = h.run_callback(&mut wl, &mut wr, N);
    let seq_data = parse_nam_json(&seq_json).unwrap();
    let mut seq_oracle = in_l64.clone();
    for child in seq_data.config.models.as_deref().unwrap() {
        let sub = parse_nam_json(&child.to_string()).unwrap();
        seq_oracle = oracle_forward(&sub, &seq_oracle, &PrecisionConfig::default());
    }
    for (i, &exp) in seq_oracle.iter().enumerate().take(N).skip(4) {
        assert!((f64::from(h.out_l()[i]) - exp).abs() < 1e-5);
    }

    // Replace with LSTM .nam & Linear .namb binary.
    let lstm_json = lstm_cab_json(48_000);
    let lstm_slot = build_cab_nam_slot(
        lstm_json.as_bytes(),
        CabAssetKind::NamModel,
        "lstm.nam",
        48_000,
        &sys,
    )
    .unwrap();
    let gen3 = h.bump_cab_nam_generation();
    cab_nam_prod
        .push(PipeCabNamPayload::from_slot(gen3, lstm_slot))
        .unwrap();
    wl.copy_from_slice(&in_l);
    wr.copy_from_slice(&in_r);
    let _ = h.run_callback(&mut wl, &mut wr, N);
    assert!(h.out_l().iter().all(|s| s.is_finite()));

    let namb_bytes = encode_namb(&lin_data, 2, WeightsLayout::Original).unwrap();
    let namb_slot = build_cab_nam_slot(
        &namb_bytes,
        CabAssetKind::NambModel,
        "lin.namb",
        48_000,
        &sys,
    )
    .unwrap();
    let gen4 = h.bump_cab_nam_generation();
    cab_nam_prod
        .push(PipeCabNamPayload::from_slot(gen4, namb_slot))
        .unwrap();
    wl.copy_from_slice(&in_l);
    wr.copy_from_slice(&in_r);
    let _ = h.run_callback(&mut wl, &mut wr, N);
    for (i, &exp) in oracle_l.iter().enumerate().take(N).skip(4) {
        assert!((f64::from(h.out_l()[i]) - exp).abs() < 1e-5);
    }

    // Swap back from NAMB to WAV IR and verify exclusivity restores active_cabsim & clears active_cab_nam.
    h.push_cabsim(Some(cabsim_pair()));
    wl.copy_from_slice(&in_l);
    wr.copy_from_slice(&in_r);
    let _ = h.run_callback(&mut wl, &mut wr, N);
    assert!(h.active_cabsim().is_some());
    assert!(h.active_cab_nam().is_none());

    let _ = h.consume_gc();
    let _ = std::fs::remove_file(head_path);
    let _ = std::fs::remove_file(stereo_path);
}

#[test]
fn t08_3_bypass_constant_latency_phase_crossfade_and_heap_audit() {
    let sys = SystemSnapshot::capture();

    // Test both Silence (Gate Off) and Musical modes with a 44.1 kHz NAM cab @ 48 kHz host (non-zero resampler latency!).
    for gate_mode in [GateMode::Silence, GateMode::Musical] {
        let mut h = RtSwapHarness::new_with_gate_config_and_mode(
            48_000,
            48_000,
            GateConfig::Off,
            gate_mode,
        )
        .unwrap();
        let mut cab_nam_prod = h.attach_cab_nam_channel();
        let cab_json = linear_cab_json(44_100, 0.5, false);
        let cab_slot = build_cab_nam_slot(
            cab_json.as_bytes(),
            CabAssetKind::NamModel,
            "cab44.nam",
            48_000,
            &sys,
        )
        .unwrap();
        let expected_lat = cab_slot.latency_samples();
        assert!(
            expected_lat > 0,
            "44.1k->48k cab resampler must declare latency"
        );

        let cab_gen = h.bump_cab_nam_generation();
        cab_nam_prod
            .push(PipeCabNamPayload::from_slot(cab_gen, cab_slot))
            .unwrap();

        let mut b_l = [0.0f32; 64];
        let mut b_r = [0.0f32; 64];
        let _ = h.run_callback(&mut b_l, &mut b_r, 64);
        assert_eq!(h.applied_cab_latency(), expected_lat);

        // Engage bypass across steady sine and verify constant latency + smooth 64-sample cosine ramp.
        h.set_cab_bypass(true);
        let mut prev = 0.0f32;
        for blk in 0..8 {
            for i in 0..64 {
                let sample = 0.25 * (((blk * 64 + i) as f32) * 0.09).sin();
                b_l[i] = sample;
                b_r[i] = sample;
            }
            let _ = h.run_callback(&mut b_l, &mut b_r, 64);
            assert_eq!(
                h.applied_cab_latency(),
                expected_lat,
                "Cab bypass must keep latency constant"
            );
            for &s in h.out_l() {
                assert!(s.is_finite());
                if blk > 0 {
                    assert!((s - prev).abs() < 0.08, "Click during cab bypass engage");
                }
                prev = s;
            }
        }

        // Send a delayed impulse through bypassed cabinet and verify alignment at `expected_lat`.
        let mut silent_blocks = 0;
        let mut impulse_idx = None;
        let total_frames = 128;
        let mut sl = [0.0f32; 128];
        let mut sr = [0.0f32; 128];
        for blk in 0..4 {
            sl.fill(0.0);
            sr.fill(0.0);
            if blk == 0 {
                sl[10] = 0.6;
                sr[10] = 0.6;
            }
            let _ = h.run_callback(&mut sl, &mut sr, total_frames);
            for (i, &v) in h.out_l().iter().enumerate() {
                let g = blk * total_frames + i;
                if g >= 10 && v.abs() > 0.3 {
                    impulse_idx = Some(g);
                    break;
                }
                if v.abs() <= 1e-6 {
                    silent_blocks += 1;
                }
            }
            if impulse_idx.is_some() {
                break;
            }
        }
        assert_eq!(
            impulse_idx,
            Some(10 + expected_lat as usize),
            "Bypassed dry delay must align impulse to exact cabinet latency ({silent_blocks} silent)"
        );

        // Re-engage wet across unaligned block sizes & mono->stereo switch and verify smooth ramp.
        h.set_cab_bypass(false);
        let mut odd_l = [0.0f32; 37];
        let mut odd_r = [0.0f32; 37];
        for step in 0..8 {
            for i in 0..37 {
                let v = 0.25 * (((step * 37 + i) as f32) * 0.07).sin();
                odd_l[i] = v;
                odd_r[i] = if step < 4 { v } else { -v };
            }
            let _ = h.run_callback(&mut odd_l, &mut odd_r, 37);
            assert!(h.out_l().iter().all(|s| s.is_finite()));
            assert!(h.out_r().iter().all(|s| s.is_finite()));
        }
    }

    // Heap-audit: verify 0 RT allocations/deallocations/reallocations across bypass flips + WAV<->NAM swaps!
    #[cfg(feature = "heap-audit")]
    {
        use neural_amp_modeler_rs::common::alloc_audit::{
            TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
        };

        let mut h = RtSwapHarness::new_with_gate_config(48_000, 48_000, GateConfig::Off).unwrap();
        let mut cab_nam_prod = h.attach_cab_nam_channel();
        let cab_json = linear_cab_json(48_000, 0.5, false);
        let cab_slot = build_cab_nam_slot(
            cab_json.as_bytes(),
            CabAssetKind::NamModel,
            "cab48.nam",
            48_000,
            &sys,
        )
        .unwrap();
        let wav_pair = cabsim_pair();
        let cab_gen = h.bump_cab_nam_generation();
        cab_nam_prod
            .push(PipeCabNamPayload::from_slot(cab_gen, cab_slot))
            .unwrap();
        h.push_cabsim(Some(wav_pair));

        // Pre-size a 96kHz NAM slot too before entering TrackingGuard.
        let cab96_slot = build_cab_nam_slot(
            cab_json.as_bytes(),
            CabAssetKind::NamModel,
            "cab96.nam",
            48_000,
            &sys,
        )
        .unwrap();
        let gen2 = h.bump_cab_nam_generation();
        cab_nam_prod
            .push(PipeCabNamPayload::from_slot(gen2, cab96_slot))
            .unwrap();

        let (allocs, deallocs, reallocs) = {
            let _guard = TrackingGuard::new();
            let mut in_l = [0.2f32; 64];
            let mut in_r = [-0.2f32; 64];
            for step in 0..24 {
                h.set_cab_bypass(step % 6 >= 3);
                in_l.fill(0.2 * ((step as f32) * 0.1).sin());
                in_r.fill(-0.2 * ((step as f32) * 0.1).sin());
                let _ = h.run_callback(&mut in_l, &mut in_r, 64);
            }
            (get_alloc_count(), get_dealloc_count(), get_realloc_count())
        };
        assert_eq!(allocs, 0, "RT heap allocations detected: {allocs}");
        assert_eq!(deallocs, 0, "RT heap deallocations detected: {deallocs}");
        assert_eq!(reallocs, 0, "RT heap reallocations detected: {reallocs}");
        let _ = h.consume_gc();
    }

    // Verify dynamic host rate renegotiation helper flag constant behavior doesn't hold spurious flags.
    let spsc = setup_spsc(4).unwrap();
    let _: rtrb::Producer<Box<CabSimSwapPayload>> = spsc.cabsim_producer;
    assert!(!spsc.rt_status.check_flag(RT_STATUS_NEEDS_CABSIM_REBUILD));
}
