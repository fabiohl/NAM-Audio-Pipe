// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::super::state::{CaptureState, RtHostChannels};
use crate::recording::transport::RecordingSender;
use crate::standalone::pw_host::SharedBackendStatus;
use crate::standalone::pw_host::rt_callback;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{GcItem, RtStatusFlags};
use neural_amp_modeler_rs::dsp::pipeline::{BridgeRef, DspBridgeWriter};

use pipewire as pw;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32};

#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn setup_capture_stream<'c>(
    core: &'c pw::core::Core,
    bridge_ptr: BridgeRef,
    buffer_size: u32,
    initial_latency_samples: u32,
    target_cpu: usize,
    state_ptr: *mut CaptureState,
    channels_ptr: *mut RtHostChannels,
    rate_for_param: Arc<AtomicU32>,
    rt_status: Arc<RtStatusFlags>,
    recording_sender_ptr: *mut RecordingSender,
    parking_lot_ptr: *mut [Option<GcItem>; 16],
    parking_lot_dirty_ptr: *const AtomicBool,
    recording_failed: Option<Arc<AtomicBool>>,
    backend_status: Arc<SharedBackendStatus>,
) -> anyhow::Result<(pw::stream::StreamBox<'c>, pw::stream::StreamListener<()>)> {
    let capture_props = super::properties::create_capture_properties(buffer_size);

    let mut capture_stream = pw::stream::StreamBox::new(
        core,
        crate::standalone::pw_host::identity::PW_CAPTURE_STREAM_NAME,
        capture_props,
    )?;

    let rt_status_for_listener = rt_status.clone();
    let rt_status_for_process = rt_status;
    let stream_status_for_process = backend_status.stream_status().clone();
    let backend_for_state = backend_status.clone();
    let backend_for_params = backend_status;

    let lut = neural_amp_modeler_rs::math::dsp::gain_lut::get_gain_lut();

    // Last callback counter observed by the adaptive cadence guard. Lives per
    // stream instance; a reconnect starts a fresh measurement window.
    let mut adaptive_prev_frame_count: u32 = 0;

    let capture_listener = capture_stream
        .add_local_listener::<()>()
        .state_changed(move |_stream, _user_data, old, new| {
            super::super::listeners::state_changed_handler(old, new, &backend_for_state)
        })
        .param_changed(move |stream, user_data, id, param| {
            super::super::listeners::param_changed_handler(
                stream,
                user_data,
                id,
                param,
                &rate_for_param,
                &rt_status_for_listener,
                &backend_for_params,
            )
        })
        .process(move |stream: &pw::stream::Stream, _info| {
            // Panic containment: the whole callback body runs under `catch_unwind`
            // (one guard per quantum, outside the inner sample loops). A panic
            // inside the RT callback is contained here — it never reaches the
            // `pipewire` crate's `extern "C"` trampoline (which would `abort`)
            // — and raises the fatal `RT_STATUS_PANIC_CAPTURED` latch for the
            // main control loop to run the ordered teardown.
            rt_callback::run_rt_callback_body(
                &rt_status_for_process,
                std::panic::AssertUnwindSafe(|| {
                    // SAFETY: state_ptr points to the `Box<CaptureState>` owned by
                    // run_pipewire_host. The pointer is re-derived at the top of each
                    // `'host` iteration, after the owner's pre-stream writes and while
                    // the previous stream/loop are already destroyed; it is therefore
                    // unique to this stream instance. While this loop runs the RT
                    // callback is the sole accessor; the main thread touches the state
                    // only before `thread_loop.start()` and after `thread_loop.stop()`,
                    // and never uses a pointer carried over from a previous instance.
                    let state = unsafe { &mut *state_ptr };
                    let t_cap_start = rt_setup::rdtsc_nanos();
                    // SAFETY: channels_ptr points to the `Box<RtHostChannels>` owned
                    // by run_pipewire_host — same per-instance provenance and
                    // exclusive-access contract as state_ptr.
                    let channels = unsafe { &mut *channels_ptr };
                    // SAFETY: recording_sender_ptr points to the `RecordingSender`
                    // owned by `run_pipewire_host` (the recording worker guard's
                    // sender slot, or the stack-local fallback), which outlives this
                    // closure. The RT thread is its only writer; after
                    // `thread_loop.stop()` the shutdown path takes sequential
                    // ownership, so no concurrent access occurs.
                    let recording_sender = unsafe { &mut *recording_sender_ptr };
                    let recording_failed_ref = recording_failed.as_deref();
                    // SAFETY: parking_lot_ptr and parking_lot_dirty_ptr point to slots in
                    // run_pipewire_host that outlive this closure. They are re-derived
                    // per `'host` iteration (same provenance contract as state_ptr), so
                    // no pointer from a previous instance outlives its teardown. While
                    // the loop runs, the RT callback is the sole writer; after
                    // thread_loop.stop() the main thread takes single-owner handoff
                    // (`&mut rt_parking_lot`) and drains the 16 slots off-RT. The
                    // periodic drain in run_pipewire_host NEVER touches this slot —
                    // that would race with the RT flush below.
                    let parking_lot = unsafe { &mut *parking_lot_ptr };
                    // SAFETY: `parking_lot_dirty_ptr` shares the same single-owner
                    // handoff contract as `parking_lot_ptr` above; the RT callback is
                    // the sole writer while the loop runs and the main thread drains
                    // it off-RT only after `thread_loop.stop()`.
                    let parking_lot_dirty = unsafe { &*parking_lot_dirty_ptr };

                    // RT thread setup: performed on the first invocation of `process` on the
                    // actual PipeWire data-loop thread. Sets DAZ/FTZ, thread naming, core affinity,
                    // and scheduler inspection directly on the audio thread.
                    if !state.thread_configured {
                        super::listener::configure_rt_thread_once(
                            state,
                            target_cpu,
                            &rt_status_for_process,
                        );
                    }

                    let outcome = rt_callback::rt_quantum_prologue(
                        state,
                        channels,
                        parking_lot,
                        parking_lot_dirty,
                        &rt_status_for_process,
                        lut,
                    );
                    let rt_callback::PrologueOutcome::Proceed {
                        current_host_rate, ..
                    } = outcome
                    else {
                        let _ = stream.dequeue_buffer();
                        return;
                    };

                    let cabsim_rate = state.active_cabsim.as_deref().map(|pair| pair.sample_rate);
                    let nam_cab_rate = state.active_cab_nam.as_ref().map(|slot| slot.host_rate);
                    stream_status_for_process.note_cab_rate_stale(
                        super::listener::cab_rate_diverged(
                            current_host_rate,
                            cabsim_rate,
                            nam_cab_rate,
                        ),
                    );
                    let rt_view = state.split_rt();
                    let plan = rt_callback::assemble_rt_dsp(
                        rt_view,
                        &rt_status_for_process,
                        // SAFETY: the PipeWire capture callback is the sole SPSC producer on `bridge_ptr`.
                        unsafe { DspBridgeWriter::from_ref(bridge_ptr) },
                        current_host_rate,
                    );
                    rt_callback::process_dsp_buffer(
                        stream,
                        plan,
                        &stream_status_for_process,
                        recording_sender,
                        recording_failed_ref,
                        t_cap_start,
                    );

                    rt_callback::capture_epilogue(
                        stream,
                        state,
                        &stream_status_for_process,
                        &rt_status_for_process,
                        current_host_rate,
                        &mut adaptive_prev_frame_count,
                    );
                }),
            );
        })
        .register()?;

    super::connect::connect_capture_stream(&mut capture_stream, initial_latency_samples)?;

    Ok((capture_stream, capture_listener))
}
