// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire host configuration and stream RAII state.

use crate::standalone::cli::GateConfig;
use crate::standalone::pw_host::stream_status::StreamStatusFlags;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use std::time::Duration;

/// Holds essential PipeWire instances (`StreamBox` and `Listener`).
///
/// RAII-only struct: fields are never read directly — they exist solely
/// to keep streams and listeners alive via drop semantics. The compiler
/// may warn about unused fields; that is expected and safe here.
/// Removing the fields would cause premature deallocation and audio dropout.
///
/// # Drop Order Invariant (Memory Safety)
///
/// In Rust, struct fields are dropped in top-to-bottom declaration order (RFC 1857).
/// Every `*_listener` MUST precede its corresponding `*_stream` field:
///
/// 1. `capture_listener` before `capture_stream`
/// 2. `playback_listener` before `playback_stream`
///
/// `StreamBox::drop` invokes `pw_stream_destroy`, which unlinks and frees the
/// underlying PipeWire `pw_stream` struct (`stream_free`). If a `StreamListener`
/// is dropped afterwards, its `Drop` implementation calls `spa_hook_remove` /
/// `spa_list_remove`, performing pointer writes (`elem.prev->next = elem.next`,
/// `elem.next->prev = elem.prev`) into deallocated memory — causing use-after-free (UB).
/// By declaring the listeners first, each listener is cleanly detached from a still-live
/// PipeWire stream during teardown, reconnect cycles, and shutdown.
pub struct AppState<L1, S1, L2, S2> {
    /// Listener bound to the capture stream (RAII anchor).
    /// Must precede `capture_stream` for safe drop order.
    pub capture_listener: L1,
    /// PipeWire capture stream (RAII anchor).
    pub capture_stream: S1,
    /// Listener bound to the playback stream (RAII anchor).
    /// Must precede `playback_stream` for safe drop order.
    pub playback_listener: L2,
    /// PipeWire playback stream (RAII anchor).
    pub playback_stream: S2,
}

/// Configuration for PipeWire host initialization.
pub struct PipewireHostConfig {
    /// Requested audio buffer size.
    pub buffer_size: u32,
    /// Cab-sim UPOLS partition policy in samples (`--cabsim-partition`):
    /// fixes the cab-sim algorithmic latency (exactly N samples) and is used
    /// for every pair installation, decoupled from `buffer_size`.
    pub cabsim_partition: u32,
    /// System snapshot for diagnostics.
    pub sys: SystemSnapshot,
    /// Raw IR samples for adaptive partition rebuild (None if no IR loaded).
    pub ir_raw_samples: Option<Vec<f32>>,
    /// Sample rate of `ir_raw_samples` (the IR file's native rate). Rebuilds
    /// resample the preserved original IR for the applied host output rate.
    pub ir_source_rate: u32,
    /// Full WaveNet model (L channel) stored for main-thread slimmable rebuild.
    pub full_wavenet_model_l: Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    /// Full WaveNet model (R channel) stored for main-thread slimmable rebuild.
    pub full_wavenet_model_r: Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    /// Whether the loaded model has a right-channel model (stereo config).
    /// The slimmable rebuild slices an R model only when this is true.
    pub has_model_r: bool,
    /// Producer to send slimmable-rebuilt atomic L/R pairs to the audio thread.
    pub slimmable_producer: rtrb::Producer<Box<neural_amp_modeler_rs::common::spsc::SlimModelPair>>,
    /// Producer to send oversampling engines rebuilt on main thread to the audio thread.
    pub os_producer: rtrb::Producer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>,
    /// Initial oversampling factor for the neural stage.
    pub oversample: OversampleFactor,
    /// Pre-computed CPU selection receipt (`--cpu` already resolved by the
    /// caller). Carries the selected RT core for `configure_realtime_thread`
    /// and the housekeeping set applied to the thread-loop thread.
    pub cpu_receipt: crate::standalone::rt_setup::affinity::CpuSelectionReceipt,
    /// `--fail-fast` on the CLI: disables the bounded reconnect cycle —
    /// the first backend failure triggers observable teardown immediately.
    pub fail_fast: bool,
    /// Optional per-episode reconnect backoff budget (`--reconnect-timeout <s>`).
    /// `None` uses the production schedule (6 attempts, 250 ms → 4 s).
    pub reconnect_timeout: Option<Duration>,
    /// Noise gate configuration from the polymorphic CLI flag `--gate MODE`:
    /// [`GateConfig::Off`] keeps the gate FSM permanently open (silence passes
    /// through), while a [`GateConfig::Threshold`] carries the open/close dBFS
    /// thresholds of the Schmitt trigger (auto hysteresis of 10 dB).
    pub gate_config: GateConfig,
    /// Noise gate topology from `--gate-mode <silence|musical>` (defaults to `Silence`).
    pub gate_mode: crate::standalone::cli::GateMode,
    /// Optional externally-owned stream status flags (for integration testing).
    pub stream_status: Option<std::sync::Arc<StreamStatusFlags>>,
    /// Optional observable recording status: when `--record` is
    /// active, `main` hands the same handle the recording worker writes so the
    /// main control loop can turn an `Active → Failed` transition (and the
    /// low-space pre-warning) into a visible, de-duplicated alert.
    pub recording_status: Option<crate::recording::SharedRecordingStatus>,
    /// Static banner facts resolved by `main`. The control loop emits
    /// one consolidated status banner once the first RT confirmations arrive.
    pub banner: crate::standalone::banner::BannerRequest,
    /// Explicit activation precision mode (`Standard` exact-grade or `Fast` Padé).
    pub activation: neural_amp_modeler_rs::math::activations::ActivationPrecision,
    /// Cabinet IR level policy (`--ir-level-policy`).
    pub ir_level_policy: crate::standalone::cli_domain::IrLevelPolicy,
    /// Optional hardware playback output limit (`--output-limit`).
    pub output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
}
