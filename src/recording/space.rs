// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Disk-space estimation and low-space pre-warning (F-UX-02b).
//!
//! The recording worker originally detected a full disk only when the write itself
//! failed with `ENOSPC` (after the fact). A long take could therefore consume the
//! whole volume and abort mid-session. This module adds an early warning layer:
//!
//! * a startup estimate of how many minutes the current volume can still hold
//!   (logged when the first segment opens), and
//! * a low-space check (every [`SPACE_CHECK_INTERVAL`] and on every new segment)
//!   that publishes a warning through the shared [`RecordingStatus`] so the main
//!   control loop's [`crate::recording::alert::RecordingAlert`] renders it —
//!   one single output path for failures and pre-warnings alike.
//!
//! [`FreeSpace`] is a trait so the threshold logic is proven with an injected
//! probe, and the whole threshold math is pure and unit-tested without a disk.
//!
//! [`RecordingStatus`]: crate::recording::status::RecordingStatus

use std::path::Path;
use std::time::Duration;

use crate::recording::buffer::AudioMetadata;
use crate::recording::summary::format_bytes;

/// Below this many free bytes the volume warns regardless of the remaining time.
pub const LOW_SPACE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

/// Below this many remaining minutes the volume warns.
pub const LOW_SPACE_MINUTES: f64 = 5.0;

/// How often the worker re-checks free space between segment openings.
pub const SPACE_CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// Injectable free-space probe (production: `statvfs`).
pub trait FreeSpace {
    /// Returns the bytes available to the (unprivileged) writer at `path`.
    fn available_bytes(&self, path: &Path) -> std::io::Result<u64>;
}

/// Production probe backed by `libc::statvfs`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StatvfsFreeSpace;

impl FreeSpace for StatvfsFreeSpace {
    fn available_bytes(&self, path: &Path) -> std::io::Result<u64> {
        statvfs_available_bytes(path)
    }
}

/// Queries `statvfs(2)` for the space available to an unprivileged writer
/// (`f_bavail`), in bytes.
fn statvfs_available_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `libc::statvfs` writes the filesystem statistics into the supplied
    // `&mut statvfs`. `buf` is a properly aligned, fully initialized (zeroed)
    // `libc::statvfs` on the stack and `c_path` is a valid NUL-terminated C
    // string; on failure the call returns non-zero and `buf` is not read.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated C string and `buf` is a
    // properly aligned, fully initialized `libc::statvfs`; on failure the call
    // returns non-zero and `buf` is not read.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64))
}

/// Audio payload bytes captured per second for `meta`.
pub fn bytes_per_second(meta: &AudioMetadata) -> f64 {
    meta.sample_rate as f64 * meta.channels as f64 * (meta.bit_depth as f64 / 8.0)
}

/// Remaining writable minutes at the current format, given `free_bytes`.
pub fn writable_minutes(free_bytes: u64, meta: &AudioMetadata) -> f64 {
    let bps = bytes_per_second(meta);
    if bps <= 0.0 {
        0.0
    } else {
        free_bytes as f64 / bps / 60.0
    }
}

/// Startup estimate line: minutes writable at the current format.
pub fn estimate_message(free_bytes: u64, meta: &AudioMetadata) -> String {
    let minutes = writable_minutes(free_bytes, meta);
    format!(
        "≈ {minutes:.0} min writable at {} Hz / {} ch ({} free)",
        meta.sample_rate as u32,
        meta.channels,
        format_bytes(free_bytes),
    )
}

/// Returns a low-space warning when the volume is below either threshold.
pub fn low_space_warning(free_bytes: u64, meta: &AudioMetadata) -> Option<String> {
    let minutes = writable_minutes(free_bytes, meta);
    if free_bytes < LOW_SPACE_BYTES || minutes < LOW_SPACE_MINUTES {
        Some(format!(
            "{} free (~{minutes:.0} min of recording left)",
            format_bytes(free_bytes),
        ))
    } else {
        None
    }
}

#[cfg(test)]
#[path = "space_test.rs"]
mod space_test;
