// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire output DSP pipeline and host configuration (standalone).

use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::pipeline::{DspBridgeReader, MAX_BRIDGE_BUF};
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
    /// Noise gate enabled flag from CLI (`--gate on|off`).
    pub gate_enabled: bool,
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

/// Deterministic silence delivery for bridge starvation (G-RB-001 / T4.2,
/// S5 / E2304).
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
/// The caller ([`deliver_silence_block`]) quantizes `silence_bytes` from the
/// active stream quantum (`last_n_samples`/`requested_buffer_frames`, 128-frame
/// fallback), bounded by both channel capacities and `MAX_BRIDGE_BUF × 4`, so
/// pausing the input or starting before the first block never raises
/// `RT_STATUS_HOST_CONTRACT_VIOLATION` (`E2304`).
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
) -> Option<usize> {
    let (_n_bytes, n_out) = handle_spa_pair_fail_closed(
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
/// with analytical silence and recycles it (G-RB-001 / T4.2, S5 / E2304).
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

    // S5 / E2304: silence quantum = last active frame count (or requested
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
        )
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
#[path = "output_pw_test.rs"]
mod output_pw_test;
