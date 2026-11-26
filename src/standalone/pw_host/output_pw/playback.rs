// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Playback DSP cycle (Bridge → Hardware).

use super::super::rt_callback::silence_available_datas;
use super::delivery::deliver_playback_pair_fail_closed_with_limit;
use super::silence::{
    SilenceReason, active_quantum_frames, classify_missing_bridge_block, deliver_silence_block,
};
use crate::standalone::pw_host::stream_status::StreamStatusFlags;
use crate::standalone::rt_setup;
use neural_amp_modeler_rs::common::spsc::RT_STATUS_HOST_CONTRACT_VIOLATION;
use neural_amp_modeler_rs::common::spsc::RtStatusFlags;
use neural_amp_modeler_rs::dsp::pipeline::DspBridgeReader;
use pipewire as pw;
use std::sync::atomic::Ordering;

/// Playback DSP Pipeline (Bridge → Hardware).
#[inline(always)]
pub fn playback_dsp_cycle(
    stream: &pw::stream::Stream,
    bridge: &DspBridgeReader,
    last_bridge_gen: &mut u64,
    rt_status: &RtStatusFlags,
    stream_status: &StreamStatusFlags,
    pb_callback_count: u32,
    output_limit: crate::standalone::cli_domain::OutputLimitPolicy,
) {
    let should_measure = (pb_callback_count & 0xF) == 0;
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
        deliver_silence_block(stream, rt_status, stream_status, SilenceReason::Muted);
        return;
    }

    // Reads the newest bridge block. The closure only captures raw source
    // descriptors (base pointers + byte lengths) so the bridge front-buffer
    // borrow ends before the dequeue below. Per the double-buffer + skip-on-
    // overflow protocol (`consumed_gen`), the writer can publish at most one
    // newer block before this callback consumes it, and that block lands in
    // the complementary buffer — the captured source regions therefore stay
    // stable for the whole cycle (see `DspBridgeReader::read_block`) provided
    // playback `process()` is non-reentrant (single reader on `pw_data_loop`)
    // and teardown stops streams/loops before `reset_to_silence`/`prime_silence`.
    let Some((_generation, capture_tsc, (src_l, n_bytes, src_r, _))) =
        bridge.read_block_stamped(last_bridge_gen, |buf_l, buf_r| {
            (
                buf_l.as_ptr() as usize,
                std::mem::size_of_val(buf_l),
                buf_r.as_ptr() as usize,
                std::mem::size_of_val(buf_r),
            )
        })
    else {
        // Bridge starvation or an early playback callback: the capture stream
        // produced no new block for this quantum. The delivered generation's
        // own capture timestamp (never the last global capture timestamp)
        // classifies the miss: if the next capture quantum could not have
        // started yet, the callback fired ahead of schedule (early); otherwise
        // the gap is a real starvation (paused input, pending resampler
        // rebuild, clock drift or capture quantum miss). Deterministic silence
        // policy: still dequeue, validate fail-closed, fill 100% of the output
        // extension with analytical silence and recycle the buffer — the
        // hardware never repeats stale audio left in the previous buffer.
        //
        // `pb_callback_count` is the monotonic playback-callback counter, not
        // a quantum length: classifying the miss from it would make the derived
        // period grow without bound (FINDING-RES-01). The active stream quantum
        // is resolved from the RT frame counters instead, exactly like the
        // silence window below.
        let reason = classify_missing_bridge_block(stream_status, active_quantum_frames(rt_status));
        deliver_silence_block(stream, rt_status, stream_status, reason);
        return;
    };
    // Remember the capture timestamp of the generation actually delivered so a
    // later miss can be classified without the last global capture timestamp.
    if capture_tsc != 0 {
        stream_status
            .playback_last_capture_tsc
            .store(capture_tsc, Ordering::Relaxed);
    }

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
        stream_status.playback_hist.record(pb_nanos);

        // Per-generation capture→playback latency: the timestamp published with
        // this exact generation, not the last global capture timestamp.
        let cap_start = capture_tsc;
        if cap_start > 0 && t_pb_end > cap_start {
            let e2e_nanos = t_pb_end.saturating_sub(cap_start);
            stream_status.e2e_hist.record(e2e_nanos);
            // Mirror the sample into the exact reservoir so the test harness can
            // report true percentiles. Testing/heap-audit only; absent from the
            // default/release build.
            #[cfg(any(feature = "testing", feature = "heap-audit"))]
            stream_status.e2e_exact.record(e2e_nanos);
        }
    }
}
