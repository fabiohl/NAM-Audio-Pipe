// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Capture/playback stream setup for one host instance.

use super::super::capture::state::{CaptureState, RtHostChannels};
use super::super::run_lifecycle::snapshot_and_publish_initial_latencies;
use super::super::{SharedBackendStatus, capture, playback};
use crate::recording::transport::RecordingSender;
use neural_amp_modeler_rs::common::spsc::{GcItem, RtStatusFlags};
use neural_amp_modeler_rs::dsp::pipeline::BridgeRef;
use pipewire as pw;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};

pub(super) type StreamSet<'c> = (
    pw::stream::StreamBox<'c>,
    pw::stream::StreamListener<()>,
    pw::stream::StreamBox<'c>,
    pw::stream::StreamListener<()>,
    u32,
);

#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub(super) fn create_streams<'c>(
    core: &'c pw::core::Core,
    bridge_ptr: BridgeRef,
    buffer_size: u32,
    target_cpu: usize,
    state_ptr: *mut CaptureState,
    channels_ptr: *mut RtHostChannels,
    rate_for_param: Arc<AtomicU32>,
    rt_status: Arc<RtStatusFlags>,
    rec_ptr: *mut RecordingSender,
    parking_lot_ptr: *mut [Option<GcItem>; 16],
    parking_lot_dirty_ptr: *const AtomicBool,
    recording_failed: Option<Arc<AtomicBool>>,
    backend_for_capture: Arc<SharedBackendStatus>,
    backend_for_playback: Arc<SharedBackendStatus>,
    backend_status: &SharedBackendStatus,
    thread_loop: &pw::thread_loop::ThreadLoopBox,
    output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
) -> anyhow::Result<StreamSet<'c>> {
    let _lock = thread_loop.lock();
    let latency_str = format!("{}/48000", buffer_size);
    // SAFETY: both PipeWire loops are stopped here, so `state_ptr` is exclusive.
    let initial_latency_samples =
        unsafe { snapshot_and_publish_initial_latencies(state_ptr, backend_status) };

    let (cs, cl) = capture::setup_capture_stream(
        core,
        bridge_ptr,
        buffer_size,
        initial_latency_samples,
        target_cpu,
        state_ptr,
        channels_ptr,
        rate_for_param.clone(),
        rt_status.clone(),
        rec_ptr,
        parking_lot_ptr,
        parking_lot_dirty_ptr,
        recording_failed.clone(),
        backend_for_capture.clone(),
    )?;

    let (ps, pl) = playback::setup_playback_stream(
        core,
        bridge_ptr,
        buffer_size,
        initial_latency_samples,
        &latency_str,
        rt_status.clone(),
        backend_for_playback.clone(),
        output_limit,
    )?;

    Ok((cs, cl, ps, pl, initial_latency_samples))
}
