// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Asynchronous WAV writer abstraction and `io_uring`-backed sink.
//!
//! Extracted from [`crate::recording::disk`] so file-level header management,
//! RIFF 4 GiB overflow bounds, and periodic durability checkpoints stay
//! cleanly separated from ring/pool orchestration.

use anyhow::{Context, Result};
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::recording::buffer::{AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE};
use crate::recording::io::{WriteAt, write_all_at};
use crate::recording::space::FreeSpace;
use crate::recording::wav_header::{build_wav_header, capture_filename, current_capture_timestamp};

/// Interval, in seconds of captured audio, between periodic WAV-header
/// checkpoints.
///
/// Every window the header is rewritten with the current `data_bytes_written`
/// and `fsync`ed, bounding the audio a crash can lose. The value is
/// a policy choice in the 2–5 s range. `0` disables checkpointing (the header
/// is then only corrected on `finalize`) — the documented rollback.
///
/// The trigger is derived per-format as
/// `CHECKPOINT_INTERVAL_SECS × sample_rate × channels × (bit_depth / 8)`
/// bytes: at 48 kHz / 2 ch / 32-bit float one window is exactly 1 152 000 bytes
/// (~1.1 MiB) of audio. The wall-clock cost of the periodic `fdatasync` on
/// SSD/HDD/USB is an operator-executed measurement.
pub const CHECKPOINT_INTERVAL_SECS: u64 = 3;

/// Audio-data bytes accumulated per checkpoint window for `metadata`.
///
/// Returns `0` when [`CHECKPOINT_INTERVAL_SECS`] is `0` (checkpointing
/// disabled). Recomputed per writer, so a mid-stream rate/format change — which
/// always opens a fresh `_partN` writer — picks up the new cadence.
fn checkpoint_interval_bytes(metadata: &AudioMetadata) -> u64 {
    if CHECKPOINT_INTERVAL_SECS == 0 {
        return 0;
    }
    let bytes_per_sec =
        metadata.sample_rate as f64 * metadata.channels as f64 * (metadata.bit_depth as f64 / 8.0);
    (CHECKPOINT_INTERVAL_SECS as f64 * bytes_per_sec) as u64
}

/// Asynchronous WAV writer using `tokio_uring` for purely zero-blocking disk I/O.
///
/// Generic over the positioned-write backend ([`WriteAt`]) so the real
/// `io_uring` file and the test-only fault-injecting mock are interchangeable.
///
/// Maintains a reusable I/O buffer (`io_buf`) to avoid heap allocation on every audio block
/// received from the ring buffer, significantly reducing allocator pressure.
pub(crate) struct AsyncWavWriter<W: WriteAt> {
    /// Positioned-write backend (io_uring file or injected mock).
    pub(crate) file: W,
    /// Current audio stream metadata (sample rate, bit depth, channels).
    metadata: AudioMetadata,
    /// Length of the WAV header in bytes (constant for a given format).
    /// Used to compute the exact RIFF 32-bit data-payload ceiling in
    /// `AsyncWavWriter::would_overflow`.
    pub(crate) header_len: u64,
    /// Total audio data bytes written (excludes the WAV header).
    /// Bounded by `u32::MAX - (header_len - 8)` (the RIFF/WAV 32-bit limit).
    pub(crate) data_bytes_written: u32,
    /// Current write offset in the file (header + data already written).
    pub(crate) current_offset: u64,
    /// Audio-data bytes accumulated since the last periodic header checkpoint.
    /// When it reaches `checkpoint_interval_bytes` the header is rewritten and
    /// `fsync`ed, bounding the audio a crash can lose.
    bytes_since_checkpoint: u64,
    /// Checkpoint trigger in audio-data bytes (`CHECKPOINT_INTERVAL_SECS` of
    /// audio at the current format). `0` disables checkpointing.
    pub(crate) checkpoint_interval_bytes: u64,
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
    /// the old racy `exists()` pre-check. Sequential parts (`part > 1`) go
    /// through the same resolution. Returns the writer and the path actually
    /// created.
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
pub(crate) async fn create_new_capture<F, Fut, T>(
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
    pub(crate) async fn open(mut file: W, metadata: AudioMetadata) -> Result<Self> {
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
            bytes_since_checkpoint: 0,
            checkpoint_interval_bytes: checkpoint_interval_bytes(&metadata),
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
    pub(crate) async fn write_block(&mut self, block: &AlignedBlock<MAX_BLOCK_SIZE>) -> Result<()> {
        let valid_samples = block.valid_len();
        if valid_samples == 0 {
            return Ok(());
        }

        // The block is planar (L samples then R samples); the RIFF/WAV spec requires
        // interleaved (L, R, L, R, ...) frame order, so interleave here off-RT.
        let bytes_len = valid_samples * 4;
        self.io_buf.clear();
        self.io_buf.reserve(bytes_len);

        let frames = valid_samples / 2;
        let (left, right) = block.as_slice().split_at(frames);
        for (l, r) in left.iter().zip(right) {
            self.io_buf.extend_from_slice(&l.to_le_bytes());
            self.io_buf.extend_from_slice(&r.to_le_bytes());
        }

        let buf = std::mem::take(&mut self.io_buf);
        let (res, returned_buf) = write_all_at(&mut self.file, buf, self.current_offset).await;
        self.io_buf = returned_buf;

        res.context("Failed to write audio block via io_uring")?;

        self.data_bytes_written += bytes_len as u32;
        self.current_offset += bytes_len as u64;
        self.bytes_since_checkpoint += bytes_len as u64;

        Ok(())
    }

    /// Persists `frames` stereo frames of digital silence (zero samples).
    ///
    /// Used for [intentional-silence markers](crate::recording::pool::SILENCE_MARKER_SLOT):
    /// the RT thread never copies zeros, and the writer materializes them here,
    /// off-RT, from the reusable I/O buffer. Chunked to the buffer capacity, so
    /// the zero fill never exceeds `MAX_BLOCK_SIZE` samples per disk write and
    /// never grows the allocation.
    pub(crate) async fn write_silence(&mut self, frames: usize) -> Result<()> {
        const MAX_FRAMES_PER_WRITE: usize = MAX_BLOCK_SIZE / 2;
        let mut remaining = frames;
        while remaining > 0 {
            let chunk = remaining.min(MAX_FRAMES_PER_WRITE);
            let bytes_len = chunk * 2 * 4;
            self.io_buf.clear();
            self.io_buf.resize(bytes_len, 0);

            let buf = std::mem::take(&mut self.io_buf);
            let (res, returned_buf) = write_all_at(&mut self.file, buf, self.current_offset).await;
            self.io_buf = returned_buf;

            res.context("Failed to write silence frames via io_uring")?;

            self.data_bytes_written += bytes_len as u32;
            self.current_offset += bytes_len as u64;
            self.bytes_since_checkpoint += bytes_len as u64;
            remaining -= chunk;
        }
        Ok(())
    }

    /// Returns true if writing `sample_count` samples would overflow the `u32`
    /// RIFF size field mandated by the RIFF/WAV specification (~4 GiB).
    ///
    /// The RIFF size field is `(header_len - 8) + data_payload`, so the largest
    /// data payload that still fits is `u32::MAX - (header_len - 8)` — the 8-byte
    /// `RIFF`+size envelope is accounted for. Checked arithmetic
    /// guarantees the `u32` fields can never wrap.
    pub(crate) fn would_overflow(&self, sample_count: usize) -> bool {
        let bytes_to_add = (sample_count as u64) * 4;
        let max_data_payload = u32::MAX as u64 - (self.header_len - 8);
        (self.data_bytes_written as u64)
            .checked_add(bytes_to_add)
            .is_none_or(|total| total > max_data_payload)
    }

    /// Returns `true` when a periodic header checkpoint is due.
    pub(crate) fn checkpoint_due(&self) -> bool {
        self.checkpoint_interval_bytes > 0
            && self.bytes_since_checkpoint >= self.checkpoint_interval_bytes
    }

    /// Rewrites the header at offset 0 with the current byte count and `fsync`s
    /// the file (short-write safe). Shared by the periodic checkpoint and the
    /// terminal finalize so their durability behavior can never drift apart.
    ///
    /// Never advances `current_offset` or `data_bytes_written`: the header
    /// reflects persisted data, it does not move the stream.
    async fn rewrite_header_and_sync(&mut self, context: &str) -> Result<()> {
        let header = build_wav_header(&self.metadata, self.data_bytes_written)?;
        let (res, _buf) = write_all_at(&mut self.file, header, 0).await;
        res.with_context(|| context.to_string())?;
        self.file
            .sync_all()
            .await
            .with_context(|| context.to_string())?;
        Ok(())
    }

    /// Rewrites the header with the current `data_bytes_written` and `fsync`s
    /// the file, without advancing the data offsets or the byte accounting.
    ///
    /// Called strictly between blocks (never inside [`Self::write_block`]), so
    /// it can never interleave with an in-flight audio write. A failure is a
    /// fatal recording error, handled exactly like a `write_block` failure.
    pub(crate) async fn checkpoint(&mut self) -> Result<()> {
        self.rewrite_header_and_sync("Failed to write the WAV header during checkpoint")
            .await?;
        self.bytes_since_checkpoint = 0;
        Ok(())
    }

    /// Finalizes the WAV file by rewriting the header with the final data size,
    /// and performing an explicit `fsync` to ensure data persistence on disk.
    ///
    /// `tokio_uring::fs::File` is automatically closed when dropped.
    pub(crate) async fn finalize(&mut self) -> Result<()> {
        self.rewrite_header_and_sync("Failed to write the WAV file during finalization")
            .await
    }
}

/// Factory that creates the per-part WAV writer consumed by the drain loop.
///
/// Injectable: the production sink opens real `io_uring` files under the output
/// directory, while tests drive the full loop against a mock writer so the
/// shutdown decoupling and the integral drain are proven deterministically
/// without a real kernel.
pub(crate) trait WavSink {
    /// Concrete writer type produced by [`WavSink::create`].
    type Writer: WavWriter;

    /// Creates a new capture writer for segment `part` with `metadata`,
    /// returning the writer and the actual file path.
    async fn create(&self, part: u32, metadata: AudioMetadata) -> Result<(Self::Writer, PathBuf)>;

    /// Returns the bytes available to the capture writer for `metadata`.
    ///
    /// `None` means "unknown": either the sink has no probe (test sinks) or the
    /// probe itself failed (e.g. a transient `statvfs` error). Callers must
    /// distinguish this from a *successful* probe reporting comfortable headroom
    /// so a transient failure never clears an active low-space warning.
    ///
    /// The production [`TokioUringSink`] overrides this with `statvfs`; the
    /// default keeps non-production sinks space-agnostic.
    fn free_bytes(&self, _metadata: &AudioMetadata) -> Option<u64> {
        None
    }

    /// Cadence of the periodic free-space re-check.
    ///
    /// Production uses [`crate::recording::space::SPACE_CHECK_INTERVAL`]; tests
    /// override it with a zero interval to prove the drain loop probes space
    /// independently of ring occupancy, without waiting a real minute.
    fn space_check_interval(&self) -> std::time::Duration {
        crate::recording::space::SPACE_CHECK_INTERVAL
    }
}

/// Minimal per-file surface the drain loop needs from a live WAV writer.
pub(crate) trait WavWriter {
    /// Stream metadata of the currently open capture.
    fn metadata(&self) -> AudioMetadata;

    /// Returns `true` if writing `sample_count` samples would overflow the
    /// `u32` RIFF size field (triggers a sequential `_partN` rollover).
    fn would_overflow(&self, sample_count: usize) -> bool;

    /// Persists one audio block fully (short-write safe).
    async fn write_block(&mut self, block: &AlignedBlock<MAX_BLOCK_SIZE>) -> Result<()>;

    /// Persists `frames` stereo frames of digital silence.
    async fn write_silence(&mut self, frames: usize) -> Result<()>;

    /// Returns `true` when a periodic header checkpoint is due.
    fn checkpoint_due(&self) -> bool;

    /// Rewrites the header with the running byte count and `fsync`s it, without
    /// advancing the write offsets or the byte accounting.
    async fn checkpoint(&mut self) -> Result<()>;

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

    async fn write_silence(&mut self, frames: usize) -> Result<()> {
        AsyncWavWriter::<W>::write_silence(self, frames).await
    }

    fn checkpoint_due(&self) -> bool {
        AsyncWavWriter::<W>::checkpoint_due(self)
    }

    async fn checkpoint(&mut self) -> Result<()> {
        AsyncWavWriter::<W>::checkpoint(self).await
    }

    async fn finalize(&mut self) -> Result<()> {
        AsyncWavWriter::<W>::finalize(self).await
    }
}

/// Production [`WavSink`]: creates real `io_uring`-backed capture files under
/// the configured output directory.
pub(crate) struct TokioUringSink<'a> {
    /// Output directory for capture files.
    pub(crate) base_dir: &'a Path,
    /// Injectable free-space probe used for the low-space pre-warning.
    pub(crate) space: &'a dyn FreeSpace,
}

impl WavSink for TokioUringSink<'_> {
    type Writer = AsyncWavWriter<tokio_uring::fs::File>;

    async fn create(&self, part: u32, metadata: AudioMetadata) -> Result<(Self::Writer, PathBuf)> {
        AsyncWavWriter::create(self.base_dir, part, metadata).await
    }

    fn free_bytes(&self, _metadata: &AudioMetadata) -> Option<u64> {
        self.space.available_bytes(self.base_dir).ok()
    }
}
