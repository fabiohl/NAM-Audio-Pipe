// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#![cfg(feature = "testing")]
#![allow(unused_imports)]

//! Per-thread PMU counters.
//!
//! Re-exported from the engine's `testing::host::pmu` (single source of truth);
//! this shim keeps the `common::pmu::PmuCounters` import path used by the RT
//! metric harness stable across the migration.

pub use neural_amp_modeler_rs::testing::host::pmu::*;
