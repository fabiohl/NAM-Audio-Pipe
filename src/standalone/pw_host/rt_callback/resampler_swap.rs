// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.1. Resampler Draining (Zero-Alloc Swap)
//! Replaces resamplers without using memory allocation in the critical path.
//!
//! Budgeting (F-RB-011 / T2.5): at most [`STRUCTURAL_SWAPS_PER_CALLBACK`]
//! structural swap applies per callback (budget shared across every RT swap
//! drain); current-generation envelopes in the coalescing window collapse to
//! the latest one (intermediate envelopes discarded to GC) and the excess is
//! parked in the deferred slot for the next callback.

use super::commands::{STRUCTURAL_POPS_PER_CALLBACK, STRUCTURAL_SWAPS_PER_CALLBACK};
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, RT_STATUS_STRUCTURAL_DEFERRED, RT_STATUS_STRUCTURAL_SUPERSEDED,
    ResamplerSwapPayload, RtStatusFlags, gc_cascade,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer;

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// 5.1.1. Resampler Draining (Zero-Alloc Swap)
/// Replaces resamplers without using memory allocation in the critical path.
///
/// Versioned delivery (F-RB-004): each envelope carries the request generation
/// it was built for. An envelope whose generation still equals
/// `requested_rate_generation` is current and is installed (the previous
/// resampler cascades to GC). An envelope whose generation is older has been
/// superseded by a newer host renegotiation — it goes straight to the GC
/// cascade **without** unmuting and **without** clearing
/// `RT_STATUS_RESAMP_SWAP_PENDING`, so the callback keeps waiting for the build
/// that matches the most recent request.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time callback signature with SPSC queues, parking lot, and dirty flag"
)]
pub fn drain_resamplers(
    resampler_consumer: &mut Consumer<Box<ResamplerSwapPayload>>,
    deferred: &mut Option<Box<ResamplerSwapPayload>>,
    structural_applied: &mut usize,
    resampler: &mut Box<NamResampler>,
    stream: &mut Box<StreamingResampleBuffer>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    // Phase 0 — resolve a resampler deferred by the previous callback.
    if let Some(pending) = deferred.take() {
        let current_req_gen = rt_status_for_process
            .requested_rate_generation
            .load(Ordering::Acquire);
        let head_is_current = resampler_consumer
            .peek()
            .is_ok_and(|head| head.generation == current_req_gen);
        if pending.generation != current_req_gen {
            // Stale while parked (F-RB-004): discard to GC without unmuting and
            // without clearing RESAMP_SWAP_PENDING.
            discard_resampler(
                pending,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
        } else if head_is_current {
            // A newer same-generation build is already queued (latest-wins).
            discard_resampler(
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
            install_resampler(
                pending,
                resampler,
                stream,
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
    let current_req_gen = rt_status_for_process
        .requested_rate_generation
        .load(Ordering::Acquire);
    let mut candidate: Option<Box<ResamplerSwapPayload>> = None;
    let mut pops = 0usize;
    while pops < STRUCTURAL_POPS_PER_CALLBACK {
        let Some(payload) = resampler_consumer.pop().ok() else {
            break;
        };
        pops += 1;
        if payload.generation != current_req_gen {
            // Stale envelope (F-RB-004): the host renegotiated the clock while
            // this resampler was being built. Discard it for GC without
            // unmuting and without clearing RESAMP_SWAP_PENDING.
            discard_resampler(
                payload,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            continue;
        }
        if let Some(older) = candidate.replace(payload) {
            // Coalescing: an intermediate current-generation envelope is
            // obsolete — its resampler cascades to GC (latest-wins).
            discard_resampler(
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
    if let Some(payload) = candidate {
        if *structural_applied < STRUCTURAL_SWAPS_PER_CALLBACK {
            install_resampler(
                payload,
                resampler,
                stream,
                gc_producer,
                parking_lot,
                parking_lot_dirty,
                gc_overflow_for_process,
                rt_status_for_process,
            );
            *structural_applied += 1;
        } else if deferred.is_none() {
            *deferred = Some(payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // The deferred slot holds an older envelope; the popped one is
            // newer — supersede the parked envelope and park this one
            // (latest-wins).
            let older = deferred.take().expect("slot occupied, checked above");
            discard_resampler(
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
            *deferred = Some(payload);
            rt_status_for_process.set_flag(RT_STATUS_STRUCTURAL_DEFERRED);
            rt_status_for_process
                .structural_deferred_total
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Installs a current-generation resampler envelope: swaps the active
/// resampler and streaming adapter, records the applied generation and active rates,
/// unmutes (clears `RT_STATUS_RESAMP_SWAP_PENDING`) and cascades the retired
/// resampler envelope to GC.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Real-time swap helper receiving active resources, queues, parking lot, and flags"
)]
fn install_resampler(
    mut payload: Box<ResamplerSwapPayload>,
    resampler: &mut Box<NamResampler>,
    stream: &mut Box<StreamingResampleBuffer>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    std::mem::swap(&mut payload.resampler, resampler);
    std::mem::swap(&mut payload.stream, stream);

    rt_status_for_process
        .applied_rate_generation
        .store(payload.generation, Ordering::Release);
    rt_status_for_process
        .active_rate
        .store(resampler.host_rate(), Ordering::Relaxed);
    rt_status_for_process
        .active_rate_changed
        .store(resampler.host_rate(), Ordering::Relaxed);

    rt_status_for_process
        .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);

    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::ResamplerSwap(payload)),
        gc_producer,
        parking_lot,
        gc_overflow_for_process,
        rt_status_for_process,
    );
}

/// Discards a resampler envelope to the GC cascade **without** unmuting and
/// **without** clearing `RT_STATUS_RESAMP_SWAP_PENDING` (stale or superseded
/// builds never substitute the most recent request).
#[inline(always)]
fn discard_resampler(
    payload: Box<ResamplerSwapPayload>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    parking_lot_dirty.store(true, Ordering::Release);
    gc_cascade(
        Some(GcItem::ResamplerSwap(payload)),
        gc_producer,
        parking_lot,
        gc_overflow_for_process,
        rt_status_for_process,
    );
}

#[cfg(test)]
mod tests {
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

    /// Deterministic lost-wakeup interleaving (F-RB-004 / T2.1 acceptance):
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

    // ── T2.5 Structural Budget & Coalescing (F-RB-011) ──────────────────────

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
}
