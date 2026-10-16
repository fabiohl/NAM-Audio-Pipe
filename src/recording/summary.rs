// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! End-of-session capture summary (F-UX-11).
//!
//! The recording worker accumulates one [`CaptureSegment`] per `_partN` file it
//! opens (a mid-stream rate change or the 4 GiB RIFF rollover both start a new
//! segment). When the capture is finalized the worker logs a single
//! human-readable line naming the output directory, the total duration and the
//! total size across **all** segments, so the musician knows exactly where the
//! take landed and how big it is.
//!
//! Everything here is pure formatting over plain data, so the summary line is
//! unit-testable with synthetic two-segment sessions without touching io_uring.

use std::path::PathBuf;
use std::time::Duration;

/// One finalized capture segment (`capture_..._partN.wav`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSegment {
    /// Absolute path of the segment file.
    pub path: PathBuf,
    /// Audio payload bytes written (excludes the WAV header).
    pub data_bytes: u64,
    /// Stereo frames written.
    pub frames: u64,
    /// Sample rate the segment was written at.
    pub sample_rate: u32,
}

/// Aggregated summary of a recording session across every segment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CaptureSummary {
    /// Output directory the capture was written to.
    pub directory: PathBuf,
    /// Segments in creation order.
    pub segments: Vec<CaptureSegment>,
}

impl CaptureSummary {
    /// Builds an empty summary for `directory`.
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            segments: Vec::new(),
        }
    }

    /// Registers a freshly opened segment.
    pub fn push_segment(&mut self, segment: CaptureSegment) {
        self.segments.push(segment);
    }

    /// Adds a written block to the most recently opened segment.
    ///
    /// No-op when no segment is open (a block can never precede the first
    /// `Metadata` payload).
    pub fn add_block(&mut self, frames: u64, bytes: u64) {
        if let Some(segment) = self.segments.last_mut() {
            segment.frames += frames;
            segment.data_bytes += bytes;
        }
    }

    /// Total audio payload bytes across every segment.
    pub fn total_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.data_bytes).sum()
    }

    /// Wall-clock duration of the captured audio (sum of per-segment
    /// `frames / rate`, so a mid-stream rate change is accounted correctly).
    pub fn duration(&self) -> Duration {
        let secs: f64 = self
            .segments
            .iter()
            .map(|s| {
                if s.sample_rate == 0 {
                    0.0
                } else {
                    s.frames as f64 / s.sample_rate as f64
                }
            })
            .sum();
        Duration::from_secs_f64(secs.max(0.0))
    }

    /// Renders the end-of-session line(s), or an empty string when nothing was
    /// captured (no segment was ever opened).
    pub fn render(&self) -> String {
        if self.segments.is_empty() {
            return String::new();
        }
        let plural = if self.segments.len() == 1 { "" } else { "s" };
        format!(
            "💾 Capture saved: {} ({} across {} segment{})",
            self.directory.display(),
            format_capture_size(self.duration(), self.total_bytes()),
            self.segments.len(),
            plural,
        )
    }
}

/// Formats a `(duration, bytes)` pair as `"<dur>, <size>"` (e.g. `2m11s, 21.4 MiB`).
pub fn format_capture_size(duration: Duration, bytes: u64) -> String {
    format!("{}, {}", format_duration(duration), format_bytes(bytes))
}

/// Formats a duration as `[Hh ]Mm Ss` (seconds always shown).
pub fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else {
        format!("{m}m {s:02}s")
    }
}

/// Formats a byte count with binary units and one decimal place.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= KIB * KIB * KIB {
        format!("{:.1} GiB", b / (KIB * KIB * KIB))
    } else if b >= KIB * KIB {
        format!("{:.1} MiB", b / (KIB * KIB))
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
#[path = "summary_test.rs"]
mod summary_test;
