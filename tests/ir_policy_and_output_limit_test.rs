// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration tests for S03:
//! - T03.2: Cabinet IR level policy (`peak-normalized`, `preserved`, `legacy-nam-plugin`)
//! - T03.3: Optional hardware playback output hard-clamp (`--output-limit off|clamp`)
//!   preserving float WAV recording untouched.

#![cfg(feature = "testing")]

use nam_audio_pipe::standalone::cli::{
    IrLevelPolicy, OutputLimitPolicy, parse_ir_level_policy, parse_output_limit,
};
use nam_audio_pipe::standalone::pw_host::output_pw::deliver_playback_pair_fail_closed_with_limit;
use nam_audio_pipe::standalone::setup::{load_initial_cabsim, setup_communication_channels};
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::testing::wav::write_wav_f32;
use pipewire as pw;
use std::path::PathBuf;

fn temp_wav_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nam_pipe_s03_{tag}_{}.wav", std::process::id()))
}

#[test]
fn cli_parsers_accept_valid_modes_and_reject_invalid() {
    assert_eq!(
        parse_ir_level_policy("peak-normalized").unwrap(),
        IrLevelPolicy::PeakNormalized
    );
    assert_eq!(
        parse_ir_level_policy("preserved").unwrap(),
        IrLevelPolicy::Preserved
    );
    assert_eq!(
        parse_ir_level_policy("legacy-nam-plugin").unwrap(),
        IrLevelPolicy::LegacyNamPlugin
    );
    assert!(parse_ir_level_policy("unknown").is_err());

    assert_eq!(parse_output_limit("off").unwrap(), OutputLimitPolicy::Off);
    assert_eq!(
        parse_output_limit("clamp").unwrap(),
        OutputLimitPolicy::Clamp
    );
    assert!(parse_output_limit("limiter").is_err());
    assert_eq!(OutputLimitPolicy::default(), OutputLimitPolicy::Off);
    assert_eq!(IrLevelPolicy::default(), IrLevelPolicy::PeakNormalized);
}

#[test]
fn load_initial_cabsim_preserves_raw_samples_and_applies_policy_to_convolution() {
    let rates = [44_100u32, 48_000, 96_000];
    let peaks = [0.25f32, 0.5, 1.0, 2.0];

    for &fs in &rates {
        for &peak in &peaks {
            let wav_path = temp_wav_path(&format!("ir_{fs}_{}", (peak * 100.0) as u32));
            let mut ir = vec![0.0f32; 128];
            ir[0] = peak;
            write_wav_f32(&wav_path, &ir, fs).expect("write wav");

            for &(policy, expected_gain) in &[
                (IrLevelPolicy::PeakNormalized, 1.0_f64),
                (IrLevelPolicy::Preserved, peak as f64),
                (
                    IrLevelPolicy::LegacyNamPlugin,
                    (peak as f64) * 10.0_f64.powf(-18.0 / 20.0) * (48_000.0 / fs as f64),
                ),
            ] {
                let mut channels = setup_communication_channels().expect("spsc");
                let initial = load_initial_cabsim(
                    Some(&wav_path),
                    128,
                    false,
                    policy,
                    &mut channels.cabsim_producer,
                )
                .expect("load")
                .expect("some ir");

                // Raw samples must ALWAYS remain unnormalized (original amplitude preserved).
                assert!(
                    (initial.raw_samples[0] - peak).abs() < 1e-6,
                    "raw_samples must not be destructively normalized (expected {peak}, got {})",
                    initial.raw_samples[0]
                );
                assert_eq!(initial.source_rate, fs);

                let payload = channels.cabsim_consumer.pop().expect("pair payload");
                let mut pair = payload.pair.expect("pair");
                let lat = pair.l.latency_samples();
                let mut chunk1 = vec![0.0f32; lat];
                chunk1[0] = 1.0;
                pair.l.process_in_place(&mut chunk1, None);
                let mut chunk2 = vec![0.0f32; lat];
                pair.l.process_in_place(&mut chunk2, None);
                let observed = chunk2[0] as f64;
                assert!(
                    (observed - expected_gain).abs() < 1e-5,
                    "fs={fs} peak={peak} policy={policy:?} lat={lat}: expected {expected_gain}, got {observed}"
                );
            }
            let _ = std::fs::remove_file(&wav_path);
        }
    }
}

#[test]
fn load_initial_cabsim_handles_silent_wav_without_nan() {
    let wav_path = temp_wav_path("silent");
    let silent = vec![0.0f32; 128];
    write_wav_f32(&wav_path, &silent, 48_000).expect("write silent wav");

    for policy in [
        IrLevelPolicy::PeakNormalized,
        IrLevelPolicy::Preserved,
        IrLevelPolicy::LegacyNamPlugin,
    ] {
        let mut channels = setup_communication_channels().expect("spsc");
        let initial = load_initial_cabsim(
            Some(&wav_path),
            128,
            false,
            policy,
            &mut channels.cabsim_producer,
        )
        .expect("load")
        .expect("some");
        assert!(initial.raw_samples.iter().all(|&s| s == 0.0));
        let payload = channels.cabsim_consumer.pop().expect("pair");
        let mut pair = payload.pair.expect("pair");
        let mut block = vec![1.0f32; 256];
        pair.l.process_in_place(&mut block, None);
        assert!(block.iter().all(|&s| s == 0.0));
    }

    let _ = std::fs::remove_file(&wav_path);
}

#[test]
fn playback_output_limit_off_vs_clamp_handles_excursions_channels_and_non_finite() {
    let rt_status = RtStatusFlags::new();
    let src_l = [
        0.5f32,
        1.5,
        -1.8,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        -1.0,
        1.0,
    ];
    let src_r = [
        -0.25f32,
        2.25,
        -3.5,
        0.0,
        f32::NAN,
        f32::INFINITY,
        0.99,
        -0.99,
    ];
    let n_out = src_l.len();
    let n_bytes = n_out * std::mem::size_of::<f32>();

    let mut dst_l = vec![0.0f32; n_out];
    let mut dst_r = vec![0.0f32; n_out];
    let mut chunk_l = pw::spa::sys::spa_chunk {
        offset: 0,
        size: 0,
        stride: 0,
        flags: 0,
    };
    let mut chunk_r = pw::spa::sys::spa_chunk {
        offset: 0,
        size: 0,
        stride: 0,
        flags: 0,
    };

    // 1. Default `Off`: delivers exact raw f32 samples (excursions > ±1.0 preserved).
    // SAFETY: `dst_l`, `dst_r`, `src_l`, and `src_r` are valid, aligned, non-overlapping buffers of `n_out` f32s (`n_bytes` bytes).
    let res_off = unsafe {
        deliver_playback_pair_fail_closed_with_limit(
            dst_l.as_mut_ptr() as usize,
            n_bytes,
            &mut chunk_l,
            dst_r.as_mut_ptr() as usize,
            n_bytes,
            &mut chunk_r,
            src_l.as_ptr(),
            src_r.as_ptr(),
            n_bytes,
            &rt_status,
            OutputLimitPolicy::Off,
        )
    };
    assert_eq!(res_off, Some(n_out));
    assert_eq!(dst_l[1], 1.5);
    assert_eq!(dst_l[2], -1.8);
    assert_eq!(dst_r[1], 2.25);
    assert_eq!(dst_r[2], -3.5);

    // 2. `Clamp`: hard-clamps L and R independently to [-1.0, 1.0], maps NaN -> 0.0, ±Inf -> ±1.0.
    // SAFETY: `dst_l`, `dst_r`, `src_l`, and `src_r` are valid, aligned, non-overlapping buffers of `n_out` f32s (`n_bytes` bytes).
    let res_clamp = unsafe {
        deliver_playback_pair_fail_closed_with_limit(
            dst_l.as_mut_ptr() as usize,
            n_bytes,
            &mut chunk_l,
            dst_r.as_mut_ptr() as usize,
            n_bytes,
            &mut chunk_r,
            src_l.as_ptr(),
            src_r.as_ptr(),
            n_bytes,
            &rt_status,
            OutputLimitPolicy::Clamp,
        )
    };
    assert_eq!(res_clamp, Some(n_out));
    assert_eq!(dst_l, [0.5, 1.0, -1.0, 1.0, -1.0, 0.0, -1.0, 1.0]);
    assert_eq!(dst_r, [-0.25, 1.0, -1.0, 0.0, 0.0, 1.0, 0.99, -0.99]);
}
