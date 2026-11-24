// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Disk I/O thread for WAV recording.
//! Consumes audio data from the lock-free SPSC ring buffer and writes WAV files
//! using the **`io_uring`** subsystem for 100% asynchronous, non-blocking disk I/O.
//! To maintain truly zero-blocking semantics, WAV headers are generated manually
//! (standard 44-byte RIFF/WAVE header for Float32) and samples are written directly
//! via the short-write-safe `crate::recording::io::write_all_at` (a single
//! `write_at` may persist only a prefix of the buffer; looping guarantees full
//! persistence or an explicit, observable failure). Capture files
//! are opened exclusively with `create_new(true)` (O_CREAT|O_EXCL, no `truncate`)
//! and timestamp/part collisions are resolved atomically by retrying incremental
//! `-1`, `-2`, ... suffixes — anti-TOCTOU. On stream format changes
//! or graceful shutdown, the file header is rewritten with the final byte count and
//! an `fsync` is issued to ensure file integrity before closing.
//!
//! ## Startup handshake & failure propagation
//!
//! The worker is spawned with a [`RecordingInit`] carrying a
//! `tokio::sync::oneshot` handshake, an observable [`RecordingStatus`] and an
//! RT-observable atomic failure flag. Before the main loop starts it (a) fails
//! fast if `io_uring` is unavailable ([`spawn_recording_worker`] probes before
//! entering the runtime), (b) validates that the output directory is a real,
//! writable directory via `validate_output_dir`, and only then (c) publishes
//! `Active` on the handshake. If any startup step fails the worker reports
//! `Failed { reason }` on the handshake and the main thread aborts **before**
//! connecting any PipeWire stream. A fatal runtime error (`EIO`, `ENOSPC`)
//! transitions the status to `Failed` and raises the atomic flag the RT
//! callback polls to suspend enqueueing without panics.
//!
//! ## Lifecycle decoupling & integral drain
//!
//! The drain loop deliberately ignores the process-global `SHUTDOWN` flag. A
//! SIGINT arriving while the ring is momentarily empty must never finalize
//! the capture, because the RT callback can still produce blocks for up to
//! one main-loop iteration. The worker terminates only when the ring producer
//! can no longer emit:
//!
//! 1. the [`RingPayload::StreamStop`] token is consumed — the main thread
//!    pushes it exclusively after `thread_loop.stop()` confirmed the RT loop
//!    stopped; or
//! 2. the ring `Producer` was dropped **and** the ring is fully drained
//!    (`Consumer::is_abandoned()` + `Consumer::is_empty()`).
//!
//! Both terminal paths drain every pending block (integral drain), rewrite
//! the WAV header with the final byte count and `fsync` before returning. An
//! I/O error during the drain propagates as `Failed` and the partial file is
//! preserved on disk for recovery.
//!
//! ## Durable capture: periodic header checkpoints
//!
//! Rewriting the header only on `finalize` means *any* ungraceful death
//! (SIGKILL, OOM-killer, power loss, unhandled signals, a stuck second
//! Ctrl+C) leaves a `.wav` whose `data` chunk declares `0` bytes even with
//! gigabytes already on disk — the whole take is unrecoverable by every
//! standard reader. The writer therefore rewrites the header with the
//! running byte count and `fsync`s it every [`CHECKPOINT_INTERVAL_SECS`] seconds
//! of audio, from the disk thread, between blocks. A crash can then lose at
//! most one checkpoint window (≤ `CHECKPOINT_INTERVAL_SECS` s) instead of the
//! entire recording. Checkpointing never advances the data offsets or the byte
//! accounting, and is disabled by setting [`CHECKPOINT_INTERVAL_SECS`] to `0`
//! (rollback).
//!
//! # Known Limitation: Maximum WAV file size
//! The RIFF/WAV format uses `u32` fields for chunk sizes, limiting the data payload
//! to `u32::MAX - (header_len - 8)` (~4 GiB — the 8-byte `RIFF`+size envelope is
//! accounted for). The limit is enforced with checked arithmetic in
//! `AsyncWavWriter::would_overflow`, which triggers a sequential `_partN` rollover
//! before the `u32` fields can wrap. In practice, this limit is not reachable for
//! NAM-Audio-Pipe's intended use cases (short captures via qpwgraph).

use anyhow::{Context, Result};
use rtrb::Consumer;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crate::recording::buffer::{
    AlignedBlock, AudioMetadata, ControlPayload, MAX_BLOCK_SIZE, OVERRUN_COUNT,
    OVERRUN_FRAMES_COUNT, RingPayload,
};
use crate::recording::pool::{InFlightBlock, POOL_CAPACITY, PoolConsumer};
use crate::recording::probe::{IoUringSupport, probe_io_uring};
use crate::recording::space::{
    SPACE_CHECK_INTERVAL, StatvfsFreeSpace, estimate_message, low_space_warning,
};
use crate::recording::status::{
    RecordingInit, RecordingStatus, SharedRecordingStatus, mark_active, publish_space_warning,
    record_failure,
};
use crate::recording::summary::CaptureSummary;
use crate::recording::transport::RecordingReceiver;
use crate::recording::writer::{TokioUringSink, WavSink, WavWriter};

pub use crate::recording::writer::CHECKPOINT_INTERVAL_SECS;

#[cfg(test)]
pub(super) use crate::recording::writer::{AsyncWavWriter, create_new_capture};

/// Monotonic sequence suffix for the writability probe file.
static OUTPUT_PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

// Silence trimming is performed in the RT thread (process.rs): audio blocks are only
// enqueued when the noise gate is open (n_pw > 0). The disk writer receives only
// blocks containing real signal — never silence, never padding — UNLESS the gate is
// explicitly disabled via `--gate off` (see CaptureState::init in capture/state.rs).
// The gate is configurable via CLI; the default is active
// (`cli::GateConfig::default_on()` in CaptureState::init).

/// Main entry point for the Disk I/O thread.
/// Consumes the lock-free ring buffer and writes WAV files fully asynchronously via `io_uring`.
///
/// Before consuming anything it completes the startup handshake: validates
/// that the output directory is a real writable directory
/// (`validate_output_dir`), publishes `Active` on the handshake, then runs
/// the drain loop. The drain loop ignores the process-global `SHUTDOWN` flag:
/// it terminates only when the [`RingPayload::StreamStop`] token is consumed
/// or the ring `Producer` is dropped with the ring fully drained — never
/// while the RT producer can still emit. Every terminal path drains all
/// remaining data, rewrites the WAV header with the final byte count and
/// `fsync`s before returning. Any fatal error transitions the observable
/// [`RecordingStatus`] to `Failed` and raises the RT-observable failure flag.
pub async fn disk_writer_loop(
    mut receiver: RecordingReceiver,
    recording_data_available: Option<Arc<AtomicBool>>,
    init: RecordingInit,
) -> Result<()> {
    let RecordingInit {
        status,
        handshake,
        failed_flag,
        base_dir,
        ..
    } = init;

    // Fail-fast startup: abort BEFORE the main thread starts PipeWire.
    if let Err(e) = validate_output_dir(&base_dir) {
        let reason = format!(
            "Recording output directory {} is not usable: {e:#}",
            base_dir.display()
        );
        return Err(fail_startup(&status, &failed_flag, handshake, reason));
    }

    let _ = handshake.send(Ok(base_dir.clone()));
    mark_active(&status, base_dir.clone());
    log::info!(
        "🎙️  Recording worker ready — capture directory: {}",
        base_dir.display()
    );

    let space = StatvfsFreeSpace;
    let sink = TokioUringSink {
        base_dir: &base_dir,
        space: &space,
    };
    let result =
        disk_writer_loop_inner(&sink, &mut receiver, recording_data_available, &status).await;

    publish_worker_result(&result, &status, &failed_flag);

    result
}

/// Publishes the terminal worker result on the observable status.
fn publish_worker_result(
    result: &anyhow::Result<()>,
    status: &SharedRecordingStatus,
    failed_flag: &AtomicBool,
) {
    match result {
        Ok(()) => {
            if let Ok(mut guard) = status.lock() {
                *guard = RecordingStatus::Stopped;
            }
        }
        Err(e) => {
            let reason = format!("{e:#}");
            record_failure(status, failed_flag, &reason);
        }
    }
}

/// Validates that `base_dir` is a real directory and that the worker can
/// actually create, write, `fsync` and delete a file in it.
pub(crate) fn validate_output_dir(base_dir: &Path) -> Result<()> {
    if !base_dir.is_dir() {
        anyhow::bail!(
            "output directory {} does not exist or is not a directory",
            base_dir.display()
        );
    }
    let probe = base_dir.join(format!(
        ".nam-io-probe-{}-{}.tmp",
        std::process::id(),
        OUTPUT_PROBE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .with_context(|| format!("output directory {} is not writable", base_dir.display()))?;
    let write_res = file.write_all(b"io");
    let sync_res = file.sync_all();
    drop(file);
    let _ = std::fs::remove_file(&probe);
    write_res.with_context(|| format!("write probe failed in {}", base_dir.display()))?;
    sync_res.with_context(|| format!("fsync probe failed in {}", base_dir.display()))?;
    Ok(())
}

/// Records a startup failure on the observable status and failure flag, sends
/// `Err` over the startup handshake (consuming the one-shot sender), and
/// returns the error to propagate.
fn fail_startup(
    status: &SharedRecordingStatus,
    failed_flag: &AtomicBool,
    handshake: tokio::sync::oneshot::Sender<anyhow::Result<PathBuf>>,
    reason: String,
) -> anyhow::Error {
    record_failure(status, failed_flag, &reason);
    let _ = handshake.send(Err(anyhow::anyhow!(reason.clone())));
    anyhow::anyhow!(reason)
}

/// Mutable per-session state threaded through both transport drain loops.
struct DrainSession<W: WavWriter> {
    wav_writer: Option<W>,
    part_counter: u32,
    summary: CaptureSummary,
    last_space_check: Instant,
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
async fn disk_writer_loop_inner<S: WavSink>(
    sink: &S,
    receiver: &mut RecordingReceiver,
    recording_data_available: Option<Arc<AtomicBool>>,
    status: &SharedRecordingStatus,
) -> Result<()> {
    match receiver {
        RecordingReceiver::Pool { control, pool } => {
            disk_writer_loop_pool(sink, control, pool, recording_data_available, status).await
        }
        RecordingReceiver::Inline(consumer) => {
            disk_writer_loop_inline(sink, consumer, recording_data_available, status).await
        }
    }
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

async fn process_pool_item<S: WavSink>(
    sink: &S,
    session: &mut DrainSession<S::Writer>,
    status: &SharedRecordingStatus,
    control: &mut Consumer<ControlPayload>,
    in_flight: InFlightBlock<'_, POOL_CAPACITY>,
) -> Result<()> {
    if in_flight.is_barrier() {
        handle_control_barrier(sink, session, status, control, in_flight.barrier_seq()).await?;
    } else {
        write_block_with_overflow_rollover(sink, session, status, in_flight.block()).await?;
    }
    in_flight.release();
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
    recording_data_available: Option<Arc<AtomicBool>>,
    status: &SharedRecordingStatus,
) -> Result<()> {
    let mut session = DrainSession::new();

    loop {
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

        idle_poll_and_sleep(
            sink,
            &session.wav_writer,
            status,
            &mut session.last_space_check,
            &recording_data_available,
        )
        .await;
    }

    Ok(())
}

/// Rollback drain loop: consumes the single inline SPSC ring carrying
/// Metadata, Audio and StreamStop in FIFO order.
async fn disk_writer_loop_inline<S: WavSink>(
    sink: &S,
    consumer: &mut Consumer<RingPayload<MAX_BLOCK_SIZE>>,
    recording_data_available: Option<Arc<AtomicBool>>,
    status: &SharedRecordingStatus,
) -> Result<()> {
    let mut session = DrainSession::new();

    loop {
        if let Ok(payload) = consumer.pop() {
            match payload {
                RingPayload::Metadata(meta) => {
                    open_new_part(sink, &mut session, status, meta).await?;
                }
                RingPayload::Audio(block) => {
                    write_block_with_overflow_rollover(sink, &mut session, status, &block).await?;
                }
                RingPayload::StreamStop => {
                    finalize_writer_and_report(
                        &mut session.wav_writer,
                        &session.summary,
                        0,
                        "⏹️  Audio source stopped. WAV file safely closed and ready for use.",
                        "Failed to finalize WAV on stream stop",
                    )
                    .await?;
                    break;
                }
            }
        } else if consumer.is_abandoned() && consumer.is_empty() {
            finalize_writer_and_report(
                &mut session.wav_writer,
                &session.summary,
                0,
                "⏹️  Recording producer disconnected; capture finalized.",
                "Failed to finalize WAV file after recording producer was dropped",
            )
            .await?;
            break;
        } else {
            idle_poll_and_sleep(
                sink,
                &session.wav_writer,
                status,
                &mut session.last_space_check,
                &recording_data_available,
            )
            .await;
        }
    }

    Ok(())
}

async fn idle_poll_and_sleep<S: WavSink>(
    sink: &S,
    wav_writer: &Option<S::Writer>,
    status: &SharedRecordingStatus,
    last_space_check: &mut Instant,
    recording_data_available: &Option<Arc<AtomicBool>>,
) {
    maybe_check_space(sink, wav_writer, status, last_space_check);
    if let Some(flag) = recording_data_available {
        flag.store(false, Ordering::Relaxed);
    }
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
}

/// Finalizes the previous capture segment (if any), opens a new sequential
/// `_partN` file for `meta` and points the observable status at it.
async fn open_new_part<S: WavSink>(
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

/// Periodic low-space re-check on the disk loop's idle path.
fn maybe_check_space<S: WavSink>(
    sink: &S,
    wav_writer: &Option<S::Writer>,
    status: &SharedRecordingStatus,
    last_check: &mut Instant,
) {
    if last_check.elapsed() < SPACE_CHECK_INTERVAL {
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
fn log_capture_summary(summary: &CaptureSummary) {
    let line = summary.render();
    if !line.is_empty() {
        log::info!("{line}");
    }
}

/// Writes one audio block through the shared overflow-rollover logic used by
/// both transports.
async fn write_block_with_overflow_rollover<S: WavSink>(
    sink: &S,
    session: &mut DrainSession<S::Writer>,
    status: &SharedRecordingStatus,
    block: &AlignedBlock<MAX_BLOCK_SIZE>,
) -> Result<()> {
    let overflow_detected = session
        .wav_writer
        .as_ref()
        .is_some_and(|w| w.would_overflow(block.valid_len()));
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

/// Logs a warning if the RT producer reported ring overruns (audio loss).
fn report_overruns(leaked_slots: u64) {
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

/// Spawns the `nam-recording-io` worker thread.
pub fn spawn_recording_worker(
    receiver: RecordingReceiver,
    recording_data_available: Option<Arc<AtomicBool>>,
    init: RecordingInit,
    housekeeping_cpus: Vec<usize>,
) -> std::io::Result<std::thread::JoinHandle<anyhow::Result<()>>> {
    std::thread::Builder::new()
        .name("nam-recording-io".into())
        .spawn(move || {
            let _ = crate::standalone::rt_setup::apply_housekeeping_affinity(&housekeeping_cpus);
            let probe = init.io_uring_probe.unwrap_or(probe_io_uring);
            let verdict = probe();
            if verdict != IoUringSupport::Available {
                let reason = format!(
                    "io_uring is unavailable on this kernel/security policy \
                     (verdict: {verdict:?}); recording cannot start"
                );
                record_failure(&init.status, &init.failed_flag, &reason);
                let _ = init.handshake.send(Err(anyhow::anyhow!(reason.clone())));
                log::error!("🛑 {reason}");
                Err(anyhow::anyhow!(reason))
            } else {
                tokio_uring::start(async move {
                    let result = disk_writer_loop(receiver, recording_data_available, init).await;
                    if let Err(e) = &result {
                        log::error!("🛑 Disk writer error: {e:#}");
                    }
                    result
                })
            }
        })
}

#[cfg(test)]
#[path = "disk_test.rs"]
mod disk_test;
