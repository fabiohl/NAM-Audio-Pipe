// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cab-sim Convolution Engine Draining (Zero-Alloc Swap)
//! Replaces the active stereo-decoupled convolution pair without using memory
//! allocation in the critical path.
//!
//! Budgeting (F-RB-011 / T2.5): at most [`STRUCTURAL_SWAPS_PER_CALLBACK`]
//! structural swap applies per callback (budget shared across every RT swap
//! drain); pairs in the coalescing window collapse to the latest one
//! (intermediate pairs discarded to GC) and the excess is parked in the
//! deferred slot for the next callback.

use super::commands::{STRUCTURAL_POPS_PER_CALLBACK, STRUCTURAL_SWAPS_PER_CALLBACK};
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED,
    RtStatusFlags, gc_cascade,
};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair;

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// Drains the cab-sim pair SPSC channel and swaps the active pair atomically.
///
/// Follows the same cascade pattern as `drain_resamplers`:
/// GC channel → parking_lot → overflow buffer.
///
/// An `Option` is used so that `None` can be sent to clear/bypass the
/// convolution (F-RB-006 rollback: safe bypass beats a divergent-rate IR).
///
/// The pair is `Box`ed for its whole lifetime (F-RB-007): the active state,
/// the SPSC channel and the GC item all carry the *same* `Box`. The retired
/// pair is moved — via `std::mem::replace` — directly into a single
/// `GcItem::CabSimPair`, so installing, replacing or removing the cab-sim
/// performs zero allocations/deallocations on the audio thread.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
pub fn drain_cabsims(
    cabsim_consumer: &mut Consumer<Option<Box<CabSimPair>>>,
    deferred: &mut Option<Option<Box<CabSimPair>>>,
    structural_applied: &mut usize,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    // Phase 0 — resolve a pair (or a `None` bypass) deferred by the previous
    // callback.
    if let Some(pending) = deferred.take() {
        let head_queued = cabsim_consumer.peek().is_ok();
        if head_queued {
            // A newer command is already queued (latest-wins): the deferred
            // pair is obsolete and cascades to GC.
            discard_cabsim(
                pending,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status_for_process
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
        } else if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_cabsim(
                pending,
                active_cabsim,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            *structural_applied += 1;
        } else {
            // Budget exhausted and nothing newer queued: re-park.
            *deferred = Some(pending);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    // Phase 1 — bounded drain with coalescing (F-RB-011 / T2.5).
    let mut candidate: Option<Option<Box<CabSimPair>>> = None;
    let mut pops = 0usize;
    while pops < STRUCTURAL_POPS_PER_CALLBACK {
        let Some(new_pair) = cabsim_consumer.pop().ok() else {
            break;
        };
        pops += 1;
        if let Some(older) = candidate.replace(new_pair) {
            // Coalescing: an intermediate command is obsolete — its pair (if
            // any) cascades to GC (latest-wins).
            discard_cabsim(
                older,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status_for_process
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(new_pair) = candidate {
        if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_cabsim(
                new_pair,
                active_cabsim,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            *structural_applied += 1;
        } else if deferred.is_none() {
            *deferred = Some(new_pair);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // The deferred slot holds an older command; the popped one is newer
            // — supersede the parked command and park this one (latest-wins).
            let older = deferred.take().expect("slot occupied, checked above");
            discard_cabsim(
                older,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_SUPERSEDED);
            rt_status_for_process
                .structural_superseded_total
                .fetch_add(1, Ordering::Relaxed);
            *deferred = Some(new_pair);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Installs a cab-sim command atomically: the active pair (or bypass) is
/// replaced and the retired pair cascades to GC as a single moved `Box`
/// (F-RB-007).
#[inline(always)]
fn install_cabsim(
    new_pair: Option<Box<CabSimPair>>,
    active_cabsim: &mut Option<Box<CabSimPair>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    let old_pair = std::mem::replace(active_cabsim, new_pair);

    if let Some(old) = old_pair {
        parking_lot_dirty.store(true, Ordering::Release);
        gc_cascade(
            Some(GcItem::CabSimPair(old)),
            gc_producer,
            parking_lot,
            gc_overflow_for_process,
            rt_status_for_process,
        );
    }
}

/// Discards an obsolete cab-sim command to the GC cascade (its pair, if any,
/// is moved as a single `Box` — F-RB-007).
#[inline(always)]
fn discard_cabsim(
    old_pair: Option<Box<CabSimPair>>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    if let Some(old) = old_pair {
        parking_lot_dirty.store(true, Ordering::Release);
        gc_cascade(
            Some(GcItem::CabSimPair(old)),
            gc_producer,
            parking_lot,
            gc_overflow_for_process,
            rt_status_for_process,
        );
    }
}

#[cfg(test)]
fn make_pair(ir: &[f32], partition: usize, rate: u32) -> CabSimPair {
    use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;
    use neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine;
    let make_adapter = || {
        let engine = ConvEngine::new(ir, partition).unwrap();
        CabSimAdapter::new(Box::new(engine)).unwrap()
    };
    CabSimPair {
        l: Box::new(make_adapter()),
        r: Box::new(make_adapter()),
        sample_rate: rate,
    }
}

#[cfg(test)]
mod tests {
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
        let mut deferred = None;
        let mut structural_applied = 0usize;

        // Push None to bypass / clear cabsim
        prod.push(None).unwrap();

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
        assert!(parking_lot_dirty.load(Ordering::Acquire));
        // The retired pair reaches GC as a single moved Box (F-RB-007).
        let old = gc_c.pop().unwrap();
        assert_matches!(old, GcItem::CabSimPair(_));
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
        let mut deferred = None;
        let mut structural_applied = 0usize;

        // Continuous IR replacement: two successive pairs.
        prod.push(Some(Box::new(make_pair(&ir, 64, 96000))))
            .unwrap();
        prod.push(Some(Box::new(make_pair(&ir, 128, 96000))))
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
        assert!(parking_lot_dirty.load(Ordering::Acquire));
        // Both retired pairs reach GC as single moved boxes.
        assert_matches!(gc_c.pop().unwrap(), GcItem::CabSimPair(_));
        assert_matches!(gc_c.pop().unwrap(), GcItem::CabSimPair(_));
        assert!(gc_c.pop().is_err());
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

        // Backlog: two successive pairs, then a `None` bypass as the newest
        // command (e.g. F-RB-006 rebuild-failure rollback).
        prod.push(Some(Box::new(make_pair(&ir, 64, 96000))))
            .unwrap();
        prod.push(Some(Box::new(make_pair(&ir, 128, 96000))))
            .unwrap();
        prod.push(None).unwrap();

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

        // GC: 2 superseded pairs + the replaced active pair = 3 moved Boxes.
        let mut pairs = 0usize;
        while let Ok(item) = gc_c.pop() {
            assert_matches!(item, GcItem::CabSimPair(_));
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

        prod.push(Some(Box::new(make_pair(&ir, 128, 96000))))
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
}

#[cfg(all(test, feature = "heap-audit"))]
mod heap_audit_tests {
    use super::*;
    use neural_amp_modeler_rs::common::alloc_audit::{TrackingGuard, get_alloc_count};

    const IR: [f32; 16] = [
        1.0, 0.5, 0.25, 0.125, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    ];

    /// Channel/state harness for an audited RT drain cycle. All pairs and
    /// channel contents are prepared *before* the `TrackingGuard` starts.
    struct DrainHarness {
        prod: rtrb::Producer<Option<Box<CabSimPair>>>,
        cons: Consumer<Option<Box<CabSimPair>>>,
        active: Option<Box<CabSimPair>>,
        deferred: Option<Option<Box<CabSimPair>>>,
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
            self.prod.push(item).unwrap();
        }

        fn push_pair(&mut self, partition: usize, rate: u32) {
            self.push(Some(Box::new(make_pair(&IR, partition, rate))));
        }

        /// Runs one RT drain cycle under the allocation watchdog, asserts zero
        /// heap traffic, then reclaims retired pairs through the off-RT GC.
        fn run_audited_drain(&mut self, label: &str) {
            let count = {
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
                get_alloc_count()
            };
            assert_eq!(
                count, 0,
                "heap allocations detected during {label}: count={count}"
            );
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
        assert!(!h.parking_lot_dirty.load(Ordering::Acquire));
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

        // Pre-fill the overflow ring with old pairs, then drain it once so the
        // ring is empty but the audit forces tier 3 after SPSC+lot fill.
        for i in 0..4 {
            h.gc_overflow.push(GcItem::CabSimPair(Box::new(make_pair(
                &IR,
                64 + i * 64,
                48000,
            ))));
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
