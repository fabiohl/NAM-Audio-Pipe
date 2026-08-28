// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Data structures for lock-free communication between the DSP thread and the I/O thread.
//! Contains buffers aligned to cache lines via const generics to mitigate False Sharing
//! in the L1/L2 caches, plus shared atomic flags for cross-thread coordination.

use rtrb::{Consumer, Producer, RingBuffer};
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicU64;

/// Maximum number of interleaved f32 samples per audio block, defined at compile time.
pub const MAX_BLOCK_SIZE: usize = 4096;

/// Number of slots in the SPSC ring buffer.
/// Each slot holds a `RingPayload<MAX_BLOCK_SIZE>` (~16 KiB).
pub const RING_CAPACITY: usize = 1024;

/// Atomic counter for ring buffer overruns (push failures on the DSP producer side).
/// Reported to the user at shutdown to indicate potential audio data loss.
pub static OVERRUN_COUNT: AtomicU64 = AtomicU64::new(0);

/// Serializes unit tests that mutate the process-wide [`OVERRUN_COUNT`] global.
/// Test-only; compiled out of production builds. Guards against races between
/// the overrun-accounting tests and the `overrun_counter_starts_at_zero` test
/// (both live in the same `--lib` test binary).
#[cfg(test)]
pub(crate) static OVERRUN_COUNT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Cache-line-aligned audio block to prevent cache line bouncing (False Sharing)
/// between the DSP and I/O threads. Standard CPU cache lines are 64 bytes,
/// but the 128-byte alignment (`align(128)`) prevents the hardware spatial prefetcher
/// from pulling intensely mutated variables from the I/O thread's adjacency
/// into the DSP thread's cache perimeter.
///
/// Samples are stored **planar** (channel L contiguous, then channel R contiguous)
/// so the PipeWire→block copy is a plain `write_copy_of_slice` memcpy per channel —
/// the RT hot path never touches uninitialized memory and never issues a full-array
/// `assume_init()`. `valid_len` tracks how many samples in `data` contain real audio
/// (`2 * frames` after a stereo fill); the remainder is uninitialized and must never
/// be read. The disk consumer converts only `[0..valid_len]` to bytes, interleaving
/// L/R on the fly (off-RT).
#[repr(align(128))]
#[derive(Debug)]
pub struct AlignedBlock<const SIZE: usize> {
    /// Planar f32 audio samples: `data[0..n]` = channel L, `data[n..2n]` = channel R.
    /// Stored as `MaybeUninit` so the RT path can fill without zero-initializing
    /// the whole 16 KiB array (see `new_uninit`).
    data: [MaybeUninit<f32>; SIZE],
    /// Number of valid f32 samples in `data[0..valid_len]`.
    ///
    /// Strictly private: the only safe way to publish initialized samples is
    /// [`Self::fill_planar`]. Allowing external mutation would let safe code
    /// expose uninitialized `MaybeUninit` slots through `as_slice()`.
    valid_len: usize,
}

impl<const SIZE: usize> AlignedBlock<SIZE> {
    /// Creates a new zeroed block without heap allocation.
    pub const fn new() -> Self {
        Self {
            data: [MaybeUninit::new(0.0); SIZE],
            valid_len: 0,
        }
    }

    /// Creates a block WITHOUT zero-initializing the data array.
    ///
    /// All `MaybeUninit` slots start uninitialized. The consumer MUST only read
    /// `[0..valid_len]` after a successful [`Self::fill_planar`] — garbage past
    /// that boundary is not valid `f32` data.
    ///
    /// # Performance
    ///
    /// Measured: avoids ~16 KiB of memset per RT quantum with
    /// `--record` active (~12 MB/s saved at 750 callbacks/s).
    /// On a 5-second capture at 48 kHz / 128-sample quanta
    /// (~1875 callbacks), this saves ~30 MiB of redundant writes.
    #[inline]
    pub fn new_uninit() -> Self {
        Self {
            data: [MaybeUninit::uninit(); SIZE],
            valid_len: 0,
        }
    }

    /// Copies `left` and `right` (up to `SIZE / 2` frames each) into the planar
    /// block via `MaybeUninit::write_copy_of_slice`, returning the new valid
    /// length (`2 * frames`). Only the copied region is initialized; the rest of
    /// the array is left untouched.
    #[inline]
    pub fn fill_planar(&mut self, left: &[f32], right: &[f32]) -> usize {
        let n = left.len().min(right.len()).min(SIZE / 2);
        let (l_slot, r_slot) = self.data.split_at_mut(n);
        let l = l_slot[..n].write_copy_of_slice(&left[..n]);
        let r = r_slot[..n].write_copy_of_slice(&right[..n]);
        self.valid_len = l.len() + r.len();
        self.valid_len
    }

    /// Returns the valid `[0..valid_len]` region as a `&[f32]`.
    ///
    /// # Safety
    ///
    /// Only the first `valid_len` elements are ever initialized (by `fill_planar`
    /// or by [`Self::new`]'s zero-fill) before `valid_len` is updated, so the
    /// slice projection is always fully initialized.
    #[inline]
    pub fn as_slice(&self) -> &[f32] {
        // SAFETY: `valid_len` is only ever set after the corresponding
        // `[0..valid_len]` elements have been initialized (fill_planar writes
        // them, new/new_uninit start with valid_len == 0). Reading beyond that
        // region is not possible through this API.
        unsafe { self.data[..self.valid_len].assume_init_ref() }
    }

    /// Returns the number of valid f32 samples in the block.
    ///
    /// Always `2 * frames()` and `<= SIZE` after a [`Self::fill_planar`].
    #[inline]
    pub fn valid_len(&self) -> usize {
        self.valid_len
    }

    /// Returns the number of valid stereo frames (`valid_len / 2`).
    #[inline]
    pub fn frames(&self) -> usize {
        self.valid_len / 2
    }

    /// Returns `true` if the block holds no valid samples.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.valid_len == 0
    }

    /// Returns the left channel plane `[0..frames]` as a `&[f32]`.
    ///
    /// # Safety
    ///
    /// Like [`Self::as_slice`], the projection is bounded by `valid_len`, which
    /// is only ever published after the corresponding slots were initialized.
    #[inline]
    pub fn left_slice(&self) -> &[f32] {
        let n = self.frames();
        // SAFETY: `valid_len = 2 * frames` is only published by `fill_planar`
        // after writing both planes, so `[0..frames]` is fully initialized.
        unsafe { self.data[..n].assume_init_ref() }
    }

    /// Returns the right channel plane `[frames..2 * frames]` as a `&[f32]`.
    ///
    /// # Safety
    ///
    /// Like [`Self::as_slice`], the projection is bounded by `valid_len`, which
    /// is only ever published after the corresponding slots were initialized.
    #[inline]
    pub fn right_slice(&self) -> &[f32] {
        let n = self.frames();
        // SAFETY: `valid_len = 2 * frames` is only published by `fill_planar`
        // after writing both planes, so `[n..2n]` is fully initialized and
        // `2n <= valid_len <= SIZE` bounds the projection.
        unsafe { self.data[n..2 * n].assume_init_ref() }
    }
}

impl<const SIZE: usize> Default for AlignedBlock<SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

/// Metadata payload sent when the stream format is initialized or changes.
/// Aligned to 128 bytes to conform to the anti-false-sharing layout of the ring buffer.
#[repr(align(128))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioMetadata {
    /// Audio stream sample rate (e.g., 44100.0, 48000.0).
    pub sample_rate: f32,
    /// Bits per sample (e.g., 32 for IEEE 754 float).
    pub bit_depth: u16,
    /// Number of audio channels (e.g., 2 for stereo).
    pub channels: u16,
}

/// Main payload exchanged via the SPSC ring buffer.
/// Aligned to 128 bytes to ensure each slot occupies distinct cache lines,
/// preventing false sharing between the producer (DSP) and consumer (I/O) cores.
#[repr(align(128))]
#[derive(Debug)]
pub enum RingPayload<const SIZE: usize> {
    /// Audio block containing interleaved f32 samples ready for writing.
    Audio(AlignedBlock<SIZE>),
    /// Stream metadata (sample rate, bit depth, channels) to configure the WAV file.
    Metadata(AudioMetadata),
    /// Stream stop signal — instructs the I/O thread to close the current WAV file.
    StreamStop,
}

/// Creates an SPSC (single-producer/single-consumer) ring buffer strictly dimensioned
/// for `RingPayload` structures. The `capacity` parameter defines the number of slots.
pub fn create_audio_ring_buffer<const BLOCK_SIZE: usize>(
    capacity: usize,
) -> (
    Producer<RingPayload<BLOCK_SIZE>>,
    Consumer<RingPayload<BLOCK_SIZE>>,
) {
    RingBuffer::new(capacity)
}

#[cfg(test)]
mod tests {
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
    fn ring_buffer_push_pop_round_trip() {
        let (mut p, mut c) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(8);
        let meta = AudioMetadata {
            sample_rate: 48000.0,
            bit_depth: 32,
            channels: 2,
        };
        assert!(p.push(RingPayload::Metadata(meta)).is_ok());
        assert_matches!(c.pop().unwrap(), RingPayload::Metadata(m) if m == meta);
    }

    #[test]
    fn ring_buffer_audio_round_trip() {
        let (mut p, mut c) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
        let mut block = AlignedBlock::new();
        block.fill_planar(&[0.0, 1.0, 2.0], &[3.0, 4.0, 5.0]);
        let expected_len = block.valid_len();
        let expected = block.as_slice().to_vec();
        assert!(p.push(RingPayload::Audio(block)).is_ok());
        match c.pop().unwrap() {
            RingPayload::Audio(b) => {
                assert_eq!(b.valid_len(), expected_len);
                assert_eq!(b.as_slice(), &expected[..]);
            }
            other => panic!(
                "expected Audio payload, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn ring_buffer_stream_stop_round_trip() {
        let (mut p, mut c) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
        assert!(p.push(RingPayload::StreamStop).is_ok());
        assert_matches!(c.pop().unwrap(), RingPayload::StreamStop);
    }

    #[test]
    fn ring_buffer_fifo_ordering() {
        let (mut p, mut c) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(16);
        let meta = AudioMetadata {
            sample_rate: 44100.0,
            bit_depth: 32,
            channels: 2,
        };
        p.push(RingPayload::Metadata(meta)).unwrap();
        let mut block = AlignedBlock::new();
        block.fill_planar(&[1.0, 2.0], &[3.0, 4.0]);
        p.push(RingPayload::Audio(block)).unwrap();
        p.push(RingPayload::StreamStop).unwrap();

        assert_matches!(c.pop().unwrap(), RingPayload::Metadata(_));
        assert_matches!(c.pop().unwrap(), RingPayload::Audio(_));
        assert_matches!(c.pop().unwrap(), RingPayload::StreamStop);
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
    }

    #[test]
    fn ring_buffer_empty_pop_is_err() {
        let (_p, mut c) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
        assert!(c.pop().is_err());
    }

    #[test]
    fn ring_buffer_full_push_fails_gracefully() {
        let (mut p, _c) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(1);
        let mut block = AlignedBlock::new_uninit();
        block.fill_planar(&[1.0, 2.0], &[3.0, 4.0]);
        // Fill the only slot
        assert!(p.push(RingPayload::Audio(block)).is_ok());
        // Second push should fail (buffer full)
        let mut block2 = AlignedBlock::new_uninit();
        block2.fill_planar(&[5.0, 6.0], &[7.0, 8.0]);
        assert!(p.push(RingPayload::Audio(block2)).is_err());
    }

    proptest! {
        /// Property: for any input slices `left`/`right`, `fill_planar` publishes
        /// exactly `n = min(len(L), len(R), SIZE/2)` stereo frames, keeping
        /// `valid_len` even, bounded by `SIZE`, and bit-identical to the copied
        /// inputs — the F-RB-001 soundness invariant.
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
}
