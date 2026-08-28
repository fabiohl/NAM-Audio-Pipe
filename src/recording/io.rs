// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Injectable positioned-write I/O backend for the WAV recording subsystem.
//!
//! [`tokio_uring::fs::File::write_at`] documents that a single call may write
//! only a *prefix* of the supplied buffer without signalling an error (short
//! write). Persisting a WAV header or an audio block partially would silently
//! corrupt the recorded stream, so every recording write goes through
//! [`write_all_at`], which loops until 100% of the buffer is written or the
//! operation fails explicitly (F-RB-008 / T3.1).
//!
//! The [`WriteAt`] trait abstracts the positioned-write file so the real
//! `io_uring` file and the test-only `FaultInjectingWriter` mock are
//! interchangeable: short writes of arbitrary byte counts, `ENOSPC` and `EIO`
//! faults are injected without touching a real disk.

use std::io;

/// Positioned-write file backend used by the WAV writer.
///
/// Mirrors the `tokio_uring` owned-buffer contract: each call takes ownership
/// of the buffer, performs a single `write_at` submission and returns both the
/// operation result and the buffer, so the caller can reuse the allocation.
/// A successful call may still be a *short write* (`Ok(n)` with `n < len`);
/// callers that require full persistence must use [`write_all_at`].
pub(crate) trait WriteAt {
    /// Writes up to `buf.len()` bytes at `offset`, returning the number of
    /// bytes actually written (possibly fewer than requested).
    async fn write_at(&mut self, buf: Vec<u8>, offset: u64) -> (io::Result<usize>, Vec<u8>);

    /// Flushes the file to durable storage.
    async fn sync_all(&mut self) -> io::Result<()>;
}

impl WriteAt for tokio_uring::fs::File {
    async fn write_at(&mut self, buf: Vec<u8>, offset: u64) -> (io::Result<usize>, Vec<u8>) {
        // Fully qualified: `File::write_at` is the inherent io_uring submission;
        // the trait method of the same name must not shadow it.
        tokio_uring::fs::File::write_at(self, buf, offset)
            .submit()
            .await
    }

    async fn sync_all(&mut self) -> io::Result<()> {
        tokio_uring::fs::File::sync_all(self).await
    }
}

/// Writes the whole `buf` to `file` at `offset`, looping over short writes
/// until every byte is persisted.
///
/// Returns the operation result and the buffer so the caller can reuse the
/// allocation for the next block — zero additional allocations on the
/// continuous-write path. On success the returned buffer is empty (everything
/// was written); on failure it holds the unwritten remainder (the already
/// persisted prefix lives on the backing file).
///
/// # Errors
///
/// * [`io::ErrorKind::WriteZero`] when the backend reports `Ok(0)` with bytes
///   still pending (the file can no longer accept data).
/// * [`io::ErrorKind::InvalidData`] when the backend claims to have written
///   more bytes than the buffer holds (defensive; the kernel contract forbids
///   this).
/// * The first I/O error surfaced by the backend otherwise.
pub(crate) async fn write_all_at<W: WriteAt>(
    file: &mut W,
    mut buf: Vec<u8>,
    mut offset: u64,
) -> (io::Result<()>, Vec<u8>) {
    while !buf.is_empty() {
        let (res, returned) = file.write_at(buf, offset).await;
        buf = returned;
        match res {
            Ok(0) => {
                return (
                    Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    )),
                    buf,
                );
            }
            Ok(written) => {
                if written > buf.len() {
                    return (
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "writer reported more bytes than buffer length",
                        )),
                        buf,
                    );
                }
                offset += written as u64;
                // Drop the persisted prefix; the remaining bytes stay in the
                // same allocation, so the common full-write case costs nothing
                // and pathological short writes only shift the unwritten tail.
                buf.drain(..written);
            }
            Err(e) => return (Err(e), buf),
        }
    }
    (Ok(()), buf)
}

/// Test-only configurable writer that simulates disk faults.
///
/// `write_at` records bytes into an in-memory "disk" (`Vec<u8>`) addressed by
/// file offset, exactly like the kernel would, but constrained by a
/// configurable per-call byte budget. It can inject short writes (including
/// the zero-progress `Ok(0)` case), `ENOSPC` and `EIO` failures after a
/// configurable number of successful writes.
#[cfg(test)]
pub(crate) struct FaultInjectingWriter {
    /// Simulated on-disk bytes, addressed by file offset.
    disk: Vec<u8>,
    /// Per-call write budget cycle. `usize::MAX` = write everything in one
    /// shot (no short write); `0` = zero-progress (`Ok(0)`).
    chunk_sizes: Vec<usize>,
    /// Fail after this many successful (non-zero) `write_at` calls.
    fail_after_ok: Option<usize>,
    /// Error kind used when failing (defaults to `StorageFull` = ENOSPC).
    fail_kind: Option<io::ErrorKind>,
    /// Total `write_at` submissions performed.
    write_calls: usize,
    /// Successful (non-zero) `write_at` submissions performed.
    ok_calls: usize,
    /// Number of `sync_all` invocations.
    sync_calls: usize,
}

#[cfg(test)]
impl FaultInjectingWriter {
    pub(crate) fn new() -> Self {
        Self {
            disk: Vec::new(),
            chunk_sizes: vec![usize::MAX],
            fail_after_ok: None,
            fail_kind: None,
            write_calls: 0,
            ok_calls: 0,
            sync_calls: 0,
        }
    }

    /// Simulates short writes delivering at most `max_bytes` per call.
    pub(crate) fn with_short_writes(mut self, max_bytes: usize) -> Self {
        self.chunk_sizes = vec![max_bytes];
        self
    }

    /// Cycles through arbitrary per-call write budgets (e.g. `[1, 3, 7, 11]`).
    pub(crate) fn with_chunk_sizes(mut self, sizes: &[usize]) -> Self {
        self.chunk_sizes = sizes.to_vec();
        self
    }

    /// Fails every subsequent call with `kind` after `ok_calls` successful
    /// (non-zero) writes.
    pub(crate) fn fail_after(mut self, ok_calls: usize, kind: io::ErrorKind) -> Self {
        self.fail_after_ok = Some(ok_calls);
        self.fail_kind = Some(kind);
        self
    }

    /// Bytes currently "persisted" on the simulated disk.
    pub(crate) fn disk(&self) -> &[u8] {
        &self.disk
    }

    /// Total `write_at` submissions performed.
    pub(crate) fn write_calls(&self) -> usize {
        self.write_calls
    }

    /// Number of `sync_all` invocations performed.
    pub(crate) fn sync_calls(&self) -> usize {
        self.sync_calls
    }
}

#[cfg(test)]
impl Default for FaultInjectingWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl WriteAt for FaultInjectingWriter {
    async fn write_at(&mut self, buf: Vec<u8>, offset: u64) -> (io::Result<usize>, Vec<u8>) {
        self.write_calls += 1;
        if let Some(limit) = self.fail_after_ok
            && self.ok_calls >= limit
        {
            let kind = self.fail_kind.unwrap_or(io::ErrorKind::StorageFull);
            return (Err(io::Error::new(kind, "injected write fault")), buf);
        }

        let budget = self.chunk_sizes[(self.write_calls - 1) % self.chunk_sizes.len()];
        let n = buf.len().min(budget);
        if n > 0 {
            let start = offset as usize;
            let end = start + n;
            if self.disk.len() < end {
                self.disk.resize(end, 0);
            }
            self.disk[start..end].copy_from_slice(&buf[..n]);
            self.ok_calls += 1;
        }
        (Ok(n), buf)
    }

    async fn sync_all(&mut self) -> io::Result<()> {
        self.sync_calls += 1;
        Ok(())
    }
}

#[cfg(test)]
#[path = "io_test.rs"]
mod io_test;
