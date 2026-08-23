// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Disk I/O thread for WAV recording.
//! Consumes audio data from the lock-free SPSC ring buffer and writes WAV files
//! using the **`io_uring`** subsystem for 100% asynchronous, non-blocking disk I/O.
//! To maintain truly zero-blocking semantics, WAV headers are generated manually
//! (standard 44-byte RIFF/WAVE header for Float32) and samples are written directly
//! via `tokio_uring::fs::File::write_at`. On stream format changes or graceful shutdown,
//! the file header is rewritten with the final byte count and an `fsync` is issued
//! to ensure file integrity before closing.
//!
//! # Known Limitation: Maximum WAV file size
//! The RIFF/WAV format uses `u32` fields for chunk sizes, limiting the data payload
//! to ~4 GiB (~3h of stereo 32-bit float audio at 48kHz). In practice, this limit
//! is not reachable for NAM-Audio-Pipe's intended use cases (short captures via qpwgraph).

use anyhow::{Context, Result};
use rtrb::Consumer;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use neural_amp_modeler_rs::common::spsc::SHUTDOWN;

use crate::recording::buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, RingPayload,
};
use crate::recording::wav_header::{build_wav_header, resolve_available_filename};

// Silence trimming is performed in the RT thread (process.rs): audio blocks are only
// enqueued when the noise gate is open (n_pw > 0). The disk writer receives only
// blocks containing real signal — never silence, never padding.
// The gate is always active regardless of DSP configuration (architectural invariant).

/// Asynchronous WAV writer using `tokio_uring` for purely zero-blocking disk I/O.
///
/// Maintains a reusable I/O buffer (`io_buf`) to avoid heap allocation on every audio block
/// received from the ring buffer, significantly reducing allocator pressure.
struct AsyncWavWriter {
    /// File handle opened via io_uring.
    file: tokio_uring::fs::File,
    /// Current audio stream metadata (sample rate, bit depth, channels).
    metadata: AudioMetadata,
    /// Total audio data bytes written (excludes the WAV header).
    /// Limited to u32 by the RIFF/WAV specification (~4 GiB maximum).
    data_bytes_written: u32,
    /// Current write offset in the file (header + data already written).
    current_offset: u64,
    /// Reusable I/O buffer for f32→bytes conversion before writing via io_uring.
    /// `tokio_uring` requires ownership of the buffer; after writing, the buffer is returned
    /// and reused on the next block, eliminating repeated allocations.
    io_buf: Vec<u8>,
}

impl AsyncWavWriter {
    /// Creates a new WAV file and writes the initial header.
    async fn create(path: &PathBuf, metadata: AudioMetadata) -> Result<Self> {
        let file = tokio_uring::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await
            .context("Failed to open file via io_uring")?;

        let header = build_wav_header(&metadata, 0)?;
        let header_len = header.len() as u64;
        let (res, _): (std::io::Result<usize>, _) = file.write_at(header, 0).submit().await;
        res.context("Failed to write initial WAV header")?;

        Ok(Self {
            file,
            metadata,
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
    async fn write_block(&mut self, block: &AlignedBlock<MAX_BLOCK_SIZE>) -> Result<()> {
        let valid_samples = block.valid_len;
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

        // tokio_uring takes ownership of the buffer for the async write.
        // After completion, the buffer is returned and reassigned for reuse.
        let buf = std::mem::take(&mut self.io_buf);
        let (res, returned_buf): (std::io::Result<usize>, Vec<u8>) =
            self.file.write_at(buf, self.current_offset).submit().await;
        self.io_buf = returned_buf;

        let written = res.context("Failed to write audio block via io_uring")?;

        self.data_bytes_written += written as u32;
        self.current_offset += written as u64;

        Ok(())
    }

    /// Returns true if writing `sample_count` samples would overflow the
    /// `u32` data size field mandated by the RIFF/WAV specification (~4 GiB).
    fn would_overflow(&self, sample_count: usize) -> bool {
        let bytes_to_add = (sample_count * 4) as u32;
        self.data_bytes_written.checked_add(bytes_to_add).is_none()
    }

    /// Finalizes the WAV file by rewriting the header with the final data size,
    /// and performing an explicit `fsync` to ensure data persistence on disk.
    async fn finalize(self) -> Result<()> {
        let header = build_wav_header(&self.metadata, self.data_bytes_written)?;
        // Rewrite the header at the origin (offset 0)
        let (res, _): (std::io::Result<usize>, _) = self.file.write_at(header, 0).submit().await;
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

/// Main entry point for the Disk I/O thread.
/// Consumes the lock-free ring buffer and writes WAV files fully asynchronously via `io_uring`.
/// Supports graceful shutdown: when `SHUTDOWN` is activated, all remaining data is drained,
/// and the WAV file is properly finalized (via `fsync`) before returning.
pub async fn disk_writer_loop(
    mut consumer: Consumer<RingPayload<MAX_BLOCK_SIZE>>,
    recording_data_available: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<()> {
    let mut wav_writer: Option<AsyncWavWriter> = None;
    let mut part_counter: u32 = 0;
    let base_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    loop {
        if let Ok(payload) = consumer.pop() {
            match payload {
                RingPayload::Metadata(meta) => {
                    // Finalize the previous WAV if the format changed mid-stream.
                    if let Some(existing_writer) = wav_writer.take() {
                        existing_writer
                            .finalize()
                            .await
                            .context("Failed to finalize the previous WAV file")?;
                        log::info!("⏹️  Safely closed the previous capture.");
                    }

                    part_counter += 1;
                    let filename = resolve_available_filename(&base_dir, part_counter);

                    log::info!("🎬 Created file: {}", filename.display());
                    log::info!("🎧 Started writing strict PipeWire source audio...");

                    let writer = AsyncWavWriter::create(&filename, meta).await?;
                    wav_writer = Some(writer);
                }
                RingPayload::Audio(block) => {
                    let overflow_detected = wav_writer
                        .as_ref()
                        .is_some_and(|w| w.would_overflow(block.valid_len));
                    if overflow_detected {
                        log::warn!(
                            "WAV file reached 4 GiB RIFF limit. Closing current segment \
                             and starting part {}.",
                            part_counter + 1
                        );
                        if let Some(old_writer) = wav_writer.take() {
                            let meta = old_writer.metadata;
                            old_writer
                                .finalize()
                                .await
                                .context("Failed to finalize previous WAV segment on overflow")?;

                            part_counter += 1;
                            let filename = resolve_available_filename(&base_dir, part_counter);
                            log::info!("🎬 Continuing capture in: {}", filename.display());

                            let new_writer = AsyncWavWriter::create(&filename, meta).await?;
                            wav_writer = Some(new_writer);
                        }
                    }

                    if let Some(writer) = &mut wav_writer {
                        writer.write_block(&block).await?;
                    }
                }
                RingPayload::StreamStop => {
                    if let Some(writer) = wav_writer.take() {
                        writer
                            .finalize()
                            .await
                            .context("Failed to finalize WAV on stream stop")?;
                        log::info!(
                            "⏹️  Audio source stopped. WAV file safely closed and ready for use.",
                        );
                    }
                }
            }
        } else if SHUTDOWN.load(Ordering::Acquire) {
            // Pairs with Release store in main.rs (spsc::SHUTDOWN.store(true, Ordering::Release))
            // Drain remaining items that arrived between the last pop and shutdown detection.
            while let Ok(payload) = consumer.pop() {
                if let RingPayload::Audio(block) = payload
                    && let Some(writer) = &mut wav_writer
                {
                    if writer.would_overflow(block.valid_len) {
                        log::warn!(
                            "WAV file reached 4 GiB RIFF limit during shutdown drain. \
                             Remaining audio discarded."
                        );
                        break;
                    }
                    writer.write_block(&block).await?;
                }
            }
            // Finalize the WAV header and sync to disk to guarantee a valid file.
            if let Some(writer) = wav_writer.take() {
                writer
                    .finalize()
                    .await
                    .context("Failed to finalize WAV file on shutdown")?;
                log::info!("⏹️  Safely closed the capture file.");
            }

            // Report detected overruns to the user (potential audio data loss)
            let overruns = OVERRUN_COUNT.load(Ordering::Relaxed);
            if overruns > 0 {
                log::warn!(
                    "⚠️  Detected {} ring buffer overruns — possible audio data loss.",
                    overruns
                );
            }

            break;
        } else {
            // Check hint flag before sleeping to reduce unnecessary poll latency.
            // The flag is set Relaxed by the RT producer; we reset it here before sleeping
            // to avoid missing a notification that arrives during the sleep window.
            if let Some(ref flag) = recording_data_available {
                flag.store(false, Ordering::Relaxed);
            }
            // Brief sleep to avoid busy-spinning while allowing timely SHUTDOWN detection.
            // Wakeup latency of 10ms is acceptable for a disk writer (not latency-sensitive).
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    Ok(())
}
