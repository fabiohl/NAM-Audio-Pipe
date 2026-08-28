// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.4. REAL-TIME DSP LOGIC
//! Acquires the raw system buffer and delegates the heavy lifting to the Audio Factory (pipeline).

use crate::recording::buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, RingPayload,
};
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::pipeline::{
    DspBuffers, DspPipelineContext, MAX_BRIDGE_BUF, capture_dsp_pipeline,
};

use pipewire as pw;
use rtrb::Producer;
use std::sync::atomic::{AtomicBool, Ordering};

/// Runtime FFI contract validation for a single PipeWire buffer channel.
///
/// Returns `(n_bytes, n_samples)` if the contract is satisfied, or `None` if
/// the buffer violates bounds or alignment guarantees. Both the base pointer
/// and the intra-buffer `offset` must be aligned to `align_of::<f32>()`, since
/// the caller subsequently reinterprets the region as `&mut [f32]` (an
/// unaligned `f32` slice is undefined behavior).
///
/// Fail-closed quantum bound (G-RB-003 / T6.2): the valid frame count
/// `size / stride` must not exceed [`MAX_BRIDGE_BUF`]. A spurious SPA
/// descriptor with an oversized quantum is rejected here — before any access
/// to the fixed-capacity `DspBuffers` — so `capture_dsp_pipeline` can never
/// panic on an out-of-bounds copy inside the RT callback.
#[inline(always)]
pub(crate) fn check_ffi_contract(raw: &[u8], offset: usize, size: usize) -> Option<(usize, usize)> {
    let align = std::mem::align_of::<f32>();
    let stride = std::mem::size_of::<f32>();
    if !(raw.as_ptr() as usize).is_multiple_of(align) || !offset.is_multiple_of(align) {
        core::hint::cold_path();
        return None;
    }
    if !size.is_multiple_of(stride) {
        core::hint::cold_path();
        return None;
    }
    let end = offset.checked_add(size)?;
    if end > raw.len() {
        core::hint::cold_path();
        return None;
    }
    let n_samples = size / stride;
    if n_samples > MAX_BRIDGE_BUF {
        core::hint::cold_path();
        return None;
    }
    Some((size, n_samples))
}

/// Consolidated stereo FFI contract validation for a pair of SPA audio
/// channels, with strict anti-aliasing and frame-symmetry proof.
///
/// Validates each channel via [`check_ffi_contract`], then proves that the two
/// mutable memory intervals `[ptr_l .. ptr_l + n_bytes_l)` and
/// `[ptr_r .. ptr_r + n_bytes_r)` are **completely disjoint** and hold the same
/// number of frames. Only after this returns `Some` may the caller form the two
/// `&mut [f32]` slices over the host buffers; forming them earlier would create
/// overlapping mutable references (undefined behavior) when a malformed host
/// hands us identical or partially overlapping descriptors.
///
/// Returns `(n_bytes, n_samples)` (identical for both channels) on success.
#[inline(always)]
pub(crate) fn check_spa_buffer_pair(
    raw_l: &[u8],
    offset_l: usize,
    size_l: usize,
    raw_r: &[u8],
    offset_r: usize,
    size_r: usize,
) -> Option<(usize, usize)> {
    let (n_bytes_l, n_samples_l) = check_ffi_contract(raw_l, offset_l, size_l)?;
    let (n_bytes_r, n_samples_r) = check_ffi_contract(raw_r, offset_r, size_r)?;

    if n_samples_l != n_samples_r {
        core::hint::cold_path();
        return None;
    }

    let p_l = raw_l.as_ptr() as usize + offset_l;
    let p_r = raw_r.as_ptr() as usize + offset_r;
    let end_l = p_l.checked_add(n_bytes_l)?;
    let end_r = p_r.checked_add(n_bytes_r)?;

    if (p_l < end_r) && (p_r < end_l) {
        // Overlap detected (aliasing of mutable buffers) -> reject.
        core::hint::cold_path();
        return None;
    }

    Some((n_bytes_l, n_samples_l))
}

/// Silences both SPA data regions after a contract violation.
///
/// Uses raw pointer writes instead of forming `&mut` slices, so even when the
/// Left/Right descriptors alias the same (or overlapping) memory the zeroing
/// never creates overlapping mutable references. Only regions whose base
/// pointer is non-null are touched; the span is the descriptor's `maxsize`,
/// which is exactly the region `Data::data()` exposes as writable.
#[inline(always)]
pub(crate) fn silence_spa_channels(ptr_l: usize, max_l: usize, ptr_r: usize, max_r: usize) {
    if ptr_l != 0 && max_l > 0 {
        // SAFETY: `maxsize` is the host-declared writable span of the SPA data
        // region (the same span `Data::data()` re-exposes as `&mut [u8]`).
        unsafe { std::ptr::write_bytes(ptr_l as *mut u8, 0, max_l) };
    }
    if ptr_r != 0 && max_r > 0 {
        // SAFETY: ditto for the right channel.
        unsafe { std::ptr::write_bytes(ptr_r as *mut u8, 0, max_r) };
    }
}

/// Reads `(offset, size)` from an SPA chunk descriptor, or `None` when the
/// chunk pointer is null (malformed descriptor).
///
/// The capture path uses the host-declared chunk metadata to learn how many
/// valid audio bytes the host published this quantum. Reading the two scalar
/// fields as integers never forms a reference to the audio bytes themselves.
#[inline(always)]
pub(crate) fn read_chunk_meta(chunk: *const pw::spa::sys::spa_chunk) -> Option<(usize, usize)> {
    if chunk.is_null() {
        core::hint::cold_path();
        return None;
    }
    // SAFETY: `chunk` was validated non-null; the struct is owned by the SPA
    // buffer and stable for the duration of the callback.
    let c = unsafe { &*chunk };
    Some((c.offset as usize, c.size as usize))
}

/// Raises `RT_STATUS_HOST_CONTRACT_VIOLATION` and silences both SPA data
/// regions. The fail-closed reaction shared by the RT callbacks and exercised
/// verbatim by the malformed-FFI harness: after a contract violation the
/// callback must (a) flag the host, (b) zero the buffers so no stale audio
/// reaches the DAC/recording, and (c) return without running the DSP pipeline.
/// Zero allocations, zero panics, zero locks.
#[inline(always)]
pub(crate) fn report_ffi_contract_violation(
    rt_status: &RtStatusFlags,
    ptr_l: usize,
    max_l: usize,
    ptr_r: usize,
    max_r: usize,
) {
    rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    silence_spa_channels(ptr_l, max_l, ptr_r, max_r);
}

/// Pure FFI/SPA descriptor harness: validates a raw stereo channel pair read as
/// integers/pointers, before any mutable slice reference is formed.
///
/// Proves — for both channels — that the data pointer is non-null, the base
/// pointer and intra-buffer offset are `f32`-aligned, the byte count is
/// cardinal, the region is in-bounds, the frame counts are symmetric and the
/// two mutable intervals are strictly disjoint. Returns `(n_bytes, n_samples)`
/// (identical for both channels) or `None` for any malformed descriptor
/// (fail-closed).
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields required by the FFI contract; signature is stable and shared verbatim by the RT callbacks and the harness tests"
)]
fn validate_spa_channel_pair(
    ptr_l: usize,
    max_l: usize,
    offset_l: usize,
    size_l: usize,
    ptr_r: usize,
    max_r: usize,
    offset_r: usize,
    size_r: usize,
) -> Option<(usize, usize)> {
    if ptr_l == 0 || ptr_r == 0 {
        core::hint::cold_path();
        return None;
    }
    // SAFETY: `data` pointers were validated non-null; `maxsize` is the
    // host-declared writable span of each SPA data region (the same span
    // `Data::data()` re-exposes). Shared `&[u8]` views are sound even when the
    // channels alias; they only feed the pure validator below and are dead
    // before any mutable `f32` slice is formed.
    let raw_l: &[u8] = unsafe { std::slice::from_raw_parts(ptr_l as *const u8, max_l) };
    let raw_r: &[u8] = unsafe { std::slice::from_raw_parts(ptr_r as *const u8, max_r) };
    check_spa_buffer_pair(raw_l, offset_l, size_l, raw_r, offset_r, size_r)
}

/// Consolidated fail-closed FFI/SPA descriptor handling shared by the RT
/// callbacks (capture and playback) and by the malformed-FFI harness tests.
///
/// `chunk_l`/`chunk_r` must be non-null (the callbacks need the chunk structs
/// either to read capture metadata or to write playback metadata afterwards);
/// `offset_l`/`size_l` are the resolved valid-data window — capture reads them
/// from the host chunk via [`read_chunk_meta`], playback passes `(0, n_bytes)`.
///
/// On any violation — null data pointer, null chunk, misaligned base or offset,
/// non-cardinal size, out-of-bounds region, asymmetric frame counts or
/// overlapping intervals — it raises `RT_STATUS_HOST_CONTRACT_VIOLATION` on
/// `rt_status`, silences both SPA data regions and returns `None`. Only after a
/// `Some` may the caller form the two `&mut [f32]` slices over host memory.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields required by the FFI contract; signature is stable and shared verbatim by the RT callbacks and the harness tests"
)]
pub(crate) fn handle_spa_pair_fail_closed(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *const pw::spa::sys::spa_chunk,
    offset_l: usize,
    size_l: usize,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *const pw::spa::sys::spa_chunk,
    offset_r: usize,
    size_r: usize,
    rt_status: &RtStatusFlags,
) -> Option<(usize, usize)> {
    if chunk_l.is_null() || chunk_r.is_null() {
        core::hint::cold_path();
        report_ffi_contract_violation(rt_status, ptr_l, max_l, ptr_r, max_r);
        return None;
    }
    let verdict = validate_spa_channel_pair(
        ptr_l, max_l, offset_l, size_l, ptr_r, max_r, offset_r, size_r,
    );
    if verdict.is_none() {
        report_ffi_contract_violation(rt_status, ptr_l, max_l, ptr_r, max_r);
    }
    verdict
}

/// Attempts to send recording metadata for `current_host_rate`.
///
/// The sticky `recording_meta_sent` flag is confirmed only after a successful
/// `push(Metadata)`; a failed push (or an absent producer) leaves it false so
/// the next callback retries. A host sample-rate change invalidates the flag so
/// a new header is emitted for the new rate.
///
/// `recording_failed` (F-RB-009 / T3.3) is the RT-observable failure flag: once
/// the disk worker reports a fatal error it is set and enqueueing is suspended —
/// pushing into a ring whose consumer has exited would only inflate
/// `OVERRUN_COUNT` pointlessly.
#[inline(always)]
fn send_recording_metadata(
    recording_producer: &mut Option<Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    current_host_rate: u32,
    recording_meta_sent: &mut bool,
    recording_meta_rate: &mut u32,
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
) {
    if recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        return;
    }
    if *recording_meta_sent && *recording_meta_rate == current_host_rate {
        return;
    }
    if let Some(producer) = recording_producer.as_mut() {
        let meta = AudioMetadata {
            sample_rate: current_host_rate as f32,
            bit_depth: 32,
            channels: 2,
        };
        if producer.push(RingPayload::Metadata(meta)).is_ok() {
            *recording_meta_sent = true;
            *recording_meta_rate = current_host_rate;
            if let Some(flag) = recording_data_available {
                flag.store(true, Ordering::Relaxed);
            }
        }
    }
}

/// Pushes one interleaved audio block to the recording producer.
///
/// Blocks whose interleaved length exceeds `MAX_BLOCK_SIZE` are dropped and
/// counted in `OVERRUN_COUNT` (fail-closed telemetry) instead of silently
/// vanishing. A full ring also increments the counter. The reusable block is
/// swapped out with a fresh uninitialized one to avoid 16 KiB of memset per
/// quantum in the RT hot path.
///
/// `recording_failed` (F-RB-009 / T3.3) suspends enqueueing as soon as the disk
/// worker reports a fatal error — no panics, no pointless pushes.
#[inline(always)]
fn send_recording_audio(
    recording_producer: &mut Option<Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    n_pw: usize,
    resamp_out_l: &[f32],
    resamp_out_r: &[f32],
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
) {
    if n_pw == 0 || recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        return;
    }
    let Some(producer) = recording_producer.as_mut() else {
        return;
    };
    let interleaved_len = n_pw * 2;
    if interleaved_len > MAX_BLOCK_SIZE {
        core::hint::cold_path();
        OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut block = std::mem::replace(recording_block, AlignedBlock::new_uninit());
    block.fill_planar(&resamp_out_l[..n_pw], &resamp_out_r[..n_pw]);
    if producer.push(RingPayload::Audio(block)).is_err() {
        core::hint::cold_path();
        OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
    } else if let Some(flag) = recording_data_available {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Executes the DSP pipeline on the dequeued PipeWire audio buffer.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Core DSP kernel signature required by design; recording parameters are minimal additions"
)]
pub fn process_dsp_buffer(
    stream: &pw::stream::Stream,
    context: DspPipelineContext,
    buffers: DspBuffers,
    current_host_rate: u32,
    frame_count: &mut u32,
    rt_status_for_process: &RtStatusFlags,
    recording_producer: &mut Option<Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    recording_meta_sent: &mut bool,
    recording_meta_rate: &mut u32,
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
) {
    let mut _buf = match stream.dequeue_buffer() {
        Some(b) => b,
        None => {
            rt_status_for_process
                .input_buffer_miss
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    // T4.3 fail-closed mute: while the negotiated format contract is broken
    // (a divergent renegotiation was rejected by the param_changed listener),
    // the DSP pipeline must not run on potentially wrong-format input. The
    // dequeued buffer is recycled via drop and the bridge publishes no new
    // block — the playback side delivers deterministic silence.
    if rt_status_for_process
        .format_contract_ok
        .load(Ordering::Relaxed)
        == 0
    {
        return;
    }

    let datas = _buf.datas_mut();
    if datas.len() < 2 {
        rt_status_for_process.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        return;
    }
    let (left_datas, right_datas) = datas.split_at_mut(1);
    let (d_l, d_r) = match (left_datas.first_mut(), right_datas.first_mut()) {
        (Some(l), Some(r)) => (l, r),
        _ => {
            rt_status_for_process.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
            return;
        }
    };

    // Reads the raw SPA descriptors as integers/pointers before forming any
    // reference to the audio bytes: `Data::chunk()` panics on a null chunk and
    // `Data::data()` would form `&mut [u8]` aliases when L/R share memory.
    let (ptr_l, max_l) = (d_l.as_raw().data as usize, d_l.as_raw().maxsize as usize);
    let (ptr_r, max_r) = (d_r.as_raw().data as usize, d_r.as_raw().maxsize as usize);
    let (chunk_l, chunk_r) = (d_l.as_raw().chunk, d_r.as_raw().chunk);

    // The host-declared valid-data window. A null chunk (malformed descriptor)
    // yields `(0, 0)`; the consolidated harness below still rejects the pair
    // fail-closed via its own chunk-null proof, so no reference is ever formed
    // from a null chunk and no panic reaches the C trampoline.
    let (offset_l, size_l) = read_chunk_meta(chunk_l).unwrap_or((0, 0));
    let (offset_r, size_r) = read_chunk_meta(chunk_r).unwrap_or((0, 0));

    // Consolidated fail-closed FFI/SPA validation: proves per-channel
    // alignment, bounds, cardinality, frame symmetry and strict pointer
    // disjunction before any mutable `f32` slice is formed. On any violation it
    // raises `RT_STATUS_HOST_CONTRACT_VIOLATION` and silences both channels.
    let Some((_n_bytes, n_samples)) = handle_spa_pair_fail_closed(
        ptr_l,
        max_l,
        chunk_l,
        offset_l,
        size_l,
        ptr_r,
        max_r,
        chunk_r,
        offset_r,
        size_r,
        rt_status_for_process,
    ) else {
        return;
    };

    if n_samples > 0 {
        // SAFETY: `check_spa_buffer_pair` proved per-channel alignment, bounds,
        // frame symmetry and strict pointer disjunction, so the two `&mut [f32]`
        // below are well-formed and non-overlapping.
        let samples_l =
            unsafe { std::slice::from_raw_parts_mut((ptr_l + offset_l) as *mut f32, n_samples) };
        let samples_r =
            unsafe { std::slice::from_raw_parts_mut((ptr_r + offset_r) as *mut f32, n_samples) };

        send_recording_metadata(
            recording_producer,
            current_host_rate,
            recording_meta_sent,
            recording_meta_rate,
            recording_data_available,
            recording_failed,
        );

        let should_measure = (*frame_count & 0xF) == 0;
        *frame_count = frame_count.wrapping_add(1);

        let start_nanos = if should_measure {
            rt_setup::rdtsc_nanos()
        } else {
            0
        };

        if (*frame_count & 0x3FF) == 0 {
            unsafe {
                neural_amp_modeler_rs::math::common::set_daz_ftz();
            }
        }

        let n_pw = capture_dsp_pipeline(
            samples_l,
            samples_r,
            n_samples,
            context,
            DspBuffers {
                resamp_mid_l: &mut *buffers.resamp_mid_l,
                resamp_mid_r: &mut *buffers.resamp_mid_r,
                resamp_out_l: &mut *buffers.resamp_out_l,
                resamp_out_r: &mut *buffers.resamp_out_r,
                model_out_l: &mut *buffers.model_out_l,
                model_out_r: &mut *buffers.model_out_r,
                os_in_l: &mut *buffers.os_in_l,
                os_in_r: &mut *buffers.os_in_r,
                os_model_l: &mut *buffers.os_model_l,
                os_model_r: &mut *buffers.os_model_r,
                crossfade_scratch_l: buffers.crossfade_scratch_l,
                crossfade_scratch_r: buffers.crossfade_scratch_r,
            },
            current_host_rate,
        );

        if *recording_meta_sent {
            send_recording_audio(
                recording_producer,
                n_pw,
                &buffers.resamp_out_l[..],
                &buffers.resamp_out_r[..],
                recording_block,
                recording_data_available,
                recording_failed,
            );
        }

        if should_measure {
            let elapsed_nanos = rt_setup::rdtsc_nanos().wrapping_sub(start_nanos);
            rt_status_for_process
                .dsp_cycle_time
                .store(elapsed_nanos, Ordering::Relaxed);
            rt_status_for_process.latency_hist.record(elapsed_nanos);

            let elapsed_secs = elapsed_nanos as f64 / 1_000_000_000.0;
            let budget_secs = (n_samples as f64 / current_host_rate as f64) * 0.85;
            if elapsed_secs > budget_secs {
                rt_status_for_process
                    .dsp_overloads
                    .fetch_add(1, Ordering::Relaxed);
            }
        }

        let n = n_samples as u32;
        if rt_status_for_process.last_n_samples.load(Ordering::Relaxed) != n {
            rt_status_for_process
                .last_n_samples
                .store(n, Ordering::Relaxed);
            rt_status_for_process
                .requested_buffer_frames
                .store(n, Ordering::Relaxed);
            rt_status_for_process
                .set_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_QUANTUM_LOG);
        }
    }
}

#[cfg(test)]
#[path = "process_test.rs"]
mod tests;
