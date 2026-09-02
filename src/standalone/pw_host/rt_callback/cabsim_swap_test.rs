// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use std::assert_matches;

#[test]
fn empty_consumer_no_change_and_clean_lot() {
    let (_prod, mut cons) = rtrb::RingBuffer::new(4);
    let mut active = None;
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let mut deferred = None;
    let mut structural_applied = 0usize;

    drain_cabsims(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert!(active.is_none());
    assert!(!parking_lot_dirty.load(Ordering::Acquire));
    assert!(gc_c.pop().is_err());
}

#[test]
fn swap_clears_active_and_sets_dirty() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let ir = [1.0f32, 0.5, 0.25];
    let mut active = Some(Box::new(make_pair(&ir, 64, 48000)));
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let req_gen = flags.requested_cabsim_generation.load(Ordering::Acquire);
    let mut deferred = None;
    let mut structural_applied = 0usize;

    // Push None to bypass / clear cabsim
    prod.push(Box::new(CabSimSwapPayload {
        generation: req_gen,
        pair: None,
    }))
    .unwrap();

    drain_cabsims(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert!(active.is_none());
    assert_eq!(
        flags.applied_cabsim_generation.load(Ordering::Acquire),
        req_gen
    );
    assert!(parking_lot_dirty.load(Ordering::Acquire));
    // The retired payload reaches GC as a single moved Box (F-RB-007).
    let old = gc_c.pop().unwrap();
    assert_matches!(old, GcItem::CabSimSwap(_));
    assert!(gc_c.pop().is_err());
}

#[test]
fn swap_replaces_active_and_gcs_retired_pair() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let ir = [1.0f32, 0.5, 0.25];
    let mut active = Some(Box::new(make_pair(&ir, 64, 48000)));
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let req_gen = flags.requested_cabsim_generation.load(Ordering::Acquire);
    let mut deferred = None;
    let mut structural_applied = 0usize;

    // Continuous IR replacement: two successive pairs.
    prod.push(Box::new(CabSimSwapPayload {
        generation: req_gen,
        pair: Some(Box::new(make_pair(&ir, 64, 96000))),
    }))
    .unwrap();
    prod.push(Box::new(CabSimSwapPayload {
        generation: req_gen,
        pair: Some(Box::new(make_pair(&ir, 128, 96000))),
    }))
    .unwrap();

    drain_cabsims(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    let installed = active.as_deref().expect("latest pair installed");
    assert_eq!(installed.sample_rate, 96000);
    assert_eq!(installed.partition_size(), 128);
    assert_eq!(
        flags.applied_cabsim_generation.load(Ordering::Acquire),
        req_gen
    );
    assert!(parking_lot_dirty.load(Ordering::Acquire));
    // Both retired payloads reach GC as single moved boxes.
    assert_matches!(gc_c.pop().unwrap(), GcItem::CabSimSwap(_));
    assert_matches!(gc_c.pop().unwrap(), GcItem::CabSimSwap(_));
    assert!(gc_c.pop().is_err());
}

#[test]
fn stale_cabsim_payload_is_discarded_without_modifying_active() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let ir = [1.0f32, 0.5, 0.25];
    let initial_pair = make_pair(&ir, 64, 48000);
    let mut active = Some(Box::new(initial_pair));
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();

    // Current requested generation is 2
    flags
        .requested_cabsim_generation
        .store(2, Ordering::Release);
    flags.applied_cabsim_generation.store(1, Ordering::Release);

    // Stale payload with generation 1
    prod.push(Box::new(CabSimSwapPayload {
        generation: 1,
        pair: Some(Box::new(make_pair(&ir, 128, 96000))),
    }))
    .unwrap();

    let mut deferred = None;
    let mut structural_applied = 0usize;
    drain_cabsims(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    // Active pair is unchanged (still 64 partition, 48000 rate)
    let installed = active.as_deref().expect("initial pair retained");
    assert_eq!(installed.sample_rate, 48000);
    assert_eq!(installed.partition_size(), 64);
    // Applied generation is untouched (remains 1)
    assert_eq!(flags.applied_cabsim_generation.load(Ordering::Acquire), 1);
    // Stale payload was discarded to GC
    assert_matches!(gc_c.pop().unwrap(), GcItem::CabSimSwap(_));
}

// ── T2.5 Structural Budget & Coalescing (F-RB-011) ──────────────────────

/// With multiple commands queued (including a `None` bypass), exactly one
/// structural swap applies per callback and the obsolete intermediate
/// commands are coalesced to the GC cascade — the latest command wins.
#[test]
fn budget_applies_one_and_coalesces_backlog_latest_wins() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let ir = [1.0f32, 0.5, 0.25];
    let mut active = Some(Box::new(make_pair(&ir, 64, 48000)));
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(8);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let req_gen = flags.requested_cabsim_generation.load(Ordering::Acquire);

    // Backlog: two successive pairs, then a `None` bypass as the newest
    // command (e.g. F-RB-006 rebuild-failure rollback).
    prod.push(Box::new(CabSimSwapPayload {
        generation: req_gen,
        pair: Some(Box::new(make_pair(&ir, 64, 96000))),
    }))
    .unwrap();
    prod.push(Box::new(CabSimSwapPayload {
        generation: req_gen,
        pair: Some(Box::new(make_pair(&ir, 128, 96000))),
    }))
    .unwrap();
    prod.push(Box::new(CabSimSwapPayload {
        generation: req_gen,
        pair: None,
    }))
    .unwrap();

    let mut deferred = None;
    let mut structural_applied = 0usize;
    drain_cabsims(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert!(
        active.is_none(),
        "the latest command (bypass) must win over the queued pairs"
    );
    assert_eq!(
        structural_applied, 1,
        "at most one structural swap per callback"
    );
    assert!(deferred.is_none());
    assert!(cons.is_empty());
    assert!(flags.check_flag(RT_STATUS_STRUCTURAL_SUPERSEDED));

    // GC: 2 superseded payloads + the replaced active payload = 3 moved Boxes.
    let mut pairs = 0usize;
    while let Ok(item) = gc_c.pop() {
        assert_matches!(item, GcItem::CabSimSwap(_));
        pairs += 1;
    }
    assert_eq!(pairs, 3);
}

/// When the shared structural budget is exhausted, the cab-sim command is
/// parked and applied by the next callback (fresh budget).
#[test]
fn budget_exhausted_parks_command_resolved_next_callback() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let ir = [1.0f32, 0.5, 0.25];
    let mut active = Some(Box::new(make_pair(&ir, 64, 48000)));
    let (mut gc_p, mut _gc_c) = rtrb::RingBuffer::new(8);
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let gc_overflow = GcOverflowBuffer::default();
    let flags = RtStatusFlags::new();
    let req_gen = flags.requested_cabsim_generation.load(Ordering::Acquire);

    prod.push(Box::new(CabSimSwapPayload {
        generation: req_gen,
        pair: Some(Box::new(make_pair(&ir, 128, 96000))),
    }))
    .unwrap();

    let mut deferred = None;
    let mut structural_applied = 1usize; // another swap applied earlier
    drain_cabsims(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );
    assert_eq!(
        active.as_ref().unwrap().partition_size(),
        64,
        "not installed"
    );
    assert!(deferred.is_some(), "command must be parked");
    assert!(flags.check_flag(RT_STATUS_STRUCTURAL_DEFERRED));

    // Next callback: fresh budget → the parked pair is installed.
    structural_applied = 0;
    drain_cabsims(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );
    assert_eq!(active.as_ref().unwrap().partition_size(), 128);
    assert_eq!(active.as_ref().unwrap().sample_rate, 96000);
    assert!(deferred.is_none());
}

#[cfg(all(test, feature = "heap-audit"))]
mod heap_audit_tests {
    use super::*;
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    const IR: [f32; 16] = [
        1.0, 0.5, 0.25, 0.125, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    ];

    /// Channel/state harness for an audited RT drain cycle. All pairs and
    /// channel contents are prepared *before* the `TrackingGuard` starts.
    struct DrainHarness {
        prod: rtrb::Producer<Box<CabSimSwapPayload>>,
        cons: Consumer<Box<CabSimSwapPayload>>,
        active: Option<Box<CabSimPair>>,
        deferred: Option<Box<CabSimSwapPayload>>,
        structural_applied: usize,
        gc_p: rtrb::Producer<GcItem>,
        gc_c: rtrb::Consumer<GcItem>,
        parking_lot: [Option<GcItem>; 16],
        parking_lot_dirty: AtomicBool,
        gc_overflow: GcOverflowBuffer,
        flags: RtStatusFlags,
    }

    impl DrainHarness {
        fn new(
            cons_cap: usize,
            gc_cap: usize,
            overflow_cap: usize,
            active: Option<Box<CabSimPair>>,
        ) -> Self {
            let (prod, cons) = rtrb::RingBuffer::new(cons_cap);
            let (gc_p, gc_c) = rtrb::RingBuffer::new(gc_cap);
            Self {
                prod,
                cons,
                active,
                deferred: None,
                structural_applied: 0,
                gc_p,
                gc_c,
                parking_lot: Default::default(),
                parking_lot_dirty: AtomicBool::new(false),
                gc_overflow: GcOverflowBuffer::new(overflow_cap),
                flags: RtStatusFlags::new(),
            }
        }

        fn push(&mut self, item: Option<Box<CabSimPair>>) {
            let req_gen = self
                .flags
                .requested_cabsim_generation
                .load(Ordering::Acquire);
            self.prod
                .push(Box::new(CabSimSwapPayload {
                    generation: req_gen,
                    pair: item,
                }))
                .unwrap();
        }

        fn push_pair(&mut self, partition: usize, rate: u32) {
            self.push(Some(Box::new(make_pair(&IR, partition, rate))));
        }

        /// Runs one RT drain cycle under the allocation watchdog, asserts zero
        /// heap traffic, then reclaims retired pairs through the off-RT GC.
        fn run_audited_drain(&mut self, label: &str) {
            let (allocs, deallocs, reallocs) = {
                let _guard = TrackingGuard::new();
                drain_cabsims(
                    &mut self.cons,
                    &mut self.deferred,
                    &mut self.structural_applied,
                    &mut self.active,
                    &mut self.gc_p,
                    &mut self.parking_lot,
                    &self.parking_lot_dirty,
                    &self.gc_overflow,
                    &self.flags,
                );
                (get_alloc_count(), get_dealloc_count(), get_realloc_count())
            };
            // Medido: alloc=0, dealloc=0, realloc=0 (drain_cabsims RT cycle)
            assert_eq!(
                allocs, 0,
                "heap allocations detected during {label}: count={allocs}"
            );
            assert_eq!(deallocs, 0, "dealloc no callback RT during {label}");
            assert_eq!(reallocs, 0, "realloc no callback RT during {label}");
            // Off-RT: the retired pairs must be reclaimable via the GC drain.
            while let Ok(item) = self.gc_c.pop() {
                drop(item);
            }
            for item in self.gc_overflow.drain(&self.flags) {
                drop(item);
            }
            for slot in self.parking_lot.iter_mut() {
                *slot = None;
            }
        }
    }

    #[test]
    fn heap_audit_initial_install_zero_alloc() {
        let mut h = DrainHarness::new(4, 8, 64, None);
        h.push_pair(64, 48000);
        h.run_audited_drain("initial install");
        assert!(h.active.is_some());
        assert!(h.parking_lot_dirty.load(Ordering::Acquire));
    }

    #[test]
    fn heap_audit_continuous_replacement_zero_alloc() {
        let mut h = DrainHarness::new(8, 8, 64, Some(Box::new(make_pair(&IR, 64, 48000))));
        // Pre-fill several successive rebuilds before the audit starts.
        for partition in [64usize, 128, 256] {
            h.push_pair(partition, 48000);
        }
        h.run_audited_drain("continuous IR replacement");
        let installed = h.active.as_deref().expect("latest pair installed");
        assert_eq!(installed.partition_size(), 256);
        assert!(h.parking_lot_dirty.load(Ordering::Acquire));
    }

    #[test]
    fn heap_audit_deactivation_to_none_zero_alloc() {
        let mut h = DrainHarness::new(4, 8, 64, Some(Box::new(make_pair(&IR, 64, 48000))));
        h.push(None);
        h.run_audited_drain("deactivation to None");
        assert!(h.active.is_none());
        assert!(h.parking_lot_dirty.load(Ordering::Acquire));
    }

    #[test]
    fn heap_audit_disposal_with_saturated_parking_lot_zero_alloc() {
        // Saturation scenario: GC SPSC capacity 1 + 16 parking slots, with the
        // cascade spilling through tier 3 (overflow ring) under load.
        let mut h = DrainHarness::new(64, 1, 4, Some(Box::new(make_pair(&IR, 64, 48000))));

        // Pre-fill the overflow ring with old payloads, then drain it once so the
        // ring is empty but the audit forces tier 3 after SPSC+lot fill.
        for i in 0..4 {
            h.gc_overflow
                .push(GcItem::CabSimSwap(Box::new(CabSimSwapPayload {
                    generation: 0,
                    pair: Some(Box::new(make_pair(&IR, 64 + i * 64, 48000))),
                })));
        }
        for item in h.gc_overflow.drain(&h.flags) {
            drop(item);
        }

        // 18 successive swaps: 1 fits the SPSC, 16 park, 1+ spill to overflow.
        for i in 0..18 {
            h.push_pair(64 + (i as usize) * 64, 48000);
        }
        h.run_audited_drain("disposal with saturated parking lot");
        assert!(h.parking_lot_dirty.load(Ordering::Acquire));
    }
}
