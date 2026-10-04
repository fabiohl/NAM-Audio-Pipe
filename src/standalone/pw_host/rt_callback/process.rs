// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Real-time DSP logic
//! Acquires the raw system buffer and delegates the heavy lifting to the Audio Factory (pipeline).

use crate::recording::buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, OVERRUN_FRAMES_COUNT, RingPayload,
};
use crate::recording::transport::{RecordingSender, assert_inline_rollback_build};
use crate::standalone::pw_host::StreamStatusFlags;
use crate::standalone::pw_host::capture::state::{CaptureState, RtHostChannels};
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{
    GcItem, RT_STATUS_HOST_CONTRACT_VIOLATION, RT_STATUS_RESAMP_SWAP_PENDING, RtStatusFlags,
    SwapBudget,
};
use neural_amp_modeler_rs::dsp::pipeline::{
    DspPipelineContext, MAX_BRIDGE_BUF, StreamingDspBuffers, capture_dsp_pipeline_streaming,
};
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;
use neural_amp_modeler_rs::math::dsp::gain_lut::GainLUT;

use super::{
    STRUCTURAL_SWAPS_PER_CALLBACK, drain_cabsims, drain_os_engines, drain_resamplers,
    drain_slimmable_models, receive_commands, sync_rate, try_slimmable_rebuild,
};

use pipewire as pw;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Runtime FFI contract validation for a single PipeWire buffer channel.
///
/// Returns `(n_bytes, n_samples)` if the contract is satisfied, or `None` if
/// the buffer violates bounds or alignment guarantees. Both the base pointer
/// and the intra-buffer `offset` must be aligned to `align_of::<f32>()`, since
/// the caller subsequently reinterprets the region as `&mut [f32]` (an
/// unaligned `f32` slice is undefined behavior).
///
/// Fail-closed quantum bound: the valid frame count `size / stride` must not
/// exceed [`MAX_BRIDGE_BUF`]. A spurious SPA descriptor with an oversized
/// quantum is rejected here — before any access to the fixed-capacity `DspBuffers` —
/// so `capture_dsp_pipeline` can never panic on an out-of-bounds copy inside
/// the RT callback.
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

/// Central low-level zeroing kernel shared by every fail-closed silence path
/// of this module (see the wrappers below). One `unsafe` site, one documented
/// contract: the zeroing itself performs only byte stores of `0` and never
/// reads the region, so even when Left/Right descriptors alias (overlapping
/// host buffers) no `&mut` aliasing is formed and no uninitialized read occurs.
///
/// # Safety
///
/// For the region `[ptr, ptr + bytes)` the caller must guarantee:
/// - `ptr` is non-null and f32-aligned;
/// - `bytes` is a non-zero cardinal multiple of `sizeof::<f32>()`, already
///   bounded to the region the SPA buffer declared writable (the checked
///   wrapper [`try_zero_region`] enforces the `MAX_BRIDGE_BUF` ceiling);
/// - the region is writable for the whole call (SPA buffer handed to this
///   callback).
///
/// The kernel applies no bound of its own — every ceiling is enforced by the
/// checked wrapper [`try_zero_region`] before the call.
#[inline(always)]
unsafe fn zero_region(ptr: usize, bytes: usize) {
    debug_assert!(ptr != 0 && ptr.is_multiple_of(std::mem::align_of::<f32>()));
    debug_assert!(bytes > 0 && bytes.is_multiple_of(std::mem::size_of::<f32>()));
    // SAFETY: the caller contract documented above is upheld; `write_bytes`
    // only stores zeros and does not read the destination.
    unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, bytes) };
}

/// Checked wrapper for [`zero_region`]: clamps the span to the
/// [`MAX_BRIDGE_BUF`]-bounded fail-closed ceiling and validates the raw FFI
/// descriptor fields (non-null base pointer, f32 alignment, non-zero cardinal
/// byte count) before any memory access. Returns the number of bytes actually
/// zeroed — `0` when the descriptor was skipped (fail-closed: malformed
/// descriptors are never dereferenced).
#[inline(always)]
fn try_zero_region(ptr: usize, bytes: usize) -> usize {
    let bytes = bytes.min(MAX_BRIDGE_BUF * std::mem::size_of::<f32>());
    let align = std::mem::align_of::<f32>();
    let stride = std::mem::size_of::<f32>();
    if ptr == 0 || !ptr.is_multiple_of(align) || bytes == 0 || !bytes.is_multiple_of(stride) {
        return 0;
    }
    // SAFETY: validated non-null, f32-aligned and cardinal above; `bytes` is
    // clamped to the ceiling of `MAX_BRIDGE_BUF` frames and, by the caller
    // contract, never exceeds the SPA-declared writable region.
    unsafe { zero_region(ptr, bytes) };
    bytes
}

/// Stamps a silenced SPA chunk's valid-data window so the host consumes
/// exactly the zeroed interval (`offset = 0`, `size = bytes`,
/// `stride = sizeof(f32)`) instead of replaying trailing stale data past the
/// region actually cleared.
///
/// Defensive by construction: a null or misaligned chunk pointer is simply
/// ignored (there is no descriptor to stamp), matching the fail-closed
/// contract of [`read_chunk_meta`]. Zero allocations, zero panics.
#[inline(always)]
fn stamp_chunk(chunk: *mut pw::spa::sys::spa_chunk, bytes: usize) {
    if chunk.is_null()
        || !(chunk as usize).is_multiple_of(std::mem::align_of::<pw::spa::sys::spa_chunk>())
    {
        return;
    }
    // SAFETY: `chunk` was validated non-null and correctly aligned; the
    // struct is owned by the SPA buffer and stable for the duration of the
    // callback (same ownership contract as `read_chunk_meta`).
    unsafe {
        let chunk = &mut *chunk;
        chunk.offset = 0;
        chunk.size = bytes as u32;
        chunk.stride = std::mem::size_of::<f32>() as i32;
    }
}

/// Silences both SPA data regions after a contract violation, bounded to at most
/// [`MAX_BRIDGE_BUF`] frames per channel (`MAX_BRIDGE_BUF * sizeof(f32)` bytes).
///
/// Thin wrapper over [`try_zero_region`] (central SAFETY + zeroing kernel): raw
/// pointer byte stores instead of forming `&mut` slices, so even when the
/// Left/Right descriptors alias the same (or overlapping) memory the zeroing
/// never creates overlapping mutable references. Only regions whose base
/// pointer is non-null, f32-aligned, and cardinal in size are touched.
///
/// **Bounded write invariant**: even if the host declares an oversized `maxsize`
/// (e.g. 1 MiB or malformed integer), zeroing never touches memory beyond
/// `MAX_BRIDGE_BUF` frames (32,768 bytes), preserving RT deadlines in the fail-closed path.
///
/// Measured: bounded zeroing to 32 KiB in fail-closed executes in ~0.4µs (< 0.15% of 333µs quantum at 48kHz).
#[inline(always)]
pub(crate) fn silence_spa_channels(ptr_l: usize, max_l: usize, ptr_r: usize, max_r: usize) {
    try_zero_region(ptr_l, max_l);
    try_zero_region(ptr_r, max_r);
}
/// Silences every present SPA data descriptor, strictly bounded to at most
/// [`MAX_BRIDGE_BUF`] frames per channel (`MAX_BRIDGE_BUF * sizeof(f32)` bytes).
///
/// Thin wrapper over [`try_zero_region`] + [`stamp_chunk`] (central SAFETY,
/// ceiling and zeroing kernel). Pure descriptor kernel for zeroing available
/// data regions, mockable by harness tests.
#[cfg(test)]
#[inline(always)]
pub(crate) fn silence_available_descriptors(
    descriptors: &mut [(usize, usize, *mut pw::spa::sys::spa_chunk)],
) {
    for (ptr, maxsize, chunk_ptr) in descriptors.iter_mut() {
        let zeroed = try_zero_region(*ptr, *maxsize);
        if zeroed > 0 {
            stamp_chunk(*chunk_ptr, zeroed);
        }
    }
}

/// Silences every present SPA data region of a malformed stereo buffer, bounded
/// to at most [`MAX_BRIDGE_BUF`] frames per channel.
///
/// Fail-closed guarantee: a buffer handed back to the PipeWire graph
/// must never carry audio content that was not written by this callback. When
/// the host violates the negotiated stereo contract (`datas.len() < 2`) on either capture
/// or playback, no stereo pair can be validated, so every region present is zeroed via raw
/// pointer writes (no `&mut` aliasing is formed), strictly bounded to at most
/// `MAX_BRIDGE_BUF` frames (`MAX_BRIDGE_BUF * sizeof(f32)` bytes).
/// Valid chunk metadata is stamped with `offset=0`, `size=silence_bytes` so the
/// host consumes only the zeroed interval without replaying trailing stale data.
#[inline(always)]
pub(crate) fn silence_available_datas(datas: &mut [pw::spa::buffer::Data]) {
    for data in datas.iter_mut() {
        let raw = data.as_raw();
        let zeroed = try_zero_region(raw.data as usize, raw.maxsize as usize);
        if zeroed > 0 {
            stamp_chunk(raw.chunk, zeroed);
        }
    }
}

/// Verdict of reading an SPA chunk's valid-data window.
///
/// The capture path must distinguish "no data published" (legitimate) from
/// "non-null chunk with malformed metadata" (a real host contract violation
/// that must raise `E2304` — never silent `(0, 0)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkWindow {
    /// Host-declared valid window `(offset, size)`.
    Valid(usize, usize),
    /// Chunk pointer is null or misaligned — no descriptor to read. The
    /// consolidated fail-closed harness still rejects the pair via its own
    /// chunk-null/alignment proof (`E2304`), so the observable behavior is
    /// unchanged from the original capture path.
    Absent,
    /// Non-null chunk whose `stride` is not `sizeof(f32)` or whose
    /// `SPA_CHUNK_FLAG_CORRUPTED` bit is set. A genuine host contract
    /// violation: the caller must raise `RT_STATUS_HOST_CONTRACT_VIOLATION`
    /// instead of degrading silently to a zero window.
    Malformed,
}

/// Reads `(offset, size)` from an SPA chunk descriptor, classifying the
/// outcome per [`ChunkWindow`]:
///
/// - [`ChunkWindow::Absent`] — chunk pointer is null or misaligned (no
///   descriptor to read);
/// - [`ChunkWindow::Malformed`] — non-null chunk with an invalid stride or the
///   corrupted flag set;
/// - [`ChunkWindow::Valid`] — the host-declared valid-data window.
///
/// The capture path uses the host-declared chunk metadata to learn how many
/// valid audio bytes the host published this quantum. Reading the scalar
/// fields as integers never forms a reference to the audio bytes themselves.
#[inline(always)]
pub(crate) fn read_chunk_meta(chunk: *const pw::spa::sys::spa_chunk) -> ChunkWindow {
    if chunk.is_null()
        || !(chunk as usize).is_multiple_of(std::mem::align_of::<pw::spa::sys::spa_chunk>())
    {
        core::hint::cold_path();
        return ChunkWindow::Absent;
    }
    // SAFETY: `chunk` was validated non-null and correctly aligned; the struct is owned by the SPA
    // buffer and stable for the duration of the callback.
    let c = unsafe { &*chunk };
    if c.stride != std::mem::size_of::<f32>() as i32 || (c.flags & 1) != 0 {
        core::hint::cold_path();
        return ChunkWindow::Malformed;
    }
    ChunkWindow::Valid(c.offset as usize, c.size as usize)
}

/// Resolves one channel's capture-path valid-data window, applying the
/// fail-closed classification.
///
/// - [`ChunkWindow::Valid`] → `Some((offset, size))` — the host-declared window;
/// - [`ChunkWindow::Absent`] → `Some((0, 0))` — no descriptor to read; the
///   consolidated [`handle_spa_pair_fail_closed`] harness below still rejects a
///   null/misaligned chunk pair via its own proof;
/// - [`ChunkWindow::Malformed`] → raises the fail-closed contract violation
///   (`RT_STATUS_HOST_CONTRACT_VIOLATION` / `E2304`) and silences both
///   channels, returning `None` — a corrupted non-null chunk never degrades
///   silently to `(0, 0)` with clean telemetry.
///
/// Zero allocations, zero panics, zero locks (RT-safe).
#[inline(always)]
pub fn resolve_capture_chunk_window(
    chunk: *const pw::spa::sys::spa_chunk,
    ptr_l: usize,
    max_l: usize,
    ptr_r: usize,
    max_r: usize,
    rt_status: &RtStatusFlags,
) -> Option<(usize, usize)> {
    match read_chunk_meta(chunk) {
        ChunkWindow::Valid(offset, size) => Some((offset, size)),
        ChunkWindow::Absent => Some((0, 0)),
        ChunkWindow::Malformed => {
            core::hint::cold_path();
            report_ffi_contract_violation(rt_status, ptr_l, max_l, ptr_r, max_r);
            None
        }
    }
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
    // SAFETY: `data` pointers were validated non-null; `maxsize` is bounded
    // to MAX_BRIDGE_BUF * sizeof(f32) so `from_raw_parts` never forms an unbounded
    // slice even on malformed or huge host descriptors.
    // Shared `&[u8]` views are sound even when the channels alias; they only
    // feed the pure validator below and are dead before any mutable `f32` slice is formed.
    let max_cap = MAX_BRIDGE_BUF * std::mem::size_of::<f32>();
    let bound_l = max_l.min(max_cap);
    let bound_r = max_r.min(max_cap);
    let raw_l: &[u8] = unsafe { std::slice::from_raw_parts(ptr_l as *const u8, bound_l) };
    let raw_r: &[u8] = unsafe { std::slice::from_raw_parts(ptr_r as *const u8, bound_r) };
    check_spa_buffer_pair(raw_l, offset_l, size_l, raw_r, offset_r, size_r)
}

/// Consolidated fail-closed FFI/SPA descriptor handling shared by the RT
/// callbacks (capture and playback) and by the malformed-FFI harness tests.
///
/// `chunk_l`/`chunk_r` must be non-null and correctly aligned;
/// `offset_l`/`size_l` are the resolved valid-data window — capture reads them
/// from the host chunk via [`read_chunk_meta`], playback passes `(0, n_bytes)`.
///
/// On any violation — null data pointer, null/misaligned chunk, misaligned base or offset,
/// non-cardinal size, out-of-bounds region, asymmetric frame counts or
/// overlapping intervals — it raises `RT_STATUS_HOST_CONTRACT_VIOLATION` on
/// `rt_status`, silences both SPA data regions and returns `None`. Only after a
/// `Some` may the caller form the two `&mut [f32]` slices over host memory.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields required by the FFI contract; signature is stable and shared verbatim by the RT callbacks and the harness tests"
)]
pub fn handle_spa_pair_fail_closed(
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
    let chunk_align = std::mem::align_of::<pw::spa::sys::spa_chunk>();
    if chunk_l.is_null()
        || chunk_r.is_null()
        || !(chunk_l as usize).is_multiple_of(chunk_align)
        || !(chunk_r as usize).is_multiple_of(chunk_align)
    {
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
/// push into the transport's control channel (pool transport) or the
/// inline ring (rollback); a failed push (or an absent channel) leaves it
/// false so the next callback retries. A host sample-rate change invalidates
/// the flag so a new header is emitted for the new rate.
///
/// `recording_failed` is the RT-observable failure flag: once
/// the disk worker reports a fatal error it is set and enqueueing is suspended —
/// pushing into a channel whose consumer has exited would only inflate
/// `OVERRUN_COUNT`/`OVERRUN_FRAMES_COUNT` pointlessly.
#[inline(always)]
fn send_recording_metadata(
    recording_sender: &mut RecordingSender,
    current_host_rate: u32,
    recording_meta_sent: &mut bool,
    recording_meta_rate: &mut u32,
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
) {
    if recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        if let Some(flag) = recording_data_available {
            flag.store(false, Ordering::Relaxed);
        }
        return;
    }
    if *recording_meta_rate != current_host_rate {
        *recording_meta_sent = false;
    }
    if *recording_meta_sent {
        return;
    }
    let meta = AudioMetadata {
        sample_rate: current_host_rate as f32,
        bit_depth: 32,
        channels: 2,
    };
    if recording_sender.try_push_metadata(meta) {
        *recording_meta_sent = true;
        *recording_meta_rate = current_host_rate;
        if let Some(flag) = recording_data_available {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Pushes one audio block to the recording transport.
///
/// `MAX_BLOCK_SIZE` (16384 samples = 8192 stereo frames) covers the largest
/// legal host quantum (`MAX_BRIDGE_BUF`), so every accepted quantum is
/// persisted integrally. Blocks whose interleaved length exceeds
/// `MAX_BLOCK_SIZE` are dropped and counted in `OVERRUN_COUNT` +
/// `OVERRUN_FRAMES_COUNT` (fail-closed telemetry) instead of silently
/// vanishing. On the promoted pool transport the block is written into
/// a preallocated slot via `try_acquire` → `fill_planar` → `publish`; a pool
/// exhaustion (`try_acquire() == None`) also increments both counters,
/// mirroring a full inline ring.
///
/// `recording_failed` suspends enqueueing as soon as the disk
/// worker reports a fatal error — no panics, no pointless pushes.
#[inline(always)]
pub fn send_recording_audio(
    recording_sender: &mut RecordingSender,
    n_pw: usize,
    resamp_out_l: &[f32],
    resamp_out_r: &[f32],
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
) {
    if n_pw == 0 {
        return;
    }
    if recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        if let Some(flag) = recording_data_available {
            flag.store(false, Ordering::Relaxed);
        }
        return;
    }
    let interleaved_len = n_pw * 2;
    if interleaved_len > MAX_BLOCK_SIZE {
        core::hint::cold_path();
        OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
        OVERRUN_FRAMES_COUNT.fetch_add(n_pw as u64, Ordering::Relaxed);
        return;
    }
    match recording_sender {
        RecordingSender::Pool { pool, .. } => {
            // Zero-copy pool path: acquire a preallocated slot, fill it in
            // place and publish the descriptor — zero allocations, the
            // 64 KiB payload never moves.
            let Some(producer) = pool.as_mut() else {
                return;
            };
            let Some(mut slot) = producer.try_acquire() else {
                // Pool exhausted (all slots in flight) — the pool's overrun
                // condition, accounted exactly like a full inline ring.
                core::hint::cold_path();
                OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
                OVERRUN_FRAMES_COUNT.fetch_add(n_pw as u64, Ordering::Relaxed);
                return;
            };
            slot.block_mut()
                .fill_planar(&resamp_out_l[..n_pw], &resamp_out_r[..n_pw]);
            if slot.publish() {
                if let Some(flag) = recording_data_available {
                    flag.store(true, Ordering::Relaxed);
                }
            } else {
                core::hint::cold_path();
                OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
                OVERRUN_FRAMES_COUNT.fetch_add(n_pw as u64, Ordering::Relaxed);
            }
        }
        RecordingSender::Inline(producer) => {
            // Inline ring path (documented rollback transport): swap out the
            // reusable block to avoid 64 KiB of memset per quantum, fill and
            // push into the inline ring.
            //
            // Rollback-only tripwire: with the pool transport compiled as the
            // production default, an `Inline` sender can only reach this arm
            // through hand-wired construction outside `create_recording_transport`,
            // so debug builds accuse the path while `RECORDING_POOL_TRANSPORT`
            // stays `true`. The deliberate rollback (const flipped to `false`)
            // passes the guard, and release builds compile the assertion away —
            // the inline runtime behavior itself is untouched.
            assert_inline_rollback_build();
            let Some(producer) = producer.as_mut() else {
                return;
            };
            let mut block = std::mem::replace(recording_block, AlignedBlock::new_uninit());
            block.fill_planar(&resamp_out_l[..n_pw], &resamp_out_r[..n_pw]);
            if producer.push(RingPayload::Audio(block)).is_err() {
                core::hint::cold_path();
                OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
                OVERRUN_FRAMES_COUNT.fetch_add(n_pw as u64, Ordering::Relaxed);
            } else if let Some(flag) = recording_data_available {
                flag.store(true, Ordering::Relaxed);
            }
        }
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
    stream_resample: &mut StreamingResampleBuffer,
    buffers: StreamingDspBuffers,
    current_host_rate: u32,
    frame_count: &mut u32,
    rt_status_for_process: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
    recording_sender: &mut RecordingSender,
    recording_meta_sent: &mut bool,
    recording_meta_rate: &mut u32,
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
    t_cap_start: u64,
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

    // Fail-closed mute: while the negotiated format contract is broken
    // (a divergent renegotiation was rejected by the param_changed listener),
    // the DSP pipeline must not run on potentially wrong-format input. The
    // dequeued buffer is recycled via drop and the bridge publishes no new
    // block — the playback side delivers deterministic silence.
    if stream_status.format_contract_ok.load(Ordering::Relaxed) == 0 {
        return;
    }

    let datas = _buf.datas_mut();
    if datas.len() < 2 {
        rt_status_for_process.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        silence_available_datas(datas);
        return;
    }
    let (left_datas, right_datas) = datas.split_at_mut(1);
    let (d_l, d_r) = match (left_datas.first_mut(), right_datas.first_mut()) {
        (Some(l), Some(r)) => (l, r),
        _ => {
            rt_status_for_process.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
            silence_available_datas(datas);
            return;
        }
    };

    // Reads the raw SPA descriptors as integers/pointers before forming any
    // reference to the audio bytes: `Data::chunk()` panics on a null chunk and
    // `Data::data()` would form `&mut [u8]` aliases when L/R share memory.
    let (ptr_l, max_l) = (d_l.as_raw().data as usize, d_l.as_raw().maxsize as usize);
    let (ptr_r, max_r) = (d_r.as_raw().data as usize, d_r.as_raw().maxsize as usize);
    let (chunk_l, chunk_r) = (d_l.as_raw().chunk, d_r.as_raw().chunk);

    // The host-declared valid-data window. Three cases are distinguished:
    // a *valid* window is used as declared; an *absent* chunk (null/misaligned
    // descriptor) yields `(0, 0)` and the consolidated harness below still rejects
    // the pair fail-closed via its own chunk-null/alignment proof; a *malformed*
    // non-null chunk (`stride != 4` or `SPA_CHUNK_FLAG_CORRUPTED`) raises `E2304`
    // here instead of silently degrading to `(0, 0)` — no reference is ever
    // formed from a null chunk, no panic reaches the C trampoline, and no
    // malformed path produces "silence with clean telemetry".
    let Some((offset_l, size_l)) =
        resolve_capture_chunk_window(chunk_l, ptr_l, max_l, ptr_r, max_r, rt_status_for_process)
    else {
        return;
    };
    let Some((offset_r, size_r)) =
        resolve_capture_chunk_window(chunk_r, ptr_l, max_l, ptr_r, max_r, rt_status_for_process)
    else {
        return;
    };

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
            recording_sender,
            current_host_rate,
            recording_meta_sent,
            recording_meta_rate,
            recording_data_available,
            recording_failed,
        );

        let should_measure = (*frame_count & 0xF) == 0;
        *frame_count = frame_count.wrapping_add(1);

        // 1. CAPTURE TOTAL (callback start → end of SPA validation/dequeue)
        // Measured: TSC overhead =~15ns per sample (LFENCE+RDTSC), total < 0.05% of 333µs quantum
        if t_cap_start > 0 {
            stream_status
                .capture_start_tsc
                .store(t_cap_start, Ordering::Relaxed);
        }

        if should_measure && t_cap_start > 0 {
            let t_spa_valid = rt_setup::rdtsc_nanos();
            let cap_nanos = t_spa_valid.saturating_sub(t_cap_start);
            stream_status
                .capture_cycle_time
                .store(cap_nanos, Ordering::Relaxed);
            stream_status.capture_hist.record(cap_nanos);
        }

        // Denormal policy re-arm: this data-loop thread executes neural
        // inference whose math relies on FTZ/DAZ being active (constant-cost
        // denormal handling). The policy lives in per-thread CPU state, so it
        // must survive alongside the thread for its whole lifetime.
        if (*frame_count & 0x3FF) == 0 {
            // SAFETY: `set_daz_ftz` only sets the FTZ/DAZ bits of the calling
            // thread's MXCSR control word (x86_64 `stmxcsr`/`ldmxcsr` on a
            // stack local): it reads and writes no process memory, allocates
            // nothing, never unwinds, and the effect is confined to this
            // thread — sound to invoke in the audio callback. Idempotent (OR
            // of the control bits into the current MXCSR), so arming it again
            // here every 1024 quanta is a defensive re-assertion of the
            // denormal policy against foreign code running on this same
            // thread that might have reset MXCSR; repeated arming cannot
            // accumulate state.
            unsafe {
                neural_amp_modeler_rs::math::common::set_daz_ftz();
            }
        }

        // 2. DSP CORE (pre-DSP → post-DSP)
        let t_dsp_start = if should_measure {
            rt_setup::rdtsc_nanos()
        } else {
            0
        };

        // T9.4/F-PERF-17: the streaming pipeline never reads
        // `resamp_mid_l/r` / `model_out_l/r` — pass the `StreamingDspBuffers`
        // working set (8 buffers) and let the engine's `Into<DspBuffers>`
        // adapter zero-slice the intermediates, removing 128 KiB of dead
        // per-instance allocations from `CaptureState`.
        let n_pw = capture_dsp_pipeline_streaming(
            samples_l,
            samples_r,
            n_samples,
            context,
            stream_resample,
            StreamingDspBuffers {
                resamp_out_l: &mut *buffers.resamp_out_l,
                resamp_out_r: &mut *buffers.resamp_out_r,
                os_in_l: &mut *buffers.os_in_l,
                os_in_r: &mut *buffers.os_in_r,
                os_model_l: &mut *buffers.os_model_l,
                os_model_r: &mut *buffers.os_model_r,
                crossfade_scratch_l: buffers.crossfade_scratch_l,
                crossfade_scratch_r: buffers.crossfade_scratch_r,
            },
            current_host_rate,
        );

        let t_dsp_end = if should_measure {
            rt_setup::rdtsc_nanos()
        } else {
            0
        };

        if should_measure && t_dsp_start > 0 {
            let elapsed_nanos = t_dsp_end.saturating_sub(t_dsp_start);
            if rt_status_for_process
                .first_block_nanos
                .load(Ordering::Relaxed)
                == 0
            {
                rt_status_for_process
                    .first_block_nanos
                    .store(elapsed_nanos.max(1), Ordering::Relaxed);
            }
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

        // 3. RECORD ENQUEUE (pre-push → post-push)
        if *recording_meta_sent {
            let t_rec_start = if should_measure {
                rt_setup::rdtsc_nanos()
            } else {
                0
            };

            send_recording_audio(
                recording_sender,
                n_pw,
                &buffers.resamp_out_l[..],
                &buffers.resamp_out_r[..],
                recording_block,
                recording_data_available,
                recording_failed,
            );

            if should_measure && t_rec_start > 0 {
                let rec_nanos = rt_setup::rdtsc_nanos().saturating_sub(t_rec_start);
                stream_status
                    .record_cycle_time
                    .store(rec_nanos, Ordering::Relaxed);
                stream_status.record_hist.record(rec_nanos);
            }
        } else if recording_sender.has_producer()
            && n_pw > 0
            && !recording_failed.is_some_and(|f| f.load(Ordering::Acquire))
        {
            core::hint::cold_path();
            OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
            OVERRUN_FRAMES_COUNT.fetch_add(n_pw as u64, Ordering::Relaxed);
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

/// Verdict of running [`rt_quantum_prologue`].
///
/// Distinguishes whether the quantum should proceed with audio buffer dequeue and DSP processing,
/// or whether DSP execution must be skipped (e.g. while an off-RT resampler swap is in flight,
/// or following an FFI contract violation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrologueOutcome {
    /// Prologue completed successfully; DSP processing may proceed for this quantum.
    Proceed {
        /// Active host sample rate synchronized for this quantum.
        current_host_rate: u32,
        /// Number of structural swaps applied in this quantum (budgeted).
        structural_applied: usize,
        /// Number of scalar parameter payloads popped from the param SPSC this quantum.
        param_pops: usize,
    },
    /// The quantum must be skipped: either an off-RT resampler swap is pending,
    /// or a drain contract violation occurred. The caller must skip DSP processing
    /// and deliver deterministic silence / dequeue without processing.
    Skip {
        /// Active host sample rate at the time of skip (or 0 if contract violation).
        current_host_rate: u32,
        /// Number of structural swaps applied in this quantum (if any).
        structural_applied: usize,
        /// Number of scalar parameter payloads popped from the param SPSC this quantum (if any).
        param_pops: usize,
    },
}

#[cfg(feature = "testing")]
impl PrologueOutcome {
    /// Returns the synchronized host rate.
    #[inline(always)]
    pub fn current_host_rate(&self) -> u32 {
        match *self {
            PrologueOutcome::Proceed {
                current_host_rate, ..
            }
            | PrologueOutcome::Skip {
                current_host_rate, ..
            } => current_host_rate,
        }
    }

    /// Returns the count of structural swaps applied during this quantum.
    #[inline(always)]
    pub fn structural_applied(&self) -> usize {
        match *self {
            PrologueOutcome::Proceed {
                structural_applied, ..
            }
            | PrologueOutcome::Skip {
                structural_applied, ..
            } => structural_applied,
        }
    }

    /// Returns the count of scalar parameter payloads popped during this quantum.
    #[inline(always)]
    pub fn param_pops(&self) -> usize {
        match *self {
            PrologueOutcome::Proceed { param_pops, .. }
            | PrologueOutcome::Skip { param_pops, .. } => param_pops,
        }
    }
}

/// Unified real-time callback prologue shared verbatim by the production PipeWire capture
/// callback (`capture/setup.rs`) and the offline swap harness (`rt_callback/harness.rs`).
///
/// Executes the exact, canonical sequence instruction-by-instruction with zero heap allocations,
/// zero locks, and zero log calls:
///
/// 1. **GC Parking-Lot Flush**: pushes deferred retired items to the SPSC GC channel, clearing
///    the dirty latch when all 16 slots are drained.
/// 2. **Shared Command Budgeting**: enforces [`STRUCTURAL_SWAPS_PER_CALLBACK`] across all drains
///    via [`SwapBudget`].
/// 3. **Budgeted Drains**:
///    - Resampler swap drain ([`drain_resamplers`])
///    - Cab-sim swap drain ([`drain_cabsims`])
///    - Param & model swap consumer ([`receive_commands`])
///    - Slimmable model rebuild trigger ([`try_slimmable_rebuild`])
///    - Slimmable model drain ([`drain_slimmable_models`])
///    - Oversampling engines drain ([`drain_os_engines`])
/// 4. **Rate Synchronization**: reads the target host rate ([`sync_rate`]) using `state.shared_target_rate`.
/// 5. **Gain Multiplier Recomputation**: recomputes input/output gain multipliers when parameters changed.
/// 6. **Rollback Guard**: inspects `RT_STATUS_RESAMP_SWAP_PENDING`; if a rebuild failed, performs the
///    fail-open rollback to previous resampler generation; if a rebuild is in flight, returns [`PrologueOutcome::Skip`].
#[inline(always)]
pub fn rt_quantum_prologue(
    state: &mut CaptureState,
    channels: &mut RtHostChannels,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    rt_status: &Arc<RtStatusFlags>,
    lut: &GainLUT,
) -> PrologueOutcome {
    // 1. Fast-path parking-lot flush: skip the 16-slot scan if no GC item was ever parked since last drain.
    if parking_lot_dirty.load(Ordering::Acquire) {
        let mut any_remaining = false;
        for slot in parking_lot.iter_mut() {
            let Some(old) = slot.take() else { continue };
            if let Err(rtrb::PushError::Full(old_back)) = channels.gc_producer.push(old) {
                *slot = Some(old_back);
                any_remaining = true;
                break;
            }
        }
        if !any_remaining {
            parking_lot_dirty.store(false, Ordering::Release);
        }
    }

    // 2. Command Budgeting: shared structural budget across all drains.
    let mut budget = SwapBudget::new(STRUCTURAL_SWAPS_PER_CALLBACK);

    // 3. Budgeted drains in exact production order.
    let Some(resampler_drain) = state.resampler_drain.as_mut() else {
        core::hint::cold_path();
        rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        return PrologueOutcome::Skip {
            current_host_rate: 0,
            structural_applied: budget.used(),
            param_pops: 0,
        };
    };
    drain_resamplers(
        resampler_drain,
        &mut budget,
        &mut state.resampler,
        &mut state.stream,
        rt_status,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
    );

    let Some(cabsim_drain) = state.cabsim_drain.as_mut() else {
        core::hint::cold_path();
        rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        return PrologueOutcome::Skip {
            current_host_rate: 0,
            structural_applied: budget.used(),
            param_pops: 0,
        };
    };
    drain_cabsims(
        cabsim_drain,
        &mut budget,
        &mut state.active_cabsim,
        rt_status,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
    );

    let (param_changed, param_pops) = receive_commands(
        &mut channels.param_consumer,
        &mut state.deferred_model,
        &mut budget,
        &mut state.model_input_mult_adj,
        &mut state.model_output_mult_adj,
        &mut state.current_nam_rate,
        &mut state.active_model_l,
        &mut state.active_model_r,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
        rt_status,
        &mut state.user_input_gain_mult,
        &mut state.user_output_gain_mult,
        &mut state.gate_params,
        &mut state.threshold_open_sq,
        &mut state.threshold_close_sq,
        lut,
        &mut state.adaptive_compute,
    );

    try_slimmable_rebuild(&mut state.adaptive_compute, rt_status);

    drain_slimmable_models(
        &mut state.slimmable_drain,
        &mut budget,
        &mut state.active_model_l,
        &mut state.active_model_r,
        rt_status,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
    );

    drain_os_engines(
        &mut state.os_drain,
        &mut budget,
        &mut state.os_l,
        &mut state.os_r,
        rt_status,
        &mut channels.gc_producer,
        parking_lot,
        parking_lot_dirty,
        &channels.gc_overflow,
    );

    // 4. Rate synchronization.
    let current_host_rate = sync_rate(
        state.shared_target_rate.as_ref(),
        &state.resampler,
        state.current_nam_rate,
        rt_status,
    );

    // 5. Gain recomputation on parameter change.
    if param_changed {
        rt_setup::compute_gain_multipliers(
            state.user_input_gain_mult,
            state.user_output_gain_mult,
            state.model_input_mult_adj,
            state.model_output_mult_adj,
            &mut state.input_gain_mult,
            &mut state.output_gain_mult,
        );
    }

    // 6. Fail-open rollback guard.
    if rt_status.check_flag(RT_STATUS_RESAMP_SWAP_PENDING) {
        let failed_gen = rt_status
            .resampler_failed_generation
            .load(Ordering::Acquire);
        let requested_gen = rt_status.requested_rate_generation.load(Ordering::Acquire);

        if failed_gen != 0 && failed_gen == requested_gen {
            // Fail-open rollback: the rebuild failed for requested_gen,
            // so the callback resumes with the previous resampler in safe
            // bypass/mute mode. Record the requested generation as
            // resolved so the invariant
            // `applied_rate_generation == requested_rate_generation`
            // holds on unmute — the old resampler is the accepted
            // fallback for this request, never a stale replacement.
            rt_status
                .applied_rate_generation
                .store(requested_gen, Ordering::Release);
            rt_status.clear_flag(RT_STATUS_RESAMP_SWAP_PENDING);
            rt_status
                .resampler_failed_generation
                .store(0, Ordering::Release);
        } else {
            return PrologueOutcome::Skip {
                current_host_rate,
                structural_applied: budget.used(),
                param_pops,
            };
        }
    }

    PrologueOutcome::Proceed {
        current_host_rate,
        structural_applied: budget.used(),
        param_pops,
    }
}

#[cfg(test)]
#[path = "process_test.rs"]
mod tests;
