// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Playback buffer delivery and output limiting.

use super::super::rt_callback::{handle_spa_pair_fail_closed, stamp_chunk};
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use pipewire as pw;

/// Stateless hard-clamp for hardware playback (`[-1.0, 1.0]`, `NaN -> 0.0`).
#[inline(always)]
fn hard_clamp_playback_sample(s: f32) -> f32 {
    if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) }
}

/// Copies `src` to `dst`, applying `output_limit` (`Off` = raw memcpy, `Clamp` = hard clamp to `[-1.0, 1.0]`).
///
/// # Safety
/// `src` and `dst` must be valid, disjoint, non-overlapping buffers of at least `n_out` `f32` elements.
#[inline(always)]
unsafe fn copy_playback_channel(
    src: *const f32,
    dst: &mut [f32],
    n_out: usize,
    output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
) {
    match output_limit {
        crate::standalone::cli_domain::OutputLimitPolicy::Off => {
            // SAFETY: caller guarantees `src` and `dst` are valid, disjoint, non-overlapping of length `n_out`.
            unsafe {
                core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), n_out);
            }
        }
        crate::standalone::cli_domain::OutputLimitPolicy::Clamp => {
            // SAFETY: caller guarantees `src` is valid for `n_out` reads and disjoint from `dst`.
            let src_slice = unsafe { std::slice::from_raw_parts(src, n_out) };
            for (d, &s) in dst.iter_mut().zip(src_slice.iter()) {
                *d = hard_clamp_playback_sample(s);
            }
        }
    }
}

/// Delivers processed audio to a pair of SPA output buffers fail-closed (no output limit).
///
/// Validates pointer alignment, bounds, symmetry, and disjunction via
/// [`handle_spa_pair_fail_closed`]. If valid, copies `n_out` samples from `src_l`/`src_r`
/// to `ptr_l`/`ptr_r` and writes `offset=0`, `size=n_out * 4`, `stride=4` into
/// the respective [`pw::spa::sys::spa_chunk`] descriptors.
///
/// Returns `Some(n_out)` on success, or `None` if the SPA contract was violated.
///
/// # Safety
/// - `src_l` and `src_r` must point to valid contiguous slices of at least `n_bytes / size_of::<f32>()` floats.
/// - If `ptr_l` / `ptr_r` are valid, they must point to writable memory of size `max_l` / `max_r`.
/// - `chunk_l` and `chunk_r`, if non-null, must point to valid `spa_chunk` memory.
#[cfg(any(test, feature = "testing"))]
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields plus audio sources; signature is shared by playback RT callback and test harnesses"
)]
pub unsafe fn deliver_playback_pair_fail_closed(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *mut pw::spa::sys::spa_chunk,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *mut pw::spa::sys::spa_chunk,
    src_l: *const f32,
    src_r: *const f32,
    n_bytes: usize,
    rt_status: &RtStatusFlags,
) -> Option<usize> {
    // SAFETY: same safety preconditions forwarded directly to `deliver_playback_pair_fail_closed_with_limit`.
    unsafe {
        deliver_playback_pair_fail_closed_with_limit(
            ptr_l,
            max_l,
            chunk_l,
            ptr_r,
            max_r,
            chunk_r,
            src_l,
            src_r,
            n_bytes,
            rt_status,
            crate::standalone::cli_domain::OutputLimitPolicy::Off,
        )
    }
}

/// Delivers processed audio to a pair of SPA output buffers fail-closed with an explicit
/// [`OutputLimitPolicy`](crate::standalone::cli_domain::OutputLimitPolicy).
///
/// # Safety
/// - `src_l` and `src_r` must point to valid contiguous slices of at least `n_bytes / size_of::<f32>()` floats.
/// - If `ptr_l` / `ptr_r` are valid, they must point to writable memory of size `max_l` / `max_r`.
/// - `chunk_l` and `chunk_r`, if non-null, must point to valid `spa_chunk` memory.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields plus audio sources and output limit policy"
)]
pub unsafe fn deliver_playback_pair_fail_closed_with_limit(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *mut pw::spa::sys::spa_chunk,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *mut pw::spa::sys::spa_chunk,
    src_l: *const f32,
    src_r: *const f32,
    n_bytes: usize,
    rt_status: &RtStatusFlags,
    output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
) -> Option<usize> {
    let (_n_bytes, n_out) = match handle_spa_pair_fail_closed(
        ptr_l, max_l, chunk_l, 0, n_bytes, ptr_r, max_r, chunk_r, 0, n_bytes, rt_status,
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
    // SAFETY: `src_l` and `out_l` are valid, disjoint non-overlapping buffers of length `n_out`.
    unsafe {
        copy_playback_channel(src_l, out_l, n_out, output_limit);
    }
    // SAFETY: `check_spa_buffer_pair` proved alignment, bounds, frame
    // symmetry and strict pointer disjunction, so `out_r` is well-formed.
    let out_r = unsafe { std::slice::from_raw_parts_mut(ptr_r as *mut f32, n_out) };
    // SAFETY: `src_r` and `out_r` are valid, disjoint non-overlapping buffers of length `n_out`.
    unsafe {
        copy_playback_channel(src_r, out_r, n_out, output_limit);
    }

    // Informs the hardware exactly how much sound was delivered this time.
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
