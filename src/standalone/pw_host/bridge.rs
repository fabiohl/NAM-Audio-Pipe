// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Allocation of the `DspBridge` shared buffer for lock-free communication
//! between capture and playback streams.

use std::alloc::{Layout, alloc, handle_alloc_error};

use neural_amp_modeler_rs::dsp::pipeline::{BridgeRef, DspBridge};

/// Allocates `DspBridge` with double-buffering using page-aligned memory.
///
/// Page alignment (4096 bytes) is required for `libc::madvise` to succeed.
/// Standard heap allocation (e.g. `Box::new`) only guarantees the alignment
/// specified by the struct (128 bytes), which causes `madvise` to fail with
/// `EINVAL (errno 22)`.
///
/// The allocated buffer has a `'static` lifetime matching the process duration
/// in the standalone PipeWire host binary.
///
/// Applies `madvise(MADV_DONTFORK)` and `madvise(MADV_DONTDUMP)` separately to
/// avoid Copy-on-Write overhead on forks and to exclude the buffers from core dumps.
pub fn allocate_dsp_bridge() -> BridgeRef {
    let page_size = 4096usize;
    let align = page_size.max(std::mem::align_of::<DspBridge>());
    let size = std::mem::size_of::<DspBridge>();
    // `align` is a page size (power of two) or the struct's own alignment, and
    // `size` is a fixed compile-time constant: `from_size_align` cannot fail,
    // so this `expect` documents an invariant instead of adding panic surface
    // (F-PR-13).
    #[expect(
        clippy::expect_used,
        reason = "DspBridge layout is always valid: power-of-two alignment and a fixed size"
    )]
    let layout = Layout::from_size_align(size, align)
        .expect("Valid layout for DspBridge with page alignment")
        .pad_to_align();

    // SAFETY: Memory is allocated using the global allocator with page-aligned layout.
    // We check for null and handle OOM properly.
    let raw_ptr = unsafe { alloc(layout) as *mut DspBridge };
    if raw_ptr.is_null() {
        handle_alloc_error(layout);
    }

    // SAFETY: `raw_ptr` points to freshly allocated, properly aligned and sized memory.
    // We initialize all fields of `DspBridge` directly into the allocated memory.
    unsafe {
        DspBridge::init_in_place(raw_ptr);
    }

    // SAFETY: `raw_ptr` was just initialized in place by `init_in_place`; the
    // returned reference is tied to the same live, uniquely owned allocation.
    let bridge_ptr = unsafe { BridgeRef::new(raw_ptr) };

    // SAFETY: `raw_ptr` is page-aligned (layout alignment is at least 4096)
    // and layout.size() is a multiple of page size (padded).
    let bridge_void = raw_ptr as *mut libc::c_void;
    let bridge_size = layout.size();

    // SAFETY: `bridge_void`/`bridge_size` describe the live allocation owned by
    // this function for the whole call; `MADV_DONTFORK` is advisory and cannot
    // invalidate the mapping on failure.
    let ret = unsafe { libc::madvise(bridge_void, bridge_size, libc::MADV_DONTFORK) };
    if ret != 0 {
        log::warn!(
            "madvise(MADV_DONTFORK) returned {} (errno: {}). \
             Buffer may be included in forks.",
            ret,
            std::io::Error::last_os_error()
        );
    }

    // SAFETY: `bridge_void`/`bridge_size` describe the live allocation owned by
    // this function for the whole call; `MADV_DONTDUMP` is advisory and cannot
    // invalidate the mapping on failure.
    let ret = unsafe { libc::madvise(bridge_void, bridge_size, libc::MADV_DONTDUMP) };
    if ret != 0 {
        log::warn!(
            "madvise(MADV_DONTDUMP) returned {} (errno: {}). \
             Buffer may be included in core-dumps.",
            ret,
            std::io::Error::last_os_error()
        );
    }

    bridge_ptr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocate_dsp_bridge_page_alignment_and_initial_state() {
        let bridge_ref = allocate_dsp_bridge();
        // SAFETY: `bridge_ref` points to the static DspBridge allocation created by `allocate_dsp_bridge`.
        let ptr = unsafe { bridge_ref.as_ptr() };

        // Must be page-aligned (4096 bytes) for madvise compatibility
        assert_eq!(
            ptr as usize % 4096,
            0,
            "DspBridge pointer must be page-aligned (4096 bytes)"
        );

        // SAFETY: the raw pointer was produced by `alloc(layout)` in this test and
        // the layout was verified page-aligned and sized above.
        let bridge = unsafe { &*ptr };
        assert_eq!(
            bridge
                .active_read_idx
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            bridge.generation.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            bridge
                .consumed_gen
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            bridge
                .dropped_frames
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        // SAFETY: single-threaded read of the freshly initialized bridge; the
        // buffers are read race-free through `UnsafeCell::get`.
        assert_eq!(unsafe { (*bridge.buffers[0].get()).n_samples }, 0);
        // SAFETY: same single-owner read of the second freshly initialized buffer,
        // race-free through `UnsafeCell::get`.
        assert_eq!(unsafe { (*bridge.buffers[1].get()).n_samples }, 0);
    }
}
