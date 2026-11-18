// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Standalone adapter for the shared musical noise gate.
//!
//! The gate implementation (quadratic downward expander, `libm`-free
//! `fast_log10`/`fast_pow10`, boxed scratch buffers and `#[cold] prepare`) is
//! the single upstream copy in `neural_amp_modeler_rs::dsp::musical_gate`.
//! This module only re-exports it under the historical `dsp::musical_gate`
//! path so the RT pipeline, capture state and CLI keep their public surface.

pub use neural_amp_modeler_rs::dsp::musical_gate::*;
