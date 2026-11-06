// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Criterion harness for the NAM-Audio-Pipe RT regression gate.
//!
//! Promotes the offline `rt_deadline` real-load scenario (previously a single
//! `RT_METRICS rt_deadline …` log line parsed by the old shell gate) to a
//! Criterion suite, so the engine `nam_perf_gate` can evaluate it per scenario
//! (coverage + machine verdict over `change/estimates.json`) using the same
//! harness contract as the upstream engine. Each scenario drives the production
//! [`RtSwapHarness`] single-block callback with the same real load the long
//! suite uses (fixture-backed WaveNet A1/A2, LSTM and a synthetic stereo CabSim).
//!
//! Requires `--features testing` (the harness is test-only).

#![cfg(feature = "testing")]

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::loader::dispatcher::build_model;
use neural_amp_modeler_rs::loader::nam_json::parse_nam_json;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};

const SAMPLE_RATE: u32 = 48_000;
const BLOCK: usize = 64;
const PARTITION: usize = 512;
const IR_LEN: usize = 4096;

const WAVENET_A1_JSON: &str = include_str!("../tests/fixtures/models/wavenet_a1_standard.nam");
const WAVENET_A2_JSON: &str = include_str!("../tests/fixtures/models/a2_example.nam");
const LSTM_JSON: &str = include_str!("../tests/fixtures/models/lstm.nam");

fn model(json: &str) -> Box<StaticModel> {
    let data = parse_nam_json(json).expect("parse fixture");
    let mut model = build_model(&data).expect("build model");
    model.prewarm(64);
    model
}

fn cabsim_pair() -> Box<CabSimPair> {
    fn synth_ir(len: usize, freq: f32, decay: f32) -> Vec<f32> {
        let sr = SAMPLE_RATE as f32;
        (0..len)
            .map(|i| {
                let t = i as f32 / sr;
                (std::f32::consts::TAU * freq * t).sin() * (-decay * t).exp()
            })
            .collect()
    }
    let mk = |ir: &[f32]| {
        CabSimAdapter::new(Box::new(ConvEngine::new(ir, PARTITION).expect("conv")))
            .expect("adapter")
    };
    Box::new(CabSimPair {
        l: Box::new(mk(&synth_ir(IR_LEN, 880.0, 14.0))),
        r: Box::new(mk(&synth_ir(IR_LEN, 1760.0, 22.0))),
        sample_rate: SAMPLE_RATE,
    })
}

/// Builds and warms a harness with the given optional L/R models and CabSim.
fn harness(
    model_l: Option<Box<StaticModel>>,
    model_r: Option<Box<StaticModel>>,
    cabsim: bool,
) -> RtSwapHarness {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("rt harness");
    h.push_load_model(model_l, model_r, 1.0, 1.0, SAMPLE_RATE);
    if cabsim {
        h.push_cabsim(Some(cabsim_pair()));
    }
    h.push_input_gain(1.0);
    h.push_output_gain(1.0);
    for _ in 0..8 {
        let mut l = [0.1f32; BLOCK];
        let mut r = [0.2f32; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }
    h.consume_gc();
    h
}

/// Benchmarks one warmed harness callback, re-presenting fresh input blocks via
/// `iter_batched_ref` outside the timed region so the signal is non-feedback and
/// constant across iterations, with zero allocations inside the timed loop.
fn bench_callback(
    c: &mut Criterion,
    group: &str,
    id: &str,
    mut h: RtSwapHarness,
    has_effect: bool,
) {
    // 1. Preflight check: verify run_callback processes BLOCK frames and produces valid output
    let mut pre_l = [0.1f32; BLOCK];
    let mut pre_r = [0.2f32; BLOCK];
    let n_pw = h.run_callback(&mut pre_l, &mut pre_r, BLOCK);
    assert_eq!(
        n_pw, BLOCK,
        "scenario {id}: run_callback did not process full block (got {n_pw})"
    );
    assert!(
        h.out_l().iter().all(|s| s.is_finite()),
        "scenario {id}: out_l contains non-finite samples"
    );
    assert!(
        h.out_r().iter().all(|s| s.is_finite()),
        "scenario {id}: out_r contains non-finite samples"
    );
    if has_effect {
        let energy_l: f32 = h.out_l().iter().map(|s| s * s).sum();
        let energy_r: f32 = h.out_r().iter().map(|s| s * s).sum();
        assert!(
            energy_l > 1e-9 && energy_r > 1e-9,
            "scenario {id}: expected non-silent output with effect"
        );
    }

    let mut g = c.benchmark_group(group);
    g.bench_function(id, |b| {
        b.iter_batched_ref(
            || ([0.1f32; BLOCK], [0.2f32; BLOCK]),
            |(l, r)| {
                let n_pw = h.run_callback(l, r, BLOCK);
                assert_eq!(n_pw, BLOCK);
                std::hint::black_box(h.out_l());
                std::hint::black_box(h.out_r());
            },
            BatchSize::SmallInput,
        );
    });
    g.finish();

    // 2. Post-bench validation: verify output is still finite and non-silent
    assert!(
        h.out_l().iter().all(|s| s.is_finite()),
        "scenario {id}: post-bench out_l contains non-finite samples"
    );
    assert!(
        h.out_r().iter().all(|s| s.is_finite()),
        "scenario {id}: post-bench out_r contains non-finite samples"
    );
    if has_effect {
        let energy_l: f32 = h.out_l().iter().map(|s| s * s).sum();
        let energy_r: f32 = h.out_r().iter().map(|s| s * s).sum();
        assert!(
            energy_l > 1e-9 && energy_r > 1e-9,
            "scenario {id}: post-bench expected non-silent output with effect"
        );
    }
}

fn bench_host_scenarios(c: &mut Criterion) {
    bench_callback(
        c,
        "PIPE_Host",
        "Full_Load_64_samp",
        harness(
            Some(model(WAVENET_A1_JSON)),
            Some(model(WAVENET_A2_JSON)),
            true,
        ),
        true,
    );
    bench_callback(
        c,
        "PIPE_Host",
        "LSTM_64_samp",
        harness(Some(model(LSTM_JSON)), Some(model(LSTM_JSON)), false),
        true,
    );
    bench_callback(
        c,
        "PIPE_Host",
        "Passthrough_64_samp",
        harness(None, None, false),
        false,
    );

    // Per-topology anchors mirroring the ab_opt_bench scenarios,
    // so a single coverage manifest spans the host and A/B-opt envelopes.
    bench_callback(
        c,
        "PIPE_AbOpt",
        "WaveNet_A1_48k_64_samp",
        harness(
            Some(model(WAVENET_A1_JSON)),
            Some(model(WAVENET_A1_JSON)),
            false,
        ),
        true,
    );
    bench_callback(
        c,
        "PIPE_AbOpt",
        "WaveNet_A2_48k_64_samp",
        harness(
            Some(model(WAVENET_A2_JSON)),
            Some(model(WAVENET_A2_JSON)),
            false,
        ),
        true,
    );
    bench_callback(
        c,
        "PIPE_AbOpt",
        "LSTM_48k_64_samp",
        harness(Some(model(LSTM_JSON)), Some(model(LSTM_JSON)), false),
        true,
    );
}

criterion_group! {
    name = rt_gate_benches;
    config = Criterion::default().sample_size(30).noise_threshold(0.05);
    targets = bench_host_scenarios
}
criterion_main!(rt_gate_benches);
