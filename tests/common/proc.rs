// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

#![cfg(feature = "testing")]
#![allow(unused_imports)]

//! `/proc` process telemetry readers.
//!
//! Re-exported from the engine's `testing::host::proc` (single source of
//! truth); this shim keeps the `common::proc::*` import paths used by the
//! soak and endurance harnesses stable across the migration.

pub use neural_amp_modeler_rs::testing::host::proc::*;
