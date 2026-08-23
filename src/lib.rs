// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `nam_audio_pipe` — Low-latency PipeWire audio host and standalone engine.
//!
//! Provides real-time PipeWire node setup, lock-free SPSC channel communication,
//! automatic resampler and CabSim management, and asynchronous WAV recording.

pub mod recording;
pub mod standalone;
