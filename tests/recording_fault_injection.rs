// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! ER-3 fault-injection, byte-by-byte validation and resource-leak harness
//! for the recording subsystem (T3.6).
//!
//! Complements `tests/recording.rs` (functional lifecycle) with the
//! certification battery required by the ER-3 gates:
//!
//! * **Independent RIFF validator** — a hand-rolled chunk walker (no `hound`)
//!   that structurally validates the `RIFF`/`WAVE` envelope, `fmt `, `fact`
//!   and `data` chunks of every WAV produced by the real `io_uring` writer.
//! * **Byte-by-byte fidelity** — sine, deterministic noise and ramp signals
//!   injected through the SPSC ring are compared sample-by-sample (bit-exact,
//!   NaN-safe) against the PCM persisted on disk, including a mid-stream
//!   metadata change that must split the capture into sequential `_partN`
//!   files.
//! * **Fault injection mid-stream** — an `ENOSPC`-class fault (`RLIMIT_FSIZE`
//!   → `EFBIG` with `SIGXFSZ` ignored) fired in the middle of the audio flow
//!   must transition the observable status to `Failed`, raise the RT failure
//!   flag, surface the error on the join and preserve the partial file exactly
//!   up to the fault point.
//! * **Concurrent signals** — a simulated `SIGINT` (`SHUTDOWN`) arriving under
//!   high transfer rate must never truncate the tail, and a simulated `SIGTERM`
//!   (recording producer dropped without `StreamStop`) must drain and finalize
//!   every pending block through the "abandoned + drained" terminal condition.
//! * **Anti-TOCTOU concurrency** — 20 recording instances writing in the same
//!   directory at the same instant must produce 20 distinct, non-clobbered
//!   captures (atomic `create_new(true)` + suffix resolution).
//! * **FD/thread leak sweep** — 100 consecutive record/stop cycles must return
//!   the process to its baseline `/proc/self/fd` and thread counts.
//!
//! Tests that require `io_uring` are `#[ignore]`d (they run exclusively in the
//! io_uring-gated Phase 4 of `utils/tests-quick.sh`, or via
//! `cargo test --test recording_fault_injection -- --ignored`). The
//! non-ignored tests validate the RIFF parser itself against the pure header
//! builder, so the harness always contributes to `cargo test --all-targets`.

use nam_audio_pipe::recording::buffer::{
    AlignedBlock, AudioMetadata, MAX_BLOCK_SIZE, RING_CAPACITY, RingPayload,
    create_audio_ring_buffer,
};
use nam_audio_pipe::recording::wav_header::build_wav_header;
use nam_audio_pipe::recording::{
    RecordingStatus, RecordingWorkerGuard, RecordingWorkerOutcome, spawn_recording_worker,
    wait_for_recording_init,
};
use neural_amp_modeler_rs::common::spsc::SHUTDOWN;
use rtrb::Producer;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

mod common;

use common::{
    DirGuard, ShutdownGuard, TEST_MUTEX, recording_init_for, spawn_ready_worker, temp_dir,
};

const META: AudioMetadata = AudioMetadata {
    sample_rate: 48000.0,
    bit_depth: 32,
    channels: 2,
};

/// RAII guard restoring `RLIMIT_FSIZE` to the value captured before a test
/// lowered it (the EFBIG fault-injection test).
///
/// Only `rlim_cur` is ever lowered (raising a *hard* `rlim_max` back requires
/// `CAP_SYS_RESOURCE`, which a test process must not depend on), so the restore
/// in `Drop` always succeeds: `prev.rlim_cur <= prev.rlim_max` by construction.
struct RlimitGuard(libc::rlimit);

impl Drop for RlimitGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` holds the original `rlimit` returned by `getrlimit`
        // (its `rlim_cur` was only ever *lowered*, never the `rlim_max`), so
        // restoring it is always permitted and can only fail on a kernel that
        // rejects even the original values — impossible.
        unsafe {
            libc::setrlimit(libc::RLIMIT_FSIZE, &self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Independent RIFF/WAV structural parser (no `hound`)
// ---------------------------------------------------------------------------

/// Structural view of a WAV file produced by [`parse_riff_wav`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct RiffWav {
    /// Format tag from the `fmt ` chunk (3 = IEEE float).
    format_tag: u16,
    channels: u16,
    sample_rate: u32,
    byte_rate: u32,
    block_align: u16,
    bits_per_sample: u16,
    /// `fact` chunk sample count (required for float formats).
    fact_samples: Option<u32>,
    /// `data` chunk size in bytes.
    data_size: u32,
    /// Absolute offset of the `data` payload inside the file.
    data_offset: usize,
}

/// Validates the RIFF/WAVE envelope and walks every chunk independently of
/// `hound`, enforcing the structural invariants the writer must uphold:
///
/// * `RIFF` magic + size field consistent with the real file length;
/// * well-formed `fmt ` (format tag, channels, sample rate, byte rate,
///   block align, bits per sample) with internally consistent derived fields;
/// * a `fact` chunk — when present — carrying the per-channel sample count;
/// * a `data` chunk whose declared size matches the file tail exactly.
fn parse_riff_wav(bytes: &[u8]) -> Result<RiffWav, String> {
    if bytes.len() < 12 {
        return Err(format!(
            "file too short to be a WAV header: {} bytes",
            bytes.len()
        ));
    }
    if &bytes[0..4] != b"RIFF" {
        return Err("missing RIFF magic".into());
    }
    if &bytes[8..12] != b"WAVE" {
        return Err("missing WAVE form tag".into());
    }
    let riff_size = u32le(&bytes[4..8]);
    let expected_riff = (bytes.len() - 8) as u32;
    if riff_size != expected_riff {
        return Err(format!(
            "RIFF chunk size {riff_size} disagrees with file length minus 8 ({expected_riff})"
        ));
    }

    let mut format_tag = None;
    let mut channels = None;
    let mut sample_rate = None;
    let mut byte_rate = None;
    let mut block_align = None;
    let mut bits_per_sample = None;
    let mut fact_samples = None;
    let mut data: Option<(u32, usize)> = None;

    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32le(&bytes[pos + 4..pos + 8]) as usize;
        let body = pos + 8;
        let end = body.checked_add(size).ok_or("chunk size overflow")?;
        if end > bytes.len() {
            return Err(format!(
                "chunk {:?} is truncated: declares {size} bytes but only {} remain",
                String::from_utf8_lossy(id),
                bytes.len() - body
            ));
        }
        match id {
            b"fmt " => {
                if size < 16 {
                    return Err("fmt chunk is smaller than the mandatory 16 bytes".into());
                }
                let mut tag = u16le(&bytes[body..body + 2]);
                channels = Some(u16le(&bytes[body + 2..body + 4]));
                sample_rate = Some(u32le(&bytes[body + 4..body + 8]));
                byte_rate = Some(u32le(&bytes[body + 8..body + 12]));
                block_align = Some(u16le(&bytes[body + 12..body + 14]));
                bits_per_sample = Some(u16le(&bytes[body + 14..body + 16]));
                // WAVE_FORMAT_EXTENSIBLE (0xFFFE) carries the real format tag in
                // the SubFormat GUID at the tail of the 40-byte fmt chunk.
                if tag == 0xFFFE {
                    if size < 40 {
                        return Err(
                            "extensible fmt chunk is smaller than the mandatory 40 bytes".into(),
                        );
                    }
                    tag = u16le(&bytes[body + 24..body + 26]);
                }
                format_tag = Some(tag);
            }
            b"fact" => {
                if size < 4 {
                    return Err("fact chunk is smaller than 4 bytes".into());
                }
                fact_samples = Some(u32le(&bytes[body..body + 4]));
            }
            b"data" => {
                data = Some((size as u32, body));
            }
            _ => {
                // Unknown chunks (LIST, JUNK, bext, ...) are skipped; they must
                // not invalidate the structural audit.
            }
        }
        // Chunks are word-aligned (padded to an even length).
        pos = end + (size & 1);
    }

    let format_tag = format_tag.ok_or("missing fmt chunk")?;
    let channels = channels.ok_or("missing fmt channels")?;
    let sample_rate = sample_rate.ok_or("missing fmt sample rate")?;
    let byte_rate = byte_rate.ok_or("missing fmt byte rate")?;
    let block_align = block_align.ok_or("missing fmt block align")?;
    let bits_per_sample = bits_per_sample.ok_or("missing fmt bits per sample")?;
    let (data_size, data_offset) = data.ok_or("missing data chunk")?;

    let expected_align = channels as u32 * (bits_per_sample as u32 / 8);
    if block_align as u32 != expected_align {
        return Err(format!(
            "block align {block_align} disagrees with channels×bytes-per-sample ({expected_align})"
        ));
    }
    if byte_rate as u64 != sample_rate as u64 * block_align as u64 {
        return Err(format!(
            "byte rate {byte_rate} disagrees with sample rate × block align ({})",
            sample_rate as u64 * block_align as u64
        ));
    }
    if data_offset + data_size as usize != bytes.len() {
        return Err(format!(
            "data chunk size {data_size} does not match the remaining file tail ({})",
            bytes.len() - data_offset
        ));
    }

    Ok(RiffWav {
        format_tag,
        channels,
        sample_rate,
        byte_rate,
        block_align,
        bits_per_sample,
        fact_samples,
        data_size,
        data_offset,
    })
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Decodes the `data` payload of an IEEE-float WAV into interleaved `f32`
/// frames, bit-for-bit.
fn wav_float_samples(bytes: &[u8]) -> Vec<f32> {
    let wav = parse_riff_wav(bytes).expect("file must be a structurally valid WAV");
    assert_eq!(wav.format_tag, 3, "expected IEEE float format tag");
    assert_eq!(
        wav.data_size % 4,
        0,
        "float PCM must be a multiple of 4 bytes"
    );
    let payload = &bytes[wav.data_offset..wav.data_offset + wav.data_size as usize];
    let (chunks, _remainder) = payload.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_bits(u32le(c))).collect()
}

// ---------------------------------------------------------------------------
// Deterministic signal generators (bit-exact across process and compiler)
// ---------------------------------------------------------------------------

/// Sine at `freq` Hz with `amp` amplitude; right channel is the inverse phase.
fn sine_lr(freq: f32, sample_rate: f32, amp: f32) -> impl Fn(usize) -> (f32, f32) {
    let two_pi = 2.0 * std::f32::consts::PI;
    move |n| {
        let t = n as f32 / sample_rate;
        let v = (t * freq * two_pi).sin() * amp;
        (v, -v)
    }
}

/// Deterministic LCG noise in [-1, 1); every sample is a pure function of the
/// seed and the sample index, so the expected payload reproduces bit-for-bit.
fn noise_lr(seed: u64) -> impl FnMut(usize) -> (f32, f32) {
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    move |_| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let l = ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0;
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0;
        (l, r)
    }
}

/// Ramp over a period, mapped to [-1, 1] (left) and [1, -1] (right).
fn ramp_lr(period: usize) -> impl Fn(usize) -> (f32, f32) {
    move |n| {
        let k = (n % period) as f32 / period as f32;
        (k * 2.0 - 1.0, 1.0 - k * 2.0)
    }
}

/// Builds `num_blocks` planar blocks of `frames` frames each from a global
/// sample-index signal generator.
fn make_blocks(
    signal: impl FnMut(usize) -> (f32, f32),
    frames: usize,
    num_blocks: usize,
) -> Vec<(Vec<f32>, Vec<f32>)> {
    let mut signal = signal;
    (0..num_blocks)
        .map(|b| {
            let base = b * frames;
            let left: Vec<f32> = (0..frames).map(|i| signal(base + i).0).collect();
            let right: Vec<f32> = (0..frames).map(|i| signal(base + i).1).collect();
            (left, right)
        })
        .collect()
}

/// Interleaves planar blocks into the WAV `L,R,L,R,...` frame order.
fn expected_interleaved(blocks: &[(Vec<f32>, Vec<f32>)]) -> Vec<f32> {
    let mut out = Vec::new();
    for (left, right) in blocks {
        assert_eq!(
            left.len(),
            right.len(),
            "planar planes must be frame-aligned"
        );
        for (l, r) in left.iter().zip(right) {
            out.push(*l);
            out.push(*r);
        }
    }
    out
}

/// Bit-exact comparison of interleaved samples (NaN-safe via `to_bits`).
fn assert_samples_bit_exact(actual: &[f32], expected: &[f32], ctx: &str) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{ctx}: sample count mismatch (actual {} vs expected {})",
        actual.len(),
        expected.len()
    );
    for (i, (a, e)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(
            a.to_bits(),
            e.to_bits(),
            "{ctx}: bitwise mismatch at sample {i}"
        );
    }
}

// ---------------------------------------------------------------------------
// Ring push helpers (real io_uring path)
// ---------------------------------------------------------------------------

/// Pushes a payload, retrying while the ring is full (the worker drains
/// concurrently) so 100% of the injected data is guaranteed to land.
fn push_or_retry(
    producer: &mut Producer<RingPayload<MAX_BLOCK_SIZE>>,
    payload: RingPayload<MAX_BLOCK_SIZE>,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut payload = payload;
    loop {
        match producer.push(payload) {
            Ok(()) => return,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "ring never drained — recording worker stalled"
                );
                payload = match e {
                    rtrb::PushError::Full(value) => value,
                };
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }
}

/// Pushes an audio block with retry (see [`push_or_retry`]).
fn push_block_or_retry(
    producer: &mut Producer<RingPayload<MAX_BLOCK_SIZE>>,
    block: AlignedBlock<MAX_BLOCK_SIZE>,
) {
    push_or_retry(producer, RingPayload::Audio(block));
}

/// All `capture_*.wav` files in `dir`, sorted for deterministic ordering.
fn capture_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("failed to read capture dir")
        .map(|e| e.expect("dir entry error").path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "wav")
                && p.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("capture_"))
        })
        .collect();
    files.sort();
    files
}

/// Runs one full recording session over the real io_uring worker and returns
/// the finalized capture files. The session ends with a graceful `StreamStop`.
fn record_blocks(
    dir: &Path,
    metadata: AudioMetadata,
    blocks: &[(Vec<f32>, Vec<f32>)],
) -> Vec<PathBuf> {
    let (mut producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(64);
    let (handle, _status, failed_flag) = spawn_ready_worker(consumer, dir);

    producer
        .push(RingPayload::Metadata(metadata))
        .expect("metadata push must succeed");
    for (left, right) in blocks {
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        block.fill_planar(left, right);
        push_block_or_retry(&mut producer, block);
    }
    producer
        .push(RingPayload::StreamStop)
        .expect("StreamStop push must succeed");

    let guard = RecordingWorkerGuard::new(handle, Some(producer), Some(failed_flag));
    let outcome = guard.shutdown();
    assert_eq!(
        outcome,
        RecordingWorkerOutcome::Success,
        "a clean recording session must yield a successful outcome"
    );
    capture_files(dir)
}

// ---------------------------------------------------------------------------
// Non-ignored tests: the RIFF parser against the pure header builder
// ---------------------------------------------------------------------------

/// The independent parser must accept every header the writer can produce and
/// recover the exact chunk sizes for a range of payload lengths. Each header is
/// parsed with its declared payload appended, exactly like the real files.
#[test]
fn riff_parser_accepts_generated_headers() {
    for data_bytes in [0u32, 1, 3840, 192_000, 4_000_000] {
        let header = build_wav_header(&META, data_bytes).expect("failed to build WAV header");
        let mut file = header.clone();
        file.resize(header.len() + data_bytes as usize, 0);

        let wav = parse_riff_wav(&file).expect("parser must accept the generated header");
        assert_eq!(wav.format_tag, 3, "IEEE float format tag");
        assert_eq!(wav.channels, 2);
        assert_eq!(wav.sample_rate, 48000);
        assert_eq!(wav.bits_per_sample, 32);
        assert_eq!(wav.block_align, 8);
        assert_eq!(wav.byte_rate, 48000 * 8);
        assert_eq!(wav.data_size, data_bytes);
        assert_eq!(
            wav.data_offset,
            header.len(),
            "data chunk must begin exactly at the header end"
        );
        // `fact` is optional for float PCM (hound never writes it); whenever it
        // is present it must carry the exact per-channel sample count.
        if let Some(fact_samples) = wav.fact_samples {
            assert_eq!(
                fact_samples,
                data_bytes / 8,
                "fact chunk must carry samples per channel when present"
            );
        }
    }
}

/// The parser must reject corrupt files: broken magic, a lying RIFF size and a
/// truncated `data` chunk must all be flagged instead of silently accepted.
#[test]
fn riff_parser_rejects_corrupt_or_truncated_headers() {
    let header = build_wav_header(&META, 3840).expect("failed to build WAV header");
    let mut file = header.clone();
    file.resize(header.len() + 3840, 0);

    let mut bad_magic = file.clone();
    bad_magic[0] = b'X';
    assert!(
        parse_riff_wav(&bad_magic).is_err(),
        "corrupted RIFF magic must be rejected"
    );

    let mut lying_size = file.clone();
    lying_size[4..8].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    assert!(
        parse_riff_wav(&lying_size).is_err(),
        "a RIFF size field that lies about the file length must be rejected"
    );

    // Truncate the payload: the data chunk now declares more bytes than exist.
    file.truncate(file.len() - 8);
    assert!(
        parse_riff_wav(&file).is_err(),
        "a truncated data chunk must be rejected"
    );

    // A data chunk that is not the final chunk must also be rejected.
    let mut padded = build_wav_header(&META, 0).expect("failed to build empty header");
    padded.extend_from_slice(&[0u8; 12]); // trailing garbage after data
    assert!(
        parse_riff_wav(&padded).is_err(),
        "trailing bytes after the data chunk must be rejected"
    );
}

/// The independent parser must agree with `hound` on the spec of a real file
/// (cross-check between the two independent readers).
#[test]
fn riff_parser_cross_checks_against_hound() {
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());
    let path = dir.join("cross-check.wav");
    let mut file = build_wav_header(&META, 3840).expect("failed to build header");
    file.resize(file.len() + 3840, 0);
    std::fs::write(&path, &file).expect("failed to write cross-check WAV");
    let bytes = std::fs::read(&path).expect("failed to read cross-check WAV");

    let wav = parse_riff_wav(&bytes).expect("parser must accept the written file");
    let reader = hound::WavReader::open(&path).expect("hound must open the same file");
    let spec = reader.spec();
    assert_eq!(spec.sample_format, hound::SampleFormat::Float);
    assert_eq!(spec.channels, wav.channels);
    assert_eq!(spec.sample_rate, wav.sample_rate);
    assert_eq!(spec.bits_per_sample, wav.bits_per_sample);
    let parsed_frames = wav.data_size as u32 / wav.block_align as u32;
    assert_eq!(reader.duration(), parsed_frames);
    if let Some(fact_samples) = wav.fact_samples {
        assert_eq!(
            fact_samples, parsed_frames,
            "fact must match the parsed frame count"
        );
    }
}

// ---------------------------------------------------------------------------
// Ignored: byte-by-byte fidelity (real io_uring writer)
// ---------------------------------------------------------------------------

/// Injects sine, deterministic noise and ramp signals through the SPSC ring and
/// proves the finalized WAV matches the source sample-by-sample (bit-exact) and
/// passes the independent RIFF structural audit — including a cross-check of
/// the parsed spec against `hound`.
#[test]
#[ignore = "requires io_uring support"]
fn wav_byte_exact_sine_noise_ramp_roundtrip() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());

    let sine = make_blocks(sine_lr(440.0, 48_000.0, 0.5), 480, 8);
    let noise = make_blocks(noise_lr(0x00C0_FFEE), 480, 8);
    let ramp = make_blocks(ramp_lr(97), 480, 8);
    let mut blocks = sine;
    blocks.extend(noise);
    blocks.extend(ramp);

    let files = record_blocks(&dir, META, &blocks);
    assert_eq!(
        files.len(),
        1,
        "a single stream must produce a single capture"
    );

    let bytes = std::fs::read(&files[0]).expect("failed to read recorded WAV");
    let wav = parse_riff_wav(&bytes).expect("recorded WAV must be structurally valid");
    assert_eq!(wav.format_tag, 3);
    assert_eq!(wav.channels, 2);
    assert_eq!(wav.sample_rate, 48000);
    assert_eq!(wav.bits_per_sample, 32);
    assert_eq!(wav.data_size as usize, 24 * 480 * 2 * 4);

    let expected = expected_interleaved(&blocks);
    let actual = wav_float_samples(&bytes);
    assert_samples_bit_exact(&actual, &expected, "sine/noise/ramp roundtrip");

    // Cross-check the independent parser against hound on the real file.
    let reader = hound::WavReader::open(&files[0]).expect("hound must open the recorded WAV");
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.spec().sample_rate, 48000);
    assert_eq!(reader.spec().bits_per_sample, 32);
    assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
    assert_eq!(reader.duration() as usize, 24 * 480);
}

/// A mid-stream metadata change must finalize the current capture and start a
/// sequential `_partN` file; both parts must be byte-exact and carry the
/// correct per-part format.
#[test]
#[ignore = "requires io_uring support"]
fn wav_metadata_change_splits_part2_byte_exact() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());

    let part1_blocks = make_blocks(sine_lr(220.0, 48_000.0, 0.4), 480, 4);
    let part2_blocks = make_blocks(noise_lr(0x0BAD_C0DE), 480, 4);

    let (mut producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(64);
    let (handle, _status, failed_flag) = spawn_ready_worker(consumer, &dir);

    producer
        .push(RingPayload::Metadata(META))
        .expect("metadata push must succeed");
    for (left, right) in &part1_blocks {
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        block.fill_planar(left, right);
        push_block_or_retry(&mut producer, block);
    }

    let meta_44100 = AudioMetadata {
        sample_rate: 44100.0,
        bit_depth: 32,
        channels: 2,
    };
    producer
        .push(RingPayload::Metadata(meta_44100))
        .expect("metadata change push must succeed");
    for (left, right) in &part2_blocks {
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        block.fill_planar(left, right);
        push_block_or_retry(&mut producer, block);
    }

    producer
        .push(RingPayload::StreamStop)
        .expect("StreamStop push must succeed");
    let guard = RecordingWorkerGuard::new(handle, Some(producer), Some(failed_flag));
    assert_eq!(guard.shutdown(), RecordingWorkerOutcome::Success);

    let files = capture_files(&dir);
    assert_eq!(
        files.len(),
        2,
        "a metadata change must split the capture into two parts"
    );
    assert!(
        files[0].to_string_lossy().ends_with(".wav"),
        "part 1 must be the base capture: {}",
        files[0].display()
    );
    assert!(
        files[1].to_string_lossy().contains("_part2"),
        "part 2 must be a sequential part: {}",
        files[1].display()
    );

    let part1_bytes = std::fs::read(&files[0]).expect("failed to read part 1");
    let part1 = parse_riff_wav(&part1_bytes).expect("part 1 must be structurally valid");
    assert_eq!(part1.sample_rate, 48000);
    assert_samples_bit_exact(
        &wav_float_samples(&part1_bytes),
        &expected_interleaved(&part1_blocks),
        "part 1 (48 kHz sine)",
    );

    let part2_bytes = std::fs::read(&files[1]).expect("failed to read part 2");
    let part2 = parse_riff_wav(&part2_bytes).expect("part 2 must be structurally valid");
    assert_eq!(part2.sample_rate, 44100);
    assert_samples_bit_exact(
        &wav_float_samples(&part2_bytes),
        &expected_interleaved(&part2_blocks),
        "part 2 (44.1 kHz noise)",
    );
}

// ---------------------------------------------------------------------------
// Ignored: fault injection mid-stream (EFBIG = ENOSPC-class failure)
// ---------------------------------------------------------------------------

/// Fires an `ENOSPC`-class fault (`RLIMIT_FSIZE` → `EFBIG`, with `SIGXFSZ`
/// ignored so the process survives) in the middle of the audio flow, through
/// the real `io_uring` writer. The failure must be observable everywhere:
///
/// * the worker returns `Err` → the guard reports `Failed`;
/// * the observable status transitions to `Failed`;
/// * the RT failure flag is raised (the audio callback suspends enqueueing);
/// * the partial file is preserved on disk, bit-exact up to the fault point.
///
/// The rlimit is process-wide, so this test must run with `--test-threads=1`
/// (Phase 4 of `utils/tests-quick.sh`); `TEST_MUTEX` also serializes it.
#[test]
#[ignore = "requires io_uring support and --test-threads=1 (process-wide rlimit mutation)"]
fn enospc_class_failure_mid_stream_marks_failed_and_preserves_partial_wav() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());

    const BLOCK_SAMPLES: usize = 480;
    const OK_BLOCKS: usize = 2;
    let header_len = build_wav_header(&META, 0)
        .expect("failed to build header")
        .len();
    let block_bytes = (BLOCK_SAMPLES * 2 * 4) as u64;
    let fault_limit = header_len as u64 + OK_BLOCKS as u64 * block_bytes;

    // Save the previous limit so the guard can restore it (even on panic).
    // SAFETY: `prev` is a valid writable `rlimit` buffer; `zeroed` is sound
    // because every bit pattern of an `rlim_t` is a valid limit value.
    let mut prev = unsafe { std::mem::zeroed::<libc::rlimit>() };
    // SAFETY: `prev` is a valid writable `rlimit` buffer.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut prev) }, 0);
    let _rlimit_guard = RlimitGuard(prev);

    // Ignore SIGXFSZ so exceeding the limit returns EFBIG instead of killing
    // the test process. Deliberately left in place for the rest of the process
    // (no other test raises the limit, so it cannot affect them).
    // SAFETY: `signal` is async-signal-safe and the disposition change is
    // intentional for this test process.
    let prev_handler = unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
    assert_ne!(prev_handler, libc::SIG_ERR, "failed to ignore SIGXFSZ");

    // Only the *soft* limit is lowered (clamped to the existing hard limit):
    // raising the hard `rlim_max` back after the test would require
    // `CAP_SYS_RESOURCE`, which a test process must not depend on.
    assert!(
        fault_limit <= prev.rlim_max,
        "host hard RLIMIT_FSIZE is below the test's fault point"
    );
    let new_limit = libc::rlimit {
        rlim_cur: fault_limit,
        rlim_max: prev.rlim_max,
    };
    // SAFETY: lowering RLIMIT_FSIZE is always permitted; the struct is valid.
    assert_eq!(
        unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &new_limit) },
        0
    );

    let (mut producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(64);
    let (handle, status, failed_flag) = spawn_ready_worker(consumer, &dir);

    producer
        .push(RingPayload::Metadata(META))
        .expect("metadata push must succeed");

    let mut expected_payload = Vec::new();
    for block_idx in 0..(OK_BLOCKS + 1) {
        let left: Vec<f32> = (0..BLOCK_SAMPLES)
            .map(|i| (block_idx * BLOCK_SAMPLES + i) as f32 * 0.001)
            .collect();
        let right: Vec<f32> = left.iter().map(|v| -v).collect();
        if block_idx < OK_BLOCKS {
            for (l, r) in left.iter().zip(&right) {
                expected_payload.extend_from_slice(&l.to_le_bytes());
                expected_payload.extend_from_slice(&r.to_le_bytes());
            }
        }
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        block.fill_planar(&left, &right);
        push_block_or_retry(&mut producer, block);
    }

    let guard = RecordingWorkerGuard::new(handle, Some(producer), Some(failed_flag.clone()));
    let outcome = guard.shutdown();
    assert!(
        matches!(outcome, RecordingWorkerOutcome::Failed { .. }),
        "an ENOSPC-class mid-stream fault must surface as a worker failure, got {outcome:?}"
    );

    match &*status.lock().unwrap() {
        RecordingStatus::Failed { reason } => {
            assert!(
                reason.contains("audio block"),
                "failure reason must point at the block write: {reason}"
            );
        }
        other => panic!("status must be Failed, got {other:?}"),
    }
    assert!(
        failed_flag.load(Ordering::Acquire),
        "the RT failure flag must be raised on a mid-stream fault"
    );

    // Rollback: the partial file must be preserved on disk, bit-exact up to the
    // fault point — header + OK_BLOCKS full blocks, no silent truncation.
    let files = capture_files(&dir);
    assert_eq!(files.len(), 1, "exactly one partial capture must exist");
    let bytes = std::fs::read(&files[0]).expect("failed to read partial WAV");
    assert_eq!(
        bytes.len() as u64,
        fault_limit,
        "partial file must hold the header plus the blocks written before the fault"
    );
    assert_eq!(
        &bytes[..header_len],
        &build_wav_header(&META, 0).unwrap()[..]
    );
    assert_eq!(
        &bytes[header_len..],
        &expected_payload[..],
        "bytes up to the fault must match the source exactly"
    );
}

// ---------------------------------------------------------------------------
// Ignored: concurrent signals under high transfer rate
// ---------------------------------------------------------------------------

/// Simulates a `SIGINT` (`SHUTDOWN = true`, exactly what the app's signal
/// handler does) arriving under high audio transfer rate. The worker must keep
/// draining every block produced after the signal and finalize 100% of the
/// samples — the T3.4 lifecycle invariant, now proven at integration level
/// against the real `io_uring` writer.
#[test]
#[ignore = "requires io_uring support"]
fn sigint_shutdown_under_high_rate_never_truncates() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());

    const PRE: usize = 1000;
    const POST: usize = 200;
    let blocks = make_blocks(noise_lr(0x5EED_2026), 480, PRE + POST);
    let expected = expected_interleaved(&blocks);

    let (mut producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(RING_CAPACITY);
    let (handle, _status, failed_flag) = spawn_ready_worker(consumer, &dir);

    producer
        .push(RingPayload::Metadata(META))
        .expect("metadata push must succeed");

    for (i, (left, right)) in blocks.iter().enumerate() {
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        block.fill_planar(left, right);
        push_block_or_retry(&mut producer, block);
        if i == PRE - 1 {
            // SIGINT fires mid-stream while the RT callback is still pushing.
            SHUTDOWN.store(true, Ordering::Release);
        }
    }

    // The ring may still be momentarily full (the last audio push succeeded on
    // a freed slot); retry so the terminal token always lands.
    push_or_retry(&mut producer, RingPayload::StreamStop);
    let guard = RecordingWorkerGuard::new(handle, Some(producer), Some(failed_flag));
    assert_eq!(
        guard.shutdown(),
        RecordingWorkerOutcome::Success,
        "the worker must ignore SHUTDOWN and drain the full stream"
    );

    let files = capture_files(&dir);
    assert_eq!(files.len(), 1);
    let bytes = std::fs::read(&files[0]).expect("failed to read recorded WAV");
    assert_samples_bit_exact(
        &wav_float_samples(&bytes),
        &expected,
        "SIGINT under high rate must never truncate the tail",
    );
}

/// Simulates a `SIGTERM` (recording producer vanishes without `StreamStop`).
/// The worker must terminate through the "abandoned + drained" condition and
/// finalize every pending block into the WAV — nothing is orphaned.
#[test]
#[ignore = "requires io_uring support"]
fn sigterm_producer_drop_drains_and_finalizes_byte_exact() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());

    let blocks = make_blocks(ramp_lr(313), 480, 1000);

    let (mut producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(RING_CAPACITY);
    let (handle, _status, failed_flag) = spawn_ready_worker(consumer, &dir);

    producer
        .push(RingPayload::Metadata(META))
        .expect("metadata push must succeed");
    for (left, right) in &blocks {
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        block.fill_planar(left, right);
        push_block_or_retry(&mut producer, block);
    }

    // SIGTERM simulation: the producer is dropped without StreamStop.
    drop(producer);
    let guard = RecordingWorkerGuard::new(handle, None, Some(failed_flag));
    assert_eq!(
        guard.shutdown(),
        RecordingWorkerOutcome::Success,
        "abandoned + drained must finalize a complete WAV"
    );

    let files = capture_files(&dir);
    assert_eq!(files.len(), 1);
    let bytes = std::fs::read(&files[0]).expect("failed to read recorded WAV");
    assert_samples_bit_exact(
        &wav_float_samples(&bytes),
        &expected_interleaved(&blocks),
        "SIGTERM (producer drop) must drain every pending block",
    );
}

// ---------------------------------------------------------------------------
// Ignored: anti-TOCTOU atomic creation with 20 concurrent instances
// ---------------------------------------------------------------------------

/// 20 recording instances racing to create files in the same directory at the
/// same instant must each land on a distinct atomically-created capture file —
/// the kernel's `O_EXCL` (via `create_new(true)`) plus the `-1`, `-2`, ...
/// suffix resolution guarantees no clobbering and no lost capture (F-RB-008 /
/// T3.2), now proven end-to-end through the real `io_uring` workers.
#[test]
#[ignore = "requires io_uring support"]
fn concurrent_workers_same_dir_atomic_creation_no_clobber() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());

    const INSTANCES: usize = 20;
    const BLOCK_SAMPLES: usize = 480;
    let barrier = Arc::new(Barrier::new(INSTANCES));

    // Expected data payloads (interleaved bytes), one distinct signature per
    // instance — proof that every file carries exactly one instance's data.
    let expected_payloads: BTreeSet<Vec<u8>> = (0..INSTANCES)
        .map(|instance| {
            let mut payload = Vec::with_capacity(BLOCK_SAMPLES * 2 * 4);
            for i in 0..BLOCK_SAMPLES {
                let l = (instance * 4096 + i) as f32 * 0.001;
                payload.extend_from_slice(&l.to_le_bytes());
                payload.extend_from_slice(&(-l).to_le_bytes());
            }
            payload
        })
        .collect();

    let mut found_payloads: BTreeSet<Vec<u8>> = BTreeSet::new();
    std::thread::scope(|scope| {
        for instance in 0..INSTANCES {
            let dir = dir.clone();
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                let (mut producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(8);
                let (init, init_rx, _status, failed_flag) = recording_init_for(&dir);
                let handle =
                    spawn_recording_worker(consumer, None, init).expect("spawn recording worker");

                // Align all instances so the atomic create attempts collide in
                // the same instant (same-second timestamps → suffix races).
                barrier.wait();
                wait_for_recording_init(init_rx, Duration::from_secs(5))
                    .expect("recording worker must confirm readiness");

                let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
                let left: Vec<f32> = (0..BLOCK_SAMPLES)
                    .map(|i| (instance * 4096 + i) as f32 * 0.001)
                    .collect();
                let right: Vec<f32> = left.iter().map(|v| -v).collect();
                block.fill_planar(&left, &right);

                producer
                    .push(RingPayload::Metadata(META))
                    .expect("metadata push must succeed");
                producer
                    .push(RingPayload::Audio(block))
                    .expect("audio push must succeed");
                producer
                    .push(RingPayload::StreamStop)
                    .expect("StreamStop push must succeed");

                let guard = RecordingWorkerGuard::new(handle, Some(producer), Some(failed_flag));
                assert_eq!(
                    guard.shutdown(),
                    RecordingWorkerOutcome::Success,
                    "instance {instance} must finalize its capture cleanly"
                );
            });
        }
    });

    let files = capture_files(&dir);
    assert_eq!(
        files.len(),
        INSTANCES,
        "each concurrent instance must create exactly one capture file"
    );

    for path in &files {
        let bytes = std::fs::read(path).expect("failed to read capture");
        let wav = parse_riff_wav(&bytes).expect("concurrent capture must be a valid WAV");
        assert_eq!(wav.channels, 2);
        assert_eq!(wav.bits_per_sample, 32);
        assert_eq!(wav.data_size as usize, BLOCK_SAMPLES * 2 * 4);

        let payload = &bytes[wav.data_offset..wav.data_offset + wav.data_size as usize];
        assert!(
            expected_payloads.contains(payload),
            "capture {} carries unrecognized data — possible cross-instance clobbering",
            path.display()
        );
        found_payloads.insert(payload.to_vec());
    }

    assert_eq!(
        found_payloads, expected_payloads,
        "the 20 captures must cover the 20 distinct instance signatures exactly once"
    );
}

// ---------------------------------------------------------------------------
// Ignored: FD / thread leak sweep (100 consecutive cycles)
// ---------------------------------------------------------------------------

/// Executes 100 consecutive init → record → stop cycles and proves the process
/// returns to its baseline open-FD and thread counts (`/proc/self/fd` and
/// `/proc/self/status`). Catches zombie `nam-recording-io` threads and leaked
/// `io_uring` ring descriptors.
#[test]
#[ignore = "heavy: 100 consecutive recording cycles; requires io_uring support"]
fn recording_cycles_fd_thread_leak_sweep() {
    let _lock = TEST_MUTEX.lock().unwrap();
    let _sd = ShutdownGuard::new();
    let dir = temp_dir();
    let _guard = DirGuard::new(dir.clone());

    let baseline_fds = count_open_fds();
    let baseline_threads = count_threads();

    for cycle in 0..100 {
        let (mut producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(16);
        let (handle, _status, failed_flag) = spawn_ready_worker(consumer, &dir);

        producer
            .push(RingPayload::Metadata(META))
            .expect("metadata push must succeed");
        let mut block = AlignedBlock::<MAX_BLOCK_SIZE>::new();
        block.fill_planar(&[0.5f32; 128], &[-0.5f32; 128]);
        producer
            .push(RingPayload::Audio(block))
            .expect("audio push must succeed");
        producer
            .push(RingPayload::StreamStop)
            .expect("StreamStop push must succeed");

        let guard = RecordingWorkerGuard::new(handle, Some(producer), Some(failed_flag));
        assert_eq!(
            guard.shutdown(),
            RecordingWorkerOutcome::Success,
            "cycle {cycle} must finalize cleanly"
        );

        // Remove the capture produced by this cycle so the directory does not
        // accumulate 100 files across the sweep.
        for path in capture_files(&dir) {
            let _ = std::fs::remove_file(path);
        }
    }

    // Settle: give any background runtime teardown a chance to complete before
    // counting resources.
    std::thread::sleep(Duration::from_millis(150));

    let after_fds = count_open_fds();
    let after_threads = count_threads();
    assert_eq!(
        after_fds, baseline_fds,
        "FD leak after 100 recording cycles: baseline {baseline_fds}, after {after_fds}"
    );
    assert_eq!(
        after_threads, baseline_threads,
        "thread leak after 100 recording cycles: baseline {baseline_threads}, after {after_threads}"
    );
}

/// Number of open file descriptors in `/proc/self/fd` (same methodology for
/// baseline and post-sweep counts, so the count's own fd cancels out).
fn count_open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count())
        .unwrap_or(0)
}

/// Thread count reported by `/proc/self/status` (`Threads: N`).
fn count_threads() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Threads:"))
                .and_then(|value| value.trim().parse().ok())
        })
        .unwrap_or(0)
}
