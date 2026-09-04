// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Property-based and unit tests proving the noise-gate invariants:
//!
//! 1. with zeroed linear thresholds (`threshold_open == 0.0` and
//!    `threshold_close == 0.0` — the `GateConfig::Off` mapping) the gate
//!    remains permanently in `GateState::Open` for any non-negative energy
//!    sequence;
//! 2. [`CaptureState::init`] resolves every `GateConfig` variant into the
//!    correct `gate_params` dBFS fields and linear/quadratic thresholds
//!    (Sprint 1, Tarefa 1.2 acceptance evidence).

use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams, GateState};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut;
use proptest::prelude::*;

use super::{CaptureState, GateConfig};

// ── GateConfig::Off invariant (zeroed linear thresholds never close) ──────

proptest! {
    #[test]
    fn gate_off_never_closes_for_any_nonneg_energy_sequence(
        energies in prop::collection::vec(0.0f32..=1.0f32, 1..500)
    ) {
        let params = GateParams::default();
        let mut h = DynamicHysteresis::new();
        for e in energies {
            h.update(e, /*threshold_open*/ 0.0, /*threshold_close*/ 0.0, &params, 128);
            prop_assert_eq!(h.state(), GateState::Open);
        }
    }
}

#[test]
fn gate_off_never_closes_for_sustained_zero_energy() {
    let params = GateParams::default();
    let mut h = DynamicHysteresis::new();
    // Exercise sustained absolute digital silence for far more than hold_frames (e.g. 10,000 blocks).
    for _ in 0..10_000 {
        h.update(0.0, 0.0, 0.0, &params, 128);
        assert_eq!(h.state(), GateState::Open);
    }
}

// ── CaptureState::init GateConfig resolution (Sprint 1, Tarefa 1.2) ───────

#[test]
fn capture_state_init_off_configures_zeroed_linear_thresholds() {
    let sys = SystemSnapshot::capture();
    let state = CaptureState::init(&sys, OversampleFactor::Off, GateConfig::Off);
    assert_eq!(state.threshold_open_sq, 0.0);
    assert_eq!(state.threshold_close_sq, 0.0);
}

#[test]
fn capture_state_init_threshold_configures_db_and_quadratic_thresholds() {
    let sys = SystemSnapshot::capture();
    let config = GateConfig::from_open_db(-60.0);
    let state = CaptureState::init(&sys, OversampleFactor::Off, config);

    let lut = get_gain_lut();
    let expected_open = lut.db_to_linear(-60.0);
    let expected_close = lut.db_to_linear(-70.0);
    assert_eq!(state.gate_params.threshold_open_db, -60.0);
    assert_eq!(state.gate_params.threshold_close_db, -70.0);
    assert!(
        (state.threshold_open_sq - expected_open * expected_open).abs() < 1e-12,
        "open square mismatch: {} vs {}",
        state.threshold_open_sq,
        expected_open * expected_open
    );
    assert!(
        (state.threshold_close_sq - expected_close * expected_close).abs() < 1e-12,
        "close square mismatch: {} vs {}",
        state.threshold_close_sq,
        expected_close * expected_close
    );
}

// ── Schmitt hysteresis over the whole valid threshold domain (Tarefa 2.2) ────

/// Drives `blocks` updates with constant energy using the same 512-sample
/// quantum granularity of the pipeline (per-update `n_samples`); the default
/// `hold_frames` (2048) is crossed after a few consecutive sub-close blocks.
fn drive_blocks(
    h: &mut DynamicHysteresis,
    energy: f32,
    open_sq: f32,
    close_sq: f32,
    blocks: usize,
) {
    let params = GateParams::default();
    for _ in 0..blocks {
        h.update(energy, open_sq, close_sq, &params, 512);
    }
}

proptest! {
    /// No valid threshold in the accepted domain (`[-96.0, -20.0] dBFS`, with
    /// the 10 dB Schmitt close derived exactly as [`GateConfig::from_open_db`])
    /// may panic, yield a non-finite/out-of-range multiplier, or wedge the FSM:
    /// after a sustained sub-close energy run the gate must reach `Closed`, and
    /// a sustained full-scale run must reopen it. Within the non-degenerate
    /// hysteresis band, mid-band energy must never flip an open/closed gate
    /// (no chatter / discontinuity).
    #[test]
    fn any_valid_threshold_domain_preserves_fsm_continuity(
        open_db in -96.0f32..=-20.0,
        energy_walk in prop::collection::vec(0.0f32..=1.0f32, 1..200),
    ) {
        let (threshold_open_db, threshold_close_db) =
            match GateConfig::from_open_db(open_db) {
                GateConfig::Threshold {
                    threshold_open_db,
                    threshold_close_db,
                } => (threshold_open_db, threshold_close_db),
                // Impossible by construction: from_open_db always yields Threshold.
                GateConfig::Off => unreachable!(),
            };
        let lut = get_gain_lut();
        let open_lin = lut.db_to_linear(threshold_open_db);
        let close_lin = lut.db_to_linear(threshold_close_db);
        let open_sq = open_lin * open_lin;
        let close_sq = close_lin * close_lin;

        let params = GateParams::default();
        let mut h = DynamicHysteresis::new();
        for &e in &energy_walk {
            h.update(e, open_sq, close_sq, &params, 512);
            let m = h.multiplier();
            prop_assert!(
                m.is_finite(),
                "non-finite multiplier {} after energy {} (open_db {})",
                m,
                e,
                open_db
            );
            prop_assert!(
                (0.0..=1.0).contains(&m),
                "multiplier {} out of [0,1] after energy {} (open_db {})",
                m,
                e,
                open_db
            );
        }

        // Sustained deep silence (0.0 < close_sq for every valid threshold)
        // must close the gate regardless of the previous random walk.
        drive_blocks(&mut h, 0.0, open_sq, close_sq, 16);
        prop_assert_eq!(
            h.state(),
            GateState::Closed,
            "gate must reach Closed under sustained silence (open_db {})",
            open_db
        );

        // Sustained full-scale energy (1.0 >= open_sq for every valid
        // threshold) must reopen it — a stuck-Closed wedge is a regression.
        drive_blocks(&mut h, 1.0, open_sq, close_sq, 16);
        prop_assert_eq!(
            h.state(),
            GateState::Open,
            "gate must reopen under full-scale energy (open_db {})",
            open_db
        );

        // Hysteresis continuity: inside the non-degenerate Schmitt band
        // (close < mid < open), mid-band energy must never flip the FSM.
        if close_sq < open_sq {
            let mid = 0.5 * (open_sq + close_sq);
            drive_blocks(&mut h, mid, open_sq, close_sq, 16);
            prop_assert_eq!(
                h.state(),
                GateState::Open,
                "mid-band energy closed an open gate? (open_db {})",
                open_db
            );
            drive_blocks(&mut h, 0.0, open_sq, close_sq, 16);
            prop_assert_eq!(
                h.state(),
                GateState::Closed,
                "gate must re-close under sustained silence (open_db {})",
                open_db
            );
            drive_blocks(&mut h, mid, open_sq, close_sq, 16);
            prop_assert_eq!(
                h.state(),
                GateState::Closed,
                "mid-band energy opened a closed gate? (open_db {})",
                open_db
            );
        }
    }
}
