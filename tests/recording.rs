// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration tests for the recording subsystem.
//!
//! Validates end-to-end recording via the promoted pool transport (audio pool
//! + control ring) → `disk_writer_loop` → valid WAV file on disk.
//!
//! Requires `io_uring` support (Linux >= 5.1).

use nam_audio_pipe::recording::buffer::{AudioMetadata, OVERRUN_COUNT};
use nam_audio_pipe::recording::{
    RecordingStartupError, RecordingStatus, RecordingWorkerGuard, RecordingWorkerOutcome,
    create_recording_transport, spawn_recording_worker, wait_for_recording_init,
};
use nam_audio_pipe::standalone::pw_host::{self, PipewireHostConfig};
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{self, GcOverflowBuffer, RtStatusFlags, SHUTDOWN};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use rtrb::RingBuffer;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

mod common;

use common::{
    DirGuard, ShutdownGuard, TEST_MUTEX, recording_init_for, spawn_ready_worker, temp_dir,
};

/// RAII guard that changes CWD for the duration of a test.
struct CwdGuard(PathBuf);

impl CwdGuard {
    fn enter(dir: &PathBuf) -> Self {
        let prev = std::env::current_dir().expect("failed to read current dir");
        std::env::set_current_dir(dir).expect("failed to chdir to temp dir");
        Self(prev)
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

fn wav_sample_count(path: &std::path::Path) -> u32 {
    let reader = hound::WavReader::open(path).expect("failed to open WAV for reading");
    let spec = reader.spec();
    assert_eq!(spec.channels, 2);
    assert_eq!(spec.sample_rate, 48000);
    assert_eq!(spec.bits_per_sample, 32);
    assert_eq!(spec.sample_format, hound::SampleFormat::Float);
    reader.duration()
}

#[test]
#[ignore = "requires io_uring support"]
fn disk_writer_loop_creates_valid_wav() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _cwd = CwdGuard::enter(&dir);
    let _guard = DirGuard::new(dir.clone());

    let (mut sender, receiver) = create_recording_transport();

    // The worker must complete the startup handshake (io_uring + writable dir)
    // before we push any payload — F-RB-009 / T3.3.
    let (handle, _, _) = spawn_ready_worker(receiver, &dir);

    let meta = AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    };
    assert!(
        sender.try_push_metadata(meta),
        "metadata push should succeed"
    );

    const BLOCK_SAMPLES: usize = 480;
    for block_idx in 0..10u32 {
        let mut left = [0.0f32; BLOCK_SAMPLES];
        let mut right = [0.0f32; BLOCK_SAMPLES];
        for i in 0..BLOCK_SAMPLES {
            let v = (block_idx * BLOCK_SAMPLES as u32 + i as u32) as f32 * 0.001;
            left[i] = v;
            right[i] = -v;
        }
        assert!(
            sender.try_push_audio(&left, &right),
            "audio push should succeed"
        );
    }

    assert!(
        sender.try_push_stream_stop(),
        "StreamStop push should succeed"
    );

    // StreamStop is now the sole termination token (F-RB-009 / T3.4): the
    // worker finalizes the WAV and exits on it. SHUTDOWN is set afterwards on
    // purpose as a regression guard — the worker must ignore the global flag
    // (a SIGINT during a momentary empty ring must never truncate the tail).
    SHUTDOWN.store(true, Ordering::SeqCst);

    handle
        .join()
        .expect("I/O thread should complete")
        .expect("integral drain must finalize the WAV successfully");

    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(&dir).expect("failed to read temp dir") {
        let e = entry.expect("dir entry error");
        let p = e.path();
        if p.extension().is_some_and(|ext| ext == "wav") {
            found = Some(p);
            break;
        }
    }

    let wav_path = found.expect("no WAV file created by disk_writer_loop");
    let sample_count = wav_sample_count(&wav_path);
    assert_eq!(
        sample_count, 4800,
        "expected 4800 samples per channel (10 × 480), got {sample_count}"
    );

    let overruns = OVERRUN_COUNT.load(Ordering::Relaxed);
    assert_eq!(overruns, 0, "unexpected ring buffer overruns: {overruns}");
}

#[test]
#[ignore = "requires io_uring support"]
fn disk_writer_loop_metadata_then_stream_stop_creates_empty_wav() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _cwd = CwdGuard::enter(&dir);
    let _guard = DirGuard::new(dir.clone());

    let (mut sender, receiver) = create_recording_transport();

    // The worker must complete the startup handshake (io_uring + writable dir)
    // before we push any payload — F-RB-009 / T3.3.
    let (handle, _, _) = spawn_ready_worker(receiver, &dir);

    let meta = AudioMetadata {
        sample_rate: 44100.0,
        bit_depth: 32,
        channels: 2,
    };
    assert!(
        sender.try_push_metadata(meta),
        "metadata push should succeed"
    );

    assert!(
        sender.try_push_stream_stop(),
        "StreamStop push should succeed"
    );

    // T3.4: StreamStop alone must terminate the worker; SHUTDOWN is set as a
    // regression guard proving the global flag is no longer consulted.
    SHUTDOWN.store(true, Ordering::SeqCst);

    handle
        .join()
        .expect("I/O thread should complete")
        .expect("clean StreamStop shutdown must succeed");

    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(&dir).expect("failed to read temp dir") {
        let e = entry.expect("dir entry error");
        let p = e.path();
        if p.extension().is_some_and(|ext| ext == "wav") {
            found = Some(p);
            break;
        }
    }

    let wav_path = found.expect("no WAV file created");
    let reader = hound::WavReader::open(&wav_path).expect("failed to open WAV");
    assert_eq!(reader.spec().sample_rate, 44100);
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.duration(), 0);
}

#[test]
#[ignore = "requires io_uring support"]
fn disk_writer_loop_discards_audio_before_metadata() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _cwd = CwdGuard::enter(&dir);
    let _guard = DirGuard::new(dir.clone());

    let (mut sender, receiver) = create_recording_transport();

    // The worker must complete the startup handshake (io_uring + writable dir)
    // before we push any payload — F-RB-009 / T3.3.
    let (handle, _, _) = spawn_ready_worker(receiver, &dir);

    // Push Audio BEFORE Metadata — should be discarded silently
    assert!(
        sender.try_push_audio(&[1.0f32; 64], &[-1.0f32; 64]),
        "audio push should succeed"
    );

    let meta = AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    };
    assert!(
        sender.try_push_metadata(meta),
        "metadata push should succeed"
    );

    assert!(
        sender.try_push_audio(&[0.5f32, 0.6], &[-0.5f32, -0.6]),
        "audio push should succeed"
    );

    assert!(
        sender.try_push_stream_stop(),
        "StreamStop push should succeed"
    );

    // T3.4: StreamStop alone must terminate the worker; SHUTDOWN is set as a
    // regression guard proving the global flag is no longer consulted.
    SHUTDOWN.store(true, Ordering::SeqCst);

    handle
        .join()
        .expect("I/O thread should complete")
        .expect("clean StreamStop shutdown must succeed");

    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(&dir).expect("failed to read temp dir") {
        let e = entry.expect("dir entry error");
        let p = e.path();
        if p.extension().is_some_and(|ext| ext == "wav") {
            found = Some(p);
            break;
        }
    }

    let wav_path = found.expect("no WAV file created");
    // Only 4 floats: 2 samples × 2 channels
    assert_eq!(wav_sample_count(&wav_path), 2);
}

/// R-13 — full `--record` lifecycle, end-to-end:
///
/// 1. spawns `run_pipewire_host` with a REAL recording producer and the real
///    disk I/O thread (`disk_writer_loop`), inside a clean temp CWD;
/// 2. waits for ≥1 processed audio quantum (`last_n_samples > 0` — the RT
///    callback pushes `Metadata` + `Audio` into the recording ring);
/// 3. signals stop (`SHUTDOWN`) → host runs `thread_loop.stop()` →
///    `push_stream_stop` (retry 200 ms) → bounded join (5 s);
/// 4. asserts the finalized WAV `data` chunk size equals the PCM bytes
///    actually written to the file (coherent header, never `data=0` after a
///    clean stop).
///
/// The 5 s join detach in `run_pipewire_host` is a LAST RESORT: if the I/O
/// thread stalls beyond it, the header may be left with `data=0`. That case
/// FAILS this test — a clean stop must never produce a silent/incomplete WAV.
///
/// Requires a running PipeWire daemon AND io_uring. If the daemon is absent
/// the test prints an honest `TEST_RESULT[record_e2e]=SKIP:daemon_unavailable`
/// marker (Phase 4 of `utils/tests-quick.sh` matches that typed marker and
/// never emits `RECORDING_IO_URING=RAN` for a skip; on success it emits
/// `TEST_RESULT[record_e2e]=PASS`).
#[test]
#[ignore = "E2E recording: requires running PipeWire daemon + io_uring; auto-detected by utils/tests-quick.sh Phase 4"]
fn record_e2e_pipewire_wav_header_matches_bytes() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _cwd = CwdGuard::enter(&dir);
    let _guard = DirGuard::new(dir.clone());

    if !common::probe_pipewire_daemon() {
        eprintln!("TEST_RESULT[record_e2e]=SKIP:daemon_unavailable");
        eprintln!("SKIP: PipeWire daemon not detected (pw-cli info 0 failed).");
        return;
    }

    pipewire::init();

    let (_param_prod, param_cons) = RingBuffer::new(4);
    let (gc_prod, gc_cons) = RingBuffer::new(4);
    let (res_prod, res_cons) = RingBuffer::new(2);
    let (cs_prod, cs_cons) = RingBuffer::new(2);
    let (sl_prod, sl_cons) = RingBuffer::new(2);
    let (os_prod, os_cons) = RingBuffer::new(2);

    let gc_overflow = Arc::new(GcOverflowBuffer::new(64));
    let rt_status = Arc::new(RtStatusFlags::default());

    let (recording_sender, recording_receiver) = create_recording_transport();
    let (init, init_rx, _status, failed_flag) = recording_init_for(&dir);
    let io_handle = spawn_recording_worker(recording_receiver, None, init)
        .expect("failed to spawn recording I/O thread");
    wait_for_recording_init(init_rx, Duration::from_secs(5))
        .expect("recording worker must confirm readiness");

    let rt_clone = rt_status.clone();
    let gc_overflow_clone = gc_overflow.clone();
    let sys = SystemSnapshot::capture();

    // F-RB-009 / T3.5: the worker thread, its transport sender and the failure
    // flag travel together in the RAII guard, so the host's early `?` returns
    // and the normal shutdown both terminate and formally join the worker.
    let recording_worker =
        RecordingWorkerGuard::new(io_handle, Some(recording_sender), Some(failed_flag));

    let pw_thread = thread::spawn(move || {
        pw_host::run_pipewire_host(
            param_cons,
            gc_prod,
            gc_overflow_clone,
            res_cons,
            res_prod,
            cs_cons,
            cs_prod,
            rt_clone,
            PipewireHostConfig {
                buffer_size: 0,
                sys,
                ir_raw_samples: None,
                ir_source_rate: 0,
                full_wavenet_model_l: None,
                full_wavenet_model_r: None,
                has_model_r: false,
                slimmable_producer: sl_prod,
                os_producer: os_prod,
                oversample: OversampleFactor::Off,
                requested_cpu: None,
                // Fail-fast under the deterministic harness (see pw_integration).
                fail_fast: true,
            },
            gc_cons,
            sl_cons,
            os_cons,
            None,
            Some(recording_worker),
        )
    });

    // Wait (bounded) for the RT callback to process ≥1 quantum. That proves
    // the capture stream ran AND the recording ring received Metadata+Audio.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut last_n_samples = 0u32;
    while std::time::Instant::now() < deadline {
        last_n_samples = rt_status.last_n_samples.load(Ordering::Relaxed);
        if last_n_samples > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    spsc::SHUTDOWN.store(true, Ordering::Relaxed);

    let host_result = pw_thread
        .join()
        .expect("the PipeWire host thread suffered a fatal panic");
    let recording_outcome = match host_result {
        Ok(outcome) => outcome,
        Err(e) => panic!("run_pipewire_host failed while the PipeWire daemon is up: {e:#}"),
    };
    assert!(
        matches!(recording_outcome, Some(RecordingWorkerOutcome::Success)),
        "a clean stop must yield a successful recording outcome, got {recording_outcome:?}"
    );

    assert!(
        last_n_samples > 0,
        "no audio quantum was processed (last_n_samples == 0) — recording ring never received data"
    );

    // Locate the WAV written by disk_writer_loop in the temp CWD.
    let mut found: Option<PathBuf> = None;
    for entry in std::fs::read_dir(&dir).expect("failed to read temp dir") {
        let e = entry.expect("dir entry error");
        let p = e.path();
        if p.extension().is_some_and(|ext| ext == "wav") {
            found = Some(p);
            break;
        }
    }
    let wav_path = found.expect("no WAV file created by the E2E recording session");

    // The `data` chunk size field (header) must equal the PCM bytes actually
    // present in the file — the R-13 coherence invariant. A clean stop must
    // never leave `data=0` (incomplete header from the detached-join path).
    let file_bytes = std::fs::read(&wav_path).expect("failed to read recorded WAV");
    let data_pos = file_bytes
        .array_windows::<4>()
        .rposition(|w| w == b"data")
        .expect("'data' chunk not found in recorded WAV");
    let data_size = u32::from_le_bytes(file_bytes[data_pos + 4..data_pos + 8].try_into().unwrap());
    let payload_bytes = file_bytes.len() as u64 - (data_pos as u64 + 8);

    let sample_count = wav_sample_count(&wav_path);
    let expected_payload = sample_count as u64 * 2 * 4; // stereo × 32-bit float
    assert_eq!(
        data_size as u64, payload_bytes,
        "R-13: WAV 'data' header size ({data_size}) != PCM bytes actually written ({payload_bytes})"
    );
    assert_eq!(
        expected_payload, payload_bytes,
        "R-13: hound duration disagrees with actual file payload ({} vs {payload_bytes})",
        expected_payload
    );
    assert!(
        data_size > 0,
        "R-13: WAV 'data' size is 0 after a clean stop — header rewrite never completed"
    );
    eprintln!("TEST_RESULT[record_e2e]=PASS");
}

// ---------------------------------------------------------------------------
// F-RB-009 / T3.3 — fail-fast startup handshake under unusable output dirs
// ---------------------------------------------------------------------------

/// The worker must reject a missing output directory at startup: the handshake
/// reports `Failed`, the observable status transitions to `Failed`, the RT
/// failure flag is raised and the thread exits on its own — no silent success.
#[test]
#[ignore = "requires io_uring support"]
fn disk_writer_loop_fails_fast_on_missing_output_dir() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let missing =
        std::env::temp_dir().join(format!("nam-recording-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);

    let (_sender, receiver) = create_recording_transport();
    let (init, init_rx, status, failed_flag) = recording_init_for(&missing);
    let handle = spawn_recording_worker(receiver, None, init).expect("spawn recording worker");

    let err = wait_for_recording_init(init_rx, Duration::from_secs(5))
        .expect_err("a missing output dir must fail the startup handshake");
    assert!(
        matches!(&err, RecordingStartupError::Failed { reason } if reason.contains("does not exist")),
        "unexpected error: {err:?}"
    );

    match &*status.lock().unwrap() {
        RecordingStatus::Failed { reason } => assert!(reason.contains("does not exist")),
        other => panic!("status must be Failed, got {other:?}"),
    }
    assert!(
        failed_flag.load(Ordering::Acquire),
        "RT failure flag must be raised on a startup failure"
    );

    // The worker must exit on its own and must never create anything. The join
    // also surfaces the startup error (F-RB-009 / T3.5).
    handle
        .join()
        .expect("worker must finish cleanly")
        .expect_err("a missing output dir must surface as an Err on the join");
    assert!(
        !missing.exists(),
        "no artifact may be created for a failed recording"
    );
}

/// The worker must reject a regular file used as the output directory
/// (`ENOTDIR` equivalent) at startup, through the same handshake.
#[test]
#[ignore = "requires io_uring support"]
fn disk_writer_loop_fails_fast_on_file_as_output_dir() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let file_dir = std::env::temp_dir().join(format!("nam-recording-file-{}", std::process::id()));
    std::fs::write(&file_dir, b"i am a file, not a directory").unwrap();

    let (_sender, receiver) = create_recording_transport();
    let (init, init_rx, status, failed_flag) = recording_init_for(&file_dir);
    let handle = spawn_recording_worker(receiver, None, init).expect("spawn recording worker");

    let err = wait_for_recording_init(init_rx, Duration::from_secs(5))
        .expect_err("a file-as-dir must fail the startup handshake");
    assert!(
        matches!(&err, RecordingStartupError::Failed { reason } if reason.contains("does not exist")),
        "unexpected error: {err:?}"
    );

    match &*status.lock().unwrap() {
        RecordingStatus::Failed { .. } => {}
        other => panic!("status must be Failed, got {other:?}"),
    }
    assert!(failed_flag.load(Ordering::Acquire));

    handle
        .join()
        .expect("worker must finish cleanly")
        .expect_err("a file-as-dir must surface as an Err on the join");
    std::fs::remove_file(&file_dir).unwrap();
}
