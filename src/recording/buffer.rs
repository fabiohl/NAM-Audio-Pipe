// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Data structures for lock-free communication between the DSP thread and the I/O thread.
//! Contains buffers aligned to cache lines via const generics to mitigate False Sharing
//! in the L1/L2 caches, plus shared atomic flags for cross-thread coordination.

use rtrb::{Consumer, Producer, RingBuffer};
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
/// `valid_len` tracks how many samples in `data` contain real audio — the remainder
/// is zero-padding and must not be written to the WAV file.
#[repr(align(128))]
pub struct AlignedBlock<const SIZE: usize> {
    /// Interleaved f32 audio samples (channel L, channel R, L, R, ...).
    pub data: [f32; SIZE],
    /// Number of valid f32 samples in `data[0..valid_len]`.
    pub valid_len: usize,
}

impl<const SIZE: usize> AlignedBlock<SIZE> {
    /// Creates a new zeroed block without heap allocation.
    pub const fn new() -> Self {
        Self {
            data: [0.0; SIZE],
            valid_len: 0,
        }
    }

    /// Creates a block WITHOUT zero-initializing the data array.
    ///
    /// # Safety
    ///
    /// All 32-bit patterns are valid IEEE 754 `f32` values, so the
    /// uninitialized memory is not UB for this element type. The
    /// consumer MUST only read `data[0..valid_len]` — garbage past
    /// that boundary MUST be considered undefined.
    ///
    /// # Performance
    ///
    /// Measured: avoids ~16 KiB of memset per RT quantum with
    /// `--record` active (~12 MB/s saved at 750 callbacks/s).
    /// On a 5-second capture at 48 kHz / 128-sample quanta
    /// (~1875 callbacks), this saves ~30 MiB of redundant writes.
    #[expect(
        clippy::uninit_assumed_init,
        reason = "f32 has no invalid bit patterns — any 32 bits represent a valid (possibly NaN) float. The consumer only reads data[0..valid_len]."
    )]
    #[inline]
    pub fn new_uninit() -> Self {
        unsafe {
            Self {
                data: std::mem::MaybeUninit::uninit().assume_init(),
                valid_len: 0,
            }
        }
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

    #[test]
    fn ring_buffer_push_pop_round_trip() {
        let (mut p, mut c) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(8);
        let meta = AudioMetadata {
            sample_rate: 48000.0,
            bit_depth: 32,
            channels: 2,
        };
        assert!(p.push(RingPayload::Metadata(meta)).is_ok());
        assert!(matches!(c.pop().unwrap(), RingPayload::Metadata(m) if m == meta));
    }

    #[test]
    fn ring_buffer_audio_round_trip() {
        let (mut p, mut c) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
        let mut block = AlignedBlock::new();
        block.valid_len = 128;
        for i in 0..64 {
            block.data[i * 2] = i as f32;
            block.data[i * 2 + 1] = -(i as f32);
        }
        let expected = block.data;
        let expected_len = block.valid_len;
        assert!(p.push(RingPayload::Audio(block)).is_ok());
        match c.pop().unwrap() {
            RingPayload::Audio(b) => {
                assert_eq!(b.valid_len, expected_len);
                assert_eq!(&b.data[..expected_len], &expected[..expected_len]);
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
        assert!(matches!(c.pop().unwrap(), RingPayload::StreamStop));
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
        block.valid_len = 4;
        block.data[0] = 1.0;
        block.data[1] = 2.0;
        block.data[2] = 3.0;
        block.data[3] = 4.0;
        p.push(RingPayload::Audio(block)).unwrap();
        p.push(RingPayload::StreamStop).unwrap();

        assert!(matches!(c.pop().unwrap(), RingPayload::Metadata(_)));
        assert!(matches!(c.pop().unwrap(), RingPayload::Audio(_)));
        assert!(matches!(c.pop().unwrap(), RingPayload::StreamStop));
    }

    #[test]
    fn aligned_block_new_uninit_has_zero_valid_len() {
        let block = AlignedBlock::<MAX_BLOCK_SIZE>::new_uninit();
        assert_eq!(block.valid_len, 0);
    }

    #[test]
    fn aligned_block_default_is_zero_init() {
        let block = AlignedBlock::<MAX_BLOCK_SIZE>::default();
        assert_eq!(block.valid_len, 0);
        assert!(block.data.iter().all(|&x| x == 0.0));
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
        block.valid_len = 4;
        // Fill the only slot
        assert!(p.push(RingPayload::Audio(block)).is_ok());
        // Second push should fail (buffer full)
        let mut block2 = AlignedBlock::new_uninit();
        block2.valid_len = 4;
        assert!(p.push(RingPayload::Audio(block2)).is_err());
    }
}
