// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire output DSP pipeline and host configuration (standalone).

use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::pipeline::DspBridgeReader;
use pipewire as pw;
use std::sync::atomic::Ordering;

use super::rt_callback::{handle_spa_pair_fail_closed, silence_available_datas};
use crate::standalone::rt_setup;

/// Holds essential PipeWire instances (`StreamBox` and `Listener`).
///
/// RAII-only struct: fields are never read directly — they exist solely
/// to keep streams and listeners alive via drop semantics. The compiler
/// may warn about unused fields; that is expected and safe here.
/// Removing the fields would cause premature deallocation and audio dropout.
pub struct AppState<S1, L1, S2, L2> {
    /// PipeWire capture stream (RAII anchor).
    pub capture_stream: S1,
    /// Listener bound to the capture stream (RAII anchor).
    pub capture_listener: L1,
    /// PipeWire playback stream (RAII anchor).
    pub playback_stream: S2,
    /// Listener bound to the playback stream (RAII anchor).
    pub playback_listener: L2,
}

/// Configuration for PipeWire host initialization.
pub struct PipewireHostConfig {
    /// Requested audio buffer size.
    pub buffer_size: u32,
    /// System snapshot for diagnostics.
    pub sys: SystemSnapshot,
    /// Raw IR samples for adaptive partition rebuild (None if no IR loaded).
    pub ir_raw_samples: Option<Vec<f32>>,
    /// Sample rate of `ir_raw_samples` (the IR file's native rate). Rebuilds
    /// resample the preserved original IR for the applied host output rate
    /// (F-RB-006 rate calibration).
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
    /// Optional explicit CPU core index requested via CLI (`--cpu`).
    pub requested_cpu: Option<usize>,
    /// `--fail-fast` on the CLI: disables the bounded reconnect cycle
    /// (F-RB-010 / T4.5) — the first backend failure triggers the T4.4
    /// fail-fast teardown immediately.
    pub fail_fast: bool,
}

/// Playback DSP Pipeline (Bridge → Hardware).
#[inline(always)]
pub fn playback_dsp_cycle(
    stream: &pw::stream::Stream,
    bridge: DspBridgeReader,
    last_bridge_gen: &mut u64,
    rt_status: &RtStatusFlags,
    pb_frame_count: u32,
) {
    let should_measure = (pb_frame_count & 0xF) == 0;
    let t_pb_start = if should_measure {
        rt_setup::rdtsc_nanos()
    } else {
        0
    };

    // T4.3 fail-closed mute: while the negotiated format contract is broken
    // (a divergent renegotiation was rejected by the param_changed listener),
    // no processed audio may reach the hardware. Deterministic silence is
    // delivered instead, reusing the starvation silence policy — the DAC never
    // repeats stale audio and never plays garbled wrong-format data.
    if !rt_status.is_audio_unmuted() {
        deliver_silence_block(stream, rt_status);
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
        // Bridge starvation (G-RB-001 / T4.2): the capture stream produced no
        // new block this quantum (paused input, pending resampler rebuild,
        // clock drift or capture quantum miss). Deterministic silence policy:
        // still dequeue, validate fail-closed, fill 100% of the output
        // extension with analytical silence and recycle the buffer — the
        // hardware never repeats stale audio left in the previous buffer.
        deliver_silence_block(stream, rt_status);
        return;
    };

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
    let Some((_n_bytes, n_out)) = handle_spa_pair_fail_closed(
        ptr_l, max_l, chunk_l, 0, n_bytes, ptr_r, max_r, chunk_r, 0, n_bytes, rt_status,
    ) else {
        return;
    };

    // Copies the processed sound directly to your sound card outputs.
    // SAFETY: `check_spa_buffer_pair` proved alignment, bounds, frame
    // symmetry and strict pointer disjunction, so the two `&mut [f32]` below
    // are well-formed and non-overlapping; the source regions were captured
    // from the bridge front-buffer and remain stable for this cycle.
    let out_l = unsafe { std::slice::from_raw_parts_mut(ptr_l as *mut f32, n_out) };
    unsafe {
        core::ptr::copy_nonoverlapping(src_l as *const f32, out_l.as_mut_ptr(), n_out);
    }
    let out_r = unsafe { std::slice::from_raw_parts_mut(ptr_r as *mut f32, n_out) };
    unsafe {
        core::ptr::copy_nonoverlapping(src_r as *const f32, out_r.as_mut_ptr(), n_out);
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

    // 4. PLAYBACK TOTAL & 5. CAPTURE TO PLAYBACK (END-TO-END)
    // Medido: overhead TSC=~15ns por amostragem (LFENCE+RDTSC), total < 0.05% do quantum de 333µs
    if should_measure && t_pb_start > 0 {
        let t_pb_end = rt_setup::rdtsc_nanos();
        let pb_nanos = t_pb_end.saturating_sub(t_pb_start);
        rt_status
            .playback_cycle_time
            .store(pb_nanos, Ordering::Relaxed);
        rt_status.playback_hist.record(pb_nanos);

        let cap_start = rt_status.capture_start_tsc.load(Ordering::Relaxed);
        if cap_start > 0 && t_pb_end > cap_start {
            let e2e_nanos = t_pb_end.saturating_sub(cap_start);
            rt_status.e2e_cycle_time.store(e2e_nanos, Ordering::Relaxed);
            rt_status.e2e_hist.record(e2e_nanos);
        }
    }
}

/// Deterministic silence delivery for bridge starvation (G-RB-001 / T4.2).
///
/// Pure SPA-descriptor kernel (raw integers/pointers, no live PipeWire stream
/// required — mockable by the harness tests). Validates the stereo pair
/// fail-closed with the full-extension window `(0, maxsize)` per channel,
/// zero-fills 100% of both output regions, stamps `offset = 0`,
/// `size = frames × 4`, `stride = 4` on both chunks and registers the
/// starvation occurrence on `rt_status`.
///
/// Public so the ER-4 service-resilience harness (`tests/service_resilience.rs`)
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
/// the SPA buffer) for the duration of the call. The kernel validates the
/// regions via [`handle_spa_pair_fail_closed`] and only dereferences the chunk
/// pointers after proving them non-null, but the safety contract of the raw
/// pointer arguments is the caller's.
#[inline(always)]
pub unsafe fn deliver_silence_pair_fail_closed(
    ptr_l: usize,
    max_l: usize,
    chunk_l: *mut pw::spa::sys::spa_chunk,
    ptr_r: usize,
    max_r: usize,
    chunk_r: *mut pw::spa::sys::spa_chunk,
    rt_status: &RtStatusFlags,
) -> Option<usize> {
    let (_n_bytes, n_out) = handle_spa_pair_fail_closed(
        ptr_l, max_l, chunk_l, 0, max_l, ptr_r, max_r, chunk_r, 0, max_r, rt_status,
    )?;

    // SAFETY: the harness proved per-channel alignment, bounds, frame symmetry
    // and strict pointer disjunction, so the two `&mut [f32]` below are
    // well-formed and non-overlapping.
    let out_l = unsafe { std::slice::from_raw_parts_mut(ptr_l as *mut f32, n_out) };
    out_l.fill(0.0);
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
    rt_status
        .playback_bridge_starvation
        .fetch_add(1, Ordering::Relaxed);

    Some(n_out)
}

/// Dequeues the playback output buffer and, under bridge starvation, fills it
/// with analytical silence and recycles it (G-RB-001 / T4.2).
///
/// The buffer is returned to the PipeWire graph by dropping the dequeued
/// `Buffer` at the end of this function — every callback path (success,
/// contract violation or missing buffer) recycles or counts the miss, so the
/// output node never starves for buffers.
#[inline(always)]
fn deliver_silence_block(stream: &pw::stream::Stream, rt_status: &RtStatusFlags) {
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

    // Validate, silence and recycle. On a malformed descriptor the harness
    // raises `RT_STATUS_HOST_CONTRACT_VIOLATION`, silences both channels and
    // the buffer still returns to the graph via drop.
    //
    // SAFETY: the SPA `data` pointers/chunks were read as raw integers from the
    // live dequeued buffer (owned by PipeWire and stable for the callback), and
    // the kernel itself rejects null/aligned-overlapping descriptors fail-closed
    // before dereferencing any of them.
    let _ = unsafe {
        deliver_silence_pair_fail_closed(ptr_l, max_l, chunk_l, ptr_r, max_r, chunk_r, rt_status)
    };
}

/// Storage buffer for SPA POD construction with a guaranteed 8-byte alignment.
///
/// The `libspa`/PipeWire pod builder produces PODs whose addresses must be
/// aligned to `align_of::<u64>()`; plain `[u8; N]` stack arrays only guarantee
/// alignment 1. Using this type makes the alignment a compile-time property of
/// every call site.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy)]
pub struct SpaPodStorage<const N: usize = 1024>(pub [u8; N]);

impl<const N: usize> SpaPodStorage<N> {
    /// Creates a zero-initialized aligned storage buffer.
    pub const fn new() -> Self {
        Self([0u8; N])
    }

    /// Returns the storage as a mutable byte slice.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.0
    }

    /// Returns the storage as an immutable byte slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> Default for SpaPodStorage<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Validates a raw SPA POD pointer produced by a builder targeting `storage`.
///
/// Proves, before forming a `&Pod`, that the pointer is non-null, 8-byte
/// aligned, lies within the storage bounds, and that its header/body/padding
/// fit entirely inside the remaining storage (fail-closed contract validation).
fn validate_built_pod(
    storage: &SpaPodStorage<1024>,
    pod_ptr: *const pw::spa::sys::spa_pod,
) -> Option<&pw::spa::pod::Pod> {
    if pod_ptr.is_null() {
        return None;
    }
    let pod_addr = pod_ptr as usize;
    if !pod_addr.is_multiple_of(8) {
        return None;
    }
    let base = storage.as_slice().as_ptr() as usize;
    let offset = pod_addr.checked_sub(base)?;
    let bytes = storage.as_slice().get(offset..)?;
    pw::spa::pod::Pod::from_bytes(bytes)
}

/// Builds an F32P stereo audio format SPA Pod for PipeWire negotiation.
///
/// The returned pod borrows `storage`; the caller must keep `storage` alive and
/// unmodified for as long as the pod is in use.
///
/// # Safety
/// The caller must not use the returned pod beyond the lifetime of `storage`.
pub unsafe fn build_spa_format_pod<'a>(
    audio_info: &pw::spa::param::audio::AudioInfoRaw,
    storage: &'a mut SpaPodStorage<1024>,
) -> anyhow::Result<&'a pw::spa::pod::Pod> {
    // SAFETY: zeroed is a valid initial state for the C `spa_pod_builder`; it is
    // fully initialized by `spa_pod_builder_init` before any field is read.
    let mut builder: pw::spa::sys::spa_pod_builder = unsafe { std::mem::zeroed() };
    // SAFETY: `SpaPodStorage` guarantees the buffer handed to the libspa pod
    // builder is 8-byte aligned, which is required to produce well-aligned PODs.
    unsafe {
        // Prepares a "builder" to create the audio format contract.
        pw::spa::sys::spa_pod_builder_init(
            &mut builder,
            storage.as_mut_slice().as_mut_ptr().cast(),
            storage.as_mut_slice().len() as u32,
        );

        // Builds the binary document (SPA Pod) describing the audio (e.g.: 48kHz, Stereo).
        // This document is what PipeWire uses to understand how to send sound to us.
        let pod_ptr = pw::spa::sys::spa_format_audio_raw_build(
            &mut builder,
            pw::spa::param::ParamType::EnumFormat.as_raw(),
            &audio_info.as_raw(),
        );

        // Returns the contract ready to be signed and used by the system.
        validate_built_pod(storage, pod_ptr).ok_or_else(|| {
            anyhow::anyhow!("Failed to build the audio negotiation document (SPA Pod)")
        })
    }
}

/// Typed violation of the strict negotiated SPA audio format contract
/// (G-RB-001 / T4.3).
///
/// Produced by [`validate_audio_raw_format`] when the audio server renegotiates
/// a format diverging from the invariant that both streams operate under
/// `F32P` planar stereo with exactly 2 channels (FL/FR).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractViolation {
    /// The POD is not a parseable `Audio/Raw` SPA format (missing/mismatched
    /// `mediaType`/`mediaSubtype`, or a malformed/non-format POD).
    NotAudioRaw,
    /// The negotiated sample format is not planar 32-bit float (`F32P`).
    NotF32Planar(pw::spa::param::audio::AudioFormat),
    /// The negotiated channel count is not exactly 2 (stereo FL/FR).
    NotStereo(u32),
    /// The negotiated channel layout does not match FL at channel 0 and FR at channel 1.
    InvalidChannelPositions { ch0: u32, ch1: u32 },
}

impl core::fmt::Display for ContractViolation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAudioRaw => write!(f, "not an Audio/Raw SPA format"),
            Self::NotF32Planar(got) => {
                write!(
                    f,
                    "sample format is {got:?} (contract requires F32P planar)"
                )
            }
            Self::NotStereo(got) => {
                write!(
                    f,
                    "channel count is {got} (contract requires 2 stereo channels)"
                )
            }
            Self::InvalidChannelPositions { ch0, ch1 } => {
                write!(
                    f,
                    "channel positions are [ch0={ch0}, ch1={ch1}] (contract requires FL at ch0 and FR at ch1)"
                )
            }
        }
    }
}

/// Canonical validator of a negotiated SPA audio format (G-RB-001 / T4.3).
///
/// Enforces the strict host contract on both streams (capture and playback):
/// `MediaType::Audio`, `MediaSubtype::Raw`, sample format `F32P` (planar
/// 32-bit float) and exactly 2 channels (stereo FL/FR). Returns the negotiated
/// sample rate on success.
///
/// Any diverging renegotiation (mono, interleaved F32/S16, 5.1 surround, …) is
/// rejected fail-closed with a typed [`ContractViolation`] — the caller raises
/// `RT_STATUS_HOST_CONTRACT_VIOLATION` and surfaces the structured diagnostic.
pub fn validate_audio_raw_format(param: &pw::spa::pod::Pod) -> Result<u32, ContractViolation> {
    let (media_type, media_subtype) = pw::spa::param::format_utils::parse_format(param)
        .map_err(|_| ContractViolation::NotAudioRaw)?;
    if media_type != pw::spa::param::format::MediaType::Audio
        || media_subtype != pw::spa::param::format::MediaSubtype::Raw
    {
        return Err(ContractViolation::NotAudioRaw);
    }

    let mut format = pw::spa::param::audio::AudioInfoRaw::default();
    format
        .parse(param)
        .map_err(|_| ContractViolation::NotAudioRaw)?;

    if format.format() != pw::spa::param::audio::AudioFormat::F32P {
        return Err(ContractViolation::NotF32Planar(format.format()));
    }
    let channels = format.channels();
    if channels != 2 {
        return Err(ContractViolation::NotStereo(channels));
    }
    let pos = format.position();
    if pos.len() < 2
        || pos[0] != pw::spa::sys::SPA_AUDIO_CHANNEL_FL
        || pos[1] != pw::spa::sys::SPA_AUDIO_CHANNEL_FR
    {
        return Err(ContractViolation::InvalidChannelPositions {
            ch0: pos.first().copied().unwrap_or(0),
            ch1: pos.get(1).copied().unwrap_or(0),
        });
    }
    Ok(format.rate())
}

/// Fail-closed runtime reaction to a diverging SPA format negotiation
/// (G-RB-001 / T4.3).
///
/// Called from the `param_changed` listeners (PipeWire ThreadLoop thread,
/// non-RT): raises `RT_STATUS_HOST_CONTRACT_VIOLATION` on `rt_status` so the
/// backend state machine (main control loop) observes the degraded/error
/// state, latches the audio-level mute guard ([`RtStatusFlags::format_contract_ok`])
/// and emits a structured diagnostic error naming the offending stream and the
/// exact violation.
#[cold]
pub fn reject_negotiated_format_violation(
    rt_status: &RtStatusFlags,
    stream_name: &str,
    violation: ContractViolation,
) {
    rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    if stream_name == "capture" {
        rt_status.capture_format_ok.store(0, Ordering::Relaxed);
    } else if stream_name == "playback" {
        rt_status.playback_format_ok.store(0, Ordering::Relaxed);
    }
    rt_status.format_contract_ok.store(0, Ordering::Relaxed);
    log::error!(
        "Audio host renegotiated an incompatible SPA format on the {stream_name} stream — \
         strict contract violated. [E2304 | SPA_FORMAT_CONTRACT_VIOLATION] stream={stream_name} violation={violation}"
    );
}

/// Restores the SPA format contract latch after a valid `F32P` planar stereo
/// renegotiation (G-RB-001 / T4.3).
///
/// Called by the `param_changed` listeners on the successful path; re-arms the
/// RT mute guard so audio processing resumes automatically.
#[inline]
pub fn mark_format_contract_ok(rt_status: &RtStatusFlags, stream_name: &str) {
    if stream_name == "capture" {
        rt_status.capture_format_ok.store(1, Ordering::Relaxed);
    } else if stream_name == "playback" {
        rt_status.playback_format_ok.store(1, Ordering::Relaxed);
    }
    let cap = rt_status.capture_format_ok.load(Ordering::Relaxed);
    let pb = rt_status.playback_format_ok.load(Ordering::Relaxed);
    rt_status
        .format_contract_ok
        .store(if cap != 0 && pb != 0 { 1 } else { 0 }, Ordering::Relaxed);
}

/// Updates the stream active latch on `RtStatusFlags` (T1.4).
///
/// When a stream enters `Streaming`, `active` is `true` (latches `1`).
/// When a stream is `Paused`, `Unconnected` or in `Error`, `active` is `false` (latches `0`),
/// causing [`RtStatusFlags::is_audio_unmuted`] to fail-closed mute audio on the RT thread.
#[inline]
pub fn mark_stream_active(rt_status: &RtStatusFlags, stream_name: &str, active: bool) {
    let val = if active { 1 } else { 0 };
    if stream_name == "capture" {
        rt_status.capture_active.store(val, Ordering::Release);
    } else if stream_name == "playback" {
        rt_status.playback_active.store(val, Ordering::Release);
    }
}

/// Returns `Some((capture, playback))` when both streams have negotiated a
/// sample rate and the rates are discrepant (G-RB-001 / T4.3).
///
/// Both cells are written by the respective `param_changed` listeners (cold
/// path). A `None` result means at least one stream never negotiated or the
/// rates agree.
#[inline]
pub fn negotiated_rate_mismatch(rt_status: &RtStatusFlags) -> Option<(u32, u32)> {
    let capture = rt_status.capture_negotiated_rate.load(Ordering::Acquire);
    let playback = rt_status.playback_negotiated_rate.load(Ordering::Acquire);
    (capture != 0 && playback != 0 && capture != playback).then_some((capture, playback))
}

/// Warns when the capture and playback streams operate on discrepant
/// negotiated sample rates (G-RB-001 / T4.3, "Sincronização de Sample Rate").
///
/// Whenever one stream renegotiates a rate that diverges from the other's
/// currently negotiated rate, the mismatch is surfaced as a warning so the
/// operator knows the streams drift (resampler pressure / clock skew).
#[cold]
pub fn check_negotiated_rate_mismatch(rt_status: &RtStatusFlags) {
    if let Some((capture, playback)) = negotiated_rate_mismatch(rt_status) {
        log::warn!(
            "Audio streams operate at discrepant negotiated sample rates — clock drift and \
             resampler pressure expected. [E2305 | RATE_MISMATCH] capture={capture} playback={playback}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neural_amp_modeler_rs::dsp::pipeline::MAX_BRIDGE_BUF;

    #[test]
    fn spa_pod_storage_is_8_byte_aligned() {
        assert!(std::mem::align_of::<SpaPodStorage<1024>>().is_multiple_of(8));
        assert!(std::mem::align_of::<SpaPodStorage<8>>().is_multiple_of(8));
        assert_eq!(SpaPodStorage::<8>::new().as_slice().len(), 8);
        let storage = SpaPodStorage::<8>::default();
        assert!(storage.as_slice().iter().all(|&b| b == 0));
    }

    #[test]
    fn build_spa_format_pod_returns_aligned_pod_within_storage() {
        pw::init();
        let mut audio_info = pw::spa::param::audio::AudioInfoRaw::new();
        audio_info.set_format(pw::spa::param::audio::AudioFormat::F32P);
        audio_info.set_channels(2);
        let mut storage = SpaPodStorage::new();
        let base = storage.as_slice().as_ptr() as usize;
        let len = storage.as_slice().len();
        // SAFETY: storage outlives the returned pod for the duration of the test.
        let pod = unsafe { build_spa_format_pod(&audio_info, &mut storage) }.expect("pod build");
        let pod_addr = pod.as_raw_ptr() as usize;
        assert!(pod_addr.is_multiple_of(8));
        assert!(pod_addr >= base);
        assert!(pod_addr + std::mem::size_of::<pw::spa::sys::spa_pod>() <= base + len);
        assert_eq!(pod.type_(), pw::spa::utils::SpaTypes::Object);
    }

    #[test]
    fn validate_built_pod_rejects_null_and_out_of_bounds_pointers() {
        let storage = SpaPodStorage::new();
        assert!(validate_built_pod(&storage, std::ptr::null()).is_none());

        // One-past-the-end is outside the storage -> rejected.
        let base = storage.as_slice().as_ptr();
        let one_past_end = unsafe { base.add(storage.as_slice().len()) }.cast();
        assert!(validate_built_pod(&storage, one_past_end).is_none());
    }

    #[test]
    fn validate_built_pod_rejects_misaligned_pointer() {
        let storage = SpaPodStorage::new();
        let base = storage.as_slice().as_ptr() as usize;
        let misaligned = (base + 4) as *const pw::spa::sys::spa_pod;
        assert!(validate_built_pod(&storage, misaligned).is_none());
    }

    #[test]
    fn validate_built_pod_rejects_header_claiming_oversized_body() {
        let mut storage = SpaPodStorage::new();
        {
            let bytes = storage.as_mut_slice();
            bytes[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
            bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
        }
        let pod_ptr = storage.as_slice().as_ptr().cast();
        assert!(validate_built_pod(&storage, pod_ptr).is_none());
    }

    #[test]
    fn validate_built_pod_accepts_valid_header_within_storage() {
        let mut storage = SpaPodStorage::new();
        {
            let bytes = storage.as_mut_slice();
            bytes[0..4].copy_from_slice(&16u32.to_le_bytes());
            bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
        }
        let pod_ptr = storage.as_slice().as_ptr().cast();
        let pod = validate_built_pod(&storage, pod_ptr).expect("valid pod");
        assert_eq!(
            pod.as_raw_ptr() as usize,
            storage.as_slice().as_ptr() as usize
        );
        assert_eq!(pod.size(), 16);
    }

    // ── Malformed FFI/SPA playback harness (F-RB-003 Part 3 / T1.5) ──────────
    //
    // `playback_dsp_cycle` funnels every dequeued output buffer through
    // `handle_spa_pair_fail_closed` with the playback window `(0, n_bytes)`.
    // These tests feed that exact function raw pointer values (no live PipeWire
    // stream required) and assert the malformed descriptors are rejected
    // fail-closed: `RT_STATUS_HOST_CONTRACT_VIOLATION` raised, no panic, and
    // the output channels silenced.

    /// Creates an SPA chunk descriptor with the given valid-data window.
    fn chunk_of(offset: u32, size: u32) -> pw::spa::sys::spa_chunk {
        pw::spa::sys::spa_chunk {
            offset,
            size,
            stride: std::mem::size_of::<f32>() as i32,
            flags: 0,
        }
    }

    fn fill_bytes(buf: &mut [f32], byte: u8) {
        let bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len() * 4) };
        bytes.fill(byte);
    }

    #[test]
    fn playback_harness_accepts_valid_disjoint_output_buffers() {
        let l = [0.0f32; 16];
        let r = [0.0f32; 16];
        let chunk = chunk_of(0, 64);
        let rt = RtStatusFlags::default();
        let got = handle_spa_pair_fail_closed(
            l.as_ptr() as usize,
            l.len() * 4,
            &chunk,
            0,
            64,
            r.as_ptr() as usize,
            r.len() * 4,
            &chunk,
            0,
            64,
            &rt,
        );
        assert_eq!(got, Some((64, 16)));
        assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_harness_rejects_identical_output_buffers() {
        let buf = [0.0f32; 32];
        let chunk = chunk_of(0, 128);
        let rt = RtStatusFlags::default();
        let p = buf.as_ptr() as usize;
        let m = buf.len() * 4;
        // A malformed host handing the same output buffer to both channels must
        // be rejected fail-closed (identical intervals -> aliasing).
        assert_eq!(
            handle_spa_pair_fail_closed(p, m, &chunk, 0, 128, p, m, &chunk, 0, 128, &rt),
            None
        );
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_harness_rejects_partial_overlap() {
        let buf = [0.0f32; 32];
        let chunk = chunk_of(0, 128);
        let rt = RtStatusFlags::default();
        let p = buf.as_ptr() as usize;
        let m = buf.len() * 4;
        // L: [0, 64); R: [32, 96) inside the same buffer -> partial overlap.
        assert_eq!(
            handle_spa_pair_fail_closed(p, m, &chunk, 0, 64, p, m, &chunk, 32, 64, &rt),
            None
        );
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_harness_rejects_misaligned_output_base_1_2_3() {
        let buf = [0.0f32; 32];
        let chunk = chunk_of(0, 64);
        let base = buf.as_ptr() as usize;
        let m = buf.len() * 4;
        for delta in [1usize, 2, 3] {
            let rt = RtStatusFlags::default();
            assert_eq!(
                handle_spa_pair_fail_closed(
                    base + delta,
                    m.saturating_sub(delta),
                    &chunk,
                    0,
                    64,
                    base,
                    m,
                    &chunk,
                    0,
                    64,
                    &rt,
                ),
                None,
                "delta={delta}"
            );
            assert!(
                rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
                "delta={delta}"
            );
        }
    }

    #[test]
    fn playback_harness_rejects_null_output_chunk() {
        let buf = [0.0f32; 32];
        let chunk = chunk_of(0, 128);
        let rt = RtStatusFlags::default();
        let p = buf.as_ptr() as usize;
        let m = buf.len() * 4;
        assert_eq!(
            handle_spa_pair_fail_closed(p, m, &chunk, 0, 128, p, m, std::ptr::null(), 0, 128, &rt),
            None
        );
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_harness_rejects_odd_output_size() {
        let l = [0.0f32; 32];
        let r = [0.0f32; 32];
        let rt = RtStatusFlags::default();
        let chunk = chunk_of(0, 0);
        assert_eq!(
            handle_spa_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &chunk,
                0,
                6,
                r.as_ptr() as usize,
                r.len() * 4,
                &chunk,
                0,
                6,
                &rt,
            ),
            None
        );
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_harness_rejects_quantum_exceeding_max_bridge_buf() {
        // G-RB-003 / T6.2: a playback window of (MAX_BRIDGE_BUF + 1) frames
        // (32,772 bytes) must be rejected fail-closed — flag raised and both
        // output channels silenced — before any copy into the DSP buffers.
        let mut l = [0.0f32; MAX_BRIDGE_BUF + 1];
        let mut r = [0.0f32; MAX_BRIDGE_BUF + 1];
        fill_bytes(&mut l, 0x5A);
        fill_bytes(&mut r, 0xA5);
        let rt = RtStatusFlags::default();
        let size = (MAX_BRIDGE_BUF + 1) * 4;
        let chunk = chunk_of(0, size as u32);

        assert_eq!(
            handle_spa_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &chunk,
                0,
                size,
                r.as_ptr() as usize,
                r.len() * 4,
                &chunk,
                0,
                size,
                &rt,
            ),
            None
        );
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
        assert!(
            l[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0)
                && r[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
            "oversized playback window must silence both output channels fail-closed up to MAX_BRIDGE_BUF"
        );
    }

    #[test]
    fn playback_harness_accepts_max_bridge_buf_quantum() {
        // Boundary: exactly MAX_BRIDGE_BUF frames (32,768 bytes) is the largest
        // legal playback window and must be accepted.
        let l = [0.0f32; MAX_BRIDGE_BUF];
        let r = [0.0f32; MAX_BRIDGE_BUF];
        let rt = RtStatusFlags::default();
        let size = MAX_BRIDGE_BUF * 4;
        let chunk = chunk_of(0, size as u32);

        assert_eq!(
            handle_spa_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &chunk,
                0,
                size,
                r.as_ptr() as usize,
                r.len() * 4,
                &chunk,
                0,
                size,
                &rt,
            ),
            Some((size, MAX_BRIDGE_BUF))
        );
        assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_harness_rejects_size_overflow_beyond_maxsize() {
        let l = [0.0f32; 16];
        let r = [0.0f32; 16];
        let rt = RtStatusFlags::default();
        let chunk = chunk_of(0, 64);
        // offset 48 + size 32 = 80 > 64 -> out of bounds (no silent clamp).
        assert_eq!(
            handle_spa_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &chunk,
                0,
                64,
                r.as_ptr() as usize,
                r.len() * 4,
                &chunk,
                48,
                32,
                &rt,
            ),
            None
        );
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_harness_rejects_asymmetric_frame_counts() {
        let l = [0.0f32; 64];
        let r = [0.0f32; 32];
        let rt = RtStatusFlags::default();
        let chunk = chunk_of(0, 0);
        assert_eq!(
            handle_spa_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &chunk,
                0,
                l.len() * 4,
                r.as_ptr() as usize,
                r.len() * 4,
                &chunk,
                0,
                r.len() * 4,
                &rt,
            ),
            None
        );
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_harness_silences_channels_on_violation() {
        let mut buf = [0.0f32; 32];
        fill_bytes(&mut buf, 0x5A);
        let chunk = chunk_of(0, 128);
        let rt = RtStatusFlags::default();
        let p = buf.as_ptr() as usize;
        let m = buf.len() * 4;
        // Malformed partial-overlap pair: flag raised, both channels silenced,
        // no panic (reaching the end of this test proves it).
        let res = handle_spa_pair_fail_closed(p, m, &chunk, 0, 64, p, m, &chunk, 32, 64, &rt);
        assert_eq!(res, None);
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
        assert!(
            buf.iter().all(|&s| s == 0.0),
            "playback fail-closed path must silence the output channels"
        );
    }

    // ── Bridge-starvation silence policy (G-RB-001 / T4.2) ────────────────────
    //
    // When `bridge.read_block` yields no new DSP block, `playback_dsp_cycle`
    // must still dequeue, validate, silence-fill and recycle the output buffer.
    // The pure kernel `deliver_silence_pair_fail_closed` is exercised with raw
    // SPA descriptors (the mock stream) and proves: 100% of the output
    // extension is zeroed (zero residual/stale audio), the chunk metadata is
    // stamped consistently, no buffer miss is counted and the starvation
    // telemetry counter advances.

    #[test]
    fn playback_bridge_starvation_zeroes_full_extension_and_stamps_chunks() {
        let mut l = [1.0f32; 32];
        let mut r = [1.0f32; 32];
        fill_bytes(&mut l, 0x5A);
        fill_bytes(&mut r, 0xA5);
        // Stale chunk metadata from a previous cycle — the silent recycling
        // path must overwrite it deterministically.
        let mut chunk_l = pw::spa::sys::spa_chunk {
            offset: 7,
            size: 4,
            stride: 0,
            flags: 0,
        };
        let mut chunk_r = pw::spa::sys::spa_chunk {
            offset: 3,
            size: 8,
            stride: 2,
            flags: 0,
        };
        let rt = RtStatusFlags::default();

        // SAFETY: `l`/`r` are disjoint `[f32; 32]` arrays (aligned, writable,
        // non-overlapping) and `chunk_l`/`chunk_r` are local, non-null structs
        // that outlive the call.
        let frames = unsafe {
            deliver_silence_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &mut chunk_l,
                r.as_ptr() as usize,
                r.len() * 4,
                &mut chunk_r,
                &rt,
            )
        };

        assert_eq!(frames, Some(32));
        assert!(
            l.iter().all(|&s| s == 0.0),
            "L must be fully silenced (no stale audio residue)"
        );
        assert!(
            r.iter().all(|&s| s == 0.0),
            "R must be fully silenced (no stale audio residue)"
        );
        assert_eq!(chunk_l.offset, 0);
        assert_eq!(chunk_l.size, 32 * 4);
        assert_eq!(chunk_l.stride, 4);
        assert_eq!(chunk_r.offset, 0);
        assert_eq!(chunk_r.size, 32 * 4);
        assert_eq!(chunk_r.stride, 4);
        assert_eq!(
            rt.playback_bridge_starvation.load(Ordering::Relaxed),
            1,
            "the starvation occurrence must be registered on rt_status"
        );
        assert_eq!(
            rt.output_buffer_miss.load(Ordering::Relaxed),
            0,
            "a dequeued silence buffer is not a buffer miss"
        );
        assert!(!rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
    }

    #[test]
    fn playback_bridge_starvation_rejects_aliased_channels_fail_closed() {
        let mut buf = [0.0f32; 32];
        fill_bytes(&mut buf, 0x5A);
        let mut chunk = chunk_of(0, 128);
        let rt = RtStatusFlags::default();
        let p = buf.as_ptr() as usize;
        let m = buf.len() * 4;

        // SAFETY: `buf` is a local, aligned, writable `[f32; 32]` and `chunk`
        // is a local non-null struct; the kernel rejects the aliasing fail-closed.
        let frames =
            unsafe { deliver_silence_pair_fail_closed(p, m, &mut chunk, p, m, &mut chunk, &rt) };

        assert_eq!(frames, None);
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
        assert!(
            buf.iter().all(|&s| s == 0.0),
            "aliased channels must be silenced fail-closed"
        );
        assert_eq!(
            rt.playback_bridge_starvation.load(Ordering::Relaxed),
            0,
            "a host contract violation is not a starvation event"
        );
    }

    #[test]
    fn playback_bridge_starvation_rejects_asymmetric_extensions() {
        let mut l = [1.0f32; 32];
        let mut r = [1.0f32; 16];
        fill_bytes(&mut l, 0x5A);
        fill_bytes(&mut r, 0xA5);
        let mut chunk = chunk_of(0, 0);
        let rt = RtStatusFlags::default();

        // SAFETY: `l`/`r` are local aligned writable arrays and `chunk` is a
        // local non-null struct; the kernel rejects the asymmetric extensions
        // fail-closed.
        let frames = unsafe {
            deliver_silence_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &mut chunk,
                r.as_ptr() as usize,
                r.len() * 4,
                &mut chunk,
                &rt,
            )
        };

        assert_eq!(frames, None);
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
        assert!(
            l.iter().all(|&s| s == 0.0) && r.iter().all(|&s| s == 0.0),
            "asymmetric extensions must silence both channels fail-closed"
        );
        assert_eq!(rt.playback_bridge_starvation.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn playback_bridge_starvation_with_huge_maxsize_bounds_to_max_bridge_buf() {
        // F-RES-001 / T6.1: when host supplies a huge buffer (e.g. 1 MiB or > MAX_BRIDGE_BUF),
        // silence delivery must bound zeroing to MAX_BRIDGE_BUF frames (32 KiB)
        // and chunk.size must match the exact zeroed interval.
        let total_samples = MAX_BRIDGE_BUF + 1024;
        let mut l = vec![0.5f32; total_samples];
        let mut r = vec![0.5f32; total_samples];
        fill_bytes(&mut l, 0x5A);
        fill_bytes(&mut r, 0xA5);
        let mut chunk_l = chunk_of(0, 0);
        let mut chunk_r = chunk_of(0, 0);
        let rt = RtStatusFlags::default();

        let frames = unsafe {
            deliver_silence_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &mut chunk_l,
                r.as_ptr() as usize,
                r.len() * 4,
                &mut chunk_r,
                &rt,
            )
        };

        assert_eq!(frames, None);
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
        assert_eq!(rt.playback_bridge_starvation.load(Ordering::Relaxed), 0);

        // First MAX_BRIDGE_BUF samples must be zeroed
        assert!(
            l[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
            "bounded range must be zeroed"
        );
        assert!(
            r[..MAX_BRIDGE_BUF].iter().all(|&s| s == 0.0),
            "bounded range must be zeroed"
        );

        // Samples past MAX_BRIDGE_BUF must remain untouched
        let trailing_l = unsafe {
            std::slice::from_raw_parts(
                (l.as_ptr() as usize + MAX_BRIDGE_BUF * 4) as *const u8,
                1024 * 4,
            )
        };
        assert!(
            trailing_l.iter().all(|&b| b == 0x5A),
            "trailing memory past MAX_BRIDGE_BUF must not be touched"
        );

        let trailing_r = unsafe {
            std::slice::from_raw_parts(
                (r.as_ptr() as usize + MAX_BRIDGE_BUF * 4) as *const u8,
                1024 * 4,
            )
        };
        assert!(
            trailing_r.iter().all(|&b| b == 0xA5),
            "trailing memory past MAX_BRIDGE_BUF must not be touched"
        );
    }

    // ── SPA format negotiation validator (G-RB-001 / T4.3) ───────────────────
    //
    // `validate_audio_raw_format` is the canonical fail-closed gate applied by
    // both the capture and playback `param_changed` listeners. These tests
    // build real SPA PODs (via `spa_format_audio_raw_build`) and prove that
    // only `F32P` planar stereo passes, while mono, interleaved, S16, surround
    // and non-format PODs are rejected with a typed `ContractViolation`.

    /// Builds a real SPA format POD for the given audio info into `storage`.
    fn build_raw_format_pod<'a>(
        audio_info: &pw::spa::param::audio::AudioInfoRaw,
        storage: &'a mut SpaPodStorage<1024>,
    ) -> &'a pw::spa::pod::Pod {
        // SAFETY: the returned pod borrows `storage`, which outlives the call.
        unsafe { build_spa_format_pod(audio_info, storage) }.expect("pod build")
    }

    fn raw_audio_info(
        format: pw::spa::param::audio::AudioFormat,
        channels: u32,
    ) -> pw::spa::param::audio::AudioInfoRaw {
        let mut info = pw::spa::param::audio::AudioInfoRaw::new();
        info.set_format(format);
        info.set_channels(channels);
        info.set_rate(48_000);
        let mut pos = [0u32; 64];
        pos[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
        pos[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
        info.set_position(pos);
        info
    }

    #[test]
    fn validate_audio_raw_format_accepts_f32p_planar_stereo() {
        pw::init();
        let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(validate_audio_raw_format(pod), Ok(48_000));
    }

    #[test]
    fn validate_audio_raw_format_rejects_swapped_positions_fr_fl() {
        pw::init();
        let mut info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
        let mut pos = [0u32; 64];
        pos[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
        pos[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
        info.set_position(pos);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(ContractViolation::InvalidChannelPositions {
                ch0: pw::spa::sys::SPA_AUDIO_CHANNEL_FR,
                ch1: pw::spa::sys::SPA_AUDIO_CHANNEL_FL,
            })
        );
    }

    #[test]
    fn validate_audio_raw_format_rejects_fc_lfe_positions() {
        pw::init();
        let mut info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
        let mut pos = [0u32; 64];
        pos[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FC;
        pos[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_LFE;
        info.set_position(pos);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(ContractViolation::InvalidChannelPositions {
                ch0: pw::spa::sys::SPA_AUDIO_CHANNEL_FC,
                ch1: pw::spa::sys::SPA_AUDIO_CHANNEL_LFE,
            })
        );
    }

    #[test]
    fn validate_audio_raw_format_rejects_missing_positions() {
        pw::init();
        let mut info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
        let pos = [0u32; 64];
        info.set_position(pos);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(ContractViolation::InvalidChannelPositions { ch0: 0, ch1: 0 })
        );
    }

    #[test]
    fn validate_audio_raw_format_rejects_mono() {
        pw::init();
        let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 1);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(ContractViolation::NotStereo(1))
        );
    }

    #[test]
    fn validate_audio_raw_format_rejects_surround_5_1() {
        pw::init();
        let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 6);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(ContractViolation::NotStereo(6))
        );
    }

    #[test]
    fn validate_audio_raw_format_rejects_interleaved_f32() {
        pw::init();
        let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32LE, 2);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(ContractViolation::NotF32Planar(
                pw::spa::param::audio::AudioFormat::F32LE
            ))
        );
    }

    #[test]
    fn validate_audio_raw_format_rejects_s16() {
        pw::init();
        let info = raw_audio_info(pw::spa::param::audio::AudioFormat::S16, 2);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(ContractViolation::NotF32Planar(
                pw::spa::param::audio::AudioFormat::S16
            ))
        );
    }

    #[test]
    fn validate_audio_raw_format_rejects_non_format_pod() {
        // A well-formed SPA Struct POD (not an Object/Format) must fail
        // `spa_format_parse` fail-closed instead of panicking. The backing
        // buffer is `SpaPodStorage` (8-byte aligned) so `Pod::from_bytes`
        // receives a properly aligned slice (its safety contract requires a
        // well-aligned pod).
        let mut storage = SpaPodStorage::<8>::new();
        {
            let bytes = storage.as_mut_slice();
            bytes[4..8].copy_from_slice(&pw::spa::utils::SpaTypes::Struct.as_raw().to_le_bytes());
        }
        let pod = pw::spa::pod::Pod::from_bytes(storage.as_slice()).expect("struct pod");
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(ContractViolation::NotAudioRaw)
        );
    }

    #[test]
    fn reject_negotiated_format_violation_raises_host_contract_flag_and_latches() {
        let rt = RtStatusFlags::default();
        assert_eq!(
            rt.format_contract_ok.load(Ordering::Relaxed),
            1,
            "the latch defaults to contract-ok"
        );
        reject_negotiated_format_violation(&rt, "capture", ContractViolation::NotStereo(1));
        assert!(rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION));
        assert_eq!(
            rt.format_contract_ok.load(Ordering::Relaxed),
            0,
            "a rejected negotiation must latch the RT mute guard"
        );
        assert_eq!(
            rt.flags_seen.load(Ordering::Relaxed),
            0,
            "the flag is telemetry for the main loop; it is not consumed here"
        );
    }

    #[test]
    fn mark_format_contract_ok_restores_the_rt_mute_guard() {
        let rt = RtStatusFlags::default();
        reject_negotiated_format_violation(
            &rt,
            "playback",
            ContractViolation::NotF32Planar(pw::spa::param::audio::AudioFormat::S16),
        );
        assert_eq!(rt.format_contract_ok.load(Ordering::Relaxed), 0);

        mark_format_contract_ok(&rt, "playback");
        assert_eq!(
            rt.format_contract_ok.load(Ordering::Relaxed),
            1,
            "a subsequent valid F32P stereo negotiation must re-arm audio processing"
        );
    }

    #[test]
    fn negotiated_rate_mismatch_detects_discrepant_streams() {
        let rt = RtStatusFlags::default();
        assert_eq!(negotiated_rate_mismatch(&rt), None);

        rt.capture_negotiated_rate.store(48_000, Ordering::Release);
        assert_eq!(
            negotiated_rate_mismatch(&rt),
            None,
            "single negotiated stream is not a mismatch"
        );

        rt.playback_negotiated_rate.store(48_000, Ordering::Release);
        assert_eq!(
            negotiated_rate_mismatch(&rt),
            None,
            "equal negotiated rates are not a mismatch"
        );

        rt.playback_negotiated_rate.store(44_100, Ordering::Release);
        assert_eq!(
            negotiated_rate_mismatch(&rt),
            Some((48_000, 44_100)),
            "discrepant negotiated rates must be reported"
        );
    }
}
