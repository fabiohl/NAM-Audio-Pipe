// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! PipeWire Pipeline Integration (End-to-End) Test
//!
//! Validates the full lifecycle of the PipeWire host: context initialization,
//! SPSC channel setup, gain parameter injection, and graceful shutdown.
//!
//! Requires a running PipeWire daemon (session or system). Without it, the test
//! is skipped by the `#[ignore]` attribute; `utils/tests-quick.sh` Phase 3
//! auto-detects the daemon via `pw-cli info`.

use nam_audio_pipe::standalone::cli;
use nam_audio_pipe::standalone::pw_host::{self, PipewireHostConfig, StreamStatusFlags};
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{self, GcOverflowBuffer, RtStatusFlags};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use rtrb::RingBuffer;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

mod common;

/// Minimum number of E2E samples in the measurement window so the log-linear
/// histogram carries a meaningful tail (the previous 8-sample window made
/// "p99" collapse onto the maximum).
const E2E_MEASURE_SAMPLES: u64 = 2000;

/// Minimum wall-clock measurement window (whichever is longer with the sample
/// target, so a coarse quantum still yields a real time series).
const E2E_MIN_WINDOW: Duration = Duration::from_secs(4);

/// Hard ceiling for the measurement window (covers 2000 samples at q=256).
const E2E_MAX_WINDOW: Duration = Duration::from_secs(15);

/// R-17: the script (`utils/tests-quick.sh` Phase 3) only executes
/// this test after `pw-cli info 0` succeeded. A `pw-cli` failure INSIDE the
/// test therefore means the daemon vanished mid-run or the script↔test
/// probes diverged — a silent `return` would let the script print
/// `LIVE_PW=RAN` with zero DSP executed. Fail closed instead: panic.
fn assert_daemon_probe_consistent() {
    if !common::probe_pipewire_daemon() {
        panic!(
            "R-17: pw-cli info 0 failed inside the test after the script probe \
             passed — daemon vanished or probes diverged. Refusing to emit \
             LIVE_PW=RAN without real DSP execution."
        );
    }
}

/// Tests the basic initialization and communication of the PipeWire pipeline.
///
/// This test simulates the full lifecycle of the engine:
/// 1. Creation of SPSC RingBuffers for commands and telemetry.
/// 2. Spawning the audio thread (host).
/// 3. Sending gain parameters via the control channel.
///
/// Measures end-to-end metrics for a given buffer size.
fn run_smoke_session(buffer_size: u32) -> (u64, u64, u64, u32, u32, u32, u32) {
    let (mut param_prod, param_cons) = RingBuffer::new(4);
    let (gc_prod, gc_cons) = RingBuffer::new(4);
    let (res_prod, res_cons) = RingBuffer::new(2);
    let (cs_prod, cs_cons) = RingBuffer::new(2);
    let (sl_prod, sl_cons) = RingBuffer::new(2);
    let (os_prod, os_cons) = RingBuffer::new(2);

    let gc_overflow = Arc::new(GcOverflowBuffer::new(64));
    let rt_status = Arc::new(RtStatusFlags::default());

    let rt_clone = rt_status.clone();
    let gc_overflow_clone = gc_overflow.clone();
    let sys = SystemSnapshot::capture();
    let stream_status = Arc::new(StreamStatusFlags::new());
    let stream_status_clone = stream_status.clone();

    spsc::SHUTDOWN.store(false, Ordering::Relaxed);

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
                buffer_size,
                cabsim_partition: cli::CABSIM_PARTITION_DEFAULT,
                sys,
                ir_raw_samples: None,
                ir_source_rate: 0,
                full_wavenet_model_l: None,
                full_wavenet_model_r: None,
                has_model_r: false,
                slimmable_producer: sl_prod,
                os_producer: os_prod,
                oversample: OversampleFactor::Off,
                cpu_receipt: common::deterministic_cpu_receipt(),
                fail_fast: true,
                reconnect_timeout: None,
                gate_config: cli::GateConfig::Off,
                stream_status: Some(stream_status_clone),
                recording_status: None,
                banner: Default::default(),
            },
            gc_cons,
            sl_cons,
            os_cons,
            None,
            None,
        )
    });

    thread::sleep(Duration::from_millis(50));
    let _ = param_prod.push(neural_amp_modeler_rs::common::spsc::ParamPayload::InputGain(2.5));
    let _ = param_prod.push(neural_amp_modeler_rs::common::spsc::ParamPayload::OutputGain(-1.0));

    let dir = common::temp_dir();
    let _dir_guard = common::DirGuard::new(dir.clone());
    let mut tone = ToneDriver::new(&dir);
    let attached = tone.wait_for_sink_and_attach(Duration::from_secs(5));
    assert!(
        attached,
        "ToneDriver could not attach to NAM capture sink for buffer_size={}",
        buffer_size
    );

    // Wait for the pipeline to reach steady streaming state (at least 2 E2E cycles recorded)
    let warmup_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < warmup_deadline {
        if stream_status.e2e_hist.total_count() >= 2 {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }
    assert!(
        stream_status.e2e_hist.total_count() >= 2,
        "pipeline did not reach steady streaming state for buffer_size={buffer_size}"
    );

    // Bring-up transient snapshot: the single expected discard/starvation at the
    // first quantum is counted separately (`bringup_discard`) from the
    // steady-state drops, which must be exactly zero.
    let bringup_starvation = stream_status
        .playback_bridge_starvation
        .load(Ordering::Relaxed);
    let bringup_discard = stream_status.bridge_dropped_frames.load(Ordering::Relaxed);
    stream_status.e2e_hist.reset();
    #[cfg(any(feature = "testing", feature = "heap-audit"))]
    stream_status.e2e_exact.reset();

    // Measurement window: at least E2E_MEASURE_SAMPLES samples AND at least
    // E2E_MIN_WINDOW of wall clock (bounded by E2E_MAX_WINDOW).
    let measure_start = Instant::now();
    loop {
        let samples = stream_status.e2e_hist.total_count();
        if samples >= E2E_MEASURE_SAMPLES && measure_start.elapsed() >= E2E_MIN_WINDOW {
            break;
        }
        if measure_start.elapsed() >= E2E_MAX_WINDOW {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }

    // Sample steady-state metrics while audio is actively streaming
    let last_n_samples = rt_status.last_n_samples.load(Ordering::Relaxed);
    let current_starvation = stream_status
        .playback_bridge_starvation
        .load(Ordering::Relaxed);
    let current_drops = stream_status.bridge_dropped_frames.load(Ordering::Relaxed);
    let starvation = current_starvation.saturating_sub(bringup_starvation);
    let dropped_frames = current_drops.saturating_sub(bringup_discard);

    let samples = stream_status.e2e_hist.total_count();
    let p50_ns = stream_status.e2e_hist.get_percentile(0.50);
    let p99_ns = stream_status.e2e_hist.get_percentile(0.99);
    let p999_ns = stream_status.e2e_hist.get_percentile(0.999);
    let max_ns = stream_status.e2e_hist.get_exact_max();

    // Exact-sample percentiles from the testing-only reservoir. The log-linear
    // histogram quantises the tail, so on a bucket boundary `p50 == p99 == p999`;
    // these complement the bucketed fields (parsers keep working) and must stay
    // distinct when the samples differ. They never feed the gate above. Reporting
    // `0` without the `testing`/`heap-audit` reservoir keeps the receipt shape
    // (and the `--no-default-features` compile) intact; this daemon test is
    // `#[ignore]`d, so that build never reaches here in practice.
    #[cfg(any(feature = "testing", feature = "heap-audit"))]
    let (p50_exact_ns, p99_exact_ns, p999_exact_ns) = (
        stream_status.e2e_exact.percentile_exact(0.50),
        stream_status.e2e_exact.percentile_exact(0.99),
        stream_status.e2e_exact.percentile_exact(0.999),
    );
    #[cfg(not(any(feature = "testing", feature = "heap-audit")))]
    let (p50_exact_ns, p99_exact_ns, p999_exact_ns) = (0u64, 0u64, 0u64);

    spsc::SHUTDOWN.store(true, Ordering::Relaxed);

    let host_result = match pw_thread.join() {
        Ok(result) => result,
        Err(_) => {
            panic!("The PipeWire thread suffered a fatal panic for buffer_size={buffer_size}!")
        }
    };

    if let Err(e) = host_result {
        panic!(
            "run_pipewire_host failed while the PipeWire daemon is up for buffer_size={buffer_size}: {e:#}"
        );
    }

    assert!(
        last_n_samples > 0,
        "no audio quantum was processed (last_n_samples == 0) for buffer_size={buffer_size}; \
         LIVE_PW must reflect real DSP execution, not merely daemon presence"
    );

    let mut rate = stream_status
        .playback_negotiated_rate
        .load(Ordering::Relaxed);
    if rate == 0 {
        rate = stream_status
            .capture_negotiated_rate
            .load(Ordering::Relaxed);
    }
    if rate == 0 {
        rate = 48000;
    }

    let quantum = last_n_samples;
    let quantum_ns = (quantum as u64) * 1_000_000_000 / (rate as u64);

    println!(
        "E2E_METRICS buffer_size={buffer_size} samples={samples} p50_ns={p50_ns} p99_ns={p99_ns} \
         p999_ns={p999_ns} max_ns={max_ns} p50_exact_ns={p50_exact_ns} p99_exact_ns={p99_exact_ns} \
         p999_exact_ns={p999_exact_ns} starvation={starvation} \
         bringup_discard={bringup_discard} steady_dropped={dropped_frames} quantum={quantum} rate={rate}"
    );

    // Steady-state contract: after warm-up the bridge must not starve nor
    // discard. Bring-up transients are reported separately, never hidden.
    assert_eq!(
        starvation, 0,
        "zero steady-state starvation expected for buffer_size={buffer_size}"
    );
    assert_eq!(
        dropped_frames, 0,
        "steady-state dropped frames must be zero for buffer_size={buffer_size} \
         (bring-up was {bringup_discard})"
    );

    let env = common::rt_env::RtCalibration::probe();
    let force_legacy = std::env::var("NAM_E2E_LEGACY_GATE").as_deref() == Ok("1");

    if env.calibrated() && !force_legacy {
        // Tight gate relative to the host quantum: the bridge is one quantum
        // deep, so p99 must stay under 1.25·q + 100 µs and the maximum under
        // 1.5·q (F-PR-07).
        let p99_limit = quantum_ns + quantum_ns / 4 + 100_000;
        let max_limit = quantum_ns + quantum_ns / 2;
        assert!(
            p99_ns <= p99_limit,
            "p99 E2E ({p99_ns} ns) exceeded 1.25·q + 100 µs ({p99_limit} ns) for buffer_size={buffer_size}"
        );
        assert!(
            max_ns <= max_limit,
            "max E2E ({max_ns} ns) exceeded 1.5·q ({max_limit} ns) for buffer_size={buffer_size}"
        );
        println!(
            "TEST_RESULT[e2e_tight_gate]=PASS buffer_size={buffer_size} p99_ns={p99_ns} \
             max_ns={max_ns} quantum_ns={quantum_ns} {}",
            env.format_env()
        );
    } else {
        let reason = if force_legacy {
            "forced_legacy_gate"
        } else {
            "non_isolated_env"
        };
        // Legacy permissive threshold as an explicit fallback on uncertified hosts.
        let max_allowed_p99 = 2 * quantum_ns + 15_000_000;
        assert!(
            p99_ns <= max_allowed_p99,
            "p99 e2e ({p99_ns} ns) exceeded 2 quantums + hw overhead ({max_allowed_p99} ns) for buffer_size={buffer_size}"
        );
        println!(
            "TEST_RESULT[e2e_tight_gate]=GAP:{reason} buffer_size={buffer_size} p99_ns={p99_ns} \
             max_ns={max_ns} quantum_ns={quantum_ns}"
        );
    }

    (
        p50_ns,
        p99_ns,
        max_ns,
        starvation,
        dropped_frames,
        quantum,
        rate,
    )
}

/// Tests the basic initialization and communication of the PipeWire pipeline across
/// multiple buffer sizes (64, 128, 256), asserting zero starvation and emitting E2E_METRICS.
#[test]
#[ignore = "requires a running PipeWire daemon (session or system); auto-detected by utils/tests-quick.sh Phase 3"]
fn test_pipewire_integration() {
    // R-17: fail-closed divergence check — never a silent skip here (the
    // script probe already gated on the daemon).
    assert_daemon_probe_consistent();

    // Serialized with the opt-in daemon-bounce tests (they manipulate the
    // system daemon) and SHUTDOWN-guarded so the flag is pristine afterwards.
    let _daemon_lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _shutdown = common::ShutdownGuard::new();

    pipewire::init();
    println!("PipeWire initialized successfully.");

    for &buf_size in &[64, 128, 256] {
        run_smoke_session(buf_size);
        // Bounded graph settling delay between consecutive sessions
        thread::sleep(Duration::from_millis(100));
    }
}

/// Helper to spawn a silent tone against the system daemon.
fn spawn_silent_tone(wav: &std::path::Path) -> Option<std::process::Child> {
    std::process::Command::new("pw-play")
        .args(["--target", "NAM-Audio-Pipe-input", "--volume", "0"])
        .arg(wav)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

/// Best-effort termination of the tone process.
fn kill_tone(mut tone: Option<std::process::Child>) {
    if let Some(child) = tone.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// RAII silent-tone driver for the PipeWire smoke acceptances against the system daemon.
struct ToneDriver {
    wav_path: std::path::PathBuf,
    child: Option<std::process::Child>,
}

impl ToneDriver {
    /// Generates the silent WAV inside `dir`.
    fn new(dir: &std::path::Path) -> Self {
        let wav_path = dir.join("silence.wav");
        common::generate_silent_wav(&wav_path, 30);
        Self {
            wav_path,
            child: None,
        }
    }

    /// Waits for the NAM capture sink to register in the graph and attaches
    /// the tone into it. Returns whether the sink appeared in time.
    fn wait_for_sink_and_attach(&mut self, timeout: Duration) -> bool {
        if !common::wait_for_nam_sink(timeout) {
            return false;
        }
        self.attach();
        true
    }

    /// (Re)attaches the tone if not currently running.
    fn attach(&mut self) {
        if self.child.is_none() {
            self.child = spawn_silent_tone(&self.wav_path);
        }
    }

    /// Stops the tone (best-effort).
    fn detach(&mut self) {
        kill_tone(self.child.take());
    }
}

impl Drop for ToneDriver {
    fn drop(&mut self) {
        self.detach();
    }
}

/// RAII guard that kills and reaps a child process on drop (even on panic).
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// RAII silent-tone driver connected to the private PipeWire daemon.
struct PrivateToneDriver {
    runtime_dir: std::path::PathBuf,
    wav_path: std::path::PathBuf,
    child: Option<std::process::Child>,
}

impl PrivateToneDriver {
    fn new(runtime_dir: &std::path::Path, dir: &std::path::Path) -> Self {
        let wav_path = dir.join("silence.wav");
        common::generate_silent_wav(&wav_path, 30);
        Self {
            runtime_dir: runtime_dir.to_path_buf(),
            wav_path,
            child: None,
        }
    }

    fn apply_envs(&self, cmd: &mut std::process::Command) {
        cmd.env("PIPEWIRE_RUNTIME_DIR", &self.runtime_dir);
        cmd.env("XDG_RUNTIME_DIR", &self.runtime_dir);
        cmd.env("PIPEWIRE_REMOTE", "pipewire-0");
    }

    fn wait_for_sink_and_attach(&mut self, target_pid: Option<u32>, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut cmd = std::process::Command::new("pw-dump");
            self.apply_envs(&mut cmd);
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            if cmd
                .output()
                .ok()
                .is_some_and(|out| common::parse_nam_sink_in_dump(&out.stdout, target_pid))
            {
                self.attach();
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn attach(&mut self) {
        if self.child.is_none() {
            let mut cmd = std::process::Command::new("pw-play");
            self.apply_envs(&mut cmd);
            self.child = cmd
                .args(["--target", "NAM-Audio-Pipe-input", "--volume", "0"])
                .arg(&self.wav_path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok();
        }
    }

    fn detach(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for PrivateToneDriver {
    fn drop(&mut self) {
        self.detach();
    }
}

/// Smoke test: verifies that the private, isolated PipeWire daemon starts up,
/// provides a working UNIX socket, and shuts down cleanly upon Drop without
/// leaving temporary files or orphan processes.
#[test]
fn private_pw_starts_and_dies_cleanly() {
    let pw = match common::private_pw::PrivatePipewire::start() {
        Ok(pw) => pw,
        Err(common::private_pw::PrivatePwError::BinaryMissing) => {
            println!("TEST_RESULT[pw_private_daemon]=GAP:pipewire_binary_missing");
            println!("TEST_RESULT[private_pw_smoke]=GAP:pipewire_binary_missing");
            eprintln!("SKIP: pipewire binary is missing");
            return;
        }
        Err(e) => panic!("failed to start private pipewire: {e:?}"),
    };

    let dir = pw.dir().to_path_buf();
    let socket = pw.socket_path();
    assert!(
        dir.is_dir(),
        "tempdir must exist while private pipewire is active"
    );
    assert!(
        socket.exists(),
        "socket must exist while private pipewire is active"
    );

    drop(pw);

    assert!(!socket.exists(), "socket must be cleaned up on drop");
    assert!(!dir.exists(), "tempdir must be removed on drop");

    println!("TEST_RESULT[pw_private_daemon]=PASS");
    println!("TEST_RESULT[private_pw_smoke]=PASS");
}

/// Acceptance test: a momentary PipeWire daemon restart must be recovered by
/// the bounded reconnect cycle with the internal state intact.
///
/// Runs entirely against a private, isolated PipeWire daemon without touching
/// the host user's PipeWire session.
#[test]
fn test_pipewire_bounded_reconnect_recovers_audio_after_daemon_restart() {
    let mut pw = match common::private_pw::PrivatePipewire::start() {
        Ok(pw) => pw,
        Err(common::private_pw::PrivatePwError::BinaryMissing) => {
            println!("TEST_RESULT[daemon_bounce_reconnect]=GAP:pipewire_binary_missing");
            println!("TEST_RESULT[pw_private_daemon]=GAP:pipewire_binary_missing");
            eprintln!("SKIP: pipewire binary is missing");
            return;
        }
        Err(e) => panic!("failed to start private pipewire: {e:?}"),
    };

    if !common::pw_play_available() {
        println!("TEST_RESULT[daemon_bounce_reconnect]=GAP:pw_play_unavailable");
        eprintln!("SKIP: pw-play unavailable; cannot drive the graph deterministically.");
        return;
    }

    let _daemon_lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let dir = common::temp_dir();
    let _dir_guard = common::DirGuard::new(dir.clone());
    let mut tone = PrivateToneDriver::new(pw.dir(), &dir);

    let stderr_path = dir.join("reconnect-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr log");

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_nam-audio-pipe"));
    pw.apply_envs(&mut cmd);
    cmd.arg("--buffer-size")
        .arg("128")
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file));
    let child = cmd.spawn().expect("failed to spawn nam-audio-pipe");
    let mut child = ChildGuard(child);
    let child_pid = child.0.id();

    // Phase 1: wait for the host's capture sink node, attach silent tone into it
    assert!(
        tone.wait_for_sink_and_attach(Some(child_pid), Duration::from_secs(5)),
        "host capture sink never registered in the graph"
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "child exited prematurely"
    );

    // Phase 2: bounce the private daemon while the session is live
    pw.kill_hard();
    std::thread::sleep(Duration::from_millis(200));
    pw.restart()
        .expect("failed to restart private pipewire daemon");

    // Phase 3: the tone process died with the daemon. Wait for bounded reconnect
    // to re-register the fresh capture sink in the new daemon's graph (first attempt
    // 250 ms backoff + settling, well within the per-episode budget).
    assert!(
        pw.wait_for_nam_sink_pid(child_pid, Duration::from_secs(10)),
        "host never re-registered its capture sink after daemon bounce"
    );

    // Re-attach tone and verify session continues
    tone.attach();
    std::thread::sleep(Duration::from_millis(1000));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "child died after reconnect"
    );

    // Orderly cooperative shutdown via SIGTERM
    // SAFETY: `child_pid` is the live host child tracked by this test and no memory
    // is dereferenced; the child is reaped by the wait loop below.
    unsafe {
        libc::kill(child_pid as libc::pid_t, libc::SIGTERM);
    }
    let started = Instant::now();
    let deadline = started + Duration::from_secs(5);
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "child did not exit on SIGTERM within 5s"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };
    assert!(
        status.success(),
        "recovered host must exit 0 on SIGTERM, got {status:?}"
    );
    println!(
        "Bounded reconnect recovered daemon bounce: host reconnected and exited 0 cleanly after {:?}",
        started.elapsed()
    );
    println!("TEST_RESULT[daemon_bounce_reconnect]=PASS");
}

/// Acceptance test: with the daemon inaccessible, the bounded reconnect cycle
/// must exhaust its retry budget and terminate the host cleanly with an error
/// (fail-fast fallback) — never spin forever.
///
/// Runs entirely against a private, isolated PipeWire daemon without touching
/// the host user's PipeWire session.
#[test]
fn test_pipewire_reconnect_exhaustion_terminates_with_error() {
    let mut pw = match common::private_pw::PrivatePipewire::start() {
        Ok(pw) => pw,
        Err(common::private_pw::PrivatePwError::BinaryMissing) => {
            println!("TEST_RESULT[daemon_bounce_exhaustion]=GAP:pipewire_binary_missing");
            println!("TEST_RESULT[pw_private_daemon]=GAP:pipewire_binary_missing");
            eprintln!("SKIP: pipewire binary is missing");
            return;
        }
        Err(e) => panic!("failed to start private pipewire: {e:?}"),
    };

    if !common::pw_play_available() {
        println!("TEST_RESULT[daemon_bounce_exhaustion]=GAP:pw_play_unavailable");
        eprintln!("SKIP: pw-play unavailable; cannot drive the graph deterministically.");
        return;
    }

    let _daemon_lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let dir = common::temp_dir();
    let _dir_guard = common::DirGuard::new(dir.clone());
    let mut tone = PrivateToneDriver::new(pw.dir(), &dir);

    let stderr_path = dir.join("exhaustion-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr log");

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_nam-audio-pipe"));
    pw.apply_envs(&mut cmd);
    cmd.arg("--buffer-size")
        .arg("128")
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file));
    let child = cmd.spawn().expect("failed to spawn nam-audio-pipe");
    let mut child = ChildGuard(child);
    let child_pid = child.0.id();

    // Drive a healthy session first
    assert!(
        tone.wait_for_sink_and_attach(Some(child_pid), Duration::from_secs(5)),
        "host capture sink never registered in the graph"
    );
    std::thread::sleep(Duration::from_millis(1500));

    // Stop the private daemon and keep it down: retries must fail and budget must be exhausted
    pw.kill_hard();

    let started = Instant::now();
    // The per-episode backoff budget is ~11.75 s (6 attempts, 250 ms → 4 s);
    // allow generous margin for detection latency and each failed handshake.
    let deadline = started + Duration::from_secs(20);
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "host did not terminate after reconnect budget was exhausted"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };

    assert!(
        !status.success(),
        "an inaccessible daemon must exhaust retries and exit non-zero, got {status:?}"
    );
    println!(
        "Exhaustion acceptance passed: child exited {status:?} after {:?}",
        started.elapsed()
    );
    println!("TEST_RESULT[daemon_bounce_exhaustion]=PASS");
}

/// Acceptance test: with `--fail-fast`, a forced backend failure (the stream
/// observing a post-streaming disconnect when the daemon is killed) must tear the
/// host down and exit with a non-zero code inside the SLA (< 5 s).
///
/// Runs entirely against a private, isolated PipeWire daemon without touching
/// the host user's PipeWire session.
#[test]
fn test_pipewire_fail_fast_stream_error_terminates_within_sla() {
    let mut pw = match common::private_pw::PrivatePipewire::start() {
        Ok(pw) => pw,
        Err(common::private_pw::PrivatePwError::BinaryMissing) => {
            println!("TEST_RESULT[daemon_bounce_failfast]=GAP:pipewire_binary_missing");
            println!("TEST_RESULT[pw_private_daemon]=GAP:pipewire_binary_missing");
            eprintln!("SKIP: pipewire binary is missing");
            return;
        }
        Err(e) => panic!("failed to start private pipewire: {e:?}"),
    };

    if !common::pw_play_available() {
        println!("TEST_RESULT[daemon_bounce_failfast]=GAP:pw_play_unavailable");
        eprintln!("SKIP: pw-play unavailable; cannot drive the graph deterministically.");
        return;
    }

    let _daemon_lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let dir = common::temp_dir();
    let _dir_guard = common::DirGuard::new(dir.clone());
    let mut tone = PrivateToneDriver::new(pw.dir(), &dir);

    let stderr_path = dir.join("fail-fast-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create child stderr log");

    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_nam-audio-pipe"));
    pw.apply_envs(&mut cmd);
    cmd.arg("--fail-fast")
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file));
    let child = cmd
        .spawn()
        .expect("failed to spawn nam-audio-pipe --fail-fast");
    let mut child = ChildGuard(child);
    let child_pid = child.0.id();

    // Drive a healthy session first
    assert!(
        tone.wait_for_sink_and_attach(Some(child_pid), Duration::from_secs(5)),
        "host capture sink never registered / tone never attached"
    );
    std::thread::sleep(Duration::from_millis(1500));

    // Force backend failure by killing the private daemon
    let stop_start = Instant::now();
    pw.kill_hard();

    let deadline = stop_start + Duration::from_secs(5);
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "fail-fast child did not exit within the SLA"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };

    assert!(
        !status.success(),
        "a forced backend failure with --fail-fast must exit non-zero, got {status:?}"
    );
    assert!(
        stop_start.elapsed() < Duration::from_secs(5),
        "fail-fast shutdown must complete within the SLA (took {:?})",
        stop_start.elapsed()
    );
    println!(
        "Fail-fast stream-error acceptance passed: child exited {status:?} after {:?}",
        stop_start.elapsed()
    );
    println!("TEST_RESULT[daemon_bounce_failfast]=PASS");
}
