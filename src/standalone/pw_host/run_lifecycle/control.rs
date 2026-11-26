// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Non-RT control-loop housekeeping.

use super::super::SharedBackendStatus;
use super::super::handlers;
use super::super::latency;
use super::super::output_pw::AppState;
use super::status::{maybe_emit_status_banner, sample_bridge_diagnostics};
use crate::standalone::banner::BannerRequest;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, RtStatusFlags, SlimModelPair};
use neural_amp_modeler_rs::dsp::oversample::OsEnginePair;
use neural_amp_modeler_rs::models::StaticModel;
use pipewire as pw;
use rtrb::{Consumer, Producer};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// Performs one non-RT control-loop housekeeping cycle (rebuilds, latency sync,
/// telemetry polling, banner emission, and periodic GC drain).
#[expect(
    clippy::too_many_arguments,
    reason = "Aggregates the control-loop producers and trackers owned by run_pipewire_host_with_cabinet"
)]
pub(crate) fn run_control_housekeeping(
    rt_status: &Arc<RtStatusFlags>,
    sys: &SystemSnapshot,
    resampler_producer: &mut Producer<
        Box<neural_amp_modeler_rs::common::spsc::ResamplerSwapPayload>,
    >,
    nam_cab_source: Option<&(Vec<u8>, crate::standalone::cabinet::CabAssetKind, String)>,
    cab_nam_producer: Option<&mut Producer<crate::standalone::cabinet::PipeCabNamPayload>>,
    ir_raw_samples: Option<&[f32]>,
    ir_source_rate: u32,
    ir_level_policy: crate::standalone::cli_domain::IrLevelPolicy,
    cabsim_producer: &mut Producer<Box<neural_amp_modeler_rs::common::spsc::CabSimSwapPayload>>,
    backend_status: &SharedBackendStatus,
    buffer_size: u32,
    latency_tracker: &mut latency::LatencyPublicationTracker,
    thread_loop: &pw::thread_loop::ThreadLoopBox,
    app_state: &AppState<
        pw::stream::StreamListener<()>,
        pw::stream::StreamBox<'_>,
        pw::stream::StreamListener<()>,
        pw::stream::StreamBox<'_>,
    >,
    full_wavenet_model_l: Option<&StaticModel>,
    full_wavenet_model_r: Option<&StaticModel>,
    has_model_r: bool,
    slimmable_producer: &mut Producer<Box<SlimModelPair>>,
    os_producer: &mut Producer<Box<OsEnginePair>>,
    rebuild_failures: &mut handlers::RebuildFailureTracker,
    os_scratch_len: usize,
    bridge_ptr: neural_amp_modeler_rs::dsp::pipeline::BridgeRef,
    was_silent: &mut bool,
    was_fading: &mut bool,
    poll_state: &mut rt_setup::PollState,
    banner_emitted: &mut bool,
    banner: &BannerRequest,
    gc_consumer: &mut Consumer<GcItem>,
    gc_overflow: &GcOverflowBuffer,
    control_parking_lot: &mut [Option<GcItem>; 16],
) {
    let active = rt_status.active_rate.load(Ordering::Relaxed);
    if active != 0 {
        neural_amp_modeler_rs::common::diagnostics::ACTIVE_SAMPLE_RATE
            .store(active, Ordering::Relaxed);
    }

    let host_rate = latency::resolve_effective_host_rate(
        active,
        backend_status
            .stream_status()
            .last_known_rate
            .load(Ordering::Relaxed),
    );

    let _ = handlers::handle_resampler_rebuild(rt_status, sys, resampler_producer);
    if let (Some((raw_bytes, kind, display_name)), Some(nam_prod)) =
        (nam_cab_source, cab_nam_producer)
    {
        let _ = handlers::handle_cabinet_nam_rebuild(
            rt_status,
            raw_bytes,
            *kind,
            display_name,
            sys,
            nam_prod,
        );
    } else {
        let _ = handlers::handle_cabsim_rebuild(
            rt_status,
            ir_raw_samples,
            ir_source_rate,
            ir_level_policy,
            sys,
            cabsim_producer,
        );
    }

    let (active_resampler_latency, active_cabsim_latency, active_os_native_latency) =
        backend_status.stream_status().read_applied_latencies();
    let applied_nam_rate = backend_status
        .stream_status()
        .applied_nam_rate
        .load(Ordering::Relaxed);
    let model_rate = if applied_nam_rate != 0 {
        applied_nam_rate
    } else {
        48_000
    };
    let active_os_latency =
        latency::convert_os_latency_to_host(active_os_native_latency, host_rate, model_rate);

    let current_dsp_latency = active_cabsim_latency + active_resampler_latency + active_os_latency;
    handlers::handle_quantum_log(rt_status, buffer_size, current_dsp_latency, host_rate);

    let latency_target = latency::LatencyTarget {
        samples: current_dsp_latency,
        host_rate,
    };
    let now = Instant::now();
    if latency_tracker.needs_sync(latency_target, now) {
        let _lock = thread_loop.lock();
        let mut publisher = latency::PipewireLatencyPublisher::new(
            &app_state.capture_stream,
            &app_state.playback_stream,
        );
        let _ = latency_tracker.sync_at(
            &mut publisher,
            latency_target,
            active_cabsim_latency,
            active_resampler_latency,
            active_os_latency,
            now,
        );
    }

    handlers::handle_slimmable_rebuild(
        rt_status,
        full_wavenet_model_l,
        full_wavenet_model_r,
        has_model_r,
        sys,
        slimmable_producer,
        rebuild_failures,
    );
    handlers::handle_oversample_rebuild(
        rt_status,
        sys,
        os_producer,
        rebuild_failures,
        os_scratch_len,
    );

    let dropped_frames = sample_bridge_diagnostics(bridge_ptr);
    (*was_silent, *was_fading) = rt_setup::poll_rt_status(
        rt_status,
        Some(backend_status.stream_status()),
        sys,
        *was_silent,
        *was_fading,
        dropped_frames,
        poll_state,
    );

    maybe_emit_status_banner(
        banner_emitted,
        rt_status,
        banner,
        host_rate,
        current_dsp_latency,
    );

    // The parking lot is caller-owned and reused across control-loop ticks;
    // this loop only drains channels, so it never parks new items here.
    let drained = drain_control_gc(gc_consumer, gc_overflow, rt_status, control_parking_lot);
    rt_status
        .drains
        .fetch_add(drained as u32, Ordering::Relaxed);
}

/// Drains the GC ring/overflow/parking-lot once from the control loop.
///
/// The caller owns `parking_lot` so the hot 100 ms control tick never
/// re-instantiates the 16-slot array.
pub(crate) fn drain_control_gc(
    gc_consumer: &mut Consumer<GcItem>,
    gc_overflow: &GcOverflowBuffer,
    rt_status: &RtStatusFlags,
    parking_lot: &mut [Option<GcItem>; 16],
) -> usize {
    neural_amp_modeler_rs::common::spsc::drain_gc_channels(
        gc_consumer,
        gc_overflow,
        parking_lot,
        rt_status,
    )
}
