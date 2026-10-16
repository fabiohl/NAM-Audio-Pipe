// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the end-of-session capture summary (F-UX-11), including the
//! two-segment acceptance case.

use super::*;

fn segment(name: &str, frames: u64, rate: u32) -> CaptureSegment {
    CaptureSegment {
        path: PathBuf::from(name),
        frames,
        data_bytes: frames * 8, // stereo f32
        sample_rate: rate,
    }
}

#[test]
fn empty_summary_renders_nothing() {
    let summary = CaptureSummary::new(PathBuf::from("/tmp/caps"));
    assert!(summary.render().is_empty());
    assert_eq!(summary.total_bytes(), 0);
    assert_eq!(summary.duration(), Duration::ZERO);
}

#[test]
fn two_segments_sum_duration_and_size() {
    let mut summary = CaptureSummary::new(PathBuf::from("/music/NAM-Audio-Pipe"));
    summary.push_segment(segment("a.wav", 48_000, 48_000)); // 1 s
    summary.push_segment(segment("a_part2.wav", 96_000, 48_000)); // 2 s
    summary.add_block(0, 0); // no-op guard

    assert_eq!(summary.duration(), Duration::from_secs(3));
    assert_eq!(summary.total_bytes(), (48_000 + 96_000) * 8);

    let rendered = summary.render();
    assert!(rendered.contains("/music/NAM-Audio-Pipe"), "{rendered}");
    assert!(rendered.contains("2 segments"), "{rendered}");
    assert!(rendered.contains("0m 03s"), "{rendered}");
    assert!(rendered.contains("MiB"), "{rendered}");
}

#[test]
fn blocks_accumulate_into_the_last_segment() {
    let mut summary = CaptureSummary::new(PathBuf::from("/tmp/caps"));
    summary.push_segment(segment("a.wav", 0, 48_000));
    summary.add_block(256, 2048);
    summary.add_block(256, 2048);
    assert_eq!(summary.segments[0].frames, 512);
    assert_eq!(summary.segments[0].data_bytes, 4096);
    assert_eq!(summary.total_bytes(), 4096);
}

#[test]
fn duration_accounts_for_mixed_sample_rates() {
    let mut summary = CaptureSummary::new(PathBuf::from("/tmp/caps"));
    summary.push_segment(segment("a.wav", 48_000, 48_000)); // 1 s
    summary.push_segment(segment("a_part2.wav", 44_100, 44_100)); // 1 s
    assert_eq!(summary.duration(), Duration::from_secs(2));
}

#[test]
fn formatters_are_human_readable() {
    assert_eq!(format_duration(Duration::from_secs(59)), "0m 59s");
    assert_eq!(format_duration(Duration::from_secs(131)), "2m 11s");
    assert_eq!(format_duration(Duration::from_secs(3661)), "1h 01m 01s");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(2048), "2.0 KiB");
    assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
    assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
}
