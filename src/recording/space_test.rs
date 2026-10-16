// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the disk-space estimation and low-space thresholds (F-UX-02b),
//! using injected free-space values (no real disk).

use super::*;

const META: AudioMetadata = AudioMetadata {
    sample_rate: 48000.0,
    bit_depth: 32,
    channels: 2,
};

/// Injected probe returning a fixed free-space value.
struct Fixed(u64);

impl FreeSpace for Fixed {
    fn available_bytes(&self, _path: &Path) -> std::io::Result<u64> {
        Ok(self.0)
    }
}

#[test]
fn bytes_per_second_matches_the_capture_format() {
    // 48 kHz × 2 ch × 4 bytes = 384 000 B/s.
    assert_eq!(bytes_per_second(&META), 384_000.0);
}

#[test]
fn writable_minutes_scales_with_free_space() {
    let ten_minutes = (384_000.0 * 60.0 * 10.0) as u64;
    assert!((writable_minutes(ten_minutes, &META) - 10.0).abs() < 0.01);
    assert_eq!(writable_minutes(0, &META), 0.0);
}

#[test]
fn no_warning_with_comfortable_headroom() {
    // 2 GiB at 48 kHz stereo ≈ 93 min: above both thresholds.
    let free = 2 * 1024 * 1024 * 1024;
    assert_eq!(low_space_warning(free, &META), None);
}

#[test]
fn warns_below_one_gib() {
    let free = 800 * 1024 * 1024; // 0.78 GiB
    let warning = low_space_warning(free, &META).expect("below 1 GiB must warn");
    assert!(warning.contains("free"), "{warning}");
    assert!(warning.contains("min of recording left"), "{warning}");
}

#[test]
fn warns_below_five_minutes_even_with_lots_of_bytes() {
    // A very high rate makes 2 GiB hold < 5 min; the time threshold fires even
    // though the byte threshold does not.
    let high_rate = AudioMetadata {
        sample_rate: 20_000_000.0,
        bit_depth: 32,
        channels: 2,
    };
    let free = 2 * 1024 * 1024 * 1024;
    assert!(low_space_warning(free, &high_rate).is_some());
}

#[test]
fn estimate_message_is_human_readable() {
    let free = 5 * 1024 * 1024 * 1024;
    let msg = estimate_message(free, &META);
    assert!(msg.contains("min writable"), "{msg}");
    assert!(msg.contains("48000 Hz"), "{msg}");
    assert!(msg.contains("GiB"), "{msg}");
}

#[test]
fn injected_probe_is_honored() {
    let probe = Fixed(1234);
    assert_eq!(probe.available_bytes(Path::new("/tmp")).unwrap(), 1234);
}
