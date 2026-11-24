// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire host execution — dual-stream topology setup, DSP bridge allocation,
//! CPU affinity locking, main control loop, bounded backend reconnection
//! and graceful shutdown.

use super::capture::state::{CaptureState, RtHostChannels};
use super::handlers;
use super::observe_rt_panic;
use super::output_pw::AppState;
use super::reconnect::{ReconnectCycle, ReconnectPolicy, STABLE_RESET, StabilityEvent};
use super::run_lifecycle::{
    RetryDecision, finalize_host_shutdown, handle_attempt_error, reset_bridge_and_drain_gc,
    run_control_housekeeping, sleep_interruptible, snapshot_and_publish_initial_latencies,
};
use super::wakeup::ControlPlaneWakeup;
use super::{PipewireHostConfig, SharedBackendStatus};
use super::{bridge, capture, identity, latency, playback};
use crate::recording::alert::RecordingAlert;
use crate::recording::guard::{RecordingWorkerGuard, RecordingWorkerOutcome};
use crate::recording::transport::RecordingSender;
use crate::standalone::colors::Colorize;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, ParamPayload, ResamplerSwapPayload, RtStatusFlags, SHUTDOWN,
    SlimModelPair,
};
use pipewire as pw;
use rtrb::Consumer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Optional neural cabinet and bypass configuration passed to [`run_pipewire_host_with_cabinet`].
#[derive(Default)]
pub struct HostCabinetOptions {
    /// Loaded neural cabinet asset bytes/metadata for host-rate renegotiation off-RT.
    pub initial_nam_cab: Option<crate::standalone::cabinet::InitialCabSetup>,
    /// Unboxed SPSC ring for neural cabinet slot swaps/rate recalibration.
    pub cab_nam_channels: Option<(
        rtrb::Producer<crate::standalone::cabinet::PipeCabNamPayload>,
        rtrb::Consumer<crate::standalone::cabinet::PipeCabNamPayload>,
    )>,
    /// Initial cabinet bypass state (`--cab-bypass`).
    pub cab_bypass: bool,
}

/// Runs the full PipeWire host loop with the canonical cabinetless / WAV IR contract.
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn run_pipewire_host(
    consumer: Consumer<ParamPayload>,
    gc_producer: rtrb::Producer<GcItem>,
    gc_overflow: Arc<GcOverflowBuffer>,
    resampler_consumer: Consumer<Box<ResamplerSwapPayload>>,
    resampler_producer: rtrb::Producer<Box<ResamplerSwapPayload>>,
    cabsim_consumer: Consumer<Box<neural_amp_modeler_rs::common::spsc::CabSimSwapPayload>>,
    cabsim_producer: rtrb::Producer<Box<neural_amp_modeler_rs::common::spsc::CabSimSwapPayload>>,
    rt_status: Arc<RtStatusFlags>,
    config: PipewireHostConfig,
    gc_consumer: Consumer<GcItem>,
    slimmable_consumer: Consumer<Box<SlimModelPair>>,
    os_consumer: Consumer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>,
    recording_data_available: Option<Arc<AtomicBool>>,
    recording_worker: Option<RecordingWorkerGuard>,
) -> anyhow::Result<Option<RecordingWorkerOutcome>> {
    run_pipewire_host_with_cabinet(
        consumer,
        gc_producer,
        gc_overflow,
        resampler_consumer,
        resampler_producer,
        cabsim_consumer,
        cabsim_producer,
        rt_status,
        config,
        gc_consumer,
        slimmable_consumer,
        os_consumer,
        recording_data_available,
        recording_worker,
        HostCabinetOptions::default(),
    )
}

/// Runs the full PipeWire host loop with single-slot cabinet (WAV or 1x1 NAM/NAMB) and bypass support.
///
/// Architecture: Apps → [Capture: Audio/Sink] → process(DSP) → DspBridge → [Playback: Stream/Output] → Hardware.
/// Both streams share `node.group` / `node.link-group`, executing synchronously on the PipeWire
/// driver loop with zero extra quantum latency. DSP state and SPSC channels survive bounded
/// daemon reconnect cycles via heap `Box`es reached through raw pointers.
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn run_pipewire_host_with_cabinet(
    consumer: Consumer<ParamPayload>,
    gc_producer: rtrb::Producer<GcItem>,
    gc_overflow: Arc<GcOverflowBuffer>,
    resampler_consumer: Consumer<Box<ResamplerSwapPayload>>,
    mut resampler_producer: rtrb::Producer<Box<ResamplerSwapPayload>>,
    cabsim_consumer: Consumer<Box<neural_amp_modeler_rs::common::spsc::CabSimSwapPayload>>,
    mut cabsim_producer: rtrb::Producer<
        Box<neural_amp_modeler_rs::common::spsc::CabSimSwapPayload>,
    >,
    rt_status: Arc<RtStatusFlags>,
    config: PipewireHostConfig,
    mut gc_consumer: Consumer<GcItem>,
    slimmable_consumer: Consumer<Box<SlimModelPair>>,
    os_consumer: Consumer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>,
    recording_data_available: Option<Arc<AtomicBool>>,
    mut recording_worker: Option<RecordingWorkerGuard>,
    mut cabinet: HostCabinetOptions,
) -> anyhow::Result<Option<RecordingWorkerOutcome>> {
    let PipewireHostConfig {
        buffer_size,
        cabsim_partition,
        sys,
        ir_raw_samples,
        ir_source_rate,
        full_wavenet_model_l,
        full_wavenet_model_r,
        has_model_r,
        mut slimmable_producer,
        mut os_producer,
        oversample,
        cpu_receipt,
        fail_fast,
        reconnect_timeout,
        gate_config,
        gate_mode,
        stream_status,
        recording_status,
        banner,
        activation,
        ir_level_policy,
        output_limit,
    } = config;

    let reconnect_policy = if fail_fast {
        ReconnectPolicy::fail_fast()
    } else {
        match reconnect_timeout {
            Some(timeout) => ReconnectPolicy::with_backoff_timeout(timeout),
            None => ReconnectPolicy::production(),
        }
    };

    let mut recording_alert = recording_status.as_ref().map(|_| RecordingAlert::new());
    let recording_enabled = recording_worker.is_some();

    let bridge_ptr = bridge::allocate_dsp_bridge();
    // SAFETY: `bridge_ptr` points to the heap-immortal `DspBridge` just allocated;
    // no stream (and therefore no RT Writer/Reader) exists yet.
    unsafe { &mut *bridge_ptr.as_ptr() }.prime_silence(buffer_size.max(1) as usize);

    let mut rt_state = Box::new(CaptureState::init(&sys, oversample, gate_config));
    rt_state.configure_gate_mode(gate_mode);
    log::info!(
        "Gate topology: {} (threshold open: {}).",
        gate_mode.as_str(),
        match gate_config {
            crate::standalone::cli::GateConfig::Off => "OFF".to_string(),
            crate::standalone::cli::GateConfig::Threshold {
                threshold_open_db, ..
            } => format!("{threshold_open_db:.1} dB"),
        }
    );
    rt_state.activation = activation;
    rt_state.ir_raw_samples = ir_raw_samples.clone();
    rt_state.ir_source_rate = ir_source_rate;
    let (mut cab_nam_producer, cab_nam_consumer) = match cabinet.cab_nam_channels.take() {
        Some((p, c)) => (Some(p), Some(c)),
        None => (None, None),
    };
    let nam_cab_source = match cabinet.initial_nam_cab.take() {
        Some(crate::standalone::cabinet::InitialCabSetup::Nam {
            raw_bytes,
            kind,
            display_name,
            ..
        }) => {
            rt_state.has_nam_cab = true;
            Some((raw_bytes, kind, display_name))
        }
        _ => None,
    };
    rt_state.cab_nam_consumer = cab_nam_consumer;
    rt_state
        .cab_bypass
        .store(cabinet.cab_bypass, Ordering::Release);
    rt_state.cab_bypass_state = crate::standalone::cabinet::CabBypassState::new(cabinet.cab_bypass);
    rt_state.cabsim_partition_policy = cabsim_partition as usize;
    rt_state.resampler_drain = Some(super::rt_callback::resampler_swap_drain(resampler_consumer));
    rt_state.cabsim_drain = Some(super::rt_callback::cabsim_swap_drain(cabsim_consumer));
    rt_state.slimmable_drain = Some(super::rt_callback::slimmable_swap_drain(slimmable_consumer));
    rt_state.os_drain = Some(super::rt_callback::os_swap_drain(os_consumer));
    let os_scratch_len = rt_state.os_in_l.len();
    let state_ptr: *mut CaptureState = &raw mut *rt_state;

    let mut rt_channels = Box::new(RtHostChannels {
        param_consumer: consumer,
        gc_producer,
        gc_overflow: gc_overflow.clone(),
    });
    let channels_ptr: *mut RtHostChannels = &raw mut *rt_channels;
    let rate_for_param = rt_state.shared_target_rate.clone();

    let mut dummy_recording_sender = RecordingSender::none();
    let rec_ptr: *mut RecordingSender = match &mut recording_worker {
        Some(guard) => &raw mut *guard.sender_slot(),
        None => &raw mut dummy_recording_sender,
    };

    let mut rt_parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_ptr: *mut [Option<GcItem>; 16] = &raw mut rt_parking_lot;
    let rt_parking_lot_dirty = AtomicBool::new(false);
    let parking_lot_dirty_ptr: *const AtomicBool = &raw const rt_parking_lot_dirty;

    let target_cpu = cpu_receipt.selected_cpu;
    let _cpu_dma_lock = rt_setup::lock_cpu_c_states();

    let wakeup = ControlPlaneWakeup::new();
    let stream_status = stream_status.unwrap_or_else(|| Arc::new(super::StreamStatusFlags::new()));
    let mut backend_init =
        SharedBackendStatus::with_rt_and_stream_status(rt_status.clone(), stream_status);
    backend_init.bind_wakeup(wakeup.clone());
    let backend_status = Arc::new(backend_init);
    rt_state.stream_status = Some(Arc::clone(backend_status.stream_status()));
    let backend_for_capture = backend_status.clone();
    let backend_for_playback = backend_status.clone();
    let recording_failed = recording_worker
        .as_ref()
        .and_then(RecordingWorkerGuard::failed_flag)
        .cloned();

    let mut reconnect = ReconnectCycle::new(reconnect_policy);
    let mut backend_failure: Option<(&'static str, String)> = None;
    let mut rebuild_failures = handlers::RebuildFailureTracker::default();
    let mut teardown_guard: Option<super::status::TeardownGuard<'_>> = None;
    let mut banner_emitted = false;

    'host: loop {
        rt_state.thread_configured = false;

        // SAFETY: `pipewire::init()` was called in `main()`, `PW_THREAD_LOOP_NAME` is
        // static null-terminated, and the loop is stopped & dropped at instance teardown.
        let thread_loop = unsafe {
            pipewire::thread_loop::ThreadLoopBox::new(Some(identity::PW_THREAD_LOOP_NAME), None)
        }?;
        let context_props = pw::properties::properties! {
            "config.name" => "client.conf",
        };
        let context = pipewire::context::ContextBox::new(thread_loop.loop_(), Some(context_props))?;
        let core = match context.connect(None) {
            Ok(core) => core,
            Err(e) => match handle_attempt_error(
                e,
                "connect",
                "core",
                &sys,
                &mut reconnect,
                &backend_status,
            )? {
                RetryDecision::Retry => continue 'host,
                RetryDecision::BreakHost(failure) => {
                    backend_failure = failure;
                    break 'host;
                }
            },
        };

        let (
            capture_stream,
            capture_listener,
            playback_stream,
            playback_listener,
            initial_latency_samples,
        ) = {
            let setup_res: anyhow::Result<_> = (|| {
                let _lock = thread_loop.lock();
                let latency_str = format!("{}/48000", buffer_size);
                // SAFETY: both PipeWire loops are stopped here, so `state_ptr` is exclusive.
                let initial_latency_samples =
                    unsafe { snapshot_and_publish_initial_latencies(state_ptr, &backend_status) };

                let (cs, cl) = capture::setup_capture_stream(
                    &core,
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
                    recording_data_available.clone(),
                    recording_failed.clone(),
                    backend_for_capture.clone(),
                )?;

                let (ps, pl) = playback::setup_playback_stream(
                    &core,
                    bridge_ptr,
                    buffer_size,
                    initial_latency_samples,
                    &latency_str,
                    rt_status.clone(),
                    backend_for_playback.clone(),
                    output_limit,
                )?;

                Ok((cs, cl, ps, pl, initial_latency_samples))
            })();

            match setup_res {
                Ok(res) => res,
                Err(e) => match handle_attempt_error(
                    e,
                    "stream-setup",
                    "stream_setup",
                    &sys,
                    &mut reconnect,
                    &backend_status,
                )? {
                    RetryDecision::Retry => continue 'host,
                    RetryDecision::BreakHost(failure) => {
                        backend_failure = failure;
                        break 'host;
                    }
                },
            }
        };

        let app_state = AppState {
            capture_listener,
            capture_stream,
            playback_listener,
            playback_stream,
        };
        drop(teardown_guard.take());
        sys.emit_irq_advisory(target_cpu);

        let housekeeping_for_thread_loop = cpu_receipt.housekeeping_cpus.clone();
        let thread_loop_pin = thread_loop.loop_().add_event(move || {
            let _ = rt_setup::apply_housekeeping_affinity(&housekeeping_for_thread_loop);
        });
        thread_loop.start();
        thread_loop_pin.signal();

        let mut latency_tracker =
            latency::LatencyPublicationTracker::new(initial_latency_samples, 48_000);
        let mut was_silent = false;
        let mut was_fading = false;
        let mut poll_state = rt_setup::PollState::with_cpu_receipt(Some(cpu_receipt.clone()));
        let mut instance_failure: Option<(&'static str, String)> = None;
        let mut panic_is_terminal = false;

        while !SHUTDOWN.load(Ordering::Acquire) {
            if let (Some(alert), Some(status)) = (&mut recording_alert, &recording_status)
                && let Some(emitted) = alert.poll(status)
            {
                log::error!("{}", emitted.render());
            }

            if let Some((stream, reason)) = backend_status.failure() {
                instance_failure = Some((stream, reason));
                break;
            }

            if observe_rt_panic(&rt_status, &backend_status) {
                panic_is_terminal = true;
                instance_failure = Some((
                    "rt_callback",
                    "panic captured in an RT callback closure (contained — ordered teardown follows)"
                        .to_owned(),
                ));
                break;
            }

            if backend_status.is_running() {
                match reconnect.note_stable() {
                    StabilityEvent::Restored { attempts } => {
                        log::info!("🎸 Audio restored after {attempts} attempt(s)");
                    }
                    StabilityEvent::Replenished => {
                        log::debug!(
                            "Reconnect budget replenished after {STABLE_RESET:?} of stable streaming"
                        );
                    }
                    StabilityEvent::None => {}
                }
            } else {
                reconnect.note_unstable();
            }

            run_control_housekeeping(
                &rt_status,
                &rate_for_param,
                &sys,
                &mut resampler_producer,
                nam_cab_source.as_ref(),
                cab_nam_producer.as_mut(),
                ir_raw_samples.as_deref(),
                ir_source_rate,
                ir_level_policy,
                &mut cabsim_producer,
                &backend_status,
                buffer_size,
                &mut latency_tracker,
                &thread_loop,
                &app_state,
                full_wavenet_model_l.as_deref(),
                full_wavenet_model_r.as_deref(),
                has_model_r,
                &mut slimmable_producer,
                &mut os_producer,
                &mut rebuild_failures,
                os_scratch_len,
                bridge_ptr,
                &mut was_silent,
                &mut was_fading,
                &mut poll_state,
                &mut banner_emitted,
                &banner,
                &mut gc_consumer,
                &gc_overflow,
            );

            wakeup.wait_timeout(Duration::from_millis(100));
        }

        if instance_failure.is_none()
            && rt_status
                .check_flag(crate::standalone::pw_host::rt_callback::RT_STATUS_PANIC_CAPTURED)
        {
            panic_is_terminal = true;
            instance_failure = Some((
                "rt_callback",
                "panic captured in an RT callback closure (observed at shutdown — never a clean exit)"
                    .to_owned(),
            ));
        }

        teardown_guard = Some(backend_status.enter_teardown());
        thread_loop.stop();
        drop(app_state);
        drop(core);
        drop(context);
        drop(thread_loop_pin);
        drop(thread_loop);
        reset_bridge_and_drain_gc(
            &backend_status,
            bridge_ptr,
            buffer_size,
            &mut gc_consumer,
            &gc_overflow,
            &mut rt_parking_lot,
            &rt_status,
        );

        match instance_failure {
            None => break 'host,
            Some((stream, reason)) => {
                if panic_is_terminal {
                    backend_failure = Some((stream, reason));
                    break 'host;
                }
                if let Some(backoff) = reconnect.begin_attempt() {
                    log::warn!(
                        "{} PipeWire backend lost the '{stream}' stream ({reason}). \
                         Reconnect attempt {}/{} in {:?} (total backoff budget {:?}) — \
                         DSP state (models/IRs/recording) is preserved.",
                        "🔁".yellow(),
                        reconnect.attempts_made(),
                        reconnect.policy().max_attempts,
                        backoff,
                        reconnect.policy().total_backoff_budget(),
                    );
                    backend_status.begin_reconnect(
                        reconnect.attempts_made(),
                        reconnect.policy().max_attempts,
                        backoff,
                    );
                    if sleep_interruptible(backoff) {
                        break 'host;
                    }
                    continue 'host;
                }
                backend_failure = Some((stream, reason));
                break 'host;
            }
        }
    }

    finalize_host_shutdown(
        backend_failure,
        recording_enabled,
        recording_worker.take(),
        &backend_status,
        &rt_state,
        &rt_status,
        buffer_size,
        &sys,
        &reconnect,
        banner_emitted,
        &banner,
    )
}
