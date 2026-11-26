// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Explicit quantum-budget thresholds shared by the PipeWire host.
//!
//! The host measures three distinct things against the same quantum period:
//!
//! - the DSP self-measurement that raises `dsp_overloads` (85 % of the period);
//! - the adaptive-compute FSM sample, which feeds the full period to the
//!   engine and lets its own hysteresis decide (the engine keeps the numerical
//!   thresholds, so this helper only supplies the budget);
//! - the off-RT deadline check that reports `[E2001 | PROCESSING_OVERLOAD]`
//!   (100 % of the period).
//!
//! They are *not* interchangeable metrics, so they keep separate names and
//! fractions here instead of three ad-hoc literals scattered across call
//! sites. This module deliberately does not change any numeric value.

/// Fraction of the quantum period above which the DSP self-measurement counts
/// a callback as an overload (85 %).
pub(crate) const DSP_OVERLOAD_FRACTION: f64 = 0.85;

/// Fraction of the quantum period above which the off-RT deadline check reports
/// a missed deadline (100 %).
pub(crate) const DEADLINE_EXCEEDED_FRACTION: f64 = 1.0;

/// Quantum period in seconds for `n_samples` at `rate`.
#[inline]
pub(crate) fn quantum_budget_secs(n_samples: u64, rate: u64) -> f64 {
    n_samples as f64 / rate as f64
}

/// Budget in seconds for the DSP self-measurement overload counter (85 %).
#[inline]
pub(crate) fn dsp_overload_budget_secs(n_samples: u64, rate: u64) -> f64 {
    quantum_budget_secs(n_samples, rate) * DSP_OVERLOAD_FRACTION
}

/// Full quantum budget in nanoseconds, as consumed by the adaptive-compute FSM.
#[inline]
pub(crate) fn adaptive_budget_ns(n_samples: u64, rate: u64) -> u64 {
    n_samples * 1_000_000_000 / rate
}

/// Full quantum budget in microseconds, as consumed by the adaptive-compute FSM.
#[inline]
pub(crate) fn adaptive_budget_us(n_samples: u64, rate: u64) -> u64 {
    adaptive_budget_ns(n_samples, rate) / 1000
}

/// Budget in microseconds for the off-RT deadline check (100 %).
#[inline]
pub(crate) fn deadline_budget_us(n_samples: u64, rate: u64) -> f64 {
    quantum_budget_secs(n_samples, rate) * 1_000_000.0 * DEADLINE_EXCEEDED_FRACTION
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsp_overload_keeps_the_85_percent_threshold() {
        // 64 frames at 48 kHz = 1333.33 µs; 85 % of it = 1133.33 µs.
        let budget = dsp_overload_budget_secs(64, 48_000);
        assert!((budget * 1_000_000.0 - 1_133.333_333).abs() < 1e-3);
        assert_eq!(DSP_OVERLOAD_FRACTION, 0.85);
    }

    #[test]
    fn adaptive_budget_keeps_the_full_period() {
        assert_eq!(adaptive_budget_ns(64, 48_000), 1_333_333);
        assert_eq!(adaptive_budget_us(64, 48_000), 1_333);
        assert_eq!(adaptive_budget_us(128, 48_000), 2_666);
    }

    #[test]
    fn deadline_budget_keeps_the_100_percent_threshold() {
        let budget = deadline_budget_us(64, 48_000);
        assert!((budget - 1_333.333_333).abs() < 1e-3);
        assert_eq!(DEADLINE_EXCEEDED_FRACTION, 1.0);
    }
}
