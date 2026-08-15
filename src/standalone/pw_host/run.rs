// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire host execution — dual-stream topology setup, DSP bridge allocation,
//! CPU affinity locking, main control loop, and graceful shutdown.

use super::handlers;
use super::output_pw::AppState;
use crate::recording::buffer::{MAX_BLOCK_SIZE, RingPayload};
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{
    GcItem, GcOverflowBuffer, ParamPayload, RtStatusFlags, SHUTDOWN,
};
use neural_amp_modeler_rs::dsp::resampler::NamResampler;
use neural_amp_modeler_rs::models::StaticModel;

use rtrb::Consumer;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::PipewireHostConfig;
use super::bridge;
use super::capture;
use super::identity;
use super::playback;

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
#[expect(
    clippy::too_many_arguments,
    reason = "FFI design or complex DSP kernel signature required by construction"
)]
pub fn run_pipewire_host(
    consumer: Consumer<ParamPayload>,
    gc_producer: rtrb::Producer<GcItem>,
    gc_overflow: Arc<GcOverflowBuffer>,
    resampler_consumer: Consumer<Box<NamResampler>>,
    mut resampler_producer: rtrb::Producer<Box<NamResampler>>,
    cabsim_consumer: Consumer<Option<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter>>,
    mut cabsim_producer: rtrb::Producer<
        Option<neural_amp_modeler_rs::dsp::cabsim::adapter::CabSimAdapter>,
    >,
    rt_status: Arc<RtStatusFlags>,
    config: PipewireHostConfig,
    mut gc_consumer: Consumer<GcItem>,
    slimmable_consumer: Consumer<Option<Box<StaticModel>>>,
    os_consumer: Consumer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>,
    recording_producer: Option<rtrb::Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    recording_io_handle: Option<std::thread::JoinHandle<()>>,
) -> anyhow::Result<()> {
    let PipewireHostConfig {
        buffer_size,
        sys,
        ir_raw_samples,
        full_wavenet_model,
        mut slimmable_producer,
        mut os_producer,
        oversample,
    } = config;

    let full_wavenet_model = full_wavenet_model;

    // =========================================================
    // 1. PIPEWIRE LOOP INITIALIZATION
    // =========================================================
    let thread_loop = unsafe {
        pipewire::thread_loop::ThreadLoopBox::new(Some(identity::PW_THREAD_LOOP_NAME), None)
    }?;
    let context = pipewire::context::ContextBox::new(thread_loop.loop_(), None)?;
    let core = context.connect(None)?;

    // =========================================================
    // 2. DSP BRIDGE ALLOCATION (Lock-Free Communication)
    // =========================================================
    let bridge_ptr = bridge::allocate_dsp_bridge();

    // Place the recording producer on the stack so both the RT closure
    // (via a raw pointer) and the shutdown path can access it without
    // locking. Producer is not Clone; a raw pointer avoids shared-ownership
    // plumbing while respecting the SPSC contract (single writer at a time).
    let mut recording_producer_slot = recording_producer;
    let rec_ptr: *mut Option<rtrb::Producer<RingPayload<MAX_BLOCK_SIZE>>> =
        &raw mut recording_producer_slot;

    // R-04: the RT parking lot (16 slots) lives HERE — a stack-local slot in
    // the main thread, accessed by the RT callback through a raw pointer
    // (same contract as `rec_ptr`: the slot outlives the closure). While the
    // loop runs, the RT callback is the sole writer. After `thread_loop.stop()`
    // the main thread becomes single owner and the final drain releases the
    // 16 slots off-RT via `drain_gc_channels` — never on the audio thread.
    let mut rt_parking_lot: [Option<GcItem>; 16] = Default::default();
    let parking_lot_ptr: *mut [Option<GcItem>; 16] = &raw mut rt_parking_lot;

    // =========================================================
    // 3. CORE OPTIMIZATION (CPU Affinity)
    // =========================================================
    let target_cpu = rt_setup::select_optimal_cpu().unwrap_or(0);

    // =========================================================
    // 4. PROTECTED CONFIGURATION SCOPE (RAII)
    // =========================================================
    let (capture_stream, capture_listener, playback_stream, playback_listener);
    {
        let _lock = thread_loop.lock();

        let latency_str = format!("{}/48000", buffer_size);

        let (cs, cl) = capture::setup_capture_stream(
            &core,
            bridge_ptr,
            buffer_size,
            ir_raw_samples.clone(),
            &sys,
            target_cpu,
            consumer,
            gc_producer,
            gc_overflow.clone(),
            resampler_consumer,
            cabsim_consumer,
            rt_status.clone(),
            slimmable_consumer,
            os_consumer,
            oversample,
            rec_ptr,
            parking_lot_ptr,
        )?;
        capture_stream = cs;
        capture_listener = cl;

        let (ps, pl) = playback::setup_playback_stream(
            &core,
            bridge_ptr,
            buffer_size,
            &latency_str,
            rt_status.clone(),
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

    // =========================================================
    // 5. RT THREAD START (Background)
    // =========================================================
    thread_loop.start();

    // =========================================================
    // 6. MAIN CONTROL LOOP (Main Thread, Non-RT)
    // =========================================================
    let mut was_silent = false;
    let mut was_fading = false;
    while !SHUTDOWN.load(Ordering::Acquire) {
        // pairs with Release store em main.rs:90
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
            &sys,
            &mut cabsim_producer,
        );
        handlers::handle_slimmable_rebuild(
            &rt_status,
            full_wavenet_model.as_deref(),
            &mut slimmable_producer,
        );
        handlers::handle_oversample_rebuild(&rt_status, &sys, &mut os_producer);

        (was_silent, was_fading) =
            rt_setup::poll_rt_status(&rt_status, &sys, was_silent, was_fading, unsafe {
                &*(bridge_ptr.as_ptr())
            });

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
    // 7. GRACEFUL SHUTDOWN
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
            "nam-audio-pipe: final GC drain released {final_drained} item(s) off-RT (R-04)"
        );
    }

    // The main thread now exclusively owns the recording producer: signal the
    // I/O thread to close and finalize the current WAV file. Retry briefly
    // so a full ring does not drop StreamStop (header rewrite would then
    // depend only on the SHUTDOWN race against the join timeout).
    if let Some(ref mut producer) = recording_producer_slot {
        push_stream_stop(producer, STREAM_STOP_RETRY_TIMEOUT);
    }

    // Wait (bounded) for the I/O thread to rewrite the WAV header with the
    // final `data` byte count before the producer is dropped here and before
    // the caller deinitializes PipeWire.
    if let Some(handle) = recording_io_handle {
        join_recording_io(handle, RECORDING_IO_JOIN_TIMEOUT);
    }

    Ok(())
}

/// Upper bound for waiting on the `nam-recording-io` thread during shutdown.
const RECORDING_IO_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Bound for retrying `StreamStop` after the audio loop has already stopped.
const STREAM_STOP_RETRY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// Pushes `StreamStop` with a short retry. The audio callback is already
/// stopped, so the I/O thread is the only remaining consumer and should
/// drain capacity quickly. On timeout the token is dropped and finalization
/// falls back to the `SHUTDOWN` flag.
fn push_stream_stop(
    producer: &mut rtrb::Producer<RingPayload<MAX_BLOCK_SIZE>>,
    timeout: std::time::Duration,
) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if producer.push(RingPayload::StreamStop).is_ok() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            log::warn!(
                "StreamStop could not be delivered within {timeout:?}; \
                 I/O will finalize on SHUTDOWN if it is still running."
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Bounded join for the `nam-recording-io` thread.
///
/// `disk_writer_loop` rewrites the WAV header with the final `data` byte count
/// and issues an `fsync` before returning. The main thread must wait for that
/// completion so the recorded file is valid before `pipewire::deinit()` and
/// process exit. The wait is bounded by `timeout`: if the thread does not
/// finish in time, the handle is detached (`SHUTDOWN` is already set, so the
/// thread still exits on its own) and a warning is logged.
fn join_recording_io(handle: std::thread::JoinHandle<()>, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !handle.is_finished() {
        if std::time::Instant::now() >= deadline {
            log::warn!(
                "nam-recording-io did not finish within {timeout:?}; \
                 detaching — the WAV header may be incomplete."
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // `is_finished() == true` guarantees `join()` returns immediately.
    let _ = handle.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_recording_io_returns_promptly_when_thread_finishes() {
        let handle = std::thread::spawn(|| {});
        let start = std::time::Instant::now();
        join_recording_io(handle, std::time::Duration::from_secs(5));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn join_recording_io_detaches_after_timeout() {
        let handle = std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
        });
        let start = std::time::Instant::now();
        join_recording_io(handle, std::time::Duration::from_millis(50));
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(50),
            "join returned before the timeout ({elapsed:?})"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(450),
            "join blocked past the bounded timeout ({elapsed:?})"
        );
    }

    fn dummy_meta() -> RingPayload<MAX_BLOCK_SIZE> {
        RingPayload::Metadata(crate::recording::buffer::AudioMetadata {
            sample_rate: 48000.0,
            bit_depth: 32,
            channels: 2,
        })
    }

    #[test]
    fn push_stream_stop_succeeds_when_capacity_frees() {
        let (mut prod, mut cons) = rtrb::RingBuffer::new(1);
        prod.push(dummy_meta()).unwrap();

        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = cons.pop();
        });

        push_stream_stop(&mut prod, std::time::Duration::from_millis(200));
        handle.join().unwrap();
    }

    #[test]
    fn push_stream_stop_times_out_when_ring_stays_full() {
        let (mut prod, _cons) = rtrb::RingBuffer::new(1);
        prod.push(dummy_meta()).unwrap();

        let start = std::time::Instant::now();
        push_stream_stop(&mut prod, std::time::Duration::from_millis(30));
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(30),
            "retry returned before the timeout ({elapsed:?})"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "retry blocked past the bounded timeout ({elapsed:?})"
        );
    }
}
