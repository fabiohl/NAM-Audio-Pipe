// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING;
use std::assert_matches;
use std::sync::atomic::{AtomicBool, Ordering};

use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;

fn make_rs(pw: u32, nam: u32) -> Box<NamResampler> {
    Box::new(NamResampler::new(pw, nam, 64).unwrap())
}

fn make_stream(pw: u32, nam: u32) -> Box<StreamingResampleBuffer> {
    Box::new(StreamingResampleBuffer::new(pw, nam, 2048).unwrap())
}

fn make_payload(generation: u64, pw: u32, nam: u32) -> Box<ResamplerSwapPayload> {
    Box::new(ResamplerSwapPayload {
        generation,
        resampler: make_rs(pw, nam),
        stream: make_stream(pw, nam),
    })
}

#[test]
fn empty_consumer_no_change() {
    let (_prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut deferred = None;
    let mut structural_applied = 0usize;

    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(active.host_rate(), 48000);
    assert_eq!(active.nam_rate(), 48000);
    assert!(!parking_lot_dirty.load(Ordering::Acquire));
    assert!(gc_c.pop().is_err());
}

#[test]
fn single_swap_updates_active_and_clears_flag() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut deferred = None;
    let mut structural_applied = 0usize;

    // RT requested generation 1 and the main thread delivered a matching payload.
    flags.requested_rate_generation.store(1, Ordering::Release);
    prod.push(make_payload(1, 44100, 48000)).unwrap();

    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(active.host_rate(), 44100);
    assert_eq!(active.nam_rate(), 48000);
    assert_eq!(flags.active_rate.load(Ordering::Relaxed), 44100);
    assert_eq!(flags.active_rate_changed.load(Ordering::Relaxed), 44100);
    assert_eq!(
        flags.applied_rate_generation.load(Ordering::Relaxed),
        1,
        "applied generation must be recorded on install"
    );
    assert!(!flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    assert!(parking_lot_dirty.load(Ordering::Acquire));

    let old = gc_c.pop().unwrap();
    assert_matches!(old, GcItem::ResamplerSwap(_));
}

#[test]
fn multiple_swaps_keep_last() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut deferred = None;
    let mut structural_applied = 0usize;

    // Two payloads in flight; the request already moved to generation 2, so
    // the generation-1 envelope is stale and only generation 2 is applied.
    flags.requested_rate_generation.store(2, Ordering::Release);
    prod.push(make_payload(1, 44100, 48000)).unwrap();
    prod.push(make_payload(2, 96000, 48000)).unwrap();

    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(active.host_rate(), 96000);
    assert_eq!(flags.active_rate.load(Ordering::Relaxed), 96000);
    assert_eq!(flags.applied_rate_generation.load(Ordering::Relaxed), 2);
    assert!(!flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    assert!(parking_lot_dirty.load(Ordering::Acquire));

    // GC received the stale 44.1k envelope, then the previous active 48k.
    let stale = gc_c.pop().unwrap();
    match stale {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 44100),
        _ => panic!("Expected stale GcItem::ResamplerSwap(44100)"),
    }
    let previous = gc_c.pop().unwrap();
    match previous {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 48000),
        _ => panic!("Expected previous GcItem::ResamplerSwap(48000)"),
    }
    assert!(gc_c.pop().is_err());
}

#[test]
fn swap_cascades_old_resampler_to_gc() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut deferred = None;
    let mut structural_applied = 0usize;

    flags.requested_rate_generation.store(1, Ordering::Release);
    prod.push(make_payload(1, 44100, 48000)).unwrap();

    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    let gc_item = gc_c.pop().unwrap();
    match gc_item {
        GcItem::ResamplerSwap(payload) => {
            assert_eq!(payload.resampler.host_rate(), 48000);
        }
        _ => panic!("Expected GcItem::ResamplerSwap"),
    }
}

/// Deterministic lost-wakeup interleaving:
///
/// 1. RT requests A (generation 1) → `RESAMP_SWAP_PENDING` set.
/// 2. Main starts building A.
/// 3. RT renegotiates to B (generation 2) → generation advanced, PENDING re-set.
/// 4. Delivery of A is drained: it is stale → discarded to GC **without**
///    unmuting (PENDING stays set, active resampler untouched).
/// 5. Delivery of B is drained: applied successfully, generation recorded,
///    audio unmuted.
#[test]
fn stale_generation_is_gc_discarded_without_unmute() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut deferred = None;
    let mut structural_applied = 0usize;

    // (1) RT requests A: generation 1.
    flags.requested_rate_generation.store(1, Ordering::Release);
    flags.set_flag(RT_STATUS_RESAMP_SWAP_PENDING);

    // (2) Main builds A (generation 1) and delivers it.
    prod.push(make_payload(1, 44100, 48000)).unwrap();

    // (3) RT renegotiates to B while A is in flight: generation 2.
    flags.requested_rate_generation.store(2, Ordering::Release);
    flags.set_flag(RT_STATUS_RESAMP_SWAP_PENDING);

    // (4) Drain delivers A: stale → GC, no unmute, no install.
    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(active.host_rate(), 48000, "stale A must not be installed");
    assert!(
        flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING),
        "stale A must not unmute the callback"
    );
    assert_eq!(flags.applied_rate_generation.load(Ordering::Relaxed), 0);

    let stale = gc_c.pop().unwrap();
    match stale {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 44100),
        _ => panic!("Expected stale GcItem::ResamplerSwap(44100)"),
    }

    // (5) Main delivers B (generation 2) → applied and unmuted.
    prod.push(make_payload(2, 96000, 48000)).unwrap();
    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(active.host_rate(), 96000, "matching B must be installed");
    assert_eq!(
        flags.applied_rate_generation.load(Ordering::Relaxed),
        2,
        "invariant: applied == requested before unmute"
    );
    assert!(!flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    let previous = gc_c.pop().unwrap();
    match previous {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 48000),
        _ => panic!("Expected previous GcItem::ResamplerSwap(48000)"),
    }
}

/// A generation-0 envelope (built before any request was published) must
/// never be applied once a request exists: it is stale by definition.
#[test]
fn unversioned_payload_never_applied_when_request_exists() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut deferred = None;
    let mut structural_applied = 0usize;

    flags.requested_rate_generation.store(3, Ordering::Release);
    flags.set_flag(RT_STATUS_RESAMP_SWAP_PENDING);
    prod.push(make_payload(0, 44100, 48000)).unwrap();

    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(active.host_rate(), 48000);
    assert!(flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
    assert_eq!(flags.applied_rate_generation.load(Ordering::Relaxed), 0);
    let stale = gc_c.pop().unwrap();
    match stale {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 44100),
        _ => panic!("Expected stale GcItem::ResamplerSwap(44100)"),
    }
}

// ── Structural Budget & Coalescing ──────────────────────────────────────────

/// With multiple current-generation envelopes queued, exactly one swap
/// applies per callback and the obsolete intermediate envelopes are
/// coalesced to the GC cascade (latest-wins). The unmute happens exactly
/// once, for the newest build.
#[test]
fn budget_applies_one_and_coalesces_backlog() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(8);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();

    // Three builds for the same request generation: only the last applies.
    flags.requested_rate_generation.store(1, Ordering::Release);
    flags.set_flag(RT_STATUS_RESAMP_SWAP_PENDING);
    prod.push(make_payload(1, 44100, 48000)).unwrap();
    prod.push(make_payload(1, 96000, 48000)).unwrap();
    prod.push(make_payload(1, 192000, 48000)).unwrap();

    let mut deferred = None;
    let mut structural_applied = 0usize;
    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(active.host_rate(), 192000, "latest build must win");
    assert_eq!(
        flags.applied_rate_generation.load(Ordering::Relaxed),
        1,
        "applied generation recorded on install"
    );
    assert_eq!(
        structural_applied, 1,
        "at most one structural swap per callback"
    );
    assert!(deferred.is_none());
    assert!(cons.is_empty());
    assert!(
        !flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING),
        "the latest install unmutes exactly once"
    );
    assert!(flags.check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED));

    // GC: 2 superseded envelopes + the replaced active resampler = 3.
    let mut resamplers = 0usize;
    while let Ok(item) = gc_c.pop() {
        assert_matches!(item, GcItem::ResamplerSwap(_));
        resamplers += 1;
    }
    assert_eq!(resamplers, 3);
}

/// When the shared structural budget was consumed by another drain earlier
/// in the callback, the current-generation envelope is parked (deferred) —
/// the callback stays muted — and installed by the next callback.
#[test]
fn budget_exhausted_parks_envelope_resolved_next_callback() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_p, mut _gc_c) = rtrb::RingBuffer::new(8);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();

    flags.requested_rate_generation.store(1, Ordering::Release);
    flags.set_flag(RT_STATUS_RESAMP_SWAP_PENDING);
    prod.push(make_payload(1, 44100, 48000)).unwrap();

    let mut deferred = None;
    let mut structural_applied = 1usize; // another swap applied earlier
    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );
    assert_eq!(active.host_rate(), 48000, "not installed out of budget");
    assert!(deferred.is_some(), "envelope must be parked");
    assert!(flags.check_flag(RT_STATUS_STRUCTURAL_DEFERRED));
    assert!(
        flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING),
        "callback stays muted until the parked envelope is applied"
    );

    // Next callback: fresh budget → parked envelope installed and unmuted.
    structural_applied = 0;
    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );
    assert_eq!(active.host_rate(), 44100);
    assert!(deferred.is_none());
    assert!(!flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
}
