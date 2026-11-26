// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Deterministic silence delivery and bridge-miss classification.

use super::super::rt_callback::{
    handle_spa_pair_fail_closed, silence_available_datas, stamp_chunk,
};
use crate::standalone::pw_host::stream_status::StreamStatusFlags;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::pipeline::MAX_BRIDGE_BUF;
use pipewire as pw;
use std::sync::atomic::Ordering;

/// Deterministic silence delivery for bridge starvation.
///
/// Pure SPA-descriptor kernel (raw integers/pointers, no live PipeWire stream
/// required — mockable by the harness tests). Validates the stereo pair
/// fail-closed with the *silence window* `(0, silence_bytes)` per channel —
/// **not** the full allocated `maxsize` (which may be 64 KiB and exceeds the
/// `MAX_BRIDGE_BUF × 4` safety cap, causing false `E2304` on pause) —,
/// zero-fills exactly `silence_bytes / 4` frames of both output regions,
/// stamps `offset = 0`, `size = silence_bytes`, `stride = 4` on both chunks
/// and registers the starvation occurrence on `rt_status`.
///
/// The caller (`deliver_silence_block`) quantizes `silence_bytes` from the
/// active stream quantum (`last_n_samples`/`requested_buffer_frames`, 128-frame
/// fallback), bounded by both channel capacities and `MAX_BRIDGE_BUF × 4`, so
/// pausing the input or starting before the first block never raises
/// `RT_STATUS_HOST_CONTRACT_VIOLATION` (`E2304`).
///
/// Public so the resilience harness (`tests/service_resilience.rs`)
/// can prove the analytical-silence + buffer-recycle contract deterministically
/// (zero bridge generation → `0.0f32` sequences, no stalls) without a live
/// PipeWire graph.
///
/// Returns the number of frames delivered as silence, or `None` when the host
/// handed us a malformed descriptor (the consolidated harness already raised
/// `RT_STATUS_HOST_CONTRACT_VIOLATION` and silenced both channels).
///
/// # Safety
///
/// The caller must prove — for both channels — that `ptr_l`/`ptr_r` point to
/// writable, `f32`-aligned, non-overlapping regions of `max_l`/`max_r` bytes
/// each, and that `chunk_l`/`chunk_r` are non-null and remain valid (owned by
/// the SPA buffer) for the duration of the call. `silence_bytes` must be a
/// multiple of `sizeof(f32)`, not exceed either channel's `maxsize` nor
/// `MAX_BRIDGE_BUF × sizeof(f32)`. The kernel validates the regions via
/// [`handle_spa_pair_fail_closed`] and only dereferences the chunk pointers
/// after proving them non-null, but the safety contract of the raw pointer
/// arguments is the caller's.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields plus the bounded silence window; signature is stable and shared verbatim by the RT callback and the harness tests"
)]
pub unsafe fn deliver_silence_pair_fail_closed(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *mut pw::spa::sys::spa_chunk,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *mut pw::spa::sys::spa_chunk,
    silence_bytes: usize,
    rt_status: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
) -> Option<usize> {
    // SAFETY: the caller contract of this `unsafe fn` is forwarded verbatim to
    // the untracked kernel: both channels were proven writable/aligned/
    // disjoint and the chunks are valid for the callback duration.
    // SAFETY: `handle_spa_pair_fail_closed` validated the same raw descriptors
    // above; the pointers/chunks remain those of the live dequeued SPA buffer.
    let delivered = unsafe {
        deliver_silence_pair_fail_closed_untracked(
            ptr_l,
            max_l,
            chunk_l,
            ptr_r,
            max_r,
            chunk_r,
            silence_bytes,
            rt_status,
        )
    };
    if delivered.is_some() {
        // Telemetry: one deterministic silence block delivered to the graph.
        stream_status
            .playback_bridge_starvation
            .fetch_add(1, Ordering::Relaxed);
    }
    delivered
}

/// Untracked core of [`deliver_silence_pair_fail_closed`]: delivers analytical
/// silence without touching any stream counter, so the caller can attribute the
/// delivery to the correct telemetry category (starvation, early callback, or
/// fail-closed mute) without double counting.
///
/// # Safety
///
/// Same contract as [`deliver_silence_pair_fail_closed`].
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields plus the bounded silence window; signature is stable and shared verbatim by the RT callback and the harness tests"
)]
unsafe fn deliver_silence_pair_fail_closed_untracked(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *mut pw::spa::sys::spa_chunk,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *mut pw::spa::sys::spa_chunk,
    silence_bytes: usize,
    rt_status: &RtStatusFlags,
) -> Option<usize> {
    let (_n_bytes, n_out) = match handle_spa_pair_fail_closed(
        ptr_l,
        max_l,
        chunk_l,
        0,
        silence_bytes,
        ptr_r,
        max_r,
        chunk_r,
        0,
        silence_bytes,
        rt_status,
    ) {
        Some(v) => v,
        None => {
            stamp_chunk(chunk_l, 0);
            stamp_chunk(chunk_r, 0);
            return None;
        }
    };

    // SAFETY: `check_spa_buffer_pair` proved alignment, bounds, frame
    // symmetry and strict pointer disjunction, so `out_l` is well-formed.
    let out_l = unsafe { std::slice::from_raw_parts_mut(ptr_l as *mut f32, n_out) };
    out_l.fill(0.0);
    // SAFETY: `check_spa_buffer_pair` proved alignment, bounds, frame
    // symmetry and strict pointer disjunction, so `out_r` is well-formed.
    let out_r = unsafe { std::slice::from_raw_parts_mut(ptr_r as *mut f32, n_out) };
    out_r.fill(0.0);

    // SAFETY: both chunk pointers were validated non-null above; the chunk
    // structs are owned by the SPA buffer and stable for the callback duration.
    unsafe {
        let chunk_l_mut = &mut *chunk_l;
        chunk_l_mut.offset = 0;
        chunk_l_mut.size = (n_out * std::mem::size_of::<f32>()) as u32;
        chunk_l_mut.stride = std::mem::size_of::<f32>() as i32;

        let chunk_r_mut = &mut *chunk_r;
        chunk_r_mut.offset = 0;
        chunk_r_mut.size = (n_out * std::mem::size_of::<f32>()) as u32;
        chunk_r_mut.stride = std::mem::size_of::<f32>() as i32;
    }

    Some(n_out)
}

/// Dequeues the playback output buffer and, under bridge starvation, fills it
/// with analytical silence and recycles it.
///
/// The silence window is **quantized by the active stream quantum** — the last
/// processed frame count (`last_n_samples`, fallback to
/// `requested_buffer_frames`, then a 128-frame default) — bounded by both
/// channel capacities and `MAX_BRIDGE_BUF × 4`. It never uses the raw shared
/// memory `maxsize` (e.g. 64 KiB), which would trip the fail-closed SPA window
/// check (`E2304`) hundreds of times per second when the input is paused.
///
/// The buffer is returned to the PipeWire graph by dropping the dequeued
/// `Buffer` at the end of this function — every callback path (success,
/// contract violation or missing buffer) recycles or counts the miss, so the
/// output node never starves for buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SilenceReason {
    /// Format contract broken: fail-closed mute.
    Muted,
    /// No new generation and the next capture quantum was already due.
    Starvation,
    /// No new generation because the callback fired ahead of schedule.
    Early,
}

/// Resolves the active stream quantum in frames from the RT counters.
///
/// Prefers the last processed frame count, falls back to the requested buffer
/// size, and finally to the 128-frame default used by the silence window.
/// This is the quantum the miss classifier must use; it is invariant to the
/// accumulated playback-callback counter (`pb_frame_count`).
#[inline(always)]
pub(super) fn active_quantum_frames(rt_status: &RtStatusFlags) -> u32 {
    let last = rt_status.last_n_samples.load(Ordering::Relaxed);
    if last != 0 {
        last
    } else {
        let requested = rt_status.requested_buffer_frames.load(Ordering::Relaxed);
        if requested != 0 { requested } else { 128 }
    }
}

/// Classifies a playback cycle that found no new bridge generation using the
/// capture timestamp of the last delivered generation plus one quantum period.
/// RT-safe: one RDTSC read on the miss path only (no allocation, no lock).
#[inline(always)]
pub(super) fn classify_missing_bridge_block(
    stream_status: &StreamStatusFlags,
    quantum_frames: u32,
) -> SilenceReason {
    classify_missing_bridge_block_at(stream_status, quantum_frames, rt_setup::rdtsc_nanos())
}

/// Deterministic core of [`classify_missing_bridge_block`] with an explicit
/// monotonic nanosecond timestamp (tests inject the clock).
#[inline(always)]
pub(super) fn classify_missing_bridge_block_at(
    stream_status: &StreamStatusFlags,
    quantum_frames: u32,
    now: u64,
) -> SilenceReason {
    let last_capture_tsc = stream_status
        .playback_last_capture_tsc
        .load(Ordering::Relaxed);
    let rate = stream_status
        .capture_negotiated_rate
        .load(Ordering::Relaxed);
    let period_ns = if rate > 0 && quantum_frames > 0 {
        (quantum_frames as u64).saturating_mul(1_000_000_000) / rate as u64
    } else {
        0
    };
    if last_capture_tsc > 0 && period_ns > 0 && now < last_capture_tsc.saturating_add(period_ns) {
        SilenceReason::Early
    } else {
        SilenceReason::Starvation
    }
}

#[inline(always)]
pub(super) fn deliver_silence_block(
    stream: &pw::stream::Stream,
    rt_status: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
    reason: SilenceReason,
) {
    let mut buf = match stream.dequeue_buffer() {
        Some(b) => b,
        None => {
            rt_status.output_buffer_miss.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let datas = buf.datas_mut();
    if datas.len() < 2 {
        rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        silence_available_datas(datas);
        return;
    }

    let (datas_left, datas_right) = datas.split_at_mut(1);
    let data_l = &mut datas_left[0];
    let data_r = &mut datas_right[0];

    // Raw descriptor reads before any `&mut [u8]`/`&mut [f32]` is formed.
    let (ptr_l, max_l) = (
        data_l.as_raw().data as usize,
        data_l.as_raw().maxsize as usize,
    );
    let (ptr_r, max_r) = (
        data_r.as_raw().data as usize,
        data_r.as_raw().maxsize as usize,
    );
    let (chunk_l, chunk_r) = (data_l.as_raw().chunk, data_r.as_raw().chunk);

    // E2304: silence quantum = last active frame count (or requested
    // quantum, or 128-frame default), expressed in bytes and bounded by both
    // channel capacities and MAX_BRIDGE_BUF × 4. Delivering the shared-memory
    // `maxsize` here would exceed the fail-closed window cap on a 64 KiB
    // buffer and raise a false E2304 on every pause.
    let silence_frames = active_quantum_frames(rt_status);
    let silence_bytes = (silence_frames as usize * std::mem::size_of::<f32>())
        .min(max_l)
        .min(max_r)
        .min(MAX_BRIDGE_BUF * std::mem::size_of::<f32>());

    // Validate, silence and recycle. On a malformed descriptor the harness
    // raises `RT_STATUS_HOST_CONTRACT_VIOLATION`, silences both channels and
    // the buffer still returns to the graph via drop.
    //
    // SAFETY: the SPA `data` pointers/chunks were read as raw integers from the
    // live dequeued buffer (owned by PipeWire and stable for the callback), and
    // the kernel itself rejects null/aligned-overlapping descriptors fail-closed
    // before dereferencing any of them.
    // SAFETY: the caller contract of this `unsafe fn` is forwarded verbatim to
    // the untracked kernel: both channels were proven writable/aligned/
    // disjoint and the chunks are valid for the callback duration.
    // SAFETY: `handle_spa_pair_fail_closed` validated the same raw descriptors
    // above; the pointers/chunks remain those of the live dequeued SPA buffer.
    let delivered = unsafe {
        deliver_silence_pair_fail_closed_untracked(
            ptr_l,
            max_l,
            chunk_l,
            ptr_r,
            max_r,
            chunk_r,
            silence_bytes,
            rt_status,
        )
    };
    if delivered.is_some() {
        // One deterministic silence block delivered to the graph, attributed to
        // exactly one category so the miss counters close.
        let counter = match reason {
            SilenceReason::Muted => &stream_status.playback_muted_callbacks,
            SilenceReason::Starvation => &stream_status.playback_bridge_starvation,
            SilenceReason::Early => &stream_status.playback_early_callbacks,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}
