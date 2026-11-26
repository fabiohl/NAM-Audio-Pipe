// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Initial latency publication, banner emission, and bridge diagnostics.

use super::super::SharedBackendStatus;
use super::super::capture::state::CaptureState;
use crate::standalone::banner::{BannerInfo, BannerRequest, render_banner};
use crate::standalone::colors::color_enabled;
use crate::standalone::pw_host::StreamStatusFlags;
use crate::standalone::pw_host::latency;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::pipeline::DspBridge;
use std::sync::atomic::Ordering;

/// Reads the currently installed DSP latencies from `state_ptr` (while the
/// PipeWire loops are stopped) and publishes them to `StreamStatusFlags`.
///
/// # Safety
/// Both PipeWire loops must be stopped so `state_ptr` has no concurrent RT
/// accessor.
pub(crate) unsafe fn snapshot_and_publish_initial_latencies(
    state_ptr: *mut CaptureState,
    backend_status: &SharedBackendStatus,
) -> u32 {
    // SAFETY: caller guarantees PipeWire loops are stopped and `state_ptr` is exclusive.
    let (initial_resamp_lat, initial_cab_lat, initial_os_native, initial_nam_rate) = unsafe {
        let state = &*state_ptr;
        let resamp = state.stream.latency_samples();
        let cab = crate::standalone::cabinet::active_cabinet_latency(
            state.active_cabsim.as_deref(),
            state.active_cab_nam.as_ref(),
        );
        let os = state.os_l.latency_samples() as u32;
        (resamp, cab, os, state.current_nam_rate)
    };
    let initial_os_lat =
        latency::convert_os_latency_to_host(initial_os_native, 48_000, initial_nam_rate);
    let initial_latency_samples = initial_resamp_lat + initial_cab_lat + initial_os_lat;
    let ss = backend_status.stream_status();
    ss.applied_nam_rate
        .store(initial_nam_rate, Ordering::Release);
    ss.applied_resampler_latency
        .store(initial_resamp_lat, Ordering::Release);
    ss.applied_cabsim_latency
        .store(initial_cab_lat, Ordering::Release);
    ss.applied_oversample_latency
        .store(initial_os_native, Ordering::Release);
    ss.applied_latency_pair.store(
        StreamStatusFlags::pack_latency_pair(initial_resamp_lat, initial_cab_lat),
        Ordering::Release,
    );
    initial_latency_samples
}

/// Emits the consolidated status banner once the first RT quantum reports an
/// effective buffer size.
pub(crate) fn maybe_emit_status_banner(
    banner_emitted: &mut bool,
    rt_status: &RtStatusFlags,
    banner: &BannerRequest,
    host_rate: u32,
    current_dsp_latency: u32,
) {
    let effective_quantum = rt_status.requested_buffer_frames.load(Ordering::Relaxed);
    if *banner_emitted || effective_quantum == 0 {
        return;
    }
    let policy = rt_status.rt_policy.load(Ordering::Relaxed);
    let policy_name = if policy == libc::SCHED_FIFO {
        "FIFO"
    } else if policy == libc::SCHED_RR {
        "RR"
    } else {
        "OTHER"
    };
    let banner_info = BannerInfo {
        request: banner.clone(),
        sample_rate: host_rate,
        quantum: effective_quantum,
        dsp_latency_samples: current_dsp_latency,
        rt_fifo: rt_status.check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO),
        rt_policy: policy_name,
        rt_priority: rt_status.confirmed_priority.load(Ordering::Relaxed),
        mlockall_ok: rt_setup::mlockall_ok(),
    };
    log::info!(
        "{}",
        render_banner(&banner_info, color_enabled()).trim_end()
    );
    *banner_emitted = true;
}

/// Reads bridge diagnostic counters via raw field projections while RT
/// callbacks are live, returning any newly accumulated dropped frames.
pub(crate) fn sample_bridge_diagnostics(
    bridge_ptr: neural_amp_modeler_rs::dsp::pipeline::BridgeRef,
) -> u32 {
    // SAFETY: `bridge_ptr` points to the process-lifetime `DspBridge`
    // allocation; the raw projections only read/swap atomic fields through
    // `&raw const` without materializing a `&DspBridge`.
    let bridge_raw = unsafe { bridge_ptr.as_ptr() }.cast_const();
    // SAFETY: `bridge_raw` is valid; both reads are atomic field projections.
    let (written_gen, consumed_gen) = unsafe {
        (
            DspBridge::written_gen_raw(bridge_raw),
            DspBridge::consumed_gen_raw(bridge_raw),
        )
    };
    log::trace!(
        "bridge generations: published={written_gen} consumed={consumed_gen} lag={}",
        written_gen.saturating_sub(consumed_gen)
    );
    // SAFETY: same projection contract as the generation snapshot above.
    unsafe { DspBridge::drain_dropped_frames_raw(bridge_raw) }
}
