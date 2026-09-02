// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(all(feature = "stereo", feature = "testing"))]

//! Stereo fidelity verification harness.
//!
//! Proves that the unified stereo pipeline (as wired by NAM-Audio-Pipe via
//! `capture_dsp_pipeline`) processes each channel **exactly** as an isolated
//! mono instance would: the stereo output for channel X must be bit-identical
//! to a 100% independent mono pipeline fed only channel X.
//!
//! Signal sources (complex, deterministic):
//! - White noise (independent L/R seeds via xorshift64*),
//! - Chirp (L/R at quadrature phase so the mono detector never folds),
//! - Plucked-string "guitar" (Karplus–Strong synthesis; no real guitar WAV is
//!   committed in the repo — a deterministic plucked-string is the closest
//!   reproducible approximation of a guitar waveform).
//!
//! Acceptance: bit-exact equality → `MSE == 0.0` exactly and
//! `SNR > 120 dB`.

mod common;

use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
use neural_amp_modeler_rs::loader::dispatcher::build_model;
use neural_amp_modeler_rs::loader::nam_json::parse_nam_json;
use neural_amp_modeler_rs::models::{NamModel, StaticModel};

use std::fs;
use std::path::PathBuf;

/// Audio block size per callback (matches a typical 64-frame PipeWire quantum).
const BLOCK: usize = 64;
/// Cab-sim partition size (FFT partition of the convolution engine).
const PARTITION: usize = 512;
/// Synthetic IR length (samples).
const IR_LEN: usize = 4096;
/// Host/model sample rate (48 kHz — resampler bypass in the main tests; the
/// multirate test exercises the active resampler).
const SAMPLE_RATE: u32 = 48_000;
/// Measured signal length in samples (~0.5 s at 48 kHz).
const SIGNAL_LEN: usize = 24_000;
/// Number of leading blocks excluded from the bit-exact assertion. Both the
/// stereo run and the mono references share identical state machines, so in
/// principle every block matches; the warm-up only guards against any
/// uninitialized-tail artifact in the convolution engines.
const WARMUP_BLOCKS: usize = 8;

/// Accumulated fidelity result.
struct FidelityResult {
    mse: f64,
    snr_db: f64,
    compared: usize,
    first_mismatch: Option<usize>,
}

fn fixture_model(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/models")
        .join(name)
}

/// Loads one independent model instance from a fixture. Every call returns a
/// fresh instance with identical weights and prewarm, so instances fed the same
/// input stay bit-synchronized.
fn load_model_instance(name: &str) -> Box<StaticModel> {
    let json = fs::read_to_string(fixture_model(name)).expect("read model fixture");
    let data = parse_nam_json(&json).expect("parse model json");
    let mut model = build_model(&data).expect("build model");
    model.prewarm(2048);
    model
}

/// Deterministic synthetic IR (decaying sinusoid).
fn synth_ir(len: usize, freq: f32, decay: f32) -> Vec<f32> {
    let sr = SAMPLE_RATE as f32;
    (0..len)
        .map(|i| {
            let t = i as f32 / sr;
            (std::f32::consts::TAU * freq * t).sin() * (-decay * t).exp()
        })
        .collect()
}

fn adapter_from_ir(ir: &[f32]) -> CabSimAdapter {
    CabSimAdapter::new(Box::new(
        ConvEngine::new(ir, PARTITION).expect("conv engine"),
    ))
    .expect("cab-sim adapter")
}

fn pair_from_ir(ir_l: &[f32], ir_r: &[f32]) -> Box<CabSimPair> {
    Box::new(CabSimPair {
        l: Box::new(adapter_from_ir(ir_l)),
        r: Box::new(adapter_from_ir(ir_r)),
        sample_rate: SAMPLE_RATE,
    })
}

// ── Deterministic signal generators ─────────────────────────────────────────

/// xorshift64* PRNG (SplitMix finalizer) — deterministic across runs.
#[derive(Clone, Copy)]
struct Xorshift64(u64);

impl Xorshift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
}

/// Independent white-noise channel pair (two seeds).
fn white_noise_pair() -> (Vec<f32>, Vec<f32>) {
    let mut a = Xorshift64(0x9E37_79B9_7F4A_7C15);
    let mut b = Xorshift64(0xC2B2_AE3D_27D4_EB4F);
    let l: Vec<f32> = (0..SIGNAL_LEN).map(|_| 0.4 * a.next_f32()).collect();
    let r: Vec<f32> = (0..SIGNAL_LEN).map(|_| 0.4 * b.next_f32()).collect();
    (l, r)
}

/// Chirp channel pair — L at phase 0, R at quadrature phase π/2, so the mono
/// detector (L/R max-diff) always keeps the stereo run in true stereo mode.
fn chirp_pair() -> (Vec<f32>, Vec<f32>) {
    let f0 = 20.0f32;
    let f1 = 20_000.0f32;
    let sr = SAMPLE_RATE as f32;
    let phase_l: Vec<f32> = (0..SIGNAL_LEN)
        .map(|i| {
            let t = i as f32 / sr;
            let sweep = f0 * t + 0.5 * (f1 - f0) * t * t / (SIGNAL_LEN as f32 / sr);
            (std::f32::consts::TAU * sweep).sin()
        })
        .collect();
    let phase_r: Vec<f32> = (0..SIGNAL_LEN)
        .map(|i| {
            let t = i as f32 / sr;
            let sweep = f0 * t + 0.5 * (f1 - f0) * t * t / (SIGNAL_LEN as f32 / sr);
            (std::f32::consts::TAU * sweep).cos()
        })
        .collect();
    (phase_l, phase_r)
}

/// Karplus–Strong plucked-string "guitar" channels (different pitches).
fn plucked_string_pair() -> (Vec<f32>, Vec<f32>) {
    let seed = Xorshift64(0xD1B5_4A32_D192_ED03);
    let n = SIGNAL_LEN;
    let sr = SAMPLE_RATE as f32;
    let pluck = |freq: f32, mut seed: Xorshift64| -> Vec<f32> {
        let period = (sr / freq).round() as usize;
        let mut ring: Vec<f32> = (0..period).map(|_| seed.next_f32()).collect();
        let mut out = vec![0.0f32; n];
        for i in 0..n {
            let cur = ring[i % period];
            let next = ring[(i + 1) % period];
            let filtered = 0.5 * (cur + next) * 0.996;
            ring[i % period] = filtered;
            out[i] = filtered;
        }
        // Normalize to ~0.4 peak to stay clear of clipping and gate thresholds.
        let peak = out.iter().fold(0.0f32, |a, &s| a.max(s.abs())).max(1e-6);
        out.iter().map(|&s| 0.4 * s / peak).collect()
    };
    (pluck(110.0, seed), pluck(220.0, seed))
}

// ── Core comparison ─────────────────────────────────────────────────────────

/// Runs the unified stereo pipeline and two isolated mono pipelines over
/// `signal_l`/`signal_r`, accumulating per-sample squared error. Returns the
/// bit-exactness verdict as MSE/SNR.
fn compare_stereo_vs_dual_mono(
    signal_l: &[f32],
    signal_r: &[f32],
    host_rate: u32,
    nam_rate: u32,
) -> FidelityResult {
    assert_eq!(signal_l.len(), signal_r.len());
    assert!(signal_l.len().is_multiple_of(BLOCK));

    // 4 independent instances — the stereo run and each mono reference have
    // their own model/cab-sim instances (100% isolated mono paths).
    let model_l = load_model_instance("lstm.nam");
    let model_r = load_model_instance("lstm.nam");
    let model_l_ref = load_model_instance("lstm.nam");
    let model_r_ref = load_model_instance("lstm.nam");

    let ir_l = synth_ir(IR_LEN, 880.0, 14.0);
    let ir_r = synth_ir(IR_LEN, 1760.0, 22.0);

    let mut stereo = RtSwapHarness::new(host_rate, nam_rate).expect("stereo harness");
    stereo.push_load_model(Some(model_l), Some(model_r), 1.0, 1.0, nam_rate);
    stereo.push_cabsim(Some(pair_from_ir(&ir_l, &ir_r)));

    // Mono-L reference: only the L channel is processed; `r` of the pair is a
    // dummy never read under mono upmix, kept so both sides use the same
    // `conv_pair` stage shape.
    let mut mono_l = RtSwapHarness::new(host_rate, nam_rate).expect("mono-L harness");
    mono_l.push_load_model(Some(model_l_ref), None, 1.0, 1.0, nam_rate);
    mono_l.push_cabsim(Some(pair_from_ir(&ir_l, &ir_l)));

    let mut mono_r = RtSwapHarness::new(host_rate, nam_rate).expect("mono-R harness");
    mono_r.push_load_model(Some(model_r_ref), None, 1.0, 1.0, nam_rate);
    mono_r.push_cabsim(Some(pair_from_ir(&ir_r, &ir_r)));

    let mut sum_sq_err: f64 = 0.0;
    let mut sum_sq_signal: f64 = 0.0;
    let mut compared: usize = 0;
    let mut first_mismatch: Option<usize> = None;

    let mut offset = 0usize;
    let mut block_idx = 0usize;
    while offset + BLOCK <= signal_l.len() {
        let measuring = block_idx >= WARMUP_BLOCKS;

        // Unified stereo run.
        let mut in_l = signal_l[offset..offset + BLOCK].to_vec();
        let mut in_r = signal_r[offset..offset + BLOCK].to_vec();
        let stereo_n = stereo.run_callback(&mut in_l, &mut in_r, BLOCK);
        let n = stereo_n.min(BLOCK);
        let s_l: Vec<f32> = stereo.out_l().to_vec();
        let s_r: Vec<f32> = stereo.out_r().to_vec();

        // Isolated mono-L reference (feed L on both channels, read L).
        let mut m_in_l = signal_l[offset..offset + BLOCK].to_vec();
        let mut m_in_r = signal_l[offset..offset + BLOCK].to_vec();
        let mono_l_n = mono_l.run_callback(&mut m_in_l, &mut m_in_r, BLOCK);
        let m_l: Vec<f32> = mono_l.out_l().to_vec();

        // Isolated mono-R reference (feed R on both channels, read L).
        let mut m2_in_l = signal_r[offset..offset + BLOCK].to_vec();
        let mut m2_in_r = signal_r[offset..offset + BLOCK].to_vec();
        let mono_r_n = mono_r.run_callback(&mut m2_in_l, &mut m2_in_r, BLOCK);
        let m_r: Vec<f32> = mono_r.out_l().to_vec();

        assert_eq!(
            mono_l_n, stereo_n,
            "mono-L and stereo must produce equal n_pw"
        );
        assert_eq!(
            mono_r_n, stereo_n,
            "mono-R and stereo must produce equal n_pw"
        );
        assert!(m_l.len() >= n, "mono-L output too short");
        assert!(m_r.len() >= n, "mono-R output too short");

        if measuring {
            for i in 0..n {
                if s_l[i].to_bits() != m_l[i].to_bits() && first_mismatch.is_none() {
                    first_mismatch = Some(offset + i);
                }
                let el = (s_l[i] as f64 - m_l[i] as f64).abs();
                sum_sq_err += el * el;
                sum_sq_signal += s_l[i] as f64 * s_l[i] as f64;

                if s_r[i].to_bits() != m_r[i].to_bits() && first_mismatch.is_none() {
                    first_mismatch = Some(offset + i);
                }
                let er = (s_r[i] as f64 - m_r[i] as f64).abs();
                sum_sq_err += er * er;
                sum_sq_signal += s_r[i] as f64 * s_r[i] as f64;

                compared += 2;
            }
        }

        offset += BLOCK;
        block_idx += 1;
    }

    let mse = sum_sq_err / compared.max(1) as f64;
    let snr_db = if sum_sq_err == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (sum_sq_signal / sum_sq_err).log10()
    };

    FidelityResult {
        mse,
        snr_db,
        compared,
        first_mismatch,
    }
}

fn assert_bit_exact(res: &FidelityResult, label: &str) {
    assert!(
        res.compared > 0,
        "{label}: no samples compared (empty signal?)"
    );
    assert_eq!(
        res.mse, 0.0,
        "{label}: stereo vs dual-mono must be bit-exact (MSE == 0.0), got {} (first mismatch at sample {:?})",
        res.mse, res.first_mismatch
    );
    assert!(
        res.snr_db > 120.0,
        "{label}: SNR must exceed 120 dB, got {:.2} dB (first mismatch at sample {:?})",
        res.snr_db,
        res.first_mismatch
    );
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// White noise at 48 kHz / 48 kHz (resampler bypass): stereo == dual mono.
#[test]
fn stereo_fidelity_white_noise_bypass() {
    let (l, r) = white_noise_pair();
    let res = compare_stereo_vs_dual_mono(&l, &r, SAMPLE_RATE, SAMPLE_RATE);
    assert_bit_exact(&res, "white-noise@48k");
}

/// Chirp at 48 kHz / 48 kHz with cabsim: stereo == dual mono.
#[test]
fn stereo_fidelity_chirp_with_cabsim() {
    let (l, r) = chirp_pair();
    let res = compare_stereo_vs_dual_mono(&l, &r, SAMPLE_RATE, SAMPLE_RATE);
    assert_bit_exact(&res, "chirp+cabsim@48k");
}

/// Plucked-string "guitar" at 48 kHz / 48 kHz: stereo == dual mono.
#[test]
fn stereo_fidelity_plucked_guitar() {
    let (l, r) = plucked_string_pair();
    let res = compare_stereo_vs_dual_mono(&l, &r, SAMPLE_RATE, SAMPLE_RATE);
    assert_bit_exact(&res, "plucked-guitar@48k");
}

/// Chirp through the active resampler (44.1 kHz host → 48 kHz model): the
/// multirate path must also keep channels independent and bit-exact.
#[test]
fn stereo_fidelity_multirate_44100_to_48000() {
    let (l, r) = chirp_pair();
    let res = compare_stereo_vs_dual_mono(&l, &r, 44_100, SAMPLE_RATE);
    assert_bit_exact(&res, "chirp+cabsim multirate 44.1k→48k");
}
