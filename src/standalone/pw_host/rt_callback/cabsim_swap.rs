// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Cab-sim Convolution Engine Draining (Zero-Alloc Swap)
//! Replaces the active convolution engine without using memory allocation in the critical path.

use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags};
use neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter;

use rtrb::Consumer;
use std::sync::atomic::{AtomicBool, Ordering};

/// Drains the cab-sim convolution adapter SPSC channel and swaps the active adapter atomically.
///
/// Follows the same cascade pattern as `drain_resamplers`:
/// GC channel → parking_lot → overflow buffer.
///
/// An `Option` is used so that `None` can be sent to clear/bypass the convolution.
#[inline(always)]
pub fn drain_cabsims(
    cabsim_consumer: &mut Consumer<Option<CabSimAdapter>>,
    active_cabsim: &mut Option<CabSimAdapter>,
    gc_producer: &mut rtrb::Producer<GcItem>,
    parking_lot: &mut [Option<GcItem>; 16],
    parking_lot_dirty: &AtomicBool,
    gc_overflow_for_process: &GcOverflowBuffer,
    rt_status_for_process: &RtStatusFlags,
) {
    while let Ok(new_adapter) = cabsim_consumer.pop() {
        let old_adapter = std::mem::replace(active_cabsim, new_adapter);

        if let Some(old) = old_adapter {
            parking_lot_dirty.store(true, Ordering::Release);
            neural_amp_modeler_rs::common::spsc::gc_cascade(
                Some(GcItem::CabConvAdapter(Box::new(old))),
                gc_producer,
                parking_lot,
                gc_overflow_for_process,
                rt_status_for_process,
            );
        }
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

        drain_cabsims(
            &mut cons,
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
        let engine = neural_amp_modeler_rs::dsp::cabsim::conv::ConvEngine::new(&ir, 64).unwrap();
        let adapter = CabSimAdapter::new(Box::new(engine)).unwrap();
        let mut active = Some(adapter);
        let (mut gc_p, mut gc_c) = rtrb::RingBuffer::new(4);
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let gc_overflow = GcOverflowBuffer::default();
        let flags = RtStatusFlags::new();

        // Push None to bypass / clear cabsim
        prod.push(None).unwrap();

        drain_cabsims(
            &mut cons,
            &mut active,
            &mut gc_p,
            &mut parking_lot,
            &parking_lot_dirty,
            &gc_overflow,
            &flags,
        );

        assert!(active.is_none());
        assert!(parking_lot_dirty.load(Ordering::Acquire));
        let old = gc_c.pop().unwrap();
        assert_matches!(old, GcItem::CabConvAdapter(_));
    }
}
