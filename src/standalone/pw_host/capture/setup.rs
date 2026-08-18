// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! `setup_capture_stream` — configures the PipeWire capture stream (Virtual Sink)
//! and its RT listener, with process, param_changed, and state_changed closures.

use super::super::rt_callback;
use super::state::CaptureState;
use crate::recording::buffer::{MAX_BLOCK_SIZE, RingPayload};
use crate::standalone::colors::Colorize;
use crate::standalone::pw_host::output_pw::build_spa_format_pod;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{GcItem, GcOverflowBuffer, ParamPayload, RtStatusFlags};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::pipeline::{
    BridgeRef, DspBridgeWriter, DspBuffers, DspPipelineContext,
};
use neural_amp_modeler_rs::models::StaticModel;

use pipewire as pw;
use pw::properties::properties;
use rtrb::Consumer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Assembles PipeWire property attributes for the capture Virtual Sink node.
pub fn create_capture_properties(buffer_size: u32) -> pw::properties::PropertiesBox {
    let mut capture_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Duplex",
        *pw::keys::MEDIA_ROLE => "DSP",
        *pw::keys::MEDIA_CLASS => "Audio/Sink",
        *pw::keys::NODE_NAME => crate::standalone::pw_host::identity::PW_CAPTURE_NODE_NAME,
        *pw::keys::NODE_DESCRIPTION => crate::standalone::pw_host::identity::PW_CAPTURE_NODE_DESC,
        *pw::keys::NODE_VIRTUAL => "true",
        *pw::keys::PRIORITY_SESSION => "2000",
        *pw::keys::PRIORITY_DRIVER => "2000",
        "audio.position" => "FL,FR",
        "node.group" => crate::standalone::pw_host::identity::PW_NODE_GROUP,
        "node.link-group" => crate::standalone::pw_host::identity::PW_LINK_GROUP,
    };

    if buffer_size > 0 {
        let latency_str = format!("{}/48000", buffer_size);
        capture_props.insert("node.latency", latency_str.as_str());
    }

    capture_props
}

/// Builds the SPA format specification POD for planar 2-channel F32 audio.
pub fn build_capture_format_pod(format_buf: &mut [u8; 1024]) -> anyhow::Result<&pw::spa::pod::Pod> {
    let mut audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(pw::spa::param::audio::AudioFormat::F32P);
    audio_info.set_channels(2);

    // SAFETY: format_buf is guaranteed to be a 1024-byte array on the stack,
    // matching the layout and lifetime expected by build_spa_format_pod.
    unsafe { build_spa_format_pod(&audio_info, format_buf) }
}

/// Configures the capture stream (Virtual Sink) and its RT listener.
///
/// The `process()` closure captures all DSP state by `move` and executes
/// the full pipeline: resampler draining, command reception,
/// rate synchronization, and DSP processing via `capture_dsp_pipeline`.
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn setup_capture_stream<'c>(
    core: &'c pw::core::Core,
    bridge_ptr: BridgeRef,
    buffer_size: u32,
    ir_raw_samples: Option<Vec<f32>>,
    sys: &neural_amp_modeler_rs::common::diagnostics::SystemSnapshot,
    target_cpu: usize,
    mut consumer: Consumer<ParamPayload>,
    mut gc_producer: rtrb::Producer<GcItem>,
    gc_overflow: Arc<GcOverflowBuffer>,
    mut resampler_consumer: Consumer<Box<neural_amp_modeler_rs::dsp::resampler::NamResampler>>,
    mut cabsim_consumer: Consumer<
        Option<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter>,
    >,
    rt_status: Arc<RtStatusFlags>,
    slimmable_consumer: Consumer<Option<Box<StaticModel>>>,
    os_consumer: Consumer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>,
    oversample: OversampleFactor,
    recording_producer_ptr: *mut Option<rtrb::Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    parking_lot_ptr: *mut [Option<GcItem>; 16],
    parking_lot_dirty_ptr: *const AtomicBool,
    recording_data_available: Option<Arc<AtomicBool>>,
) -> anyhow::Result<(pw::stream::StreamBox<'c>, pw::stream::StreamListener<()>)> {
    let capture_props = create_capture_properties(buffer_size);

    let capture_stream = pw::stream::StreamBox::new(
        core,
        crate::standalone::pw_host::identity::PW_CAPTURE_STREAM_NAME,
        capture_props,
    )?;

    let mut state = CaptureState::init(sys, oversample);
    state.ir_raw_samples = ir_raw_samples;
    state.slimmable_rx = Some(slimmable_consumer);
    state.os_rx = Some(os_consumer);
    let rate_for_param = state.shared_target_rate.clone();
    let rate_for_process = state.shared_target_rate.clone();

    let rt_status_for_process = rt_status.clone();
    let gc_overflow_for_process = gc_overflow.clone();

    let lut = neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut();

    let capture_listener = capture_stream
        .add_local_listener::<()>()
        .state_changed(super::listeners::state_changed_handler)
        .param_changed(move |stream, user_data, id, param| {
            super::listeners::param_changed_handler(stream, user_data, id, param, &rate_for_param)
        })
        .process(move |stream: &pw::stream::Stream, _info| {
            // SAFETY: recording_producer_ptr points to a stack-local in
            // run_pipewire_host that outlives this closure. The RT thread
            // is the only writer, and after thread_loop.stop() the shutdown
            // path takes sequential ownership. No concurrent access occurs.
            let recording_producer = unsafe { &mut *recording_producer_ptr };
            let recording_data_available_ref = recording_data_available.as_deref();
            // SAFETY: parking_lot_ptr and parking_lot_dirty_ptr point to stack-local slots in
            // run_pipewire_host that outlive this closure (same contract as
            // recording_producer_ptr). While the loop runs, the RT callback
            // is the sole writer; after thread_loop.stop() the main thread
            // takes single-owner handoff and drains the 16 slots off-RT
            // (R-04). The periodic drain in run_pipewire_host NEVER touches
            // this slot — that would race with the RT flush below.
            let parking_lot = unsafe { &mut *parking_lot_ptr };
            let parking_lot_dirty = unsafe { &*parking_lot_dirty_ptr };
            // Cold-path RT setup: must run on the actual DSP data thread (this callback),
            // NOT in `state_changed_handler` (which executes on the separate PipeWire ThreadLoop thread).
            // This ensures DAZ/FTZ (MXCSR), SCHED_FIFO, thread name, and CPU affinity apply directly
            // to the active audio processing thread (H-06 / Sprint C-01).
            if !state.thread_configured {
                rt_setup::configure_realtime_thread(target_cpu, rt_status_for_process.clone());
                state.thread_configured = true;
            }

            // Fast-path: skip the 16-slot scan if no GC item was ever parked since last drain.
            // parking_lot_dirty is set Release by gc_cascade callers when parking/disposing an item.
            if parking_lot_dirty.load(Ordering::Acquire) {
                let mut any_remaining = false;
                for slot in parking_lot.iter_mut() {
                    let Some(old) = slot.take() else { continue };
                    if let Err(rtrb::PushError::Full(old_back)) = gc_producer.push(old) {
                        *slot = Some(old_back);
                        any_remaining = true;
                        break;
                    }
                }
                if !any_remaining {
                    parking_lot_dirty.store(false, Ordering::Release);
                }
            }

            rt_callback::drain_resamplers(
                &mut resampler_consumer,
                &mut state.resampler,
                &mut gc_producer,
                parking_lot,
                parking_lot_dirty,
                &gc_overflow_for_process,
                &rt_status_for_process,
            );

            rt_callback::drain_cabsims(
                &mut cabsim_consumer,
                &mut state.active_cabsim,
                &mut gc_producer,
                parking_lot,
                parking_lot_dirty,
                &gc_overflow_for_process,
                &rt_status_for_process,
            );

            let param_changed = rt_callback::receive_commands(
                &mut consumer,
                &mut state.model_input_mult_adj,
                &mut state.model_output_mult_adj,
                &mut state.current_nam_rate,
                &mut state.active_model_l,
                &mut state.active_model_r,
                &mut gc_producer,
                parking_lot,
                parking_lot_dirty,
                &gc_overflow_for_process,
                &rt_status_for_process,
                &mut state.user_input_gain_mult,
                &mut state.user_output_gain_mult,
                &mut state.gate_params,
                &mut state.threshold_open_sq,
                &mut state.threshold_close_sq,
                lut,
                &mut state.adaptive_compute,
            );

            rt_callback::try_slimmable_rebuild(&mut state.adaptive_compute, &rt_status_for_process);

            rt_callback::drain_slimmable_models(
                &mut state.slimmable_rx,
                &mut state.active_model_l,
                &mut state.active_model_r,
                &mut gc_producer,
                parking_lot,
                parking_lot_dirty,
                &gc_overflow_for_process,
                &rt_status_for_process,
            );

            rt_callback::drain_os_engines(
                &mut state.os_rx,
                &mut state.os_l,
                &mut state.os_r,
                &mut gc_producer,
                parking_lot,
                parking_lot_dirty,
                &gc_overflow_for_process,
                &rt_status_for_process,
            );

            let current_host_rate = rt_callback::sync_rate(
                &rate_for_process,
                &state.resampler,
                state.current_nam_rate,
                &rt_status_for_process,
            );

            if param_changed {
                rt_setup::compute_gain_multipliers(
                    state.user_input_gain_mult,
                    state.user_output_gain_mult,
                    state.model_input_mult_adj,
                    state.model_output_mult_adj,
                    &mut state.input_gain_mult,
                    &mut state.output_gain_mult,
                );
            }

            if rt_status_for_process
                .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING)
            {
                if rt_status_for_process.check_flag(
                    neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED,
                ) {
                    rt_status_for_process.clear_flag(
                        neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMP_SWAP_PENDING,
                    );
                    rt_status_for_process.clear_flag(
                        neural_amp_modeler_rs::common::spsc::RT_STATUS_RESAMPLER_REBUILD_FAILED,
                    );
                } else {
                    let _ = stream.dequeue_buffer();
                    return;
                }
            }

            rt_callback::process_dsp_buffer(
                stream,
                DspPipelineContext {
                    resampler: &mut state.resampler,
                    os_l: &mut state.os_l,
                    os_r: &mut state.os_r,
                    active_model_l: &mut state.active_model_l,
                    active_model_r: &mut state.active_model_r,
                    input_gain_mult: state.input_gain_mult,
                    output_gain_mult: state.output_gain_mult,
                    gate_params: &state.gate_params,
                    silence_hysteresis: &mut state.silence_hysteresis,
                    mono_hysteresis: &mut state.mono_hysteresis,
                    threshold_open_sq: state.threshold_open_sq,
                    threshold_close_sq: state.threshold_close_sq,
                    process_mono: &mut state.process_mono,
                    rt_status: &rt_status_for_process,
                    adaptive: &mut state.adaptive_compute,
                    bridge_writer: DspBridgeWriter::from_ref(bridge_ptr),
                    conv: state.active_cabsim.as_mut(),
                },
                DspBuffers {
                    resamp_mid_l: &mut *state.resamp_mid_l,
                    resamp_mid_r: &mut *state.resamp_mid_r,
                    resamp_out_l: &mut *state.resamp_out_l,
                    resamp_out_r: &mut *state.resamp_out_r,
                    model_out_l: &mut *state.model_out_l,
                    model_out_r: &mut *state.model_out_r,
                    os_in_l: &mut *state.os_in_l,
                    os_in_r: &mut *state.os_in_r,
                    os_model_l: &mut *state.os_model_l,
                    os_model_r: &mut *state.os_model_r,
                    crossfade_scratch_l: &mut *state.xfd_scratch_l,
                    crossfade_scratch_r: &mut *state.xfd_scratch_r,
                },
                current_host_rate,
                &mut state.frame_count,
                &rt_status_for_process,
                recording_producer,
                &mut state.recording_meta_sent,
                &mut state.recording_meta_rate,
                &mut state.recording_block,
                recording_data_available_ref,
            );

            // Sample PipeWire clock for drift diagnostics (every 64 frames)
            if (state.frame_count.wrapping_sub(1) & 0x3F) == 0
                && let Ok(pw_time) = stream.time()
            {
                rt_status_for_process
                    .capture_host_now
                    .store(pw_time.now(), Ordering::Relaxed);
                rt_status_for_process
                    .capture_host_ticks
                    .store(pw_time.ticks(), Ordering::Relaxed);
                rt_status_for_process
                    .capture_host_delay
                    .store(pw_time.delay(), Ordering::Relaxed);
            }

            if (state.frame_count.wrapping_sub(1) & 0xF) == 0 {
                let elapsed_nanos = rt_status_for_process.dsp_cycle_time.load(Ordering::Relaxed);
                if elapsed_nanos > 0 {
                    let n_samples =
                        rt_status_for_process.last_n_samples.load(Ordering::Relaxed) as u64;
                    if n_samples > 0 && current_host_rate > 0 {
                        let budget_ns = n_samples * 1_000_000_000 / current_host_rate as u64;
                        let latency_us = elapsed_nanos / 1000;
                        let budget_us = budget_ns / 1000;
                        state.adaptive_compute.update(
                            latency_us,
                            budget_us,
                            current_host_rate,
                            &rt_status_for_process,
                        );
                    }
                }
            }

            // Detect cabsim partition mismatch and signal rebuild
            if let Some(ref cabsim) = state.active_cabsim {
                let n_samples =
                    rt_status_for_process.last_n_samples.load(Ordering::Relaxed) as usize;
                if n_samples > 0
                    && cabsim.partition_size() != n_samples
                    && state.ir_raw_samples.is_some()
                {
                    // Write data BEFORE raising the flag (Release barrier must cover
                    // the preceding store so the consumer sees the correct value).
                    rt_status_for_process
                        .requested_cabsim_partition_size
                        .store(n_samples as u32, Ordering::Relaxed);
                    rt_status_for_process.set_flag_release(
                        neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_CABSIM_REBUILD,
                    );
                }
            }
        })
        .register()?;

    let mut format_buf = [0u8; 1024];
    let format_pod = build_capture_format_pod(&mut format_buf)?;

    capture_stream.connect(
        pw::spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS
            | pw::stream::StreamFlags::EXCLUSIVE,
        &mut [format_pod],
    )?;

    log::info!(
        "{} Capture stream connected to PipeWire (Audio/Sink, F32P Planar Stereo).",
        "\u{1f3bc}".bright_blue()
    );

    Ok((capture_stream, capture_listener))
}

#[cfg(test)]
#[path = "setup_test.rs"]
mod setup_test;
