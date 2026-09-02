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
pub enum RecordingSender {
    /// Promoted transport: a small dedicated control ring
    /// (Metadata/StreamStop) plus the preallocated audio pool producer.
    Pool {
        /// Control-ring producer (`None` when recording is disabled).
        control: Option<Producer<ControlPayload>>,
        /// Pool producer (`None` when recording is disabled).
        pool: Option<PoolProducer<POOL_CAPACITY>>,
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
        }
    }

    /// Whether any producer channel is present (recording enabled).
    pub fn has_producer(&self) -> bool {
        match self {
            RecordingSender::Pool { control, pool } => control.is_some() || pool.is_some(),
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
            RecordingSender::Pool { control, pool } => {
                let Some(control) = control.as_mut() else {
                    return false;
                };
                let Some(pool) = pool.as_mut() else {
                    return false;
                };
                control.push(ControlPayload::Metadata(meta)).is_ok() && pool.try_push_barrier()
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
            Ok(ControlPayload::Metadata(m)) => assert_eq!(m.sample_rate, 44100.0),
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
}
