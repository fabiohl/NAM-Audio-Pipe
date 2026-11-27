// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Reduced aliasing harness for the `CaptureState`/`RtHostChannels` ownership
//! pattern used by `run_pipewire_host`.
//!
//! The host owns its RT state in heap `Box`es and hands raw pointers to the
//! PipeWire stream callbacks. To stay sound across bounded reconnects, every
//! raw pointer (`state_ptr`, `channels_ptr`, `rec_ptr`, `parking_lot_ptr`,
//! `parking_lot_dirty_ptr`) is re-derived **inside** each `'host` iteration:
//! only after the owner's pre-stream writes, and only once the previous
//! stream/loop were stopped and destroyed (so the teardown's `&mut` over the
//! parking lot has exclusive access again). No raw pointer survives a
//! teardown. These tests are positive proofs of that pattern so Miri can audit
//! the pointer lifecycle in seconds without PipeWire.
//!
//! Historical note: the pre-T9.1 code derived the pointers once, before the
//! `'host` loop, and reused them after `reset_bridge_and_drain_gc(...,
//! &mut rt_parking_lot, ...)`. A reduced reproduction of that sequence is
//! rejected by Miri under Stacked Borrows (the `&mut` retag invalidates the
//! earlier `&raw mut` tag before the next `&mut *parking_lot_ptr`).

struct Inner {
    counter: u32,
    flag: bool,
}

#[test]
fn per_instance_derivation_is_used_within_its_owner_window() {
    let mut owner = Box::new(Inner {
        counter: 0,
        flag: false,
    });

    // Instance 1: owner writes complete, then derive and use within the window.
    owner.flag = true;
    let instance_ptr: *mut Inner = &raw mut *owner;
    // SAFETY: `owner` is live and unaliased for the whole block; the pointer is
    // derived from it here and not used after the borrow window closes.
    unsafe {
        let state = &mut *instance_ptr;
        state.counter = 7;
        assert_eq!(state.counter, 7);
    }

    // Instance 2: a fresh derivation after the owner regains exclusivity. The
    // pointer from instance 1 is never touched again.
    owner.flag = false;
    // SAFETY: fresh derivation from the live `owner`; no stale pointer is reused.
    let state_ptr: *mut Inner = &raw mut *owner;
    // SAFETY: same live allocation, derived above after the owner's write.
    unsafe {
        (*state_ptr).counter = 11;
        assert_eq!((*state_ptr).counter, 11);
    }
}

fn reset_lot(lot: &mut [Option<u32>; 4]) {
    lot[0] = None;
}

#[test]
fn teardown_exclusive_borrow_then_fresh_derivation() {
    let mut lot: [Option<u32>; 4] = [Some(1), Some(2), Some(3), Some(4)];

    // Instance 1: the RT side receives the parking lot via a per-instance raw
    // pointer and uses it through `&mut *ptr`.
    {
        let lot_ptr: *mut [Option<u32>; 4] = &raw mut lot;
        // SAFETY: `lot` is live and the instance-1 borrow window is exclusive.
        unsafe {
            let parking_lot = &mut *lot_ptr;
            parking_lot[1] = Some(9);
        }
    }

    // Teardown: the main thread re-takes exclusive ownership with `&mut lot`
    // (as `reset_bridge_and_drain_gc` does). This would invalidate any pointer
    // retained from instance 1 under Stacked Borrows.
    reset_lot(&mut lot);
    assert_eq!(lot[0], None);

    // Reconnection: derive a brand-new pointer from the now-exclusive owner
    // instead of reusing the instance-1 pointer.
    let fresh_ptr: *mut [Option<u32>; 4] = &raw mut lot;
    // SAFETY: fresh derivation from the live `lot` after teardown completed;
    // no stale pointer from the previous instance is used.
    unsafe {
        let parking_lot = &mut *fresh_ptr;
        parking_lot[2] = Some(21);
        assert_eq!(parking_lot[2], Some(21));
    }
}

#[test]
fn disjoint_field_raw_write_keeps_owner_allocation_valid() {
    let mut owner = Box::new(Inner {
        counter: 0,
        flag: false,
    });
    let state_ptr: *mut Inner = &raw mut *owner;

    owner.flag = true;

    // SAFETY: `state_ptr` stays derived from the live `owner` allocation; the
    // write touches a field disjoint from the owner write above.
    unsafe {
        (*state_ptr).counter = 7;
        assert_eq!((*state_ptr).counter, 7);
    }
    assert!(owner.flag);
}

#[test]
fn playback_trigger_slot_per_instance_lifecycle() {
    use std::ptr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicPtr, Ordering};

    // Mirrors `PlaybackTriggerSlot` across two `'host` reconnect instances:
    // arm -> Acquire read in RT trigger -> disarm after loop stop -> drop owner ->
    // fresh instance with new slot/owner.
    for instance in 1..=2u32 {
        let slot = Arc::new(AtomicPtr::new(ptr::null_mut()));
        let mut stream_owner = Box::new(Inner {
            counter: instance,
            flag: true,
        });

        let raw: *mut Inner = &raw mut *stream_owner;
        slot.store(raw, Ordering::Release);

        // Simulated RT capture callback reading the armed pointer.
        let rt_slot = Arc::clone(&slot);
        let observed = std::thread::spawn(move || {
            let p = rt_slot.load(Ordering::Acquire);
            if p.is_null() {
                return None;
            }
            // SAFETY: `stream_owner` stays live and unaliased for reads until
            // `disarm()` completes after the thread joins (`thread_loop.stop()`).
            unsafe { Some((*p).counter) }
        })
        .join()
        .expect("RT trigger simulation thread panicked");
        assert_eq!(observed, Some(instance));

        // Teardown order: stop loop (join above), disarm, then drop owner.
        slot.store(ptr::null_mut(), Ordering::Release);
        assert!(slot.load(Ordering::Acquire).is_null());
        drop(stream_owner);
    }
}
