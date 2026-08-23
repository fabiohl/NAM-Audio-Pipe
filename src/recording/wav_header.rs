// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! WAV file header generation and incremental filename resolution.
//!
//! Provides pure mathematical and formatting utilities:
//! - `build_wav_header`: Generates standard 44-byte (or format-patched) WAV headers for 32-bit float PCM using `hound`.
//! - `resolve_available_filename`: Resolves timestamp-based capture filenames and collision increments.

use anyhow::{Context, Result};
use core::fmt::NumBuffer;
use std::path::{Path, PathBuf};

use crate::recording::buffer::AudioMetadata;

/// Generates the WAV filename based on the current timestamp.
/// Resolves collisions by appending an incremental suffix (`-1`, `-2`, ...).
///
/// Format: `capture_YYYYMMDD_HHMMSS.wav`
/// Collided: `capture_YYYYMMDD_HHMMSS-1.wav`
/// Parts: `capture_YYYYMMDD_HHMMSS_partN.wav`
pub fn resolve_available_filename(base_dir: &Path, part: u32) -> PathBuf {
    let mut t: libc::time_t = 0;
    unsafe { libc::time(&mut t) };
    let mut tm_buf: libc::tm = unsafe { std::mem::zeroed() };
    let tm = unsafe { libc::localtime_r(&t, &mut tm_buf) };

    let timestamp = if tm.is_null() {
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
    };

    let base_name = if part <= 1 {
        format!("capture_{}.wav", timestamp)
    } else {
        let mut part_buf = NumBuffer::new();
        format!(
            "capture_{}_part{}.wav",
            timestamp,
            part.format_into(&mut part_buf)
        )
    };

    let candidate = base_dir.join(&base_name);
    if part != 1 || !candidate.exists() {
        return candidate;
    }

    for suffix in 1u32.. {
        let mut suffix_buf = NumBuffer::new();
        let alt = base_dir.join(format!(
            "capture_{}-{}.wav",
            timestamp,
            suffix.format_into(&mut suffix_buf)
        ));
        if !alt.exists() {
            return alt;
        }
    }

    // Exhausted 4 billion suffixes — should not happen in practice
    candidate
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

    // Patch the RIFF chunk size (bytes 4..8)
    let file_size = header.len() as u32 - 8 + data_bytes;
    header[4..8].copy_from_slice(&file_size.to_le_bytes());

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
    // Validates chunk size before patching for robustness against `hound` changes.
    if meta.bit_depth == 32 {
        let samples_per_channel = data_bytes / (meta.channels as u32 * (meta.bit_depth as u32 / 8));
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
    fn test_resolve_available_filename() {
        let temp_dir =
            std::env::temp_dir().join(format!("nam_wav_header_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let path1 = resolve_available_filename(&temp_dir, 1);
        assert!(path1.to_string_lossy().contains("capture_"));
        assert!(path1.to_string_lossy().ends_with(".wav"));

        let path_part2 = resolve_available_filename(&temp_dir, 2);
        assert!(path_part2.to_string_lossy().contains("_part2.wav"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
