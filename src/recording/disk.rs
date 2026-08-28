// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Disk I/O thread for WAV recording.
//! Consumes audio data from the lock-free SPSC ring buffer and writes WAV files
//! using the **`io_uring`** subsystem for 100% asynchronous, non-blocking disk I/O.
//! To maintain truly zero-blocking semantics, WAV headers are generated manually
//! (standard 44-byte RIFF/WAVE header for Float32) and samples are written directly
//! via the short-write-safe [`crate::recording::io::write_all_at`] (a single
//! `write_at` may persist only a prefix of the buffer; looping guarantees full
//! persistence or an explicit, observable failure — F-RB-008/T3.1). Capture files
//! are opened exclusively with `create_new(true)` (O_CREAT|O_EXCL, no `truncate`)
//! and timestamp/part collisions are resolved atomically by retrying incremental
//! `-1`, `-2`, ... suffixes — anti-TOCTOU (F-RB-008/T3.2). On stream format changes
//! or graceful shutdown, the file header is rewritten with the final byte count and
//! an `fsync` is issued to ensure file integrity before closing.
//!
//! ## Startup handshake & failure propagation (F-RB-009 / T3.3)
//!
//! The worker is spawned with a [`RecordingInit`] carrying a
//! `tokio::sync::oneshot` handshake, an observable [`RecordingStatus`] and an
//! RT-observable atomic failure flag. Before the main loop starts it (a) fails
//! fast if `io_uring` is unavailable ([`spawn_recording_worker`] probes before
//! entering the runtime), (b) validates that the output directory is a real,
//! writable directory via [`validate_output_dir`], and only then (c) publishes
//! `Active` over the handshake. If any startup step fails the worker reports
//! `Failed { reason }` on the handshake and the main thread aborts **before**
//! connecting any PipeWire stream. A fatal runtime error (`EIO`, `ENOSPC`)
//! transitions the status to `Failed` and raises the atomic flag the RT
//! callback polls to suspend enqueueing without panics.
//!
//! ## Lifecycle decoupling & integral drain (F-RB-009 / T3.4)
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
//! # Known Limitation: Maximum WAV file size
//! The RIFF/WAV format uses `u32` fields for chunk sizes, limiting the data payload
//! to `u32::MAX - (header_len - 8)` (~4 GiB — the 8-byte `RIFF`+size envelope is
//! accounted for). The limit is enforced with checked arithmetic in
//! [`AsyncWavWriter::would_overflow`], which triggers a sequential `_partN` rollover
//! before the `u32` fields can wrap. In practice, this limit is not reachable for
//! NAM-Audio-Pipe's intended use cases (short captures via qpwgraph).

use anyhow::{Context, Result};
use rtrb::Consumer;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::recording::buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, RingPayload,
};
use crate::recording::io::{WriteAt, write_all_at};
use crate::recording::probe::{IoUringSupport, probe_io_uring};
use crate::recording::status::{
    RecordingInit, RecordingStatus, SharedRecordingStatus, record_failure,
};
use crate::recording::wav_header::{build_wav_header, capture_filename, current_capture_timestamp};

/// Monotonic sequence suffix for the writability probe file.
///
/// Multiple recording workers running **in the same process** (e.g. the T3.6
/// anti-TOCTOU harness with 20 concurrent instances in one output directory)
/// each call [`validate_output_dir`] at startup. A probe name derived only
/// from `std::process::id()` would collide between those workers — the second
/// `create_new(true)` would fail with `AlreadyExists` and a writable directory
/// would be wrongly rejected. The atomic counter keeps every probe name unique
/// per process while staying allocation-free on the probe path.
static OUTPUT_PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

// Silence trimming is performed in the RT thread (process.rs): audio blocks are only
// enqueued when the noise gate is open (n_pw > 0). The disk writer receives only
// blocks containing real signal — never silence, never padding.
// The gate is always active regardless of DSP configuration (architectural invariant).

/// Asynchronous WAV writer using `tokio_uring` for purely zero-blocking disk I/O.
///
/// Generic over the positioned-write backend ([`WriteAt`]) so the real
/// `io_uring` file and the test-only fault-injecting mock are interchangeable.
///
/// Maintains a reusable I/O buffer (`io_buf`) to avoid heap allocation on every audio block
/// received from the ring buffer, significantly reducing allocator pressure.
struct AsyncWavWriter<W: WriteAt> {
    /// Positioned-write backend (io_uring file or injected mock).
    file: W,
    /// Current audio stream metadata (sample rate, bit depth, channels).
    metadata: AudioMetadata,
    /// Length of the WAV header in bytes (constant for a given format).
    /// Used to compute the exact RIFF 32-bit data-payload ceiling in
    /// [`AsyncWavWriter::would_overflow`].
    header_len: u64,
    /// Total audio data bytes written (excludes the WAV header).
    /// Bounded by `u32::MAX - (header_len - 8)` (the RIFF/WAV 32-bit limit).
    data_bytes_written: u32,
    /// Current write offset in the file (header + data already written).
    current_offset: u64,
    /// Reusable I/O buffer for f32→bytes conversion before writing via io_uring.
    /// `tokio_uring` requires ownership of the buffer; after writing, the buffer is returned
    /// and reused on the next block, eliminating repeated allocations.
    io_buf: Vec<u8>,
}

impl AsyncWavWriter<tokio_uring::fs::File> {
    /// Atomically creates a new WAV capture file and writes the initial header.
    ///
    /// The file is opened exclusively with `create_new(true)` (O_CREAT|O_EXCL,
    /// **no** `truncate`), so a pre-existing file is never overwritten or
    /// truncated. Timestamp/part collisions surface as
    /// [`std::io::ErrorKind::AlreadyExists`] and are resolved by retrying with
    /// incremental `-1`, `-2`, ... suffixes — an anti-TOCTOU design that removes
    /// the old racy `exists()` pre-check (F-RB-008/T3.2). Sequential parts
    /// (`part > 1`) go through the same resolution. Returns the writer and the
    /// path actually created.
    async fn create(
        base_dir: &Path,
        part: u32,
        metadata: AudioMetadata,
    ) -> Result<(Self, PathBuf)> {
        let timestamp = current_capture_timestamp();
        let (path, file) = create_new_capture(base_dir, part, &timestamp, |candidate| {
            let mut opts = tokio_uring::fs::OpenOptions::new();
            let candidate = candidate.to_path_buf();
            async move { opts.write(true).create_new(true).open(candidate).await }
        })
        .await?;
        let writer = Self::open(file, metadata).await?;
        Ok((writer, path))
    }
}

/// Atomically creates a brand-new capture file, resolving timestamp/part
/// collisions by retrying an exclusive `create_new(true)` open with incremental
/// `-1`, `-2`, ... suffixes (anti-TOCTOU: no `exists()` pre-check).
///
/// `open` must be an exclusive (`create_new(true)`) open; the loop treats
/// [`std::io::ErrorKind::AlreadyExists`] as a collision and re-tries with the
/// next suffix. Returns the chosen path and the opened file. If every suffix up
/// to `u32::MAX` collides, an explicit error is returned instead of silently
/// overwriting an existing file.
async fn create_new_capture<F, Fut, T>(
    base_dir: &Path,
    part: u32,
    timestamp: &str,
    open: F,
) -> Result<(PathBuf, T)>
where
    F: Fn(&Path) -> Fut,
    Fut: Future<Output = std::io::Result<T>>,
{
    let mut suffix: u32 = 0;
    loop {
        let candidate = base_dir.join(capture_filename(timestamp, part, suffix));
        match open(&candidate).await {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix = suffix.checked_add(1).context(
                    "Exhausted 4 billion collision suffixes while atomically creating \
                     the WAV capture file",
                )?;
            }
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "Failed to atomically create WAV capture file {}",
                        candidate.display()
                    )
                });
            }
        }
    }
}

impl<W: WriteAt> AsyncWavWriter<W> {
    /// Writes the initial WAV header to an already-open backend.
    async fn open(mut file: W, metadata: AudioMetadata) -> Result<Self> {
        let header = build_wav_header(&metadata, 0)?;
        let header_len = header.len() as u64;
        let (res, _buf) = write_all_at(&mut file, header, 0).await;
        res.context("Failed to write initial WAV header")?;

        Ok(Self {
            file,
            metadata,
            header_len,
            data_bytes_written: 0,
            current_offset: header_len,
            // Pre-sized to MAX_BLOCK_SIZE * 4 bytes (f32 -> little-endian bytes).
            // Guarantees reserve() in write_block() is always a no-op, even across
            // PipeWire quantum renegotiations that may deliver larger blocks.
            io_buf: Vec::with_capacity(MAX_BLOCK_SIZE * 4),
        })
    }

    /// Writes a raw audio block in a fully asynchronous manner.
    /// The internal I/O buffer is reused across calls to avoid repeated allocations.
    /// Persistence is guaranteed by [`write_all_at`]: the offsets only advance
    /// after 100% of the interleaved PCM bytes are on disk.
    async fn write_block(&mut self, block: &AlignedBlock<MAX_BLOCK_SIZE>) -> Result<()> {
        let valid_samples = block.valid_len();
        if valid_samples == 0 {
            return Ok(());
        }

        // Prepare the reusable I/O buffer with the block bytes (Little Endian per WAV spec).
        // The block is planar (L samples then R samples); the RIFF/WAV spec requires
        // interleaved (L, R, L, R, ...) frame order, so interleave here on the disk
        // (off-RT) thread.
        let bytes_len = valid_samples * 4;
        self.io_buf.clear();
        self.io_buf.reserve(bytes_len);

        let frames = valid_samples / 2;
        let (left, right) = block.as_slice().split_at(frames);
        for (l, r) in left.iter().zip(right) {
            // Safe iterative byte conversion. `tokio_uring` requires buffer ownership;
            // `f32::to_le_bytes()` ensures safety and platform independence.
            self.io_buf.extend_from_slice(&l.to_le_bytes());
            self.io_buf.extend_from_slice(&r.to_le_bytes());
        }

        // write_all_at takes ownership of the buffer, loops over any short
        // writes until every byte is persisted, and returns the buffer for reuse.
        let buf = std::mem::take(&mut self.io_buf);
        let (res, returned_buf) = write_all_at(&mut self.file, buf, self.current_offset).await;
        self.io_buf = returned_buf;

        res.context("Failed to write audio block via io_uring")?;

        self.data_bytes_written += bytes_len as u32;
        self.current_offset += bytes_len as u64;

        Ok(())
    }

    /// Returns true if writing `sample_count` samples would overflow the `u32`
    /// RIFF size field mandated by the RIFF/WAV specification (~4 GiB).
    ///
    /// The RIFF size field is `(header_len - 8) + data_payload`, so the largest
    /// data payload that still fits is `u32::MAX - (header_len - 8)` — the 8-byte
    /// `RIFF`+size envelope is accounted for (F-RB-008/T3.2). Checked arithmetic
    /// guarantees the `u32` fields can never wrap.
    fn would_overflow(&self, sample_count: usize) -> bool {
        let bytes_to_add = (sample_count as u64) * 4;
        let max_data_payload = u32::MAX as u64 - (self.header_len - 8);
        (self.data_bytes_written as u64)
            .checked_add(bytes_to_add)
            .is_none_or(|total| total > max_data_payload)
    }

    /// Finalizes the WAV file by rewriting the header with the final data size,
    /// and performing an explicit `fsync` to ensure data persistence on disk.
    async fn finalize(&mut self) -> Result<()> {
        let header = build_wav_header(&self.metadata, self.data_bytes_written)?;
        // Rewrite the header at the origin (offset 0) — short-write safe.
        let (res, _buf) = write_all_at(&mut self.file, header, 0).await;
        res.context("Failed to rewrite WAV header during finalization")?;

        // Ensure synchronization with the hardware state
        self.file
            .sync_all()
            .await
            .context("Failed to fsync the WAV file")?;

        // `tokio_uring::fs::File` is automatically closed when dropped
        Ok(())
    }
}

/// Factory that creates the per-part WAV writer consumed by the drain loop.
///
/// Injectable (F-RB-009 / T3.4): the production sink opens real `io_uring`
/// files under the output directory, while tests drive the full loop against
/// a mock writer so the shutdown decoupling and the integral drain are proven
/// deterministically without a real kernel.
trait WavSink {
    /// Concrete writer type produced by [`WavSink::create`].
    type Writer: WavWriter;

    /// Creates a new capture writer for segment `part` with `metadata`,
    /// returning the writer and the actual file path.
    async fn create(&self, part: u32, metadata: AudioMetadata) -> Result<(Self::Writer, PathBuf)>;
}

/// Minimal per-file surface the drain loop needs from a live WAV writer.
trait WavWriter {
    /// Stream metadata of the currently open capture.
    fn metadata(&self) -> AudioMetadata;

    /// Returns `true` if writing `sample_count` samples would overflow the
    /// `u32` RIFF size field (triggers a sequential `_partN` rollover).
    fn would_overflow(&self, sample_count: usize) -> bool;

    /// Persists one audio block fully (short-write safe).
    async fn write_block(&mut self, block: &AlignedBlock<MAX_BLOCK_SIZE>) -> Result<()>;

    /// Rewrites the header with the final byte count and issues `fsync`.
    async fn finalize(&mut self) -> Result<()>;
}

impl<W: WriteAt> WavWriter for AsyncWavWriter<W> {
    fn metadata(&self) -> AudioMetadata {
        self.metadata
    }

    fn would_overflow(&self, sample_count: usize) -> bool {
        AsyncWavWriter::<W>::would_overflow(self, sample_count)
    }

    async fn write_block(&mut self, block: &AlignedBlock<MAX_BLOCK_SIZE>) -> Result<()> {
        AsyncWavWriter::<W>::write_block(self, block).await
    }

    async fn finalize(&mut self) -> Result<()> {
        AsyncWavWriter::<W>::finalize(self).await
    }
}

/// Production [`WavSink`]: creates real `io_uring`-backed capture files under
/// the configured output directory.
struct TokioUringSink<'a> {
    /// Output directory for capture files.
    base_dir: &'a Path,
}

impl WavSink for TokioUringSink<'_> {
    type Writer = AsyncWavWriter<tokio_uring::fs::File>;

    async fn create(&self, part: u32, metadata: AudioMetadata) -> Result<(Self::Writer, PathBuf)> {
        AsyncWavWriter::create(self.base_dir, part, metadata).await
    }
}

/// Main entry point for the Disk I/O thread.
/// Consumes the lock-free ring buffer and writes WAV files fully asynchronously via `io_uring`.
///
/// Before consuming anything it completes the startup handshake (F-RB-009 /
/// T3.3): validates that the output directory is a real writable directory
/// ([`validate_output_dir`]), publishes `Active` on the handshake, then runs
/// the drain loop. The drain loop ignores the process-global `SHUTDOWN` flag
/// (F-RB-009 / T3.4): it terminates only when the [`RingPayload::StreamStop`]
/// token is consumed or the ring `Producer` is dropped with the ring fully
/// drained — never while the RT producer can still emit. Every terminal path
/// drains all remaining data, rewrites the WAV header with the final byte
/// count and `fsync`s before returning. Any fatal error transitions the
/// observable [`RecordingStatus`] to `Failed` and raises the RT-observable
/// failure flag.
pub async fn disk_writer_loop(
    mut consumer: Consumer<RingPayload<MAX_BLOCK_SIZE>>,
    recording_data_available: Option<Arc<AtomicBool>>,
    init: RecordingInit,
) -> Result<()> {
    // Destructure so the one-shot handshake sender can be consumed on either the
    // fail-fast path (sent `Err`) or the success path (sent `Ok`).
    let RecordingInit {
        status,
        handshake,
        failed_flag,
        base_dir,
        ..
    } = init;

    // Fail-fast startup: if the output directory is missing or not writable the
    // worker reports `Failed` on the handshake and exits BEFORE the main thread
    // starts PipeWire — no silent recording loss (F-RB-009 / T3.3).
    if let Err(e) = validate_output_dir(&base_dir) {
        let reason = format!(
            "Recording output directory {} is not usable: {e:#}",
            base_dir.display()
        );
        return Err(fail_startup(&status, &failed_flag, handshake, reason));
    }

    // Startup handshake succeeded: the io_uring runtime is up (the probe ran in
    // `spawn_recording_worker` before entering the runtime) and the output
    // directory is confirmed writable. Notify the main thread that it is safe
    // to start PipeWire with `--record`.
    let _ = handshake.send(Ok(base_dir.clone()));
    if let Ok(mut guard) = status.lock() {
        *guard = RecordingStatus::Active {
            path: base_dir.clone(),
        };
    }
    log::info!(
        "🎙️  Recording worker ready — capture directory: {}",
        base_dir.display()
    );

    let sink = TokioUringSink {
        base_dir: &base_dir,
    };
    let result =
        disk_writer_loop_inner(&sink, &mut consumer, recording_data_available, &status).await;

    match &result {
        Ok(()) => {
            if let Ok(mut guard) = status.lock() {
                *guard = RecordingStatus::Stopped;
            }
        }
        Err(e) => {
            // Fatal runtime error (EIO, ENOSPC, ...): publish Failed and raise
            // the RT-observable flag so the audio callback suspends enqueueing
            // without panics. The caller (`spawn_recording_worker`) logs the
            // final visible error report.
            let reason = format!("{e:#}");
            record_failure(&status, &failed_flag, &reason);
        }
    }

    result
}

/// Validates that `base_dir` is a real directory and that the worker can
/// actually create, write, `fsync` and delete a file in it.
///
/// Fails fast on a missing path, a regular file masquerading as a directory,
/// `EACCES` (no write permission), `ENOSPC` (disk full) or `EROFS` (read-only
/// mount). This is the fail-fast gate that lets the main thread abort before
/// connecting any audio stream instead of recording into the void.
pub(crate) fn validate_output_dir(base_dir: &Path) -> Result<()> {
    if !base_dir.is_dir() {
        anyhow::bail!(
            "output directory {} does not exist or is not a directory",
            base_dir.display()
        );
    }
    // Prove real write+sync capability: create a probe file atomically, persist
    // a byte, fsync and remove it. The name carries a per-process sequence so
    // concurrent workers in the same process never collide on the probe file.
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

/// The ring-consumption loop shared by the production worker and tests.
///
/// Extracted so the startup handshake and failure propagation live in
/// [`disk_writer_loop`] while the long-running drain loop stays focused.
///
/// # Lifecycle (F-RB-009 / T3.4)
///
/// The loop **never** observes the process-global `SHUTDOWN` flag. It exits
/// only when the ring producer can no longer emit audio:
///
/// 1. the [`RingPayload::StreamStop`] token is consumed — the main thread
///    pushes it exclusively after `thread_loop.stop()` confirmed the RT loop
///    stopped; or
/// 2. the `Producer` was dropped **and** the ring is fully drained
///    ([`Consumer::is_abandoned`] + [`Consumer::is_empty`]) — no block can
///    ever arrive again.
///
/// Both terminal paths drain every pending block first (integral drain),
/// finalize the WAV header and `fsync` before returning, so a SIGINT arriving
/// while the ring is momentarily empty can never orphan the blocks produced
/// afterwards.
async fn disk_writer_loop_inner<S: WavSink>(
    sink: &S,
    consumer: &mut Consumer<RingPayload<MAX_BLOCK_SIZE>>,
    recording_data_available: Option<Arc<AtomicBool>>,
    status: &SharedRecordingStatus,
) -> Result<()> {
    let mut wav_writer: Option<S::Writer> = None;
    let mut part_counter: u32 = 0;

    loop {
        if let Ok(payload) = consumer.pop() {
            match payload {
                RingPayload::Metadata(meta) => {
                    // Finalize the previous WAV if the format changed mid-stream.
                    if let Some(mut existing_writer) = wav_writer.take() {
                        existing_writer
                            .finalize()
                            .await
                            .context("Failed to finalize the previous WAV file")?;
                        log::info!("⏹️  Safely closed the previous capture.");
                    }

                    part_counter += 1;
                    let (writer, filename) = sink.create(part_counter, meta).await?;

                    log::info!("🎬 Created file: {}", filename.display());
                    log::info!("🎧 Started writing strict PipeWire source audio...");

                    // Keep the observable status pointed at the live capture file.
                    if let Ok(mut guard) = status.lock() {
                        *guard = RecordingStatus::Active {
                            path: filename.clone(),
                        };
                    }

                    wav_writer = Some(writer);
                }
                RingPayload::Audio(block) => {
                    let overflow_detected = wav_writer
                        .as_ref()
                        .is_some_and(|w| w.would_overflow(block.valid_len()));
                    if overflow_detected {
                        log::warn!(
                            "WAV file reached 4 GiB RIFF limit. Closing current segment \
                             and starting part {}.",
                            part_counter + 1
                        );
                        if let Some(mut old_writer) = wav_writer.take() {
                            let meta = old_writer.metadata();
                            old_writer
                                .finalize()
                                .await
                                .context("Failed to finalize previous WAV segment on overflow")?;

                            part_counter += 1;
                            let (new_writer, filename) = sink.create(part_counter, meta).await?;
                            log::info!("🎬 Continuing capture in: {}", filename.display());

                            if let Ok(mut guard) = status.lock() {
                                *guard = RecordingStatus::Active {
                                    path: filename.clone(),
                                };
                            }

                            wav_writer = Some(new_writer);
                        }
                    }

                    if let Some(writer) = &mut wav_writer {
                        writer.write_block(&block).await?;
                    }
                }
                RingPayload::StreamStop => {
                    // Terminal condition (1): the token is consumed only after
                    // the RT loop stopped (`thread_loop.stop()`), so no further
                    // audio can be produced — finalize and exit.
                    if let Some(mut writer) = wav_writer.take() {
                        writer
                            .finalize()
                            .await
                            .context("Failed to finalize WAV on stream stop")?;
                        log::info!(
                            "⏹️  Audio source stopped. WAV file safely closed and ready for use.",
                        );
                    }
                    report_overruns();
                    break;
                }
            }
        } else if consumer.is_abandoned() && consumer.is_empty() {
            // Terminal condition (2): the ring producer was dropped AND the
            // ring is completely drained — no block can ever arrive again.
            // Drain is integral: everything pending was already written above,
            // so finalize with the last sample and exit.
            if let Some(mut writer) = wav_writer.take() {
                writer
                    .finalize()
                    .await
                    .context("Failed to finalize WAV file after recording producer was dropped")?;
                log::info!("⏹️  Recording producer disconnected; capture finalized.");
            }
            report_overruns();
            break;
        } else {
            // Check hint flag before sleeping to reduce unnecessary poll latency.
            // The flag is set Relaxed by the RT producer; we reset it here before sleeping
            // to avoid missing a notification that arrives during the sleep window.
            if let Some(ref flag) = recording_data_available {
                flag.store(false, Ordering::Relaxed);
            }
            // Brief sleep to avoid busy-spinning. `SHUTDOWN` is deliberately
            // ignored here: termination happens exclusively via the StreamStop
            // token or a dropped+drained producer (F-RB-009 / T3.4), so a
            // SIGINT during a momentary empty ring can never truncate the
            // recording tail.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    Ok(())
}

/// Logs a warning if the RT producer reported ring overruns (audio loss).
fn report_overruns() {
    let overruns = OVERRUN_COUNT.load(Ordering::Relaxed);
    if overruns > 0 {
        log::warn!(
            "⚠️  Detected {} ring buffer overruns — possible audio data loss.",
            overruns
        );
    }
}

/// Spawns the `nam-recording-io` worker thread (F-RB-009 / T3.3).
///
/// Probes `io_uring` **before** entering the `tokio_uring` runtime — entering
/// the runtime with the subsystem disabled would panic on the driver build; a
/// clean handshake error is required instead. The probe is injectable through
/// [`RecordingInit::io_uring_probe`] so the unavailable-kernel fail-fast path
/// is unit-testable without a real kernel change.
///
/// Returns the thread handle. The worker communicates its startup outcome
/// through `init.handshake` (consumed by
/// [`crate::recording::status::wait_for_recording_init`]) and, on any later
/// fatal error, publishes `Failed` on `init.status` and raises `init.failed_flag`.
///
/// The thread **returns** the [`Result`] produced by
/// [`disk_writer_loop`] (or the `io_uring`-unavailable error) so the join is
/// observable: [`crate::recording::guard::RecordingWorkerGuard`] inspects it
/// formally and recording failures propagate into the process exit code
/// (F-RB-009 / T3.5).
pub fn spawn_recording_worker(
    consumer: Consumer<RingPayload<MAX_BLOCK_SIZE>>,
    recording_data_available: Option<Arc<AtomicBool>>,
    init: RecordingInit,
) -> std::io::Result<std::thread::JoinHandle<anyhow::Result<()>>> {
    std::thread::Builder::new()
        .name("nam-recording-io".into())
        .spawn(move || {
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
                    let result = disk_writer_loop(consumer, recording_data_available, init).await;
                    if let Err(e) = &result {
                        // `disk_writer_loop` already records the failure on the
                        // observable status and the RT failure flag; log here
                        // as the final visible report before propagating the
                        // error to the join (T3.5).
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
