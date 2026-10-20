// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for recording transport abstraction: disabled sender behavior,
//! production defaults, round-trip audio delivery, exhaustion reporting, and
//! pending barrier retries.

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

/// Debug tripwire for the metadata route: while the pool transport is the
/// compiled default, an `Inline` sender reaching `try_push_metadata` is an
/// accidental construction (`create_recording_transport` can never supply
/// this pairing) and debug builds accuse fail-fast via
/// [`assert_inline_rollback_build`]. Release builds compile the guard away
/// and this test does not exist there.
#[cfg(debug_assertions)]
#[should_panic(expected = "Inline recording transport")]
#[test]
fn inline_sender_metadata_route_is_accused_outside_rollback_build() {
    let (producer, _consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(RING_CAPACITY);
    let mut sender = RecordingSender::Inline(Some(producer));

    assert!(!sender.try_push_metadata(AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    }));
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
        Ok(ControlPayload::Metadata { seq, meta: m }) => {
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
