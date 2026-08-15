// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;

fn make_rs(pw: u32, nam: u32) -> Box<NamResampler> {
    Box::new(NamResampler::new(pw, nam, 64).unwrap())
}

fn request_rebuild(flags: &RtStatusFlags, host: u32, nam: u32) {
    flags.requested_host_rate.store(host, Ordering::Relaxed);
    flags.requested_nam_rate.store(nam, Ordering::Relaxed);
    flags.set_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
    flags.set_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING);
}

#[test]
fn resampler_push_full_keeps_needs_for_retry() {
    let flags = RtStatusFlags::new();
    request_rebuild(&flags, 44100, 48000);
    let sys = SystemSnapshot::capture();

    // Saturate the resampler channel (capacity 1) so the delivery fails.
    let (mut prod, mut cons) = rtrb::RingBuffer::new(1);
    prod.push(make_rs(48000, 48000)).unwrap();

    handle_resampler_rebuild(&flags, &sys, &mut prod);

    // Fail-closed: NEEDS stays set (retry scheduled) and REBUILD_FAILED is
    // NOT set — the RT must remain muted awaiting the in-flight replacement.
    assert!(flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(!flags.check_flag(spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED));
    assert!(flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));

    // Free the channel and retry: delivery now succeeds.
    let _ = cons.pop().unwrap();
    handle_resampler_rebuild(&flags, &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(!flags.check_flag(spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED));
    // PENDING is cleared only when the RT drains the new resampler.
    assert!(flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));

    let new_rs = cons.pop().unwrap();
    assert_eq!(new_rs.host_rate(), 44100);
    assert_eq!(new_rs.nam_rate(), 48000);
}

fn request_cabsim_rebuild(flags: &RtStatusFlags, partition: u32) {
    flags
        .requested_cabsim_partition_size
        .store(partition, Ordering::Relaxed);
    flags.set_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
}

#[test]
fn cabsim_push_full_keeps_needs_for_retry() {
    let flags = RtStatusFlags::new();
    request_cabsim_rebuild(&flags, 64);
    let sys = SystemSnapshot::capture();
    let ir = [1.0f32, 0.0, 0.0, 0.0];

    let (mut prod, mut cons) = rtrb::RingBuffer::new(1);
    let placeholder = ConvEngine::new(&ir, 64)
        .ok()
        .and_then(|engine| CabSimAdapter::new(Box::new(engine)).ok())
        .expect("placeholder cabsim adapter");
    prod.push(Some(placeholder)).unwrap();

    handle_cabsim_rebuild(&flags, Some(&ir), &sys, &mut prod);

    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD),
        "Full must keep NEEDS_CABSIM_REBUILD for retry"
    );

    let _ = cons.pop().unwrap();
    handle_cabsim_rebuild(&flags, Some(&ir), &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    assert!(cons.pop().is_ok());
}

#[test]
fn resampler_push_success_clears_needs() {
    let flags = RtStatusFlags::new();
    request_rebuild(&flags, 44100, 48000);
    let sys = SystemSnapshot::capture();

    let (mut prod, mut cons) = rtrb::RingBuffer::new(2);

    handle_resampler_rebuild(&flags, &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));
    assert!(!flags.check_flag(spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED));
    assert!(flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));

    let new_rs = cons.pop().unwrap();
    assert_eq!(new_rs.host_rate(), 44100);
}
