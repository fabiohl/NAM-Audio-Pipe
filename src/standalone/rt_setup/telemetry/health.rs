// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Xrun, playback-health, clock-drift, and deadline telemetry.

use super::state::PollState;
use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::StreamStatusFlags;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(super) fn poll_xruns_and_playback_health(
    rt_status: &RtStatusFlags,
    stream_status: Option<&StreamStatusFlags>,
    state: &mut PollState,
) {
    let overloads = rt_status.dsp_overloads.swap(0, Ordering::Relaxed);
    if overloads > 0 {
        rt_status.xruns.fetch_add(overloads, Ordering::Relaxed);
    }
    if state.latches.cpu_overload.observe(overloads > 0) {
        log::warn!(
            "{} CPU overload ({} buffers). Consider using a lighter model or a faster processor.",
            "🚨".red(),
            overloads
        );
    }

    let input_buffer_miss = rt_status.input_buffer_miss.swap(0, Ordering::Relaxed);
    if state
        .latches
        .input_buffer_miss
        .observe(input_buffer_miss > 0)
    {
        log::warn!(
            "{} PipeWire capture buffer miss ({} xruns). Check system load or buffer size.",
            "📻".yellow(),
            input_buffer_miss
        );
    }

    let output_buffer_miss = rt_status.output_buffer_miss.swap(0, Ordering::Relaxed);
    if state
        .latches
        .output_buffer_miss
        .observe(output_buffer_miss > 0)
    {
        log::warn!(
            "{} PipeWire playback buffer miss ({} xruns). Check system load or buffer size.",
            "📢".yellow(),
            output_buffer_miss
        );
    }

    if state
        .latches
        .contract_violation
        .observe(rt_status.check_and_clear_flag(RT_STATUS_HOST_CONTRACT_VIOLATION))
    {
        log::error!(
            "[E2304 | SPA_FORMAT_CONTRACT_VIOLATION] Audio host violated the strict SPA format \
             contract (F32P planar stereo, 2 channels FL/FR) — check that the PipeWire graph is \
             not forcing a mono, interleaved, S16 or surround negotiation onto the NAM streams \
             (e.g. via WirePlumber rules)."
        );
    }

    let pw_log_dropped_now = crate::standalone::pw_host::rt_log_ring::pw_log_dropped();
    let pw_log_dropped_delta = pw_log_dropped_now.saturating_sub(state.pw_log_dropped_last);
    state.pw_log_dropped_last = pw_log_dropped_now;
    if state
        .latches
        .pw_log_dropped
        .observe(pw_log_dropped_delta > 0)
    {
        log::warn!(
            "PipeWire log ring saturated: {} PW log event(s) discarded off-RT this episode \
             (marked RT thread stayed deadline-clean; PW logging at this rate is a \
             diagnostic storm — audit the PW level with PIPEWIRE_DEBUG).",
            pw_log_dropped_delta
        );
    }

    let current_divergence = stream_status
        .map(|s| s.playback_thread_divergence.load(Ordering::Relaxed))
        .unwrap_or(0);
    let delta_divergence = current_divergence.saturating_sub(state.playback_thread_divergence_last);
    state.playback_thread_divergence_last = current_divergence;
    if state
        .latches
        .playback_thread_divergence
        .observe(delta_divergence > 0)
    {
        log::warn!(
            "Playback callback ran on a thread other than the capture-marked RT data thread \
             {} time(s) this episode (multiple PipeWire data loops, or a playback thread that \
             was not promoted). Audio continues; RT scheduling and PW log offload cover only \
             the capture-marked thread.",
            delta_divergence
        );
    }

    let cab_stale = stream_status.is_some_and(StreamStatusFlags::is_cab_rate_stale);
    if state.latches.cab_rate_stale.observe(cab_stale) {
        log::warn!(
            "[E2305 | CAB_RATE_STALE] The installed cabinet (IR pair or NAM cabinet) was \
             negotiated at a different sample rate than the host quantum and is being \
             bypassed. Reload the cabinet at the active rate or renegotiate the graph."
        );
    }

    let current_starvation = stream_status
        .map(|s| s.playback_bridge_starvation.load(Ordering::Relaxed))
        .unwrap_or(0);
    let delta_starvation = current_starvation.saturating_sub(state.last_starvation);
    state.last_starvation = current_starvation;
    if state
        .latches
        .playback_starvation
        .observe(delta_starvation > 0)
    {
        log::info!(
            "{} Playback delivered {} silence block(s) under bridge starvation (no stale audio repeated).",
            "🔇".blue(),
            delta_starvation
        );
    }

    let current_early = stream_status
        .map(|s| s.playback_early_callbacks.load(Ordering::Relaxed))
        .unwrap_or(0);
    let delta_early = current_early.saturating_sub(state.last_early);
    state.last_early = current_early;
    if state.latches.playback_early.observe(delta_early > 0) {
        log::info!(
            "{} Playback callback fired ahead of the capture quantum {} time(s); classified separately from starvation.",
            "⏱",
            delta_early
        );
    }

    let current_muted = stream_status
        .map(|s| s.playback_muted_callbacks.load(Ordering::Relaxed))
        .unwrap_or(0);
    let delta_muted = current_muted.saturating_sub(state.last_muted);
    state.last_muted = current_muted;
    if state.latches.playback_muted.observe(delta_muted > 0) {
        log::info!(
            "{} Playback delivered {} fail-closed silence block(s) while the format contract was broken.",
            "🔇".blue(),
            delta_muted
        );
    }
}

pub(super) fn poll_clock_drift(
    stream_status: Option<&StreamStatusFlags>,
    dropped_frames: u32,
    state: &mut PollState,
) {
    let quanta = stream_status
        .map(|s| s.playback_quantums.load(Ordering::Relaxed))
        .unwrap_or(u32::MAX);
    if quanta < state.last_bringup_quanta {
        state.last_bringup_quanta = quanta;
        state.bringup_suppressed_polls = 0;
        state.has_polled_in_bringup = false;
        state.startup_drops_drained = false;
        state.bringup_completed = false;
    } else if quanta != state.last_bringup_quanta {
        state.last_bringup_quanta = quanta;
        state.bringup_suppressed_polls = 0;
    }

    let mut bringing_up = false;
    if !state.bringup_completed {
        if quanta < crate::standalone::pw_host::PLAYBACK_BRINGUP_QUANTUMS {
            bringing_up = state.bringup_suppressed_polls
                < crate::standalone::pw_host::MAX_BRINGUP_SUPPRESSED_POLLS;
            state.has_polled_in_bringup = true;
        } else if !state.startup_drops_drained {
            bringing_up = true;
            state.startup_drops_drained = true;
            state.bringup_completed = true;
        } else {
            state.bringup_completed = true;
        }
    }

    if bringing_up {
        state.bringup_suppressed_polls = state.bringup_suppressed_polls.saturating_add(1);
    }
    let drops = dropped_frames;
    if bringing_up && drops > 0 {
        state.startup_drops_drained = true;
    }
    if !bringing_up {
        if let Some(s) = stream_status.filter(|_| drops > 0) {
            s.bridge_dropped_frames.fetch_add(drops, Ordering::Relaxed);
        }
        if state.latches.drift_drops.observe(drops > 0) {
            log::warn!(
                "{} Drifting detected: {} audio blocks discarded (capture > playback).",
                "⚠️".yellow(),
                drops
            );
        }
    }
}

pub(super) fn check_audio_deadline(
    rt_status: &RtStatusFlags,
    duration: Duration,
    state: &mut PollState,
) {
    let rate_val = rt_status.active_rate.load(Ordering::Relaxed);
    let samples_val = rt_status.last_n_samples.load(Ordering::Relaxed);

    if rate_val > 0 && samples_val > 0 {
        let budget_us = crate::standalone::pw_host::overload::deadline_budget_us(
            samples_val as u64,
            rate_val as u64,
        );
        let elapsed_us = duration.as_micros() as f64;

        let deadline_exceeded = elapsed_us > budget_us;
        if state.latches.deadline_exceeded.observe(deadline_exceeded) {
            log::error!(
                "[E2001 | PROCESSING_OVERLOAD] Audio deadline exceeded (possible xrun): \
                 exec_time_us={} budget_us={} n_samples={} rate={} — verify model \
                 topology or reduce system load.",
                elapsed_us as u64,
                budget_us as u64,
                samples_val,
                rate_val
            );
        }
    }
}
