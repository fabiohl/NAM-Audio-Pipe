// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `nam_audio_pipe` — Low-latency PipeWire audio host and standalone engine.
//!
//! Provides real-time PipeWire node setup, lock-free SPSC channel communication,
//! automatic resampler and CabSim management, and asynchronous WAV recording.

pub mod receipt;
pub mod recording;
pub mod standalone;

/// Real-time heap-allocation audit allocator (feature `heap-audit`).
///
/// Intercepts every heap request so the RT-safety heap-audit unit tests
/// (`cabsim_swap` swap transitions) can prove zero allocations on
/// the audio-thread paths via [`neural_amp_modeler_rs::common::alloc_audit`].
#[cfg(feature = "heap-audit")]
#[global_allocator]
static GLOBAL: neural_amp_modeler_rs::common::alloc_audit::CountingAllocator =
    neural_amp_modeler_rs::common::alloc_audit::CountingAllocator;
