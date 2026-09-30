// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Recording transport selection.
//!
//! Production recording audio travels through the **promoted preallocated
//! pool** (`src/recording/pool.rs`): the RT thread `try_acquire`s a slot,
//! fills it in place and `publish`es a 4-byte [`Descriptor`]; the I/O thread
//! pops the descriptor, writes the 64 KiB block **in place** and `release`s
//! the slot back to the free ring. The pool only carries audio —
//! [`ControlPayload::Metadata`] and [`ControlPayload::StreamStop`] travel on
//! a small dedicated control ring ([`CONTROL_CAPACITY`] slots).
//!
//! # Rollback path (inline ring)
//!
//! The pre-promotion transport — a single `rtrb` ring carrying
//! [`RingPayload`] (Audio + Metadata + StreamStop) — remains fully wired as
//! the rollback path behind the compile-time [`RECORDING_POOL_TRANSPORT`]
//! switch. If the pool ever introduces an ABA/lifetime risk on the real path,
//! flipping the const back to `false` restores the inline transport without
//! any other code change: every producer/consumer type below dispatches to the
//! correct underlying channel.
//!
//! # Ownership & lifecycle semantics
//!
//! [`RecordingSender`] (owned by [`crate::recording::guard::RecordingWorkerGuard`])
//! is the worker's **stop channel**: pushing
//! [`StreamStop`](ControlPayload::StreamStop) and then dropping the sender
//! (which drops both the control producer and the pool producer) arms the
//! worker's "abandoned **and** drained" terminal condition — identical
//! semantics to the inline ring's producer drop.

use rtrb::{Consumer, Producer};

use super::buffer::{
    AlignedBlock, AudioMetadata, ControlPayload, MAX_BLOCK_SIZE, RING_CAPACITY, RingPayload,
    create_audio_ring_buffer, create_control_ring_buffer,
};
use super::pool::{POOL_CAPACITY, PoolConsumer, PoolProducer, RecordingPool};

/// Recording audio transport switch.
///
/// * `true`  — promoted preallocated-pool transport (pool + small descriptor
///   for audio, dedicated control ring for Metadata/StreamStop).
/// * `false` — inline SPSC ring (rollback: single ring carries every payload;
///   used only if the pool introduces ABA/lifetime risk in the real path).
///
/// # Status
///
/// The `Inline` rollback branch is **structurally unreachable in production**:
/// `RECORDING_POOL_TRANSPORT` is hard-coded to `true` and nothing flips it at
/// runtime, so the inline path has no coverage on the production surface —
/// only dedicated unit tests (see `docs/testing.md`). It is kept
/// intentionally as the documented rollback; if it is never needed, it
/// should be removed entirely in a future release rather than maintained as
/// dead weight.
pub const RECORDING_POOL_TRANSPORT: bool = true;

/// Producer half of the recording transport, held by
/// [`crate::recording::guard::RecordingWorkerGuard`] (RAII custody) and reached
/// by the RT callback through a raw pointer.
///
/// The RT thread is the sole writer of every channel it owns; the guard keeps
/// custody for the shutdown path.
///
/// `Pool` variant is significantly larger than `Inline` due to cache-aligned
/// SPSC structures, but `RecordingSender` is instantiated once and held by value
/// across RT thread boundaries; boxing the variant would introduce RT heap drops.
#[expect(
    clippy::large_enum_variant,
    reason = "Pool variant holds cache-aligned SPSC structures; held by value across RT threads without heap drops"
)]
pub enum RecordingSender {
    /// Promoted transport: a small dedicated control ring
    /// (Metadata/StreamStop) plus the preallocated audio pool producer.
    Pool {
        /// Control-ring producer (`None` when recording is disabled).
        control: Option<Producer<ControlPayload>>,
        /// Pool producer (`None` when recording is disabled).
        pool: Option<PoolProducer<POOL_CAPACITY>>,
        /// Local pending barrier waiting for pool work-ring capacity.
        /// Holds `(seq, meta)` if the metadata was pushed to control ring
        /// but `try_push_barrier` failed on pool work ring backpressure.
        pending_barrier: Option<(u16, AudioMetadata)>,
        /// Monotonically increasing sequence number for metadata/barrier pairing.
        next_seq: u16,
    },
    /// Rollback transport: the single inline ring producer.
    Inline(Option<Producer<RingPayload<MAX_BLOCK_SIZE>>>),
}

/// Consumer half of the recording transport, moved to the `nam-recording-io`
/// worker thread.
pub enum RecordingReceiver {
    /// Promoted transport: control-ring consumer + pool consumer.
    Pool {
        /// Control-ring consumer.
        control: Consumer<ControlPayload>,
        /// Pool consumer (audio descriptors + slot recycling).
        pool: PoolConsumer<POOL_CAPACITY>,
    },
    /// Rollback transport: the single inline ring consumer.
    Inline(Consumer<RingPayload<MAX_BLOCK_SIZE>>),
}

impl RecordingSender {
    /// A fully-disabled sender (no channel present) — the dummy slot the RT
    /// callback dereferences unconditionally when recording is not enabled.
    pub const fn none() -> Self {
        Self::Pool {
            control: None,
            pool: None,
            pending_barrier: None,
            next_seq: 1,
        }
    }

    /// Whether any producer channel is present (recording enabled).
    pub fn has_producer(&self) -> bool {
        match self {
            RecordingSender::Pool { control, pool, .. } => control.is_some() || pool.is_some(),
            RecordingSender::Inline(producer) => producer.is_some(),
        }
    }

    /// Mutable access to the control-ring producer (pool transport only).
    pub fn control_producer_mut(&mut self) -> Option<&mut Producer<ControlPayload>> {
        match self {
            RecordingSender::Pool { control, .. } => control.as_mut(),
            RecordingSender::Inline(_) => None,
        }
    }

    /// Mutable access to the pool producer (pool transport only).
    pub fn pool_producer_mut(&mut self) -> Option<&mut PoolProducer<POOL_CAPACITY>> {
        match self {
            RecordingSender::Pool { pool, .. } => pool.as_mut(),
            RecordingSender::Inline(_) => None,
        }
    }

    /// RT-safe: pushes one [`AudioMetadata`] payload through the control
    /// channel (pool transport) or the inline ring (rollback). Returns whether
    /// the payload was accepted (a full channel yields `false` — the caller
    /// retries or defers, never blocks).
    ///
    /// On the pool transport the metadata content travels on the control ring
    /// **and** a control barrier is pushed into the pool `work` ring so the
    /// I/O thread applies the header change at the exact stream position.
    /// The metadata is considered confirmed only when **both** pushes
    /// succeed — a failed barrier push leaves it unconfirmed and audio
    /// publication stays gated on the confirmation, so no ordering can break.
    #[inline]
    pub fn try_push_metadata(&mut self, meta: AudioMetadata) -> bool {
        match self {
            RecordingSender::Pool {
                control,
                pool,
                pending_barrier,
                next_seq,
            } => {
                let Some(control) = control.as_mut() else {
                    return false;
                };
                let Some(pool) = pool.as_mut() else {
                    return false;
                };

                // Single-slot local retry: if this identical metadata was already pushed
                // to control but its barrier failed due to work-ring backpressure, only
                // retry the barrier push instead of polluting control with duplicates.
                if let Some((p_seq, p_meta)) = *pending_barrier
                    && p_meta == meta
                {
                    if pool.try_push_barrier(p_seq) {
                        *pending_barrier = None;
                        return true;
                    }
                    return false;
                }

                let seq = *next_seq;
                if control
                    .push(ControlPayload::Metadata { seq, meta })
                    .is_err()
                {
                    return false;
                }
                *next_seq = next_seq.wrapping_add(1);

                if pool.try_push_barrier(seq) {
                    *pending_barrier = None;
                    true
                } else {
                    *pending_barrier = Some((seq, meta));
                    false
                }
            }
            RecordingSender::Inline(producer) => producer
                .as_mut()
                .is_some_and(|p| p.push(RingPayload::Metadata(meta)).is_ok()),
        }
    }

    /// RT-safe: publishes one stereo audio block into the transport —
    /// `try_acquire` → `fill_planar` in place → `publish` for the pool; block
    /// swap + `push` for the inline ring. Zero heap allocations on the RT
    /// thread. Returns whether the block was accepted (`false` = channel full /
    /// pool exhausted / no channel — the caller accounts it as an overrun).
    #[inline]
    pub fn try_push_audio(&mut self, left: &[f32], right: &[f32]) -> bool {
        match self {
            RecordingSender::Pool { pool, .. } => {
                let Some(producer) = pool.as_mut() else {
                    return false;
                };
                let Some(mut slot) = producer.try_acquire() else {
                    return false;
                };
                slot.block_mut().fill_planar(left, right);
                slot.publish()
            }
            RecordingSender::Inline(producer) => {
                let Some(producer) = producer.as_mut() else {
                    return false;
                };
                let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new_uninit();
                block.fill_planar(left, right);
                producer.push(RingPayload::Audio(block)).is_ok()
            }
        }
    }

    /// RT-safe: pushes the terminal [`StreamStop`](ControlPayload::StreamStop)
    /// token. Returns whether it was accepted.
    #[inline]
    pub fn try_push_stream_stop(&mut self) -> bool {
        match self {
            RecordingSender::Pool { control, .. } => control
                .as_mut()
                .is_some_and(|p| p.push(ControlPayload::StreamStop).is_ok()),
            RecordingSender::Inline(producer) => producer
                .as_mut()
                .is_some_and(|p| p.push(RingPayload::StreamStop).is_ok()),
        }
    }

    /// Total slots lost by dropping an `AcquiredSlot` without publishing.
    pub fn leaked_slots(&self) -> u64 {
        match self {
            RecordingSender::Pool { pool, .. } => pool.as_ref().map_or(0, |p| p.leaked_slots()),
            RecordingSender::Inline(_) => 0,
        }
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
        match self {
            RecordingReceiver::Pool { control, pool } => {
                control.is_abandoned()
                    && control.is_empty()
                    && pool.work_is_abandoned()
                    && pool.work_is_empty()
            }
            RecordingReceiver::Inline(consumer) => consumer.is_abandoned() && consumer.is_empty(),
        }
    }

    /// Total slots lost by dropping an `AcquiredSlot` without publishing.
    pub fn leaked_slots(&self) -> u64 {
        match self {
            RecordingReceiver::Pool { pool, .. } => pool.leaked_slots(),
            RecordingReceiver::Inline(_) => 0,
        }
    }
}

/// Builds a fresh recording transport pair (sender → RT / guard, receiver →
/// worker) for the transport selected by [`RECORDING_POOL_TRANSPORT`].
///
/// The pool preallocates `POOL_CAPACITY` × ~64 KiB slots (≈ 16.8 MiB — the
/// same memory budget as the inline ring); the control ring adds a
/// negligible 4 × 128 B.
pub fn create_recording_transport() -> (RecordingSender, RecordingReceiver) {
    if RECORDING_POOL_TRANSPORT {
        let (control, control_consumer) =
            create_control_ring_buffer(super::buffer::CONTROL_CAPACITY);
        let pool = RecordingPool::<POOL_CAPACITY>::new();
        let (pool_producer, pool_consumer) = pool.split();
        let sender = RecordingSender::Pool {
            control: Some(control),
            pool: Some(pool_producer),
            pending_barrier: None,
            next_seq: 1,
        };
        let receiver = RecordingReceiver::Pool {
            control: control_consumer,
            pool: pool_consumer,
        };
        (sender, receiver)
    } else {
        let (producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(RING_CAPACITY);
        (
            RecordingSender::Inline(Some(producer)),
            RecordingReceiver::Inline(consumer),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::buffer::CONTROL_CAPACITY;

    /// The `none()` sender must never accept any payload and must not claim a
    /// producer (the RT closure dereferences it unconditionally when recording
    /// is disabled).
    #[test]
    fn disabled_sender_rejects_everything() {
        let mut sender = RecordingSender::none();
        assert!(!sender.has_producer());
        assert!(!sender.try_push_metadata(AudioMetadata {
            sample_rate: 48000.0,
            bit_depth: 32,
            channels: 2,
        }));
        assert!(!sender.try_push_audio(&[0.0], &[0.0]));
        assert!(!sender.try_push_stream_stop());
    }

    /// The const must stay `true` until a deliberate rollback — the guard and
    /// the wiring assume the pool transport is the production default.
    #[test]
    fn pool_transport_is_the_production_default() {
        const { assert!(RECORDING_POOL_TRANSPORT) };
        let (sender, receiver) = create_recording_transport();
        assert!(matches!(
            (&sender, &receiver),
            (RecordingSender::Pool { .. }, RecordingReceiver::Pool { .. })
        ));
        assert!(sender.has_producer());
    }

    /// `try_push_audio` on the pool path must land in the pool slot
    /// bit-for-bit and the slot must return to the free ring after release.
    #[test]
    fn pool_sender_audio_round_trip() {
        let (mut sender, mut receiver) = create_recording_transport();
        let (control, pool) = match &mut receiver {
            RecordingReceiver::Pool { control, pool } => (control, pool),
            RecordingReceiver::Inline(_) => panic!("pool transport expected"),
        };

        assert!(sender.try_push_metadata(AudioMetadata {
            sample_rate: 44100.0,
            bit_depth: 32,
            channels: 2,
        }));
        match control.pop() {
            Ok(ControlPayload::Metadata { meta: m, seq }) => {
                assert_eq!(m.sample_rate, 44100.0);
                assert_eq!(seq, 1);
            }
            other => panic!("expected Metadata, got {other:?}"),
        }

        let left = [1.0f32, 2.0, 3.0];
        let right = [-1.0f32, -2.0, -3.0];
        assert!(sender.try_push_audio(&left, &right));

        // The metadata push deposits a control barrier at the head of the pool
        // FIFO — it must surface first, marking the header-change position.
        let barrier = pool.try_pop().expect("metadata barrier");
        assert!(
            barrier.is_barrier(),
            "metadata confirmation must leave a barrier"
        );
        assert_eq!(barrier.barrier_seq(), 1);
        assert!(barrier.release());

        let in_flight = pool.try_pop().expect("published descriptor");
        assert_eq!(in_flight.block().left_slice(), &left[..]);
        assert_eq!(in_flight.block().right_slice(), &right[..]);
        assert!(in_flight.release());

        assert!(pool.work_is_empty());
        assert_eq!(
            sender.pool_producer_mut().unwrap().free_available(),
            POOL_CAPACITY
        );
    }

    /// `try_push_audio` must report `false` (not panic, not leak) when the
    /// pool is exhausted — the caller turns that into overrun accounting.
    #[test]
    fn pool_sender_exhaustion_reports_false() {
        let (mut sender, mut receiver) = create_recording_transport();
        let (_, pool) = match &mut receiver {
            RecordingReceiver::Pool { control, pool } => (control, pool),
            RecordingReceiver::Inline(_) => panic!("pool transport expected"),
        };

        for _ in 0..POOL_CAPACITY {
            assert!(
                sender.try_push_audio(&[1.0], &[2.0]),
                "slots must be acquirable until the pool is exhausted"
            );
        }
        assert!(
            !sender.try_push_audio(&[3.0], &[4.0]),
            "an exhausted pool must report false — the RT overrun condition"
        );

        // Draining the pool returns every slot exactly once (no ABA).
        for _ in 0..POOL_CAPACITY {
            let in_flight = pool.try_pop().expect("published descriptor");
            assert!(in_flight.release());
        }
        assert_eq!(
            sender.pool_producer_mut().unwrap().free_available(),
            POOL_CAPACITY
        );
        assert_eq!(sender.pool_producer_mut().unwrap().leaked_slots(), 0);
    }

    /// When `try_push_barrier` fails because the pool work ring is full,
    /// `pending_barrier` remembers the failed sequence number. Retrying with
    /// the same metadata must NOT push duplicate metadata into the control ring,
    /// and once work capacity is freed, pushing the barrier succeeds.
    #[test]
    fn pool_sender_pending_barrier_retry_deduplicates_metadata() {
        let (mut sender, mut receiver) = create_recording_transport();
        let (control, pool) = match &mut receiver {
            RecordingReceiver::Pool { control, pool } => (control, pool),
            RecordingReceiver::Inline(_) => panic!("pool transport expected"),
        };

        // Fill the work ring completely: POOL_CAPACITY audio blocks + CONTROL_CAPACITY barriers
        for _ in 0..POOL_CAPACITY {
            assert!(sender.try_push_audio(&[0.5], &[0.5]));
        }
        for _ in 0..CONTROL_CAPACITY {
            assert!(
                sender.pool_producer_mut().unwrap().try_push_barrier(0),
                "work ring has CONTROL_CAPACITY slack for barriers"
            );
        }

        // Now work ring is full. try_push_metadata pushes metadata to control,
        // but fails to push barrier to work ring!
        let meta = AudioMetadata {
            sample_rate: 48000.0,
            bit_depth: 24,
            channels: 2,
        };
        assert!(
            !sender.try_push_metadata(meta),
            "must return false when pool barrier fails to push"
        );

        // Control ring holds exactly 1 metadata item with seq 1
        assert_eq!(control.slots(), 1);

        // Retrying with the SAME metadata while work ring is still full
        // must NOT push a duplicate metadata item into control!
        assert!(!sender.try_push_metadata(meta));
        assert_eq!(
            control.slots(),
            1,
            "control ring must not receive duplicate metadata on retry"
        );

        // Drain one item from the work ring to make space for the barrier
        let first_item = pool.try_pop().expect("work ring item");
        assert!(first_item.release());

        // Now retry: barrier push must succeed, confirming the metadata!
        assert!(
            sender.try_push_metadata(meta),
            "retry after freeing work ring space must succeed"
        );

        // Control ring still has exactly 1 metadata item!
        assert_eq!(control.slots(), 1);
        match control.pop() {
            Ok(ControlPayload::Metadata { seq, meta: m }) => {
                assert_eq!(seq, 1);
                assert_eq!(m.sample_rate, 48000.0);
            }
            other => panic!("expected Metadata, got {other:?}"),
        }

        // Drain remaining items from work ring:
        // (POOL_CAPACITY + CONTROL_CAPACITY - 1) items + 1 barrier
        let mut audio_count = 0;
        let mut barrier_seen = false;
        while let Some(item) = pool.try_pop() {
            if item.is_barrier() && item.barrier_seq() == 1 {
                barrier_seen = true;
            } else if !item.is_barrier() {
                audio_count += 1;
            }
            assert!(item.release());
        }
        assert!(audio_count <= POOL_CAPACITY);
        assert!(
            barrier_seen,
            "barrier must have been delivered to work ring"
        );
    }
}
