// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use crate::standalone::cli::BUFFER_SIZE_MIN;
use crate::standalone::pw_host::StreamStatusFlags;
use crate::standalone::pw_host::capture::state::CaptureState;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF;

use pipewire as pw;
use std::sync::atomic::Ordering;

/// Returns `true` when the adaptive FSM may consume a fresh latency sample.
///
/// `process_dsp_buffer` measures and stores `dsp_cycle_time` when the pre-call
/// `frame_count` is a multiple of 16 (then increments it), so the post-call
/// counter satisfies `(frame_count - 1) & 0xF == 0`. Requiring a changed
/// counter prevents a skipped/paused callback from feeding the same stale
/// `dsp_cycle_time` back into the FSM (which would falsely satisfy the
/// consecutive-overload/recovery counters).
#[inline]
pub(crate) fn adaptive_update_due(frame_count: u32, prev_frame_count: u32) -> bool {
    frame_count != prev_frame_count && (frame_count.wrapping_sub(1) & 0xF) == 0
}

#[cold]
#[inline(never)]
pub(crate) fn request_cabsim_rebuild(
    rt_status: &RtStatusFlags,
    partition_to_request: usize,
    current_host_rate: u32,
) {
    rt_status
        .requested_cabsim_partition_size
        .store(partition_to_request as u32, Ordering::Relaxed);
    rt_status
        .requested_cabsim_host_rate
        .store(current_host_rate, Ordering::Relaxed);
    rt_status
        .requested_cabsim_generation
        .fetch_add(1, Ordering::Release);
    rt_status.set_flag_release(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
}

/// Partition carried by a cab-sim rebuild request.
///
/// * Active pair present (rate-only recalibration): preserve its current
///   partition — a quantum/rate renegotiation must never change the installed
///   latency.
/// * No active pair (first install): the `--cabsim-partition` policy
///   (`CaptureState::cabsim_partition_policy`) — never the host quantum.
#[inline]
pub(crate) fn cabsim_partition_to_request(
    active: Option<&neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair>,
    policy: usize,
) -> usize {
    active.map(|pair| pair.partition_size()).unwrap_or(policy)
}

/// Decides whether a cab-sim rebuild must be requested.
///
/// A rebuild is needed when an IR is loaded and any of:
/// * no pair is active yet — first install, or safe-bypass after a failed
///   rebuild (fired once a real host quantum is observed, which proves the
///   host rate was negotiated; the partition itself comes from the
///   `--cabsim-partition` policy, never from the quantum);
/// * the pair's IR is calibrated for a different rate than the applied host
///   output rate (rate calibration).
///
/// The host quantum no longer drives a rebuild. The block-size-agnostic
/// driver (`process_block`) chunks arbitrary host blocks internally against
/// the fixed partition, so a PipeWire quantum renegotiation alone (e.g. 128
/// to 256 samples at constant rate) keeps the existing pair active with zero
/// audio interruption and zero FFT rebuild.
///
/// `failed_key` suppresses endless retries of deterministically failing IR builds:
/// when a build fails and delivers safe-bypass `None`, the requested
/// `(host_rate, partition)` is latched in `CaptureState::cabsim_failed_key` and
/// suppressed on subsequent callbacks. Since IR raw samples are immutable per process in
/// NAM-Audio-Pipe, a renegotiated host rate or partition policy resets the latch,
/// resuming rebuild requests for the fresh parameters.
///
/// `already_pending` suppresses duplicate requests while a previous rebuild is
/// in flight, for both the initial-install and the active-pair branches. Without
/// it the active-pair branch would re-arm the request on every audio callback
/// whenever the pair is still stale; each re-arm bumps
/// `requested_cabsim_generation`, so the envelope produced by the in-flight
/// rebuild arrives with an outdated generation and is discarded by the RT
/// swap, starving the active pair forever (livelock).
pub(crate) fn cabsim_rebuild_needed(
    active: Option<&neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair>,
    has_ir: bool,
    n_samples: usize,
    host_rate: u32,
    partition: usize,
    failed_key: Option<(u32, usize)>,
    already_pending: bool,
) -> bool {
    // The quantum only gates the first install (a real quantum proves the
    // host rate was negotiated and lies inside the valid buffer domain
    // [BUFFER_SIZE_MIN, MAX_RESAMP_BUF]). An active pair never rebuilds on
    // quantum drift — the block-size-agnostic driver chunks any host block
    // internally — so an anomalous quantum can never trigger successive
    // rebuilds of a healthy pair (the first-install branch keeps the domain
    // defense-in-depth). Quanta below BUFFER_SIZE_MIN are rejected explicitly
    // and reported by the off-RT quantum log.
    if !has_ir || already_pending {
        return false;
    }
    match active {
        None => {
            if matches!(failed_key, Some((r, p)) if r == host_rate && p == partition) {
                return false;
            }
            (BUFFER_SIZE_MIN as usize..=MAX_RESAMP_BUF).contains(&n_samples)
        }
        Some(pair) => pair.sample_rate != host_rate,
    }
}

/// Neural cabinet rebuild predicate. A latched failure for the same `(host_rate, partition)`
/// suppresses requests in both branches: the last installed slot is kept fail-closed (and
/// gated off by its stale rate), so without the latch a persistent build failure would
/// re-request a rebuild on every callback.
pub(crate) fn nam_cab_rebuild_needed(
    active: Option<&crate::standalone::cabinet::CabNamSlot>,
    has_nam: bool,
    n_samples: usize,
    host_rate: u32,
    partition: usize,
    failed_key: Option<(u32, usize)>,
    already_pending: bool,
) -> bool {
    if !has_nam || already_pending {
        return false;
    }
    if matches!(failed_key, Some((r, p)) if r == host_rate && p == partition) {
        return false;
    }
    match active {
        None => (BUFFER_SIZE_MIN as usize..=MAX_RESAMP_BUF).contains(&n_samples),
        Some(slot) => slot.host_rate != host_rate,
    }
}

pub(crate) fn capture_epilogue(
    stream: &pw::stream::Stream,
    state: &mut CaptureState,
    stream_status: &StreamStatusFlags,
    rt_status: &RtStatusFlags,
    current_host_rate: u32,
    adaptive_prev_frame_count: &mut u32,
) {
    // Sample PipeWire clock for drift diagnostics (every 64 frames)
    if (state.frame_count.wrapping_sub(1) & 0x3F) == 0
        && let Ok(pw_time) = stream.time()
    {
        stream_status
            .capture_host_now
            .store(pw_time.now(), Ordering::Relaxed);
        stream_status
            .capture_host_ticks
            .store(pw_time.ticks(), Ordering::Relaxed);
        stream_status
            .capture_host_delay
            .store(pw_time.delay(), Ordering::Relaxed);
    }

    if adaptive_update_due(state.frame_count, *adaptive_prev_frame_count) {
        let elapsed_nanos = rt_status.dsp_cycle_time.load(Ordering::Relaxed);
        if elapsed_nanos > 0 {
            let n_samples = rt_status.last_n_samples.load(Ordering::Relaxed) as u64;
            if n_samples > 0 && current_host_rate > 0 {
                let budget_us = crate::standalone::pw_host::overload::adaptive_budget_us(
                    n_samples,
                    current_host_rate as u64,
                );
                let latency_us = elapsed_nanos / 1000;
                state
                    .adaptive_compute
                    .update(latency_us, budget_us, current_host_rate, rt_status);
            }
        }
    }
    *adaptive_prev_frame_count = state.frame_count;

    // Detect cabsim rate mismatch and signal rebuild:
    // the IR must match the applied host output rate only.
    // The partition is decoupled from the host quantum — the
    // block-size-agnostic driver (`process_block`) chunks
    // arbitrary host blocks internally, so a quantum
    // renegotiation alone never forces a heavy FFT rebuild.
    let last_n_samples = rt_status.last_n_samples.load(Ordering::Relaxed) as usize;
    let partition_to_request = cabsim_partition_to_request(
        state.active_cabsim.as_deref(),
        state.cabsim_partition_policy,
    );
    let wav_needs_rebuild = cabsim_rebuild_needed(
        state.active_cabsim.as_deref(),
        state.ir_raw_samples.is_some(),
        last_n_samples,
        current_host_rate,
        partition_to_request,
        state.cabsim_failed_key,
        rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_CABSIM_REBUILD),
    );
    let nam_needs_rebuild = nam_cab_rebuild_needed(
        state.active_cab_nam.as_ref(),
        state.has_nam_cab,
        last_n_samples,
        current_host_rate,
        partition_to_request,
        state.cabsim_failed_key,
        rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_CABSIM_REBUILD),
    );
    if wav_needs_rebuild || nam_needs_rebuild {
        request_cabsim_rebuild(rt_status, partition_to_request, current_host_rate);
    }
}
