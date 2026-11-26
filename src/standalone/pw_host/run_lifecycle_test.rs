// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::models::lstm::Lstm1x3;

#[test]
fn interrupted_backoff_preserves_the_pending_backend_failure() {
    let (stream, reason) =
        interrupted_backoff_failure("capture", "core lost during reconnect".to_owned());
    assert_eq!(stream, "capture");
    assert_eq!(reason, "core lost during reconnect");
}

#[test]
fn control_loop_gc_drain_reuses_the_caller_owned_parking_lot() {
    let (mut producer, mut consumer) = rtrb::RingBuffer::<GcItem>::new(8);
    let overflow = GcOverflowBuffer::new(4);
    let flags = RtStatusFlags::new();
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let lot_addr = std::ptr::addr_of!(parking_lot);

    producer
        .push(GcItem::Model(Box::new(
            neural_amp_modeler_rs::models::StaticModel::Lstm1x3(Box::new(Lstm1x3::new())),
        )))
        .expect("GC ring accepts one item");

    assert_eq!(
        drain_control_gc(&mut consumer, &overflow, &flags, &mut parking_lot),
        1
    );
    assert_eq!(
        std::ptr::addr_of!(parking_lot),
        lot_addr,
        "the control loop must not re-instantiate its parking lot"
    );
    assert_eq!(
        drain_control_gc(&mut consumer, &overflow, &flags, &mut parking_lot),
        0
    );
    assert_eq!(std::ptr::addr_of!(parking_lot), lot_addr);
}
