// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire host execution — dual-stream topology setup, DSP bridge allocation,
//! CPU affinity locking, main control loop, bounded backend reconnection
//! (F-RB-010 / T4.5) and graceful shutdown.

use super::SharedBackendStatus;
use super::capture::state::{CaptureState, RtHostChannels};
use super::handlers;
use super::output_pw::AppState;
use super::reconnect::{ReconnectCycle, ReconnectPolicy};
use crate::recording::buffer::{MAX_BLOCK_SIZE, RingPayload};
use crate::recording::guard::{RecordingWorkerGuard, RecordingWorkerOutcome};
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::diagnostics::{NamDiagnostic, NamErrorCode};
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, ParamPayload, ResamplerSwapPayload, RtStatusFlags, SHUTDOWN,
    SlimModelPair,
};
use rtrb::Consumer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::PipewireHostConfig;
use super::bridge;
use super::capture;
use super::identity;
use super::playback;
use crate::standalone::colors::Colorize;

/// Initializes the PipeWire dual-stream topology (Capture + Playback).
///
/// Architecture: Apps → [Capture: Audio/Sink] → process(DSP) → DspBridge → [Playback: Stream/Output] → Hardware.
/// The monitor port of `Audio/Sink` copies the buffer *before* `process()` — therefore, the only
/// way to deliver the processed audio to hardware is via a second playback stream
/// that reads from `DspBridge` post-DSP.
///
/// ## SPSC channel parameters
///
/// - `consumer`: Consumer of the CLI→DSP parameter channel (gain, model, etc.).
/// - `gc_producer`: Producer of the GC channel for drop-delegation of obsolete models.
/// - `resampler_consumer`: Dedicated channel for receiving pre-built resamplers
///   from the main thread — **zero allocations in the RT callback**.
/// - `resampler_producer`: Producer of the resampler channel — the main thread
///   builds `NamResampler::new().expect("construction should succeed for test-sized buffers")` here (allocation outside RT) and sends to the callback.
/// - `rt_status`: Atomic flags for silent RT→Main communication.
/// - `recording_worker`: RAII custody of the recording I/O thread and its ring
///   producer (F-RB-009 / T3.5). The guard keeps the worker alive across every
///   early `?` return (its `Drop` signals termination and joins bounded) and,
///   on the normal shutdown path, `RecordingWorkerGuard::shutdown` returns the
///   observable join outcome so recording failures propagate to the process
///   exit code.
///
/// ## Bounded reconnect (F-RB-010 / T4.5)
///
/// The PipeWire daemon may be restarted by the package manager, by the user or
/// by a USB interface reconnect. Instead of aborting immediately, the host
/// re-instantiates the streams inside a **strictly bounded** retry cycle
/// ([`ReconnectCycle`], default 3 attempts with progressive 250/500/1000 ms
/// exponential backoff — disabled under `--fail-fast` or `max_attempts == 0`).
/// The DSP state ([`CaptureState`]: models, resampler, cab-sim, gains) and the
/// RT-side SPSC channels ([`RtHostChannels`]) live in heap `Box`es reached via
/// raw pointers, so **no internal state is lost** across a re-instantiation —
/// audio resumes with the same models/IRs/recorder. When the budget is
/// exhausted the host falls back to the T4.4 fail-fast teardown (RT loop stop,
/// GC drain, recording shutdown, non-zero exit).
///
/// Returns the recording worker outcome when `--record` was used (`None`
/// otherwise), so the caller can turn a failed/panicked/timed-out recording
/// into a non-zero process exit.
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn run_pipewire_host(
    consumer: Consumer<ParamPayload>,
    gc_producer: rtrb::Producer<GcItem>,
    gc_overflow: Arc<GcOverflowBuffer>,
    resampler_consumer: Consumer<Box<ResamplerSwapPayload>>,
    mut resampler_producer: rtrb::Producer<Box<ResamplerSwapPayload>>,
    cabsim_consumer: Consumer<Option<Box<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair>>>,
    mut cabsim_producer: rtrb::Producer<
        Option<Box<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimPair>>,
    >,
    rt_status: Arc<RtStatusFlags>,
    config: PipewireHostConfig,
    mut gc_consumer: Consumer<GcItem>,
    slimmable_consumer: Consumer<Box<SlimModelPair>>,
    os_consumer: Consumer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>,
    recording_data_available: Option<Arc<AtomicBool>>,
    mut recording_worker: Option<RecordingWorkerGuard>,
) -> anyhow::Result<Option<RecordingWorkerOutcome>> {
    let PipewireHostConfig {
        buffer_size,
        sys,
        ir_raw_samples,
        ir_source_rate,
        full_wavenet_model,
        has_model_r,
        mut slimmable_producer,
        mut os_producer,
        oversample,
        fail_fast,
    } = config;

    // F-RB-010 / T4.5: bounded reconnect policy. `--fail-fast` disables the
    // recovery cycle entirely; the production default allows 3 attempts with
    // progressive backoff.
    let reconnect_policy = if fail_fast {
        ReconnectPolicy::fail_fast()
    } else {
        ReconnectPolicy::production()
    };

    // =========================================================
    // 1. PERSISTENT HOST RESOURCES (survive reconnect attempts)
    // =========================================================
    // DSP state and RT-side channels live in heap Boxes reached through raw
    // pointers, so a bounded reconnect (F-RB-010 / T4.5) can re-instantiate
    // the streams without losing the models, IRs, resampler, gains or the
    // SPSC wiring. The main thread never aliases the pointed-to objects while
    // the RT callback runs — it touches them only before `thread_loop.start()`
    // and after `thread_loop.stop()`.
    let bridge_ptr = bridge::allocate_dsp_bridge();

    let mut rt_state = Box::new(CaptureState::init(&sys, oversample));
    rt_state.ir_raw_samples = ir_raw_samples.clone();
    rt_state.ir_source_rate = ir_source_rate;
    rt_state.slimmable_rx = Some(slimmable_consumer);
    rt_state.os_rx = Some(os_consumer);
    let state_ptr: *mut CaptureState = &raw mut *rt_state;

    let mut rt_channels = Box::new(RtHostChannels {
        param_consumer: consumer,
        gc_producer,
        gc_overflow: gc_overflow.clone(),
        resampler_consumer,
        cabsim_consumer,
    });
    let channels_ptr: *mut RtHostChannels = &raw mut *rt_channels;

    // The param handler stores the negotiated rate into this Arc (a clone of
    // `state.shared_target_rate`); the RT callback reads the same atomic via
    // `sync_rate` (rt_callback/rate_sync.rs).
    let rate_for_param = rt_state.shared_target_rate.clone();

    let full_wavenet_model = full_wavenet_model;

    // Place the recording producer (owned by the worker guard — RAII custody,
    // F-RB-009 / T3.5) on a stack slot so the RT closure can access it via a
    // raw pointer without locking. Producer is not cloneable; a raw pointer
    // avoids shared-ownership plumbing while respecting the SPSC contract
    // (single writer at a time). When recording is disabled the pointer
    // targets a never-written dummy slot — the RT callback dereferences it
    // unconditionally.
    let mut dummy_recording_slot: Option<rtrb::Producer<RingPayload<MAX_BLOCK_SIZE>>> = None;
    let rec_ptr: *mut Option<rtrb::Producer<RingPayload<MAX_BLOCK_SIZE>>> =
        match &mut recording_worker {
            Some(guard) => &raw mut *guard.producer_slot(),
            None => &raw mut dummy_recording_slot,
        };

    // R-04: the RT parking lot (16 slots) lives HERE — a stack-local slot in
    // the main thread, accessed by the RT callback through a raw pointer
    // (same contract as `rec_ptr`: the slot outlives the closure). While the
    // loop runs, the RT callback is the sole writer. After `thread_loop.stop()`
    // the main thread becomes single owner and the final drain releases the
    // 16 slots off-RT via `drain_gc_channels` — never on the audio thread.
    //
    // H-01 (B-01): `rt_parking_lot_dirty` avoids scanning all 16 slots in steady
    // state when no swaps occurred. It is set (Release) whenever an item cascades
    // to GC, and reset (Release) when the RT drain finishes emptying all slots.
    let mut rt_parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_ptr: *mut [Option<GcItem>; 16] = &raw mut rt_parking_lot;
    let rt_parking_lot_dirty = std::sync::atomic::AtomicBool::new(false);
    let parking_lot_dirty_ptr: *const std::sync::atomic::AtomicBool =
        &raw const rt_parking_lot_dirty;

    // =========================================================
    // 2. CORE OPTIMIZATION (CPU Affinity)
    // =========================================================
    let target_cpu = rt_setup::select_optimal_cpu().unwrap_or(0);

    // =========================================================
    // 3. PROTECTED CONFIGURATION SCOPE (RAII)
    // =========================================================
    // Shared backend state machine (F-RB-010 / T4.4): the capture and playback
    // stream state listeners mark it `Failed` on a fatal connectivity loss and
    // the main control loop below polls it every iteration — the process never
    // survives as a functionally-dead zombie. F-RB-010 / T4.5: a failure with
    // reconnect budget left transitions to `Reconnecting` and re-instantiates
    // the streams; only a budget exhaustion triggers the fail-fast exit.
    let backend_status = Arc::new(SharedBackendStatus::new());
    let backend_for_capture = backend_status.clone();
    let backend_for_playback = backend_status.clone();
    // The RT-observable failure flag lives in the worker guard; clone it here
    // for the capture stream callback (F-RB-009 / T3.3).
    let recording_failed = recording_worker
        .as_ref()
        .and_then(RecordingWorkerGuard::failed_flag)
        .cloned();

    // Bounded reconnect cycle (F-RB-010 / T4.5). One cycle per session: the
    // budget is never reset, so the recovery phase is strictly bounded in
    // number of attempts and total time by construction.
    let mut reconnect = ReconnectCycle::new(reconnect_policy);
    // Set when the control loop observes a fatal backend failure AND the
    // reconnect budget is exhausted. Drives the fail-fast teardown + `Err`
    // return below.
    let mut backend_failure: Option<(&'static str, String)> = None;

    // =========================================================
    // 4. HOST INSTANCE LOOP (one per bounded-reconnect attempt)
    // =========================================================
    'host: loop {
        // Each instance spawns a fresh PipeWire RT data thread: real-time
        // setup (DAZ/FTZ, SCHED_FIFO, CPU affinity) must re-run in its first
        // callback. The previous instance (if any) already stopped its loop,
        // so the main thread is the sole owner of the DSP state here.
        rt_state.thread_configured = false;

        // 4.1 PIPEWIRE LOOP INITIALIZATION (fresh per attempt)
        let thread_loop = unsafe {
            pipewire::thread_loop::ThreadLoopBox::new(Some(identity::PW_THREAD_LOOP_NAME), None)
        }?;
        let context = pipewire::context::ContextBox::new(thread_loop.loop_(), None)?;
        // The daemon is only reachable here. A failed `connect` during the
        // bounded reconnect phase means the daemon is still down — consume the
        // next retry slot instead of aborting. The very first connect (startup)
        // still fails fast, preserving the existing daemon-absent behavior.
        let core = match context.connect(None) {
            Ok(core) => core,
            Err(e) => {
                if reconnect.attempts_made() == 0 {
                    return Err(e.into());
                }
                match reconnect.begin_attempt() {
                    Some(backoff) => {
                        log::warn!(
                            "{} Reconnect attempt {}/{} cannot reach the PipeWire daemon \
                             ({e}); retrying in {:?}.",
                            "🔁".yellow(),
                            reconnect.attempts_made(),
                            reconnect.policy().max_attempts,
                            backoff,
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
                    None => {
                        backend_failure = Some(("core", format!("daemon unreachable: {e}")));
                        break 'host;
                    }
                }
            }
        };

        let (capture_stream, capture_listener, playback_stream, playback_listener);
        {
            let _lock = thread_loop.lock();

            let latency_str = format!("{}/48000", buffer_size);

            let (cs, cl) = capture::setup_capture_stream(
                &core,
                bridge_ptr,
                buffer_size,
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
            capture_stream = cs;
            capture_listener = cl;

            let (ps, pl) = playback::setup_playback_stream(
                &core,
                bridge_ptr,
                buffer_size,
                &latency_str,
                rt_status.clone(),
                backend_for_playback.clone(),
            )?;
            playback_stream = ps;
            playback_listener = pl;
        }

        let _app_state = AppState {
            capture_stream,
            capture_listener,
            playback_stream,
            playback_listener,
        };

        let _cpu_dma_lock = rt_setup::lock_cpu_c_states();

        sys.emit_irq_advisory(target_cpu);

        // 4.2 RT THREAD START (Background)
        thread_loop.start();

        // =========================================================
        // 5. MAIN CONTROL LOOP (Main Thread, Non-RT)
        // =========================================================
        let mut was_silent = false;
        let mut was_fading = false;
        let mut poll_state = rt_setup::PollState::default();
        // F-RB-010 / T4.4: set when the control loop observes a fatal backend
        // failure for this instance. Drives either the bounded reconnect
        // (T4.5) or the fail-fast teardown + `Err` return below.
        let mut instance_failure: Option<(&'static str, String)> = None;
        while !SHUTDOWN.load(Ordering::Acquire) {
            // F-RB-010 / T4.4: a fatal loss of backend connectivity (daemon
            // crash/restart, stream `Error`, post-streaming `Unconnected`)
            // must never leave the process idling forever without sound. It is
            // polled every iteration before any off-RT handler work; the loop
            // sleeps ≤ 100 ms per iteration, keeping detection inside the
            // < 500 ms acceptance SLA. The reconnect decision happens below
            // after the instance teardown.
            if let Some((stream, reason)) = backend_status.failure() {
                instance_failure = Some((stream, reason));
                break;
            }

            // pairs with Release store in main.rs:104
            let active = rt_status.active_rate.load(Ordering::Relaxed);
            if active != 0 {
                neural_amp_modeler_rs::common::diagnostics::ACTIVE_SAMPLE_RATE
                    .store(active, Ordering::Relaxed);
            }

            handlers::handle_resampler_rebuild(&rt_status, &sys, &mut resampler_producer);
            handlers::handle_quantum_log(&rt_status);
            handlers::handle_cabsim_rebuild(
                &rt_status,
                ir_raw_samples.as_deref(),
                ir_source_rate,
                &sys,
                &mut cabsim_producer,
            );
            handlers::handle_slimmable_rebuild(
                &rt_status,
                full_wavenet_model.as_deref(),
                has_model_r,
                &sys,
                &mut slimmable_producer,
            );
            handlers::handle_oversample_rebuild(&rt_status, &sys, &mut os_producer);

            (was_silent, was_fading) = rt_setup::poll_rt_status(
                &rt_status,
                &sys,
                was_silent,
                was_fading,
                unsafe { &*(bridge_ptr.as_ptr()) },
                &mut poll_state,
            );

            // R-04: while the loop runs, the parking lot is RT-owned (the callback
            // flushes it back to this SPSC every cycle), so this periodic drain
            // must NOT touch `rt_parking_lot` — concurrent `take()`s would race.
            // An empty main-side lot drains SPSC + overflow only; the 16 slots are
            // released by the final drain after `thread_loop.stop()` (handoff).
            let mut rt_owned_lot: [Option<GcItem>; 16] = Default::default();
            let drained = neural_amp_modeler_rs::common::spsc::drain_gc_channels(
                &mut gc_consumer,
                &gc_overflow,
                &mut rt_owned_lot,
                &rt_status,
            );
            rt_status
                .drains
                .fetch_add(drained as u32, Ordering::Relaxed);

            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        // =========================================================
        // 6. INSTANCE TEARDOWN (per attempt)
        // =========================================================
        // Ordering invariant (R-13): stop the audio loop FIRST so the RT callback
        // releases its `&mut` access to the recording producer (single-writer
        // SPSC contract). Only after `thread_loop.stop()` returns — which waits for
        // the loop thread to finish its current iteration — is the main thread the
        // sole writer of the recording channel.
        thread_loop.stop();

        // R-04: single-owner handoff — the loop thread has stopped, so the RT
        // callback will never touch `rt_parking_lot` again. One canonical
        // `drain_gc_channels` now releases SPSC + overflow + the 16 parked slots
        // on the main thread, before any RT state is dropped.
        let final_drained = neural_amp_modeler_rs::common::spsc::drain_gc_channels(
            &mut gc_consumer,
            &gc_overflow,
            &mut rt_parking_lot,
            &rt_status,
        );
        rt_status
            .drains
            .fetch_add(final_drained as u32, Ordering::Relaxed);
        if final_drained > 0 {
            log::debug!(
                "nam-audio-pipe: instance GC drain released {final_drained} item(s) off-RT (R-04)"
            );
        }

        // 6.1 Reconnect decision (F-RB-010 / T4.5)
        match instance_failure {
            // Clean shutdown (SIGINT/SIGTERM): the last instance is torn down
            // and the final teardown below finalizes the recording.
            None => break 'host,
            Some((stream, reason)) => {
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
                    // Interruptible sleep: a SIGINT/SIGTERM during the backoff
                    // is honored within 25 ms instead of delaying the shutdown.
                    if sleep_interruptible(backoff) {
                        break 'host;
                    }
                    continue 'host;
                }
                // Budget exhausted (or --fail-fast): fall back to the T4.4
                // fail-fast path below.
                backend_failure = Some((stream, reason));
                break 'host;
            }
        }
    }

    // =========================================================
    // 7. GRACEFUL SHUTDOWN (final)
    // =========================================================
    // The main thread now exclusively owns the recording producer slot (the RT
    // callback released its `&mut` after `thread_loop.stop()`). Hand the whole
    // worker custody to the guard's explicit shutdown: StreamStop → producer
    // drop → bounded join with formal result inspection (F-RB-009 / T3.5).
    // The returned outcome propagates recording failures (worker error, panic,
    // join timeout) back to `main()`, which turns them into a non-zero exit.
    //
    // If the recording worker already reported a fatal error (F-RB-009 / T3.3)
    // it has exited and its consumer is gone: the ring will never drain, so
    // the guard skips the `StreamStop` push (which would only burn the retry
    // timeout and log a misleading warning) and terminates via the producer
    // drop.
    let recording_outcome = recording_worker.take().map(RecordingWorkerGuard::shutdown);

    log::debug!(
        "PipeWire backend state at teardown: {:?}",
        backend_status.state()
    );

    // F-RB-010 / T4.4: if the reconnect budget was exhausted (or disabled),
    // the teardown above is the integral resource drain (RT loop stop, GC,
    // recording worker) and the host now returns an error instead of a clean
    // `Ok` — `main()` propagates it into a non-zero process exit, so a dead
    // backend is never mistaken for a successful run.
    if let Some((stream, reason)) = &backend_failure {
        NamDiagnostic::new(NamErrorCode::BackendFailure, &sys)
            .message(format!(
                "PipeWire backend failed: the '{stream}' stream lost connectivity \
                 and no reconnect attempt is available."
            ))
            .hint(
                "Restart the PipeWire service (systemctl --user restart pipewire) \
                 and NAM-Audio-Pipe. The process exited with a non-zero code \
                 instead of remaining alive without audio.",
            )
            .param("stream", *stream)
            .param("reason", reason)
            .param("reconnect_attempts", reconnect.attempts_made())
            .param("reconnect_max_attempts", reconnect.policy().max_attempts)
            .emit();

        if !matches!(
            recording_outcome,
            None | Some(RecordingWorkerOutcome::Success)
        ) {
            log::warn!(
                "Recording did not complete cleanly during the backend failure: \
                 {recording_outcome:?}"
            );
        }
        backend_status.mark_terminated();
        return Err(anyhow::anyhow!(
            "PipeWire backend failed ({stream}): {reason}"
        ));
    }

    backend_status.mark_terminated();
    Ok(recording_outcome)
}

/// Sleeps for `duration` in small slices, bailing out early when the
/// process-global `SHUTDOWN` flag is raised (SIGINT/SIGTERM).
///
/// Returns `true` when the shutdown flag was observed during the wait, `false`
/// when the full duration elapsed. Used by the bounded-reconnect backoff so a
/// signal is honored within ~25 ms even while the host waits for the daemon.
fn sleep_interruptible(duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if SHUTDOWN.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        let remaining = deadline.saturating_duration_since(now);
        std::thread::sleep(remaining.min(Duration::from_millis(25)));
    }
}
