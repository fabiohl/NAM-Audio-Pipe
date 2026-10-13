// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(all(feature = "dual-mono", feature = "testing"))]

//! Offline end-to-end fidelity: the full NAM-Audio-Pipe DSP chain vs the
//! engine's f64 reference oracle.
//!
//! Unlike the Pipe-internal `stereo_fidelity`/`cabsim_partition_latency`
//! harnesses — which prove self-consistency — this test feeds the *complete*
//! production pipeline (`RtSwapHarness`: input gate/gain → resampler → model →
//! resampler → output gain) and compares the delivered host-rate waveform
//! against an independent f64 oracle run over the same model and input.
//!
//! Coverage: WaveNet A1, SlimmableContainer A2 and LSTM at 48 kHz (native,
//! resampler bypass) with a contract-level hard gate, plus a 44.1 kHz
//! resampled-path smoke gate (see [`RATES`] for why the resampled rate cannot
//! carry a sample-accurate oracle gate).
//!
//! ## Stimulus choice
//!
//! The oracle is calibrated against a low-amplitude log-free sweep — the very
//! stimulus the engine's own `reference_oracle_f64` parity tests use
//! (`gen_sweep`: 220→880 Hz, amplitude 0.3, tail-window comparison). The engine
//! quality contract's `esr_f64` baselines hold in that regime. Feeding the
//! high-amplitude broadband `stress_signal_v2` instead drives the model far
//! outside f32-vs-f64 agreement (measured ≈ 9e-4 for A1) and is therefore *not*
//! a valid oracle comparison; the sweep is used here for the same reason.
//!
//! The whole suite is `#[ignore]`d: it renders the f64 oracle over full signals
//! and belongs to the long suite, not the fast CI path. Run it with
//! `cargo test --features testing -- --ignored`.
//!
//! The oracle is compiled into the engine `testing` module, so there is no
//! sibling-repository or external NAMCore binary dependency: the fixture graph
//! stays self-contained (see `tests/fixtures/.manifest.sha256`).
//!
//! Receipt provenance: each case prints `FIXTURE_SHA256[<label>]` (the model
//! fixture digest, for `pipe_long_receipt append --fixture-sha256`). The
//! in-process f64 oracle has no external artifact, so `ORACLE_SHA256` is
//! deliberately not emitted.

use nam_audio_pipe::standalone::cli::GateConfig;
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::loader::nam_json::{NamModelData, parse_nam_json};
use neural_amp_modeler_rs::loader::{LoadOptions, load_and_build_model};
use neural_amp_modeler_rs::testing::perceptual::{compute_esr, compute_snr_db};
use neural_amp_modeler_rs::testing::reference_oracle::{PrecisionConfig, oracle_forward};

use std::fs;
use std::path::{Path, PathBuf};

/// Model native rate of every committed fixture (the oracle and sweep are
/// always evaluated at 48 kHz; the host rate may differ).
const MODEL_RATE: u32 = 48_000;
/// Host quantum per callback (frames at the host rate).
const BLOCK: usize = 128;
/// Warmup length at the model rate — the oracle-vs-production transient settles
/// well before this, so it is excluded from the comparison.
const WARMUP_48K: usize = 24_000;
/// Compared tail length (host-rate samples) — long enough to be representative,
/// short enough to stay past any cold-start history mismatch.
const MEASURE: usize = 4_096;
/// Extra host-rate samples trimmed from the tail after latency alignment.
const EDGE_GUARD: usize = 32;
/// Sweep amplitude — matches the engine's calibrated oracle-test stimulus.
const SWEEP_AMPLITUDE: f64 = 0.3;

/// `(label, fixture, hard ESR gate at native 48 kHz)`.
///
/// Gates derive from the engine quality contract's *safety* envelope
/// (`max(baseline × 10, 1e-12)`) applied to the production-vs-f64 baseline
/// (`esr_f64`) recorded in `docs/quality-contract.json`:
///   wavenet-a1-standard 1.05e-13, slim-a2-example 1.82e-14, lstm-official 2.71e-12.
const FIXTURES: [(&str, &str, f64); 3] = [
    ("A1", "wavenet_a1_standard.nam", 1.05e-12),
    ("A2", "a2_example.nam", 1.0e-12),
    ("LSTM", "lstm.nam", 2.71e-11),
];

/// `(host_rate, nam_rate, esr_gate, snr_gate_db)`.
///
/// 48 kHz is the contract's native row: the resampler bypasses and the
/// comparison reaches the f64 floor, so the gate is the contract's safety
/// envelope (`esr_gate = 0.0` selects the per-fixture native gate).
///
/// 44.1 kHz exercises the active resampler, but a sample-accurate oracle gate
/// is not meaningful there: the production **streaming** resampler and the
/// one-shot reference differ by a *fractional*-sample phase (best integer
/// alignment still leaves ESR ≈ 1e-1 for the WaveNet fixtures), unlike the
/// Plug's adapter pipeline. The row therefore carries a coarse, measured
/// **resampled-path smoke gate** that only catches gross breakage (silence,
/// gain collapse, NaN) — it is deliberately *not* claimed as oracle parity.
const RATES: [(u32, u32, f64, f64); 2] = [(48_000, 48_000, 0.0, 0.0), (44_100, 48_000, 0.25, 0.0)];

fn fixture_model(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/models")
        .join(name)
}

/// SHA-256 hex of a file, for the long-suite receipt's referenced-artifact
/// provenance (`FIXTURE_SHA256`). The f64 oracle is compiled in-process, so
/// `ORACLE_SHA256` is intentionally not emitted (no external oracle artifact).
fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path).expect("read fixture for hashing");
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Log-free sine sweep (220→880 Hz) matching the engine oracle-test stimulus.
fn gen_sweep(n: usize, sample_rate: f64) -> Vec<f32> {
    let duration = n as f64 / sample_rate;
    (0..n)
        .map(|i| {
            let t = i as f64 / sample_rate;
            let phase =
                2.0 * std::f64::consts::PI * (220.0 * t + 0.5 * (880.0 - 220.0) * t * t / duration);
            (phase.sin() * SWEEP_AMPLITUDE) as f32
        })
        .collect()
}

/// Converts a mono signal `from_sr → to_sr` in one shot with a fresh, zero-state
/// resampler (identical to the adapter state at activation). Equal rates map to
/// the engine bypass (plain copy, no added latency).
fn reference_resample(signal: &[f32], from_sr: u32, to_sr: u32) -> Vec<f32> {
    if from_sr == to_sr {
        return signal.to_vec();
    }
    let mut rs = NamResampler::new_simple(to_sr, from_sr)
        .unwrap_or_else(|e| panic!("reference_resample {from_sr} → {to_sr} failed: {e}"));
    let est = (signal.len() as f64 * to_sr as f64 / from_sr as f64).ceil() as usize + 512;
    let mut out_l = vec![0.0f32; est];
    let mut out_r = vec![0.0f32; est];
    let n = rs
        .process_output_mono(signal, &mut out_l, &mut out_r)
        .samples_written;
    out_l.truncate(n);
    out_l
}

fn resampler_latency_samples(host_sr: u32, nam_sr: u32) -> usize {
    if host_sr == nam_sr {
        return 0;
    }
    NamResampler::new_simple(host_sr, nam_sr)
        .map(|r| r.latency_samples(host_sr) as usize)
        .unwrap_or(0)
}

/// Diagnostic-only integer-shift realignment of `produced` against `expected`,
/// returning `(produced_aligned, expected_aligned)` of equal length.
#[cfg(test)]
fn shift_pair<'a>(
    produced: &'a [f32],
    expected: &'a [f32],
    shift: isize,
) -> (&'a [f32], &'a [f32]) {
    if shift >= 0 {
        let s = shift as usize;
        let n = produced.len().min(expected.len().saturating_sub(s));
        (&produced[..n], &expected[s..s + n])
    } else {
        let s = (-shift) as usize;
        let n = expected.len().min(produced.len().saturating_sub(s));
        (&produced[s..s + n], &expected[..n])
    }
}

/// Loads the oracle view of a fixture.
///
/// A `SlimmableContainer` root is not understood by `oracle_forward` (it would
/// return silence); the active submodel — the last entry, matching the engine
/// container's default `active_index = len - 1` with no slimmable swap — is
/// extracted and fed instead. Calibration still comes from the container's
/// top-level metadata (handled by the caller's loader pair).
fn oracle_model_data(path: &Path) -> NamModelData {
    let json = fs::read_to_string(path).expect("read oracle fixture");
    let data = parse_nam_json(&json).expect("parse oracle fixture");
    if data.architecture == "SlimmableContainer" {
        let subs = data
            .config
            .submodels
            .as_ref()
            .expect("SlimmableContainer without submodels");
        let last = subs.last().expect("SlimmableContainer with zero submodels");
        let inner = last.get("model").expect("submodel entry without `model`");
        serde_json::from_value(inner.clone()).expect("submodel does not parse as NamModelData")
    } else {
        data
    }
}

/// Runs one `(fixture, host_rate)` fidelity comparison.
fn run_case(
    label: &str,
    fixture: &str,
    host_rate: u32,
    nam_rate: u32,
    esr_gate: f64,
    snr_gate: f64,
) {
    let model_path = fixture_model(fixture);
    assert!(model_path.exists(), "missing fixture {model_path:?}");

    // 1. Production load path (same loader the standalone uses), then harness.
    let pair = load_and_build_model(
        &model_path,
        &SystemSnapshot::capture(),
        true,
        LoadOptions::default(),
    )
    .unwrap_or_else(|e| panic!("{label}: load_and_build_model failed: {e}"));

    let mut harness = RtSwapHarness::new_with_gate_config(host_rate, nam_rate, GateConfig::Off)
        .unwrap_or_else(|e| panic!("{label}: harness construction failed: {e}"));
    harness.push_load_model(
        pair.model_l,
        pair.model_r,
        pair.input_mult_adj,
        pair.output_mult_adj,
        pair.sample_rate,
    );

    // 2. Stimulus: 48 kHz model-rate sweep, presented at the host rate exactly
    //    the way the production input stage receives it.
    let sweep = gen_sweep(WARMUP_48K + MEASURE, MODEL_RATE as f64);
    let host_input = reference_resample(&sweep, MODEL_RATE, host_rate);

    // 3. Drive the full production chain, collecting host-rate output.
    let mut produced: Vec<f32> = Vec::with_capacity(host_input.len() + (BLOCK * 4));
    let mut offset = 0usize;
    while offset < host_input.len() {
        let n = BLOCK.min(host_input.len() - offset);
        let mut in_l = host_input[offset..offset + n].to_vec();
        let mut in_r = host_input[offset..offset + n].to_vec();
        let n_pw = harness.run_callback(&mut in_l, &mut in_r, n);
        if n_pw > 0 {
            produced.extend_from_slice(&harness.out_l()[..n_pw]);
        }
        offset += n;
    }

    // 4. Independent f64 reference over the same model-rate input. The oracle
    //    input round-trips host → model rate so the input-stage group delay is
    //    represented exactly; the oracle output is resampled back to the host.
    let oracle_input = reference_resample(&host_input, host_rate, MODEL_RATE);
    let oracle_in_f64: Vec<f64> = oracle_input
        .iter()
        .map(|&s| (s as f64) * (pair.input_mult_adj as f64))
        .collect();
    let oracle_data = oracle_model_data(&model_path);
    let oracle_out = oracle_forward(&oracle_data, &oracle_in_f64, &PrecisionConfig::default());
    let oracle_scaled: Vec<f32> = oracle_out
        .iter()
        .map(|&s| (s * pair.output_mult_adj as f64) as f32)
        .collect();
    let expected = reference_resample(&oracle_scaled, MODEL_RATE, host_rate);

    // 5. Align and compare the measured tail past the warmup. The resampler
    //    phase can differ by a few host samples between the streaming
    //    production path and the one-shot reference, so a small integer
    //    alignment search is applied before measuring.
    let latency = resampler_latency_samples(host_rate, nam_rate);
    let overlap_end = produced.len().min(expected.len());
    let mut best = (0isize, f64::INFINITY, f64::NEG_INFINITY, 0usize);
    for sh in -(latency as isize) - 16..=(latency as isize) + 16 {
        let (a, b) = shift_pair(&produced[..overlap_end], &expected[..overlap_end], sh);
        let end = a.len().saturating_sub(EDGE_GUARD);
        let start = end.saturating_sub(MEASURE);
        if end <= start + 1024 {
            continue;
        }
        let e = compute_esr(&b[start..end], &a[start..end]);
        if e < best.1 {
            best = (
                sh,
                e,
                compute_snr_db(&b[start..end], &a[start..end]),
                end - start,
            );
        }
    }
    let (align, esr, snr_db, win_len) = best;
    assert!(
        win_len > 1024,
        "{label}@{host_rate}: insufficient overlap (produced={}, expected={})",
        produced.len(),
        expected.len()
    );

    let effective_snr = if snr_gate > 0.0 {
        snr_gate
    } else {
        f64::NEG_INFINITY
    };

    println!(
        "// Measured: {label}@{host_rate}Hz  ESR={esr:.3e} (gate {:.2e})  SNR={snr_db:.2} dB (gate {effective_snr:.1} dB)  align={align:+}  lat={latency}  n={win_len}",
        if esr_gate > 0.0 { esr_gate } else { f64::NAN },
    );
    println!("TEST_RESULT[offline_parity_{label}_{host_rate}] esr={esr:.3e} snr_db={snr_db:.2}");
    println!("FIXTURE_SHA256[{label}]={}", sha256_file(&model_path));

    if esr_gate > 0.0 {
        assert!(
            esr < esr_gate,
            "{label}@{host_rate}: ESR {esr:.3e} exceeds gate {esr_gate:.2e}"
        );
    }
    if snr_gate > 0.0 {
        assert!(
            snr_db > effective_snr,
            "{label}@{host_rate}: SNR {snr_db:.2} dB below gate {effective_snr:.1} dB"
        );
    }
}

/// Full offline chain parity across every fixture and both host rates.
#[test]
#[ignore = "renders the f64 oracle over full signals — long suite, run with --ignored"]
fn pipe_offline_full_chain_parity() {
    for (host_rate, nam_rate, esr_gate, snr_gate) in RATES {
        for (label, fixture, native_esr) in FIXTURES {
            let gate = if host_rate == MODEL_RATE {
                native_esr
            } else {
                esr_gate
            };
            run_case(label, fixture, host_rate, nam_rate, gate, snr_gate);
        }
    }
}
