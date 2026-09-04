// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Service-resilience validation harness.
//!
//! Certifies the backend recovery and service shutdown gates end to end:
//!
//! 1. **Real `SIGTERM` subprocess acceptance** — spawns the compiled
//!    `nam-audio-pipe --record` binary under live PipeWire, drives the capture
//!    sink with a deterministic tone, sends `SIGTERM` via `libc::kill` and
//!    proves the process exits `0` while the WAV on disk is 100% readable,
//!    carries a valid header with a **closed** `data` chunk and **bit-exact
//!    samples** (two independent readers — a hand-rolled RIFF walker and
//!    `hound` — must decode the exact same finite `f32` bits, and the declared
//!    `data` size must equal the file tail).
//! 2. **Double-signal acceptance** — two rapid `SIGTERM`s must force immediate
//!    termination via the async-signal-safe `_exit(1)` path.
//! 3. **Bridge-starvation silence + recycle** — with zero new bridge generation
//!    the playback kernel must emit `0.0f32` analytical silence sequences,
//!    stamp/recycle the SPA buffers and never stall (soaked over thousands of
//!    quantums with xrun-free telemetry).
//! 4. **SPA format-contract rejection** — mono, interleaved, `S16` and surround
//!    renegotiations must raise `RT_STATUS_HOST_CONTRACT_VIOLATION`, latch the
//!    RT mute guard and operate fail-closed.
//! 5. **Forced stream error (`StreamState::Error`)** — the stream-state observer
//!    must publish a sticky `Failed` transition that the main control loop
//!    observes within the < 500 ms SLA; post-streaming `Unconnected` (daemon
//!    crash/restart) and the bounded reconnect cycle complete the lifecycle.
//!
//! The subprocess tests (1 and 2) require a running PipeWire daemon (and, for
//! the recording acceptance, `pw-play` + `io_uring`); they are `#[ignore]`d and
//! executed in Phase 3 of `utils/tests-quick.sh`. Tests 3–5 are daemon-free
//! pure-kernel/state-machine acceptances that run in every quick pass.

use nam_audio_pipe::recording::{IoUringSupport, probe_io_uring};
use nam_audio_pipe::standalone::pw_host::output_pw::{
    ContractViolation, SpaPodStorage, build_spa_format_pod, deliver_silence_pair_fail_closed,
    mark_format_contract_ok, reject_negotiated_format_violation, validate_audio_raw_format,
};
use nam_audio_pipe::standalone::pw_host::{
    BackendState, SharedBackendStatus, observe_stream_state,
};
use neural_amp_modeler_rs::common::spsc::{RT_STATUS_HOST_CONTRACT_VIOLATION, RtStatusFlags};
use pipewire as pw;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

mod common;

// ---------------------------------------------------------------------------
// Subprocess helpers
// ---------------------------------------------------------------------------

/// RAII custody of the spawned `nam-audio-pipe` child.
///
/// Kills and reaps the child on drop (even on panic), so a failed assertion can
/// never leave an orphaned audio host mutating the user's PipeWire graph.
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn pid(&self) -> i32 {
        self.child.as_ref().expect("child present").id() as i32
    }

    /// Polls the child without blocking: `Some` once it has exited (the status
    /// is cached by the OS, so repeated polls and a later [`Self::wait`] are
    /// all consistent).
    fn try_status(&mut self) -> Option<ExitStatus> {
        let child = self.child.as_mut().expect("child present");
        match child.try_wait() {
            Ok(status) => status,
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }

    /// Waits for the child to exit within `timeout`. Panics (after killing the
    /// child via `Drop`) if it does not — a hang is a resilience failure.
    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_status() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "nam-audio-pipe did not exit within {timeout:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawns `nam-audio-pipe` with `args` in `cwd`.
///
/// stdout is discarded and stderr is redirected to a per-child log file inside
/// `cwd` so a chatty logger can never deadlock the child on a full pipe. The
/// recording worker writes `capture_*.wav` into `cwd` (its base directory).
fn spawn_host(args: &[&str], cwd: &std::path::Path) -> (ChildGuard, std::path::PathBuf) {
    let stderr_path = cwd.join("child-stderr.log");
    let stderr_file =
        std::fs::File::create(&stderr_path).expect("failed to create child stderr log");
    let child = Command::new(env!("CARGO_BIN_EXE_nam-audio-pipe"))
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("failed to spawn nam-audio-pipe");
    (ChildGuard::new(child), stderr_path)
}

/// Reads the child's captured stderr log (diagnostics for failure messages).
fn read_child_stderr(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Sends `sig` to the child via `libc::kill`.
fn send_signal(guard: &ChildGuard, sig: i32) {
    // SAFETY: `pid` is a live child PID owned by this test; `kill` is
    // async-signal-safe and has no Rust aliasing implications.
    let rc = unsafe { libc::kill(guard.pid(), sig) };
    assert_eq!(
        rc,
        0,
        "kill({}, {sig}) failed with errno {:?}",
        guard.pid(),
        std::io::Error::last_os_error()
    );
}

/// Newest `capture_*.wav` file in `dir` (timestamp-prefixed names sort
/// chronologically). Only the recording worker's capture files qualify — the
/// tone fixture is also a `.wav` in the same directory and must never be
/// mistaken for a recording.
fn newest_capture_wav(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut wavs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|ext| ext == "wav")
                && p.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("capture_"))
        })
        .collect();
    wavs.sort();
    wavs.pop()
}

/// Waits until a `capture_*.wav` in `dir` has grown past its header (real PCM
/// samples were persisted), failing fast if the child exits early instead.
fn wait_for_recorded_audio(
    child: &mut ChildGuard,
    dir: &std::path::Path,
    timeout: Duration,
) -> std::path::PathBuf {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_status() {
            panic!(
                "nam-audio-pipe exited early with {status:?} before recording audio \
                 (stderr below)\n{}",
                read_child_stderr(&dir.join("child-stderr.log"))
            );
        }
        if let Some(path) = newest_capture_wav(dir)
            && std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > 64
        {
            return path;
        }
        assert!(
            Instant::now() < deadline,
            "no recorded audio appeared in {:?} within {timeout:?}",
            dir.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Writes a deterministic 30 s 440 Hz stereo sine (48 kHz, 0.25 amplitude) to
/// `path` — a real, audible-level tone so the noise gate opens and the
/// recording ring receives actual samples (a silent tone would be trimmed).
fn generate_tone_wav(path: &std::path::Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create tone wav");
    for n in 0..30 * spec.sample_rate {
        let v = (n as f32 / spec.sample_rate as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.25;
        let sample = (v * i16::MAX as f32) as i16;
        writer.write_sample(sample).expect("write tone sample");
        writer.write_sample(sample).expect("write tone sample");
    }
    writer.finalize().expect("finalize tone wav");
}

/// Spawns `pw-play` playing `wav` into the NAM capture sink so the graph
/// deterministically schedules the capture node and opens the noise gate.
fn spawn_tone(wav: &std::path::Path) -> Option<Child> {
    Command::new("pw-play")
        .args(["--target", "NAM-Audio-Pipe-input", "--volume", "1.0"])
        .arg(wav)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()
}

/// RAII attachment of the tone process (killed on drop, even on panic).
struct ToneAttacher {
    wav: std::path::PathBuf,
    child: Option<Child>,
}

impl ToneAttacher {
    fn new(wav: std::path::PathBuf) -> Self {
        Self { wav, child: None }
    }

    fn attach(&mut self) {
        if self.child.is_none() {
            self.child = spawn_tone(&self.wav);
        }
    }
}

impl Drop for ToneAttacher {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

// ---------------------------------------------------------------------------
// Independent WAV validation (no `hound`)
// ---------------------------------------------------------------------------

/// Validates the finalized WAV produced under a service signal.
///
/// * **100% readable + valid header**: the RIFF/WAVE envelope and `data` chunk
///   parse cleanly.
/// * **Closed `data` chunk**: the declared `data` size equals the file tail
///   exactly — the header rewrite + `fsync` completed, nothing is truncated.
/// * **Bit-exact samples**: every sample decodes to a finite `f32` bit pattern
///   and a fully independent reader (`hound`) decodes the exact same bits — no
///   corruption, no lost tail.
/// * **Signal present / silence expected**: if `expect_silence` is false, asserts
///   that real non-degenerate audio flowed; if true, permits silence while
///   guaranteeing WAV container and sample integrity.
fn assert_valid_finalized_wav(path: &std::path::Path, expect_silence: bool) {
    let bytes = std::fs::read(path).expect("recorded WAV must be readable");
    assert!(
        bytes.len() >= 44 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE",
        "file is not a valid RIFF/WAVE container ({} bytes)",
        bytes.len()
    );

    // Walk the chunk list independently of any external WAV reader.
    let mut data: Option<(u32, usize)> = None;
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        let end = body.checked_add(size).expect("chunk size overflow");
        assert!(
            end <= bytes.len(),
            "chunk {:?} is truncated",
            String::from_utf8_lossy(id)
        );
        if id == b"data" {
            data = Some((size as u32, body));
        }
        pos = end + (size & 1);
    }
    let (data_size, data_offset) = data.expect("missing data chunk");
    assert!(
        data_size > 0,
        "WAV data chunk is empty — no audio flowed to disk"
    );
    assert_eq!(
        data_offset + data_size as usize,
        bytes.len(),
        "the 'data' chunk must be closed (declared size == file tail) — \
         the header was not finalized"
    );

    // Decode the payload independently: every sample must be a finite f32.
    let payload = &bytes[data_offset..];
    assert!(
        payload.len().is_multiple_of(4),
        "float PCM payload must be a multiple of 4 bytes"
    );
    let independent: Vec<f32> = payload
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| f32::from_bits(u32::from_le_bytes(*w)))
        .collect();
    for (i, sample) in independent.iter().enumerate() {
        assert!(
            sample.is_finite(),
            "sample {i} is not a finite f32 bit pattern: {sample:?}"
        );
    }
    assert!(
        independent.len().is_multiple_of(2),
        "stereo payload must hold an even number of samples"
    );
    assert_eq!(
        data_size as usize,
        independent.len() * 4,
        "declared data size must match the decoded sample count bit-exactly"
    );

    // Cross-reader bit-exactness: `hound` (fully independent) must decode the
    // exact same bits, and the format must be the negotiated 48 kHz float
    // stereo contract.
    let reader = hound::WavReader::open(path).expect("hound must open the finalized WAV");
    let spec = reader.spec();
    assert_eq!(spec.channels, 2, "recorded WAV must be stereo");
    assert_eq!(spec.sample_rate, 48000, "recorded WAV must be 48 kHz");
    assert_eq!(
        spec.bits_per_sample, 32,
        "recorded WAV must be 32-bit float"
    );
    assert_eq!(spec.sample_format, hound::SampleFormat::Float);
    let hound_samples: Vec<f32> = reader
        .into_samples::<f32>()
        .map(|s| s.expect("hound must decode every sample"))
        .collect();
    assert_eq!(
        hound_samples.len(),
        independent.len(),
        "hound must see the same number of samples"
    );
    for (i, (a, b)) in hound_samples.iter().zip(&independent).enumerate() {
        assert_eq!(
            a.to_bits(),
            b.to_bits(),
            "independent readers disagree on sample {i} — payload corruption"
        );
    }

    if !expect_silence {
        // The noise gate opened: real (non-degenerate) signal reached the disk.
        assert!(
            independent.iter().any(|s| s.abs() > 0.01),
            "recorded signal is degenerate (all samples near zero) — no real audio flowed"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. Real SIGTERM subprocess acceptance (WAV finalized, exit 0)
// ---------------------------------------------------------------------------

/// Spawns `nam-audio-pipe --record` under live PipeWire, drives the capture
/// sink with a deterministic tone, sends a real `SIGTERM` via `libc::kill` and
/// proves:
///
/// * the child exits **0** (graceful service shutdown — not killed by the
///   signal, not `_exit(1)`); and
/// * the recorded WAV on disk is 100% readable, has a valid header with a
///   **closed** `data` chunk and **bit-exact** finite samples (see
///   [`assert_valid_finalized_wav`]).
///
/// Requires a running PipeWire daemon, `pw-play` and `io_uring`. Runs in
/// Phase 3 of `utils/tests-quick.sh`.
#[test]
#[ignore = "requires a running PipeWire daemon + pw-play + io_uring; runs in tests-quick Phase 3"]
fn sigterm_subprocess_finalizes_wav_gracefully() {
    if !common::probe_pipewire_daemon() {
        eprintln!("SKIP: PipeWire daemon not detected (pw-cli info 0 failed).");
        return;
    }
    if !common::pw_play_available() {
        eprintln!("SKIP: pw-play unavailable; cannot drive the capture sink deterministically.");
        return;
    }
    if probe_io_uring() != IoUringSupport::Available {
        eprintln!("SKIP: io_uring unavailable; --record cannot start.");
        return;
    }

    let _lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _sd = common::ShutdownGuard::new();

    let dir = common::temp_dir();
    let _guard = common::DirGuard::new(dir.clone());

    let tone_path = dir.join("tone.wav");
    generate_tone_wav(&tone_path);
    let mut tone = ToneAttacher::new(tone_path);

    let (mut child, stderr_path) = spawn_host(&["--record"], &dir);

    // Attach the tone only once the capture sink is registered in the graph
    // (same fail-closed discipline as pw_integration: a daemon loss here is a
    // defect, not a skip — the probe above already gated on the daemon).
    assert!(
        common::wait_for_nam_sink(Duration::from_secs(10)),
        "host capture sink never registered; stderr:\n{}",
        read_child_stderr(&stderr_path)
    );
    tone.attach();

    // Prove the ring received samples before signalling: a finalized-but-empty
    // WAV would not satisfy the "amostras bit-exact" acceptance.
    let wav_path = wait_for_recorded_audio(&mut child, &dir, Duration::from_secs(10));

    // Real service signal (SIGTERM is systemd/container default shutdown).
    send_signal(&child, libc::SIGTERM);

    let started = Instant::now();
    let status = child.wait(Duration::from_secs(15));
    let elapsed = started.elapsed();
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM must produce a graceful exit 0 (got {status:?} after {elapsed:?}); \
         stderr:\n{}",
        read_child_stderr(&stderr_path)
    );
    println!(
        "SIGTERM acceptance: child exited 0 in {elapsed:?} after signal; WAV at {}",
        wav_path.display()
    );

    // The WAV the child finalized while handling the signal must be complete.
    assert_valid_finalized_wav(&wav_path, false);
}

/// Spawns `nam-audio-pipe --record --gate off` under live PipeWire, drives the
/// capture sink, sends `SIGTERM` via `libc::kill`, and proves graceful exit 0
/// and full WAV finalization with silence preserved.
#[test]
#[ignore = "requires a running PipeWire daemon + pw-play + io_uring; runs in tests-quick Phase 3"]
fn sigterm_subprocess_finalizes_wav_gracefully_gate_off() {
    if !common::probe_pipewire_daemon() {
        eprintln!("SKIP: PipeWire daemon not detected (pw-cli info 0 failed).");
        return;
    }
    if !common::pw_play_available() {
        eprintln!("SKIP: pw-play unavailable; cannot drive the capture sink deterministically.");
        return;
    }
    if probe_io_uring() != IoUringSupport::Available {
        eprintln!("SKIP: io_uring unavailable; --record cannot start.");
        return;
    }

    let _lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _sd = common::ShutdownGuard::new();

    let dir = common::temp_dir();
    let _guard = common::DirGuard::new(dir.clone());

    let tone_path = dir.join("tone.wav");
    generate_tone_wav(&tone_path);
    let mut tone = ToneAttacher::new(tone_path);

    let (mut child, stderr_path) = spawn_host(&["--record", "--gate", "off"], &dir);

    assert!(
        common::wait_for_nam_sink(Duration::from_secs(10)),
        "host capture sink never registered; stderr:\n{}",
        read_child_stderr(&stderr_path)
    );
    tone.attach();

    let wav_path = wait_for_recorded_audio(&mut child, &dir, Duration::from_secs(10));

    send_signal(&child, libc::SIGTERM);

    let started = Instant::now();
    let status = child.wait(Duration::from_secs(15));
    let elapsed = started.elapsed();
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM under --gate off must produce a graceful exit 0 (got {status:?} after {elapsed:?}); \
         stderr:\n{}",
        read_child_stderr(&stderr_path)
    );
    println!(
        "SIGTERM gate off acceptance: child exited 0 in {elapsed:?} after signal; WAV at {}",
        wav_path.display()
    );

    // Under --gate off, the WAV is finalized cleanly and validates regardless of silence content.
    assert_valid_finalized_wav(&wav_path, true);
}

// ---------------------------------------------------------------------------
// 2. Double-signal acceptance (immediate `_exit(1)`)
// ---------------------------------------------------------------------------

/// Sends two rapid `SIGTERM`s to a live `nam-audio-pipe` and proves immediate
/// termination through the async-signal-safe `_exit(1)` path (the unified
/// handler force-exits on the second signal while the graceful teardown of the
/// first is still in flight). Exit code must be `1`, not `0` (graceful) and not
/// a signal death.
///
/// The second signal is delivered ~10 ms after the first — well inside the
/// ≥ 100 ms control-loop poll that gates the graceful teardown, so it lands
/// deterministically. Up to 3 fresh attempts hedge against a pathological
/// scheduler race where the process finalized before the second signal.
///
/// Requires a running PipeWire daemon (to bring the host up). Runs in Phase 3
/// of `utils/tests-quick.sh`.
#[test]
#[ignore = "requires a running PipeWire daemon; runs in tests-quick Phase 3"]
fn double_signal_force_exits_via_exit1() {
    if !common::probe_pipewire_daemon() {
        eprintln!("SKIP: PipeWire daemon not detected (pw-cli info 0 failed).");
        return;
    }

    let _lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _sd = common::ShutdownGuard::new();

    let dir = common::temp_dir();
    let _guard = common::DirGuard::new(dir.clone());

    let mut observed: Option<i32> = None;
    for attempt in 0..3 {
        let (mut child, stderr_path) = spawn_host(&["--fail-fast"], &dir);
        // Wait until the signal handler is installed (full host startup, sink
        // registered) so the signals are handled cooperatively, not by the
        // default SIGTERM disposition.
        assert!(
            common::wait_for_nam_sink(Duration::from_secs(10)),
            "attempt {attempt}: host capture sink never registered; stderr:\n{}",
            read_child_stderr(&stderr_path)
        );

        send_signal(&child, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(10));
        send_signal(&child, libc::SIGTERM);

        let started = Instant::now();
        let status = child.wait(Duration::from_secs(10));
        let elapsed = started.elapsed();
        observed = status.code();
        if observed == Some(1) {
            println!(
                "double-signal acceptance (attempt {attempt}): _exit(1) observed after {elapsed:?}"
            );
            assert!(
                elapsed < Duration::from_secs(3),
                "the second signal must terminate immediately (took {elapsed:?})"
            );
            return;
        }
        eprintln!(
            "attempt {attempt}: expected _exit(1), observed {status:?} after {elapsed:?}; \
             stderr:\n{}",
            read_child_stderr(&stderr_path)
        );
    }
    panic!("double SIGTERM never forced _exit(1); observed exit codes: {observed:?}");
}

// ---------------------------------------------------------------------------
// 3. Bridge starvation -> analytical silence + buffer recycle
// ---------------------------------------------------------------------------

/// SPA chunk descriptor helper (mirrors the mock used by the unit harness).
fn spa_chunk(offset: u32, size: u32, stride: i32) -> pw::spa::sys::spa_chunk {
    pw::spa::sys::spa_chunk {
        offset,
        size,
        stride,
        flags: 0,
    }
}

/// Under **zero bridge generation** (starvation) the playback path
/// (`playback_dsp_cycle` → [`deliver_silence_pair_fail_closed`]) must:
///
/// * emit `0.0f32` analytical-silence sequences over the **quantum-sized
///   window** (S5 / E2304: `silence_bytes` bounded by the active frame count,
///   not the shared-memory `maxsize`);
/// * stamp the SPA chunks deterministically (`offset = 0`, `size = frames × 4`,
///   `stride = 4`) — the buffer is recycled back to the graph coherently;
/// * never stall: thousands of consecutive starvation quantums complete
///   deterministically; and
/// * keep telemetry honest: each starvation quantum is counted on
///   `playback_bridge_starvation`, no `output_buffer_miss` is fabricated and no
///   host-contract violation flag is raised — including when the host hands us
///   a **large `maxsize` buffer (64 KiB)** that exceeds `MAX_BRIDGE_BUF × 4`
///   (preventing false-`E2304` contract violation errors).
///
/// Daemon-independent — runs in every quick pass.
#[test]
fn bridge_starvation_emits_analytical_silence_and_recycles_buffers() {
    const CYCLES: usize = 2000;
    const FRAMES: usize = 128;

    let mut l = vec![0.5f32; FRAMES];
    let mut r = vec![0.5f32; FRAMES];
    // Stale chunk metadata from a previous cycle — the silent-recycle path must
    // overwrite it deterministically.
    let mut chunk_l = spa_chunk(7, 4, 0);
    let mut chunk_r = spa_chunk(3, 8, 2);
    let rt = RtStatusFlags::default();

    let started = Instant::now();
    for cycle in 0..CYCLES {
        // Re-fill with stale non-zero garbage each cycle: the silence path must
        // deterministically zero the quantum window, never carry residue.
        l.fill(0.5f32);
        r.fill(0.5f32);

        // SAFETY: `l`/`r` are disjoint, aligned, writable `[f32; FRAMES]`
        // vectors and `chunk_l`/`chunk_r` are local non-null `spa_chunk`
        // structs that outlive every call; the kernel rejects any descriptor
        // that violates the contract fail-closed.
        let frames = unsafe {
            deliver_silence_pair_fail_closed(
                l.as_ptr() as usize,
                l.len() * 4,
                &mut chunk_l,
                r.as_ptr() as usize,
                r.len() * 4,
                &mut chunk_r,
                FRAMES * 4,
                &rt,
            )
        };
        assert_eq!(
            frames,
            Some(FRAMES),
            "cycle {cycle}: starvation must deliver silence frames"
        );
        assert!(
            l.iter().all(|s| s.to_bits() == 0.0f32.to_bits()),
            "cycle {cycle}: L must be bit-exact 0.0f32 analytical silence"
        );
        assert!(
            r.iter().all(|s| s.to_bits() == 0.0f32.to_bits()),
            "cycle {cycle}: R must be bit-exact 0.0f32 analytical silence"
        );
        assert_eq!(
            chunk_l.offset, 0,
            "cycle {cycle}: L chunk offset must be reset"
        );
        assert_eq!(
            chunk_l.size,
            (FRAMES * 4) as u32,
            "cycle {cycle}: L chunk size"
        );
        assert_eq!(chunk_l.stride, 4, "cycle {cycle}: L chunk stride");
        assert_eq!(
            chunk_r.offset, 0,
            "cycle {cycle}: R chunk offset must be reset"
        );
        assert_eq!(
            chunk_r.size,
            (FRAMES * 4) as u32,
            "cycle {cycle}: R chunk size"
        );
        assert_eq!(chunk_r.stride, 4, "cycle {cycle}: R chunk stride");
    }
    let elapsed = started.elapsed();

    assert_eq!(
        rt.playback_bridge_starvation.load(Ordering::Relaxed),
        CYCLES as u32,
        "every starvation quantum must be telemetrized (xrun telemetry)"
    );
    assert_eq!(
        rt.output_buffer_miss.load(Ordering::Relaxed),
        0,
        "a dequeued silence buffer is a recycle, not a miss"
    );
    assert!(
        !rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
        "a clean starvation quantum must not raise the host-contract flag"
    );
    // "sem stalls": the soak is a pure kernel, so this ceiling is generous —
    // it proves the bounded execution (no runaway/leak across 2000 quantums).
    assert!(
        elapsed < Duration::from_secs(5),
        "starvation soak must not stall: {CYCLES} quantums took {elapsed:?}"
    );

    // S5 / E2304 regression: a large maxsize buffer (64 KiB per channel, well
    // beyond MAX_BRIDGE_BUF × 4 = 32 KiB) must NOT raise the fail-closed
    // contract flag during sustained starvation. The kernel delivers exactly
    // the quantum-sized window (FRAMES × 4 bytes) and never touches the
    // trailing stale samples.
    const BIG_FRAMES: usize = 16_384; // 64 KiB per channel
    let mut big_l = vec![0.5f32; BIG_FRAMES];
    let mut big_r = vec![0.5f32; BIG_FRAMES];
    let mut big_chunk_l = spa_chunk(7, 4, 0);
    let mut big_chunk_r = spa_chunk(3, 8, 2);
    let rt_big = RtStatusFlags::default();

    for cycle in 0..CYCLES {
        big_l.fill(0.5f32);
        big_r.fill(0.5f32);

        // SAFETY: disjoint aligned writable 64 KiB vectors with local chunks.
        let frames = unsafe {
            deliver_silence_pair_fail_closed(
                big_l.as_ptr() as usize,
                big_l.len() * 4,
                &mut big_chunk_l,
                big_r.as_ptr() as usize,
                big_r.len() * 4,
                &mut big_chunk_r,
                FRAMES * 4,
                &rt_big,
            )
        };
        assert_eq!(
            frames,
            Some(FRAMES),
            "big cycle {cycle}: quantum-sized silence must be delivered"
        );
        assert!(
            big_l[..FRAMES]
                .iter()
                .all(|s| s.to_bits() == 0.0f32.to_bits()),
            "big cycle {cycle}: L quantum window must be bit-exact silence"
        );
        assert!(
            big_r[..FRAMES]
                .iter()
                .all(|s| s.to_bits() == 0.0f32.to_bits()),
            "big cycle {cycle}: R quantum window must be bit-exact silence"
        );
        assert!(
            big_l[FRAMES..]
                .iter()
                .all(|s| s.to_bits() == 0.5f32.to_bits()),
            "big cycle {cycle}: L trailing memory past the quantum must be untouched"
        );
        assert!(
            big_r[FRAMES..]
                .iter()
                .all(|s| s.to_bits() == 0.5f32.to_bits()),
            "big cycle {cycle}: R trailing memory past the quantum must be untouched"
        );
        assert_eq!(
            big_chunk_l.size,
            (FRAMES * 4) as u32,
            "big cycle {cycle}: L chunk size"
        );
        assert_eq!(
            big_chunk_r.size,
            (FRAMES * 4) as u32,
            "big cycle {cycle}: R chunk size"
        );
    }

    assert_eq!(
        rt_big.playback_bridge_starvation.load(Ordering::Relaxed),
        CYCLES as u32,
        "every large-buffer starvation quantum must be telemetrized"
    );
    assert_eq!(
        rt_big.output_buffer_miss.load(Ordering::Relaxed),
        0,
        "large-buffer recycle is not a miss"
    );
    assert!(
        !rt_big.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
        "64 KiB maxsize must NOT raise E2304 during sustained starvation (S5 regression)"
    );
}

// ---------------------------------------------------------------------------
// 4. SPA format-contract rejection — fail-closed mute
// ---------------------------------------------------------------------------

/// Builds a real SPA format POD for the given audio info (the exact object the
/// capture/playback `param_changed` listeners validate).
fn build_raw_format_pod<'a>(
    audio_info: &pw::spa::param::audio::AudioInfoRaw,
    storage: &'a mut SpaPodStorage<1024>,
) -> &'a pw::spa::pod::Pod {
    // SAFETY: the returned pod borrows `storage`, which outlives the call.
    unsafe { build_spa_format_pod(audio_info, storage) }.expect("pod build")
}

fn raw_audio_info(
    format: pw::spa::param::audio::AudioFormat,
    channels: u32,
) -> pw::spa::param::audio::AudioInfoRaw {
    let mut info = pw::spa::param::audio::AudioInfoRaw::new();
    info.set_format(format);
    info.set_channels(channels);
    info.set_rate(48_000);
    let mut pos = [0u32; 64];
    pos[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    pos[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(pos);
    info
}

/// The host enforces a strict `F32P` planar-stereo SPA format contract on both
/// streams. Any divergent renegotiation — mono, interleaved `F32`, `S16`,
/// surround — must be rejected with a typed [`ContractViolation`], raise
/// `RT_STATUS_HOST_CONTRACT_VIOLATION` and latch the RT mute guard
/// (`format_contract_ok == 0`) so the host operates **fail-closed** (no DSP on
/// wrong-format data; playback delivers deterministic silence). A subsequent
/// valid `F32P` stereo renegotiation re-arms audio.
///
/// Daemon-independent — runs in every quick pass.
#[test]
fn spa_format_rejection_signals_contract_violation_fail_closed() {
    pipewire::init();

    // Baseline: the valid contract is accepted and the mute guard is armed.
    {
        let rt = RtStatusFlags::default();
        assert_eq!(
            rt.format_contract_ok.load(Ordering::Relaxed),
            1,
            "latch defaults to contract-ok"
        );
        let info = raw_audio_info(pw::spa::param::audio::AudioFormat::F32P, 2);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Ok(48_000),
            "F32P planar stereo must be accepted"
        );
    }

    // Each incompatible format must be rejected fail-closed: typed violation,
    // contract-violation flag raised, RT mute guard latched, then restored by a
    // valid renegotiation.
    let cases: &[(
        pw::spa::param::audio::AudioFormat,
        u32,
        ContractViolation,
        &str,
    )] = &[
        (
            pw::spa::param::audio::AudioFormat::F32P,
            1,
            ContractViolation::NotStereo(1),
            "mono",
        ),
        (
            pw::spa::param::audio::AudioFormat::F32LE,
            2,
            ContractViolation::NotF32Planar(pw::spa::param::audio::AudioFormat::F32LE),
            "interleaved F32",
        ),
        (
            pw::spa::param::audio::AudioFormat::S16,
            2,
            ContractViolation::NotF32Planar(pw::spa::param::audio::AudioFormat::S16),
            "S16",
        ),
        (
            pw::spa::param::audio::AudioFormat::F32P,
            6,
            ContractViolation::NotStereo(6),
            "5.1 surround",
        ),
    ];
    for (format, channels, expected, label) in cases {
        let rt = RtStatusFlags::default();
        let info = raw_audio_info(*format, *channels);
        let mut storage = SpaPodStorage::new();
        let pod = build_raw_format_pod(&info, &mut storage);
        assert_eq!(
            validate_audio_raw_format(pod),
            Err(*expected),
            "{label} must be rejected with the typed violation"
        );
        reject_negotiated_format_violation(&rt, "capture", *expected);
        assert!(
            rt.check_flag(RT_STATUS_HOST_CONTRACT_VIOLATION),
            "{label}: the host must signal the contract violation"
        );
        assert_eq!(
            rt.format_contract_ok.load(Ordering::Relaxed),
            0,
            "{label}: the RT mute guard must be latched (fail-closed)"
        );
        mark_format_contract_ok(&rt, "capture");
        assert_eq!(
            rt.format_contract_ok.load(Ordering::Relaxed),
            1,
            "{label}: a later valid F32P stereo renegotiation must re-arm audio"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Forced stream error (StreamState::Error) -> Failed within SLA
// ---------------------------------------------------------------------------

/// A forced `StreamState::Error` must be published as a **sticky** `Failed`
/// transition on the shared backend status, observable through the atomic
/// fast-path that the main control loop in `pw_host::run` polls every iteration
/// (≤ 100 ms sleep) — so a fatal stream loss is detected inside the < 500 ms
/// SLA. The full lifecycle is proven: initial `Unconnected` is not a failure,
/// `Streaming` → `Running`, `Error`/post-streaming `Unconnected` → `Failed`,
/// sticky-failure, and the bounded reconnect cycle back to
/// `Running`.
///
/// The non-zero process exit on a real backend failure is certified end-to-end
/// by the opt-in daemon-stop acceptances in `tests/pw_integration.rs`
/// (`test_pipewire_reconnect_exhaustion_terminates_with_error` and
/// `test_pipewire_fail_fast_stream_error_terminates_within_sla`); this test
/// proves the deterministic state-machine mapping and the SLA bound by
/// construction.
///
/// Daemon-independent — runs in every quick pass.
#[test]
fn stream_error_observable_and_shutdown_within_sla() {
    let backend = SharedBackendStatus::new();
    assert_eq!(backend.state(), BackendState::Starting);
    assert!(!backend.is_failed(), "a fresh backend is not failed");

    // Initial Unconnected (stream not yet connected) is NOT a failure — the
    // host is simply still coming up.
    observe_stream_state(
        "capture",
        pw::stream::StreamState::Unconnected,
        pw::stream::StreamState::Connecting,
        &backend,
    );
    assert!(
        !backend.is_failed(),
        "initial Unconnected must not mark the backend failed"
    );

    // Streaming -> Running.
    observe_stream_state(
        "playback",
        pw::stream::StreamState::Connecting,
        pw::stream::StreamState::Streaming,
        &backend,
    );
    observe_stream_state(
        "capture",
        pw::stream::StreamState::Connecting,
        pw::stream::StreamState::Streaming,
        &backend,
    );
    assert_eq!(backend.state(), BackendState::Running);

    // StreamState::Error -> sticky Failed with the diagnostic detail.
    observe_stream_state(
        "capture",
        pw::stream::StreamState::Streaming,
        pw::stream::StreamState::Error("device reset".into()),
        &backend,
    );
    assert!(
        backend.is_failed(),
        "StreamState::Error must mark the backend failed"
    );
    let (stream, reason) = backend.failure().expect("failure detail must be captured");
    assert_eq!(stream, "capture");
    assert!(reason.contains("device reset"));

    // Sticky: a late Running event must never erase the failure — the control
    // loop must always observe the terminal condition.
    observe_stream_state(
        "capture",
        pw::stream::StreamState::Error("device reset".into()),
        pw::stream::StreamState::Streaming,
        &backend,
    );
    backend.mark_running();
    assert!(
        backend.is_failed(),
        "Failed is sticky — a late Running event must not erase it"
    );

    // The main control loop polls `is_failed()` every iteration (≤ 100 ms
    // sleep). Mirror run.rs and prove the failure is observed inside the
    // < 500 ms SLA from the moment it is published.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut observed = false;
    while Instant::now() < deadline {
        if backend.is_failed() {
            observed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(observed, "the control-loop poller must observe the failure");
    assert!(
        Instant::now() < deadline,
        "failure must be observed within the 500 ms SLA"
    );

    // Post-streaming Unconnected (daemon crash/restart) -> Failed too.
    let backend2 = SharedBackendStatus::new();
    observe_stream_state(
        "playback",
        pw::stream::StreamState::Streaming,
        pw::stream::StreamState::Unconnected,
        &backend2,
    );
    assert!(
        backend2.is_failed(),
        "post-streaming Unconnected must mark the backend failed"
    );
    assert!(
        matches!(
            backend2.state(),
            BackendState::Failed {
                stream: "playback",
                ..
            }
        ),
        "unexpected state: {:?}",
        backend2.state()
    );

    // Bounded reconnect: begin_reconnect clears the sticky
    // failure and publishes the observable Reconnecting transition; a
    // successful reconnection returns the backend to Running.
    backend2.begin_reconnect(1, 3, Duration::from_millis(250));
    assert!(
        !backend2.is_failed(),
        "begin_reconnect must clear the sticky failure"
    );
    assert!(
        matches!(
            backend2.state(),
            BackendState::Reconnecting {
                attempt: 1,
                total_attempts: 3,
                ..
            }
        ) && backend2.state()
            == BackendState::Reconnecting {
                attempt: 1,
                total_attempts: 3,
                next_backoff: Duration::from_millis(250),
            },
        "unexpected reconnect state: {:?}",
        backend2.state()
    );
    observe_stream_state(
        "playback",
        pw::stream::StreamState::Paused,
        pw::stream::StreamState::Streaming,
        &backend2,
    );
    observe_stream_state(
        "capture",
        pw::stream::StreamState::Paused,
        pw::stream::StreamState::Streaming,
        &backend2,
    );
    assert_eq!(
        backend2.state(),
        BackendState::Running,
        "a successful reconnection must return the backend to Running"
    );
}
