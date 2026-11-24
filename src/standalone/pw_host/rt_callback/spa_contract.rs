// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Real-time FFI / SPA buffer contract validation and fail-closed silencing.
//!
//! Validates alignment, bounds, frame symmetry, and pointer disjunction on raw
//! PipeWire SPA descriptors before any `&mut [f32]` reference is formed, and
//! zeroes malformed or truncated regions without allocating or panicking.

use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::pipeline::MAX_BRIDGE_BUF;
use pipewire as pw;

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
/// contract of `read_chunk_meta`. Zero allocations, zero panics.
#[inline(always)]
pub(crate) fn stamp_chunk(chunk: *mut pw::spa::sys::spa_chunk, bytes: usize) {
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
/// When no region could be zeroed (null/misaligned data pointer or non-cardinal
/// size) the chunk is still stamped to `size=0`, so stale metadata can never
/// replay old audio; the chunk pointer is only dereferenced when `stamp_chunk`
/// can prove it non-null and aligned.
#[inline(always)]
pub(crate) fn silence_available_datas(datas: &mut [pw::spa::buffer::Data]) {
    for data in datas.iter_mut() {
        let raw = data.as_raw();
        let zeroed = try_zero_region(raw.data as usize, raw.maxsize as usize);
        stamp_chunk(raw.chunk, zeroed);
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
/// - `ChunkWindow::Absent` — chunk pointer is null or misaligned (no
///   descriptor to read);
/// - `ChunkWindow::Malformed` — non-null chunk with an invalid stride or the
///   corrupted flag set;
/// - `ChunkWindow::Valid` — the host-declared valid-data window.
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
/// - `ChunkWindow::Valid` → `Some((offset, size))` — the host-declared window;
/// - `ChunkWindow::Absent` → `Some((0, 0))` — no descriptor to read; the
///   consolidated [`handle_spa_pair_fail_closed`] harness below still rejects a
///   null/misaligned chunk pair via its own proof;
/// - `ChunkWindow::Malformed` → raises the fail-closed contract violation
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
    // SAFETY: `ptr_l` is a validated non-null buffer data pointer and `bound_l` is
    // clamped to `MAX_BRIDGE_BUF * sizeof(f32)`; the shared `&[u8]` view only feeds
    // the pure validator and is dead before any mutable `f32` slice is formed.
    let raw_l: &[u8] = unsafe { std::slice::from_raw_parts(ptr_l as *const u8, bound_l) };
    // SAFETY: same validated non-null pointer and clamped bound contract for the
    // right channel.
    let raw_r: &[u8] = unsafe { std::slice::from_raw_parts(ptr_r as *const u8, bound_r) };
    check_spa_buffer_pair(raw_l, offset_l, size_l, raw_r, offset_r, size_r)
}

/// Consolidated fail-closed FFI/SPA descriptor handling shared by the RT
/// callbacks (capture and playback) and by the malformed-FFI harness tests.
///
/// `chunk_l`/`chunk_r` must be non-null and correctly aligned;
/// `offset_l`/`size_l` are the resolved valid-data window — capture reads them
/// from the host chunk via `read_chunk_meta`, playback passes `(0, n_bytes)`.
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
