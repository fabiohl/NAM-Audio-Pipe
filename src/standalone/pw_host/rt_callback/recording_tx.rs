// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Real-time recording transport enqueueing (zero-alloc, lock-free).

use crate::recording::buffer::{
    AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, OVERRUN_FRAMES_COUNT,
};
use crate::recording::transport::RecordingSender;
use std::sync::atomic::{AtomicBool, Ordering};

/// Records one dropped recording block (`frames` stereo frames) in the atomic
/// overrun counters without allocating or locking.
#[inline(always)]
pub(crate) fn record_overrun(frames: usize) {
    core::hint::cold_path();
    OVERRUN_COUNT.fetch_add(1, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.fetch_add(frames as u64, Ordering::Relaxed);
}

/// Attempts to send recording metadata for `current_host_rate`.
///
/// The sticky `recording_meta_sent` flag is confirmed only after a successful
/// push into the transport's control channel; a failed push (or an absent
/// channel) leaves it false so the next callback retries. A host sample-rate
/// change invalidates the flag so a new header is emitted for the new rate.
///
/// `recording_failed` is the RT-observable failure flag: once
/// the disk worker reports a fatal error it is set and enqueueing is suspended —
/// pushing into a channel whose consumer has exited would only inflate
/// `OVERRUN_COUNT`/`OVERRUN_FRAMES_COUNT` pointlessly.
#[inline(always)]
pub(crate) fn send_recording_metadata(
    recording_sender: &mut RecordingSender,
    current_host_rate: u32,
    recording_meta_sent: &mut bool,
    recording_meta_rate: &mut u32,
    recording_failed: Option<&AtomicBool>,
) {
    if recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        return;
    }
    if *recording_meta_rate != current_host_rate {
        *recording_meta_sent = false;
    }
    if *recording_meta_sent {
        return;
    }
    let meta = AudioMetadata {
        sample_rate: current_host_rate as f32,
        bit_depth: 32,
        channels: 2,
    };
    if recording_sender.try_push_metadata(meta) {
        *recording_meta_sent = true;
        *recording_meta_rate = current_host_rate;
    }
}

/// Registers `frames` stereo frames intentionally suppressed by the `Silence`
/// noise gate.
///
/// The transport only accumulates the count (no zero copy on the RT thread);
/// the I/O thread writes the zeros. Frames lost to a real transport overrun are
/// deliberately **not** routed here — they go through [`record_overrun`] so
/// intentional silence and loss stay distinguishable.
#[inline(always)]
pub fn send_recording_silence(
    recording_sender: &mut RecordingSender,
    frames: usize,
    recording_failed: Option<&AtomicBool>,
) {
    if frames == 0 {
        return;
    }
    if recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        return;
    }
    recording_sender.try_push_silence(frames);
}

/// Pushes one audio block to the recording transport.
///
/// `MAX_BLOCK_SIZE` (16384 samples = 8192 stereo frames) covers the largest
/// legal host quantum (`MAX_BRIDGE_BUF`), so every accepted quantum is
/// persisted integrally. Blocks whose interleaved length exceeds
/// `MAX_BLOCK_SIZE` are dropped and counted in `OVERRUN_COUNT` +
/// `OVERRUN_FRAMES_COUNT` (fail-closed telemetry) instead of silently
/// vanishing. Pool exhaustion also increments both counters.
///
/// `recording_failed` suspends enqueueing as soon as the disk
/// worker reports a fatal error — no panics, no pointless pushes.
#[inline(always)]
pub fn send_recording_audio(
    recording_sender: &mut RecordingSender,
    n_pw: usize,
    resamp_out_l: &[f32],
    resamp_out_r: &[f32],
    recording_failed: Option<&AtomicBool>,
) {
    if n_pw == 0 {
        return;
    }
    if recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        return;
    }
    // Recording disabled: no channel to account against (a missing producer is
    // not an overrun — the caller dereferences the dummy sender unconditionally).
    if !recording_sender.has_producer() {
        return;
    }
    if n_pw * 2 > MAX_BLOCK_SIZE {
        record_overrun(n_pw);
        return;
    }
    if !recording_sender.try_push_audio(&resamp_out_l[..n_pw], &resamp_out_r[..n_pw]) {
        record_overrun(n_pw);
    }
}
