// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, SlimModelPair};
use neural_amp_modeler_rs::loader;
use neural_amp_modeler_rs::math::common::AlignedVec;
use neural_amp_modeler_rs::models::wavenet::WaveNetModelDyn;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

/// Serializes the resampler fault-injection tests: the armed fault is a single
/// global inside `handlers.rs`, so tests that arm it must not run concurrently.
#[cfg(feature = "testing")]
static RESAMPLER_FAULT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes the oversample fault-injection tests (same global-arm constraint
/// as the resampler hook).
#[cfg(feature = "testing")]
static OS_FAULT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes the slimmable fault-injection tests (same global-arm constraint
/// as the resampler hook).
#[cfg(feature = "testing")]
static SLIMMABLE_FAULT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Minimal structurally-invalid-but-drop-safe `WavenetDyn` used to occupy a
/// slimmable channel slot (the drain never runs DSP on these).
fn fake_wavenet(ch: usize) -> Box<StaticModel> {
    Box::new(StaticModel::WavenetDyn(Box::new(WaveNetModelDyn {
        ch,
        k: 3,
        head: 1,
        arrays: vec![],
        head_scale: 1.0,
        receptive_field_size: 1,
        condition_dsp: None,
        condition_dsp_output: AlignedVec::new(1, 0.0f32).expect("alloc"),
        post_stack_head: None,
        head_output_scratch: AlignedVec::new(1, 0.0f32).expect("alloc"),
        prewarm_on_reset: false,
        slimmable_capable: true,
        allowed_channels: None,
        pending_slim_channel: None,
    })))
}

fn fake_pair(generation: u64, ch: usize) -> Box<SlimModelPair> {
    Box::new(SlimModelPair {
        generation,
        channels: ch,
        l: Some(fake_wavenet(ch)),
        r: Some(fake_wavenet(ch)),
    })
}

/// Loads the engine's free-topology WaveNet fixture (`WavenetDyn`, CH=7) and
/// returns a full-model storage clone suitable for `handle_slimmable_rebuild`.
fn load_slimmable_full_model() -> Option<Box<StaticModel>> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/models/wavenet_dyn_free.nam");
    let sys = SystemSnapshot::capture();
    let loaded = loader::load_and_build_model(&path, &sys, true, loader::LoadOptions::default())
        .expect("slimmable fixture must load");
    loaded.model_l.as_ref().and_then(|m| {
        if let StaticModel::WavenetDyn(w) = m.as_ref() {
            neural_amp_modeler_rs::models::slimmable::clone_wavenet_for_slimmable_storage(w).ok()
        } else {
            None
        }
    })
}

fn request_slimmable_rebuild(flags: &RtStatusFlags, ch: u32) {
    flags.requested_slimmable_ch.store(ch, Ordering::Relaxed);
    flags
        .requested_slimmable_generation
        .fetch_add(1, Ordering::Release);
    flags.set_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
}

/// Core slimmable rebuild (producer side): `handle_slimmable_rebuild` slices and
/// prewarms BOTH channels before a single atomic push of a `SlimModelPair`;
/// the envelope carries the captured generation and the requested channel count.
#[test]
fn slimmable_pair_built_and_pushed_atomically() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let full = load_slimmable_full_model().expect("fixture model");
    request_slimmable_rebuild(&flags, 4);

    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(2);
    let mut failures = RebuildFailureTracker::default();

    handle_slimmable_rebuild(
        &flags,
        Some(full.as_ref()),
        Some(full.as_ref()),
        true,
        &sys,
        &mut prod,
        &mut failures,
    );

    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "successful atomic push must clear the request"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED));

    let pair = cons.pop().expect("one pair must be delivered");
    assert_eq!(pair.generation, 1, "pair must carry the request generation");
    assert_eq!(pair.channels, 4);
    assert_eq!(pair.l.as_ref().unwrap().channels(), 4);
    assert!(
        pair.r.is_some(),
        "stereo config must build and deliver an R model"
    );
    assert_eq!(pair.r.as_ref().unwrap().channels(), 4);
    assert!(cons.pop().is_err(), "both channels delivered in ONE push");
}

/// Mono config (`has_model_r == false`) must slice L only and deliver `r: None`.
#[test]
fn slimmable_mono_pair_has_no_r() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let full = load_slimmable_full_model().expect("fixture model");
    request_slimmable_rebuild(&flags, 4);

    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(2);
    let mut failures = RebuildFailureTracker::default();

    handle_slimmable_rebuild(
        &flags,
        Some(full.as_ref()),
        None,
        false,
        &sys,
        &mut prod,
        &mut failures,
    );

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));
    let pair = cons.pop().expect("one pair must be delivered");
    assert_eq!(pair.l.as_ref().unwrap().channels(), 4);
    assert!(pair.r.is_none(), "mono config must not build an R model");
}

/// Fail-closed: when the SPSC channel is full, NEITHER channel is delivered
/// and `NEEDS_SLIMMABLE_REBUILD` stays armed for a full retry — a partial
/// L-without-R delivery is impossible.
#[test]
fn slimmable_push_full_keeps_needs_for_retry() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let full = load_slimmable_full_model().expect("fixture model");
    request_slimmable_rebuild(&flags, 4);

    // Saturate the single-slot channel so the handler's push must fail.
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(1);
    prod.push(fake_pair(0, 8)).unwrap();
    let mut failures = RebuildFailureTracker::default();

    handle_slimmable_rebuild(
        &flags,
        Some(full.as_ref()),
        Some(full.as_ref()),
        true,
        &sys,
        &mut prod,
        &mut failures,
    );

    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "full channel must keep the request armed for a full retry"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED));

    // Free the channel and retry: the whole pair is now delivered.
    let _ = cons.pop().unwrap();
    handle_slimmable_rebuild(
        &flags,
        Some(full.as_ref()),
        Some(full.as_ref()),
        true,
        &sys,
        &mut prod,
        &mut failures,
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));
    let pair = cons.pop().expect("retry must deliver the pair");
    assert_eq!(pair.channels, 4);
    assert!(pair.r.is_some());
    assert!(cons.pop().is_err());
}

/// Lost-wakeup guard: `rearm_slimmable_if_superseded` keeps the request armed
/// exactly when a newer slimmable generation was published while the pair was
/// being built.
#[test]
fn slimmable_rearm_guard_keeps_request_when_generation_advanced() {
    let flags = RtStatusFlags::new();

    // Generation did not advance during the build → request is cleared.
    flags.set_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
    rearm_slimmable_if_superseded(&flags, 0);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));

    // A newer request was published while building generation 1 → re-armed.
    flags.set_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
    flags
        .requested_slimmable_generation
        .store(2, Ordering::Release);
    rearm_slimmable_if_superseded(&flags, 1);
    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "a newer request published during the build must keep NEEDS armed"
    );
}

/// End-to-end protocol with a real sliceable model: request A (gen 1) → build
/// and push pair A → request B (gen 2) → build and push pair B → RT drain
/// discards the stale A pair whole to GC and applies B, so the active L/R
/// always belong to the same generation and channel count. (The fixture
/// `wavenet_dyn_free.nam` is CH=7/CH=4, so only target_ch=4 is sliceable; the
/// generation — not the channel count — distinguishes the stale pair.)
#[test]
fn slimmable_full_protocol_discards_stale_applies_latest() {
    use crate::standalone::pw_host::rt_callback::drain_slimmable_models;

    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let full = load_slimmable_full_model().expect("fixture model");

    let (mut sl_prod, sl_cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(4);
    let mut sl_rx = Some(sl_cons);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::<GcItem>::new(8);
    let gc_overflow = GcOverflowBuffer::default();
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let mut model_l: Option<Box<StaticModel>> = Some(fake_wavenet(7));
    let mut model_r: Option<Box<StaticModel>> = Some(fake_wavenet(7));
    let mut deferred_slimmable = None;
    let mut structural_applied = 0usize;
    let mut failures = RebuildFailureTracker::default();

    // RT requests A (gen 1, ch 4); main builds and pushes pair A.
    request_slimmable_rebuild(&flags, 4);
    handle_slimmable_rebuild(
        &flags,
        Some(full.as_ref()),
        Some(full.as_ref()),
        true,
        &sys,
        &mut sl_prod,
        &mut failures,
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));

    // RT renegotiates to B (gen 2, ch 4) while A is still in the channel.
    request_slimmable_rebuild(&flags, 4);

    // RT drain: A (gen 1 < 2) is stale → discarded whole to GC, not installed.
    drain_slimmable_models(
        &mut sl_rx,
        &mut deferred_slimmable,
        &mut structural_applied,
        &mut model_l,
        &mut model_r,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );
    assert_eq!(
        model_l.as_ref().unwrap().channels(),
        7,
        "stale pair not applied"
    );
    assert_eq!(model_r.as_ref().unwrap().channels(), 7);

    // Main delivers B for generation 2 and clears the request.
    handle_slimmable_rebuild(
        &flags,
        Some(full.as_ref()),
        Some(full.as_ref()),
        true,
        &sys,
        &mut sl_prod,
        &mut failures,
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));

    // RT drain: B matches the current generation → applied atomically.
    drain_slimmable_models(
        &mut sl_rx,
        &mut deferred_slimmable,
        &mut structural_applied,
        &mut model_l,
        &mut model_r,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );
    assert_eq!(model_l.as_ref().unwrap().channels(), 4);
    assert_eq!(
        model_r.as_ref().unwrap().channels(),
        4,
        "L and R must always belong to the same pair"
    );

    // The stale pair A (both channels, ch 4) went to GC whole.
    let stale = gc_c.pop().unwrap();
    match stale {
        GcItem::SlimModelPair(p) => {
            assert_eq!(p.channels, 4);
            assert_eq!(p.l.as_ref().unwrap().channels(), 4);
            assert_eq!(p.r.as_ref().unwrap().channels(), 4);
        }
        _ => panic!("expected stale GcItem::SlimModelPair"),
    }
}

fn make_rs(pw: u32, nam: u32) -> Box<NamResampler> {
    Box::new(NamResampler::new(pw, nam, 64).unwrap())
}

fn make_stream(
    pw: u32,
    nam: u32,
) -> Box<neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer> {
    Box::new(
        neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer::new(pw, nam, 2048)
            .unwrap(),
    )
}

fn make_payload(generation: u64, pw: u32, nam: u32) -> Box<ResamplerSwapPayload> {
    Box::new(ResamplerSwapPayload {
        generation,
        resampler: make_rs(pw, nam),
        stream: Box::new(
            neural_amp_modeler_rs::dsp::resampling::StreamingResampleBuffer::new(pw, nam, 2048)
                .unwrap(),
        ),
    })
}

fn request_rebuild(flags: &RtStatusFlags, host: u32, nam: u32) {
    flags.requested_host_rate.store(host, Ordering::Relaxed);
    flags.requested_nam_rate.store(nam, Ordering::Relaxed);
    flags
        .requested_rate_generation
        .fetch_add(1, Ordering::Release);
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
    prod.push(make_payload(0, 48000, 48000)).unwrap();

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
    assert_eq!(
        new_rs.generation, 1,
        "payload must carry the request generation"
    );
    assert_eq!(new_rs.resampler.host_rate(), 44100);
    assert_eq!(new_rs.resampler.nam_rate(), 48000);
}

fn request_cabsim_rebuild(flags: &RtStatusFlags, partition: u32, host_rate: u32) {
    flags
        .requested_cabsim_partition_size
        .store(partition, Ordering::Relaxed);
    flags
        .requested_cabsim_host_rate
        .store(host_rate, Ordering::Relaxed);
    flags
        .requested_cabsim_generation
        .fetch_add(1, Ordering::Release);
    flags.set_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
}

fn test_pair(ir: &[f32], partition: usize, rate: u32) -> CabSimPair {
    let make = || {
        let engine = ConvEngine::new(ir, partition).expect("placeholder engine");
        CabSimAdapter::new(Box::new(engine)).expect("placeholder adapter")
    };
    CabSimPair {
        l: Box::new(make()),
        r: Box::new(make()),
        sample_rate: rate,
    }
}

#[test]
fn cabsim_push_full_keeps_needs_for_retry() {
    let flags = RtStatusFlags::new();
    request_cabsim_rebuild(&flags, 64, 48000);
    let sys = SystemSnapshot::capture();
    let ir = [1.0f32, 0.0, 0.0, 0.0];

    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<CabSimSwapPayload>>::new(1);
    prod.push(Box::new(CabSimSwapPayload {
        generation: 0,
        pair: Some(Box::new(test_pair(&ir, 64, 48000))),
    }))
    .unwrap();

    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);

    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD),
        "Full must keep NEEDS_CABSIM_REBUILD for retry"
    );

    let _ = cons.pop().unwrap();
    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    assert!(cons.pop().is_ok());
}

#[test]
fn cabsim_rebuild_resamples_to_requested_host_rate() {
    let sys = SystemSnapshot::capture();
    let ir: Vec<f32> = (0..4096)
        .map(|i| {
            let t = i as f32 / 48000.0;
            (std::f32::consts::TAU * 500.0 * t).sin() * (-8.0 * t).exp()
        })
        .collect();

    let mut partition_counts = Vec::new();
    for target_rate in [44100u32, 48000, 96000] {
        let flags = RtStatusFlags::new();
        request_cabsim_rebuild(&flags, 64, target_rate);
        let (mut prod, mut cons) = rtrb::RingBuffer::<Box<CabSimSwapPayload>>::new(2);

        handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);

        assert!(
            !flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD),
            "successful delivery must clear the request ({target_rate} Hz)"
        );
        let payload = cons
            .pop()
            .unwrap_or_else(|_| panic!("pair must be delivered ({target_rate} Hz)"));
        let pair = payload
            .pair
            .expect("successful rebuild delivers a pair, not bypass");
        assert_eq!(
            pair.sample_rate, target_rate,
            "pair must be stamped with the applied host rate"
        );
        assert_eq!(pair.partition_size(), 64);
        assert_eq!(pair.l.partition_size(), pair.r.partition_size());
        partition_counts.push(pair.l.num_partitions());
    }

    let [p44, p48, p96] = *partition_counts.as_slice() else {
        panic!("three rates must be measured");
    };
    assert_eq!(
        p48, 64,
        "same-rate rebuild must keep the 4096-sample IR untouched (4096/64)"
    );
    assert!(
        p44 > p48 && p96 > p44,
        "resampled IR duration in samples must grow with the target rate (44.1k={p44}, 48k={p48}, 96k={p96})"
    );
}

#[test]
fn cabsim_rebuild_same_rate_no_resample() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let ir: Vec<f32> = (0..256).map(|i| (-i as f32 / 64.0).exp()).collect();
    request_cabsim_rebuild(&flags, 64, 48000);
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<CabSimSwapPayload>>::new(2);

    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    let pair = cons.pop().unwrap().pair.expect("pair delivered");
    assert_eq!(pair.sample_rate, 48000);
    assert_eq!(pair.l.num_partitions(), 4, "256 samples / 64 partition");
}

#[test]
fn cabsim_rebuild_clamps_out_of_domain_partition_size() {
    let sys = SystemSnapshot::capture();
    let ir: Vec<f32> = (0..256).map(|i| (-i as f32 / 64.0).exp()).collect();

    // Above the ceiling: 16384 -> clamped to MAX_RESAMP_BUF.
    let flags = RtStatusFlags::new();
    request_cabsim_rebuild(&flags, 16384, 48000);
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<CabSimSwapPayload>>::new(2);
    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    let pair = cons.pop().unwrap().pair.expect("pair delivered");
    assert_eq!(
        pair.partition_size(),
        MAX_RESAMP_BUF,
        "oversized partition must be clamped to the ceiling"
    );
    assert_eq!(pair.l.num_partitions(), 1, "256-sample IR / 8192 partition");

    // Below the floor: 1 -> clamped to the 16-sample minimum.
    let flags = RtStatusFlags::new();
    request_cabsim_rebuild(&flags, 1, 48000);
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<CabSimSwapPayload>>::new(2);
    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    let pair = cons.pop().unwrap().pair.expect("pair delivered");
    assert_eq!(
        pair.partition_size(),
        16,
        "sub-minimum partition must be clamped"
    );

    // In-domain requests pass through untouched.
    let flags = RtStatusFlags::new();
    request_cabsim_rebuild(&flags, 64, 48000);
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<CabSimSwapPayload>>::new(2);
    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    let pair = cons.pop().unwrap().pair.expect("pair delivered");
    assert_eq!(pair.partition_size(), 64);
}

#[test]
fn cabsim_rebuild_failure_pushes_none_bypass() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let empty: Vec<f32> = Vec::new();
    request_cabsim_rebuild(&flags, 64, 96000);
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<CabSimSwapPayload>>::new(2);

    // Empty raw samples make the IR resample fail deterministically.
    handle_cabsim_rebuild(&flags, Some(&empty), 48000, &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    assert!(
        cons.pop().unwrap().pair.is_none(),
        "rebuild failure must deliver None (safe bypass)"
    );
}

#[test]
fn cabsim_rearm_guard_keeps_request_when_generation_advanced() {
    let flags = RtStatusFlags::new();

    // Generation did not advance during the build → request is cleared.
    flags.set_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
    rearm_cabsim_if_superseded(&flags, 0);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));

    // A newer request was published while building generation 1 → re-armed.
    flags.set_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD);
    flags
        .requested_cabsim_generation
        .store(2, Ordering::Release);
    rearm_cabsim_if_superseded(&flags, 1);
    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD),
        "a newer request published during the build must keep NEEDS armed"
    );
}

#[test]
fn cabsim_full_protocol_installs_latest_and_gcs_retired_pair() {
    use crate::standalone::pw_host::rt_callback::drain_cabsims;
    use neural_amp_modeler_rs::common::spsc::GcItem;
    use std::sync::atomic::AtomicBool;

    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let ir: Vec<f32> = (0..128).map(|i| (-i as f32 / 32.0).exp()).collect();

    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);
    let (mut gc_p, mut gc_c) = rtrb::RingBuffer::<GcItem>::new(8);
    let gc_overflow = GcOverflowBuffer::default();
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);

    let mut active = Some(Box::new(test_pair(&ir, 64, 48000)));
    let mut deferred_cabsim = None;
    let mut structural_applied = 0usize;

    request_cabsim_rebuild(&flags, 64, 96000);
    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);

    drain_cabsims(
        &mut cons,
        &mut deferred_cabsim,
        &mut structural_applied,
        &mut active,
        &mut gc_p,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    let installed = active.as_deref().expect("pair installed");
    assert_eq!(installed.sample_rate, 96000, "latest pair must be applied");
    assert!(parking_lot_dirty.load(Ordering::Acquire));
    let old = gc_c.pop().unwrap();
    assert!(
        matches!(old, GcItem::CabSimSwap(_)),
        "the retired pair must reach GC as a single moved Box"
    );
    assert!(gc_c.pop().is_err());
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
    assert_eq!(new_rs.generation, 1);
    assert_eq!(new_rs.resampler.host_rate(), 44100);
}

/// Lost-wakeup guard unit test: `rearm_rebuild_if_superseded` must keep
/// `NEEDS_RESAMPLER_REBUILD` armed exactly when a newer request generation was
/// published while the main thread was building.
#[test]
fn rearm_guard_keeps_request_when_generation_advanced() {
    let flags = RtStatusFlags::new();

    // Generation did not advance during the build → request is cleared.
    flags.set_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
    rearm_rebuild_if_superseded(&flags, 0);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));

    // A newer request was published while building generation 1 → re-armed.
    flags.set_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD);
    flags.requested_rate_generation.store(2, Ordering::Release);
    rearm_rebuild_if_superseded(&flags, 1);
    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD),
        "a newer request published during the build must keep NEEDS armed"
    );
}

/// A failed resampler rebuild for generation N must never erase a newer request
/// (N+1) published while the failure was being handled.
///
/// Deterministic reproduction of the race: the build for generation 1 fails
/// (fault-injected under `feature = "testing"`) and the handler is paused at
/// the injection point; the RT side then publishes generation 2. The fix
/// re-arms `NEEDS_RESAMPLER_REBUILD` because the generation advanced, and
/// generation 2 is later rebuilt and applied.
#[cfg(feature = "testing")]
#[test]
fn resampler_failure_preserves_newer_generation() {
    use crate::standalone::pw_host::rt_callback::drain_resamplers;
    use neural_amp_modeler_rs::common::spsc::GcItem;
    use std::sync::atomic::AtomicBool;

    let _serial = RESAMPLER_FAULT_TEST_LOCK.lock().unwrap();
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<ResamplerSwapPayload>>::new(4);

    // RT publishes request A (generation 1) and the main thread starts
    // rebuilding it; the fault is armed for generation 1.
    request_rebuild(&flags, 44100, 48000);
    let (reached_rx, release_tx) = super::resampler_fault::arm_fail_and_pause(&flags, 1);

    // Main-thread side: the handler captures generation 1, fails the build and
    // pauses at the injection point until the test releases it.
    let mut prod = std::thread::scope(|scope| {
        let handler = scope.spawn(|| {
            handle_resampler_rebuild(&flags, &sys, &mut prod);
            prod
        });

        // Wait for the handler to be paused after capturing generation 1.
        reached_rx
            .recv()
            .expect("handler must reach the fault pause");

        // RT publishes request B (generation 2) while the failed generation-1
        // rebuild is still being handled.
        request_rebuild(&flags, 96000, 48000);
        release_tx.send(()).expect("release must reach the handler");

        handler.join().expect("handler must finish")
    });

    // The newer generation survives the failed rebuild of the older one.
    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD),
        "a newer generation must survive a failed rebuild of an older one"
    );
    assert_eq!(
        flags.resampler_failed_generation.load(Ordering::Relaxed),
        1,
        "the failed generation must remain recorded for the RT fail-open guard"
    );
    assert!(flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));

    // The main thread processes the surviving request B (generation 2).
    handle_resampler_rebuild(&flags, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));

    // RT drain installs B (generation 2) and unmutes the callback.
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let (mut gc_prod, mut gc_cons) = rtrb::RingBuffer::<GcItem>::new(8);
    let gc_overflow = GcOverflowBuffer::default();
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let mut deferred = None;
    let mut structural_applied = 0usize;

    drain_resamplers(
        &mut cons,
        &mut deferred,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_prod,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(
        active.host_rate(),
        96000,
        "generation N+1 must be the one applied"
    );
    assert_eq!(
        flags.applied_rate_generation.load(Ordering::Relaxed),
        2,
        "applied generation must equal the requested generation before unmute"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));
    assert!(gc_cons.pop().is_ok(), "retired resampler must reach the GC");
}

/// Without a newer generation, a failed resampler rebuild clears the request:
/// no spurious retry is armed (preserves the existing correct behavior for the
/// simple case).
#[cfg(feature = "testing")]
#[test]
fn resampler_failure_without_newer_generation_still_clears() {
    let _serial = RESAMPLER_FAULT_TEST_LOCK.lock().unwrap();
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<ResamplerSwapPayload>>::new(4);

    // RT publishes request A (generation 1); the build for it is fault-injected
    // to fail deterministically.
    request_rebuild(&flags, 44100, 48000);
    super::resampler_fault::arm_fail_once(&flags, 1);

    handle_resampler_rebuild(&flags, &sys, &mut prod);

    // No newer generation: the request is cleared (no infinite retry loop)...
    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD),
        "a failure without a newer request must clear the rebuild flag"
    );
    // ...the failed generation stays recorded for the RT fail-open guard...
    assert_eq!(
        flags.resampler_failed_generation.load(Ordering::Relaxed),
        1,
        "the failed generation must remain recorded"
    );
    // ...and no payload is delivered (the RT stays muted until the fail-open
    // rollback resolves the failed generation).
    assert!(flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));
    assert!(
        cons.pop().is_err(),
        "failure path must not deliver a payload"
    );
}

/// Deterministic lost-wakeup interleaving:
///
/// 1. RT requests A (generation 1).
/// 2. Main builds and delivers A while RT renegotiates to B (generation 2).
/// 3. The stale delivery of A is discarded to GC **without** unmuting.
/// 4. The latest delivery of B is applied and the audio unmutes.
#[test]
fn superseded_delivery_is_discarded_latest_is_applied() {
    use crate::standalone::pw_host::rt_callback::drain_resamplers;
    use neural_amp_modeler_rs::common::spsc::GcItem;
    use std::sync::atomic::AtomicBool;

    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let (mut prod, mut cons) = rtrb::RingBuffer::new(4);

    // (1) RT requests A (gen 1); main delivers A (captured generation 1).
    request_rebuild(&flags, 44100, 48000);
    handle_resampler_rebuild(&flags, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));

    // (2) RT renegotiates to B (gen 2) while A is still in the channel; the
    // handler delivers the matching generation-2 build.
    request_rebuild(&flags, 96000, 48000);
    handle_resampler_rebuild(&flags, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));

    // (3)+(4) Drain: stale A (gen 1) is GC'd without unmute; B (gen 2) is
    // applied and the audio unmutes.
    let (mut gc_prod, mut gc_cons) = rtrb::RingBuffer::<GcItem>::new(8);
    let gc_overflow = GcOverflowBuffer::default();
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let mut deferred_resampler = None;
    let mut structural_applied = 0usize;

    drain_resamplers(
        &mut cons,
        &mut deferred_resampler,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_prod,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );

    assert_eq!(
        active.host_rate(),
        96000,
        "latest generation must be applied"
    );
    assert_eq!(
        flags.applied_rate_generation.load(Ordering::Relaxed),
        flags.requested_rate_generation.load(Ordering::Relaxed),
        "invariant: applied == requested before unmute"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));

    // GC received the stale A, then the original active resampler.
    let stale = gc_cons.pop().unwrap();
    match stale {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 44100),
        _ => panic!("Expected stale GcItem::ResamplerSwap(44100)"),
    }
    let previous = gc_cons.pop().unwrap();
    match previous {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 48000),
        _ => panic!("Expected previous GcItem::ResamplerSwap(48000)"),
    }
    assert!(gc_cons.pop().is_err());
}

/// Full-protocol simulation: request A → deliver A → request B → deliver B →
/// RT drain. The stale A must be discarded to GC without unmuting and B must
/// be applied with `applied_rate_generation == requested_rate_generation`.
#[test]
fn full_protocol_discards_stale_and_applies_latest() {
    use crate::standalone::pw_host::rt_callback::drain_resamplers;
    use neural_amp_modeler_rs::common::spsc::GcItem;
    use std::sync::atomic::AtomicBool;

    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let (mut res_prod, mut res_cons) = rtrb::RingBuffer::<Box<ResamplerSwapPayload>>::new(4);
    let (mut gc_prod, mut gc_cons) = rtrb::RingBuffer::<GcItem>::new(8);
    let gc_overflow = GcOverflowBuffer::default();
    let mut parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_dirty = AtomicBool::new(false);
    let mut active = make_rs(48000, 48000);
    let mut active_stream = make_stream(48000, 48000);
    let mut deferred_resampler = None;
    let mut structural_applied = 0usize;

    // RT requests A (gen 1).
    request_rebuild(&flags, 44100, 48000);
    // Main delivers A (captured generation 1).
    handle_resampler_rebuild(&flags, &sys, &mut res_prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));

    // RT renegotiates to B (gen 2) while A is still in the channel.
    request_rebuild(&flags, 96000, 48000);

    // RT drain: A is stale (1 < 2) → GC without unmute; PENDING stays set.
    drain_resamplers(
        &mut res_cons,
        &mut deferred_resampler,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_prod,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );
    assert_eq!(active.host_rate(), 48000);
    assert!(flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));

    // Main delivers B for generation 2 and clears the request.
    handle_resampler_rebuild(&flags, &sys, &mut res_prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_RESAMPLER_REBUILD));

    // RT drain: B matches → applied, generation recorded, unmuted.
    drain_resamplers(
        &mut res_cons,
        &mut deferred_resampler,
        &mut structural_applied,
        &mut active,
        &mut active_stream,
        &mut gc_prod,
        &mut parking_lot,
        &parking_lot_dirty,
        &gc_overflow,
        &flags,
    );
    assert_eq!(active.host_rate(), 96000);
    assert_eq!(
        flags.applied_rate_generation.load(Ordering::Relaxed),
        flags.requested_rate_generation.load(Ordering::Relaxed),
        "invariant: applied == requested before unmute"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING));

    // GC received the stale A, then the original active resampler.
    let stale = gc_cons.pop().unwrap();
    match stale {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 44100),
        _ => panic!("Expected stale GcItem::ResamplerSwap(44100)"),
    }
    let previous = gc_cons.pop().unwrap();
    match previous {
        GcItem::ResamplerSwap(payload) => assert_eq!(payload.resampler.host_rate(), 48000),
        _ => panic!("Expected previous GcItem::ResamplerSwap(48000)"),
    }
    assert!(gc_cons.pop().is_err());
}

/// Concurrent stress of the full request→build→deliver→drain protocol.
///
/// The RT worker simulates a renegotiation storm (host clock changes faster
/// than the DSP can catch up) while the main worker rebuilds whatever
/// generation is requested. The invariant
/// `applied_rate_generation == requested_rate_generation` must hold whenever
/// the callback is unmuted (`RESAMP_SWAP_PENDING` clear) — a stale resampler
/// must never be installed, and a pending request must never be lost.
#[test]
fn lost_wakeup_stress_invariant() {
    use crate::standalone::pw_host::rt_callback::{drain_resamplers, sync_rate};
    use neural_amp_modeler_rs::common::spsc::GcItem;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let flags = Arc::new(RtStatusFlags::new());
    let sys = SystemSnapshot::capture();
    let (res_prod, res_cons) = rtrb::RingBuffer::<Box<ResamplerSwapPayload>>::new(64);
    let (mut gc_prod, mut gc_cons) = rtrb::RingBuffer::<GcItem>::new(128);
    let gc_overflow = Arc::new(GcOverflowBuffer::default());
    let stop = Arc::new(AtomicBool::new(false));

    // Main-thread worker: mirrors the control loop — rebuild and deliver
    // whatever generation is currently requested.
    let flags_main = Arc::clone(&flags);
    let stop_main = Arc::clone(&stop);
    let main_worker = std::thread::spawn(move || {
        let mut prod = res_prod;
        while !stop_main.load(Ordering::Acquire) {
            handle_resampler_rebuild(&flags_main, &sys, &mut prod);
            std::thread::yield_now();
        }
    });

    // RT worker: simulates the audio callback — host clock renegotiation
    // (sync_rate) interleaved with payload draining and the mute invariant.
    let flags_rt = Arc::clone(&flags);
    let stop_rt = Arc::clone(&stop);
    let gc_overflow_rt = Arc::clone(&gc_overflow);
    let rt_worker = std::thread::spawn(move || {
        let mut cons = res_cons;
        let mut active = make_rs(48000, 48000);
        let mut active_stream = make_stream(48000, 48000);
        let mut deferred_resampler = None;
        let mut structural_applied;
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let rate_for_process = std::sync::atomic::AtomicU32::new(0);
        let host_rates = [44100u32, 48000, 96000, 44100, 96000];
        let mut i = 0usize;

        while !stop_rt.load(Ordering::Acquire) {
            // Each loop iteration is one audio callback: the per-quantum
            // structural budget resets to zero.
            structural_applied = 0;
            // Host clock renegotiation: advertise the next rate whenever the
            // DSP has not yet caught up — an adversarial renegotiation storm.
            let rate = host_rates[i % host_rates.len()];
            if rate != active.host_rate() {
                rate_for_process.store(rate, Ordering::Release);
            }
            sync_rate(&rate_for_process, &active, 48000, &flags_rt);

            drain_resamplers(
                &mut cons,
                &mut deferred_resampler,
                &mut structural_applied,
                &mut active,
                &mut active_stream,
                &mut gc_prod,
                &mut parking_lot,
                &parking_lot_dirty,
                &gc_overflow_rt,
                &flags_rt,
            );

            // Mute invariant: unmuted ⇔ applied == requested; a muted callback
            // may only lag (requested >= applied), never regress.
            let applied = flags_rt.applied_rate_generation.load(Ordering::Acquire);
            let requested = flags_rt.requested_rate_generation.load(Ordering::Acquire);
            let is_pending = flags_rt
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING);
            if is_pending {
                assert!(
                    requested >= applied,
                    "muted callback: applied={applied} > requested={requested}"
                );
            } else {
                assert_eq!(
                    applied, requested,
                    "unmuted with applied={applied} != requested={requested}"
                );
            }
            i += 1;
        }
    });

    // Control-plane role on the test thread: drain GC while the workers run.
    let mut drain_lot: [Option<GcItem>; 16] = Default::default();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    while std::time::Instant::now() < deadline {
        let _ = neural_amp_modeler_rs::common::spsc::drain_gc_channels(
            &mut gc_cons,
            &gc_overflow,
            &mut drain_lot,
            &flags,
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    stop.store(true, Ordering::Release);
    main_worker.join().unwrap();
    rt_worker.join().unwrap();

    // Settled state: muted-pending or fully applied (applied == requested).
    let _ = neural_amp_modeler_rs::common::spsc::drain_gc_channels(
        &mut gc_cons,
        &gc_overflow,
        &mut drain_lot,
        &flags,
    );
    if !flags.check_flag(spsc::RT_STATUS_RESAMP_SWAP_PENDING) {
        assert_eq!(
            flags.applied_rate_generation.load(Ordering::Acquire),
            flags.requested_rate_generation.load(Ordering::Acquire),
            "final state unmuted with applied != requested"
        );
    }
}

/// Oversampling rebuild: `handle_oversample_rebuild` builds both L/R engines,
/// stamps the pair with the captured `requested_os_generation`, pushes to SPSC,
/// and clears the rebuild flag.
#[test]
fn oversample_pair_built_stamped_and_pushed_atomically() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<OsEnginePair>>::new(2);
    let mut failures = RebuildFailureTracker::default();

    flags
        .requested_os_factor
        .store(OversampleFactor::X4.to_f32() as u32, Ordering::Relaxed);
    flags.requested_os_generation.store(7, Ordering::Release);
    flags.set_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD);

    handle_oversample_rebuild(&flags, &sys, &mut prod, &mut failures);

    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD),
        "successful push must clear the rebuild flag"
    );

    let pair = cons.pop().expect("one pair delivered");
    assert_eq!(pair.generation, 7, "pair must carry generation 7");
    assert_eq!(pair.l.factor(), OversampleFactor::X4);
    assert_eq!(pair.r.factor(), OversampleFactor::X4);
    assert!(
        cons.pop().is_err(),
        "both channels delivered in one envelope"
    );
}

/// Lost-wakeup guard for oversampling.
///
/// If `requested_os_generation` advances while the main thread is building the
/// engines, `rearm_os_if_superseded` re-arms `NEEDS_OS_REBUILD` so the main
/// loop triggers a follow-up rebuild immediately.
#[test]
fn oversample_rearm_when_superseded_during_build() {
    let flags = RtStatusFlags::new();

    flags.set_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD);
    flags.requested_os_generation.store(2, Ordering::Release);

    // Pair was built with generation 1 (stale compared to current generation 2)
    rearm_os_if_superseded(&flags, 1);

    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD),
        "superseded generation must re-arm the flag"
    );

    // If generation matches, the flag stays cleared
    rearm_os_if_superseded(&flags, 2);
    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD),
        "matching generation must leave the flag cleared"
    );
}

/// Fail-closed delivery retry: when the SPSC channel is full,
/// `handle_oversample_rebuild` emits a diagnostic and retains `NEEDS_OS_REBUILD`
/// so the swap is retried on the next cycle.
#[test]
fn oversample_channel_full_keeps_flag_for_retry() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let (mut prod, _cons) = rtrb::RingBuffer::<Box<OsEnginePair>>::new(1);
    let mut failures = RebuildFailureTracker::default();

    // Saturate the queue
    prod.push(Box::new(OsEnginePair {
        generation: 0,
        l: Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap()),
        r: Box::new(OversampleEngine::new(OversampleFactor::Off, 64).unwrap()),
    }))
    .unwrap();

    flags
        .requested_os_factor
        .store(OversampleFactor::X2.to_f32() as u32, Ordering::Relaxed);
    flags.requested_os_generation.store(1, Ordering::Release);
    flags.set_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD);

    handle_oversample_rebuild(&flags, &sys, &mut prod, &mut failures);

    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD),
        "channel full must retain the flag for automatic retry"
    );
}

/// Requests an oversampling engine rebuild exactly like the RT callback does
/// (factor first, then the generation bump, then the flag).
#[cfg(feature = "testing")]
fn request_os_rebuild(flags: &RtStatusFlags, factor: OversampleFactor) {
    flags
        .requested_os_factor
        .store(factor.to_f32() as u32, Ordering::Relaxed);
    flags
        .requested_os_generation
        .fetch_add(1, Ordering::Release);
    flags.set_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD);
}

/// A persistent `OversampleEngine::new` failure must not be retried on every
/// control-loop tick.
///
/// The fault is armed persistently for generation 1; the handler is called N
/// times (simulating N ≤ 100 ms control-loop iterations). Only the first call
/// attempts the build (counted by the fault hook); the failed-generation latch
/// suppresses the rest, so a persistent OOM produces exactly one allocation
/// attempt and one `log::error!` per generation. A newer request (generation 2)
/// re-enables the rebuild, which fails once and latches again. The latch also
/// suppresses a hypothetical same-generation flag re-arm (defense in depth).
#[cfg(feature = "testing")]
#[test]
fn oversample_build_failure_stops_retry_storm_until_newer_generation() {
    let _serial = OS_FAULT_TEST_LOCK.lock().unwrap();
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<OsEnginePair>>::new(4);
    let mut failures = RebuildFailureTracker::default();

    // RT requests gen 1; the engine build for it fails persistently.
    request_os_rebuild(&flags, OversampleFactor::X2);
    super::os_fault::arm_fail(&flags, 1);

    // N control-loop ticks with the same generation: exactly one build attempt.
    for _ in 0..16 {
        handle_oversample_rebuild(&flags, &sys, &mut prod, &mut failures);
    }
    assert_eq!(
        super::os_fault::attempts(&flags, 1),
        1,
        "a failed generation must be attempted at most once, never per tick"
    );
    assert_eq!(
        failures.os_failed_generation, 1,
        "the failed generation must be latched"
    );
    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD),
        "a failure without a newer request must clear the rebuild flag"
    );
    assert!(
        cons.pop().is_err(),
        "no payload may be delivered for a failed build"
    );

    // Defense in depth: even if the flag were re-armed for the SAME generation
    // (a path that today does not exist), the failed-generation latch must
    // suppress the rebuild rather than storm.
    flags.set_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD);
    handle_oversample_rebuild(&flags, &sys, &mut prod, &mut failures);
    assert_eq!(
        super::os_fault::attempts(&flags, 1),
        1,
        "same-generation re-arm must stay suppressed by the latch"
    );

    // A newer request (gen 2) re-enables the rebuild: one more attempt, one
    // more latch.
    super::os_fault::arm_fail(&flags, 2);
    request_os_rebuild(&flags, OversampleFactor::X4);
    handle_oversample_rebuild(&flags, &sys, &mut prod, &mut failures);
    assert_eq!(
        super::os_fault::attempts(&flags, 2),
        1,
        "a newer generation must re-enable exactly one rebuild attempt"
    );
    assert_eq!(failures.os_failed_generation, 2);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_OS_REBUILD));
}

/// Deterministic slimmable rejections must clear the request immediately:
/// no retry of the slice/prewarm allocators on later ticks.
///
/// Covers the two terminal classes: `target_ch < 4` and a non-WaveNet model
/// (the `lstm.nam` fixture loads as an LSTM, which `slice_wavenet_model`
/// cannot process).
#[test]
fn slimmable_terminal_rejection_clears_flag_no_retry() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(2);
    let mut failures = RebuildFailureTracker::default();

    // Terminal case 1: target_ch < 4.
    request_slimmable_rebuild(&flags, 2);
    handle_slimmable_rebuild(&flags, None, None, false, &sys, &mut prod, &mut failures);
    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "target_ch < 4 is terminal: the request must be cleared"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED));
    assert!(cons.pop().is_err(), "terminal rejection delivers nothing");

    // Repeated ticks stay inert: no retry storm.
    for _ in 0..8 {
        handle_slimmable_rebuild(&flags, None, None, false, &sys, &mut prod, &mut failures);
    }
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));

    // Terminal case 2: model not sliceable (non-WaveNetDyn). A later request
    // (new generation) is also rejected terminally — still no retry.
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/models/lstm.nam");
    let loaded = loader::load_and_build_model(&path, &sys, true, loader::LoadOptions::default())
        .expect("lstm fixture must load");
    let lstm = loaded.model_l.expect("lstm fixture must expose an L model");
    assert!(
        !matches!(lstm.as_ref(), StaticModel::WavenetDyn(_)),
        "precondition: the fixture must not be a WavenetDyn"
    );

    request_slimmable_rebuild(&flags, 4);
    handle_slimmable_rebuild(
        &flags,
        Some(lstm.as_ref()),
        None,
        false,
        &sys,
        &mut prod,
        &mut failures,
    );
    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "non-WaveNet is terminal: the request must be cleared, no retry"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED));
    assert!(cons.pop().is_err(), "terminal rejection delivers nothing");
}

/// A transient `slice_wavenet_model` failure must not be retried on every
/// control-loop tick.
///
/// The slice fault is armed persistently for generation 1; the handler is
/// called N times (simulating N ≤ 100 ms control-loop iterations). Only the
/// first call enters `slice_wavenet_model` + `prewarm()` (counted by the fault
/// hook); the failed-generation latch suppresses the rest. A newer request
/// (generation 2) re-enables the rebuild, which fails once and latches again.
#[cfg(feature = "testing")]
#[test]
fn slimmable_slice_failure_does_not_retry_every_control_loop_tick() {
    let _serial = SLIMMABLE_FAULT_TEST_LOCK.lock().unwrap();
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let full = load_slimmable_full_model().expect("fixture model");
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(4);
    let mut failures = RebuildFailureTracker::default();

    // RT requests gen 1 (ch 4); slicing it fails persistently.
    request_slimmable_rebuild(&flags, 4);
    super::slimmable_fault::arm_fail(&flags, 1);

    // N control-loop ticks with the same generation: exactly one slice attempt.
    for _ in 0..16 {
        handle_slimmable_rebuild(
            &flags,
            Some(full.as_ref()),
            Some(full.as_ref()),
            true,
            &sys,
            &mut prod,
            &mut failures,
        );
    }
    assert_eq!(
        super::slimmable_fault::attempts(&flags, 1),
        1,
        "a failed generation must be sliced at most once, never per tick"
    );
    assert_eq!(
        failures.slimmable_failed_generation, 1,
        "the failed generation must be latched"
    );
    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "a slice failure without a newer request must clear the rebuild flag"
    );
    assert!(
        flags.check_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED),
        "the transient failure signal must be published for telemetry"
    );
    assert!(
        cons.pop().is_err(),
        "no pair may be delivered for a failed slice"
    );

    // Defense in depth: a same-generation flag re-arm stays suppressed.
    flags.set_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD);
    handle_slimmable_rebuild(
        &flags,
        Some(full.as_ref()),
        Some(full.as_ref()),
        true,
        &sys,
        &mut prod,
        &mut failures,
    );
    assert_eq!(
        super::slimmable_fault::attempts(&flags, 1),
        1,
        "same-generation re-arm must stay suppressed by the latch"
    );

    // A newer request (gen 2) re-enables the rebuild: one more attempt, one
    // more latch.
    super::slimmable_fault::arm_fail(&flags, 2);
    request_slimmable_rebuild(&flags, 4);
    handle_slimmable_rebuild(
        &flags,
        Some(full.as_ref()),
        Some(full.as_ref()),
        true,
        &sys,
        &mut prod,
        &mut failures,
    );
    assert_eq!(
        super::slimmable_fault::attempts(&flags, 2),
        1,
        "a newer generation must re-enable exactly one slice attempt"
    );
    assert_eq!(failures.slimmable_failed_generation, 2);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));
}
