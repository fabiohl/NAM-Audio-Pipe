// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Integration tests for the recording subsystem.
//!
//! Validates end-to-end recording via the SPSC ring buffer → `disk_writer_loop`
//! → valid WAV file on disk. Requires `io_uring` support (Linux >= 5.1).

use nam_audio_pipe::recording::buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, OVERRUN_COUNT, RING_CAPACITY, RingPayload,
    create_audio_ring_buffer,
};
use nam_audio_pipe::standalone::pw_host::{self, PipewireHostConfig};
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{self, GcOverflowBuffer, RtStatusFlags, SHUTDOWN};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use rtrb::RingBuffer;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

mod common;

static TEST_MUTEX: Mutex<()> = Mutex::new(());

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nam-recording-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
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

/// RAII guard that resets SHUTDOWN after each test.
struct ShutdownGuard;

impl ShutdownGuard {
    fn new() -> Self {
        SHUTDOWN.store(false, Ordering::SeqCst);
        Self
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        SHUTDOWN.store(false, Ordering::SeqCst);
    }
}

/// RAII guard that cleans up the temp directory on drop.
struct DirGuard(PathBuf);

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

#[test]
#[ignore = "requires io_uring support"]
fn disk_writer_loop_creates_valid_wav() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _cwd = CwdGuard::enter(&dir);
    let _guard = DirGuard(dir.clone());

    let (mut producer, consumer) = create_audio_ring_buffer::<{ MAX_BLOCK_SIZE }>(RING_CAPACITY);

    let handle = std::thread::Builder::new()
        .name("nam-test-recording-io".into())
        .spawn(move || {
            tokio_uring::start(async {
                nam_audio_pipe::recording::disk_writer_loop(consumer, None)
                    .await
                    .expect("disk_writer_loop should succeed");
            });
        })
        .expect("failed to spawn test I/O thread");

    let meta = AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    };
    producer
        .push(RingPayload::Metadata(meta))
        .expect("metadata push should succeed");

    const BLOCK_SAMPLES: usize = 480;
    for block_idx in 0..10u32 {
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        let mut left = [0.0f32; BLOCK_SAMPLES];
        let mut right = [0.0f32; BLOCK_SAMPLES];
        for i in 0..BLOCK_SAMPLES {
            let v = (block_idx * BLOCK_SAMPLES as u32 + i as u32) as f32 * 0.001;
            left[i] = v;
            right[i] = -v;
        }
        block.fill_planar(&left, &right);
        producer
            .push(RingPayload::Audio(block))
            .expect("audio push should succeed");
    }

    producer
        .push(RingPayload::StreamStop)
        .expect("StreamStop push should succeed");

    // Signal shutdown so disk_writer_loop exits after draining.
    // StreamStop finalizes the WAV; SHUTDOWN triggers the loop to exit.
    SHUTDOWN.store(true, Ordering::SeqCst);

    handle.join().expect("I/O thread should complete");

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
    let _guard = DirGuard(dir.clone());

    let (mut producer, consumer) = create_audio_ring_buffer::<{ MAX_BLOCK_SIZE }>(RING_CAPACITY);

    let handle = std::thread::Builder::new()
        .name("nam-test-recording-io".into())
        .spawn(move || {
            tokio_uring::start(async {
                nam_audio_pipe::recording::disk_writer_loop(consumer, None)
                    .await
                    .expect("disk_writer_loop should succeed");
            });
        })
        .expect("failed to spawn test I/O thread");

    let meta = AudioMetadata {
        sample_rate: 44100.0,
        bit_depth: 32,
        channels: 2,
    };
    producer
        .push(RingPayload::Metadata(meta))
        .expect("metadata push should succeed");

    producer
        .push(RingPayload::StreamStop)
        .expect("StreamStop push should succeed");

    SHUTDOWN.store(true, Ordering::SeqCst);

    handle.join().expect("I/O thread should complete");

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
    let _guard = DirGuard(dir.clone());

    let (mut producer, consumer) = create_audio_ring_buffer::<{ MAX_BLOCK_SIZE }>(RING_CAPACITY);

    let handle = std::thread::Builder::new()
        .name("nam-test-recording-io".into())
        .spawn(move || {
            tokio_uring::start(async {
                nam_audio_pipe::recording::disk_writer_loop(consumer, None)
                    .await
                    .expect("disk_writer_loop should succeed");
            });
        })
        .expect("failed to spawn test I/O thread");

    // Push Audio BEFORE Metadata — should be discarded silently
    let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    block.fill_planar(&[1.0f32; 64], &[-1.0f32; 64]);
    producer
        .push(RingPayload::Audio(block))
        .expect("audio push should succeed");

    let meta = AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    };
    producer
        .push(RingPayload::Metadata(meta))
        .expect("metadata push should succeed");

    let mut block2 = AlignedBlock::<MAX_BLOCK_SIZE>::new();
    block2.fill_planar(&[0.5, 0.6], &[-0.5, -0.6]);
    producer
        .push(RingPayload::Audio(block2))
        .expect("audio push should succeed");

    producer
        .push(RingPayload::StreamStop)
        .expect("StreamStop push should succeed");

    SHUTDOWN.store(true, Ordering::SeqCst);

    handle.join().expect("I/O thread should complete");

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
/// the test prints an honest `SKIP:` (Phase 4 of `utils/tests-quick.sh`
/// recognizes it and never emits `RECORDING_IO_URING=RAN` for a skip).
#[test]
#[ignore = "E2E recording: requires running PipeWire daemon + io_uring; auto-detected by utils/tests-quick.sh Phase 4"]
fn record_e2e_pipewire_wav_header_matches_bytes() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _cwd = CwdGuard::enter(&dir);
    let _guard = DirGuard(dir.clone());

    if !common::probe_pipewire_daemon() {
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

    let (recording_producer, recording_consumer) =
        create_audio_ring_buffer::<{ MAX_BLOCK_SIZE }>(RING_CAPACITY);
    let io_handle = std::thread::Builder::new()
        .name("nam-e2e-recording-io".into())
        .spawn(move || {
            tokio_uring::start(async {
                nam_audio_pipe::recording::disk_writer_loop(recording_consumer, None)
                    .await
                    .expect("disk_writer_loop should succeed");
            });
        })
        .expect("failed to spawn recording I/O thread");

    let rt_clone = rt_status.clone();
    let gc_overflow_clone = gc_overflow.clone();
    let sys = SystemSnapshot::capture();

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
                full_wavenet_model: None,
                slimmable_producer: sl_prod,
                os_producer: os_prod,
                oversample: OversampleFactor::Off,
            },
            gc_cons,
            sl_cons,
            os_cons,
            Some(recording_producer),
            None,
            Some(io_handle),
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
    if let Err(e) = host_result {
        panic!("run_pipewire_host failed while the PipeWire daemon is up: {e:#}");
    }

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
}
