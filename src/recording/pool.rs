// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Preallocated pool + small-descriptor SPSC transport for the recording path.
//!
//! # Why this transport exists
//!
//! The inline recording transport moves every 64 KiB audio block *by value*
//! through an `rtrb` SPSC ring: the RT thread copies the PipeWire frames into a
//! scratch block and `push`es it (64 KiB write into the ring slot), and the
//! I/O thread `pop`s it (64 KiB read out of the ring slot into its own stack
//! block) before interleaving it into the WAV I/O buffer. The block therefore
//! crosses the L1/L2/LLC hierarchy an extra round-trip per quantum.
//!
//! This module provides a **preallocated slot pool** whose data never moves.
//! The RT thread pops a small slot *index* from a free-list ring, copies the
//! frames directly into the preallocated slot, and publishes a 4-byte
//! `Descriptor` through a second SPSC ring. The I/O thread pops the
//! descriptor, reads the block **in place**, and returns the index to the
//! free-list ring — the "return" channel. The 64 KiB payload is written once
//! and read once; only 4-byte descriptors and 2-byte indices travel through the
//! rings.
//!
//! ```text
//!            ┌──────────── free ring (u16 indices, I/O → RT) ────────────┐
//!            │                                                            │
//!   RT thread│  try_acquire() → pop index  │  publish() → push descriptor│
//!            ▼                                                           │
//!   ┌───────────────────────┐       ┌──────────────┐                     │
//!   │  pool.slots[idx]      │       │  work ring   │ (Descriptor, RT→IO) │
//!   │  (64 KiB, written     │◄──────┤  (4 bytes)   │                     │
//!   │   in place, never     │       └──────┬───────┘                     │
//!   │   moved)              │              │ try_pop()                   │
//!   └───────────────────────┘              ▼                             │
//!                                  I/O thread reads slot in place,        │
//!                                  then release() → push index ───────────┘
//! ```
//!
//! # Memory-traffic model (the A/B claim being tested)
//!
//! Per quantum of `F` stereo frames (`B = 8·F` bytes, 64 B lines = `B/64`):
//!
//! | path | producer | consumer | total data lines |
//! |------|----------|----------|------------------|
//! | inline | fill scratch (`B`W) + push (`B`R+`B`W) | pop (`B`R+`B`W) + write (`B`R+`B`W) | **7·B/64** |
//! | pool + descriptor | fill slot in place (`B`W) | read slot + write io buf (`B`R+`B`W) | **3·B/64** |
//!
//! The pool touches ~57% fewer data cache lines per quantum, significantly
//! reducing CPU cache pressure and memory bandwidth consumption on the RT thread.
//!
//! # Ownership protocol & soundness
//!
//! Every slot index is in **exactly one** of four states at any instant, and
//! transitions are strictly ordered through the two SPSC rings:
//!
//! 1. **FREE** — the index sits in the `free` ring; the producer may pop it.
//! 2. **RT-owned** — the producer popped it; it fills the slot, then publishes.
//! 3. **IN-FLIGHT** — a `Descriptor{slot}` sits in the `work` ring.
//! 4. **I/O-owned** — the consumer popped the descriptor; it reads the slot,
//!    then releases (pushes the index back to `free`).
//!
//! Because `free` and `work` are strict SPSC FIFO channels and every slot
//! traverses `FREE → RT → IN-FLIGHT → I/O → FREE`, a slot can never be
//! returned twice (double-return) nor reused while still referenced by an older
//! descriptor (ABA): a slot index is observable by the producer again only
//! *after* the consumer released it, and each descriptor is consumed exactly
//! once in order. The unit tests prove this with a full ownership-transition
//! state machine plus an end-of-shutdown exactly-once drain.
//!
//! With `POOL_CAPACITY == RING_CAPACITY == 256`, both ring pushes are
//! **infallible by construction**: `publish` holds ≥ 1 slot, so `work` can
//! hold at most `N - 1` descriptors; `release` holds ≥ 1 slot, so `free` can
//! hold at most `N - 1` indices. The only failure point is `try_acquire` on an
//! empty `free` ring, which is the pool's overrun condition — the caller
//! accounts it exactly like a full inline ring.
//!
//! A slot acquired (`AcquiredSlot`) or popped (`InFlightBlock`) but dropped
//! without `publish`/`release` is lost forever (the free-ring producer lives
//! on the opposite thread); `Drop` counts it in [`PoolProducer::leaked_slots`]
//! so such a bug degrades toward overrun accounting instead of corruption.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

use super::buffer::{AlignedBlock, MAX_BLOCK_SIZE};

/// Number of preallocated slots. Mirrors [`super::buffer::RING_CAPACITY`]
/// (256 × ~64 KiB ≈ 16.8 MiB) so both transports operate within the same
/// memory budget.
pub const POOL_CAPACITY: usize = 256;

/// Reserved slot index marking a **control barrier** in the `work` ring.
///
/// `0xFFFF` can never be handed out by [`PoolProducer::try_acquire`] (the free
/// ring is seeded with `0..N` and `N <= 256`), so the value is free to act as
/// an ordering marker: the RT thread pushes it into the `work` ring right after
/// pushing a [`Metadata`](crate::recording::buffer::ControlPayload::Metadata)
/// into the dedicated control ring, telling the I/O thread that the control
/// message must be applied **at this exact position** of the audio stream.
///
/// # Why the barrier exists
///
/// The pool work ring and the control ring are independent SPSC FIFOs; polling
/// them in any fixed order cannot reconstruct the RT thread's publication order
/// when a rate change interleaves with audio (`... A4 A5 M B1 B2 ...`). The
/// barrier carries the ordering information *inside* the audio FIFO, so the
/// I/O thread applies `M` between `A5` and `B1` exactly like the inline ring
/// does — the promoted transport stays byte-exact under mid-stream metadata
/// changes.
pub const CONTROL_BARRIER_SLOT: u16 = u16::MAX;

/// Small descriptor published by the RT thread and consumed by the I/O thread.
///
/// `#[repr(C)]`, 4 bytes: the 16-bit slot index into the pool plus the valid
/// planar sample count. The payload itself stays in the preallocated slot —
/// only this word travels through the `work` SPSC ring. A descriptor with
/// [`slot == CONTROL_BARRIER_SLOT`](CONTROL_BARRIER_SLOT) is a control barrier,
/// not audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Descriptor {
    /// Index of the pool slot holding the audio data, or
    /// [`CONTROL_BARRIER_SLOT`] for a control barrier.
    pub slot: u16,
    /// Valid f32 samples in the slot (`2 * frames`), as published by
    /// `fill_planar`. `0` for a control barrier.
    pub valid_len: u16,
}

/// The preallocated slot array shared between the RT and I/O threads.
///
/// `slots[i]` is accessed by a thread only while it owns index `i` according
/// to the ownership protocol above. `UnsafeCell` + `unsafe impl Sync` encode
/// "the *array* may be shared, but each *element* is exclusively owned at any
/// moment"; soundness rests on the ring-handoff protocol, not on locks.
struct PoolSlots<const N: usize> {
    slots: Box<[UnsafeCell<AlignedBlock<MAX_BLOCK_SIZE>>; N]>,
    /// Total slots acquired/popped but dropped without publish/release
    /// (fail-closed leak telemetry; see module docs).
    slot_leaks: AtomicU64,
}

// SAFETY: `PoolSlots` may be shared between the two transport threads because
// access to any element is serialized by the ownership protocol: a mutable
// borrow of `slots[i]` exists only on the thread that popped index `i` from
// the `free` ring (RT thread) and ends before the `work` push; a shared borrow
// exists only on the thread that popped `Descriptor{i}` from the `work` ring
// (I/O thread) and ends before the `free` push. Both rings are SPSC, so at any
// instant at most one thread holds access to any given slot — no data race and
// no aliasing `&mut`. `slot_leaks` is a lock-free atomic.
unsafe impl<const N: usize> Sync for PoolSlots<N> {}

/// Full pool: two SPSC rings plus the preallocated slot array.
///
/// Created on the main thread at recording start and [`split`](Self::split)
/// into the two halves that move to the RT and I/O threads. Construction is
/// off-RT (cold path) and performs the single preallocation of the pool.
pub struct RecordingPool<const N: usize> {
    inner: Arc<PoolSlots<N>>,
    work: Producer<Descriptor>,
    work_consumer: Consumer<Descriptor>,
    free_consumer: Consumer<u16>,
    free: Producer<u16>,
}

impl<const N: usize> RecordingPool<N> {
    /// Preallocates `N` slots and seeds the free-list ring with every index.
    ///
    /// # Panics
    ///
    /// Panics if `N == 0` (a pool with no slots is meaningless) or if the ring
    /// seeding fails — impossible for `N > 0` because both rings have capacity
    /// `N`, but checked explicitly so a wrong capacity can never silently
    /// reduce the pool.
    pub fn new() -> Self {
        assert!(N > 0, "RecordingPool requires at least one slot");
        let (work, work_consumer) = RingBuffer::new(N);
        let (mut free, free_consumer) = RingBuffer::new(N);
        for idx in 0..N as u16 {
            free.push(idx)
                .unwrap_or_else(|_| panic!("free ring seeded with capacity {N}"));
        }
        let slots = {
            // SAFETY: `new_uninit` on an array of length N > 0 yields N
            // uninitialized slots; each is written exactly once below before
            // `assume_init`.
            let mut boxed = Box::<[UnsafeCell<AlignedBlock<MAX_BLOCK_SIZE>>; N]>::new_uninit();
            let ptr = boxed.as_mut_ptr() as *mut UnsafeCell<AlignedBlock<MAX_BLOCK_SIZE>>;
            for i in 0..N {
                unsafe {
                    ptr.add(i)
                        .write(UnsafeCell::new(AlignedBlock::<MAX_BLOCK_SIZE>::new_uninit()))
                };
            }
            // SAFETY: every slot was initialized in the loop above.
            unsafe { boxed.assume_init() }
        };
        Self {
            inner: Arc::new(PoolSlots {
                slots,
                slot_leaks: AtomicU64::new(0),
            }),
            work,
            work_consumer,
            free_consumer,
            free,
        }
    }

    /// Splits the pool into its two single-thread halves.
    ///
    /// `PoolProducer` moves to the RT thread; `PoolConsumer` moves to the I/O
    /// thread. Both halves share the preallocated slots through an `Arc` whose
    /// soundness is the ownership protocol documented at module level.
    pub fn split(self) -> (PoolProducer<N>, PoolConsumer<N>) {
        let producer = PoolProducer {
            inner: self.inner.clone(),
            work: self.work,
            free: self.free_consumer,
        };
        let consumer = PoolConsumer {
            inner: self.inner,
            work: self.work_consumer,
            free: self.free,
        };
        (producer, consumer)
    }
}

impl<const N: usize> Default for RecordingPool<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// RT-thread half of the pool. `Send` by construction (single producer).
pub struct PoolProducer<const N: usize> {
    inner: Arc<PoolSlots<N>>,
    work: Producer<Descriptor>,
    free: Consumer<u16>,
}

impl<const N: usize> PoolProducer<N> {
    /// Number of preallocated slots (== `N`).
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Number of slots currently in the free ring (available to acquire).
    #[inline]
    pub fn free_available(&self) -> usize {
        self.free.slots()
    }

    /// Total slots lost by dropping an `AcquiredSlot` without publishing.
    #[inline]
    pub fn leaked_slots(&self) -> u64 {
        self.inner.slot_leaks.load(Ordering::Relaxed)
    }

    /// Pops a free slot index and returns exclusive mutable access to the
    /// preallocated block. `None` means the pool is exhausted — the caller
    /// accounts it as an overrun (mirroring a full inline ring).
    #[inline]
    pub fn try_acquire(&mut self) -> Option<AcquiredSlot<'_, N>> {
        let idx = self.free.pop().ok()?;
        // SAFETY: `idx` was popped from the `free` ring, whose SPSC producer
        // (I/O thread) pushed it only after finishing reading the slot and
        // before any new descriptor for it was published. Therefore no other
        // thread can hold this slot while we own it (state RT-owned).
        let block = self.inner.slots[idx as usize].get();
        Some(AcquiredSlot {
            producer: self,
            idx,
            block,
        })
    }

    /// Drains the free ring for post-shutdown verification (acceptance: every
    /// index present exactly once — no ABA, no double-return).
    #[cfg(any(test, feature = "testing"))]
    pub fn drain_free_for_check(&mut self) -> Vec<u16> {
        let mut out = Vec::with_capacity(N);
        while let Ok(idx) = self.free.pop() {
            out.push(idx);
        }
        out
    }

    /// Publishes a **control barrier** into the `work` ring.
    ///
    /// The barrier is a pure ordering marker: it tells the I/O thread that a
    /// control message — pushed into the dedicated control ring **just before**
    /// this call — must be applied at this exact position of the audio stream
    /// (mid-stream metadata changes). Unlike [`AcquiredSlot::publish`] it is
    /// *not* infallible by construction: it does not hold a pool slot, so the
    /// `work` ring may be full. Returns `false` on a full ring — the caller
    /// leaves the metadata unconfirmed and retries on the next callback (audio
    /// publication stays gated on the confirmation, so no ordering can break).
    #[inline]
    pub fn try_push_barrier(&mut self) -> bool {
        self.work
            .push(Descriptor {
                slot: CONTROL_BARRIER_SLOT,
                valid_len: 0,
            })
            .is_ok()
    }
}

/// A pool slot owned by the RT thread, mid-fill.
///
/// Consumed by [`publish`](AcquiredSlot::publish). Dropping without publishing
/// leaks the slot and is counted in `leaked_slots()`.
#[must_use = "an acquired pool slot must be published; dropping it leaks the slot"]
pub struct AcquiredSlot<'a, const N: usize> {
    producer: &'a mut PoolProducer<N>,
    idx: u16,
    block: *mut AlignedBlock<MAX_BLOCK_SIZE>,
}

impl<const N: usize> AcquiredSlot<'_, N> {
    /// The slot index this guard holds (test/bench verification aid).
    #[cfg(any(test, feature = "testing"))]
    pub fn slot_index(&self) -> u16 {
        self.idx
    }

    /// Mutable access to the preallocated audio block (write the frames here).
    #[inline]
    pub fn block_mut(&mut self) -> &mut AlignedBlock<MAX_BLOCK_SIZE> {
        // SAFETY: this slot is exclusively owned by the RT thread (popped from
        // `free`, not yet published), so forming `&mut` is sound and cannot
        // alias the I/O thread's reads.
        unsafe { &mut *self.block }
    }

    /// Publishes the slot to the I/O thread by pushing its descriptor.
    ///
    /// Infallible by construction (`work` ring capacity `N` and ≥ 1 slot held
    /// by this producer, so `work` can never be full) — returns `false` only
    /// on an invariant violation, in which case the slot is counted as leaked
    /// rather than double-returned.
    #[inline]
    pub fn publish(self) -> bool {
        // SAFETY: exclusive ownership (see `block_mut`).
        let valid_len = unsafe { (*self.block).valid_len() } as u16;
        let desc = Descriptor {
            slot: self.idx,
            valid_len,
        };
        let pushed = self.producer.work.push(desc).is_ok();
        if pushed {
            // Ownership moved to the `work` ring; suppress the leak counter.
            std::mem::forget(self);
        }
        pushed
    }
}

impl<const N: usize> Drop for AcquiredSlot<'_, N> {
    fn drop(&mut self) {
        // A slot dropped mid-fill is gone forever (the free-ring producer is on
        // the I/O thread). Count it fail-closed toward overrun accounting.
        self.producer
            .inner
            .slot_leaks
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// I/O-thread half of the pool. `Send` by construction (single consumer).
pub struct PoolConsumer<const N: usize> {
    inner: Arc<PoolSlots<N>>,
    work: Consumer<Descriptor>,
    free: Producer<u16>,
}

impl<const N: usize> PoolConsumer<N> {
    /// Number of descriptors waiting to be consumed.
    #[inline]
    pub fn work_available(&self) -> usize {
        self.work.slots()
    }

    /// `true` when the I/O thread has drained every published descriptor.
    #[inline]
    pub fn work_is_empty(&self) -> bool {
        self.work.is_empty()
    }

    /// `true` when the producer half has been dropped — the work ring can never
    /// receive another descriptor (used by the I/O loop's terminal condition).
    #[inline]
    pub fn work_is_abandoned(&self) -> bool {
        self.work.is_abandoned()
    }

    /// Pops the next descriptor and returns read access to its slot.
    ///
    /// A [`control barrier`](CONTROL_BARRIER_SLOT) descriptor yields an
    /// [`InFlightBlock`] whose [`is_barrier`](InFlightBlock::is_barrier) is
    /// `true` and whose [`block`](InFlightBlock::block) must not be accessed.
    #[inline]
    pub fn try_pop(&mut self) -> Option<InFlightBlock<'_, N>> {
        let desc = self.work.pop().ok()?;
        if desc.slot == CONTROL_BARRIER_SLOT {
            // SAFETY-free path: no slot is associated with a barrier; the
            // block pointer stays null and `InFlightBlock::block` rejects it.
            return Some(InFlightBlock {
                consumer: self,
                desc,
                block: std::ptr::null(),
            });
        }
        // SAFETY: `desc` was popped from the `work` ring, whose SPSC producer
        // (RT thread) pushed it only after finishing writing the slot and
        // before the slot can be re-acquired (the index is not in `free` until
        // we release it). No other thread touches this slot while we own it
        // (state I/O-owned).
        let block = self.inner.slots[desc.slot as usize].get();
        Some(InFlightBlock {
            consumer: self,
            desc,
            block,
        })
    }
}

/// A pool slot owned by the I/O thread, mid-consume.
///
/// Consumed by [`release`](InFlightBlock::release). Dropping without releasing
/// leaks the slot and is counted in the producer's `leaked_slots()`.
#[must_use = "an in-flight pool block must be released; dropping it leaks the slot"]
pub struct InFlightBlock<'a, const N: usize> {
    consumer: &'a mut PoolConsumer<N>,
    desc: Descriptor,
    block: *const AlignedBlock<MAX_BLOCK_SIZE>,
}

impl<const N: usize> InFlightBlock<'_, N> {
    /// `true` for a [control barrier](CONTROL_BARRIER_SLOT) — an ordering
    /// marker carrying no audio. The I/O thread must apply the pending control
    /// message (pushed to the control ring before the barrier) instead of
    /// writing a block.
    #[inline]
    pub fn is_barrier(&self) -> bool {
        self.desc.slot == CONTROL_BARRIER_SLOT
    }

    /// Read access to the audio block (consume it here; no copy out of the
    /// pool is needed).
    ///
    /// # Panics
    ///
    /// Panics on a [control barrier](CONTROL_BARRIER_SLOT) — barriers carry no
    /// audio block; check [`is_barrier`](Self::is_barrier) first.
    #[inline]
    pub fn block(&self) -> &AlignedBlock<MAX_BLOCK_SIZE> {
        assert!(
            !self.is_barrier(),
            "a control barrier carries no audio block"
        );
        // SAFETY: this slot is exclusively owned by the I/O thread (popped
        // from `work`, not yet released), so forming `&` is sound and cannot
        // alias the RT thread's writes.
        unsafe { &*self.block }
    }

    /// The descriptor that carried this block (slot index + valid length), or
    /// the barrier marker for [`control barriers`](CONTROL_BARRIER_SLOT).
    #[inline]
    pub fn descriptor(&self) -> Descriptor {
        self.desc
    }

    /// Returns the slot index to the free-list ring (infallible by
    /// construction, as with [`AcquiredSlot::publish`]).
    ///
    /// For a [control barrier](CONTROL_BARRIER_SLOT) this is a no-op — the
    /// barrier holds no slot to recycle.
    #[inline]
    pub fn release(self) -> bool {
        if self.is_barrier() {
            // No slot to recycle; ownership of the marker is simply dissolved.
            std::mem::forget(self);
            return true;
        }
        let pushed = self.consumer.free.push(self.desc.slot).is_ok();
        if pushed {
            // Ownership moved to the `free` ring; suppress the leak counter.
            std::mem::forget(self);
        }
        pushed
    }
}

impl<const N: usize> Drop for InFlightBlock<'_, N> {
    fn drop(&mut self) {
        self.consumer
            .inner
            .slot_leaks
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[path = "pool_test.rs"]
mod pool_test;
