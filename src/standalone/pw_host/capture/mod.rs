// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Configuration of the PipeWire capture stream (`Audio/Sink`) — Virtual Sink that
//! receives audio from apps, applies the DSP chain and writes to `DspBridge`.

mod listeners;
mod setup;
pub(crate) mod state;

pub use setup::setup_capture_stream;
