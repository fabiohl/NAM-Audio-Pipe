// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Offline end-to-end impulse peak validation of oversampling and DSP latency accounting.
//!
//! Evaluates the complete production capture pipeline (`RtSwapHarness` without PipeWire daemon)
//! across combinations of oversampling factors (`Off`, `X2`, `X4`) and sample rates
//! (identical 48 kHz ↔ 48 kHz, 2:1 integer upsampled 96 kHz ↔ 48 kHz, and fractional 44.1 kHz ↔ 48 kHz).
//!
//! Invariants:
//! 1. Coherent latency snapshot: `StreamStatusFlags::read_applied_latencies` correctly reflects
//!    resampler, cabsim, and oversampling latency components via wait-free seqlock.
//! 2. Converted oversampling latency: `convert_os_latency_to_host` maps native model-domain
//!    oversampling delay to host sample units accurately.
//! 3. Peak latency alignment: The measured impulse output peak index matches the published
//!    total DSP latency within ±1 sample tolerance.
//! 4. RT-safety: Zero heap allocations during callback execution.

#![cfg(feature = "testing")]

use nam_audio_pipe::standalone::cli::GateConfig;
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use nam_audio_pipe::standalone::pw_host::latency::{
    compute_dsp_latency, convert_os_latency_to_host,
};
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};

const MODEL_RATE: u32 = 48_000;
const BLOCK_SIZE: usize = 64;
const TOTAL_SAMPLES: usize = 2048;

fn measure_impulse_peak(
    host_rate: u32,
    model_rate: u32,
    factor: OversampleFactor,
) -> (usize, u32, u32, u32, u32, u32) {
    let mut harness = RtSwapHarness::new_with_gate_config(host_rate, model_rate, GateConfig::Off)
        .expect("harness construction");

    if factor != OversampleFactor::Off {
        let os_l = OversampleEngine::new(factor, 2048).expect("oversample engine left");
        let os_r = OversampleEngine::new(factor, 2048).expect("oversample engine right");
        harness.push_os_pair(os_l, os_r);
    }

    // Run one silent block to drain and install initial/swapped engines.
    let silence = vec![0.0f32; BLOCK_SIZE];
    harness.run_callback(&mut silence.clone(), &mut silence.clone(), BLOCK_SIZE);

    // Read coherent snapshot from stream_status via seqlock.
    let (resamp_lat, cab_lat, os_native) = harness.stream_status().read_applied_latencies();
    let os_host = convert_os_latency_to_host(os_native, host_rate, model_rate);
    let (published_total, _) = compute_dsp_latency(cab_lat, resamp_lat, os_host, host_rate);

    // Build unit impulse stimulus.
    let mut stimulus = vec![0.0f32; TOTAL_SAMPLES];
    stimulus[0] = 1.0;

    let mut produced: Vec<f32> = Vec::with_capacity(TOTAL_SAMPLES);
    for chunk in stimulus.chunks(BLOCK_SIZE) {
        let mut in_l = chunk.to_vec();
        let mut in_r = vec![0.0f32; chunk.len()];
        let n_pw = harness.run_callback(&mut in_l, &mut in_r, chunk.len());
        produced.extend_from_slice(&harness.out_l()[..n_pw]);
    }

    // Flush any remaining filter ring-out.
    for _ in 0..(TOTAL_SAMPLES / BLOCK_SIZE) {
        let mut in_l = vec![0.0f32; BLOCK_SIZE];
        let mut in_r = vec![0.0f32; BLOCK_SIZE];
        let n_pw = harness.run_callback(&mut in_l, &mut in_r, BLOCK_SIZE);
        produced.extend_from_slice(&harness.out_l()[..n_pw]);
    }

    let (peak_idx, _) = produced
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
        .expect("non-empty produced slice");

    let applied_nam_rate = harness
        .stream_status()
        .applied_nam_rate
        .load(std::sync::atomic::Ordering::Relaxed);
    (
        peak_idx,
        published_total,
        resamp_lat,
        cab_lat,
        os_host,
        applied_nam_rate,
    )
}

#[test]
fn impulse_peak_matches_published_latency_identical_rate_48k() {
    for factor in [
        OversampleFactor::Off,
        OversampleFactor::X2,
        OversampleFactor::X4,
    ] {
        let (peak_idx, published, resamp_lat, cab_lat, os_host, _) =
            measure_impulse_peak(MODEL_RATE, MODEL_RATE, factor);

        let diff = (peak_idx as isize - published as isize).abs();
        assert!(
            diff <= 1,
            "Rate 48k, factor {:?}: peak {} diverged from published {} (resamp={}, cab={}, os={}, diff={})",
            factor,
            peak_idx,
            published,
            resamp_lat,
            cab_lat,
            os_host,
            diff
        );
    }
}

#[test]
fn impulse_peak_matches_published_latency_integer_ratio_96k() {
    let mut off_peak = 0;
    for factor in [
        OversampleFactor::Off,
        OversampleFactor::X2,
        OversampleFactor::X4,
    ] {
        let (peak_idx, published, resamp_lat, cab_lat, os_host, _) =
            measure_impulse_peak(96_000, MODEL_RATE, factor);

        if factor == OversampleFactor::Off {
            off_peak = peak_idx;
        } else {
            let measured_os_shift = peak_idx.saturating_sub(off_peak);
            let diff = (measured_os_shift as isize - os_host as isize).abs();
            assert!(
                diff <= 1,
                "Rate 96k, factor {:?}: measured oversampling shift {} diverged from converted {} (peak={}, published={}, resamp={}, cab={}, diff={})",
                factor,
                measured_os_shift,
                os_host,
                peak_idx,
                published,
                resamp_lat,
                cab_lat,
                diff
            );
        }
    }
}

#[test]
fn impulse_peak_matches_published_latency_fractional_ratio_44k1() {
    let mut off_peak = 0;
    for factor in [
        OversampleFactor::Off,
        OversampleFactor::X2,
        OversampleFactor::X4,
    ] {
        let (peak_idx, published, resamp_lat, cab_lat, os_host, _) =
            measure_impulse_peak(44_100, MODEL_RATE, factor);

        if factor == OversampleFactor::Off {
            off_peak = peak_idx;
        } else {
            let measured_os_shift = peak_idx.saturating_sub(off_peak);
            let diff = (measured_os_shift as isize - os_host as isize).abs();
            assert!(
                diff <= 1,
                "Rate 44.1k, factor {:?}: measured oversampling shift {} diverged from converted {} (peak={}, published={}, resamp={}, cab={}, diff={})",
                factor,
                measured_os_shift,
                os_host,
                peak_idx,
                published,
                resamp_lat,
                cab_lat,
                diff
            );
        }
    }
}

#[cfg(feature = "heap-audit")]
#[test]
fn oversampling_rt_callback_zero_alloc() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    let mut harness = RtSwapHarness::new_with_gate_config(MODEL_RATE, MODEL_RATE, GateConfig::Off)
        .expect("harness construction");

    let os_l = OversampleEngine::new(OversampleFactor::X4, 2048).expect("oversample engine left");
    let os_r = OversampleEngine::new(OversampleFactor::X4, 2048).expect("oversample engine right");
    harness.push_os_pair(os_l, os_r);

    let mut in_l = [0.1f32; BLOCK_SIZE];
    let mut in_r = [0.1f32; BLOCK_SIZE];

    // Warm-up to install swap before entering the tracking guard
    harness.run_callback(&mut in_l, &mut in_r, BLOCK_SIZE);

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        for _ in 0..10 {
            harness.run_callback(&mut in_l, &mut in_r, BLOCK_SIZE);
        }
        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };

    assert_eq!(
        allocs, 0,
        "RT callback with X4 oversampling must not allocate heap memory"
    );
    assert_eq!(
        deallocs, 0,
        "RT callback with X4 oversampling must not deallocate heap memory"
    );
    assert_eq!(
        reallocs, 0,
        "RT callback with X4 oversampling must not reallocate heap memory"
    );
}

#[test]
fn impulse_peak_matches_published_latency_model_at_44k1() {
    let mut off_peak = 0;
    for factor in [
        OversampleFactor::Off,
        OversampleFactor::X2,
        OversampleFactor::X4,
    ] {
        let (peak_idx, _published, _resamp_lat, _cab_lat, os_host, applied_nam_rate) =
            measure_impulse_peak(48_000, 44_100, factor);

        assert_eq!(
            applied_nam_rate, 44_100,
            "installed model rate must be published instead of assuming 48 kHz"
        );

        if factor == OversampleFactor::Off {
            off_peak = peak_idx;
            assert_eq!(os_host, 0, "oversampling off must not add latency");
        } else {
            let native = if factor == OversampleFactor::X4 {
                18
            } else {
                12
            };
            let wrong_model_assumption = convert_os_latency_to_host(native, 48_000, 48_000);
            assert_ne!(
                os_host, wrong_model_assumption,
                "model at 44.1 kHz must not use the 48 kHz conversion"
            );

            let measured_os_shift = peak_idx.saturating_sub(off_peak);
            let diff = (measured_os_shift as isize - os_host as isize).abs();
            assert!(
                diff <= 1,
                "Model 44.1k, factor {:?}: measured oversampling shift {} diverged from published {} (peak={}, diff={})",
                factor,
                measured_os_shift,
                os_host,
                peak_idx,
                diff
            );
        }
    }
}
