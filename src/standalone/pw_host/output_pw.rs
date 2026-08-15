// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire output DSP pipeline and host configuration (standalone).

use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use neural_amp_modeler_rs::dsp::pipeline::DspBridgeReader;
use pipewire as pw;
use std::sync::atomic::Ordering;

use super::rt_callback::check_ffi_contract;

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
    /// Full WaveNet model stored for main-thread slimmable rebuild.
    pub full_wavenet_model: Option<Box<neural_amp_modeler_rs::models::StaticModel>>,
    /// Producer to send slimmable-rebuilt models to the audio thread.
    pub slimmable_producer: rtrb::Producer<Option<Box<neural_amp_modeler_rs::models::StaticModel>>>,
    /// Producer to send oversampling engines rebuilt on main thread to the audio thread.
    pub os_producer: rtrb::Producer<Box<neural_amp_modeler_rs::dsp::oversample::OsEnginePair>>,
    /// Initial oversampling factor for the neural stage.
    pub oversample: OversampleFactor,
}

/// Playback FFI contract validation for both output channels.
///
/// Returns `Some((n_bytes, n_samples))` when both raw buffers are present,
/// aligned to `align_of::<f32>()`, and large enough to hold `n_samples`
/// within their `maxsize`; `None` otherwise. A `None` verdict must skip the
/// quantum and set `RT_STATUS_HOST_CONTRACT_VIOLATION` — never clamp or cast.
#[inline(always)]
fn check_playback_contract(
    raw_l: Option<&[u8]>,
    raw_r: Option<&[u8]>,
    max_l: usize,
    max_r: usize,
    n_samples: usize,
) -> Option<(usize, usize)> {
    let n_bytes = n_samples * std::mem::size_of::<f32>();
    if n_samples == 0 || n_bytes > max_l || n_bytes > max_r {
        return None;
    }
    let (bl, sl) = check_ffi_contract(raw_l?, 0, n_bytes)?;
    let (br, sr) = check_ffi_contract(raw_r?, 0, n_bytes)?;
    if bl != n_bytes || sl != n_samples || br != n_bytes || sr != n_samples {
        return None;
    }
    Some((n_bytes, n_samples))
}

/// Playback DSP Pipeline (Bridge → Hardware).
#[inline(always)]
pub fn playback_dsp_cycle(
    stream: &pw::stream::Stream,
    bridge: DspBridgeReader,
    last_bridge_gen: &mut u64,
    rt_status: &RtStatusFlags,
) {
    bridge.read_block(last_bridge_gen, |buf_l, buf_r| {
        let n_samples = buf_l.len();
        if n_samples == 0 {
            return;
        }

        let mut buf = match stream.dequeue_buffer() {
            Some(b) => b,
            None => {
                rt_status.output_buffer_miss.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };

        let datas = buf.datas_mut();
        if datas.len() < 2 {
            return;
        }

        // Splits the Left and Right channels for final delivery.
        let (datas_left, datas_right) = datas.split_at_mut(1);
        let data_l = &mut datas_left[0];
        let data_r = &mut datas_right[0];

        let max_l = data_l.as_raw().maxsize as usize;
        let max_r = data_r.as_raw().maxsize as usize;

        let raw_l = data_l.data();
        let raw_r = data_r.data();

        let Some((_n_bytes, n_out)) =
            check_playback_contract(raw_l.as_deref(), raw_r.as_deref(), max_l, max_r, n_samples)
        else {
            rt_status.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
            return;
        };

        // Copies the processed sound directly to your sound card outputs.
        if let Some(raw_l) = raw_l {
            let out_l =
                unsafe { std::slice::from_raw_parts_mut(raw_l.as_mut_ptr().cast::<f32>(), n_out) };
            unsafe {
                core::ptr::copy_nonoverlapping(buf_l.as_ptr(), out_l.as_mut_ptr(), n_out);
            }
        }
        if let Some(raw_r) = raw_r {
            let out_r =
                unsafe { std::slice::from_raw_parts_mut(raw_r.as_mut_ptr().cast::<f32>(), n_out) };
            unsafe {
                core::ptr::copy_nonoverlapping(buf_r.as_ptr(), out_r.as_mut_ptr(), n_out);
            }
        }

        // Informs the hardware exactly how much sound was delivered this time.
        {
            let chunk = data_l.chunk_mut();
            *chunk.size_mut() = (n_out * std::mem::size_of::<f32>()) as u32;
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = std::mem::size_of::<f32>() as i32;
        }
        {
            let chunk = data_r.chunk_mut();
            *chunk.size_mut() = (n_out * std::mem::size_of::<f32>()) as u32;
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = std::mem::size_of::<f32>() as i32;
        }
    });
}

/// Builds an F32P stereo audio format SPA Pod for PipeWire negotiation.
///
/// # Safety
/// The returned binary pod points directly to the provided `format_buf`.
pub unsafe fn build_spa_format_pod<'a>(
    audio_info: &pw::spa::param::audio::AudioInfoRaw,
    format_buf: &'a mut [u8; 1024],
) -> anyhow::Result<&'a pw::spa::pod::Pod> {
    unsafe {
        let mut builder: pw::spa::sys::spa_pod_builder = std::mem::zeroed();
        // Prepares a "builder" to create the audio format contract.
        pw::spa::sys::spa_pod_builder_init(
            &mut builder,
            format_buf.as_mut_ptr().cast(),
            format_buf.len() as u32,
        );

        // Builds the binary document (SPA Pod) describing the audio (e.g.: 48kHz, Stereo).
        // This document is what PipeWire uses to understand how to send sound to us.
        let pod_ptr = pw::spa::sys::spa_format_audio_raw_build(
            &mut builder,
            pw::spa::param::ParamType::EnumFormat.as_raw(),
            &audio_info.as_raw(),
        );

        if pod_ptr.is_null() {
            // If failure, the system won't know how to negotiate sound with your card.
            return Err(anyhow::anyhow!(
                "Failed to build the audio negotiation document (SPA Pod)"
            ));
        }

        // Returns the contract ready to be signed and used by the system.
        Ok(&*(pod_ptr as *const pw::spa::pod::Pod))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_contract_accepts_aligned_buffers_within_maxsize() {
        let buf = [0u8; 64];
        let raw = &buf[..];
        assert!(check_playback_contract(Some(raw), Some(raw), 64, 64, 16).is_some());
        assert!(check_playback_contract(Some(raw), Some(raw), 64, 64, 4).is_some());
    }

    #[test]
    fn playback_contract_rejects_claimed_larger_than_maxsize() {
        let buf = [0u8; 16];
        let raw = &buf[..];
        // 8 samples need 32 bytes, but maxsize is only 16 -> rejected.
        assert!(check_playback_contract(Some(raw), Some(raw), 16, 16, 8).is_none());
        // Asymmetric maxsize: right channel too small -> rejected.
        assert!(check_playback_contract(Some(raw), Some(raw), 16, 8, 4).is_none());
    }

    #[test]
    fn playback_contract_rejects_missing_or_unaligned_buffer() {
        let buf = [0u8; 64];
        let raw = &buf[..];
        // Missing raw buffer -> rejected.
        assert!(check_playback_contract(None, Some(raw), 64, 64, 4).is_none());
        assert!(check_playback_contract(Some(raw), None, 64, 64, 4).is_none());

        // Misaligned base pointer -> rejected.
        let storage = [0u8; 4 + 64];
        let base = storage.as_ptr() as usize;
        let align = std::mem::align_of::<f32>();
        let mut delta = 1usize;
        while (base + delta).is_multiple_of(align) {
            delta += 1;
        }
        let misaligned = &storage[delta..];
        assert!(check_playback_contract(Some(misaligned), Some(raw), 64, 64, 4).is_none());
    }
}
