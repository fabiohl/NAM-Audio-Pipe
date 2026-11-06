// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire stream-specific telemetry, format latches, and clock synchronization status.
//!
//! Separated from core `RtStatusFlags` to maintain clear architectural boundaries:
//! stream lifecycle, capture/playback rate negotiation, and host clock delay tracking
//! belong strictly to the audio streaming host (`NAM-Audio-Pipe`), keeping the core
//! DSP engine host-agnostic.

#[cfg(any(feature = "testing", feature = "heap-audit"))]
use neural_amp_modeler_rs::dsp::telemetry::ExactLatencyReservoir;
use neural_amp_modeler_rs::dsp::telemetry::{AtomicHistogram, LatencyHistogram};
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

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
    pub capture_hist: AtomicHistogram,

    /// Latency histogram for record enqueue stage (pre-push → post-push).
    pub record_hist: AtomicHistogram,

    /// Latency histogram for playback stage (callback start → hardware buffer write).
    pub playback_hist: AtomicHistogram,

    /// Latency histogram for end-to-end processing (capture start → hardware playback).
    ///
    /// Uses the log-linear [`LatencyHistogram`] (≤ 12.5 % error) so the E2E gate
    /// can be relative to the host quantum instead of snapping to a power of two.
    pub e2e_hist: LatencyHistogram,

    /// Exact-sample companion to [`Self::e2e_hist`], holding the most recent E2E
    /// samples so the harness can report true percentiles. The log-linear
    /// histogram quantises the tail (`p50 == p99 == p999` on a bucket boundary);
    /// this reservoir keeps them distinct. Gated on the same propagated features
    /// as the engine type (testing/heap-audit), so the default/release build is
    /// byte-identical. Not gated on `cfg(test)`: a dependency is never built with
    /// the host crate's test cfg, so `test` here would reference a type the
    /// engine configured out under `--no-default-features`.
    #[cfg(any(feature = "testing", feature = "heap-audit"))]
    pub e2e_exact: ExactLatencyReservoir,

    /// Incremented by the playback callback each time the bridge produced no
    /// new DSP block (capture paused, resampler rebuild pending, clock drift or
    /// quantum miss) and the deterministic silence policy delivered a recycled
    /// output buffer filled with `0.0f32`. Telemetry only —
    /// the hardware never repeats stale audio.
    pub playback_bridge_starvation: AtomicU32,

    /// Cumulative count of frames dropped by the bridge due to clock drift (capture > playback).
    pub bridge_dropped_frames: AtomicU32,

    /// Playback quanta delivered since the playback stream last entered
    /// `Streaming` (reset to `0` by the state handler on that transition).
    ///
    /// Bounds the bring-up window: bridge clock-drift drops
    /// observed while this counter is below
    /// [`PLAYBACK_BRINGUP_QUANTUMS`](crate::standalone::pw_host::PLAYBACK_BRINGUP_QUANTUMS)
    /// are the expected startup ramp (the pre-rolled silence block plus the
    /// capture producer's first block), not real drift, so the telemetry loop
    /// suppresses the `Drifting detected` warning and does not account them.
    pub playback_quantums: AtomicU32,

    /// Last sample rate negotiated by the capture stream's host renegotiation
    /// listener (`0` = never negotiated). Written on the host stream-negotiation
    /// thread (cold path, outside the RT audio data thread); read by the playback
    /// listener for the cross-stream rate comparison and by the main loop for
    /// diagnostics.
    pub capture_negotiated_rate: AtomicU32,

    /// Last sample rate negotiated by the playback stream's host renegotiation
    /// listener (`0` = never negotiated). Written on the host stream-negotiation
    /// thread (cold path, outside the RT audio data thread); read by the capture
    /// listener for the cross-stream rate comparison and by the main loop for
    /// diagnostics.
    pub playback_negotiated_rate: AtomicU32,

    /// Sticky latch guarding the capture stream format contract negotiated with the host.
    pub capture_format_ok: AtomicU32,

    /// Sticky latch guarding the playback stream format contract negotiated with the host.
    pub playback_format_ok: AtomicU32,

    /// Active state of the capture stream (1 = Streaming, 0 = Paused/Unconnected/Error).
    pub capture_active: AtomicU32,

    /// Active state of the playback stream (1 = Streaming, 0 = Paused/Unconnected/Error).
    pub playback_active: AtomicU32,

    /// Aggregate sticky latch guarding the strict format contract negotiated with the host.
    ///
    /// `1` = both stream formats are valid (`F32P` planar stereo); `0` = a divergent format
    /// was negotiated on either stream.
    pub format_contract_ok: AtomicU32,

    /// Algorithmic latency in samples of the resampler stage currently applied on the RT audio thread.
    /// Updated with `Ordering::Release` in `install_resampler` upon committing a swap.
    pub applied_resampler_latency: AtomicU32,

    /// Algorithmic latency in samples of the cab-sim stage currently applied on the RT audio thread
    /// (`0` when bypassed / no pair active). Updated with `Ordering::Release` in `install_cabsim`.
    pub applied_cabsim_latency: AtomicU32,

    /// Algorithmic latency in native model samples of the oversampling stage currently applied on the RT audio thread
    /// (`0` when bypassed / Off, `12` for X2, `18` for X4). Updated with `Ordering::Release` in `install_os_pair`.
    pub applied_oversample_latency: AtomicU32,

    /// Sample rate of the model actually installed behind the applied resampler
    /// (`0` = not published yet). Updated off-/RT with `Ordering::Release` when
    /// the resampler is installed, so latency conversion never assumes 48 kHz.
    pub applied_nam_rate: AtomicU32,

    /// Sequence lock counter for the coherent multi-component latency snapshot (resampler, cabsim, oversample).
    /// Odd = write in progress; Even = clean snapshot.
    pub latency_sequence: AtomicU64,

    /// Coherent 64-bit atomic latency snapshot.
    /// High 32 bits: resampler latency in samples.
    /// Low 32 bits: cabsim latency in samples.
    /// Eliminates transient latency tearing when reading aggregate DSP latency off-RT.
    pub applied_latency_pair: AtomicU64,

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
            capture_hist: AtomicHistogram::new(),
            record_hist: AtomicHistogram::new(),
            playback_hist: AtomicHistogram::new(),
            e2e_hist: LatencyHistogram::new(),
            #[cfg(any(feature = "testing", feature = "heap-audit"))]
            e2e_exact: ExactLatencyReservoir::new(),
            playback_bridge_starvation: AtomicU32::new(0),
            bridge_dropped_frames: AtomicU32::new(0),
            playback_quantums: AtomicU32::new(0),
            capture_negotiated_rate: AtomicU32::new(0),
            playback_negotiated_rate: AtomicU32::new(0),
            capture_format_ok: AtomicU32::new(1),
            playback_format_ok: AtomicU32::new(1),
            capture_active: AtomicU32::new(1),
            playback_active: AtomicU32::new(1),
            format_contract_ok: AtomicU32::new(1),
            applied_resampler_latency: AtomicU32::new(0),
            applied_cabsim_latency: AtomicU32::new(0),
            applied_oversample_latency: AtomicU32::new(0),
            applied_nam_rate: AtomicU32::new(0),
            latency_sequence: AtomicU64::new(0),
            applied_latency_pair: AtomicU64::new(0),
            capture_host_now: AtomicI64::new(0),
            capture_host_ticks: AtomicU64::new(0),
            capture_host_delay: AtomicI64::new(0),
            playback_host_now: AtomicI64::new(0),
            playback_host_ticks: AtomicU64::new(0),
            playback_host_delay: AtomicI64::new(0),
        }
    }

    /// Packs resampler and cabsim latencies into a single 64-bit snapshot.
    #[inline(always)]
    pub fn pack_latency_pair(resampler: u32, cabsim: u32) -> u64 {
        ((resampler as u64) << 32) | (cabsim as u64)
    }

    /// Unpacks a 64-bit snapshot into (resampler_latency, cabsim_latency).
    #[inline(always)]
    pub fn unpack_latency_pair(packed: u64) -> (u32, u32) {
        ((packed >> 32) as u32, packed as u32)
    }

    /// Atomically reads the coherent (resampler, cabsim) latency snapshot.
    #[inline(always)]
    pub fn read_applied_latency_pair(&self) -> (u32, u32) {
        Self::unpack_latency_pair(self.applied_latency_pair.load(Ordering::Acquire))
    }

    /// Atomically reads the coherent (resampler, cabsim, oversample_native) latency snapshot via seqlock.
    ///
    /// Single writer (the RT audio thread); writers bump the sequence before and
    /// after the stores. The `Acquire` fence before the closing sequence load
    /// orders the field loads before the validation, so a concurrent writer can
    /// never expose a torn snapshot.
    #[inline]
    pub fn read_applied_latencies(&self) -> (u32, u32, u32) {
        loop {
            let s0 = self.latency_sequence.load(Ordering::Acquire);
            if s0 & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let res = self.applied_resampler_latency.load(Ordering::Relaxed);
            let cab = self.applied_cabsim_latency.load(Ordering::Relaxed);
            let os = self.applied_oversample_latency.load(Ordering::Relaxed);
            core::sync::atomic::fence(Ordering::Acquire);
            let s1 = self.latency_sequence.load(Ordering::Relaxed);
            if s0 == s1 {
                return (res, cab, os);
            }
        }
    }

    /// Atomically updates resampler latency and the coherent latency snapshots.
    #[inline]
    pub fn update_resampler_latency(&self, resampler: u32) {
        self.latency_sequence.fetch_add(1, Ordering::Release);
        self.applied_resampler_latency
            .store(resampler, Ordering::Release);
        self.update_latency_pair_compat(
            resampler,
            self.applied_cabsim_latency.load(Ordering::Relaxed),
        );
        self.latency_sequence.fetch_add(1, Ordering::Release);
    }

    /// Atomically updates cabsim latency and the coherent latency snapshots.
    #[inline]
    pub fn update_cabsim_latency(&self, cabsim: u32) {
        self.latency_sequence.fetch_add(1, Ordering::Release);
        self.applied_cabsim_latency.store(cabsim, Ordering::Release);
        self.update_latency_pair_compat(
            self.applied_resampler_latency.load(Ordering::Relaxed),
            cabsim,
        );
        self.latency_sequence.fetch_add(1, Ordering::Release);
    }

    /// Atomically updates oversample latency (native model samples) and the coherent latency snapshot.
    #[inline]
    pub fn update_oversample_latency(&self, oversample: u32) {
        self.latency_sequence.fetch_add(1, Ordering::Release);
        self.applied_oversample_latency
            .store(oversample, Ordering::Release);
        self.latency_sequence.fetch_add(1, Ordering::Release);
    }

    #[inline]
    fn update_latency_pair_compat(&self, resampler: u32, cabsim: u32) {
        let mut cur = self.applied_latency_pair.load(Ordering::Relaxed);
        loop {
            let next = Self::pack_latency_pair(resampler, cabsim);
            match self.applied_latency_pair.compare_exchange_weak(
                cur,
                next,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
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

#[cfg(all(test, any(feature = "testing", feature = "heap-audit")))]
#[path = "stream_status_test.rs"]
mod tests;
