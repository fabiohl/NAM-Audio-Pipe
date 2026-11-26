// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Latency and host-timing telemetry.

use super::latency_summary;
use super::state::PollState;
use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::StreamStatusFlags;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use std::sync::atomic::Ordering;

pub(super) fn emit_throttled_latency_and_timing(
    rt_status: &RtStatusFlags,
    stream_status: Option<&StreamStatusFlags>,
    state: &mut PollState,
) {
    state.telemetry_throttle = state.telemetry_throttle.wrapping_add(1);
    if state.telemetry_throttle.wrapping_rem(100) != 0 {
        return;
    }

    let cap = stream_status
        .map(|ss| latency_summary(&ss.capture_hist))
        .unwrap_or_default();
    let dsp = latency_summary(&rt_status.latency_hist);
    let rec = stream_status
        .map(|ss| latency_summary(&ss.record_hist))
        .unwrap_or_default();
    let pb = stream_status
        .map(|ss| latency_summary(&ss.playback_hist))
        .unwrap_or_default();
    let e2e = stream_status
        .map(|ss| latency_summary(&ss.e2e_hist))
        .unwrap_or_default();

    let total_calls = rt_status.latency_hist.total_count();

    log::info!(
        "{} RT Latency Breakdown (10s) [{} blocks]:\n\
         ├─ 1. Capture Total:        min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs\n\
         ├─ 2. DSP Core:             min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs\n\
         ├─ 3. Record Enqueue:       min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs\n\
         ├─ 4. Playback Total:       min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs\n\
         └─ 5. Capture↦Playback E2E: min={}µs | mean={}µs | p50={}µs | p99={}µs | p999={}µs | max={}µs",
        "📊".bright_blue(),
        total_calls,
        cap.min_ns / 1000,
        cap.mean_ns / 1000,
        cap.p50_ns / 1000,
        cap.p99_ns / 1000,
        cap.p999_ns / 1000,
        cap.max_ns / 1000,
        dsp.min_ns / 1000,
        dsp.mean_ns / 1000,
        dsp.p50_ns / 1000,
        dsp.p99_ns / 1000,
        dsp.p999_ns / 1000,
        dsp.max_ns / 1000,
        rec.min_ns / 1000,
        rec.mean_ns / 1000,
        rec.p50_ns / 1000,
        rec.p99_ns / 1000,
        rec.p999_ns / 1000,
        rec.max_ns / 1000,
        pb.min_ns / 1000,
        pb.mean_ns / 1000,
        pb.p50_ns / 1000,
        pb.p99_ns / 1000,
        pb.p999_ns / 1000,
        pb.max_ns / 1000,
        e2e.min_ns / 1000,
        e2e.mean_ns / 1000,
        e2e.p50_ns / 1000,
        e2e.p99_ns / 1000,
        e2e.p999_ns / 1000,
        e2e.max_ns / 1000,
    );

    if let Some(ss) = stream_status {
        ss.capture_hist.reset();
        ss.record_hist.reset();
        ss.playback_hist.reset();
        ss.e2e_hist.reset();
        #[cfg(any(feature = "testing", feature = "heap-audit"))]
        ss.e2e_exact.reset();
    }
    rt_status.latency_hist.reset();

    let Some(ss) = stream_status else {
        return;
    };
    let cap_ticks = ss.capture_host_ticks.load(Ordering::Relaxed);
    let pb_ticks = ss.playback_host_ticks.load(Ordering::Relaxed);
    if cap_ticks == 0 || pb_ticks == 0 {
        return;
    }

    let cap_now = ss.capture_host_now.load(Ordering::Relaxed);
    let pb_now = ss.playback_host_now.load(Ordering::Relaxed);
    let cap_delay = ss.capture_host_delay.load(Ordering::Relaxed);
    let pb_delay = ss.playback_host_delay.load(Ordering::Relaxed);

    let tick_delta = pb_ticks.wrapping_sub(cap_ticks);
    let time_delta_us = if pb_now > 0 && cap_now > 0 && pb_now > cap_now {
        ((pb_now - cap_now) / 1000) as u64
    } else {
        0
    };

    let rate = rt_status.active_rate.load(Ordering::Relaxed);
    let samples = rt_status.last_n_samples.load(Ordering::Relaxed);
    let quantum_us = if rate > 0 && samples > 0 {
        (samples as u64 * 1_000_000) / rate as u64
    } else {
        0
    };
    let cap_delay_us = if rate > 0 {
        (cap_delay.max(0) as u64 * 1_000_000) / rate as u64
    } else {
        0
    };
    let pb_delay_us = if rate > 0 {
        (pb_delay.max(0) as u64 * 1_000_000) / rate as u64
    } else {
        0
    };

    log::info!(
        "{} PW Stream Timing: cap↦pb gap={} µs | tick_delta={} | cap_ticks={} pb_ticks={} | cap_delay={} µs pb_delay={} µs | quantum={} µs",
        "⏱️".bright_blue(),
        time_delta_us,
        tick_delta,
        cap_ticks,
        pb_ticks,
        cap_delay_us,
        pb_delay_us,
        quantum_us,
    );
}
