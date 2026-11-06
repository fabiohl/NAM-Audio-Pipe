// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! SPA format POD construction and negotiated audio format contract validation.

use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use pipewire as pw;
use std::sync::atomic::Ordering;

use super::stream_status::StreamStatusFlags;

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
pub(crate) fn validate_built_pod(
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

/// Typed violation of the strict negotiated SPA audio format contract.
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

/// Canonical validator of a negotiated SPA audio format.
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

/// Fail-closed runtime reaction to a diverging SPA format negotiation.
///
/// Called from the `param_changed` listeners (PipeWire ThreadLoop thread,
/// non-RT): raises `RT_STATUS_HOST_CONTRACT_VIOLATION` on `rt_status` so the
/// backend state machine (main control loop) observes the degraded/error
/// state, latches the audio-level mute guard ([`StreamStatusFlags::format_contract_ok`])
/// and emits a structured diagnostic error naming the offending stream and the
/// exact violation.
#[cold]
pub fn reject_negotiated_format_violation(
    rt_status: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
    stream_name: &str,
    violation: ContractViolation,
) {
    rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
    if stream_name == "capture" {
        stream_status.capture_format_ok.store(0, Ordering::Relaxed);
    } else if stream_name == "playback" {
        stream_status.playback_format_ok.store(0, Ordering::Relaxed);
    }
    stream_status.format_contract_ok.store(0, Ordering::Relaxed);
    log::error!(
        "Audio host renegotiated an incompatible SPA format on the {stream_name} stream — \
         strict contract violated. [E2304 | SPA_FORMAT_CONTRACT_VIOLATION] stream={stream_name} violation={violation}"
    );
}

/// Restores the SPA format contract latch after a valid `F32P` planar stereo
/// renegotiation.
///
/// Called by the `param_changed` listeners on the successful path; re-arms the
/// RT mute guard so audio processing resumes automatically.
#[inline]
pub fn mark_format_contract_ok(stream_status: &StreamStatusFlags, stream_name: &str) {
    if stream_name == "capture" {
        stream_status.capture_format_ok.store(1, Ordering::Relaxed);
    } else if stream_name == "playback" {
        stream_status.playback_format_ok.store(1, Ordering::Relaxed);
    }
    let cap = stream_status.capture_format_ok.load(Ordering::Relaxed);
    let pb = stream_status.playback_format_ok.load(Ordering::Relaxed);
    stream_status
        .format_contract_ok
        .store(if cap != 0 && pb != 0 { 1 } else { 0 }, Ordering::Relaxed);
}

/// Updates the stream active latch on `StreamStatusFlags`.
///
/// When a stream enters `Streaming`, `active` is `true` (latches `1`).
/// When a stream is `Paused`, `Unconnected` or in `Error`, `active` is `false` (latches `0`),
/// causing [`StreamStatusFlags::is_audio_unmuted`] to fail-closed mute audio on the RT thread.
#[inline]
pub fn mark_stream_active(stream_status: &StreamStatusFlags, stream_name: &str, active: bool) {
    let val = if active { 1 } else { 0 };
    if stream_name == "capture" {
        stream_status.capture_active.store(val, Ordering::Release);
    } else if stream_name == "playback" {
        stream_status.playback_active.store(val, Ordering::Release);
    }
}

/// Returns `Some((capture, playback))` when both streams have negotiated a
/// sample rate and the rates are discrepant.
///
/// Both cells are written by the respective `param_changed` listeners (cold
/// path). A `None` result means at least one stream never negotiated or the
/// rates agree.
#[inline]
pub fn negotiated_rate_mismatch(stream_status: &StreamStatusFlags) -> Option<(u32, u32)> {
    let capture = stream_status
        .capture_negotiated_rate
        .load(Ordering::Acquire);
    let playback = stream_status
        .playback_negotiated_rate
        .load(Ordering::Acquire);
    (capture != 0 && playback != 0 && capture != playback).then_some((capture, playback))
}

/// Warns when the capture and playback streams operate on discrepant
/// negotiated sample rates.
///
/// Whenever one stream renegotiates a rate that diverges from the other's
/// currently negotiated rate, the mismatch is surfaced as a warning so the
/// operator knows the streams drift (resampler pressure / clock skew).
#[cold]
pub fn check_negotiated_rate_mismatch(stream_status: &StreamStatusFlags) {
    if let Some((capture, playback)) = negotiated_rate_mismatch(stream_status) {
        log::warn!(
            "Audio streams operate at discrepant negotiated sample rates — clock drift and \
             resampler pressure expected. [E2307 | RATE_MISMATCH] capture={capture} playback={playback}"
        );
    }
}
