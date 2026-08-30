// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! DSP audio processing core using `pipewire-rs`.
//!
//! This is the "heart" of NAM-Audio-Pipe: the module that processes audio in
//! real time. It receives raw audio samples from PipeWire (the Linux sound
//! server), passes them through the "neural engine", and delivers the processed
//! final result to the hardware via a dual-stream architecture.
//!
//! ## Dual-Stream Architecture with DspBridge
//!
//! PipeWire copies buffers to the monitor port **before** calling `process()`.
//! Therefore, in-place modifications on a single `Audio/Sink` stream would be invisible
//! to the hardware. The solution uses two streams:
//!
//! 1. **Capture stream** (`Audio/Sink`, `Direction::Input`) — acts as a Virtual Sink
//!    that receives audio from apps, applies the DSP chain (gain + neural inference)
//!    and writes the result to `DspBridge`.
//! 2. **Playback stream** (`Stream/Output/Audio`, `Direction::Output`) — acts as a
//!    playback client that reads from `DspBridge` and delivers to hardware.
//!
//! The `DspBridge` is a `#[repr(align(128))]` buffer shared between the two
//! closures via raw pointer, with lock-free synchronization via `Ordering::Release/Acquire`
//! and an atomic generation counter.
//!
//! ## Intra-cycle ordering guarantee (0 quantum extra latency)
//!
//! Both streams share `node.group = "nam-audio-pipe-dsp"` and
//! `node.link-group = "nam-audio-pipe-link-group"`, ensuring they are scheduled by the
//! same driver in the same PipeWire quantum. Within the driver's `target_list`,
//! nodes are processed in **FIFO registration order**. The capture stream is
//! created first (`setup_capture_stream`), therefore it always processes before the
//! playback stream (`setup_playback_stream`) within the same cycle:
//!
//! 1. Capture `process()` runs → DSP pipeline → writes `DspBridge` with Release
//! 2. Playback `process()` runs → reads `DspBridge` with Acquire → delivers to hardware
//!
//! The `PRIORITY_DRIVER = 2000` on the capture node further ensures it leads the
//! group. The PipeWire library version is reported at startup via
//! `pw_get_library_version()` in the `SystemSnapshot` diagnostic bundle.
//!
//! ## Dual-stream validation via `pw_stream::time()`
//!
//! The intra-cycle ordering guarantee was empirically validated using
//! `pw_stream::time()` — the PipeWire API that retrieves the stream clock
//! time in nanoseconds. By logging `pw_stream::time()` at both the capture
//! and playback `process()` entry points across multiple test sessions,
//! we confirmed that the capture stream consistently executes before the
//! playback stream within the same quantum, with no extra cycle of latency.
//! This confirms the 0-quantum-extra-latency property of the dual-stream
//! topology with `node.group` scheduling.
//!
//! Ref: [PipeWire Graph Scheduling](https://docs.pipewire.org/page_scheduling.html)
//!
//! ## Absolute rules of this module (why are they so strict?)
//!
//! In the `process()` callback (the function called hundreds of times per second by PipeWire):
//! - **Zero heap allocation** — we never request new memory from the system during processing.
//! - **Zero I/O** — we never write to the terminal or files; status is reported via atomic flags.
//! - **Zero mutexes** — we never lock/wait for other threads.
//!
//! These rules exist because any pause, no matter how small, would cause clicks and glitches in
//! the audio — unacceptable for a musician playing live.
//!
//! ## Processing flow (Capture callback)
//!
//! The `process()` callback follows this sequence for each audio block:
//! 1. **Noise Gate and Input Gain** — Evaluates signal energy and applies the initial gain (pre-DSP).
//! 2. `NamResampler::process_input()` — Converts sample rate to the compatible rate (usually 48 kHz).
//! 3. **WaveNet/LSTM neural inference** — The neural engine that processes the audio signal.
//! 4. `NamResampler::process_output()` — Converts back to the original host sample rate.
//! 5. **Output Gain and Clipping** — Applies the final volume and detects digital saturation.
//! 6. **Write to `DspBridge`** — Publishes the result with `Ordering::Release` to the playback callback.
//!
//! When no model is loaded, the engine operates in **True-Bypass** (the input signal passes clean).
//! When the PipeWire sample rate is the same as the nam model, the resampler operates in bypass without overhead.

mod bridge;
mod capture;
mod handlers;
pub mod identity;
pub mod output_pw;
mod playback;
mod reconnect;
mod rt_callback;
mod run;
pub mod status;
mod wakeup;

pub use output_pw::PipewireHostConfig;
pub use run::run_pipewire_host;
pub use status::{BackendState, BackendStatusSnapshot, SharedBackendStatus, observe_stream_state};
pub use wakeup::ControlPlaneWakeup;

/// Offline RT swap-stress harness (T2.6 / ER-2) — full capture-callback drain
/// sequence + DSP with no PipeWire daemon. Compiled only under `testing`.
#[cfg(feature = "testing")]
pub use rt_callback::harness::RtSwapHarness;

// Re-exports for test module compatibility (pw_host_test.rs).
#[cfg(test)]
pub(crate) use neural_amp_modeler_rs::dsp::pipeline::{BridgeBuffer, DspBridge};

#[cfg(test)]
#[path = "pw_host_test.rs"]
mod pw_host_test;
