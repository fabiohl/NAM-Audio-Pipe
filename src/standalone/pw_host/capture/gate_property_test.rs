// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Property-based and unit tests proving the invariant that the noise gate
//! with zeroed linear thresholds (`threshold_open == 0.0` and `threshold_close == 0.0`)
//! remains permanently in `GateState::Open` for any non-negative energy sequence.

use neural_amp_modeler_rs::dsp::gate::{DynamicHysteresis, GateParams, GateState};
use proptest::prelude::*;

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
