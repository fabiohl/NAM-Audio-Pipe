// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, SlimModelPair};
use neural_amp_modeler_rs::loader;
use neural_amp_modeler_rs::math::common::AlignedVec;
use neural_amp_modeler_rs::models::wavenet::WaveNetModelDyn;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

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
        l: fake_wavenet(ch),
        r: Some(fake_wavenet(ch)),
    })
}

/// Loads the engine's free-topology WaveNet fixture (`WavenetDyn`, CH=7) and
/// returns a full-model storage clone suitable for `handle_slimmable_rebuild`.
fn load_slimmable_full_model() -> Option<Box<StaticModel>> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("../NeuralAmpModeler-rs/tests/fixtures/models/wavenet_dyn_free.nam");
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

/// F-RB-005 core (producer side): `handle_slimmable_rebuild` slices and
/// prewarms BOTH channels before a single atomic push of a `SlimModelPair`;
/// the envelope carries the captured generation and the requested channel count.
#[test]
fn slimmable_pair_built_and_pushed_atomically() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let full = load_slimmable_full_model().expect("fixture model");
    request_slimmable_rebuild(&flags, 4);

    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(2);

    handle_slimmable_rebuild(&flags, Some(full.as_ref()), true, &sys, &mut prod);

    assert!(
        !flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "successful atomic push must clear the request"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED));

    let pair = cons.pop().expect("one pair must be delivered");
    assert_eq!(pair.generation, 1, "pair must carry the request generation");
    assert_eq!(pair.channels, 4);
    assert_eq!(pair.l.channels(), 4);
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

    handle_slimmable_rebuild(&flags, Some(full.as_ref()), false, &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));
    let pair = cons.pop().expect("one pair must be delivered");
    assert_eq!(pair.l.channels(), 4);
    assert!(pair.r.is_none(), "mono config must not build an R model");
}

/// Fail-closed (F-RB-005): when the SPSC channel is full, NEITHER channel is
/// delivered and `NEEDS_SLIMMABLE_REBUILD` stays armed for a full retry — a
/// partial L-without-R delivery is impossible.
#[test]
fn slimmable_push_full_keeps_needs_for_retry() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let full = load_slimmable_full_model().expect("fixture model");
    request_slimmable_rebuild(&flags, 4);

    // Saturate the single-slot channel so the handler's push must fail.
    let (mut prod, mut cons) = rtrb::RingBuffer::<Box<SlimModelPair>>::new(1);
    prod.push(fake_pair(0, 8)).unwrap();

    handle_slimmable_rebuild(&flags, Some(full.as_ref()), true, &sys, &mut prod);

    assert!(
        flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD),
        "full channel must keep the request armed for a full retry"
    );
    assert!(!flags.check_flag(spsc::RT_STATUS_SLIMMABLE_SLICE_FAILED));

    // Free the channel and retry: the whole pair is now delivered.
    let _ = cons.pop().unwrap();
    handle_slimmable_rebuild(&flags, Some(full.as_ref()), true, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_SLIMMABLE_REBUILD));
    let pair = cons.pop().expect("retry must deliver the pair");
    assert_eq!(pair.channels, 4);
    assert!(pair.r.is_some());
    assert!(cons.pop().is_err());
}

/// Lost-wakeup guard (F-RB-004 pattern): `rearm_slimmable_if_superseded` keeps
/// the request armed exactly when a newer slimmable generation was published
/// while the pair was being built.
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

    // RT requests A (gen 1, ch 4); main builds and pushes pair A.
    request_slimmable_rebuild(&flags, 4);
    handle_slimmable_rebuild(&flags, Some(full.as_ref()), true, &sys, &mut sl_prod);
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
    handle_slimmable_rebuild(&flags, Some(full.as_ref()), true, &sys, &mut sl_prod);
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
    let stale_l = gc_c.pop().unwrap();
    let stale_r = gc_c.pop().unwrap();
    match (stale_l, stale_r) {
        (GcItem::Model(m1), GcItem::Model(m2)) => {
            let mut chs = [m1.channels(), m2.channels()];
            chs.sort_unstable();
            assert_eq!(chs, [4, 4], "stale pair must be discarded whole");
        }
        _ => panic!("expected stale GcItem::Model pair"),
    }
}

fn make_rs(pw: u32, nam: u32) -> Box<NamResampler> {
    Box::new(NamResampler::new(pw, nam, 64).unwrap())
}

fn make_payload(generation: u64, pw: u32, nam: u32) -> Box<ResamplerSwapPayload> {
    Box::new(ResamplerSwapPayload {
        generation,
        resampler: make_rs(pw, nam),
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

    let (mut prod, mut cons) = rtrb::RingBuffer::new(1);
    prod.push(Some(Box::new(test_pair(&ir, 64, 48000))))
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

/// F-RB-006 rate calibration: the preserved original IR must be resampled
/// specifically for the requested host output rate, the delivered pair is
/// stamped with that rate, and both channel adapters share the requested
/// partition. The partition count scales with the rate ratio — evidence the
/// IR was actually resampled, not merely restamped.
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
        let (mut prod, mut cons) = rtrb::RingBuffer::new(2);

        handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);

        assert!(
            !flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD),
            "successful delivery must clear the request ({target_rate} Hz)"
        );
        let pair = cons
            .pop()
            .unwrap_or_else(|_| panic!("pair must be delivered ({target_rate} Hz)"));
        let pair = pair.expect("successful rebuild delivers a pair, not bypass");
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

/// Same-rate rebuild takes the no-resample path and keeps the IR untouched.
#[test]
fn cabsim_rebuild_same_rate_no_resample() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let ir: Vec<f32> = (0..256).map(|i| (-i as f32 / 64.0).exp()).collect();
    request_cabsim_rebuild(&flags, 64, 48000);
    let (mut prod, mut cons) = rtrb::RingBuffer::new(2);

    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    let pair = cons.pop().unwrap().expect("pair delivered");
    assert_eq!(pair.sample_rate, 48000);
    assert_eq!(pair.l.num_partitions(), 4, "256 samples / 64 partition");
}

/// G-RB-003 / T6.2: a spurious RT-requested partition outside the domain
/// [16, MAX_RESAMP_BUF] is clamped before any `ConvEngine` is built — the
/// delivered pair is never instantiated with an oversized (or degenerate) FFT.
#[test]
fn cabsim_rebuild_clamps_out_of_domain_partition_size() {
    let sys = SystemSnapshot::capture();
    let ir: Vec<f32> = (0..256).map(|i| (-i as f32 / 64.0).exp()).collect();

    // Above the ceiling: 16384 -> clamped to MAX_RESAMP_BUF.
    let flags = RtStatusFlags::new();
    request_cabsim_rebuild(&flags, 16384, 48000);
    let (mut prod, mut cons) = rtrb::RingBuffer::new(2);
    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    let pair = cons.pop().unwrap().expect("pair delivered");
    assert_eq!(
        pair.partition_size(),
        MAX_RESAMP_BUF,
        "oversized partition must be clamped to the ceiling"
    );
    assert_eq!(pair.l.num_partitions(), 1, "256-sample IR / 8192 partition");

    // Below the floor: 1 -> clamped to the 16-sample minimum.
    let flags = RtStatusFlags::new();
    request_cabsim_rebuild(&flags, 1, 48000);
    let (mut prod, mut cons) = rtrb::RingBuffer::new(2);
    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    let pair = cons.pop().unwrap().expect("pair delivered");
    assert_eq!(
        pair.partition_size(),
        16,
        "sub-minimum partition must be clamped"
    );

    // In-domain requests pass through untouched.
    let flags = RtStatusFlags::new();
    request_cabsim_rebuild(&flags, 64, 48000);
    let (mut prod, mut cons) = rtrb::RingBuffer::new(2);
    handle_cabsim_rebuild(&flags, Some(&ir), 48000, &sys, &mut prod);
    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    let pair = cons.pop().unwrap().expect("pair delivered");
    assert_eq!(pair.partition_size(), 64);
}

/// Rollback (F-RB-006): a failed IR resample/build must deliver `None` —
/// safe cab-sim bypass — instead of letting the RT run a divergent-rate IR.
#[test]
fn cabsim_rebuild_failure_pushes_none_bypass() {
    let flags = RtStatusFlags::new();
    let sys = SystemSnapshot::capture();
    let empty: Vec<f32> = Vec::new();
    request_cabsim_rebuild(&flags, 64, 96000);
    let (mut prod, mut cons) = rtrb::RingBuffer::new(2);

    // Empty raw samples make the IR resample fail deterministically.
    handle_cabsim_rebuild(&flags, Some(&empty), 48000, &sys, &mut prod);

    assert!(!flags.check_flag(spsc::RT_STATUS_NEEDS_CABSIM_REBUILD));
    assert!(
        cons.pop().unwrap().is_none(),
        "rebuild failure must deliver None (safe bypass)"
    );
}

/// Lost-wakeup guard (F-RB-004 pattern): `rearm_cabsim_if_superseded` keeps
/// the request armed exactly when a newer cabsim generation was published
/// while the pair was being built.
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

/// End-to-end request→deliver→drain: the delivered pair replaces the active
/// one; the retired pair's two channel adapters reach GC (zero alloc swap).
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
        matches!(old, GcItem::CabSimPair(_)),
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

/// Lost-wakeup guard unit test (F-RB-004): `rearm_rebuild_if_superseded` must
/// keep `NEEDS_RESAMPLER_REBUILD` armed exactly when a newer request generation
/// was published while the main thread was building.
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

/// Deterministic lost-wakeup interleaving (F-RB-004 / T2.1 acceptance):
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
    let mut deferred_resampler = None;
    let mut structural_applied = 0usize;

    drain_resamplers(
        &mut cons,
        &mut deferred_resampler,
        &mut structural_applied,
        &mut active,
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
        GcItem::Resampler(rs) => assert_eq!(rs.host_rate(), 44100),
        _ => panic!("Expected stale GcItem::Resampler(44100)"),
    }
    let previous = gc_cons.pop().unwrap();
    match previous {
        GcItem::Resampler(rs) => assert_eq!(rs.host_rate(), 48000),
        _ => panic!("Expected previous GcItem::Resampler(48000)"),
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
        GcItem::Resampler(rs) => assert_eq!(rs.host_rate(), 44100),
        _ => panic!("Expected stale GcItem::Resampler(44100)"),
    }
    let previous = gc_cons.pop().unwrap();
    match previous {
        GcItem::Resampler(rs) => assert_eq!(rs.host_rate(), 48000),
        _ => panic!("Expected previous GcItem::Resampler(48000)"),
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
        let mut deferred_resampler = None;
        let mut structural_applied;
        let mut parking_lot: [Option<GcItem>; 16] = Default::default();
        let parking_lot_dirty = AtomicBool::new(false);
        let rate_for_process = std::sync::atomic::AtomicU32::new(0);
        let host_rates = [44100u32, 48000, 96000, 44100, 96000];
        let mut i = 0usize;

        while !stop_rt.load(Ordering::Acquire) {
            // Each loop iteration is one audio callback: the per-quantum
            // structural budget resets to zero (T2.5).
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
