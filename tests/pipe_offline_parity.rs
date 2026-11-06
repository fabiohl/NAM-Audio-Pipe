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
//! The f64 oracle is compiled into the engine `testing` module, so the fixture
//! graph stays self-contained (see `tests/fixtures/.manifest.sha256`). When a
//! NAMCore C++ render binary is provisioned (`NAM_CORE_RENDER_BIN`, exported by
//! the hosted workflow), the same case is additionally compared against the C++
//! oracle. `NAM_REQUIRE_CPP_ORACLE=1` (strict mode) makes its absence fatal.
//!
//! Receipt provenance: each case prints `FIXTURE_SHA256[<label>]` (the model
//! fixture digest, for `pipe_long_receipt append --fixture-sha256`). The
//! in-process f64 oracle has no external artifact, so `ORACLE_SHA256` is
//! emitted only for the provisioned C++ render binary.

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
/// (`esr_f64`) recorded in `NeuralAmpModeler-rs/docs/quality-contract.json`:
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

// Measured (2026-11-05, release, NAMCore C++ oracle): the C++ rows sit on the
// same floor as f64 at both host rates and reuse the per-fixture native gates:
//   A1   48 kHz: f64 1.418e-13 / C++ 1.854e-13
//   A2   48 kHz: f64 6.095e-14 / C++ 1.526e-13
//   LSTM 48 kHz: f64 1.119e-11 / C++ 1.064e-11
//   44.1 kHz rows: ESR ≤ 9.9e-12 and SNR ≥ 110 dB for both oracles.

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

/// Resolves the NAMCore C++ render binary.
///
/// `NAM_CORE_RENDER_BIN` is authoritative (exported by the hosted workflow).
/// When it is absent, the engine's fixture helper resolves the render build of
/// the linked engine checkout. `NAM_REQUIRE_CPP_ORACLE=1` turns absence into a
/// hard failure instead of a silent skip.
fn cpp_oracle_bin() -> Option<PathBuf> {
    if let Ok(val) = std::env::var("NAM_CORE_RENDER_BIN") {
        let p = PathBuf::from(val);
        if p.is_file() {
            return Some(p);
        }
    }
    let fallback = neural_amp_modeler_rs::testing::fixtures::render_bin_path();
    fallback.is_file().then_some(fallback)
}

fn oracle_required() -> bool {
    std::env::var("NAM_REQUIRE_CPP_ORACLE").as_deref() == Ok("1")
}

/// Renders the same model-rate input through the C++ NAMCore oracle and returns
/// the host-rate expected curve (calibrated and resampled like the f64 path).
fn cpp_expected(
    bin: &Path,
    model_path: &Path,
    oracle_input: &[f32],
    input_mult_adj: f32,
    output_mult_adj: f32,
    host_rate: u32,
    tmp_dir: &Path,
) -> Vec<f32> {
    let scaled: Vec<f32> = oracle_input.iter().map(|&s| s * input_mult_adj).collect();
    let stem = model_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("model");
    let wav_in = tmp_dir.join(format!("cpp_in_{stem}_{host_rate}.wav"));
    let wav_out = tmp_dir.join(format!("cpp_out_{stem}_{host_rate}.wav"));
    neural_amp_modeler_rs::testing::wav::write_wav_f32(&wav_in, &scaled, MODEL_RATE)
        .expect("write C++ oracle input WAV");
    let output = std::process::Command::new(bin)
        .arg(model_path)
        .arg(&wav_in)
        .arg(&wav_out)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute NAMCore render {bin:?}: {e}"));
    assert!(
        output.status.success(),
        "{host_rate}: NAMCore render failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let (raw, _rate) = neural_amp_modeler_rs::testing::wav::read_wav_f32(&wav_out)
        .expect("read C++ oracle output WAV");
    let calibrated: Vec<f32> = raw.iter().map(|&s| s * output_mult_adj).collect();
    reference_resample(&calibrated, MODEL_RATE, host_rate)
}

/// Aligns `produced` against `expected`, measures the steady-state tail, prints
/// the `// Measured:` evidence and enforces the contract gates.
fn compare_curves(
    label: &str,
    host_rate: u32,
    produced: &[f32],
    expected: &[f32],
    latency: usize,
    gates: (f64, f64),
    oracle: &str,
) {
    let (esr_gate, snr_gate) = gates;
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
        "{label}@{host_rate}[{oracle}]: insufficient overlap (produced={}, expected={})",
        produced.len(),
        expected.len()
    );

    let effective_snr = if snr_gate > 0.0 {
        snr_gate
    } else {
        f64::NEG_INFINITY
    };

    println!(
        "// Measured: {label}@{host_rate}Hz [{oracle}]  ESR={esr:.3e} (gate {:.2e})  SNR={snr_db:.2} dB (gate {effective_snr:.1} dB)  align={align:+}  lat={latency}  n={win_len}",
        if esr_gate > 0.0 { esr_gate } else { f64::NAN },
    );

    if esr_gate > 0.0 {
        assert!(
            esr < esr_gate,
            "{label}@{host_rate}[{oracle}]: ESR {esr:.3e} exceeds gate {esr_gate:.2e}"
        );
    }
    if snr_gate > 0.0 {
        assert!(
            snr_db > effective_snr,
            "{label}@{host_rate}[{oracle}]: SNR {snr_db:.2} dB below gate {effective_snr:.1} dB"
        );
    }

    println!(
        "TEST_RESULT[offline_parity_{label}_{host_rate}_{oracle}]=PASS esr={esr:.3e} snr_db={snr_db:.2}"
    );
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

    // 5. Compare against both oracles: the in-process f64 reference (mandatory)
    //    and, when provisioned, the C++ NAMCore render binary. The hosted
    //    workflow exports NAM_CORE_RENDER_BIN; `NAM_REQUIRE_CPP_ORACLE=1`
    //    (strict mode) makes an absent oracle a hard failure.
    let latency = resampler_latency_samples(host_rate, nam_rate);
    let cpp_bin = cpp_oracle_bin();
    if cpp_bin.is_none() && oracle_required() {
        panic!(
            "{label}@{host_rate}: NAM_REQUIRE_CPP_ORACLE=1 but no NAMCore render binary was found"
        );
    }
    println!("FIXTURE_SHA256[{label}]={}", sha256_file(&model_path));
    compare_curves(
        label,
        host_rate,
        &produced,
        &expected,
        latency,
        (esr_gate, snr_gate),
        "f64",
    );
    if let Some(bin) = cpp_bin {
        let tmp_dir = std::env::temp_dir().join("nam_pipe_offline_parity");
        std::fs::create_dir_all(&tmp_dir).ok();
        let cpp = cpp_expected(
            &bin,
            &model_path,
            &oracle_input,
            pair.input_mult_adj,
            pair.output_mult_adj,
            host_rate,
            &tmp_dir,
        );
        compare_curves(
            label,
            host_rate,
            &produced,
            &cpp,
            latency,
            (esr_gate, snr_gate),
            "cpp",
        );
        println!("ORACLE_SHA256[cpp]={}", sha256_file(&bin));
    }
}

/// Full offline chain parity across every fixture and both host rates.
#[test]
#[ignore = "renders the f64 and NAMCore C++ oracles over full signals — long suite, run with --ignored"]
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
