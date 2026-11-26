// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the control ring buffer, `AlignedBlock` memory invariants,
//! overrun accounting counters, and proptests for planar sample consistency.

use super::*;
use proptest::prelude::*;
use std::assert_matches;

/// Asserts two `f32` slices are bit-identical (robust to `NaN` payloads,
/// which `assert_eq!` would treat as unequal).
fn assert_bits_eq(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x.to_bits(), y.to_bits(), "bitwise sample mismatch");
    }
}

#[test]
fn control_ring_buffer_round_trip() {
    let (mut p, mut c) = create_control_ring_buffer(4);
    let meta = AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    };
    assert!(p.push(ControlPayload::Metadata { seq: 42, meta }).is_ok());
    assert!(p.push(ControlPayload::StreamStop).is_ok());

    match c.pop().unwrap() {
        ControlPayload::Metadata { seq, meta: m } => {
            assert_eq!(seq, 42);
            assert_eq!(m, meta);
        }
        _ => panic!("expected ControlPayload::Metadata"),
    }
    assert_matches!(c.pop().unwrap(), ControlPayload::StreamStop);
}

#[test]
fn aligned_block_new_uninit_has_zero_valid_len() {
    let block = AlignedBlock::<MAX_BLOCK_SIZE>::new_uninit();
    assert_eq!(block.valid_len(), 0);
}

#[test]
fn aligned_block_default_is_zero_init() {
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::default();
    assert_eq!(block.valid_len(), 0);
    let zero_plane = [0.0f32; MAX_BLOCK_SIZE / 2];
    block.fill_planar(&zero_plane, &zero_plane);
    assert!(block.as_slice().iter().all(|&x| x == 0.0));
}

#[test]
fn overrun_counter_starts_at_zero() {
    let _guard = OVERRUN_COUNT_LOCK.lock().unwrap();
    assert_eq!(OVERRUN_COUNT.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(
        OVERRUN_FRAMES_COUNT.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

/// Memory-impact contract: a pool slot must hold the largest legal host
/// quantum (8192 frames = `MAX_BRIDGE_BUF` × 2 interleaved channels) and the
/// preallocated pool must keep the total payload footprint within the ~16 MiB
/// budget.
#[test]
fn pool_footprint_covers_max_quantum_within_budget() {
    // 8192 = MAX_BRIDGE_BUF (largest quantum the RT callbacks accept).
    const {
        assert!(
            MAX_BLOCK_SIZE / 2 >= 8192,
            "a slot must hold every legal quantum (8192 frames)"
        );
    }
    assert_eq!(MAX_BLOCK_SIZE % 2, 0);
    let slot = std::mem::size_of::<AlignedBlock<MAX_BLOCK_SIZE>>();
    let footprint = slot * crate::recording::pool::POOL_CAPACITY;
    // Measured: slot=65664 B → footprint = 256 × 65664 ≈ 16.8 MiB
    // (was 1024 × 16640 ≈ 16.3 MiB with MAX_BLOCK_SIZE=4096).
    assert!(
        footprint <= 18 * 1024 * 1024,
        "pool footprint exceeded the 16 MiB budget: {footprint} bytes"
    );
}

proptest! {
    /// Property: for any input slices `left`/`right`, `fill_planar` publishes
    /// exactly `n = min(len(L), len(R), SIZE/2)` stereo frames, keeping
    /// `valid_len` even, bounded by `SIZE`, and bit-identical to the copied
    /// inputs — preserving planar sound consistency.
    #[test]
    fn fill_planar_preserves_invariants(
        left in proptest::collection::vec(any::<f32>(), 0..(MAX_BLOCK_SIZE / 2 + 16)),
        right in proptest::collection::vec(any::<f32>(), 0..(MAX_BLOCK_SIZE / 2 + 16)),
    ) {
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new_uninit();
        let n = left.len().min(right.len()).min(MAX_BLOCK_SIZE / 2);

        let returned = block.fill_planar(&left, &right);

        assert_eq!(returned, 2 * n);
        assert!(block.valid_len() <= MAX_BLOCK_SIZE);
        assert_eq!(block.valid_len() % 2, 0);
        assert_eq!(block.frames(), n);
        assert_eq!(block.is_empty(), n == 0);
        assert_eq!(block.as_slice().len(), block.valid_len());
        assert_eq!(block.left_slice().len(), n);
        assert_eq!(block.right_slice().len(), n);

        // Every published sample matches the copied input, bit for bit.
        assert_bits_eq(block.left_slice(), &left[..n]);
        assert_bits_eq(block.right_slice(), &right[..n]);
        assert_bits_eq(&block.as_slice()[..n], &left[..n]);
        assert_bits_eq(&block.as_slice()[n..], &right[..n]);
    }

    /// Property: `new()` (zeroed) + `fill_planar` with short inputs must never
    /// expose samples past the published length, and the exposed region must
    /// stay within the block capacity.
    #[test]
    fn fill_planar_small_inputs_never_overrun(
        left in proptest::collection::vec(any::<f32>(), 0..64),
        right in proptest::collection::vec(any::<f32>(), 0..64),
    ) {
        let mut block = AlignedBlock::<64>::new();
        let n = left.len().min(right.len()).min(64 / 2);
        block.fill_planar(&left, &right);
        assert_eq!(block.valid_len(), 2 * n);
        assert_eq!(block.frames(), n);
        assert_eq!(block.as_slice().len(), 2 * n);
        assert_bits_eq(block.left_slice(), &left[..n]);
        assert_bits_eq(block.right_slice(), &right[..n]);
    }
}
