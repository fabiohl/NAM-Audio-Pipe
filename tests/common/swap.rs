// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Shared model/signal helpers for swap-stress and soak integration tests.
//!
//! Keeps the deterministic Linear tags, fixture-backed WaveNet/LSTM builders,
//! synthetic IR factory and stereo test signal in one place so
//! `tests/swap_stress.rs`, `tests/soak_extended.rs` and `tests/endurance.rs`
//! never drift apart. Also owns the shared swap-batch cadence and the
//! fail-closed Linear validation window used by both the accelerated-timeline
//! soak and the real wall-clock endurance.

#[cfg(feature = "testing")]
use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use neural_amp_modeler_rs::dsp::cabsim::adapter::{CabSimAdapter, CabSimPair};
use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
#[cfg(feature = "testing")]
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};
use neural_amp_modeler_rs::loader::dispatcher::build_model;
use neural_amp_modeler_rs::loader::nam_json::{NamModelData, parse_nam_json};
use neural_amp_modeler_rs::models::{NamModel, StaticModel};

use std::sync::OnceLock;

pub const BLOCK: usize = 64;
pub const SAMPLE_RATE: u32 = 48_000;
pub const PARTITION: usize = 512;
pub const IR_LEN: usize = 4096;

/// Linear RF=4, bias=true. Steady-state gain on the FIR taps:
/// `(0.125+0.25+0.5+1.0)·x + 0.1 = 1.875·x + 0.1`.
pub const LINEAR_A_JSON: &str = r#"{
    "version": "0.7.0",
    "architecture": "Linear",
    "sample_rate": 48000.0,
    "config": { "receptive_field": 4, "bias": true },
    "weights": [0.125, 0.25, 0.5, 1.0, 0.1]
}"#;

/// Linear RF=4, bias=false. Steady-state gain:
/// `(-0.28941+0.07963-0.09878+0.14898)·x = -0.1596·x` (inverting, clearly
/// distinguishable from `LINEAR_A` in sign and scale).
pub const LINEAR_B_JSON: &str = r#"{
    "version": "0.5.4",
    "architecture": "Linear",
    "config": { "receptive_field": 4, "bias": false },
    "weights": [-0.28941273269402096, 0.07962766854305478, -0.09877673902492994, 0.14897901952289913],
    "sample_rate": 48000
}"#;

/// WaveNet Nano / standard fixture: representative topology for soak / test harnesses.
pub const WAVENET_JSON: &str = include_str!("../fixtures/models/wavenet_a1_standard.nam");

/// Full WaveNet A1 fixture (`wavenet_a1_standard.nam`): representative
/// production-grade WaveNet topology for the RT deadline/jitter gates.
pub const WAVENET_A1_JSON: &str = include_str!("../fixtures/models/wavenet_a1_standard.nam");

/// Full WaveNet A2 fixture (`a2_example.nam`): production-grade A2
/// topology for the RT deadline/jitter gates.
pub const WAVENET_A2_JSON: &str = include_str!("../fixtures/models/a2_example.nam");

/// LSTM fixture: recurrent model for soak coverage.
pub const LSTM_JSON: &str = include_str!("../fixtures/models/lstm.nam");

fn linear_data(json: &str) -> NamModelData {
    parse_nam_json(json).expect("parse linear model")
}

fn model_from(data: &NamModelData) -> Box<StaticModel> {
    let mut model = build_model(data).expect("build model");
    model.prewarm(64);
    model
}

/// Linear tag model A (positive steady-state gain).
pub fn linear_a() -> Box<StaticModel> {
    model_from(&linear_data(LINEAR_A_JSON))
}

/// Linear tag model B (negative / inverting steady-state gain).
pub fn linear_b() -> Box<StaticModel> {
    model_from(&linear_data(LINEAR_B_JSON))
}

fn cached_wavenet_data() -> &'static NamModelData {
    static DATA: OnceLock<NamModelData> = OnceLock::new();
    DATA.get_or_init(|| parse_nam_json(WAVENET_JSON).expect("parse WaveNet fixture"))
}

fn cached_lstm_data() -> &'static NamModelData {
    static DATA: OnceLock<NamModelData> = OnceLock::new();
    DATA.get_or_init(|| parse_nam_json(LSTM_JSON).expect("parse LSTM fixture"))
}

/// WaveNet Nano fixture model (off-RT build, safe for test setup).
pub fn wavenet_model() -> Box<StaticModel> {
    model_from(cached_wavenet_data())
}

/// LSTM 1×10 fixture model (off-RT build, safe for test setup).
pub fn lstm_model() -> Box<StaticModel> {
    model_from(cached_lstm_data())
}

fn cached_a1_data() -> &'static NamModelData {
    static DATA: OnceLock<NamModelData> = OnceLock::new();
    DATA.get_or_init(|| parse_nam_json(WAVENET_A1_JSON).expect("parse WaveNet A1 fixture"))
}

fn cached_a2_data() -> &'static NamModelData {
    static DATA: OnceLock<NamModelData> = OnceLock::new();
    DATA.get_or_init(|| parse_nam_json(WAVENET_A2_JSON).expect("parse WaveNet A2 fixture"))
}

/// Full WaveNet A1 model (off-RT build, safe for test setup).
pub fn wavenet_a1() -> Box<StaticModel> {
    model_from(cached_a1_data())
}

/// Full WaveNet A2 model (off-RT build, safe for test setup).
pub fn wavenet_a2() -> Box<StaticModel> {
    model_from(cached_a2_data())
}

/// Builds a synthetic stereo CabSim IR pair (different L/R spectra).
pub fn cabsim_pair() -> Box<CabSimPair> {
    fn synth_ir(len: usize, freq: f32, decay: f32) -> Vec<f32> {
        let sr = SAMPLE_RATE as f32;
        (0..len)
            .map(|i| {
                let t = i as f32 / sr;
                (std::f32::consts::TAU * freq * t).sin() * (-decay * t).exp()
            })
            .collect()
    }

    let ir_l = synth_ir(IR_LEN, 880.0, 14.0);
    let ir_r = synth_ir(IR_LEN, 1760.0, 22.0);
    let mk = |ir: &[f32]| {
        CabSimAdapter::new(Box::new(ConvEngine::new(ir, PARTITION).expect("conv")))
            .expect("adapter")
    };
    Box::new(CabSimPair {
        l: Box::new(mk(&ir_l)),
        r: Box::new(mk(&ir_r)),
        sample_rate: SAMPLE_RATE,
    })
}

/// Deterministic stereo test signal (L != R so the mono detector stays open).
pub fn test_signal_blocks(total_blocks: usize) -> (Vec<f32>, Vec<f32>) {
    let n = total_blocks * BLOCK;
    let mut seed_l: u64 = 0x243F_6A88_85A3_08D3;
    let mut seed_r: u64 = 0x1319_8A2E_0370_7344;
    let next = |seed: &mut u64| -> f32 {
        let mut x = *seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *seed = x;
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (r >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    };
    let l: Vec<f32> = (0..n).map(|_| 0.35 * next(&mut seed_l)).collect();
    let r: Vec<f32> = (0..n).map(|_| 0.35 * next(&mut seed_r)).collect();
    (l, r)
}

/// Drain budget for a validation window: the command burst must be fully
/// absorbed within this many callbacks or the window fails hard.
#[cfg(feature = "testing")]
pub const VALIDATION_DRAIN_BUDGET: usize = 64;

/// Settling barrier for a validation window (count of DC blocks executed after
/// the command drain, before the window tips are sampled).
///
/// Covers the slowest propagation paths a swap batch can leave in flight — the
/// wet tail of a removed CabSim convolution queue (one `PARTITION` = 512
/// samples), the oversampled stream of an `X4`→`Off` engine exchange
/// (`4×BLOCK` per exchange) and the model receptive fields themselves — with a
/// large margin. The unified RT state is replaced at the swap quantum (old
/// engines are disposed, never drained into the output), which is why a fixed,
/// guaranteed count is sufficient: the window only needs the *new* pipeline —
/// Linear A/B, no CabSim, OS Off, unity gain — to reach and hold its
/// steady state, which it does within a single block once that state is
/// installed. 48 blocks × 64 samples ≈ 64 ms of tail headroom (3.07k
/// samples; 1.33 ms per block) at an RT cost of 48 trivial DC callbacks per
/// window.
#[cfg(feature = "testing")]
pub const VALIDATION_SETTLE_BLOCKS: usize = 48;

/// Frames sampled from the output tail for the oracle (and for the
/// stationarity snapshots below). `BLOCK - 4 .. BLOCK` covers the quantum's
/// fresh tip and straddles the previous callback boundary — the exact window
/// the old drain-race surfaced at (sample 60 with `BLOCK = 64`).
#[cfg(feature = "testing")]
pub const VALIDATION_TIP_SAMPLES: usize = 4;

/// Left-channel DC anchor for the validation window (see the oracle table in
/// [`validate_linear_window`]).
#[cfg(feature = "testing")]
pub const VALIDATION_DC_L: f32 = 0.3;

/// Right-channel DC anchor for the validation window — deliberately distinct
/// from [`VALIDATION_DC_L`]: a mono (identical L/R) window feed would engage
/// the production dual-mono fold and reduce the R oracle to a tautology
/// (R == L), certifying the fold instead of the independent right chain.
#[cfg(feature = "testing")]
pub const VALIDATION_DC_R: f32 = 0.21;

/// Per-sample gain of the validation right model (`LINEAR_B_JSON`): the sum of
/// its FIR taps (`-0.28941+0.07963-0.09878+0.14898`), folding to the R oracle
/// `VALIDATION_DC_R · gain`.
#[cfg(feature = "testing")]
pub const VALIDATION_EXPECTED_R_GAIN: f32 = -0.1596;

/// Swap cadence shared by the soak/endurance harnesses: a batch of
/// model/cabsim/OS/gain commands every N blocks.
#[cfg(feature = "testing")]
pub const SWAP_INTERVAL: usize = 20;

/// Applies a mixed swap batch: model, CabSim, oversampling and gain.
///
/// Shared cadence between the accelerated soak and the real endurance:
/// a soak cadence/kind change lands here exactly once.
#[cfg(feature = "testing")]
pub fn apply_swap_batch(h: &mut RtSwapHarness, block: usize) {
    let cycle = block / SWAP_INTERVAL;

    // Model rotation: Linear A/B → WaveNet → LSTM → Linear B/A → ...
    let model = match cycle % 4 {
        0 => linear_a(),
        1 => wavenet_model(),
        2 => lstm_model(),
        _ => linear_b(),
    };
    let model_r = match cycle % 4 {
        0 => linear_b(),
        1 => wavenet_model(),
        2 => lstm_model(),
        _ => linear_a(),
    };
    h.push_load_model(Some(model), Some(model_r), 1.0, 1.0, SAMPLE_RATE);

    // CabSim: install every 2nd batch, clear every 3rd batch.
    if cycle.is_multiple_of(2) {
        h.push_cabsim(Some(cabsim_pair()));
    } else if cycle.is_multiple_of(3) {
        h.push_cabsim(None);
    }

    // Oversampling: cycle through Off / X2 / X4.
    let os_factor = match cycle % 3 {
        0 => OversampleFactor::Off,
        1 => OversampleFactor::X2,
        _ => OversampleFactor::X4,
    };
    let os_max = BLOCK * 4;
    if let (Ok(l), Ok(r)) = (
        OversampleEngine::new(os_factor, os_max),
        OversampleEngine::new(os_factor, os_max),
    ) {
        h.push_os_pair(l, r);
    }

    // Continuous gain variation.
    let mult = 0.5 + 0.01 * ((cycle % 100) as f32);
    h.push_input_gain(mult);
    h.push_output_gain(mult);
}

/// Installs Linear A/B with unity gain and DC input, then verifies polarity
/// and gain symmetry (L positive, R negative, symmetric scaling).
///
/// Returns `false` when the pipeline produced no frames yet (a zero-frame
/// sentinel — the caller retries within a bounded budget) or when the tail is
/// still transient — the stationarity gate below. It never silently
/// returns a pass: after the drain budget, a persistent zero-frame or
/// never-stationary state fails hard in the caller.
///
/// The validation must also neutralize any active CabSim and oversampling
/// engine (pushed by the swap batch that shares this block), otherwise the
/// IR convolution and/or resampled processing would distort the steady-state
/// Linear gain tags.
///
/// Accommodation barrier (drain-race hardening): the steady-state oracle only
/// samples after (1) the full command drain, (2) a guaranteed
/// [`VALIDATION_SETTLE_BLOCKS`] settling window sized to the slowest
/// propagation path the shared swap batches can leave in flight (CabSim
/// partition queue tail, `X4`→`Off` oversampled stream, model receptive
/// fields), and (3) a **stationarity gate** — the output tail must repeat
/// bit-exactly across two consecutive DC callbacks before the oracle runs. A
/// pipeline whose tail is still propagating a removed engine's transient (or
/// whose stream is being rewritten by an external producer) is classified as
/// *not settled* (`false` → bounded retry → hard failure), never validated
/// against the oracle mid-transient and never granted a loosened pass.
#[cfg(feature = "testing")]
pub fn validate_linear_window(h: &mut RtSwapHarness) -> bool {
    // Distinct DC anchors per channel: identical L/R would engage the
    // production dual-mono mono-detector (per-block fold of R over L), making
    // the R oracle certify the fold instead of the independent right chain.
    // Distinct anchors keep the detector open exactly as the real stereo
    // stream does, and both chains stay certified independently.
    let dc_l = VALIDATION_DC_L; // 0.3: L oracle `1.875·dc + 0.1 = 0.6625`
    let dc_r = VALIDATION_DC_R; // 0.21: R oracle `-0.1596·dc = -0.0335160`
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    h.push_cabsim(None);
    h.push_os_pair(
        OversampleEngine::new(OversampleFactor::Off, BLOCK * 4).expect("OS Off"),
        OversampleEngine::new(OversampleFactor::Off, BLOCK * 4).expect("OS Off"),
    );
    h.push_output_gain(1.0);
    h.push_input_gain(1.0);

    let mut drained = 0usize;
    while h.commands_pending() && drained < VALIDATION_DRAIN_BUDGET {
        let mut l = [dc_l; BLOCK];
        let mut r = [dc_r; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
        drained += 1;
    }
    assert!(
        !h.commands_pending(),
        "validation commands not drained within {VALIDATION_DRAIN_BUDGET} callbacks"
    );
    for _ in 0..VALIDATION_SETTLE_BLOCKS {
        let mut l = [dc_l; BLOCK];
        let mut r = [dc_r; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }

    let n = h.current_n_pw();
    if n == 0 || n < VALIDATION_TIP_SAMPLES {
        // Zero-frame sentinel: the pipeline is mid-transition (e.g. resampler
        // swap pending). Documented skip — the caller retries within the
        // bounded budget; never a silent pass.
        return false;
    }
    let tip = n.saturating_sub(VALIDATION_TIP_SAMPLES);
    let expected_l = 1.875 * dc_l + 0.1;
    let expected_r = VALIDATION_EXPECTED_R_GAIN * dc_r;

    // Stationarity gate: the output tip must repeat **bit-exactly** across two
    // consecutive settled callbacks before the oracle samples it. Any tail
    // still propagating a removed engine's transient (or a stream being
    // rewritten by an external producer) fails the repeat, classifying the
    // window as not-yet-settled — the caller retries within its bounded budget
    // and a persistent non-stationary state fails hard. Never an oracle sample
    // mid-transient, never a loosened pass. The tips are snapshotted into
    // owned slots (shallow borrows of `h` end inside each iteration).
    let mut tip_l = [[0.0f32; VALIDATION_TIP_SAMPLES]; 2];
    let mut tip_r = [[0.0f32; VALIDATION_TIP_SAMPLES]; 2];
    for (slot, (slot_l, slot_r)) in tip_l.iter_mut().zip(tip_r.iter_mut()).enumerate() {
        if slot > 0 {
            // A callbacks-skipped quantum must not confirm "steady": a skipped
            // write keeps the previous bridge content bit-identical, which a
            // naive snapshot would classify as stability.
            let frames_before = h.frame_count();
            let mut l = [dc_l; BLOCK];
            let mut r = [dc_r; BLOCK];
            let written = h.run_callback(&mut l, &mut r, BLOCK);
            let n2 = h.current_n_pw();
            if n2 != n || h.frame_count() == frames_before || written == 0 || written != n {
                // The quantum itself changed mid-window (renegotiation,
                // pipeline transition or a skipped write): not settled —
                // bounded retry.
                return false;
            }
        }
        let out_l = h.out_l();
        let out_r = h.out_r();
        if out_l.len() < n || out_r.len() < n {
            // Output shrank under the window: not settled — bounded retry.
            return false;
        }
        slot_l.copy_from_slice(&out_l[tip..n]);
        slot_r.copy_from_slice(&out_r[tip..n]);
    }
    if tip_l[0] != tip_l[1] || tip_r[0] != tip_r[1] {
        eprintln!(
            "validating window not settled — {}",
            validation_state_debug(h)
        );
        return false;
    }

    for (i, (l_sample, r_sample)) in tip_l[0].iter().zip(tip_r[0].iter()).enumerate() {
        if ((l_sample - expected_l).abs() >= 1e-2) || ((r_sample - expected_r).abs() >= 1e-2) {
            // Full-tail diagnostic dump: the whole out buffer that the oracle
            // rejected — a failing run must carry the discriminating waveform
            // (values + magnitudes) alongside the state snapshot.
            let out_l_full = h.out_l().to_vec();
            let out_r_full = h.out_r().to_vec();
            panic!(
                "soak validation: L at sample {} = {l_sample} expected {expected_l}; \
                 R tip {r_sample} expected {expected_r}; out_l={out_l_full:?}; \
                 out_r={out_r_full:?}; {}",
                tip + i,
                validation_state_debug(h)
            );
        }
    }
    true
}

/// One-line RT pipeline state snapshot for failing window diagnostics:
/// mono-fold state, gain chain, cabsim presence, OS factor request and
/// structural-swap counters — the discriminating state that a drained-but-
/// wrong window exhibits.
#[cfg(feature = "testing")]
pub fn validation_state_debug(h: &RtSwapHarness) -> String {
    let st = h.rt_status();
    let label = |m: Option<&StaticModel>| match m {
        Some(m) => format!(
            "{} rf={} fp={}",
            m.class_label(),
            m.receptive_field(),
            m.memory_footprint()
        ),
        None => "none".to_string(),
    };
    format!(
        "state: mono={} in_gain={} out_gain={} cabsim={} os_factor_req={} \
         model_l=[{}] model_r=[{}] deferred={} superseded={}",
        h.process_mono(),
        h.input_gain_mult(),
        h.output_gain_mult(),
        h.active_cabsim().is_some(),
        st.requested_os_factor
            .load(std::sync::atomic::Ordering::Relaxed),
        label(h.active_model_l()),
        label(h.active_model_r()),
        st.structural_deferred_total
            .load(std::sync::atomic::Ordering::Relaxed),
        st.structural_superseded_total
            .load(std::sync::atomic::Ordering::Relaxed),
    )
}
