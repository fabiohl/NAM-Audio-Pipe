// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

/// Test-only ownership state machine. Every legal transition updates the
/// map; any illegal transition (double-return, ABA reuse while in flight,
/// release of a slot not in flight) panics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Free,
    Rt,
    InFlight,
    Io,
}

struct OwnershipMap<const N: usize> {
    state: Vec<Owner>,
}

impl<const N: usize> OwnershipMap<N> {
    fn new() -> Self {
        Self {
            state: vec![Owner::Free; N],
        }
    }

    fn transition(&mut self, idx: usize, from: Owner, to: Owner) {
        let cur = &mut self.state[idx];
        assert_eq!(
            *cur, from,
            "illegal transition slot {idx}: expected {from:?}, was {cur:?}"
        );
        *cur = to;
    }
}

fn assert_bits_eq(a: &[f32], b: &[f32]) {
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x.to_bits(), y.to_bits(), "bitwise sample mismatch");
    }
}

/// T4.2 acceptance: single-threaded round trip — acquire, fill, publish,
/// pop, read, release — followed by a full free-ring drain proving every
/// index is present exactly once (no ABA, no double-return).
#[test]
fn pool_round_trip_reuses_slot_and_drains_exactly_once() {
    let pool = RecordingPool::<POOL_CAPACITY>::new();
    let (mut producer, mut consumer) = pool.split();
    let mut ownership = OwnershipMap::<POOL_CAPACITY>::new();

    for pass in 0..3 {
        let mut acquired = producer.try_acquire().expect("a free slot");
        let idx = acquired.slot_index() as usize;
        ownership.transition(idx, Owner::Free, Owner::Rt);

        let n = 4usize;
        let l: Vec<f32> = (0..n).map(|i| pass as f32 + i as f32).collect();
        let r: Vec<f32> = (0..n).map(|i| -(pass as f32 + i as f32)).collect();
        acquired.block_mut().fill_planar(&l, &r);
        assert!(acquired.publish(), "publish is infallible at capacity N");
        ownership.transition(idx, Owner::Rt, Owner::InFlight);

        let in_flight = consumer.try_pop().expect("published descriptor");
        assert_eq!(in_flight.descriptor().slot as usize, idx);
        assert_eq!(in_flight.descriptor().valid_len as usize, 2 * n);
        ownership.transition(idx, Owner::InFlight, Owner::Io);
        assert_bits_eq(in_flight.block().left_slice(), &l[..]);
        assert_bits_eq(in_flight.block().right_slice(), &r[..]);
        assert!(in_flight.release(), "release is infallible at capacity N");
        ownership.transition(idx, Owner::Io, Owner::Free);
    }

    // Shutdown: everything released → full free-ring drain contains each
    // index exactly once (acceptance: no ABA/double-return).
    let drained = producer.drain_free_for_check();
    assert_eq!(drained.len(), POOL_CAPACITY);
    let mut seen = std::collections::HashSet::new();
    for idx in drained {
        assert!(seen.insert(idx), "slot {idx} double-returned to the pool");
    }
    assert_eq!(seen.len(), POOL_CAPACITY);
    assert!(consumer.work_is_empty());
    assert_eq!(producer.leaked_slots(), 0);
}

/// Descriptors must arrive in FIFO order, matching the inline ring.
#[test]
fn pool_descriptors_are_fifo() {
    let pool = RecordingPool::<8>::new();
    let (mut producer, mut consumer) = pool.split();

    for i in 0..8u16 {
        let mut slot = producer.try_acquire().unwrap();
        let block = slot.block_mut();
        block.fill_planar(&[i as f32], &[-(i as f32)]);
        assert!(slot.publish());
    }

    let mut seen = Vec::new();
    while let Some(block) = consumer.try_pop() {
        seen.push(block.descriptor().slot);
        assert!(block.release());
    }
    assert_eq!(seen.len(), 8);
    // FIFO: the producer acquired slots in free-ring order, which after
    // seeding is 0,1,2,...,7 — and publish order is the same.
    assert_eq!(seen, (0..8u16).collect::<Vec<_>>());
    assert!(consumer.work_is_empty());
}

/// Pool exhaustion must surface as `None` (overrun) — never a panic, never
/// a slot leak — and every slot must return to `free` exactly once after
/// the consumer drains.
#[test]
fn pool_exhaustion_overruns_without_leak() {
    let pool = RecordingPool::<4>::new();
    let (mut producer, mut consumer) = pool.split();
    let mut ownership = OwnershipMap::<4>::new();

    // Fill the pool: 4 slots acquired and published, 0 consumed yet →
    // 5th acquire fails (the free ring is exhausted, mirroring a full
    // inline ring).
    for _ in 0..4 {
        let mut s = producer.try_acquire().expect("slots 0..3 free");
        let idx = s.slot_index() as usize;
        ownership.transition(idx, Owner::Free, Owner::Rt);
        s.block_mut().fill_planar(&[1.0], &[2.0]);
        assert!(s.publish(), "publish is infallible at capacity N");
        ownership.transition(idx, Owner::Rt, Owner::InFlight);
    }
    assert!(producer.try_acquire().is_none(), "pool exhausted → overrun");
    assert_eq!(producer.leaked_slots(), 0);

    // Consumer drains all four descriptors; every index returns exactly once.
    while let Some(in_flight) = consumer.try_pop() {
        let idx = in_flight.descriptor().slot as usize;
        ownership.transition(idx, Owner::InFlight, Owner::Io);
        assert!(in_flight.release());
        ownership.transition(idx, Owner::Io, Owner::Free);
    }
    assert_eq!(producer.free_available(), 4);
    assert_eq!(producer.drain_free_for_check().len(), 4);
    assert_eq!(producer.leaked_slots(), 0);
}

/// A dropped `AcquiredSlot` must be counted (fail-closed) and never
/// double-returned by a later release of the same index.
#[test]
fn pool_dropped_acquired_slot_is_counted_as_leak() {
    let pool = RecordingPool::<8>::new();
    let (mut producer, mut consumer) = pool.split();

    {
        let slot = producer.try_acquire().unwrap();
        // Dropped without publish → leak.
        drop(slot);
    }
    assert_eq!(producer.leaked_slots(), 1);
    // Remaining 7 slots are still usable and drain exactly once.
    for _ in 0..7 {
        let mut slot = producer.try_acquire().unwrap();
        slot.block_mut().fill_planar(&[1.0], &[2.0]);
        assert!(slot.publish());
    }
    while let Some(in_flight) = consumer.try_pop() {
        assert!(in_flight.release());
    }
    assert_eq!(producer.drain_free_for_check().len(), 7);
    assert_eq!(producer.leaked_slots(), 1);
}

/// A dropped `InFlightBlock` must be counted and the slot must not come
/// back to the pool.
#[test]
fn pool_dropped_in_flight_block_is_counted_as_leak() {
    let pool = RecordingPool::<8>::new();
    let (mut producer, mut consumer) = pool.split();

    let mut slot = producer.try_acquire().unwrap();
    slot.block_mut().fill_planar(&[1.0], &[2.0]);
    assert!(slot.publish());

    {
        let in_flight = consumer.try_pop().unwrap();
        // Dropped without release → leak.
        drop(in_flight);
    }
    assert_eq!(producer.leaked_slots(), 1);
    // Only 7 slots remain in the pool.
    for _ in 0..7 {
        let mut s = producer.try_acquire().unwrap();
        s.block_mut().fill_planar(&[1.0], &[2.0]);
        assert!(s.publish());
    }
    assert!(
        producer.try_acquire().is_none(),
        "8th slot is the leaked one"
    );
    while let Some(in_flight) = consumer.try_pop() {
        assert!(in_flight.release());
    }
    assert_eq!(producer.drain_free_for_check().len(), 7);
}

/// `try_acquire` after the consumer released everything must hand out the
/// full index set again (slot reuse across cycles, no state corruption).
#[test]
fn pool_slots_cycle_and_are_reusable() {
    let pool = RecordingPool::<16>::new();
    let (mut producer, mut consumer) = pool.split();

    for cycle in 0..64 {
        for i in 0..4 {
            let mut slot = producer.try_acquire().expect("4 slots free");
            let block = slot.block_mut();
            block.fill_planar(&[cycle as f32], &[i as f32]);
            assert!(slot.publish());
        }
        for _ in 0..4 {
            let in_flight = consumer.try_pop().expect("4 descriptors");
            assert!(in_flight.release());
        }
    }
    assert_eq!(producer.drain_free_for_check().len(), 16);
    assert_eq!(producer.leaked_slots(), 0);
}

/// Capacity/descriptor layout contract: 4-byte descriptor, 16-bit indices.
#[test]
fn descriptor_is_a_small_word() {
    assert_eq!(std::mem::size_of::<Descriptor>(), 4);
    assert_eq!(POOL_CAPACITY, super::super::buffer::RING_CAPACITY);
}

/// T4.3 ordering contract: a control barrier must (a) be published through
/// the `work` ring in FIFO position, (b) surface as `is_barrier()` without
/// an audio block, (c) release as a no-op (no slot recycled), and (d) never
/// disturb the slot ownership around it — audio descriptors before and
/// after the barrier still drain exactly once.
#[test]
fn control_barrier_preserves_fifo_position_and_slot_ownership() {
    let pool = RecordingPool::<8>::new();
    let (mut producer, mut consumer) = pool.split();

    // Audio A → barrier → audio B, in FIFO order.
    let mut a = producer.try_acquire().unwrap();
    a.block_mut().fill_planar(&[1.0], &[2.0]);
    assert!(a.publish());
    assert!(producer.try_push_barrier(), "barrier push is capacity-safe");
    let mut b = producer.try_acquire().unwrap();
    b.block_mut().fill_planar(&[3.0], &[4.0]);
    assert!(b.publish());

    // Consumer sees A, barrier, B — in exactly the published order.
    let a = consumer.try_pop().expect("audio A");
    assert!(!a.is_barrier());
    assert_eq!(a.block().left_slice(), &[1.0]);
    assert!(a.release());

    let barrier = consumer.try_pop().expect("barrier");
    assert!(barrier.is_barrier(), "the marker must surface as a barrier");
    assert_eq!(barrier.descriptor().slot, CONTROL_BARRIER_SLOT);
    assert_eq!(barrier.descriptor().valid_len, 0);
    assert!(barrier.release(), "barrier release is a no-op success");

    let b = consumer.try_pop().expect("audio B");
    assert!(!b.is_barrier());
    assert_eq!(b.block().left_slice(), &[3.0]);
    assert!(b.release());

    // Full ownership round-trip intact: every slot back exactly once.
    assert!(consumer.work_is_empty());
    let drained = producer.drain_free_for_check();
    assert_eq!(drained.len(), 8);
    assert_eq!(producer.leaked_slots(), 0);
}

/// The barrier slot marker must never collide with a real pool slot: the
/// free ring only ever contains `0..N`.
#[test]
fn barrier_slot_is_outside_the_free_ring_domain() {
    let pool = RecordingPool::<POOL_CAPACITY>::new();
    let (mut producer, _consumer) = pool.split();
    let drained = producer.drain_free_for_check();
    assert!(
        drained.iter().all(|&idx| idx != CONTROL_BARRIER_SLOT),
        "0xFFFF must never be handed out as a slot index"
    );
}
