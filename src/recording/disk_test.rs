// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for `AsyncWavWriter` against the fault-injecting I/O mock
//! and for the atomic `create_new` + TOCTOU-free collision resolution and
//! checked RIFF limits.
//!
//! Proves that with short writes of arbitrary sizes the WAV file reconstructed
//! on the simulated disk is bit-identical to the expected header + interleaved
//! PCM payload, that injected `ENOSPC`/`EIO` failures abort the recording
//! explicitly instead of being masked as success, that capture creation is
//! exclusive (`create_new`) and never overwrites existing files even under
//! concurrent writers, and that the 4 GiB RIFF limit is enforced with
//! mathematical precision (exact boundary vs. `+1 sample`).
//!
//! Also proves the lifecycle decoupling with a deterministic barrier test:
//! `SHUTDOWN` arriving while the ring is momentarily empty can never truncate
//! the blocks produced afterwards — the worker ignores the global flag and
//! drains 100% of the samples into the finalized WAV.

use std::collections::BTreeSet;
use std::io;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use neural_amp_modeler_rs::common::spsc::SHUTDOWN;

use crate::recording::buffer::{AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE};
use crate::recording::disk::{
    AsyncWavWriter, WavSink, create_new_capture, disk_writer_loop_inner, spawn_recording_worker,
    validate_output_dir,
};
use crate::recording::io::{FaultInjectingWriter, WriteAt};
use crate::recording::pool::POOL_CAPACITY;
use crate::recording::probe::IoUringSupport;
use crate::recording::status::{
    RecordingInit, RecordingStartupError, RecordingStatus, SharedRecordingStatus,
    wait_for_recording_init,
};
use crate::recording::transport::create_recording_transport;
use crate::recording::wav_header::{build_wav_header, capture_filename};

const META: AudioMetadata = AudioMetadata {
    sample_rate: 48000.0,
    bit_depth: 32,
    channels: 2,
};

/// Interleaved little-endian f32 bytes for a planar `(left, right)` pair.
fn interleave(left: &[f32], right: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(left.len() * 8);
    for (l, r) in left.iter().zip(right) {
        out.extend_from_slice(&l.to_le_bytes());
        out.extend_from_slice(&r.to_le_bytes());
    }
    out
}

#[tokio::test]
async fn open_writes_initial_header_via_write_all() {
    let mock = FaultInjectingWriter::new().with_short_writes(7);
    let writer = AsyncWavWriter::<FaultInjectingWriter>::open(mock, META)
        .await
        .expect("open must succeed under short writes");
    let expected_header = build_wav_header(&META, 0).expect("header build");
    assert_eq!(
        writer.file.disk(),
        &expected_header[..],
        "initial header must be fully persisted bit-for-bit"
    );
}

#[tokio::test]
async fn short_writes_produce_bit_exact_wav() {
    // Pathological disk delivering 1 byte per write_at call: every single byte
    // of header and PCM must still land in the exact position.
    let mock = FaultInjectingWriter::new().with_short_writes(1);
    let mut writer = AsyncWavWriter::<FaultInjectingWriter>::open(mock, META)
        .await
        .expect("open must succeed");

    let mut expected_payload = Vec::new();
    for ch in 0..3u32 {
        let mut left = Vec::new();
        let mut right = Vec::new();
        for i in 0..96usize {
            let v = (ch * 96 + i as u32) as f32 * 0.125;
            left.push(v);
            right.push(-v);
        }
        expected_payload.extend_from_slice(&interleave(&left, &right));

        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        block.fill_planar(&left, &right);
        writer
            .write_block(&block)
            .await
            .expect("write_block must succeed under 1-byte short writes");
    }

    // Final header carries the real payload size (rewritten at offset 0).
    let expected_header =
        build_wav_header(&META, expected_payload.len() as u32).expect("header build");
    let mut expected_file = expected_header;
    expected_file.extend_from_slice(&expected_payload);

    writer
        .finalize()
        .await
        .expect("finalize must succeed under 1-byte short writes");

    assert_eq!(
        writer.file.disk(),
        &expected_file[..],
        "reconstructed WAV must match expected header + PCM bit-for-bit"
    );
    assert_eq!(writer.data_bytes_written, expected_payload.len() as u32);
    assert_eq!(
        writer.file.sync_calls(),
        1,
        "finalize must fsync exactly once"
    );
}

#[tokio::test]
async fn short_writes_7_bytes_produce_bit_exact_wav() {
    let mock = FaultInjectingWriter::new().with_short_writes(7);
    let mut writer = AsyncWavWriter::<FaultInjectingWriter>::open(mock, META)
        .await
        .expect("open must succeed");

    let left = (0..256).map(|i| i as f32 * 0.001).collect::<Vec<_>>();
    let right = (0..256).map(|i| -(i as f32) * 0.001).collect::<Vec<_>>();
    let expected_payload = interleave(&left, &right);
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    block.fill_planar(&left, &right);
    writer
        .write_block(&block)
        .await
        .expect("write_block must succeed under 7-byte short writes");

    let mut expected_file =
        build_wav_header(&META, expected_payload.len() as u32).expect("header build");
    expected_file.extend_from_slice(&expected_payload);

    writer.finalize().await.expect("finalize must succeed");
    assert_eq!(
        writer.file.disk(),
        &expected_file[..],
        "reconstructed WAV must match expected bytes bit-for-bit"
    );
}

#[tokio::test]
async fn initial_header_failure_propagates_explicitly() {
    // EIO on the very first write_at: open must fail loudly, never leave a
    // partially-written WAV header behind.
    let mock = FaultInjectingWriter::new().fail_after(0, ErrorKind::Other);
    let err = match AsyncWavWriter::<FaultInjectingWriter>::open(mock, META).await {
        Ok(_) => panic!("open must propagate the injected EIO"),
        Err(e) => e,
    };
    assert_eq!(
        err.root_cause()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        ErrorKind::Other
    );
}

#[tokio::test]
async fn block_failure_propagates_and_keeps_accounting_consistent() {
    // Header write succeeds (full write = the 1st ok call), then the first
    // block write hits ENOSPC.
    let mock = FaultInjectingWriter::new().fail_after(1, ErrorKind::StorageFull);
    let mut writer = AsyncWavWriter::<FaultInjectingWriter>::open(mock, META)
        .await
        .expect("open must succeed (header is the first ok write)");

    let left = (0..128).map(|i| i as f32).collect::<Vec<_>>();
    let right = (0..128).map(|i| -(i as f32)).collect::<Vec<_>>();
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    block.fill_planar(&left, &right);

    let err = match writer.write_block(&block).await {
        Ok(()) => panic!("write_block must propagate the injected ENOSPC"),
        Err(e) => e,
    };
    assert_eq!(
        err.root_cause()
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .kind(),
        ErrorKind::StorageFull
    );

    // A failed block must never be accounted as audio data: offsets stay at
    // the pre-block position (header only) — no partial silent success.
    let header_len = writer.current_offset;
    assert_eq!(writer.data_bytes_written, 0);
    assert_eq!(header_len, build_wav_header(&META, 0).unwrap().len() as u64);
}

// ---------------------------------------------------------------------------
// Atomic create_new + anti-TOCTOU collision resolution
// ---------------------------------------------------------------------------

const FIXED_TS: &str = "20260826_000000";

fn temp_capture_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nam_wav_capture_test_{}_{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("failed to create temp capture dir");
    dir
}

#[tokio::test]
async fn create_new_capture_never_overwrites_existing_file() {
    let dir = temp_capture_dir("nooverwrite");
    let base = dir.join(capture_filename(FIXED_TS, 1, 0));
    let first = dir.join(capture_filename(FIXED_TS, 1, 1));
    std::fs::write(&base, b"SENTINEL-base").expect("pre-create base capture");
    std::fs::write(&first, b"SENTINEL-first").expect("pre-create first suffix");

    let (path, _file) = create_new_capture(&dir, 1, FIXED_TS, |candidate| {
        let mut opts = std::fs::OpenOptions::new();
        let candidate = candidate.to_path_buf();
        async move { opts.write(true).create_new(true).open(candidate) }
    })
    .await
    .expect("resolver must find a free suffix");

    // Both occupied names are skipped; the resolver lands atomically on `-2`.
    assert_eq!(path, dir.join(capture_filename(FIXED_TS, 1, 2)));

    // Pre-existing files are preserved bit-for-bit — never truncated/overwritten.
    assert_eq!(std::fs::read(&base).unwrap(), b"SENTINEL-base");
    assert_eq!(std::fs::read(&first).unwrap(), b"SENTINEL-first");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn create_new_capture_resolves_part_collisions() {
    let dir = temp_capture_dir("parts");
    // Pre-create the sequential part base; the resolver must fall back to -1.
    let base = dir.join(capture_filename(FIXED_TS, 2, 0));
    std::fs::write(&base, b"existing-part2").expect("pre-create part2 base");

    let (path, _file) = create_new_capture(&dir, 2, FIXED_TS, |candidate| {
        let mut opts = std::fs::OpenOptions::new();
        let candidate = candidate.to_path_buf();
        async move { opts.write(true).create_new(true).open(candidate) }
    })
    .await
    .expect("resolver must find a free part suffix");

    assert_eq!(path, dir.join(capture_filename(FIXED_TS, 2, 1)));

    // The pre-created sequential part was not clobbered.
    assert_eq!(std::fs::read(&base).unwrap(), b"existing-part2");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn create_new_capture_concurrent_collisions_yield_distinct_files() {
    let dir = temp_capture_dir("concurrent");
    const THREADS: usize = 12;

    let results: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("failed to build test runtime");
                let (path, _file) = rt
                    .block_on(create_new_capture(&dir, 1, FIXED_TS, |candidate| {
                        let mut opts = std::fs::OpenOptions::new();
                        let candidate = candidate.to_path_buf();
                        async move { opts.write(true).create_new(true).open(candidate) }
                    }))
                    .expect("concurrent create_new_capture must succeed");
                results.lock().unwrap().push(path);
            });
        }
    });

    let mut paths = results.into_inner().unwrap();
    assert_eq!(paths.len(), THREADS);
    paths.sort();

    // Every returned path exists and all are distinct (the kernel's O_EXCL
    // guarantees no two threads can both create the same path).
    let distinct: BTreeSet<_> = paths.iter().collect();
    assert_eq!(
        distinct.len(),
        THREADS,
        "each concurrent thread must create a distinct file"
    );
    for p in &paths {
        assert!(p.is_file(), "returned path must exist: {}", p.display());
    }

    // Exactly one capture_*.wav file per thread — no duplicate/overwritten files.
    let created = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("capture_")
        })
        .count();
    assert_eq!(
        created, THREADS,
        "exactly one atomically-created file per thread"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn create_new_capture_propagates_non_collision_errors() {
    // Point the base dir at a regular file so any `create_new` open must fail
    // with something other than AlreadyExists — and that error must propagate
    // (with context) instead of being masked as a collision.
    let not_a_dir = std::env::temp_dir().join(format!("nam_not_a_dir_{}", std::process::id()));
    std::fs::write(&not_a_dir, b"i am a file, not a directory").expect("write sentinel file");

    let err = create_new_capture(&not_a_dir, 1, FIXED_TS, |candidate| {
        let mut opts = std::fs::OpenOptions::new();
        let candidate = candidate.to_path_buf();
        async move { opts.write(true).create_new(true).open(candidate) }
    })
    .await
    .expect_err("opening under a non-directory parent must fail explicitly");
    let rendered = format!("{err:#}");
    assert!(!rendered.is_empty(), "error must be descriptive");
    assert!(
        err.root_cause().downcast_ref::<std::io::Error>().is_some(),
        "root cause must be the underlying io::Error"
    );

    std::fs::remove_file(&not_a_dir).unwrap();
}

// ---------------------------------------------------------------------------
// Checked RIFF limits: exact 4 GiB boundary and +1 sample
// ---------------------------------------------------------------------------

#[tokio::test]
async fn would_overflow_at_exact_riff_boundary() {
    let mock = FaultInjectingWriter::new();
    let mut writer = AsyncWavWriter::<FaultInjectingWriter>::open(mock, META)
        .await
        .expect("open must succeed");

    let header_overhead = writer.header_len - 8;
    let max_data_payload = u32::MAX as u64 - header_overhead;

    // Filling the RIFF size field to exactly u32::MAX is valid.
    writer.data_bytes_written = max_data_payload as u32;
    assert!(
        !writer.would_overflow(0),
        "0 samples can never overflow the RIFF limit"
    );
    assert!(
        writer.would_overflow(1),
        "1 sample (4 bytes) past the exact u32::MAX limit must overflow"
    );

    // One sample before the boundary: exactly reaching u32::MAX is allowed.
    writer.data_bytes_written = (max_data_payload - 4) as u32;
    assert!(
        !writer.would_overflow(1),
        "exactly reaching the RIFF limit must NOT overflow"
    );
    assert!(
        writer.would_overflow(2),
        "2 samples (8 bytes) past the limit must overflow"
    );

    // Defensive: even a zero-byte addition overflows once already past the cap.
    writer.data_bytes_written = (max_data_payload + 1) as u32;
    assert!(
        writer.would_overflow(0),
        "already past the cap must always report overflow"
    );
}

#[tokio::test]
async fn would_overflow_accounts_for_riff_envelope() {
    let mock = FaultInjectingWriter::new();
    let writer = AsyncWavWriter::<FaultInjectingWriter>::open(mock, META)
        .await
        .expect("open must succeed");

    // The data-payload cap is strictly smaller than u32::MAX because the RIFF
    // envelope (header_len - 8) consumes part of the 32-bit size field.
    let header_overhead = writer.header_len - 8;
    let max_data_payload = u32::MAX as u64 - header_overhead;
    assert!(
        max_data_payload < u32::MAX as u64,
        "the RIFF envelope must reduce the data payload cap below u32::MAX"
    );

    // Cross-check against the real header: `build_wav_header` and the writer's
    // stored `header_len` must agree on the RIFF envelope.
    let header0 = build_wav_header(&META, 0).unwrap();
    assert_eq!(header_overhead, header0.len() as u64 - 8);
    assert_eq!(
        max_data_payload,
        u32::MAX as u64 - (header0.len() as u64 - 8)
    );
}

// ---------------------------------------------------------------------------
// Startup handshake, fail-fast directory validation and RT failure flag
// ---------------------------------------------------------------------------

/// Builds a handshake/status bundle for a test recording worker.
fn test_recording_init(
    base_dir: PathBuf,
) -> (
    RecordingInit,
    tokio::sync::oneshot::Receiver<anyhow::Result<PathBuf>>,
    SharedRecordingStatus,
    Arc<AtomicBool>,
) {
    let status: SharedRecordingStatus = Arc::new(Mutex::new(RecordingStatus::Starting));
    let failed_flag = Arc::new(AtomicBool::new(false));
    let (init_tx, init_rx) = tokio::sync::oneshot::channel();
    let init = RecordingInit::new(status.clone(), init_tx, Arc::clone(&failed_flag), base_dir);
    (init, init_rx, status, failed_flag)
}

#[test]
fn validate_output_dir_accepts_writable_directory() {
    let dir = temp_capture_dir("writable");
    validate_output_dir(&dir).expect("writable temp dir must validate");

    // The probe file must never be left behind.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".nam-io-probe"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "probe file must be removed after validation: {leftovers:?}"
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn validate_output_dir_rejects_missing_directory() {
    let missing = std::env::temp_dir().join(format!("nam_missing_dir_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);

    let err = validate_output_dir(&missing).expect_err("missing dir must fail fast");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("does not exist"),
        "unexpected: {rendered}"
    );
}

#[test]
fn validate_output_dir_rejects_regular_file() {
    let not_a_dir = std::env::temp_dir().join(format!("nam_not_dir_{}", std::process::id()));
    std::fs::write(&not_a_dir, b"i am a file").unwrap();

    let err = validate_output_dir(&not_a_dir).expect_err("regular file must fail fast");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("does not exist"),
        "unexpected: {rendered}"
    );

    std::fs::remove_file(&not_a_dir).unwrap();
}

#[test]
fn spawn_recording_worker_fails_fast_when_io_uring_unavailable() {
    // An unavailable-kernel verdict must fail the handshake BEFORE the
    // tokio_uring runtime is entered — so this test needs no real io_uring.
    let (_sender, receiver) = create_recording_transport();
    let (mut init, init_rx, status, failed_flag) = test_recording_init(std::env::temp_dir());
    init.io_uring_probe = Some(|| IoUringSupport::KernelUnsupported);

    let handle = spawn_recording_worker(receiver, None, init).expect("spawn must succeed");

    let err = wait_for_recording_init(init_rx, std::time::Duration::from_secs(5))
        .expect_err("unavailable io_uring must fail the startup handshake");
    assert!(
        matches!(&err, RecordingStartupError::Failed { reason } if reason.contains("io_uring")),
        "unexpected error: {err:?}"
    );

    match &*status.lock().unwrap() {
        RecordingStatus::Failed { reason } => {
            assert!(reason.contains("io_uring"));
        }
        other => panic!("status must be Failed, got {other:?}"),
    }
    assert!(
        failed_flag.load(Ordering::Acquire),
        "RT failure flag must be raised on startup failure"
    );

    // The worker must exit on its own (no io_uring runtime was entered) and
    // the join must surface the io_uring verdict as an error.
    handle
        .join()
        .expect("worker thread must finish cleanly")
        .expect_err("the io_uring verdict must be returned as an Err on the join");
}

// ---------------------------------------------------------------------------
// Lifecycle decoupling: SHUTDOWN must never truncate the drain
// ---------------------------------------------------------------------------

/// RAII guard that restores the previous `SHUTDOWN` value after the test.
struct ShutdownGuard(bool);

impl ShutdownGuard {
    fn new() -> Self {
        Self(SHUTDOWN.swap(true, Ordering::AcqRel))
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        SHUTDOWN.store(self.0, Ordering::Release);
    }
}

/// In-memory "disk" shared between the mock writer and the test, so the test
/// can assert the finalized WAV bytes bit-for-bit after the loop exits.
#[derive(Clone, Default)]
struct SharedDisk {
    bytes: Arc<Mutex<Vec<u8>>>,
    sync_calls: Arc<AtomicUsize>,
}

/// Positioned-write backend writing into a [`SharedDisk`] — no io_uring.
struct SharedDiskWriter {
    disk: SharedDisk,
}

impl WriteAt for SharedDiskWriter {
    async fn write_at(&mut self, buf: Vec<u8>, offset: u64) -> (io::Result<usize>, Vec<u8>) {
        let mut disk = self.disk.bytes.lock().unwrap();
        let start = offset as usize;
        let end = start + buf.len();
        if disk.len() < end {
            disk.resize(end, 0);
        }
        disk[start..end].copy_from_slice(&buf);
        (Ok(buf.len()), buf)
    }

    async fn sync_all(&mut self) -> io::Result<()> {
        self.disk.sync_calls.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Test [`WavSink`] producing mock writers over shared in-memory disks.
#[derive(Default)]
struct MockWavSink {
    disks: Arc<Mutex<Vec<SharedDisk>>>,
}

impl WavSink for MockWavSink {
    type Writer = AsyncWavWriter<SharedDiskWriter>;

    async fn create(&self, part: u32, metadata: AudioMetadata) -> Result<(Self::Writer, PathBuf)> {
        let disk = SharedDisk::default();
        let writer = AsyncWavWriter::<SharedDiskWriter>::open(
            SharedDiskWriter { disk: disk.clone() },
            metadata,
        )
        .await?;
        self.disks.lock().unwrap().push(disk);
        let path = std::env::temp_dir().join(format!("mock_capture_part{part}.wav"));
        Ok((writer, path))
    }
}

/// Acceptance — deterministic barrier test.
///
/// Scenario: SIGINT (`SHUTDOWN`) fires while the ring is momentarily empty;
/// the test parks the worker with empty channels (barrier), then emits blocks
/// **after** `SHUTDOWN` through the promoted pool transport — the worker must
/// ignore the flag, drain 100% of the samples and finalize a bit-exact WAV
/// with exactly one `fsync`.
// REASON (T-C1): serializing SHUTDOWN-touching tests requires holding the
// std MutexGuard across `.await`; this is Send-safe because the runtime is
// `current_thread` (LocalSet), so the guard never crosses threads.
#[expect(
    clippy::await_holding_lock,
    reason = "T-C1 SHUTDOWN test serialization"
)]
#[tokio::test(flavor = "current_thread")]
async fn shutdown_with_empty_ring_never_truncates_subsequent_blocks() {
    let _shutdown_lock = crate::standalone::SHUTDOWN_TEST_LOCK
        .lock()
        .expect("shutdown test lock");
    // `InFlightBlock` owns a raw pointer into the pool slot and crosses an
    // `.await` inside the drain loop, so the worker future is `!Send`; it must
    // run inside a `LocalSet` (the production worker runs under the
    // single-threaded `tokio_uring` runtime, which has the same shape).
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (mut sender, mut receiver) = create_recording_transport();
            let status: SharedRecordingStatus = Arc::new(Mutex::new(RecordingStatus::Starting));
            let status_check = Arc::clone(&status);
            let sink = MockWavSink::default();
            let disks_handle = Arc::clone(&sink.disks);
            let worker = tokio::task::spawn_local(async move {
                disk_writer_loop_inner(&sink, &mut receiver, None, &status).await
            });

            // Barrier: push Metadata, then wait until the worker consumed it
            // and both channels are empty again (`control` ring back to full
            // capacity, every pool slot back in the free ring) — the worker is
            // now parked in its idle poll with empty channels.
            assert!(sender.try_push_metadata(META), "metadata push must succeed");

            {
                // Scope the producer borrows so they are released before the
                // pushes below re-borrow `sender`.
                let (control_prod, pool_prod) = match &mut sender {
                    crate::recording::transport::RecordingSender::Pool { control, pool } => {
                        (control.as_mut().unwrap(), pool.as_mut().unwrap())
                    }
                    crate::recording::transport::RecordingSender::Inline(_) => {
                        panic!("pool transport expected")
                    }
                };
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while control_prod.slots() != crate::recording::buffer::CONTROL_CAPACITY
                    || pool_prod.free_available() != POOL_CAPACITY
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "worker never drained the metadata barrier"
                    );
                    tokio::task::yield_now().await;
                }
            }

            // Scenario: SHUTDOWN fires with empty channels. Keep the worker
            // idle for several 10 ms poll cycles, so any shutdown-on-empty
            // race condition would have finalized and quit before the blocks
            // below land.
            let _shutdown_guard = ShutdownGuard::new();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            // The RT callback still emits blocks after SHUTDOWN (up to one
            // main-loop iteration before `thread_loop.stop()`).
            const BLOCK_SAMPLES: usize = 256;
            let mut expected_payload = Vec::new();
            for block_idx in 0..8u32 {
                let left: Vec<f32> = (0..BLOCK_SAMPLES)
                    .map(|i| (block_idx * BLOCK_SAMPLES as u32 + i as u32) as f32 * 0.001)
                    .collect();
                let right: Vec<f32> = left.iter().map(|v| -v).collect();
                expected_payload.extend_from_slice(&interleave(&left, &right));

                assert!(
                    sender.try_push_audio(&left, &right),
                    "post-shutdown audio publish must succeed"
                );
            }

            // The main thread confirms the RT loop stopped
            // (`thread_loop.stop()`) and only then sends the terminal
            // StreamStop token.
            assert!(
                sender.try_push_stream_stop(),
                "StreamStop push must succeed"
            );

            // The worker must drain 100% of the blocks and finalize before
            // exiting.
            tokio::time::timeout(std::time::Duration::from_secs(5), worker)
                .await
                .expect("worker must exit after consuming StreamStop")
                .expect("worker task must not panic")
                .expect("integral drain must succeed");

            // The observable status tracked the live capture file created on
            // Metadata.
            match &*status_check.lock().unwrap() {
                RecordingStatus::Active { path } => {
                    assert_eq!(path, &std::env::temp_dir().join("mock_capture_part1.wav"));
                }
                other => panic!("status must be Active after Metadata, got {other:?}"),
            }

            // 100% of the samples must be in the finalized WAV: bit-exact
            // header + interleaved PCM, with exactly one fsync from `finalize`.
            let disks = disks_handle.lock().unwrap();
            assert_eq!(disks.len(), 1, "a single capture file must be produced");
            let mut expected_wav = build_wav_header(&META, expected_payload.len() as u32)
                .expect("expected header build");
            expected_wav.extend_from_slice(&expected_payload);
            assert_eq!(
                &*disks[0].bytes.lock().unwrap(),
                &expected_wav[..],
                "post-SHUTDOWN blocks must be fully drained into the finalized WAV"
            );
            assert_eq!(
                disks[0].sync_calls.load(Ordering::Relaxed),
                1,
                "finalize must fsync exactly once"
            );
        })
        .await;
}
