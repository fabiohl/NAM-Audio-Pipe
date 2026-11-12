// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire output DSP pipeline and host configuration (standalone).

use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::pipeline::{DspBridgeReader, MAX_BRIDGE_BUF};
use pipewire as pw;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::rt_callback::{handle_spa_pair_fail_closed, silence_available_datas, stamp_chunk};
pub(crate) use super::spa_format::validate_built_pod;
pub use super::spa_format::{
    ContractViolation, SpaPodStorage, build_spa_format_pod, check_negotiated_rate_mismatch,
    mark_format_contract_ok, mark_stream_active, negotiated_rate_mismatch,
    reject_negotiated_format_violation, validate_audio_raw_format,
};
use super::stream_status::StreamStatusFlags;
use crate::standalone::cli::GateConfig;
use crate::standalone::rt_setup;

/// Holds essential PipeWire instances (`StreamBox` and `Listener`).
///
/// RAII-only struct: fields are never read directly — they exist solely
/// to keep streams and listeners alive via drop semantics. The compiler
/// may warn about unused fields; that is expected and safe here.
/// Removing the fields would cause premature deallocation and audio dropout.
///
/// # Drop Order Invariant (Memory Safety)
///
/// In Rust, struct fields are dropped in top-to-bottom declaration order (RFC 1857).
/// Every `*_listener` MUST precede its corresponding `*_stream` field:
///
/// 1. `capture_listener` before `capture_stream`
/// 2. `playback_listener` before `playback_stream`
///
/// `StreamBox::drop` invokes `pw_stream_destroy`, which unlinks and frees the
/// underlying PipeWire `pw_stream` struct (`stream_free`). If a `StreamListener`
/// is dropped afterwards, its `Drop` implementation calls `spa_hook_remove` /
/// `spa_list_remove`, performing pointer writes (`elem.prev->next = elem.next`,
/// `elem.next->prev = elem.prev`) into deallocated memory — causing use-after-free (UB).
/// By declaring the listeners first, each listener is cleanly detached from a still-live
/// PipeWire stream during teardown, reconnect cycles, and shutdown.
pub struct AppState<L1, S1, L2, S2> {
    /// Listener bound to the capture stream (RAII anchor).
    /// Must precede `capture_stream` for safe drop order.
    pub capture_listener: L1,
    /// PipeWire capture stream (RAII anchor).
    pub capture_stream: S1,
    /// Listener bound to the playback stream (RAII anchor).
    /// Must precede `playback_stream` for safe drop order.
    pub playback_listener: L2,
    /// PipeWire playback stream (RAII anchor).
    pub playback_stream: S2,
}

/// Configuration for PipeWire host initialization.
pub struct PipewireHostConfig {
    /// Requested audio buffer size.
    pub buffer_size: u32,
    /// Cab-sim UPOLS partition policy in samples (`--cabsim-partition`):
    /// fixes the cab-sim algorithmic latency (exactly N samples) and is used
    /// for every pair installation, decoupled from `buffer_size`.
    pub cabsim_partition: u32,
    /// System snapshot for diagnostics.
    pub sys: SystemSnapshot,
    /// Raw IR samples for adaptive partition rebuild (None if no IR loaded).
    pub ir_raw_samples: Option<Vec<f32>>,
    /// Sample rate of `ir_raw_samples` (the IR file's native rate). Rebuilds
    /// resample the preserved original IR for the applied host output rate.
    pub ir_source_rate: u32,
    /// Full WaveNet model (L channel) stored for main-thread slimmable rebuild.
    pub full_wavenet_model_l: Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    /// Full WaveNet model (R channel) stored for main-thread slimmable rebuild.
    pub full_wavenet_model_r: Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    /// Whether the loaded model has a right-channel model (stereo config).
    /// The slimmable rebuild slices an R model only when this is true.
    pub has_model_r: bool,
    /// Producer to send slimmable-rebuilt atomic L/R pairs to the audio thread.
    pub slimmable_producer: rtrb::Producer<Box<neural_amp_modeler_rs::common::spsc::SlimModelPair>>,
    /// Producer to send oversampling engines rebuilt on main thread to the audio thread.
    pub os_producer: rtrb::Producer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>,
    /// Initial oversampling factor for the neural stage.
    pub oversample: OversampleFactor,
    /// Pre-computed CPU selection receipt (`--cpu` already resolved by the
    /// caller). Carries the selected RT core for `configure_realtime_thread`
    /// and the housekeeping set applied to the thread-loop thread.
    pub cpu_receipt: crate::standalone::rt_setup::affinity::CpuSelectionReceipt,
    /// `--fail-fast` on the CLI: disables the bounded reconnect cycle —
    /// the first backend failure triggers observable teardown immediately.
    pub fail_fast: bool,
    /// Optional per-episode reconnect backoff budget (`--reconnect-timeout <s>`).
    /// `None` uses the production schedule (6 attempts, 250 ms → 4 s).
    pub reconnect_timeout: Option<Duration>,
    /// Noise gate configuration from the polymorphic CLI flag `--gate MODE`:
    /// [`GateConfig::Off`] keeps the gate FSM permanently open (silence passes
    /// through), while a [`GateConfig::Threshold`] carries the open/close dBFS
    /// thresholds of the Schmitt trigger (auto hysteresis of 10 dB).
    pub gate_config: GateConfig,
    /// Optional externally-owned stream status flags (for integration testing).
    pub stream_status: Option<std::sync::Arc<StreamStatusFlags>>,
    /// Optional observable recording status: when `--record` is
    /// active, `main` hands the same handle the recording worker writes so the
    /// main control loop can turn an `Active → Failed` transition (and the
    /// low-space pre-warning) into a visible, de-duplicated alert.
    pub recording_status: Option<crate::recording::SharedRecordingStatus>,
    /// Static banner facts resolved by `main`. The control loop emits
    /// one consolidated status banner once the first RT confirmations arrive.
    pub banner: crate::standalone::banner::BannerRequest,
    /// Explicit activation precision mode (`Standard` exact-grade or `Fast` Padé).
    pub activation: neural_amp_modeler_rs::math::activations::ActivationPrecision,
    /// Cabinet IR level policy (`--ir-level-policy`).
    pub ir_level_policy: crate::standalone::cli_domain::IrLevelPolicy,
    /// Optional hardware playback output limit (`--output-limit`).
    pub output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
}

/// Playback DSP Pipeline (Bridge → Hardware).
#[inline(always)]
pub fn playback_dsp_cycle(
    stream: &pw::stream::Stream,
    bridge: DspBridgeReader,
    last_bridge_gen: &mut u64,
    rt_status: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
    pb_frame_count: u32,
    output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
) {
    let should_measure = (pb_frame_count & 0xF) == 0;
    let t_pb_start = if should_measure {
        rt_setup::rdtsc_nanos()
    } else {
        0
    };

    // Fail-closed mute: while the negotiated format contract is broken
    // (a divergent renegotiation was rejected by the param_changed listener),
    // no processed audio may reach the hardware. Deterministic silence is
    // delivered instead, reusing the starvation silence policy — the DAC never
    // repeats stale audio and never plays garbled wrong-format data.
    if !stream_status.is_audio_unmuted() {
        deliver_silence_block(stream, rt_status, stream_status);
        return;
    }

    // Reads the newest bridge block. The closure only captures raw source
    // descriptors (base pointers + byte lengths) so the bridge front-buffer
    // borrow ends before the dequeue below. Per the double-buffer + skip-on-
    // overflow protocol (`consumed_gen`), the writer can publish at most one
    // newer block before this callback consumes it, and that block lands in
    // the complementary buffer — the captured source regions therefore stay
    // stable for the whole cycle (see `DspBridgeReader::read_block`).
    let Some((src_l, n_bytes, src_r, _)) = bridge.read_block(last_bridge_gen, |buf_l, buf_r| {
        (
            buf_l.as_ptr() as usize,
            std::mem::size_of_val(buf_l),
            buf_r.as_ptr() as usize,
            std::mem::size_of_val(buf_r),
        )
    }) else {
        // Bridge starvation: the capture stream produced no
        // new block this quantum (paused input, pending resampler rebuild,
        // clock drift or capture quantum miss). Deterministic silence policy:
        // still dequeue, validate fail-closed, fill 100% of the output
        // extension with analytical silence and recycle the buffer — the
        // hardware never repeats stale audio left in the previous buffer.
        deliver_silence_block(stream, rt_status, stream_status);
        return;
    };

    let Some(mut buf) = stream.dequeue_buffer() else {
        rt_status.output_buffer_miss.fetch_add(1, Ordering::Relaxed);
        return;
    };

    let datas = buf.datas_mut();
    if datas.len() < 2 {
        rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        silence_available_datas(datas);
        return;
    }

    // Splits the Left and Right channels for final delivery.
    let (datas_left, datas_right) = datas.split_at_mut(1);
    let data_l = &mut datas_left[0];
    let data_r = &mut datas_right[0];

    // Reads the raw SPA descriptors as integers/pointers before forming any
    // reference to the audio bytes: `Data::chunk()`/`chunk_mut()` panic on a
    // null chunk and `Data::data()` would form `&mut [u8]` aliases when L/R
    // share memory.
    let (ptr_l, max_l) = (
        data_l.as_raw().data as usize,
        data_l.as_raw().maxsize as usize,
    );
    let (ptr_r, max_r) = (
        data_r.as_raw().data as usize,
        data_r.as_raw().maxsize as usize,
    );
    let (chunk_l, chunk_r) = (data_l.as_raw().chunk, data_r.as_raw().chunk);

    // Playback always writes `n_bytes` at offset 0 into the chunk after the
    // contract is proven. The consolidated fail-closed harness rejects any
    // malformed descriptor (null data/chunk pointer, misaligned, OOB,
    // asymmetric or overlapping) by raising `RT_STATUS_HOST_CONTRACT_VIOLATION`
    // and silencing both channels, before the mutable `f32` output slices
    // are formed.
    // SAFETY: `ptr_l`/`ptr_r`/`max_l`/`max_r`/`chunk_l`/`chunk_r` are the SPA buffer
    // descriptors of the current callback and `src_l`/`src_r` are the live bridge
    // output blocks; the fail-closed harness validates the whole pair before any
    // slice is formed, so the call is sound for valid descriptors and returns `None`
    // (silencing) otherwise.
    let Some(_n_out) = (unsafe {
        deliver_playback_pair_fail_closed_with_limit(
            ptr_l,
            max_l,
            chunk_l,
            ptr_r,
            max_r,
            chunk_r,
            src_l as *const f32,
            src_r as *const f32,
            n_bytes,
            rt_status,
            output_limit,
        )
    }) else {
        return;
    };

    // 4. PLAYBACK TOTAL & 5. CAPTURE TO PLAYBACK (END-TO-END)
    // Measured: TSC overhead=~15ns per sample (LFENCE+RDTSC), total < 0.05% of 333µs quantum
    if should_measure && t_pb_start > 0 {
        let t_pb_end = rt_setup::rdtsc_nanos();
        let pb_nanos = t_pb_end.saturating_sub(t_pb_start);
        stream_status
            .playback_cycle_time
            .store(pb_nanos, Ordering::Relaxed);
        stream_status.playback_hist.record(pb_nanos);

        let cap_start = stream_status.capture_start_tsc.load(Ordering::Relaxed);
        if cap_start > 0 && t_pb_end > cap_start {
            let e2e_nanos = t_pb_end.saturating_sub(cap_start);
            stream_status
                .e2e_cycle_time
                .store(e2e_nanos, Ordering::Relaxed);
            stream_status.e2e_hist.record(e2e_nanos);
            // Mirror the sample into the exact reservoir so the test harness can
            // report true percentiles. Testing/heap-audit only; absent from the
            // default/release build.
            #[cfg(any(feature = "testing", feature = "heap-audit"))]
            stream_status.e2e_exact.record(e2e_nanos);
        }
    }
}

/// Stateless hard-clamp for hardware playback (`[-1.0, 1.0]`, `NaN -> 0.0`).
#[inline(always)]
fn hard_clamp_playback_sample(s: f32) -> f32 {
    if s.is_nan() { 0.0 } else { s.clamp(-1.0, 1.0) }
}

/// Copies `src` to `dst`, applying `output_limit` (`Off` = raw memcpy, `Clamp` = hard clamp to `[-1.0, 1.0]`).
///
/// # Safety
/// `src` and `dst` must be valid, disjoint, non-overlapping buffers of at least `n_out` `f32` elements.
#[inline(always)]
unsafe fn copy_playback_channel(
    src: *const f32,
    dst: &mut [f32],
    n_out: usize,
    output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
) {
    match output_limit {
        crate::standalone::cli_domain::OutputLimitPolicy::Off => {
            // SAFETY: caller guarantees `src` and `dst` are valid, disjoint, non-overlapping of length `n_out`.
            unsafe {
                core::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), n_out);
            }
        }
        crate::standalone::cli_domain::OutputLimitPolicy::Clamp => {
            // SAFETY: caller guarantees `src` is valid for `n_out` reads and disjoint from `dst`.
            let src_slice = unsafe { std::slice::from_raw_parts(src, n_out) };
            for (d, &s) in dst.iter_mut().zip(src_slice.iter()) {
                *d = hard_clamp_playback_sample(s);
            }
        }
    }
}

/// Delivers processed audio to a pair of SPA output buffers fail-closed (no output limit).
///
/// Validates pointer alignment, bounds, symmetry, and disjunction via
/// [`handle_spa_pair_fail_closed`]. If valid, copies `n_out` samples from `src_l`/`src_r`
/// to `ptr_l`/`ptr_r` and writes `offset=0`, `size=n_out * 4`, `stride=4` into
/// the respective [`pw::spa::sys::spa_chunk`] descriptors.
///
/// Returns `Some(n_out)` on success, or `None` if the SPA contract was violated.
///
/// # Safety
/// - `src_l` and `src_r` must point to valid contiguous slices of at least `n_bytes / size_of::<f32>()` floats.
/// - If `ptr_l` / `ptr_r` are valid, they must point to writable memory of size `max_l` / `max_r`.
/// - `chunk_l` and `chunk_r`, if non-null, must point to valid `spa_chunk` memory.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields plus audio sources; signature is shared by playback RT callback and test harnesses"
)]
pub unsafe fn deliver_playback_pair_fail_closed(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *mut pw::spa::sys::spa_chunk,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *mut pw::spa::sys::spa_chunk,
    src_l: *const f32,
    src_r: *const f32,
    n_bytes: usize,
    rt_status: &RtStatusFlags,
) -> Option<usize> {
    // SAFETY: same safety preconditions forwarded directly to `deliver_playback_pair_fail_closed_with_limit`.
    unsafe {
        deliver_playback_pair_fail_closed_with_limit(
            ptr_l,
            max_l,
            chunk_l,
            ptr_r,
            max_r,
            chunk_r,
            src_l,
            src_r,
            n_bytes,
            rt_status,
            crate::standalone::cli_domain::OutputLimitPolicy::Off,
        )
    }
}

/// Delivers processed audio to a pair of SPA output buffers fail-closed with an explicit
/// [`OutputLimitPolicy`](crate::standalone::cli_domain::OutputLimitPolicy).
///
/// # Safety
/// - `src_l` and `src_r` must point to valid contiguous slices of at least `n_bytes / size_of::<f32>()` floats.
/// - If `ptr_l` / `ptr_r` are valid, they must point to writable memory of size `max_l` / `max_r`.
/// - `chunk_l` and `chunk_r`, if non-null, must point to valid `spa_chunk` memory.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields plus audio sources and output limit policy"
)]
pub unsafe fn deliver_playback_pair_fail_closed_with_limit(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *mut pw::spa::sys::spa_chunk,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *mut pw::spa::sys::spa_chunk,
    src_l: *const f32,
    src_r: *const f32,
    n_bytes: usize,
    rt_status: &RtStatusFlags,
    output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
) -> Option<usize> {
    let (_n_bytes, n_out) = match handle_spa_pair_fail_closed(
        ptr_l, max_l, chunk_l, 0, n_bytes, ptr_r, max_r, chunk_r, 0, n_bytes, rt_status,
    ) {
        Some(v) => v,
        None => {
            stamp_chunk(chunk_l, 0);
            stamp_chunk(chunk_r, 0);
            return None;
        }
    };

    // SAFETY: `check_spa_buffer_pair` proved alignment, bounds, frame
    // symmetry and strict pointer disjunction, so `out_l` is well-formed.
    let out_l = unsafe { std::slice::from_raw_parts_mut(ptr_l as *mut f32, n_out) };
    // SAFETY: `src_l` and `out_l` are valid, disjoint non-overlapping buffers of length `n_out`.
    unsafe {
        copy_playback_channel(src_l, out_l, n_out, output_limit);
    }
    // SAFETY: `check_spa_buffer_pair` proved alignment, bounds, frame
    // symmetry and strict pointer disjunction, so `out_r` is well-formed.
    let out_r = unsafe { std::slice::from_raw_parts_mut(ptr_r as *mut f32, n_out) };
    // SAFETY: `src_r` and `out_r` are valid, disjoint non-overlapping buffers of length `n_out`.
    unsafe {
        copy_playback_channel(src_r, out_r, n_out, output_limit);
    }

    // Informs the hardware exactly how much sound was delivered this time.
    // SAFETY: both chunk pointers were validated non-null above; the chunk
    // structs are owned by the SPA buffer and stable for the callback duration.
    unsafe {
        let chunk_l_mut = &mut *chunk_l;
        chunk_l_mut.offset = 0;
        chunk_l_mut.size = (n_out * std::mem::size_of::<f32>()) as u32;
        chunk_l_mut.stride = std::mem::size_of::<f32>() as i32;

        let chunk_r_mut = &mut *chunk_r;
        chunk_r_mut.offset = 0;
        chunk_r_mut.size = (n_out * std::mem::size_of::<f32>()) as u32;
        chunk_r_mut.stride = std::mem::size_of::<f32>() as i32;
    }

    Some(n_out)
}

/// Deterministic silence delivery for bridge starvation.
///
/// Pure SPA-descriptor kernel (raw integers/pointers, no live PipeWire stream
/// required — mockable by the harness tests). Validates the stereo pair
/// fail-closed with the *silence window* `(0, silence_bytes)` per channel —
/// **not** the full allocated `maxsize` (which may be 64 KiB and exceeds the
/// `MAX_BRIDGE_BUF × 4` safety cap, causing false `E2304` on pause) —,
/// zero-fills exactly `silence_bytes / 4` frames of both output regions,
/// stamps `offset = 0`, `size = silence_bytes`, `stride = 4` on both chunks
/// and registers the starvation occurrence on `rt_status`.
///
/// The caller (`deliver_silence_block`) quantizes `silence_bytes` from the
/// active stream quantum (`last_n_samples`/`requested_buffer_frames`, 128-frame
/// fallback), bounded by both channel capacities and `MAX_BRIDGE_BUF × 4`, so
/// pausing the input or starting before the first block never raises
/// `RT_STATUS_HOST_CONTRACT_VIOLATION` (`E2304`).
///
/// Public so the resilience harness (`tests/service_resilience.rs`)
/// can prove the analytical-silence + buffer-recycle contract deterministically
/// (zero bridge generation → `0.0f32` sequences, no stalls) without a live
/// PipeWire graph.
///
/// Returns the number of frames delivered as silence, or `None` when the host
/// handed us a malformed descriptor (the consolidated harness already raised
/// `RT_STATUS_HOST_CONTRACT_VIOLATION` and silenced both channels).
///
/// # Safety
///
/// The caller must prove — for both channels — that `ptr_l`/`ptr_r` point to
/// writable, `f32`-aligned, non-overlapping regions of `max_l`/`max_r` bytes
/// each, and that `chunk_l`/`chunk_r` are non-null and remain valid (owned by
/// the SPA buffer) for the duration of the call. `silence_bytes` must be a
/// multiple of `sizeof(f32)`, not exceed either channel's `maxsize` nor
/// `MAX_BRIDGE_BUF × sizeof(f32)`. The kernel validates the regions via
/// [`handle_spa_pair_fail_closed`] and only dereferences the chunk pointers
/// after proving them non-null, but the safety contract of the raw pointer
/// arguments is the caller's.
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Raw SPA descriptor fields plus the bounded silence window; signature is stable and shared verbatim by the RT callback and the harness tests"
)]
pub unsafe fn deliver_silence_pair_fail_closed(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *mut pw::spa::sys::spa_chunk,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *mut pw::spa::sys::spa_chunk,
    silence_bytes: usize,
    rt_status: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
) -> Option<usize> {
    let (_n_bytes, n_out) = match handle_spa_pair_fail_closed(
        ptr_l,
        max_l,
        chunk_l,
        0,
        silence_bytes,
        ptr_r,
        max_r,
        chunk_r,
        0,
        silence_bytes,
        rt_status,
    ) {
        Some(v) => v,
        None => {
            stamp_chunk(chunk_l, 0);
            stamp_chunk(chunk_r, 0);
            return None;
        }
    };

    // SAFETY: `check_spa_buffer_pair` proved alignment, bounds, frame
    // symmetry and strict pointer disjunction, so `out_l` is well-formed.
    let out_l = unsafe { std::slice::from_raw_parts_mut(ptr_l as *mut f32, n_out) };
    out_l.fill(0.0);
    // SAFETY: `check_spa_buffer_pair` proved alignment, bounds, frame
    // symmetry and strict pointer disjunction, so `out_r` is well-formed.
    let out_r = unsafe { std::slice::from_raw_parts_mut(ptr_r as *mut f32, n_out) };
    out_r.fill(0.0);

    // SAFETY: both chunk pointers were validated non-null above; the chunk
    // structs are owned by the SPA buffer and stable for the callback duration.
    unsafe {
        let chunk_l_mut = &mut *chunk_l;
        chunk_l_mut.offset = 0;
        chunk_l_mut.size = (n_out * std::mem::size_of::<f32>()) as u32;
        chunk_l_mut.stride = std::mem::size_of::<f32>() as i32;

        let chunk_r_mut = &mut *chunk_r;
        chunk_r_mut.offset = 0;
        chunk_r_mut.size = (n_out * std::mem::size_of::<f32>()) as u32;
        chunk_r_mut.stride = std::mem::size_of::<f32>() as i32;
    }

    // Telemetry: one deterministic silence block delivered to the graph.
    stream_status
        .playback_bridge_starvation
        .fetch_add(1, Ordering::Relaxed);

    Some(n_out)
}

/// Dequeues the playback output buffer and, under bridge starvation, fills it
/// with analytical silence and recycles it.
///
/// The silence window is **quantized by the active stream quantum** — the last
/// processed frame count (`last_n_samples`, fallback to
/// `requested_buffer_frames`, then a 128-frame default) — bounded by both
/// channel capacities and `MAX_BRIDGE_BUF × 4`. It never uses the raw shared
/// memory `maxsize` (e.g. 64 KiB), which would trip the fail-closed SPA window
/// check (`E2304`) hundreds of times per second when the input is paused.
///
/// The buffer is returned to the PipeWire graph by dropping the dequeued
/// `Buffer` at the end of this function — every callback path (success,
/// contract violation or missing buffer) recycles or counts the miss, so the
/// output node never starves for buffers.
#[inline(always)]
fn deliver_silence_block(
    stream: &pw::stream::Stream,
    rt_status: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
) {
    let mut buf = match stream.dequeue_buffer() {
        Some(b) => b,
        None => {
            rt_status.output_buffer_miss.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let datas = buf.datas_mut();
    if datas.len() < 2 {
        rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
        silence_available_datas(datas);
        return;
    }

    let (datas_left, datas_right) = datas.split_at_mut(1);
    let data_l = &mut datas_left[0];
    let data_r = &mut datas_right[0];

    // Raw descriptor reads before any `&mut [u8]`/`&mut [f32]` is formed.
    let (ptr_l, max_l) = (
        data_l.as_raw().data as usize,
        data_l.as_raw().maxsize as usize,
    );
    let (ptr_r, max_r) = (
        data_r.as_raw().data as usize,
        data_r.as_raw().maxsize as usize,
    );
    let (chunk_l, chunk_r) = (data_l.as_raw().chunk, data_r.as_raw().chunk);

    // E2304: silence quantum = last active frame count (or requested
    // quantum, or 128-frame default), expressed in bytes and bounded by both
    // channel capacities and MAX_BRIDGE_BUF × 4. Delivering the shared-memory
    // `maxsize` here would exceed the fail-closed window cap on a 64 KiB
    // buffer and raise a false E2304 on every pause.
    let active_frames = {
        let last = rt_status.last_n_samples.load(Ordering::Relaxed);
        if last != 0 {
            last
        } else {
            rt_status.requested_buffer_frames.load(Ordering::Relaxed)
        }
    };
    let silence_frames = if active_frames == 0 {
        128
    } else {
        active_frames
    };
    let silence_bytes = (silence_frames as usize * std::mem::size_of::<f32>())
        .min(max_l)
        .min(max_r)
        .min(MAX_BRIDGE_BUF * std::mem::size_of::<f32>());

    // Validate, silence and recycle. On a malformed descriptor the harness
    // raises `RT_STATUS_HOST_CONTRACT_VIOLATION`, silences both channels and
    // the buffer still returns to the graph via drop.
    //
    // SAFETY: the SPA `data` pointers/chunks were read as raw integers from the
    // live dequeued buffer (owned by PipeWire and stable for the callback), and
    // the kernel itself rejects null/aligned-overlapping descriptors fail-closed
    // before dereferencing any of them.
    let _ = unsafe {
        deliver_silence_pair_fail_closed(
            ptr_l,
            max_l,
            chunk_l,
            ptr_r,
            max_r,
            chunk_r,
            silence_bytes,
            rt_status,
            stream_status,
        )
    };
}

#[cfg(test)]
#[path = "output_pw_test.rs"]
mod output_pw_test;
