// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire stream-specific telemetry, format latches, and clock synchronization status.
//!
//! Separated from core `RtStatusFlags` to maintain clear architectural boundaries:
//! stream lifecycle, capture/playback rate negotiation, and host clock delay tracking
//! belong strictly to the audio streaming host (`NAM-Audio-Pipe`), keeping the core
//! DSP engine host-agnostic.

use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
use neural_amp_modeler_rs::dsp::telemetry::LatencyHistogram;

/// Host audio streaming telemetry, format latches, and clock synchronization.
///
/// Isolated cache-line aligned (`#[repr(align(128))]`) to avoid false sharing
/// between real-time streaming audio threads and the main telemetry/diagnostics loop.
#[repr(align(128))]
pub struct StreamStatusFlags {
    /// Duration of the last capture stage (callback start to end of host format validation) in nanoseconds.
    pub capture_cycle_time: AtomicU64,

    /// Duration of the last audio recording enqueue stage in nanoseconds.
    pub record_cycle_time: AtomicU64,

    /// Duration of the last playback stage (callback start to host buffer write completion) in nanoseconds.
    pub playback_cycle_time: AtomicU64,

    /// Duration of the last end-to-end cycle (capture start to playback output) in nanoseconds.
    pub e2e_cycle_time: AtomicU64,

    /// Starting timestamp of current capture block in nanoseconds (serialized RDTSC).
    pub capture_start_tsc: AtomicU64,

    /// Latency histogram for capture stage (callback start → end of host format validation/dequeue).
    pub capture_hist: LatencyHistogram,

    /// Latency histogram for record enqueue stage (pre-push → post-push).
    pub record_hist: LatencyHistogram,

    /// Latency histogram for playback stage (callback start → hardware buffer write).
    pub playback_hist: LatencyHistogram,

    /// Latency histogram for end-to-end processing (capture start → hardware playback).
    pub e2e_hist: LatencyHistogram,

    /// Incremented by the playback callback each time the bridge produced no
    /// new DSP block (capture paused, resampler rebuild pending, clock drift or
    /// quantum miss) and the deterministic silence policy delivered a recycled
    /// output buffer filled with `0.0f32` (G-RB-001). Telemetry only —
    /// the hardware never repeats stale audio.
    pub playback_bridge_starvation: AtomicU32,

    /// Last sample rate negotiated by the capture stream's host renegotiation
    /// listener (`0` = never negotiated). Written on the host stream-negotiation
    /// thread (cold path, outside the RT audio data thread); read by the playback
    /// listener for the cross-stream rate comparison and by the main loop for
    /// diagnostics (G-RB-001).
    pub capture_negotiated_rate: AtomicU32,

    /// Last sample rate negotiated by the playback stream's host renegotiation
    /// listener (`0` = never negotiated). Written on the host stream-negotiation
    /// thread (cold path, outside the RT audio data thread); read by the capture
    /// listener for the cross-stream rate comparison and by the main loop for
    /// diagnostics (G-RB-001).
    pub playback_negotiated_rate: AtomicU32,

    /// Sticky latch guarding the capture stream format contract negotiated with the host.
    pub capture_format_ok: AtomicU32,

    /// Sticky latch guarding the playback stream format contract negotiated with the host.
    pub playback_format_ok: AtomicU32,

    /// Active state of the capture stream (1 = Streaming, 0 = Paused/Unconnected/Error).
    pub capture_active: AtomicU32,

    /// Active state of the playback stream (1 = Streaming, 0 = Paused/Unconnected/Error).
    pub playback_active: AtomicU32,

    /// Aggregate sticky latch guarding the strict format contract negotiated with the host (G-RB-001).
    ///
    /// `1` = both stream formats are valid (`F32P` planar stereo); `0` = a divergent format
    /// was negotiated on either stream.
    pub format_contract_ok: AtomicU32,

    /// Host clock `time.now` from the last capture stream time() call (nanoseconds).
    pub capture_host_now: AtomicI64,
    /// Host clock `time.ticks` from the last capture stream time() call.
    pub capture_host_ticks: AtomicU64,
    /// Host clock `time.delay` from the last capture stream time() call (ticks).
    pub capture_host_delay: AtomicI64,
    /// Host clock `time.now` from the last playback stream time() call (nanoseconds).
    pub playback_host_now: AtomicI64,
    /// Host clock `time.ticks` from the last playback stream time() call.
    pub playback_host_ticks: AtomicU64,
    /// Host clock `time.delay` from the last playback stream time() call (ticks).
    pub playback_host_delay: AtomicI64,
}

impl Default for StreamStatusFlags {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamStatusFlags {
    /// Creates a new instance with standard initial values.
    #[cold]
    pub fn new() -> Self {
        Self {
            capture_cycle_time: AtomicU64::new(0),
            record_cycle_time: AtomicU64::new(0),
            playback_cycle_time: AtomicU64::new(0),
            e2e_cycle_time: AtomicU64::new(0),
            capture_start_tsc: AtomicU64::new(0),
            capture_hist: LatencyHistogram::new(),
            record_hist: LatencyHistogram::new(),
            playback_hist: LatencyHistogram::new(),
            e2e_hist: LatencyHistogram::new(),
            playback_bridge_starvation: AtomicU32::new(0),
            capture_negotiated_rate: AtomicU32::new(0),
            playback_negotiated_rate: AtomicU32::new(0),
            capture_format_ok: AtomicU32::new(1),
            playback_format_ok: AtomicU32::new(1),
            capture_active: AtomicU32::new(1),
            playback_active: AtomicU32::new(1),
            format_contract_ok: AtomicU32::new(1),
            capture_host_now: AtomicI64::new(0),
            capture_host_ticks: AtomicU64::new(0),
            capture_host_delay: AtomicI64::new(0),
            playback_host_now: AtomicI64::new(0),
            playback_host_ticks: AtomicU64::new(0),
            playback_host_delay: AtomicI64::new(0),
        }
    }

    /// Whether audio is unmuted across both streams (capture and playback format contracts
    /// valid AND both streams active).
    #[inline(always)]
    pub fn is_audio_unmuted(&self) -> bool {
        self.capture_format_ok.load(Ordering::Relaxed) != 0
            && self.playback_format_ok.load(Ordering::Relaxed) != 0
            && self.capture_active.load(Ordering::Relaxed) != 0
            && self.playback_active.load(Ordering::Relaxed) != 0
    }
}
