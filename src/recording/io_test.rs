// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the injectable positioned-write backend.
//!
//! Exercises [`write_all_at`] against the [`FaultInjectingWriter`] mock with
//! short writes of 1 byte, 7 bytes and arbitrary patterns, zero-progress
//! (`Ok(0)`), `ENOSPC` and `EIO`, proving the persisted bytes are always
//! bit-identical to the input or the failure is explicit and observable.

use std::io::ErrorKind;

use crate::recording::io::{FaultInjectingWriter, write_all_at};

/// Deterministic non-trivial byte pattern (xorshift32 LFSR) so short-write
/// reconstruction is exercised over varied data, not zeros.
fn pseudo_random(len: usize) -> Vec<u8> {
    (0..len)
        .scan(0x9e37_79b9u32, |state, _| {
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            Some((*state & 0xff) as u8)
        })
        .collect()
}

#[tokio::test]
async fn write_all_at_writes_everything_without_faults() {
    let data = pseudo_random(4096);
    let mut mock = FaultInjectingWriter::new();
    let (res, buf) = write_all_at(&mut mock, data.clone(), 0).await;
    assert!(res.is_ok());
    assert_eq!(mock.disk(), &data[..]);
    assert!(buf.is_empty(), "full write must return an empty buffer");
    assert_eq!(mock.write_calls(), 1);
    assert!(
        buf.capacity() >= data.len(),
        "buffer allocation must be preserved for reuse"
    );
}

#[tokio::test]
async fn write_all_at_short_write_1_byte_reconstructs_bit_for_bit() {
    let data = pseudo_random(2048);
    let mut mock = FaultInjectingWriter::new().with_short_writes(1);
    let (res, buf) = write_all_at(&mut mock, data.clone(), 0).await;
    assert!(res.is_ok(), "short writes must not fail the operation");
    assert_eq!(mock.disk(), &data[..], "1-byte short writes corrupted data");
    assert!(buf.is_empty());
    assert_eq!(mock.write_calls(), data.len());
}

#[tokio::test]
async fn write_all_at_short_write_7_bytes_reconstructs_bit_for_bit() {
    let data = pseudo_random(4096);
    let mut mock = FaultInjectingWriter::new().with_short_writes(7);
    let (res, buf) = write_all_at(&mut mock, data.clone(), 0).await;
    assert!(res.is_ok());
    assert_eq!(mock.disk(), &data[..], "7-byte short writes corrupted data");
    assert!(buf.is_empty());
    assert_eq!(mock.write_calls(), data.len().div_ceil(7));
}

#[tokio::test]
async fn write_all_at_arbitrary_chunk_pattern_reconstructs_bit_for_bit() {
    let data = pseudo_random(10_000);
    let pattern = [1usize, 3, 7, 11, 257, 1024];
    let mut mock = FaultInjectingWriter::new().with_chunk_sizes(&pattern);
    let (res, buf) = write_all_at(&mut mock, data.clone(), 0).await;
    assert!(res.is_ok());
    assert_eq!(
        mock.disk(),
        &data[..],
        "arbitrary short writes corrupted data"
    );
    assert!(buf.is_empty());
}

#[tokio::test]
async fn write_all_at_empty_buffer_is_no_op() {
    let mut mock = FaultInjectingWriter::new().with_short_writes(1);
    let (res, buf) = write_all_at(&mut mock, Vec::new(), 42).await;
    assert!(res.is_ok());
    assert!(buf.is_empty());
    assert_eq!(
        mock.write_calls(),
        0,
        "empty buffer must not hit the backend"
    );
}

#[tokio::test]
async fn write_all_at_zero_progress_returns_write_zero() {
    let data = pseudo_random(256);
    let mut mock = FaultInjectingWriter::new().with_short_writes(0);
    let (res, buf) = write_all_at(&mut mock, data.clone(), 0).await;
    assert_eq!(res.unwrap_err().kind(), ErrorKind::WriteZero);
    assert_eq!(
        buf, data,
        "zero-progress must return the buffer intact, not spin forever"
    );
}

#[tokio::test]
async fn write_all_at_enospc_after_partial_progress_preserves_remainder() {
    let data = pseudo_random(1024);
    let mut mock = FaultInjectingWriter::new()
        .with_short_writes(64)
        .fail_after(3, ErrorKind::StorageFull);
    let (res, buf) = write_all_at(&mut mock, data.clone(), 0).await;
    assert_eq!(res.unwrap_err().kind(), ErrorKind::StorageFull);
    // Exactly 3 x 64 bytes landed before the injected ENOSPC.
    assert_eq!(mock.disk().len(), 192);
    assert_eq!(mock.disk(), &data[..192]);
    // The returned buffer holds the unwritten remainder (not the full input),
    // so the caller can retry or report exactly what did not persist.
    assert_eq!(buf, &data[192..]);
    assert!(
        buf.capacity() >= data.len(),
        "allocation must survive errors"
    );
}

#[tokio::test]
async fn write_all_at_eio_fails_and_preserves_buffer() {
    let data = pseudo_random(512);
    let mut mock = FaultInjectingWriter::new().fail_after(0, ErrorKind::Other);
    let (res, buf) = write_all_at(&mut mock, data.clone(), 0).await;
    assert_eq!(res.unwrap_err().kind(), ErrorKind::Other);
    assert_eq!(
        buf, data,
        "EIO before any write must return the buffer intact"
    );
    assert!(mock.disk().is_empty());
}

#[tokio::test]
async fn write_all_at_respects_non_zero_offsets() {
    let data = pseudo_random(777);
    let offset = 1000u64;
    let mut mock = FaultInjectingWriter::new().with_short_writes(13);
    let (res, buf) = write_all_at(&mut mock, data.clone(), offset).await;
    assert!(res.is_ok());
    assert!(buf.is_empty());
    let disk = mock.disk();
    assert_eq!(disk.len(), offset as usize + data.len());
    assert!(disk[..offset as usize].iter().all(|&b| b == 0));
    assert_eq!(&disk[offset as usize..], &data[..]);
}
