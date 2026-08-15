// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! 5.1.4. REAL-TIME DSP LOGIC
//! Acquires the raw system buffer and delegates the heavy lifting to the Audio Factory (pipeline).

use crate::recording::buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, RingPayload,
};
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use neural_amp_modeler_rs::dsp::pipeline::{DspBuffers, DspPipelineContext, capture_dsp_pipeline};

use pipewire as pw;
use rtrb::Producer;
use std::sync::atomic::Ordering;

/// Runtime FFI contract validation for a single PipeWire buffer channel.
///
/// Returns `(n_bytes, n_samples)` if the contract is satisfied, or `None` if
/// the buffer violates bounds or alignment guarantees. Both the base pointer
/// and the intra-buffer `offset` must be aligned to `align_of::<f32>()`, since
/// the caller subsequently reinterprets the region as `&mut [f32]` (an
/// unaligned `f32` slice is undefined behavior).
#[inline(always)]
pub(crate) fn check_ffi_contract(raw: &[u8], offset: usize, size: usize) -> Option<(usize, usize)> {
    let align = std::mem::align_of::<f32>();
    if !(raw.as_ptr() as usize).is_multiple_of(align) || !offset.is_multiple_of(align) {
        return None;
    }
    let n_bytes = size.min(raw.len().saturating_sub(offset));
    let n_samples = n_bytes / std::mem::size_of::<f32>();
    if offset + n_bytes > raw.len() || n_samples * std::mem::size_of::<f32>() != n_bytes {
        return None;
    }
    Some((n_bytes, n_samples))
}

/// Attempts to send recording metadata for `current_host_rate`.
///
/// The sticky `recording_meta_sent` flag is confirmed only after a successful
/// `push(Metadata)`; a failed push (or an absent producer) leaves it false so
/// the next callback retries. A host sample-rate change invalidates the flag so
/// a new header is emitted for the new rate.
#[inline(always)]
fn send_recording_metadata(
    recording_producer: &mut Option<Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    current_host_rate: u32,
    recording_meta_sent: &mut bool,
    recording_meta_rate: &mut u32,
) {
    if *recording_meta_sent && *recording_meta_rate == current_host_rate {
        return;
    }
    *recording_meta_sent = false;
    if let Some(producer) = recording_producer {
        let meta = AudioMetadata {
            sample_rate: current_host_rate as f32,
            bit_depth: 32,
            channels: 2,
        };
        if producer.push(RingPayload::Metadata(meta)).is_ok() {
            *recording_meta_sent = true;
            *recording_meta_rate = current_host_rate;
        }
    }
}

/// Pushes one interleaved audio block to the recording producer.
///
/// Blocks whose interleaved length exceeds `MAX_BLOCK_SIZE` are dropped and
/// counted in `OVERRUN_COUNT` (fail-closed telemetry) instead of silently
/// vanishing. A full ring also increments the counter. The reusable block is
/// swapped out with a fresh uninitialized one to avoid 16 KiB of memset per
/// quantum in the RT hot path.
#[inline(always)]
fn send_recording_audio(
    recording_producer: &mut Option<Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    n_pw: usize,
    resamp_out_l: &[f32],
    resamp_out_r: &[f32],
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
) {
    if n_pw == 0 {
        return;
    }
    let Some(producer) = recording_producer else {
        return;
    };
    let interleaved_len = n_pw * 2;
    if interleaved_len > MAX_BLOCK_SIZE {
        OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut block = std::mem::replace(recording_block, AlignedBlock::new_uninit());
    for i in 0..n_pw {
        block.data[i * 2] = resamp_out_l[i];
        block.data[i * 2 + 1] = resamp_out_r[i];
    }
    block.valid_len = interleaved_len;
    if producer.push(RingPayload::Audio(block)).is_err() {
        OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// 5.1.4. REAL-TIME DSP LOGIC
/// Acquires the raw system buffer and delegates the heavy lifting to the Audio Factory (pipeline).
#[inline(always)]
#[expect(
    clippy::too_many_arguments,
    reason = "Core DSP kernel signature required by design; recording parameters are minimal additions"
)]
pub fn process_dsp_buffer(
    stream: &pw::stream::Stream,
    context: DspPipelineContext,
    buffers: DspBuffers,
    current_host_rate: u32,
    frame_count: &mut u32,
    rt_status_for_process: &RtStatusFlags,
    recording_producer: &mut Option<Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    recording_meta_sent: &mut bool,
    recording_meta_rate: &mut u32,
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
) {
    let mut _buf = match stream.dequeue_buffer() {
        Some(b) => b,
        None => {
            rt_status_for_process
                .input_buffer_miss
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let datas = _buf.datas_mut();
    if datas.len() >= 2 {
        let (left_datas, right_datas) = datas.split_at_mut(1);
        if let (Some(d_l), Some(d_r)) = (left_datas.first_mut(), right_datas.first_mut()) {
            let chunk_l = d_l.chunk();
            let chunk_r = d_r.chunk();
            let offset_l = chunk_l.offset() as usize;
            let size_l = chunk_l.size() as usize;
            let offset_r = chunk_r.offset() as usize;
            let size_r = chunk_r.size() as usize;

            if let (Some(raw_l), Some(raw_r)) = (d_l.data(), d_r.data()) {
                let contract_l = check_ffi_contract(raw_l, offset_l, size_l);
                let contract_r = check_ffi_contract(raw_r, offset_r, size_r);

                let (_n_bytes_l, n_samples_l, _n_bytes_r, n_samples_r) =
                    match (contract_l, contract_r) {
                        (Some((bl, sl)), Some((br, sr))) => (bl, sl, br, sr),
                        _ => {
                            rt_status_for_process.set_flag(RT_STATUS_HOST_CONTRACT_VIOLATION);
                            raw_l.fill(0);
                            raw_r.fill(0);
                            return;
                        }
                    };

                let n_samples = n_samples_l.min(n_samples_r);

                if n_samples > 0 {
                    let samples_l = unsafe {
                        std::slice::from_raw_parts_mut(
                            raw_l.as_mut_ptr().add(offset_l).cast::<f32>(),
                            n_samples,
                        )
                    };
                    let samples_r = unsafe {
                        std::slice::from_raw_parts_mut(
                            raw_r.as_mut_ptr().add(offset_r).cast::<f32>(),
                            n_samples,
                        )
                    };

                    send_recording_metadata(
                        recording_producer,
                        current_host_rate,
                        recording_meta_sent,
                        recording_meta_rate,
                    );

                    let should_measure = (*frame_count & 0xF) == 0;
                    *frame_count = frame_count.wrapping_add(1);

                    let start_nanos = if should_measure {
                        rt_setup::rdtsc_nanos()
                    } else {
                        0
                    };

                    if (*frame_count & 0x3FF) == 0 {
                        unsafe {
                            neural_amp_modeler_rs::math::common::set_daz_ftz();
                        }
                    }

                    let n_pw = capture_dsp_pipeline(
                        samples_l,
                        samples_r,
                        n_samples,
                        context,
                        DspBuffers {
                            resamp_mid_l: &mut *buffers.resamp_mid_l,
                            resamp_mid_r: &mut *buffers.resamp_mid_r,
                            resamp_out_l: &mut *buffers.resamp_out_l,
                            resamp_out_r: &mut *buffers.resamp_out_r,
                            model_out_l: &mut *buffers.model_out_l,
                            model_out_r: &mut *buffers.model_out_r,
                            os_in_l: &mut *buffers.os_in_l,
                            os_in_r: &mut *buffers.os_in_r,
                            os_model_l: &mut *buffers.os_model_l,
                            os_model_r: &mut *buffers.os_model_r,
                            crossfade_scratch_l: buffers.crossfade_scratch_l,
                            crossfade_scratch_r: buffers.crossfade_scratch_r,
                        },
                        current_host_rate,
                    );

                    if *recording_meta_sent {
                        send_recording_audio(
                            recording_producer,
                            n_pw,
                            &buffers.resamp_out_l[..],
                            &buffers.resamp_out_r[..],
                            recording_block,
                        );
                    }

                    if should_measure {
                        let elapsed_nanos = rt_setup::rdtsc_nanos().wrapping_sub(start_nanos);
                        rt_status_for_process
                            .dsp_cycle_time
                            .store(elapsed_nanos, Ordering::Relaxed);
                        rt_status_for_process.latency_hist.record(elapsed_nanos);

                        let elapsed_secs = elapsed_nanos as f64 / 1_000_000_000.0;
                        let budget_secs = (n_samples as f64 / current_host_rate as f64) * 0.85;
                        if elapsed_secs > budget_secs {
                            rt_status_for_process
                                .dsp_overloads
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }

                    let n = n_samples as u32;
                    if rt_status_for_process.last_n_samples.load(Ordering::Relaxed) != n {
                        rt_status_for_process
                            .last_n_samples
                            .store(n, Ordering::Relaxed);
                        rt_status_for_process
                            .requested_buffer_frames
                            .store(n, Ordering::Relaxed);
                        rt_status_for_process.set_flag(
                            neural_amp_modeler_rs::common::spsc::RT_STATUS_NEEDS_QUANTUM_LOG,
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::buffer::create_audio_ring_buffer;

    #[test]
    fn ffi_contract_valid_buffer() {
        let buf = [0u8; 128];
        assert!(check_ffi_contract(&buf, 0, 64).is_some());
        assert!(check_ffi_contract(&buf, 4, 60).is_some());
    }

    #[test]
    fn ffi_contract_exact_fit() {
        let buf = [0u8; 128];
        let (n_bytes, n_samples) = check_ffi_contract(&buf, 0, 128).unwrap();
        assert_eq!(n_bytes, 128);
        assert_eq!(n_samples, 32);
    }

    #[test]
    fn ffi_contract_misaligned_byte_count_rejected() {
        let buf = [0u8; 128];
        // 3 bytes is not a multiple of sizeof(f32)
        assert!(check_ffi_contract(&buf, 0, 3).is_none());
        // 5 bytes also not aligned
        assert!(check_ffi_contract(&buf, 0, 5).is_none());
    }

    #[test]
    fn ffi_contract_unaligned_offset_rejected() {
        let buf = [0u8; 128];
        // 1 byte unaligned offset + 4 bytes -> offset misaligned, rejected
        assert!(check_ffi_contract(&buf, 1, 4).is_none());
        // 2 byte unaligned offset -> rejected
        assert!(check_ffi_contract(&buf, 2, 8).is_none());
        // 4 byte aligned offset -> accepted
        assert!(check_ffi_contract(&buf, 4, 8).is_some());
    }

    #[test]
    fn ffi_contract_misaligned_base_pointer_rejected() {
        let storage = [0u8; 4 + 128];
        let base = storage.as_ptr() as usize;
        let align = std::mem::align_of::<f32>();
        let mut delta = 1usize;
        while (base + delta).is_multiple_of(align) {
            delta += 1;
        }
        let raw = &storage[delta..];
        assert!(check_ffi_contract(raw, 0, 64).is_none());
        assert!(check_ffi_contract(raw, 4, 60).is_none());
    }

    #[test]
    fn ffi_contract_offset_oob_rejected() {
        let buf = [0u8; 128];
        assert!(check_ffi_contract(&buf, 200, 64).is_none());
    }

    #[test]
    fn ffi_contract_size_clamped_rejected_if_offset_oob() {
        let buf = [0u8; 128];
        // offset is valid but size > remaining -> size is clamped
        let r = check_ffi_contract(&buf, 64, 128);
        assert!(r.is_some());
        let (n_bytes, n_samples) = r.unwrap();
        assert_eq!(n_bytes, 64);
        assert_eq!(n_samples, 16);

        // offset at end -> 0 bytes
        let r = check_ffi_contract(&buf, 128, 64);
        assert!(r.is_some());
        assert_eq!(r.unwrap(), (0, 0));
    }

    #[test]
    fn ffi_contract_misaligned_when_clamped() {
        let buf = [0u8; 5];
        // size=8 clamped to 5, but 5 is not f32-aligned
        assert!(check_ffi_contract(&buf, 0, 8).is_none());
    }

    #[test]
    fn recording_metadata_confirmed_only_on_push_success() {
        let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(2);
        let mut prod_opt = Some(prod);
        let mut meta_sent = false;
        let mut meta_rate = 0u32;

        send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate);

        assert!(meta_sent);
        assert_eq!(meta_rate, 48000);
        match cons.pop().unwrap() {
            RingPayload::Metadata(m) => assert_eq!(m.sample_rate, 48000.0),
            _ => panic!("expected Metadata"),
        }
    }

    #[test]
    fn recording_metadata_not_confirmed_when_channel_full() {
        let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(1);
        let mut prod_opt = Some(prod);
        let mut meta_sent = false;
        let mut meta_rate = 0u32;

        // Saturate the single-slot channel.
        prod_opt
            .as_mut()
            .unwrap()
            .push(RingPayload::Audio(AlignedBlock::new()))
            .unwrap();

        send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate);

        assert!(!meta_sent, "metadata must not be confirmed when push fails");
        assert_eq!(meta_rate, 0);

        // Free the channel and retry: the flag is confirmed.
        let _ = cons.pop().unwrap();
        send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate);

        assert!(meta_sent);
        assert_eq!(meta_rate, 48000);
        match cons.pop().unwrap() {
            RingPayload::Metadata(m) => assert_eq!(m.sample_rate, 48000.0),
            _ => panic!("expected Metadata"),
        }
    }

    #[test]
    fn recording_metadata_reset_on_host_rate_change() {
        let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
        let mut prod_opt = Some(prod);
        let mut meta_sent = false;
        let mut meta_rate = 0u32;

        send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate);
        assert!(meta_sent);
        assert_eq!(meta_rate, 48000);
        let _ = cons.pop().unwrap();

        // Same rate again: no duplicate header.
        send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate);
        assert!(cons.pop().is_err());

        // Host rate change: a new header is emitted for the new rate.
        send_recording_metadata(&mut prod_opt, 44100, &mut meta_sent, &mut meta_rate);
        assert!(meta_sent);
        assert_eq!(meta_rate, 44100);
        match cons.pop().unwrap() {
            RingPayload::Metadata(m) => assert_eq!(m.sample_rate, 44100.0),
            _ => panic!("expected Metadata"),
        }
    }

    #[test]
    fn recording_metadata_absent_producer_never_confirmed() {
        let mut prod_opt: Option<Producer<RingPayload<MAX_BLOCK_SIZE>>> = None;
        let mut meta_sent = false;
        let mut meta_rate = 0u32;

        send_recording_metadata(&mut prod_opt, 48000, &mut meta_sent, &mut meta_rate);

        assert!(!meta_sent);
        assert_eq!(meta_rate, 0);
    }

    #[test]
    fn recording_audio_oversized_block_dropped_and_counted() {
        let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
        OVERRUN_COUNT.store(0, Ordering::Relaxed);

        let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
        let mut prod_opt = Some(prod);
        let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
        let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();

        // interleaved_len = MAX_BLOCK_SIZE * 2 > MAX_BLOCK_SIZE -> dropped.
        send_recording_audio(
            &mut prod_opt,
            MAX_BLOCK_SIZE,
            &resamp_l,
            &resamp_r,
            &mut block,
        );

        assert!(cons.pop().is_err(), "oversized block must not be pushed");
        assert_eq!(OVERRUN_COUNT.load(Ordering::Relaxed), 1);

        OVERRUN_COUNT.store(0, Ordering::Relaxed);
    }

    #[test]
    fn recording_audio_normal_block_pushed() {
        let (prod, mut cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(4);
        let mut prod_opt = Some(prod);
        let mut resamp_l = [0.0f32; MAX_BLOCK_SIZE];
        let mut resamp_r = [0.0f32; MAX_BLOCK_SIZE];
        for i in 0..4 {
            resamp_l[i] = i as f32;
            resamp_r[i] = -(i as f32);
        }
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();

        send_recording_audio(&mut prod_opt, 4, &resamp_l, &resamp_r, &mut block);

        match cons.pop().unwrap() {
            RingPayload::Audio(b) => {
                assert_eq!(b.valid_len, 8);
                assert_eq!(b.data[0], 0.0);
                assert_eq!(b.data[1], -0.0);
                assert_eq!(b.data[2], 1.0);
                assert_eq!(b.data[3], -1.0);
            }
            _ => panic!("expected Audio"),
        }
    }

    #[test]
    fn recording_audio_full_channel_counted_as_overrun() {
        let _guard = crate::recording::buffer::OVERRUN_COUNT_LOCK.lock().unwrap();
        OVERRUN_COUNT.store(0, Ordering::Relaxed);

        let (prod, _cons) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(1);
        let mut prod_opt = Some(prod);
        prod_opt
            .as_mut()
            .unwrap()
            .push(RingPayload::Audio(AlignedBlock::new()))
            .unwrap();

        let resamp_l = [0.0f32; MAX_BLOCK_SIZE];
        let resamp_r = [0.0f32; MAX_BLOCK_SIZE];
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();

        send_recording_audio(&mut prod_opt, 4, &resamp_l, &resamp_r, &mut block);

        assert_eq!(OVERRUN_COUNT.load(Ordering::Relaxed), 1);

        OVERRUN_COUNT.store(0, Ordering::Relaxed);
    }
}
