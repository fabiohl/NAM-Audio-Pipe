// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![cfg(feature = "testing")]

//! Extended soak and heap-audit harness for the PipeWire host (T6.4 / G-RB-002).
//!
//! Two `#[ignore]`d tests run by Phase 1 of `utils/tests-long.sh`:
//!
//! - `test_soak_100k_multichannel_swaps` — 100 000 continuous audio blocks
//!   (~2.2 min of continuous audio at BLOCK=64 / 48 kHz) with thousands of
//!   concurrent swaps across WaveNet, LSTM and Linear models, CabSim IRs,
//!   oversampling factors and gain variations. Validates continuous integrity
//!   (no undue silence, no channel inversion, no gain asymmetry) during
//!   periodic Linear-model windows.
//!
//! - `test_soak_rss_memory_stability` — captures resident memory (VmRSS) at
//!   block 1 000 and block 100 000 of the same soak and asserts the delta is
//!   below the OS page-margin threshold (zero memory drift / leak).

mod common;

use nam_audio_pipe::standalone::pw_host::RtSwapHarness;
use neural_amp_modeler_rs::dsp::oversample::{OversampleEngine, OversampleFactor};

use common::swap::*;

/// Total continuous audio blocks for the soak (≈133 s at 48 kHz / BLOCK=64).
const TOTAL_BLOCKS: usize = 100_000;

/// Swap cadence: a batch of model/cabsim/OS/gain commands every N blocks.
const SWAP_INTERVAL: usize = 20;

/// Validation window: every N blocks, install Linear A/B and verify polarity.
const VALIDATION_INTERVAL: usize = 500;

/// Maximum allowed blocks of undue silence during the soak (gate-closed
/// transitions and resampler-pending skips are legitimate).
const MAX_UNDUE_SILENCE: usize = 200;

/// Maximum allowed RSS drift (KiB) between block 1 000 and block 100 000.
/// A single Linux page is 4 KiB; 64 KiB (16 pages) is a generous margin for
/// allocator noise while still catching real leaks.
const MAX_RSS_DRIFT_KB: usize = 64;

/// Reads the current process resident set size in KiB from `/proc/self/status`.
fn read_rss_kb() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
        }
    }
    0
}

/// Runs the soak over `sig_l`/`sig_r` for the `[start, start + blocks)`
/// segment of the audio timeline, performing periodic swap batches and (when
/// `validate` is true) Linear-model validation windows. The signal buffers
/// must hold at least `(start + blocks) * BLOCK` samples per channel.
///
/// Returns the number of undue-silence blocks observed.
fn run_soak_blocks(
    h: &mut RtSwapHarness,
    start: usize,
    blocks: usize,
    validate: bool,
    sig_l: &[f32],
    sig_r: &[f32],
) -> usize {
    let mut undue_silence = 0usize;

    for local in 0..blocks {
        let block = start + local;
        if block.is_multiple_of(SWAP_INTERVAL) {
            apply_swap_batch(h, block);
        }

        if validate && block > 0 && block.is_multiple_of(VALIDATION_INTERVAL) {
            validate_linear_window(h);
        }

        // Production main loop drains the GC cascade periodically; the soak
        // must mimic that or retired models accumulate in the overflow ring
        // (whose overwrite policy is a documented bounded leak).
        if block.is_multiple_of(50) {
            h.consume_gc();
        }

        let mut in_l = [0.0f32; BLOCK];
        let mut in_r = [0.0f32; BLOCK];
        in_l.copy_from_slice(&sig_l[block * BLOCK..(block + 1) * BLOCK]);
        in_r.copy_from_slice(&sig_r[block * BLOCK..(block + 1) * BLOCK]);
        h.run_callback(&mut in_l, &mut in_r, BLOCK);

        if h.frame_count() == 0 {
            continue;
        }

        let out_l = h.out_l();
        let out_r = h.out_r();
        if !out_l.is_empty()
            && in_l.iter().any(|&s| s != 0.0)
            && out_l.iter().all(|&s| s == 0.0)
            && out_r.iter().all(|&s| s == 0.0)
        {
            undue_silence += 1;
        }
    }

    undue_silence
}

/// Applies a mixed swap batch: model, CabSim, oversampling and gain.
fn apply_swap_batch(h: &mut RtSwapHarness, block: usize) {
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
/// The validation must also neutralize any active CabSim and oversampling
/// engine (pushed by the swap batch that shares this block), otherwise the
/// IR convolution and/or resampled processing would distort the steady-state
/// Linear gain tags.
fn validate_linear_window(h: &mut RtSwapHarness) {
    h.push_load_model(Some(linear_a()), Some(linear_b()), 1.0, 1.0, SAMPLE_RATE);
    h.push_cabsim(None);
    h.push_os_pair(
        OversampleEngine::new(OversampleFactor::Off, BLOCK * 4).expect("OS Off"),
        OversampleEngine::new(OversampleFactor::Off, BLOCK * 4).expect("OS Off"),
    );
    h.push_output_gain(1.0);
    h.push_input_gain(1.0);

    let dc = 0.3f32;
    let mut drained = 0usize;
    while h.commands_pending() && drained < 32 {
        let mut l = [dc; BLOCK];
        let mut r = [dc; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
        drained += 1;
    }
    assert!(
        !h.commands_pending(),
        "validation commands not drained within 32 callbacks"
    );
    for _ in 0..8 {
        let mut l = [dc; BLOCK];
        let mut r = [dc; BLOCK];
        h.run_callback(&mut l, &mut r, BLOCK);
    }

    let n = h.current_n_pw();
    if n == 0 {
        return;
    }
    let out_l = h.out_l().to_vec();
    let out_r = h.out_r().to_vec();
    if out_l.is_empty() || out_r.is_empty() {
        return;
    }

    let expected_l = 1.875 * dc + 0.1;
    let expected_r = -0.1596 * dc;
    let idx = n.saturating_sub(4);
    for i in idx..n {
        assert!(
            (out_l[i] - expected_l).abs() < 1e-2,
            "soak validation: L at sample {i} = {} expected {expected_l}",
            out_l[i]
        );
        assert!(
            (out_r[i] - expected_r).abs() < 1e-2,
            "soak validation: R at sample {i} = {} expected {expected_r}",
            out_r[i]
        );
    }
}

/// 100 000 continuous audio blocks with thousands of concurrent swaps across
/// WaveNet, LSTM and Linear models, CabSim IRs, oversampling factors and gain
/// variations. Validates continuous integrity (no undue silence, no channel
/// inversion, no gain asymmetry) during periodic Linear-model windows.
#[test]
#[ignore]
fn test_soak_100k_multichannel_swaps() {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    let (sig_l, sig_r) = test_signal_blocks(TOTAL_BLOCKS);

    let undue_silence = run_soak_blocks(&mut h, 0, TOTAL_BLOCKS, true, &sig_l, &sig_r);

    assert!(
        undue_silence <= MAX_UNDUE_SILENCE,
        "soak observed {undue_silence} undue-silence blocks (limit {MAX_UNDUE_SILENCE})"
    );
}

/// Captures resident memory (VmRSS) at block 1 000 and block 100 000 of the
/// same soak and asserts the post-warmup drift is strictly below the OS
/// page-margin threshold (zero memory leak).
///
/// The full-length signal is pre-allocated once and kept alive across both
/// measurements so its heap footprint is part of the baseline (the delta
/// isolates the soak's own allocation behavior).
#[test]
#[ignore]
fn test_soak_rss_memory_stability() {
    let mut h = RtSwapHarness::new(SAMPLE_RATE, SAMPLE_RATE).expect("harness");
    let (sig_l, sig_r) = test_signal_blocks(TOTAL_BLOCKS);

    let _ = run_soak_blocks(&mut h, 0, 1_000, false, &sig_l, &sig_r);
    let rss_1k = read_rss_kb();
    assert!(rss_1k > 0, "VmRSS unavailable on this platform");

    let _ = run_soak_blocks(&mut h, 1_000, TOTAL_BLOCKS - 1_000, false, &sig_l, &sig_r);
    let rss_final = read_rss_kb();

    let drift = rss_final.saturating_sub(rss_1k);
    assert!(
        drift < MAX_RSS_DRIFT_KB,
        "RSS drift of {drift} KiB between block 1 000 ({rss_1k} KiB) and block {TOTAL_BLOCKS} \
         ({rss_final} KiB) exceeds {MAX_RSS_DRIFT_KB} KiB margin — possible memory leak"
    );
}
