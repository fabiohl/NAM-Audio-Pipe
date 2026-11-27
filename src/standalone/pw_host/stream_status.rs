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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

/// Host audio streaming telemetry, format latches, and clock synchronization.
///
/// Isolated cache-line aligned (`#[repr(align(128))]`) to avoid false sharing
/// between real-time streaming audio threads and the main telemetry/diagnostics loop.
#[repr(align(128))]
pub struct StreamStatusFlags {
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

    /// Incremented by the playback callback when it asks the bridge for a new
    /// block *before* the next capture quantum could have started (the previous
    /// delivered generation's capture timestamp plus one quantum period is
    /// still in the future). Kept separate from
    /// [`Self::playback_bridge_starvation`] so an early callback is not
    /// misreported as a lost capture.
    pub playback_early_callbacks: AtomicU32,

    /// Capture-start timestamp (nanoseconds, TSC domain) of the last generation
    /// actually delivered to playback. Used to classify a missing block as an
    /// early callback versus a real starvation gap. `0` = nothing delivered yet.
    pub playback_last_capture_tsc: AtomicU64,

    /// Incremented by the playback callback when it delivers deterministic
    /// silence because the format contract is broken (fail-closed mute), which
    /// is neither a starvation gap nor an early callback.
    pub playback_muted_callbacks: AtomicU32,

    /// Cumulative count of frames dropped by the bridge due to clock drift (capture > playback).
    pub bridge_dropped_frames: AtomicU32,

    /// Incremented by the playback callback when the thread running it is not
    /// the PipeWire RT data thread marked by the capture callback. With
    /// multiple data loops (PipeWire ≥ 1.2) the playback callback may run on a
    /// distinct, unmarked thread: it then lacks SCHED_FIFO/affinity and its
    /// PipeWire logs follow the synchronous path. Diagnostic only — the
    /// playback thread is deliberately not promoted here, preserving the
    /// single-producer invariant of `RtProducer`.
    pub playback_thread_divergence: AtomicU64,

    /// Incremented by the capture callback when `pw_stream_trigger_process`
    /// fails while the playback stream is active.
    pub playback_trigger_errors: AtomicU32,

    /// `true` when at least one cab-sim slot (IR pair or NAM cabinet) is
    /// installed at a rate that differs from the host quantum rate, so the
    /// callback is currently bypassing it. The Pipe-local signal for the
    /// otherwise silent "cabinet ignored on rate mismatch" path.
    pub cab_rate_stale: AtomicBool,

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

    /// Last host sample rate observed on a valid capture format negotiation.
    ///
    /// Diagnostic-only companion to the RT rate inbox: the callback consumes
    /// that inbox with `swap(0)`, so after the first quantum it no longer
    /// reports the running rate. This cell is only ever stored (never swapped
    /// or cleared) and therefore keeps the last known rate available to
    /// control-loop diagnostics and the terminal backend-failure report.
    pub last_known_rate: AtomicU32,

    /// Sticky latch guarding the capture stream format contract negotiated with the host.
    pub capture_format_ok: AtomicU32,

    /// Sticky latch guarding the playback stream format contract negotiated with the host.
    pub playback_format_ok: AtomicU32,

    /// Active state of the capture stream (1 = Streaming, 0 = Paused/Unconnected/Error).
    pub capture_active: AtomicU32,

    /// Active state of the playback stream (1 = Streaming, 0 = Paused/Unconnected/Error).
    pub playback_active: AtomicU32,

    /// Aggregate sticky latch guarding the strict format contract negotiated
    /// with the host.
    ///
    /// `1` = both stream formats are valid (`F32P` planar stereo); `0` = a
    /// divergent format was negotiated on either stream. This derived value is
    /// diagnostic: each RT path gates on its own per-stream latch
    /// (`capture_format_ok` in the capture callback, `is_audio_unmuted` for
    /// playback), so a briefly stale aggregate cannot fail open.
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
            capture_hist: AtomicHistogram::new(),
            record_hist: AtomicHistogram::new(),
            playback_hist: AtomicHistogram::new(),
            e2e_hist: LatencyHistogram::new(),
            #[cfg(any(feature = "testing", feature = "heap-audit"))]
            e2e_exact: ExactLatencyReservoir::new(),
            playback_bridge_starvation: AtomicU32::new(0),
            playback_early_callbacks: AtomicU32::new(0),
            playback_last_capture_tsc: AtomicU64::new(0),
            playback_muted_callbacks: AtomicU32::new(0),
            bridge_dropped_frames: AtomicU32::new(0),
            playback_thread_divergence: AtomicU64::new(0),
            playback_trigger_errors: AtomicU32::new(0),
            cab_rate_stale: AtomicBool::new(false),
            playback_quantums: AtomicU32::new(0),
            capture_negotiated_rate: AtomicU32::new(0),
            playback_negotiated_rate: AtomicU32::new(0),
            last_known_rate: AtomicU32::new(0),
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

    /// Advances the playback bring-up quantum counter without a
    /// load-then-store (which would lose updates against concurrent bumps).
    ///
    /// Saturates at `limit` so a long-running stream can never re-open the
    /// suppression window.
    #[inline]
    pub fn bump_playback_quantums(&self, limit: u32) {
        let _ = self
            .playback_quantums
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |quanta| {
                (quanta < limit).then_some(quanta + 1)
            });
    }

    /// Publishes whether the installed cabinet slots are at a stale rate.
    ///
    /// Written once per capture quantum with `Relaxed` (diagnostic only); the
    /// telemetry loop reports the episode off-RT.
    #[inline]
    pub fn note_cab_rate_stale(&self, stale: bool) {
        self.cab_rate_stale.store(stale, Ordering::Relaxed);
    }

    /// Whether the installed cabinet slots are currently rate-stale.
    #[must_use]
    #[inline]
    pub fn is_cab_rate_stale(&self) -> bool {
        self.cab_rate_stale.load(Ordering::Relaxed)
    }

    /// Records whether the playback callback ran on the marked RT data thread.
    ///
    /// The check itself is a TLS read performed by the caller (no syscall, no
    /// lock); this method only advances a `Relaxed` telemetry counter when the
    /// playback thread differs from the capture-marked data thread.
    #[inline]
    pub fn note_playback_thread_divergence(&self, on_rt_thread: bool) {
        if !on_rt_thread {
            self.playback_thread_divergence
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Atomically restarts the playback bring-up window.
    #[inline]
    pub fn reset_playback_quantums(&self) {
        self.playback_quantums.swap(0, Ordering::Relaxed);
    }

    /// Restarts the playback-thread divergence window for a fresh stream
    /// instance.
    #[inline]
    pub fn reset_playback_thread_divergence(&self) {
        self.playback_thread_divergence.store(0, Ordering::Relaxed);
    }

    /// Restarts the playback-trigger error window for a fresh stream instance.
    #[inline]
    pub fn reset_playback_trigger_errors(&self) {
        self.playback_trigger_errors.store(0, Ordering::Relaxed);
    }

    /// Re-arms the format-contract latches for a fresh stream instance.
    ///
    /// The latches live on the process-wide backend status, so a violation
    /// observed on a previous instance would otherwise keep the RT mute guard
    /// closed forever when the new graph never re-emits a `Format` event.
    /// A fresh graph starts fail-open: the streams re-latch their own contract
    /// when (and if) they negotiate a format.
    #[inline]
    pub fn reset_format_contract(&self) {
        self.capture_format_ok.store(1, Ordering::Relaxed);
        self.playback_format_ok.store(1, Ordering::Relaxed);
        self.format_contract_ok.store(1, Ordering::Relaxed);
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
