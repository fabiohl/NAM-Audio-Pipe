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

/// Debug tripwire for the `Inline` rollback dispatch.
///
/// With [`RECORDING_POOL_TRANSPORT`] compiled as the production default, the
/// only way an `Inline` sender can flow through one of the RT-facing send
/// paths ([`RecordingSender::try_push_metadata`], [`RecordingSender::try_push_audio`],
/// [`RecordingSender::try_push_stream_stop`] and the audio enqueue of the
/// capture callback) is hand-wired construction outside
/// [`create_recording_transport`] — an accidental production use that debug
/// builds must accuse fail-fast instead of silently misbehave. The deliberate
/// rollback (const flipped to `false`) satisfies the condition, and release
/// builds compile the assertion away so the rollback runtime behavior stays
/// exactly what it was (debug-only cost, and only while the pool transport is
/// the default).
#[inline(always)]
#[expect(
    clippy::assertions_on_constants,
    reason = "the constant condition is the design: with the pool transport compiled as default the rollback dispatch must panic on every debug invocation, and a deliberate const flip renders it permanently inert"
)]
pub(crate) fn assert_inline_rollback_build() {
    debug_assert!(
        !RECORDING_POOL_TRANSPORT,
        "Inline recording transport used while the pool transport is the \
         compiled default: `Inline` is the documented rollback path and must \
         be exercised only after flipping RECORDING_POOL_TRANSPORT to false"
    );
}

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
            RecordingSender::Inline(producer) => {
                assert_inline_rollback_build();
                producer
                    .as_mut()
                    .is_some_and(|p| p.push(RingPayload::Metadata(meta)).is_ok())
            }
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
                assert_inline_rollback_build();
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
            RecordingSender::Inline(producer) => {
                assert_inline_rollback_build();
                producer
                    .as_mut()
                    .is_some_and(|p| p.push(RingPayload::StreamStop).is_ok())
            }
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
#[path = "transport_test.rs"]
mod transport_test;
