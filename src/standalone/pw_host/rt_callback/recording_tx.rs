// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Real-time recording transport enqueueing (zero-alloc, lock-free).

use crate::recording::buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, OVERRUN_FRAMES_COUNT, RingPayload,
};
use crate::recording::transport::{RecordingSender, assert_inline_rollback_build};
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
/// push into the transport's control channel (pool transport) or the
/// inline ring (rollback); a failed push (or an absent channel) leaves it
/// false so the next callback retries. A host sample-rate change invalidates
/// the flag so a new header is emitted for the new rate.
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
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
) {
    if recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        if let Some(flag) = recording_data_available {
            flag.store(false, Ordering::Relaxed);
        }
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
        if let Some(flag) = recording_data_available {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Cold rollback helper for the deprecated inline ring recording transport.
///
/// Kept `#[inline(never)]` so the 64 KiB `AlignedBlock<MAX_BLOCK_SIZE>` move
/// does not inflate the production `Pool` capture callback's stack frame.
#[cold]
#[inline(never)]
fn send_recording_audio_inline(
    producer: &mut Option<rtrb::Producer<RingPayload<MAX_BLOCK_SIZE>>>,
    n_pw: usize,
    resamp_out_l: &[f32],
    resamp_out_r: &[f32],
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
    recording_data_available: Option<&AtomicBool>,
) {
    assert_inline_rollback_build();
    let Some(producer) = producer.as_mut() else {
        return;
    };
    let mut block = std::mem::replace(recording_block, AlignedBlock::new_uninit());
    block.fill_planar(&resamp_out_l[..n_pw], &resamp_out_r[..n_pw]);
    if producer.push(RingPayload::Audio(block)).is_err() {
        record_overrun(n_pw);
    } else if let Some(flag) = recording_data_available {
        flag.store(true, Ordering::Relaxed);
    }
}

/// Pushes one audio block to the recording transport.
///
/// `MAX_BLOCK_SIZE` (16384 samples = 8192 stereo frames) covers the largest
/// legal host quantum (`MAX_BRIDGE_BUF`), so every accepted quantum is
/// persisted integrally. Blocks whose interleaved length exceeds
/// `MAX_BLOCK_SIZE` are dropped and counted in `OVERRUN_COUNT` +
/// `OVERRUN_FRAMES_COUNT` (fail-closed telemetry) instead of silently
/// vanishing. On the promoted pool transport the block is written into
/// a preallocated slot via `try_acquire` → `fill_planar` → `publish`; a pool
/// exhaustion (`try_acquire() == None`) also increments both counters,
/// mirroring a full inline ring.
///
/// `recording_failed` suspends enqueueing as soon as the disk
/// worker reports a fatal error — no panics, no pointless pushes.
#[inline(always)]
pub fn send_recording_audio(
    recording_sender: &mut RecordingSender,
    n_pw: usize,
    resamp_out_l: &[f32],
    resamp_out_r: &[f32],
    recording_block: &mut AlignedBlock<MAX_BLOCK_SIZE>,
    recording_data_available: Option<&AtomicBool>,
    recording_failed: Option<&AtomicBool>,
) {
    if n_pw == 0 {
        return;
    }
    if recording_failed.is_some_and(|f| f.load(Ordering::Acquire)) {
        if let Some(flag) = recording_data_available {
            flag.store(false, Ordering::Relaxed);
        }
        return;
    }
    let interleaved_len = n_pw * 2;
    if interleaved_len > MAX_BLOCK_SIZE {
        record_overrun(n_pw);
        return;
    }
    match recording_sender {
        RecordingSender::Pool { pool, .. } => {
            // Zero-copy pool path: acquire a preallocated slot, fill it in
            // place and publish the descriptor — zero allocations, the
            // 64 KiB payload never moves.
            let Some(producer) = pool.as_mut() else {
                return;
            };
            let Some(mut slot) = producer.try_acquire() else {
                // Pool exhausted (all slots in flight) — the pool's overrun
                // condition, accounted exactly like a full inline ring.
                record_overrun(n_pw);
                return;
            };
            slot.block_mut()
                .fill_planar(&resamp_out_l[..n_pw], &resamp_out_r[..n_pw]);
            if slot.publish() {
                if let Some(flag) = recording_data_available {
                    flag.store(true, Ordering::Relaxed);
                }
            } else {
                record_overrun(n_pw);
            }
        }
        RecordingSender::Inline(producer) => {
            send_recording_audio_inline(
                producer,
                n_pw,
                resamp_out_l,
                resamp_out_r,
                recording_block,
                recording_data_available,
            );
        }
    }
}
