// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.1. Resampler Draining (Zero-Alloc Swap)
//! Replaces resamplers without using memory allocation in the critical path.

use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags, gc_cascade};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// 5.1.1. Resampler Draining (Zero-Alloc Swap)
/// Replaces resamplers without using memory allocation in the critical path.
#[inline(always)]
pub fn drain_resamplers(
    resampler_consumer: &mut Consumer<Box<NamResampler>>,
    resampler: &mut Box<NamResampler>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    while let Ok(new_rs) = resampler_consumer.pop() {
        rt_status_for_process
            .active_rate
            .store(new_rs.host_rate(), Ordering::Relaxed);
        rt_status_for_process
            .active_rate_changed
            .store(new_rs.host_rate(), Ordering::Relaxed);

        let old_rs = std::mem::replace(resampler, new_rs);

        rt_status_for_process
            .clear_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);

        parking_lot_dirty.store(true, Ordering::Release);
        gc_cascade(
            Some(GcItem::Resampler(old_rs)),
            gc_producer,
            parking_lot,
            gc_overflow_for_process,
            rt_status_for_process,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn make_rs(pw: u32, nam: u32) -> Box<NamResampler> {
        Box::new(NamResampler::new(pw, nam, 64).unwrap())
    }

    #[test]
    fn empty_consumer_no_change() {
        let (_prod, mut cons) = rtrb::RingBuffer::new(4);
        let mut active = make_rs(48000, 48000);
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        drain_resamplers(
            &mut cons,
            &mut active,
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
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        prod.push(make_rs(44100, 48000)).unwrap();

        drain_resamplers(
            &mut cons,
            &mut active,
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
        assert!(!flags.check_flag(RT_STATUS_RESAMP_SWAP_PENDING));
        assert!(parking_lot_dirty.load(Ordering::Acquire));

        let old = gc_c.pop().unwrap();
        assert!(matches!(old, GcItem::Resampler(_)));
    }

    #[test]
    fn multiple_swaps_keep_last() {
        let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
        let mut active = make_rs(48000, 48000);
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        prod.push(make_rs(44100, 48000)).unwrap();
        prod.push(make_rs(96000, 48000)).unwrap();

        drain_resamplers(
            &mut cons,
            &mut active,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert_eq!(active.host_rate(), 96000);
        assert_eq!(flags.active_rate.load(Ordering::Relaxed), 96000);
        assert!(parking_lot_dirty.load(Ordering::Acquire));

        let _first = gc_c.pop().unwrap();
        let _second = gc_c.pop().unwrap();
        assert!(gc_c.pop().is_err());
    }

    #[test]
    fn swap_cascades_old_resampler_to_gc() {
        let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
        let mut active = make_rs(48000, 48000);
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        prod.push(make_rs(44100, 48000)).unwrap();

        drain_resamplers(
            &mut cons,
            &mut active,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        let gc_item = gc_c.pop().unwrap();
        match gc_item {
            GcItem::Resampler(old_rs) => {
                assert_eq!(old_rs.host_rate(), 48000);
            }
            _ => panic!("Expected GcItem::Resampler"),
        }
    }
}
