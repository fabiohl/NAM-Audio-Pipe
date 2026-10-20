// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Data structures for lock-free communication between the DSP thread and the I/O thread.
//! Contains buffers aligned to cache lines via const generics to mitigate False Sharing
//! in the L1/L2 caches, plus shared atomic flags for cross-thread coordination.

use rtrb::{Consumer, Producer, RingBuffer};
use std::mem::MaybeUninit;
use std::sync::atomic::AtomicU64;

/// Maximum number of interleaved f32 samples per audio block, defined at compile time.
/// Sized for the maximum host quantum (8192 frames × 2 channels = 16384 samples).
pub const MAX_BLOCK_SIZE: usize = 16384;

// Compile-time invariant: a block's valid sample count (structurally
// `<= MAX_BLOCK_SIZE`) is carried in the 16-bit `Descriptor::valid_len` word
// of the pool transport (`recording/pool.rs`), so a future resize at or
// beyond `u16::MAX` — the pool's reserved `CONTROL_BARRIER_SLOT` sentinel —
// would publish a silently truncated `valid_len` through `AcquiredSlot::publish`.
// Fail the build instead; the strict `<` (not `<=`) documents that even the
// largest legal audio word keeps clear of the sentinel.
const _: () = assert!(MAX_BLOCK_SIZE < u16::MAX as usize);

/// Number of slots in the SPSC ring buffer.
/// Each slot holds a `RingPayload<MAX_BLOCK_SIZE>` (~64 KiB).
/// Total memory: 256 slots × 64 KiB = 16 MiB (same footprint as the previous
/// 1024 × 16 KiB layout, but with 4× deeper frame buffering).
pub const RING_CAPACITY: usize = 256;

/// Atomic counter for ring buffer overruns (push failures on the DSP producer side).
/// Reported to the user at shutdown to indicate potential audio data loss.
pub static OVERRUN_COUNT: AtomicU64 = AtomicU64::new(0);

/// Atomic counter for total frames lost due to ring buffer overruns.
/// Reported alongside `OVERRUN_COUNT` at shutdown for reconciliation.
pub static OVERRUN_FRAMES_COUNT: AtomicU64 = AtomicU64::new(0);

/// Serializes unit tests that mutate the process-wide [`OVERRUN_COUNT`] and
/// [`OVERRUN_FRAMES_COUNT`] globals. Test-only; compiled out of production
/// builds. Guards against races between the overrun-accounting tests and the
/// `overrun_counter_starts_at_zero` test (both live in the same `--lib` test binary).
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
    /// the whole 64 KiB array (see `new_uninit`).
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
    /// Measured: avoids ~64 KiB of memset per RT quantum with
    /// `--record` active (~12 MB/s saved at 750 callbacks/s).
    /// On a 5-second capture at 48 kHz / 128-sample quanta
    /// (~1875 callbacks), this saves ~120 MiB of redundant writes.
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

/// Control messages exchanged through the **dedicated control channel** of the
/// recording pool transport.
///
/// The preallocated pool only carries audio; [`AudioMetadata`] and the
/// [`StreamStop`](ControlPayload::StreamStop) token travel on a small separate
/// SPSC ring so the RT path never pushes 64 KiB metadata frames and the I/O
/// thread can finalize a capture independently of the audio backlog.
#[repr(align(128))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ControlPayload {
    /// Stream metadata (sample rate, bit depth, channels) to configure the WAV
    /// file — pushed on format initialization and on every rate change.
    ///
    /// Paired with a [`CONTROL_BARRIER_SLOT`](crate::recording::pool::CONTROL_BARRIER_SLOT)
    /// descriptor in the pool `work` ring carrying the identical sequence number `seq`,
    /// eliminating stale/duplicate pairings under retry or renegotiation.
    Metadata {
        /// Sequence number matching the barrier descriptor in the `work` ring.
        seq: u16,
        /// Audio format parameters.
        meta: AudioMetadata,
    },
    /// Stream stop signal — instructs the I/O thread to drain the remaining
    /// pool descriptors and close the current WAV file.
    StreamStop,
}

/// Capacity of the dedicated recording control channel.
///
/// [`ControlPayload::Metadata`] is pushed only on stream-format changes
/// (rate renegotiations, rare) and [`ControlPayload::StreamStop`] exactly once
/// at shutdown, so 4 slots are far beyond the steady-state need while keeping
/// the control-ring footprint negligible (~4 × 128 B).
pub const CONTROL_CAPACITY: usize = 4;

/// Creates the SPSC control ring for the pool transport:
/// [`Metadata`](ControlPayload::Metadata) / [`StreamStop`](ControlPayload::StreamStop)
/// only — audio goes through the preallocated pool.
pub fn create_control_ring_buffer(
    capacity: usize,
) -> (Producer<ControlPayload>, Consumer<ControlPayload>) {
    RingBuffer::new(capacity)
}

/// Creates an SPSC (single-producer/single-consumer) ring buffer strictly dimensioned
/// for `RingPayload` structures. The `capacity` parameter defines the number of slots.
///
/// This is the legacy inline ring transport constructor maintained as an alternative
/// rollback path. Production wiring uses [`crate::recording::transport::create_recording_transport`].
pub fn create_audio_ring_buffer<const BLOCK_SIZE: usize>(
    capacity: usize,
) -> (
    Producer<RingPayload<BLOCK_SIZE>>,
    Consumer<RingPayload<BLOCK_SIZE>>,
) {
    RingBuffer::new(capacity)
}

#[cfg(test)]
#[path = "buffer_test.rs"]
mod buffer_test;
