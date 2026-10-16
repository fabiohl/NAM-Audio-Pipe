// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire host execution — dual-stream topology setup, DSP bridge allocation,
//! CPU affinity locking, main control loop, bounded backend reconnection
//! and graceful shutdown.

use super::SharedBackendStatus;
use super::capture::state::{CaptureState, RtHostChannels};
use super::handlers;
use super::observe_rt_panic;
use super::output_pw::AppState;
use super::reconnect::{ReconnectCycle, ReconnectPolicy, STABLE_RESET, StabilityEvent};
use super::wakeup::ControlPlaneWakeup;
use crate::recording::alert::RecordingAlert;
use crate::recording::guard::{RecordingWorkerGuard, RecordingWorkerOutcome};
use crate::recording::transport::RecordingSender;
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
use super::latency;
use super::playback;
use crate::standalone::banner::{
    BannerInfo, SessionSummary, render_banner, render_session_summary,
};
use crate::standalone::colors::{Colorize, color_enabled};
use pipewire as pw;

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
///   builds `NamResampler::new_simple().expect("construction should succeed for test-sized buffers")` here (allocation outside RT) and sends to the callback.
/// - `rt_status`: Atomic flags for silent RT→Main communication.
/// - `recording_worker`: RAII custody of the recording I/O thread and its ring
///   producer. The guard keeps the worker alive across every early `?` return
///   (its `Drop` signals termination and joins bounded) and, on the normal
///   shutdown path, `RecordingWorkerGuard::shutdown` returns the observable join
///   outcome so recording failures propagate to the process exit code.
///
/// ## Bounded reconnect
///
/// The PipeWire daemon may be restarted by the package manager, by the user or
/// by a USB interface reconnect. Instead of aborting immediately, the host
/// re-instantiates the streams inside a **strictly bounded** retry cycle
/// ([`ReconnectCycle`], default 6 attempts with progressive 250 ms → 4 s
/// exponential backoff — disabled under `--fail-fast` or `max_attempts == 0`).
/// The budget is bounded **per episode** and replenished after
/// [`STABLE_RESET`] of uninterrupted healthy streaming (F-UX-09), so isolated
/// drops spread over a long session never accumulate into an exhaustion.
/// The DSP state ([`CaptureState`]: models, resampler, cab-sim, gains) and the
/// RT-side SPSC channels ([`RtHostChannels`]) live in heap `Box`es reached via
/// raw pointers, so **no internal state is lost** across a re-instantiation —
/// audio resumes with the same models/IRs/recorder. When the budget is
/// exhausted the host falls back to the fail-fast teardown (RT loop stop,
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
        stream_status,
        recording_status,
        banner,
    } = config;

    // Bounded reconnect policy. `--fail-fast` disables the recovery cycle
    // entirely; otherwise the production default allows 6 attempts per episode
    // with progressive backoff, optionally capped by `--reconnect-timeout <s>`.
    let reconnect_policy = if fail_fast {
        ReconnectPolicy::fail_fast()
    } else {
        match reconnect_timeout {
            Some(timeout) => ReconnectPolicy::with_backoff_timeout(timeout),
            None => ReconnectPolicy::production(),
        }
    };

    // F-UX-02a: turn the observable recording status into visible, de-duplicated
    // alerts. Built once (outside the reconnect loop) so a reconnect never
    // resets the reminder cadence or re-emits an already-reported failure.
    let mut recording_alert = recording_status.as_ref().map(|_| RecordingAlert::new());

    // Whether a WAV capture is live. Drives the extended termination grace
    // window (armed in `main` after the recording handshake) and the shutdown
    // feedback message: finalizing a capture must be allowed to finish.
    let recording_enabled = recording_worker.is_some();

    // =========================================================
    // 1. PERSISTENT HOST RESOURCES (survive reconnect attempts)
    // =========================================================
    // DSP state and RT-side channels live in heap Boxes reached through raw
    // pointers, so a bounded reconnect can re-instantiate the streams without
    // losing the models, IRs, resampler, gains or the SPSC wiring. The main
    // thread never aliases the pointed-to objects while the RT callback runs —
    // it touches them only before `thread_loop.start()` and after `thread_loop.stop()`.
    let bridge_ptr = bridge::allocate_dsp_bridge();

    // S6-T4 / F-PR-11: off-RT bring-up pre-roll. Before any stream is created,
    // the main thread holds exclusive access to the bridge (no RT producer or
    // consumer is live), so one quantum of analytical silence can be published
    // safely. The first playback quantum then reads a real silence block
    // through the normal wet path instead of paying the starvation/ZOH delay.
    // `buffer_size == 0` (host-driven quantum) primes a single sample; the
    // capture producer overwrites the primed block on its first callback.
    // SAFETY: `bridge_ptr` points to the heap-immortal `DspBridge` just
    // allocated above; no stream (and therefore no RT Writer/Reader) exists yet.
    unsafe { &mut *bridge_ptr.as_ptr() }.prime_silence(buffer_size.max(1) as usize);

    let mut rt_state = Box::new(CaptureState::init(&sys, oversample, gate_config));
    rt_state.ir_raw_samples = ir_raw_samples.clone();
    rt_state.ir_source_rate = ir_source_rate;
    // The cab-sim partition policy (--cabsim-partition) sizes every pair
    // installation; the RT callback publishes it on the first-install
    // request instead of inheriting the host quantum. Read-only after this
    // store (same exclusivity contract as `ir_source_rate`).
    rt_state.cabsim_partition_policy = cabsim_partition as usize;
    // T9.5: the four dedicated swap channels live inside the engine
    // `RtSwapDrain`s (each owns its ring consumer and deferred slot); the
    // mixed param ring stays in `RtHostChannels`.
    rt_state.resampler_drain = Some(super::rt_callback::resampler_swap_drain(resampler_consumer));
    rt_state.cabsim_drain = Some(super::rt_callback::cabsim_swap_drain(cabsim_consumer));
    rt_state.slimmable_drain = Some(super::rt_callback::slimmable_swap_drain(slimmable_consumer));
    rt_state.os_drain = Some(super::rt_callback::os_swap_drain(os_consumer));
    // T9.4/F-PERF-18: the main-thread oversample rebuild handler gates runtime
    // factor upgrades against the scratch capacity sized in `init` (the DSP
    // state becomes RT-owned after this point — the length is captured here,
    // before the raw-pointer handoff).
    let os_scratch_len = rt_state.os_in_l.len();
    let state_ptr: *mut CaptureState = &raw mut *rt_state;

    let mut rt_channels = Box::new(RtHostChannels {
        param_consumer: consumer,
        gc_producer,
        gc_overflow: gc_overflow.clone(),
    });
    let channels_ptr: *mut RtHostChannels = &raw mut *rt_channels;

    // The param handler stores the negotiated rate into this Arc (a clone of
    // `state.shared_target_rate`); the RT callback reads the same atomic via
    // `sync_rate` (rt_callback/rate_sync.rs).
    let rate_for_param = rt_state.shared_target_rate.clone();

    let full_wavenet_model_l = full_wavenet_model_l;
    let full_wavenet_model_r = full_wavenet_model_r;

    // Place the recording sender (owned by the worker guard — RAII custody)
    // on a stack slot so the RT closure can access it via a raw pointer
    // without locking. The sender bundles the pool producer and the
    // control-channel producer of the promoted transport; it is not cloneable,
    // so a raw pointer avoids shared-ownership plumbing while respecting the
    // SPSC contract (single writer at a time). When recording is disabled the
    // pointer targets a never-written dummy sender — the RT callback
    // dereferences it unconditionally.
    let mut dummy_recording_sender = RecordingSender::none();
    let rec_ptr: *mut RecordingSender = match &mut recording_worker {
        Some(guard) => &raw mut *guard.sender_slot(),
        None => &raw mut dummy_recording_sender,
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
    // 2. CORE OPTIMIZATION (CPU Affinity — receipt computed once by the caller)
    // =========================================================
    let target_cpu = cpu_receipt.selected_cpu;

    // =========================================================
    // 3. PROTECTED CONFIGURATION SCOPE (RAII)
    // =========================================================
    // Shared backend state machine: the capture and playback stream state
    // listeners mark it `Failed` on a fatal connectivity loss and the main
    // control loop below polls it every iteration — the process never survives
    // as a functionally-dead zombie. A failure with reconnect budget left
    // transitions to `Reconnecting` and re-instantiates the streams; only a
    // budget exhaustion triggers the fail-fast exit.
    //
    // Event-driven control plane wakeup: condition variable notification wakes
    // the control loop immediately on rate renegotiations and stream state
    // changes without waiting for the 100 ms health poll timer.
    let wakeup = ControlPlaneWakeup::new();
    let stream_status = stream_status.unwrap_or_else(|| Arc::new(super::StreamStatusFlags::new()));
    let mut backend_init =
        SharedBackendStatus::with_rt_and_stream_status(rt_status.clone(), stream_status);
    backend_init.bind_wakeup(wakeup.clone());
    let backend_status = Arc::new(backend_init);
    let backend_for_capture = backend_status.clone();
    let backend_for_playback = backend_status.clone();
    // The RT-observable failure flag lives in the worker guard; clone it here
    // for the capture stream callback.
    let recording_failed = recording_worker
        .as_ref()
        .and_then(RecordingWorkerGuard::failed_flag)
        .cloned();

    // Bounded reconnect cycle. One cycle per session: the budget is never reset,
    // so the recovery phase is strictly bounded in number of attempts and total
    // time by construction.
    let mut reconnect = ReconnectCycle::new(reconnect_policy);
    // Set when the control loop observes a fatal backend failure AND the
    // reconnect budget is exhausted. Drives the fail-fast teardown + `Err`
    // return below.
    let mut backend_failure: Option<(&'static str, String)> = None;

    // Main-thread latches for failed oversample and slimmable rebuild
    // generations. Lives here (outside the 'host loop) so a reconnect never
    // re-opens the retry storm for a generation that already failed.
    let mut rebuild_failures = handlers::RebuildFailureTracker::default();

    // Finding F-RB-101: latch guard protecting against spurious `state_changed(Unconnected)`
    // events emitted synchronously by `pw_stream_destroy()` during teardown and reconnection.
    // Lives outside the `'host` loop so it covers stream destruction across loop iterations.
    let mut teardown_guard: Option<super::status::TeardownGuard<'_>> = None;

    // F-UX-13: emit the consolidated status banner exactly once, as soon as the
    // first RT confirmations (rate / quantum / SCHED) are available.
    let mut banner_emitted = false;

    // =========================================================
    // 4. HOST INSTANCE LOOP (one per bounded-reconnect attempt)
    // =========================================================
    'host: loop {
        // Each instance spawns a fresh PipeWire RT data thread: real-time
        // setup (DAZ/FTZ, SCHED_FIFO, CPU affinity) must re-run on that fresh
        // thread during the first quantum of the new instance — the first
        // `process()` invocation is the only consumer-owned hook on the data
        // thread, because PipeWire's `state_changed`/`param_changed` listeners
        // dispatch on the thread-loop (main-loop) thread, never on the data
        // thread (verified against PipeWire 1.6.2; see
        // `rt_setup/thread.rs::configure_realtime_thread`).
        // The previous instance (if any) already stopped its loop, so the main
        // thread is the sole owner of the DSP state here.
        rt_state.thread_configured = false;

        // 4.1 PIPEWIRE LOOP INITIALIZATION (fresh per attempt)
        // SAFETY: `pipewire::init()` is called once in `main()` before entering this host loop.
        // `identity::PW_THREAD_LOOP_NAME` is a valid null-terminated static string slice.
        // `ThreadLoopBox::new` wraps `pw_thread_loop_new`; the returned loop is stopped via
        // `thread_loop.stop()` and dropped at instance teardown.
        let thread_loop = unsafe {
            pipewire::thread_loop::ThreadLoopBox::new(Some(identity::PW_THREAD_LOOP_NAME), None)
        }?;
        let context_props = pw::properties::properties! {
            "config.name" => "client.conf",
        };
        let context = pipewire::context::ContextBox::new(thread_loop.loop_(), Some(context_props))?;
        // The daemon is only reachable here. A failed `connect` during the
        // bounded reconnect phase means the daemon is still down — consume the
        // next retry slot instead of aborting. The very first connect (startup)
        // still fails fast, preserving the existing daemon-absent behavior.
        let core = match context.connect(None) {
            Ok(core) => core,
            Err(e) => {
                if reconnect.attempts_made() == 0 {
                    emit_startup_backend_diagnostic(&sys, "connect", &e);
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

        let setup_res: anyhow::Result<_> = (|| {
            let _lock = thread_loop.lock();

            // PipeWire `node.latency` property contract:
            // Expressed as a rational fraction ("quantum/rate", e.g. "128/48000").
            // PipeWire treats this as a requested duration in seconds: tau = quantum / rate.
            // When connecting to a graph running at a different rate R, PipeWire dynamically
            // reschedules the quantum to round(tau * R). Thus "{buffer_size}/48000" requests
            // exactly `buffer_size` frames of latency at 48kHz, dynamically rescaled by PipeWire.
            let latency_str = format!("{}/48000", buffer_size);

            let initial_latency_samples = if ir_raw_samples.is_some() {
                cabsim_partition.clamp(
                    16,
                    neural_amp_modeler_rs::dsp::pipeline::MAX_RESAMP_BUF as u32,
                )
            } else {
                0
            };

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
            )?;

            Ok((cs, cl, ps, pl, initial_latency_samples))
        })();

        let (
            capture_stream,
            capture_listener,
            playback_stream,
            playback_listener,
            initial_latency_samples,
        ) = match setup_res {
            Ok(res) => res,
            Err(e) => {
                if reconnect.attempts_made() == 0 {
                    emit_startup_backend_diagnostic(&sys, "stream-setup", &e);
                    return Err(e);
                }
                match reconnect.begin_attempt() {
                    Some(backoff) => {
                        log::warn!(
                            "{} Stream setup failed during reconnect attempt {}/{} \
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
                        backend_failure =
                            Some(("stream_setup", format!("stream setup failed: {e}")));
                        break 'host;
                    }
                }
            }
        };

        let app_state = AppState {
            capture_listener,
            capture_stream,
            playback_listener,
            playback_stream,
        };

        // All new streams for this instance are constructed and initialized.
        // We can safely release the teardown guard from any previous iteration.
        drop(teardown_guard.take());

        let _cpu_dma_lock = rt_setup::lock_cpu_c_states();

        sys.emit_irq_advisory(target_cpu);

        // 4.2 RT THREAD START (Background)
        //
        // T9.1(c): pin the PipeWire thread-loop thread to the housekeeping set
        // via a one-shot event source. The ThreadLoop runs its own internal
        // thread that dispatches the cold-path listeners (`param_changed`,
        // `state_changed`, proxy events) — scheduler noise on that thread must
        // stay off the selected RT core, exactly like the main control-loop
        // thread (T9.1(a)) and the recording worker (T9.1(b)). The event is
        // signalled once right after `start()` so its callback executes on the
        // thread-loop thread itself; an `eventfd` signal is cross-thread safe
        // (the same mechanism `pipewire::channel` uses).
        let housekeeping_for_thread_loop = cpu_receipt.housekeeping_cpus.clone();
        let thread_loop_pin = thread_loop.loop_().add_event(move || {
            let _ = rt_setup::apply_housekeeping_affinity(&housekeeping_for_thread_loop);
        });
        thread_loop.start();
        thread_loop_pin.signal();

        // =========================================================
        // 5. MAIN CONTROL LOOP (Main Thread, Non-RT)
        // =========================================================
        let mut active_cabsim_latency = initial_latency_samples;
        let mut active_resampler_latency = 0u32;
        let mut last_published_dsp_latency = initial_latency_samples;
        let mut last_published_rate = 48_000u32;

        let mut was_silent = false;
        let mut was_fading = false;
        let mut poll_state = rt_setup::PollState::with_cpu_receipt(Some(cpu_receipt.clone()));
        // Set when the control loop observes a fatal backend failure for this
        // instance. Drives either the bounded reconnect or the fail-fast
        // teardown + `Err` return below.
        let mut instance_failure: Option<(&'static str, String)> = None;
        // A contained RT panic is not a recoverable connectivity loss — the
        // callback code that panicked will panic again on any reconnected
        // instance and the `RT_STATUS_PANIC_CAPTURED` latch persists. Marking
        // it terminal skips the bounded reconnect cycle (which would burn all
        // attempts guaranteed-futile) and fails fast.
        let mut panic_is_terminal = false;
        while !SHUTDOWN.load(Ordering::Acquire) {
            // F-UX-02a: surface a runtime recording failure (or low-space
            // warning) immediately instead of only at shutdown. `try_lock` only
            // — the control loop never blocks on the worker's status mutex.
            if let (Some(alert), Some(status)) = (&mut recording_alert, &recording_status)
                && let Some(emitted) = alert.poll(status)
            {
                log::error!("{}", emitted.render());
            }

            // A fatal loss of backend connectivity (daemon crash/restart,
            // stream `Error`, post-streaming `Unconnected`) must never leave the
            // process idling forever without sound. It is polled every
            // iteration before any off-RT handler work; the loop sleeps ≤ 100 ms
            // per iteration, keeping detection inside the < 500 ms acceptance SLA.
            // The reconnect decision happens below after the instance teardown.
            if let Some((stream, reason)) = backend_status.failure() {
                instance_failure = Some((stream, reason));
                break;
            }

            // A panic contained inside an RT callback (capture or playback
            // `process` closure) is never an abort — the fatal
            // `RT_STATUS_PANIC_CAPTURED` latch is observed on this poll
            // (< 100 ms), the backend transitions to `Failed` and the ordered
            // teardown runs (thread-loop stop, GC drain,
            // `RecordingWorkerGuard::shutdown` finalizing the WAV).
            if observe_rt_panic(&rt_status, &backend_status) {
                panic_is_terminal = true;
                instance_failure = Some((
                    "rt_callback",
                    "panic captured in an RT callback closure (contained — ordered teardown follows)"
                        .to_owned(),
                ));
                break;
            }

            // F-UX-09: a healthy, streaming iteration feeds the stability
            // window. After `STABLE_RESET` of uninterrupted streaming the
            // per-episode retry budget is replenished (isolated drops spread
            // over a long session never accumulate into an exhaustion), and the
            // first healthy poll after a recovery confirms the restoration.
            // `is_running()` is a lock-free atomic mirror of `state == Running`,
            // so this per-iteration gate never takes the backend state mutex.
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
                // Not streaming (startup, a degraded blip or a lost stream): the
                // uninterrupted-healthy window is broken.
                reconnect.note_unstable();
            }

            // pairs with Release store in main.rs:104
            let active = rt_status.active_rate.load(Ordering::Relaxed);
            if active != 0 {
                neural_amp_modeler_rs::common::diagnostics::ACTIVE_SAMPLE_RATE
                    .store(active, Ordering::Relaxed);
            }

            let host_rate = if active != 0 {
                active
            } else {
                let p_rate = rate_for_param.load(Ordering::Relaxed);
                if p_rate != 0 { p_rate } else { 48_000 }
            };

            if let Some(resamp_lat) =
                handlers::handle_resampler_rebuild(&rt_status, &sys, &mut resampler_producer)
            {
                active_resampler_latency = resamp_lat;
            }

            if let Some(cab_lat) = handlers::handle_cabsim_rebuild(
                &rt_status,
                ir_raw_samples.as_deref(),
                ir_source_rate,
                &sys,
                &mut cabsim_producer,
            ) {
                active_cabsim_latency = cab_lat;
            }

            let current_dsp_latency = active_cabsim_latency + active_resampler_latency;
            handlers::handle_quantum_log(&rt_status, buffer_size, current_dsp_latency, host_rate);

            if current_dsp_latency != last_published_dsp_latency
                || (active != 0 && active != last_published_rate)
            {
                let _lock = thread_loop.lock();
                let (_, lat_ns) = latency::compute_dsp_latency(
                    active_cabsim_latency,
                    active_resampler_latency,
                    host_rate,
                );
                let _ = latency::update_stream_latency(
                    &app_state.capture_stream,
                    pw::spa::sys::SPA_DIRECTION_INPUT,
                    current_dsp_latency,
                    lat_ns,
                );
                let _ = latency::update_stream_latency(
                    &app_state.playback_stream,
                    pw::spa::sys::SPA_DIRECTION_OUTPUT,
                    current_dsp_latency,
                    lat_ns,
                );
                log::info!(
                    "{} Published updated DSP processing latency to PipeWire graph: {} samples ({:.2} ms @{}Hz)",
                    "⏱️".cyan(),
                    current_dsp_latency,
                    lat_ns as f64 / 1_000_000.0,
                    host_rate,
                );
                last_published_dsp_latency = current_dsp_latency;
                if active != 0 {
                    last_published_rate = active;
                }
            }
            handlers::handle_slimmable_rebuild(
                &rt_status,
                full_wavenet_model_l.as_deref(),
                full_wavenet_model_r.as_deref(),
                has_model_r,
                &sys,
                &mut slimmable_producer,
                &mut rebuild_failures,
            );
            handlers::handle_oversample_rebuild(
                &rt_status,
                &sys,
                &mut os_producer,
                &mut rebuild_failures,
                os_scratch_len,
            );

            (was_silent, was_fading) = rt_setup::poll_rt_status(
                &rt_status,
                Some(backend_status.stream_status()),
                &sys,
                was_silent,
                was_fading,
                // SAFETY: `bridge_ptr` points to the static `DspBridge` allocation created
                // by `allocate_dsp_bridge()`, valid for the entire process duration.
                // `poll_rt_status` only invokes `bridge.drain_dropped_frames()`, which reads and
                // swaps an `AtomicU64` field without racing with concurrent audio buffer access.
                unsafe { &*(bridge_ptr.as_ptr()) },
                &mut poll_state,
            );

            // F-UX-13: one consolidated status banner, emitted a single time as
            // soon as the first real audio quantum is processed — the RT thread is
            // configured in that same first callback, so `SCHED_FIFO`/`mlockall`
            // are already visible. The gate is the effective quantum, NOT
            // `active_rate`: `active_rate` is only published on a resampler swap,
            // which never happens for a 48 kHz graph with a 48 kHz/identity model
            // or in bypass, so gating on it would suppress the banner in the most
            // common configuration.
            let effective_quantum = rt_status.requested_buffer_frames.load(Ordering::Relaxed);
            if !banner_emitted && effective_quantum != 0 {
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
                    rt_fifo: rt_status
                        .check_flag(neural_amp_modeler_rs::common::spsc::RT_STATUS_RT_IS_FIFO),
                    rt_policy: policy_name,
                    rt_priority: rt_status.confirmed_priority.load(Ordering::Relaxed),
                    mlockall_ok: rt_setup::mlockall_ok(),
                };
                log::info!(
                    "{}",
                    render_banner(&banner_info, color_enabled()).trim_end()
                );
                banner_emitted = true;
            }

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

            // Event-driven wakeup — wakes up immediately on rate changes or
            // stream state transitions, with 100 ms as health-poll fallback to ensure
            // liveness and prevent busy-spin.
            wakeup.wait_timeout(std::time::Duration::from_millis(100));
        }

        // The panic latch is only observed inside the control loop above.
        // A panic captured in the same window as a SIGINT (the loop exits via
        // the `SHUTDOWN` condition before the next poll) would otherwise be
        // swallowed by a clean exit 0 with the captured audio silently lost.
        // Re-check the latch once after the loop so a contained panic always
        // surfaces as a failure, never as a `Success`.
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

        // =========================================================
        // 6. INSTANCE TEARDOWN (per attempt)
        // =========================================================
        // F-RB-101: Enter teardown BEFORE stopping the thread loop and destroying streams.
        teardown_guard = Some(backend_status.enter_teardown());

        // Ordering invariant (R-13): stop the audio loop FIRST so the RT callback
        // releases its `&mut` access to the recording producer (single-writer
        // SPSC contract). Only after `thread_loop.stop()` returns — which waits for
        // the loop thread to finish its current iteration — is the main thread the
        // sole writer of the recording channel.
        thread_loop.stop();

        // Drain any remaining clock drift drops accumulated in the bridge before resetting.
        // SAFETY: `bridge_ptr` is the live bridge allocation for this attempt and the
        // audio loop has been stopped above, so the main thread is the sole accessor.
        let remaining_drops = unsafe { &*(bridge_ptr.as_ptr()) }.drain_dropped_frames();
        if remaining_drops > 0 {
            backend_status
                .stream_status()
                .bridge_dropped_frames
                .fetch_add(remaining_drops, Ordering::Relaxed);
        }

        // Invalidates/advances the DSP bridge to zero so a reconnected instance
        // begins strictly in silence.
        // SAFETY: Sound because `thread_loop.stop()` immediately preceding this call
        // guarantees that the PipeWire loop thread is stopped and no RT Writer/Reader
        // is alive or accessing the bridge. The main thread is the sole owner of `bridge_ptr`.
        unsafe { &mut *bridge_ptr.as_ptr() }.reset_to_silence();

        // S6-T4 / F-PR-11: re-arm the off-RT bring-up pre-roll for the next
        // reconnect attempt, still under the exclusive-access teardown window
        // (`thread_loop.stop()` above guarantees no live RT Writer/Reader).
        // SAFETY: same exclusive-access contract as `reset_to_silence` above.
        unsafe { &mut *bridge_ptr.as_ptr() }.prime_silence(buffer_size.max(1) as usize);

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

        // F-RB-102 & F-RB-101: Explicitly drop the streams under the teardown guard.
        // Listeners are dropped before streams (F-RB-102), and any synchronous
        // `Unconnected` transitions are discarded by `observe_stream_state` (F-RB-101).
        drop(app_state);

        // 6.1 Reconnect decision
        match instance_failure {
            // Clean shutdown (SIGINT/SIGTERM): the last instance is torn down
            // and the final teardown below finalizes the recording.
            None => break 'host,
            Some((stream, reason)) => {
                // A contained RT panic is terminal — reconnecting re-runs the
                // same callback code and the latch persists, so every attempt
                // would fail on the first poll. Skip the bounded reconnect
                // cycle and fail fast instead.
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
                    // Interruptible sleep: a SIGINT/SIGTERM during the backoff
                    // is honored within 25 ms instead of delaying the shutdown.
                    if sleep_interruptible(backoff) {
                        break 'host;
                    }
                    continue 'host;
                }
                // Budget exhausted (or --fail-fast): fall back to the fail-fast
                // path below.
                backend_failure = Some((stream, reason));
                break 'host;
            }
        }
    }

    // =========================================================
    // 7. GRACEFUL SHUTDOWN (final)
    // =========================================================
    // User feedback on the first cooperative SHUTDOWN observation (F-UX-04):
    // without it the terminal shows nothing while the ring drains and the WAV
    // header is rewritten + `fsync`ed, so the operator presses Ctrl+C again and
    // (past the grace window) forces an exit that can corrupt the capture. The
    // message is emitted only for a genuine cooperative shutdown, never for a
    // backend failure (which is already reported by the diagnostic below).
    if backend_failure.is_none() {
        if recording_enabled {
            log::warn!(
                "⏹ Shutting down… finalizing recording (a second Ctrl+C will \
                 force exit and may corrupt the capture)"
            );
        } else {
            log::info!("⏹ Shutting down…");
        }
    }

    // The main thread now exclusively owns the recording producer slot (the RT
    // callback released its `&mut` after `thread_loop.stop()`). Hand the whole
    // worker custody to the guard's explicit shutdown: StreamStop → producer
    // drop → bounded join with formal result inspection.
    // The returned outcome propagates recording failures (worker error, panic,
    // join timeout) back to `main()`, which turns them into a non-zero exit.
    //
    // If the recording worker already reported a fatal error it has exited and
    // its consumer is gone: the ring will never drain, so the guard skips the
    // `StreamStop` push (which would only burn the retry timeout and log a
    // misleading warning) and terminates via the producer drop.
    let recording_outcome = recording_worker.take().map(RecordingWorkerGuard::shutdown);

    log::debug!(
        "PipeWire backend state at teardown: {:?}",
        backend_status.state()
    );

    // If the reconnect budget was exhausted (or disabled), the teardown above
    // is the integral resource drain (RT loop stop, GC, recording worker) and
    // the host now returns an error instead of a clean `Ok` — `main()` propagates
    // it into a non-zero process exit, so a dead backend is never mistaken for
    // a successful run.
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

    // F-UX-13: analogous end-of-session summary (xruns, bridge drops,
    // recording), emitted only once a real streaming session was established.
    if banner_emitted {
        let summary = SessionSummary {
            xruns: rt_status.xruns.load(Ordering::Relaxed),
            bridge_dropped_frames: backend_status
                .stream_status()
                .bridge_dropped_frames
                .load(Ordering::Relaxed),
            recording_dir: banner.recording_dir.clone(),
        };
        log::info!(
            "{}",
            render_session_summary(&summary, color_enabled()).trim_end()
        );
    }

    backend_status.mark_terminated();
    Ok(recording_outcome)
}

/// Emits the first-connection backend diagnostic with a stage-specific hint.
///
/// The two startup failures reached before the bounded-reconnect control loop
/// used to bubble a raw `anyhow` error that `main` printed with no hint
/// (F-UX-08). Emitting the structured diagnostic here keeps the remediation
/// hint reachable (it is rendered into the support block consumed by
/// `--diagnose`) and pairs with the classified `Backend` exit code in `main`.
fn emit_startup_backend_diagnostic(
    sys: &neural_amp_modeler_rs::SystemSnapshot,
    stage: &'static str,
    error: &dyn std::fmt::Display,
) {
    let hint = match stage {
        "connect" => crate::standalone::exit::BACKEND_UNREACHABLE_HINT,
        _ => {
            "PipeWire is running but the audio streams could not be created: \
             verify the graph with `pw-cli info 0` and that a capture/playback \
             device is available."
        }
    };
    NamDiagnostic::new(NamErrorCode::BackendFailure, sys)
        .message(format!(
            "PipeWire first connection failed ({stage}): {error}"
        ))
        .hint(hint)
        .param("phase", "startup")
        .param("stage", stage)
        .emit();
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
