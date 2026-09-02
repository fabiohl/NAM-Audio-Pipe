// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! WAV file header generation and atomic, TOCTOU-free capture naming.
//!
//! Provides pure mathematical and formatting utilities:
//! - `build_wav_header`: Generates standard 44-byte (or format-patched) WAV headers
//!   for 32-bit float PCM using `hound`, with **checked** RIFF size arithmetic so a
//!   `u32` size field can never wrap around.
//! - `current_capture_timestamp` / `capture_filename`: Pure capture filename
//!   generation. Collision resolution is intentionally *not* done here via
//!   `Path::exists()` (a TOCTOU race); callers combine the pure name with an atomic
//!   `create_new(true)` open and retry on `AlreadyExists`.

use anyhow::{Context, Result};

use crate::recording::buffer::AudioMetadata;

/// Returns the `YYYYMMDD_HHMMSS` timestamp used in capture filenames, or
/// `"unknown"` if the system clock cannot be read.
pub fn current_capture_timestamp() -> String {
    let mut t: libc::time_t = 0;
    unsafe { libc::time(&mut t) };
    let mut tm_buf: libc::tm = unsafe { std::mem::zeroed() };
    let tm = unsafe { libc::localtime_r(&t, &mut tm_buf) };

    if tm.is_null() {
        "unknown".to_string()
    } else {
        format!(
            "{:04}{:02}{:02}_{:02}{:02}{:02}",
            tm_buf.tm_year + 1900,
            tm_buf.tm_mon + 1,
            tm_buf.tm_mday,
            tm_buf.tm_hour,
            tm_buf.tm_min,
            tm_buf.tm_sec
        )
    }
}

/// Builds a capture filename for a given `timestamp`, segment `part` and
/// collision `suffix`.
///
/// * `part <= 1`, `suffix == 0` → `capture_YYYYMMDD_HHMMSS.wav`
/// * `part <= 1`, `suffix > 0`  → `capture_YYYYMMDD_HHMMSS-N.wav`
/// * `part > 1`, `suffix == 0`  → `capture_YYYYMMDD_HHMMSS_partN.wav`
/// * `part > 1`, `suffix > 0`   → `capture_YYYYMMDD_HHMMSS_partN-M.wav`
///
/// Pure formatting with no filesystem access: the caller pairs it with an
/// atomic `create_new(true)` open so collisions are resolved by the kernel
/// (`AlreadyExists`) instead of a racy `exists()` pre-check.
pub fn capture_filename(timestamp: &str, part: u32, suffix: u32) -> String {
    if part <= 1 {
        if suffix == 0 {
            format!("capture_{timestamp}.wav")
        } else {
            format!("capture_{timestamp}-{suffix}.wav")
        }
    } else if suffix == 0 {
        format!("capture_{timestamp}_part{part}.wav")
    } else {
        format!("capture_{timestamp}_part{part}-{suffix}.wav")
    }
}

/// Generates a standard WAV header for IEEE Float 32-bit PCM using the `hound` crate.
/// `data_bytes` is the total size of raw audio data (may be 0 initially).
///
/// The generation process creates a valid header via `hound` and then applies surgical patches
/// to the size fields (RIFF, data, fact) to reflect the actual amount of data written.
/// Chunk scanning is done via explicit search for robustness against internal `hound` changes.
pub fn build_wav_header(meta: &AudioMetadata, data_bytes: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: meta.channels,
        sample_rate: meta.sample_rate as u32,
        bits_per_sample: meta.bit_depth,
        sample_format: hound::SampleFormat::Float,
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        // Writing 0 samples produces only the header.
        let writer = hound::WavWriter::new(&mut cursor, spec)
            .context("Failed to create WAV header writer")?;
        writer
            .finalize()
            .context("Failed to finalize WAV header writer")?;
    }
    let mut header = cursor.into_inner();

    // Patch the RIFF chunk size (bytes 4..8) with checked arithmetic so the
    // `u32` size field can never wrap around.
    let header_overhead = (header.len() as u64)
        .checked_sub(8)
        .context("Malformed WAV header: length is less than 8 bytes")?;
    let riff_size = header_overhead
        .checked_add(data_bytes as u64)
        .context("RIFF chunk size calculation overflowed u64")?;
    if riff_size > u32::MAX as u64 {
        anyhow::bail!("RIFF chunk size exceeds 32-bit limit: {riff_size}");
    }
    header[4..8].copy_from_slice(&(riff_size as u32).to_le_bytes());

    // Patch the "data" chunk size — explicit search via `rposition` for robustness,
    // avoiding the assumption that the last 4 bytes of the header are necessarily the size field.
    let data_pos = header
        .array_windows::<4>()
        .rposition(|w| w == b"data")
        .context("'data' chunk not found in the hound-generated WAV header")?;
    if data_pos + 8 > header.len() {
        anyhow::bail!("Malformed WAV header: 'data' chunk is truncated");
    }
    header[data_pos + 4..data_pos + 8].copy_from_slice(&data_bytes.to_le_bytes());

    // Patch the sample count field of the "fact" chunk (required for Float format).
    // Validates chunk size and per-frame width before patching for robustness
    // against `hound` changes and malformed metadata (zero-width frames would
    // otherwise divide by zero).
    if meta.bit_depth == 32 {
        let bytes_per_frame = meta.channels as u32 * (meta.bit_depth as u32 / 8);
        if bytes_per_frame == 0 {
            anyhow::bail!(
                "Malformed WAV metadata: zero bytes per frame (channels={}, bit_depth={})",
                meta.channels,
                meta.bit_depth
            );
        }
        let samples_per_channel = data_bytes / bytes_per_frame;
        if let Some(fact_pos) = header.array_windows::<4>().position(|w| w == b"fact")
            && fact_pos + 12 <= header.len()
        {
            let chunk_size = u32::from_le_bytes(
                header[fact_pos + 4..fact_pos + 8]
                    .try_into()
                    .expect("4-byte slice to u32 conversion must be infallible"),
            );
            if chunk_size >= 4 {
                header[fact_pos + 8..fact_pos + 12]
                    .copy_from_slice(&samples_per_channel.to_le_bytes());
            }
        }
    }

    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_wav_header_basic() {
        let meta = AudioMetadata {
            channels: 2,
            sample_rate: 48000.0,
            bit_depth: 32,
        };
        let header = build_wav_header(&meta, 0).expect("failed to build wav header");
        assert!(header.len() >= 44);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[8..12], b"WAVE");
    }

    #[test]
    fn test_build_wav_header_with_data() {
        let meta = AudioMetadata {
            channels: 2,
            sample_rate: 48000.0,
            bit_depth: 32,
        };
        let data_bytes = 192000; // 1 second of stereo 32-bit float audio
        let header = build_wav_header(&meta, data_bytes).expect("failed to build wav header");

        // Verify RIFF chunk size patch
        let file_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
        assert_eq!(file_size, header.len() as u32 - 8 + data_bytes);

        // Verify data chunk size patch
        let data_pos = header
            .array_windows::<4>()
            .rposition(|w| w == b"data")
            .unwrap();
        let chunk_data_bytes =
            u32::from_le_bytes(header[data_pos + 4..data_pos + 8].try_into().unwrap());
        assert_eq!(chunk_data_bytes, data_bytes);
    }

    #[test]
    fn test_capture_filename_patterns() {
        let ts = "20260826_164500";
        // Base capture (part <= 1, no suffix).
        assert_eq!(capture_filename(ts, 1, 0), "capture_20260826_164500.wav");
        assert_eq!(capture_filename(ts, 0, 0), "capture_20260826_164500.wav");
        // Collision suffix on the base capture.
        assert_eq!(capture_filename(ts, 1, 1), "capture_20260826_164500-1.wav");
        assert_eq!(
            capture_filename(ts, 1, 42),
            "capture_20260826_164500-42.wav"
        );
        // Sequential part, no collision.
        assert_eq!(
            capture_filename(ts, 2, 0),
            "capture_20260826_164500_part2.wav"
        );
        // Sequential part with collision suffix.
        assert_eq!(
            capture_filename(ts, 2, 1),
            "capture_20260826_164500_part2-1.wav"
        );
        assert_eq!(
            capture_filename(ts, 10, 3),
            "capture_20260826_164500_part10-3.wav"
        );
    }

    #[test]
    fn test_current_capture_timestamp_shape() {
        let ts = current_capture_timestamp();
        // Either the real `YYYYMMDD_HHMMSS` or the "unknown" fallback.
        assert!(
            ts == "unknown" || (ts.len() == 15 && ts.as_bytes()[8] == b'_'),
            "unexpected timestamp shape: {ts:?}"
        );
    }

    #[test]
    fn test_build_wav_header_riff_boundary_exact() {
        let meta = AudioMetadata {
            channels: 2,
            sample_rate: 48000.0,
            bit_depth: 32,
        };
        // `max_data_payload` makes the RIFF size field equal exactly u32::MAX
        // (the largest representable value) — still a valid header.
        let header0 = build_wav_header(&meta, 0).expect("failed to build empty header");
        let header_overhead = header0.len() as u64 - 8;
        let max_data_payload = u32::MAX as u64 - header_overhead;

        let header = build_wav_header(&meta, max_data_payload as u32)
            .expect("RIFF size == u32::MAX must be accepted");
        let riff_size = u32::from_le_bytes(header[4..8].try_into().unwrap());
        assert_eq!(riff_size as u64, max_data_payload + header_overhead);
        assert_eq!(riff_size, u32::MAX);
    }

    #[test]
    fn test_build_wav_header_riff_overflow_rejected() {
        let meta = AudioMetadata {
            channels: 2,
            sample_rate: 48000.0,
            bit_depth: 32,
        };
        let header0 = build_wav_header(&meta, 0).expect("failed to build empty header");
        let header_overhead = header0.len() as u64 - 8;
        let max_data_payload = u32::MAX as u64 - header_overhead;

        // One byte past the RIFF 32-bit limit must fail explicitly (no wrap).
        let err = build_wav_header(&meta, max_data_payload as u32 + 1)
            .expect_err("RIFF size beyond u32::MAX must be rejected");
        assert!(
            format!("{err:#}").contains("exceeds 32-bit limit"),
            "unexpected error: {err:#}"
        );
    }
}
