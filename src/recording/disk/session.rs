// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Recording session drain loop and segment lifecycle.

use anyhow::{Context, Result};
use rtrb::Consumer;
use std::time::Instant;

use crate::recording::buffer::{AudioMetadata, ControlPayload};
use crate::recording::pool::{InFlightBlock, POOL_CAPACITY, PoolConsumer};
use crate::recording::space::{estimate_message, low_space_warning};
use crate::recording::status::{SharedRecordingStatus, mark_active, publish_space_warning};
use crate::recording::summary::CaptureSummary;
use crate::recording::transport::RecordingReceiver;
use crate::recording::writer::{WavSink, WavWriter};

use super::write::{
    log_capture_summary, maybe_check_space, report_overruns, write_block_with_overflow_rollover,
    write_silence_frames,
};

/// Mutable per-session state threaded through both transport drain loops.
pub(super) struct DrainSession<W: WavWriter> {
    pub(super) wav_writer: Option<W>,
    pub(super) part_counter: u32,
    pub(super) summary: CaptureSummary,
    pub(super) last_space_check: Instant,
}

impl<W: WavWriter> DrainSession<W> {
    fn new() -> Self {
        Self {
            wav_writer: None,
            part_counter: 0,
            summary: CaptureSummary::default(),
            last_space_check: Instant::now(),
        }
    }
}

/// The ring-consumption loop shared by the production worker and tests.
pub(crate) async fn disk_writer_loop_inner<S: WavSink>(
    sink: &S,
    receiver: &mut RecordingReceiver,
    status: &SharedRecordingStatus,
) -> Result<()> {
    disk_writer_loop_pool(sink, &mut receiver.control, &mut receiver.pool, status).await
}

/// Applies metadata corresponding to a pool control barrier using RFC 1982 serial arithmetic.
async fn handle_control_barrier<S: WavSink>(
    sink: &S,
    session: &mut DrainSession<S::Writer>,
    status: &SharedRecordingStatus,
    control: &mut Consumer<ControlPayload>,
    barrier_seq: u16,
) -> Result<()> {
    while let Ok(head) = control.peek() {
        match head {
            ControlPayload::Metadata { seq, .. } => {
                let seq = *seq;
                let diff = barrier_seq.wrapping_sub(seq);
                if diff == 0 {
                    if let Ok(ControlPayload::Metadata { meta, .. }) = control.pop() {
                        open_new_part(sink, session, status, meta).await?;
                    }
                    return Ok(());
                } else if diff < 0x8000 {
                    log::debug!(
                        "Discarding superseded metadata seq {seq} (waiting for barrier {barrier_seq})"
                    );
                    let _ = control.pop();
                } else {
                    log::warn!(
                        "Barrier seq {barrier_seq} precedes head metadata seq {seq}; skipping barrier"
                    );
                    return Ok(());
                }
            }
            ControlPayload::StreamStop => {
                log::warn!("Encountered StreamStop while processing barrier seq {barrier_seq}");
                return Ok(());
            }
        }
    }
    log::warn!("Barrier seq {barrier_seq} had no matching metadata in control ring");
    Ok(())
}

/// RAII lease over an in-flight pool block.
///
/// Guarantees the slot index returns to the free ring on **every** exit path,
/// including a failed write/barrier handling (`?` early return) or a panic:
/// the previous inline `in_flight.release()` after the write skipped the
/// release whenever the write failed, leaking a pool slot. The lease is the
/// single release point.
struct InFlightLease<'a, const N: usize> {
    in_flight: Option<InFlightBlock<'a, N>>,
}

impl<'a, const N: usize> InFlightLease<'a, N> {
    fn new(in_flight: InFlightBlock<'a, N>) -> Self {
        Self {
            in_flight: Some(in_flight),
        }
    }

    fn get(&self) -> Option<&InFlightBlock<'a, N>> {
        self.in_flight.as_ref()
    }
}

impl<const N: usize> Drop for InFlightLease<'_, N> {
    fn drop(&mut self) {
        if let Some(in_flight) = self.in_flight.take() {
            in_flight.release();
        }
    }
}

async fn process_pool_item<S: WavSink>(
    sink: &S,
    session: &mut DrainSession<S::Writer>,
    status: &SharedRecordingStatus,
    control: &mut Consumer<ControlPayload>,
    in_flight: InFlightBlock<'_, POOL_CAPACITY>,
) -> Result<()> {
    let lease = InFlightLease::new(in_flight);
    let Some(item) = lease.get() else {
        return Ok(());
    };
    if item.is_barrier() {
        handle_control_barrier(sink, session, status, control, item.barrier_seq()).await?;
    } else if item.is_silence() {
        write_silence_frames(sink, session, status, item.silence_frames() as usize).await?;
    } else {
        write_block_with_overflow_rollover(sink, session, status, item.block()).await?;
    }
    Ok(())
}

async fn finalize_writer_and_report<W: WavWriter>(
    wav_writer: &mut Option<W>,
    summary: &CaptureSummary,
    leaked_slots: u64,
    log_msg: &str,
    err_ctx: &'static str,
) -> Result<()> {
    if let Some(mut writer) = wav_writer.take() {
        writer.finalize().await.context(err_ctx)?;
        log::info!("{log_msg}");
    }
    log_capture_summary(summary);
    report_overruns(leaked_slots);
    Ok(())
}

async fn disk_writer_loop_pool<S: WavSink>(
    sink: &S,
    control: &mut Consumer<ControlPayload>,
    pool: &mut PoolConsumer<POOL_CAPACITY>,
    status: &SharedRecordingStatus,
) -> Result<()> {
    let mut session = DrainSession::new();
    let space_interval = sink.space_check_interval();

    loop {
        maybe_check_space(
            sink,
            &session.wav_writer,
            status,
            &mut session.last_space_check,
            space_interval,
        );
        if let Some(in_flight) = pool.try_pop() {
            process_pool_item(sink, &mut session, status, control, in_flight).await?;
            continue;
        }

        if let Ok(head) = control.peek() {
            match head {
                ControlPayload::StreamStop => {
                    let _ = control.pop();
                    while let Some(in_flight) = pool.try_pop() {
                        process_pool_item(sink, &mut session, status, control, in_flight).await?;
                    }
                    while let Ok(ctrl) = control.pop() {
                        if let ControlPayload::Metadata { seq, .. } = ctrl {
                            log::debug!(
                                "Discarding unconfirmed orphan metadata seq {seq} on StreamStop"
                            );
                        }
                    }
                    finalize_writer_and_report(
                        &mut session.wav_writer,
                        &session.summary,
                        pool.leaked_slots(),
                        "⏹️  Audio source stopped. WAV file safely closed and ready for use.",
                        "Failed to finalize WAV on stream stop",
                    )
                    .await?;
                    break;
                }
                ControlPayload::Metadata { seq, .. } => {
                    let seq = *seq;
                    if pool.work_is_abandoned() && control.is_abandoned() {
                        let _ = control.pop();
                        log::debug!(
                            "Discarding unconfirmed orphan metadata seq {seq} on abandoned transport"
                        );
                        continue;
                    }
                }
            }
        }

        if control.is_abandoned()
            && control.is_empty()
            && pool.work_is_abandoned()
            && pool.work_is_empty()
        {
            finalize_writer_and_report(
                &mut session.wav_writer,
                &session.summary,
                pool.leaked_slots(),
                "⏹️  Recording producer disconnected; capture finalized.",
                "Failed to finalize WAV file after recording producer was dropped",
            )
            .await?;
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    Ok(())
}

/// Finalizes the previous capture segment (if any), opens a new sequential
/// `_partN` file for `meta` and points the observable status at it.
pub(super) async fn open_new_part<S: WavSink>(
    sink: &S,
    session: &mut DrainSession<S::Writer>,
    status: &SharedRecordingStatus,
    meta: AudioMetadata,
) -> Result<()> {
    if let Some(mut existing_writer) = session.wav_writer.take() {
        existing_writer
            .finalize()
            .await
            .context("Failed to finalize the previous WAV file")?;
        log::info!("⏹️  Safely closed the previous capture.");
    }

    session.part_counter += 1;
    let (writer, filename) = sink.create(session.part_counter, meta).await?;

    log::info!("🎬 Created file: {}", filename.display());
    log::info!("🎧 Started writing strict PipeWire source audio...");

    if session.summary.directory.as_os_str().is_empty()
        && let Some(parent) = filename.parent()
    {
        session.summary.directory = parent.to_path_buf();
    }
    session
        .summary
        .push_segment(crate::recording::summary::CaptureSegment {
            path: filename.clone(),
            data_bytes: 0,
            frames: 0,
            sample_rate: meta.sample_rate as u32,
        });

    mark_active(status, filename);
    publish_space_status(sink, &meta, status, true);

    session.wav_writer = Some(writer);
    Ok(())
}

/// Publishes the low-space warning (and, when `with_estimate`, the writable-time
/// estimate) for the current format on the shared status.
fn publish_space_status<S: WavSink>(
    sink: &S,
    meta: &AudioMetadata,
    status: &SharedRecordingStatus,
    with_estimate: bool,
) {
    if let Some(free) = sink.free_bytes(meta) {
        if with_estimate {
            log::info!("{}", estimate_message(free, meta));
        }
        publish_space_warning(status, low_space_warning(free, meta));
    } else {
        publish_space_warning(status, None);
    }
}
