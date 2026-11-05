// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Allocation of the `DspBridge` shared buffer for lock-free communication
//! between capture and playback streams.

use std::alloc::{Layout, alloc, handle_alloc_error};

use neural_amp_modeler_rs::dsp::pipeline::{BridgeRef, DspBridge};

/// Error conditions encountered during system page size discovery or bridge layout calculation.
#[derive(Debug, PartialEq, Eq)]
pub enum BridgeAllocError {
    /// POSIX `sysconf(_SC_PAGESIZE)` failed.
    PageSizeQueryFailed(i32),
    /// The page size returned or provided is not a valid positive power of two.
    InvalidPageSize(usize),
    /// Memory layout computation failed (e.g. arithmetic overflow).
    LayoutCalculationFailed(String),
}

impl std::fmt::Display for BridgeAllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PageSizeQueryFailed(errno) => {
                write!(
                    f,
                    "Failed to query system page size via sysconf (errno: {errno})"
                )
            }
            Self::InvalidPageSize(sz) => {
                write!(f, "Invalid page size {sz}: must be a positive power of two")
            }
            Self::LayoutCalculationFailed(msg) => {
                write!(f, "Failed to compute bridge memory layout: {msg}")
            }
        }
    }
}

impl std::error::Error for BridgeAllocError {}

/// Queries the effective runtime system page size in bytes via POSIX `sysconf(_SC_PAGESIZE)`.
///
/// Validates that the returned value is positive and a power of two.
pub fn query_system_page_size() -> Result<usize, BridgeAllocError> {
    // SAFETY: sysconf with _SC_PAGESIZE is a standard POSIX runtime query with no
    // pointer dereferences or memory mutation side-effects.
    let res = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if res <= 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(BridgeAllocError::PageSizeQueryFailed(errno));
    }
    let page_size = res as usize;
    if !page_size.is_power_of_two() {
        return Err(BridgeAllocError::InvalidPageSize(page_size));
    }
    Ok(page_size)
}

/// Computes the memory layout for `DspBridge` aligned to and padded by the specified page size.
///
/// Enforces:
/// 1. `page_size` must be non-zero and a power of two.
/// 2. `align = page_size.max(align_of::<DspBridge>())`.
/// 3. `layout.pad_to_align()` ensures that `layout.size()` is an exact multiple of `align`.
///
/// This guarantees that both the starting address and the end address of the allocation
/// are exact page boundaries. As a result, `madvise(MADV_DONTFORK)` / `madvise(MADV_DONTDUMP)`
/// will never affect any preceding or succeeding heap objects.
pub fn calculate_bridge_layout(page_size: usize) -> Result<Layout, BridgeAllocError> {
    if page_size == 0 || !page_size.is_power_of_two() {
        return Err(BridgeAllocError::InvalidPageSize(page_size));
    }
    let struct_align = std::mem::align_of::<DspBridge>();
    let align = page_size.max(struct_align);
    let size = std::mem::size_of::<DspBridge>();

    Layout::from_size_align(size, align)
        .map(|l| l.pad_to_align())
        .map_err(|e| BridgeAllocError::LayoutCalculationFailed(e.to_string()))
}

/// Allocates `DspBridge` using double-buffering with memory aligned and padded to `page_size`.
///
/// Applies `madvise(MADV_DONTFORK)` and `madvise(MADV_DONTDUMP)` separately to avoid Copy-on-Write
/// overhead on forks and exclude audio ringbuffers from core dumps.
pub fn allocate_dsp_bridge_with_page_size(page_size: usize) -> Result<BridgeRef, BridgeAllocError> {
    let layout = calculate_bridge_layout(page_size)?;

    // SAFETY: Memory is allocated using the global allocator with page-aligned and page-padded layout.
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

    let bridge_void = raw_ptr as *mut libc::c_void;
    let bridge_size = layout.size();

    debug_assert_eq!(
        raw_ptr as usize % page_size,
        0,
        "Bridge allocation address must be aligned to page size"
    );
    debug_assert_eq!(
        bridge_size % page_size,
        0,
        "Bridge allocation size must be a multiple of page size"
    );

    // SAFETY: `bridge_void`/`bridge_size` describe the live allocation owned by
    // this function for the whole call; `MADV_DONTFORK` is advisory and cannot
    // invalidate the mapping on failure.
    let ret = unsafe { libc::madvise(bridge_void, bridge_size, libc::MADV_DONTFORK) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        log::warn!(
            "madvise(MADV_DONTFORK, addr={:p}, len={}) returned {} (errno: {}). \
             Buffer may be included in forks.",
            bridge_void,
            bridge_size,
            ret,
            err
        );
    }

    // SAFETY: `bridge_void`/`bridge_size` describe the live allocation owned by
    // this function for the whole call; `MADV_DONTDUMP` is advisory and cannot
    // invalidate the mapping on failure.
    let ret = unsafe { libc::madvise(bridge_void, bridge_size, libc::MADV_DONTDUMP) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        log::warn!(
            "madvise(MADV_DONTDUMP, addr={:p}, len={}) returned {} (errno: {}). \
             Buffer may be included in core-dumps.",
            bridge_void,
            bridge_size,
            ret,
            err
        );
    }

    Ok(bridge_ptr)
}

/// Allocates `DspBridge` with double-buffering using memory aligned and padded to the real system page size.
///
/// Page alignment is required for `libc::madvise` to succeed. Standard heap allocation
/// only guarantees the struct alignment (128 bytes), which causes `madvise` to fail with `EINVAL (errno 22)`.
///
/// The effective page size is obtained at runtime via `query_system_page_size()`. If discovery fails,
/// a diagnostic warning is logged and a safe fallback of 4096 bytes is used.
///
/// The allocated buffer has a `'static` lifetime matching the process duration
/// in the standalone PipeWire host binary.
pub fn allocate_dsp_bridge() -> BridgeRef {
    let page_size = match query_system_page_size() {
        Ok(ps) => ps,
        Err(e) => {
            log::warn!(
                "[E2120 | PAGE_SIZE_QUERY_FAILED] Could not query system page size ({e}); falling back to 4096 bytes"
            );
            4096
        }
    };

    allocate_dsp_bridge_with_page_size(page_size).unwrap_or_else(|e| {
        log::error!("Critical error allocating DSP bridge: {e}");
        panic!("Critical error allocating DSP bridge: {e}");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_page_size_query() {
        let ps = query_system_page_size().expect("system page size query must succeed");
        assert!(ps >= 4096, "system page size must be at least 4096 bytes");
        assert!(
            ps.is_power_of_two(),
            "system page size must be a power of two"
        );
    }

    #[test]
    fn test_calculate_bridge_layout_4k_16k_64k() {
        let raw_struct_size = std::mem::size_of::<DspBridge>();

        for &page_size in &[4096, 16384, 65536] {
            let layout = calculate_bridge_layout(page_size)
                .unwrap_or_else(|_| panic!("layout calculation for {page_size} must succeed"));

            assert_eq!(layout.align(), page_size);
            assert_eq!(layout.size() % page_size, 0);
            assert!(layout.size() >= raw_struct_size);
        }
    }

    #[test]
    fn test_calculate_bridge_layout_invalid_page_sizes() {
        assert_eq!(
            calculate_bridge_layout(0),
            Err(BridgeAllocError::InvalidPageSize(0))
        );
        assert_eq!(
            calculate_bridge_layout(3000),
            Err(BridgeAllocError::InvalidPageSize(3000))
        );
        assert_eq!(
            calculate_bridge_layout(5000),
            Err(BridgeAllocError::InvalidPageSize(5000))
        );
        // Overflow / oversized layout
        assert!(matches!(
            calculate_bridge_layout(1 << (usize::BITS - 1)),
            Err(BridgeAllocError::LayoutCalculationFailed(_))
        ));
    }

    #[test]
    fn test_allocate_dsp_bridge_with_page_size_16k_and_64k() {
        for &page_size in &[16384, 65536] {
            let bridge_ref = allocate_dsp_bridge_with_page_size(page_size)
                .unwrap_or_else(|e| panic!("allocation with {page_size} failed: {e}"));
            // SAFETY: bridge_ref was freshly returned by allocate_dsp_bridge_with_page_size,
            // referencing an intact, live DspBridge allocation.
            let ptr = unsafe { bridge_ref.as_ptr() };

            assert_eq!(
                ptr as usize % page_size,
                0,
                "Pointer must be aligned to requested page size {page_size}"
            );
        }
    }

    #[test]
    fn test_allocate_dsp_bridge_page_alignment_and_initial_state() {
        let sys_page_size = query_system_page_size().unwrap_or(4096);
        let bridge_ref = allocate_dsp_bridge();
        // SAFETY: `bridge_ref` points to the static DspBridge allocation created by `allocate_dsp_bridge`.
        let ptr = unsafe { bridge_ref.as_ptr() };

        // Must be aligned to real system page size for madvise compatibility
        assert_eq!(
            ptr as usize % sys_page_size,
            0,
            "DspBridge pointer must be page-aligned to {sys_page_size} bytes"
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
