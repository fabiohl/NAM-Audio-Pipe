// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Empirical end-to-end validation of the T2.3 cab-sim partition policy
//! (`--cabsim-partition`), measured through the REAL RT callback sequence
//! (`RtSwapHarness` — the full `capture process()` mirror: drains, rate sync,
//! gate FSM, resampler and `capture_dsp_pipeline_streaming`) with no PipeWire
//! daemon.
//!
//! Invariants proven here (the ones T2.3 publishes for telemetry and delay
//! compensation):
//!
//! 1. **Partition policy is really in effect end-to-end** — feeding
//!    sub-partition host blocks (16-sample, the sample-accurate automation
//!    regime) through the production pipeline yields the exact UPOLS FIFO
//!    underrun prefix documented by the engine
//!    (`block * (ceil(P / block) - 1)`), which scales strictly with the
//!    policy value; the audible stream after the prefix is the direct causal
//!    FIR of the chosen IR (ESR gate).
//! 2. **Partition < quantum safety** — a 32-sample policy under a 512-sample
//!    host quantum never raises `RT_STATUS_CABSIM_CONTRACT_VIOLATION`,
//!    neither on the active path (`process_block`) nor on the gate-closed
//!    ring-out (`drain_tail`), and the IR tail is rendered to completion.
//! 3. **Published latency** — the pair state matches the policy
//!    (`latency_samples() == partition`, the figure the standalone logs
//!    publish in samples and milliseconds at initial load and rebuild).

#![cfg(feature = "testing")]

use nam_audio_pipe::standalone::cli::GateConfig;
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_CABSIM_CONTRACT_VIOLATION;
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;

const RATE: u32 = 48000;
const IR_LEN: usize = 2304; // 48 ms @ 48 kHz — 9 partitions of 256

fn make_ir() -> Vec<f32> {
    (0..IR_LEN)
        .map(|i| {
            let decay = (-9.0 * i as f32 / IR_LEN as f32).exp();
            let carrier = ((i % 7) as f32 / 7.0 + 0.1) * 0.8;
            carrier * decay
        })
        .collect()
}

/// Builds a stereo-decoupled cab-sim pair exactly as the main-thread
/// `build_cabsim_pair` does (identical engines, independent adapters), with a
/// deterministic decaying IR (the `target_rate = 0` path of
/// `load_initial_cabsim`: no resampling).
fn make_pair(partition: usize, rate: u32) -> CabSimPair {
    let ir = make_ir();
    let make = || {
        let engine = ConvEngine::new(&ir, partition).expect("test engine");
        CabSimAdapter::new(Box::new(engine)).expect("test adapter")
    };
    CabSimPair {
        l: Box::new(make()),
        r: Box::new(make()),
        sample_rate: rate,
    }
}

/// Exact causal FIR reference.
fn direct_convolve(ir: &[f32], signal: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0f32; ir.len() + signal.len() - 1];
    for (i, &s) in signal.iter().enumerate() {
        if s == 0.0 {
            continue;
        }
        let max_j = ir.len().min(out.len() - i);
        for (j, &h) in ir.iter().take(max_j).enumerate() {
            out[i + j] += s * h;
        }
    }
    out
}

/// ESR between two aligned streams.
fn compute_esr(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    let mut num = 0.0f64;
    let mut den = 0.0f64;
    for i in 0..n {
        let e = (a[i] - b[i]) as f64;
        num += e * e;
        den += b[i] as f64 * b[i] as f64;
    }
    if den == 0.0 {
        return if num == 0.0 { 0.0 } else { f64::INFINITY };
    }
    num / den
}

/// Causal silence prefix of a fresh adapter driven with host blocks of
/// `block` samples (`block <= partition`): the samples delivered before the
/// first partition completes. Same formula the engine's ESR suite pins.
fn accumulation_prefix(partition: usize, block: usize) -> usize {
    block * (partition.div_ceil(block) - 1)
}

/// Invariant 1 (T2.3): the partition policy is really in effect end-to-end.
/// Sample-accurate 16-sample host blocks through the REAL RT callback yield
/// the documented underrun prefix — which scales strictly with the policy —
/// followed by the direct causal FIR of the same IR (ESR gate).
#[test]
fn cabsim_partition_policy_shapes_the_rt_output_stream() {
    let block = 16usize;
    let stimulus: Vec<f32> = (0..1024).map(|i| 0.7 * (i as f32 * 0.043).sin()).collect();

    // Direct FIR reference of the same IR and stimulus.
    let ir = make_ir();
    let reference = direct_convolve(&ir, &stimulus);

    let mut prev_prefix = 0usize;
    for partition in [32usize, 64, 128, 256] {
        let mut harness = RtSwapHarness::new_with_gate_config(RATE, RATE, GateConfig::Off)
            .expect("harness construction");
        harness.push_cabsim(Some(Box::new(make_pair(partition, RATE))));

        let silence = vec![0.0f32; block];
        harness.run_callback(&mut silence.clone(), &mut silence.clone(), block);
        assert!(harness.active_cabsim().is_some());

        // Feed the stimulus in 16-sample callbacks, collecting the produced
        // stream (plus generous zero-input tail so the FIR completes).
        let mut produced: Vec<f32> = Vec::new();
        for chunk in stimulus.chunks(block) {
            let mut in_l = chunk.to_vec();
            let mut in_r = vec![0.0f32; chunk.len()];
            harness.run_callback(&mut in_l, &mut in_r, chunk.len());
            produced.extend_from_slice(harness.out_l());
        }
        for _ in 0..(IR_LEN / block + 4) {
            harness.run_callback(&mut silence.clone(), &mut silence.clone(), block);
            produced.extend_from_slice(harness.out_l());
        }

        // The documented underrun prefix: silence until the first partition
        // completes, then the direct causal FIR, block-aligned. The prefix is
        // not bit-silent: the pipeline's denormal dither (1e-11) accumulates
        // through the FIR to ~1e-6 over the prefix — 120 dB below the signal,
        // so the inaudibility floor for the prefix is 1e-4.
        let expected_prefix = accumulation_prefix(partition, block);
        let offending = produced[..expected_prefix]
            .iter()
            .enumerate()
            .find(|(_i, s)| s.abs() >= 1e-4)
            .map(|(i, &s)| (i, s));
        assert!(
            offending.is_none(),
            "partition {partition}: the first {expected_prefix} samples must be the documented \
             underrun prefix (inaudible, < 1e-4); offending sample {:?}; first 6 produced: {:?}",
            offending,
            &produced[..produced.len().min(6)]
        );
        let n = reference.len().min(produced.len() - expected_prefix);
        let esr = compute_esr(
            &reference[..n],
            &produced[expected_prefix..expected_prefix + n],
        );
        assert!(
            esr < 1e-5,
            "partition {partition}: the RT output stream must be the direct causal FIR after the prefix (ESR = {esr:.2e})"
        );
        assert!(
            expected_prefix > prev_prefix,
            "the underrun prefix must scale strictly with the policy value"
        );
        prev_prefix = expected_prefix;
    }
    // Prefix at the top of the CLI domain: P - block.
    assert_eq!(prev_prefix, 256 - block);
}

/// Invariant 2 (T2.3 + the T2.1 latent note): with the partition policy below
/// the host quantum (32 under 512), neither the active path (`process_block`)
/// nor the gate-closed ring-out (`drain_tail`) raises
/// `RT_STATUS_CABSIM_CONTRACT_VIOLATION`, and the IR tail is flushed to
/// completion.
#[test]
fn partition_below_quantum_never_violates_contract_and_drains_tail() {
    let partition = 32usize;
    let quantum = 512usize;
    let mut harness = RtSwapHarness::new_with_gate_config(RATE, RATE, GateConfig::default_on())
        .expect("harness construction");
    harness.push_cabsim(Some(Box::new(make_pair(partition, RATE))));

    let silence = vec![0.0f32; quantum];

    // 1. Install the pair (silent callback).
    harness.run_callback(&mut silence.clone(), &mut silence.clone(), quantum);
    assert!(harness.active_cabsim().is_some());

    // 2. Loud active audio: opens the gate and arms the tail budget.
    let mut loud = vec![0.0f32; quantum];
    for (i, s) in loud.iter_mut().enumerate() {
        *s = if (i as f32 * 0.37).sin() >= 0.0 {
            0.9
        } else {
            -0.9
        };
    }
    for _ in 0..4 {
        harness.run_callback(&mut loud.clone(), &mut silence.clone(), quantum);
    }
    assert!(
        !harness
            .rt_status()
            .check_flag(RT_STATUS_CABSIM_CONTRACT_VIOLATION),
        "process_block with partition < quantum must never flag a contract violation"
    );

    // 3. Silence: the gate closes and the drain path runs with 512-sample
    //    blocks — 16× the partition. Block-agnostic drain must flush the
    //    complete tail without clamping or flagging.
    let mut ringout_samples = 0usize;
    let mut ringout_nonsilent = false;
    for _ in 0..24 {
        harness.run_callback(&mut silence.clone(), &mut silence.clone(), quantum);
        if harness
            .rt_status()
            .check_flag(RT_STATUS_CABSIM_CONTRACT_VIOLATION)
        {
            panic!(
                "gate-closed drain with quantum {quantum} > partition {partition} \
                 must never flag a contract violation"
            );
        }
        let n = harness.current_n_pw();
        let out = &harness.out_l()[..n];
        ringout_samples += n;
        ringout_nonsilent |= out.iter().any(|&s| s.abs() > 1e-4);
    }

    assert!(
        ringout_nonsilent,
        "the drain path must render the audible IR ring-out through oversize blocks"
    );
    // The pair budget is `tail_samples` (IR + one partition); the quantum
    // stream consumed it fully within the 24 rounds above.
    let budget = harness
        .active_cabsim()
        .map(|p| p.l.tail_samples())
        .unwrap_or(0);
    assert!(
        ringout_samples >= budget,
        "the drain must be given at least the armed budget ({budget}) in host samples, got {ringout_samples}"
    );
}

/// Invariant 3 (T2.3): the pair state matches the published policy — exact
/// partition, exact algorithmic latency and the expected partition count at
/// the top of the CLI domain, under the maximum production quantum (1024).
#[test]
fn latency_publication_matches_pair_state() {
    let mut harness = RtSwapHarness::new_with_gate_config(RATE, RATE, GateConfig::Off)
        .expect("harness construction");
    harness.push_cabsim(Some(Box::new(make_pair(256, RATE))));
    let silence = vec![0.0f32; 1024];
    harness.run_callback(&mut silence.clone(), &mut silence.clone(), 1024);

    let pair = harness.active_cabsim().expect("pair installed");
    assert_eq!(pair.partition_size(), 256);
    assert_eq!(pair.l.latency_samples(), 256);
    assert_eq!(
        pair.l.num_partitions(),
        IR_LEN.div_ceil(256),
        "engine partition count must cover the whole IR"
    );
}
