// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! WAV block/silence writing, rollover, and space checks.

use anyhow::{Context, Result};
use std::time::Instant;

use crate::recording::buffer::{AlignedBlock, MAX_BLOCK_SIZE, OVERRUN_COUNT, OVERRUN_FRAMES_COUNT};
use crate::recording::space::low_space_warning;
use crate::recording::status::{SharedRecordingStatus, publish_space_warning};
use crate::recording::summary::CaptureSummary;
use crate::recording::writer::{WavSink, WavWriter};
use std::sync::atomic::Ordering;

use super::session::{DrainSession, open_new_part};

/// Periodic low-space re-check.
///
/// Called at the **top of every drain-loop iteration**, independently of
/// whether the rings are empty: a continuously-saturated capture must still
/// observe a filling disk instead of only discovering it when the write fails.
pub(super) fn maybe_check_space<S: WavSink>(
    sink: &S,
    wav_writer: &Option<S::Writer>,
    status: &SharedRecordingStatus,
    last_check: &mut Instant,
    interval: std::time::Duration,
) {
    if last_check.elapsed() < interval {
        return;
    }
    *last_check = Instant::now();
    if let Some(writer) = wav_writer {
        let meta = writer.metadata();
        if let Some(free) = sink.free_bytes(&meta) {
            publish_space_warning(status, low_space_warning(free, &meta));
        }
    }
}

/// Logs the end-of-session capture summary once the worker finalizes.
pub(super) fn log_capture_summary(summary: &CaptureSummary) {
    let line = summary.render();
    if !line.is_empty() {
        log::info!("{line}");
    }
}

/// Closes the current segment and opens the next `_partN` when `sample_count`
/// more samples would overflow the 4 GiB RIFF payload.
async fn roll_over_if_needed<S: WavSink>(
    sink: &S,
    session: &mut DrainSession<S::Writer>,
    status: &SharedRecordingStatus,
    sample_count: usize,
) -> Result<()> {
    let overflow_detected = session
        .wav_writer
        .as_ref()
        .is_some_and(|w| w.would_overflow(sample_count));
    if overflow_detected {
        log::warn!(
            "WAV file reached 4 GiB RIFF limit. Closing current segment \
             and starting part {}.",
            session.part_counter + 1
        );
        if let Some(meta) = session.wav_writer.as_ref().map(|w| w.metadata()) {
            open_new_part(sink, session, status, meta).await?;
            log::info!("🎬 Continuing capture in the new segment.");
        }
    }
    Ok(())
}

/// Writes one audio block through the shared overflow-rollover logic used by
/// both transports.
pub(super) async fn write_block_with_overflow_rollover<S: WavSink>(
    sink: &S,
    session: &mut DrainSession<S::Writer>,
    status: &SharedRecordingStatus,
    block: &AlignedBlock<MAX_BLOCK_SIZE>,
) -> Result<()> {
    roll_over_if_needed(sink, session, status, block.valid_len()).await?;

    if let Some(writer) = &mut session.wav_writer {
        writer.write_block(block).await?;
        let samples = block.valid_len() as u64;
        session.summary.add_block(samples / 2, samples * 4);
        if writer.checkpoint_due() {
            writer
                .checkpoint()
                .await
                .context("Periodic WAV header checkpoint failed")?;
        }
    }
    Ok(())
}

/// Materializes an intentional-silence marker: writes exactly `frames` zero
/// stereo frames, applying the same 4 GiB rollover and checkpoint cadence as
/// audio blocks. Accounting includes the zeros, so the capture summary reports
/// the true continuous duration and file size.
pub(super) async fn write_silence_frames<S: WavSink>(
    sink: &S,
    session: &mut DrainSession<S::Writer>,
    status: &SharedRecordingStatus,
    frames: usize,
) -> Result<()> {
    let samples = frames.saturating_mul(2);
    roll_over_if_needed(sink, session, status, samples).await?;

    if let Some(writer) = &mut session.wav_writer {
        writer.write_silence(frames).await?;
        session.summary.add_block(frames as u64, samples as u64 * 4);
        if writer.checkpoint_due() {
            writer
                .checkpoint()
                .await
                .context("Periodic WAV header checkpoint failed")?;
        }
    }
    Ok(())
}

/// Logs a warning if the RT producer reported ring overruns (audio loss).
pub(super) fn report_overruns(leaked_slots: u64) {
    let blocks = OVERRUN_COUNT.load(Ordering::Relaxed);
    let frames = OVERRUN_FRAMES_COUNT.load(Ordering::Relaxed);
    if blocks > 0 || frames > 0 || leaked_slots > 0 {
        log::warn!(
            "⚠️  lost blocks: {} (frames: {}, leaked pool slots: {}) — possible audio drop.",
            blocks,
            frames,
            leaked_slots
        );
    }
}
