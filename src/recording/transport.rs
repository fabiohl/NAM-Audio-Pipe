// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Recording transport.
//!
//! Recording audio travels through the **preallocated pool**
//! (`src/recording/pool.rs`): the RT thread `try_acquire`s a slot, fills it in
//! place and `publishes` a 4-byte `Descriptor`; the I/O thread pops the
//! descriptor, writes the 64 KiB block **in place** and `release`s the slot
//! back to the free ring. The pool only carries audio —
//! [`ControlPayload::Metadata`] and [`ControlPayload::StreamStop`] travel on a
//! small dedicated control ring (`CONTROL_CAPACITY` slots).
//!
//! # Ownership & lifecycle semantics
//!
//! [`RecordingSender`] (owned by [`crate::recording::guard::RecordingWorkerGuard`])
//! is the worker's **stop channel**: pushing
//! [`StreamStop`](ControlPayload::StreamStop) and then dropping the sender
//! (which drops both the control producer and the pool producer) arms the
//! worker's "abandoned **and** drained" terminal condition.

use rtrb::{Consumer, Producer};

use super::buffer::{AudioMetadata, ControlPayload, create_control_ring_buffer};
use super::pool::{MAX_SILENCE_FRAMES, POOL_CAPACITY, PoolConsumer, PoolProducer, RecordingPool};

/// Producer half of the recording transport, held by
/// [`crate::recording::guard::RecordingWorkerGuard`] (RAII custody) and reached
/// by the RT callback through a raw pointer.
///
/// The RT thread is the sole writer of every channel it owns; the guard keeps
/// custody for the shutdown path.
pub struct RecordingSender {
    /// Control-ring producer (`None` when recording is disabled).
    control: Option<Producer<ControlPayload>>,
    /// Pool producer (`None` when recording is disabled).
    pool: Option<PoolProducer<POOL_CAPACITY>>,
    /// Local pending barrier waiting for pool work-ring capacity.
    /// Holds `(seq, meta)` if the metadata was pushed to the control ring
    /// but `try_push_barrier` failed on pool work ring backpressure.
    pending_barrier: Option<(u16, AudioMetadata)>,
    /// Monotonically increasing sequence number for metadata/barrier pairing.
    next_seq: u16,
    /// Frames intentionally suppressed by the `Silence` gate that have not yet
    /// been published as a silence marker. Accumulated on the RT thread
    /// (`O(1)`, no zero copies) and flushed as one descriptor per
    /// `MAX_SILENCE_FRAMES` frames at the next boundary (gate reopen, metadata
    /// change or stream stop).
    pending_silence_frames: u64,
}

/// Consumer half of the recording transport, moved to the `nam-recording-io`
/// worker thread.
pub struct RecordingReceiver {
    pub(crate) control: Consumer<ControlPayload>,
    pub(crate) pool: PoolConsumer<POOL_CAPACITY>,
}

impl RecordingSender {
    /// Publishes as many pending intentional-silence frames as the work ring
    /// accepts, leaving the remainder in `pending`. Returns `true` when every
    /// pending frame was published.
    ///
    /// Each descriptor carries at most [`MAX_SILENCE_FRAMES`] frames, so a long
    /// gate-closed episode is split into the minimum number of markers and a
    /// full ring never drops the count — it is retried on the next callback.
    pub(crate) fn flush_pending_silence(
        pool: &mut PoolProducer<POOL_CAPACITY>,
        pending: &mut u64,
    ) -> bool {
        while *pending > 0 {
            let chunk = (*pending).min(MAX_SILENCE_FRAMES as u64) as u16;
            if !pool.try_push_silence(chunk) {
                return false;
            }
            *pending -= chunk as u64;
        }
        true
    }

    /// A fully-disabled sender (no channel present) — the dummy slot the RT
    /// callback dereferences unconditionally when recording is not enabled.
    pub const fn none() -> Self {
        Self {
            control: None,
            pool: None,
            pending_barrier: None,
            next_seq: 1,
            pending_silence_frames: 0,
        }
    }

    /// Builds a sender from explicit channel halves. Test-only: production
    /// wiring always goes through [`create_recording_transport`].
    #[cfg(test)]
    pub(crate) fn from_parts(
        control: Option<Producer<ControlPayload>>,
        pool: Option<PoolProducer<POOL_CAPACITY>>,
    ) -> Self {
        Self {
            control,
            pool,
            pending_barrier: None,
            next_seq: 1,
            pending_silence_frames: 0,
        }
    }

    /// Whether any producer channel is present (recording enabled).
    pub fn has_producer(&self) -> bool {
        self.control.is_some() || self.pool.is_some()
    }

    /// Mutable access to the control-ring producer.
    pub fn control_producer_mut(&mut self) -> Option<&mut Producer<ControlPayload>> {
        self.control.as_mut()
    }

    /// Mutable access to the pool producer.
    pub fn pool_producer_mut(&mut self) -> Option<&mut PoolProducer<POOL_CAPACITY>> {
        self.pool.as_mut()
    }

    /// Simultaneous split-borrow of both producer halves. Test-only: the RT
    /// path never needs both channels at once.
    #[cfg(test)]
    pub(crate) fn parts_mut(
        &mut self,
    ) -> (
        Option<&mut Producer<ControlPayload>>,
        Option<&mut PoolProducer<POOL_CAPACITY>>,
    ) {
        (self.control.as_mut(), self.pool.as_mut())
    }

    /// RT-safe: pushes one [`AudioMetadata`] payload through the control
    /// channel. Returns whether the payload was accepted (a full channel
    /// yields `false` — the caller retries or defers, never blocks).
    ///
    /// The metadata content travels on the control ring **and** a control
    /// barrier is pushed into the pool `work` ring so the I/O thread applies
    /// the header change at the exact stream position. The metadata is
    /// considered confirmed only when **both** pushes succeed — a failed
    /// barrier push leaves it unconfirmed and audio publication stays gated on
    /// the confirmation, so no ordering can break.
    #[inline]
    pub fn try_push_metadata(&mut self, meta: AudioMetadata) -> bool {
        let Some(control) = self.control.as_mut() else {
            return false;
        };
        let Some(pool) = self.pool.as_mut() else {
            return false;
        };

        // Silence accrued before the format change belongs to the previous
        // segment and rate: publish it before the metadata barrier opens the
        // new part. A rejected flush leaves the metadata unconfirmed (retried
        // next callback).
        if !Self::flush_pending_silence(pool, &mut self.pending_silence_frames) {
            return false;
        }

        // Single-slot local retry: if this identical metadata was already pushed
        // to control but its barrier failed due to work-ring backpressure, only
        // retry the barrier push instead of polluting control with duplicates.
        if let Some((p_seq, p_meta)) = self.pending_barrier
            && p_meta == meta
        {
            if pool.try_push_barrier(p_seq) {
                self.pending_barrier = None;
                return true;
            }
            return false;
        }

        let seq = self.next_seq;
        if control
            .push(ControlPayload::Metadata { seq, meta })
            .is_err()
        {
            return false;
        }
        self.next_seq = self.next_seq.wrapping_add(1);

        if pool.try_push_barrier(seq) {
            self.pending_barrier = None;
            true
        } else {
            self.pending_barrier = Some((seq, meta));
            false
        }
    }

    /// RT-safe: publishes one stereo audio block into the pool —
    /// `try_acquire` → `fill_planar` in place → `publish`. Zero heap
    /// allocations on the RT thread. Returns whether the block was accepted
    /// (`false` = pool exhausted / no channel — the caller accounts it as an
    /// overrun).
    #[inline]
    pub fn try_push_audio(&mut self, left: &[f32], right: &[f32]) -> bool {
        let Some(producer) = self.pool.as_mut() else {
            return false;
        };
        // Ordering: intentional silence that preceded this audio block must
        // reach the I/O thread first. A rejected flush means the work ring is
        // saturated — the audio publish would fail too, so the caller's
        // overrun accounting stays truthful.
        if !Self::flush_pending_silence(producer, &mut self.pending_silence_frames) {
            return false;
        }
        let Some(mut slot) = producer.try_acquire() else {
            return false;
        };
        slot.block_mut().fill_planar(left, right);
        slot.publish()
    }

    /// RT-safe: records `frames` stereo frames intentionally suppressed by the
    /// `Silence` noise gate. The sender accumulates the count and publishes at
    /// most one zero-copy descriptor per [`MAX_SILENCE_FRAMES`] frames — the RT
    /// thread never copies zeros or allocates.
    ///
    /// Returns whether the transport is able to keep the count (a missing
    /// channel yields `false`; a temporarily full work ring keeps the frames
    /// pending in the sender and still returns `true`).
    #[inline]
    pub fn try_push_silence(&mut self, frames: usize) -> bool {
        if frames == 0 {
            return true;
        }
        let Some(pool) = self.pool.as_mut() else {
            return false;
        };
        if self.control.is_none() {
            return false;
        }
        self.pending_silence_frames = self.pending_silence_frames.saturating_add(frames as u64);
        if self.pending_silence_frames >= MAX_SILENCE_FRAMES as u64 {
            Self::flush_pending_silence(pool, &mut self.pending_silence_frames);
        }
        true
    }

    /// RT-safe: pushes the terminal [`StreamStop`](ControlPayload::StreamStop)
    /// token. Returns whether it was accepted.
    #[inline]
    pub fn try_push_stream_stop(&mut self) -> bool {
        let Some(control) = self.control.as_mut() else {
            return false;
        };
        // Drain the last pending silence before the terminal token so the WAV
        // duration covers every gate-closed frame.
        if let Some(pool) = self.pool.as_mut()
            && !Self::flush_pending_silence(pool, &mut self.pending_silence_frames)
        {
            return false;
        }
        control.push(ControlPayload::StreamStop).is_ok()
    }

    /// Total slots lost by dropping an `AcquiredSlot` without publishing.
    pub fn leaked_slots(&self) -> u64 {
        self.pool.as_ref().map_or(0, |p| p.leaked_slots())
    }
}

impl Default for RecordingSender {
    fn default() -> Self {
        Self::none()
    }
}

impl RecordingReceiver {
    /// `true` when every producer side is gone and every channel is fully
    /// drained — the worker's terminal condition (2).
    pub fn is_fully_drained(&self) -> bool {
        self.control.is_abandoned()
            && self.control.is_empty()
            && self.pool.work_is_abandoned()
            && self.pool.work_is_empty()
    }

    /// Mutable access to the pool consumer (audio descriptors + slot
    /// recycling).
    pub fn pool_consumer_mut(&mut self) -> &mut PoolConsumer<POOL_CAPACITY> {
        &mut self.pool
    }

    /// Mutable access to the control-ring consumer.
    #[cfg(any(test, feature = "testing"))]
    pub fn control_consumer_mut(&mut self) -> &mut Consumer<ControlPayload> {
        &mut self.control
    }

    /// Total slots lost by dropping an `AcquiredSlot` without publishing.
    pub fn leaked_slots(&self) -> u64 {
        self.pool.leaked_slots()
    }
}

/// Builds a fresh recording transport pair (sender → RT / guard, receiver →
/// worker).
///
/// The pool preallocates `POOL_CAPACITY` × ~64 KiB slots (≈ 16.8 MiB); the
/// control ring adds a negligible 4 × 128 B.
pub fn create_recording_transport() -> (RecordingSender, RecordingReceiver) {
    let (control, control_consumer) = create_control_ring_buffer(super::buffer::CONTROL_CAPACITY);
    let pool = RecordingPool::<POOL_CAPACITY>::new();
    let (pool_producer, pool_consumer) = pool.split();
    let sender = RecordingSender {
        control: Some(control),
        pool: Some(pool_producer),
        pending_barrier: None,
        next_seq: 1,
        pending_silence_frames: 0,
    };
    let receiver = RecordingReceiver {
        control: control_consumer,
        pool: pool_consumer,
    };
    (sender, receiver)
}

#[cfg(test)]
#[path = "transport_test.rs"]
mod transport_test;
