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
use nam_audio_pipe::standalone::pw_host::{self, PipewireHostConfig};
use neural_amp_modeler_rs::common::diagnostics::SystemSnapshot;
use neural_amp_modeler_rs::common::spsc::{self, GcOverflowBuffer, RtStatusFlags};
use neural_amp_modeler_rs::dsp::oversample::OversampleFactor;
use rtrb::RingBuffer;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

mod common;

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
/// 4. Shutdown signaled via atomic flag.
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
                // Reconnect disabled in the deterministic integration harness:
                // the daemon probe already guarantees it is up, so any backend
                // failure is a defect that must surface immediately.
                fail_fast: true,
                gate_config: cli::GateConfig::default_on(),
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

    // Drive the graph deterministically when `pw-play` is available: a silent
    // tone into the NAM capture sink keeps the capture node scheduled (a sink
    // without an active stream may never process a quantum). Without pw-play
    // the wait below falls back to the graph's own scheduling.
    let dir = common::temp_dir();
    let _dir_guard = common::DirGuard::new(dir.clone());
    let mut tone = ToneDriver::new(&dir);
    let _ = tone.wait_for_sink_and_attach(Duration::from_secs(5));

    // Wait (bounded) for the RT callback to observe at least one audio quantum.
    // `last_n_samples > 0` proves the capture stream actually processed a buffer
    // — the daemon probe alone is not evidence of DSP execution.
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

    let host_result = match pw_thread.join() {
        Ok(result) => result,
        Err(_) => panic!("The PipeWire thread suffered a fatal panic!"),
    };

    // Fail-closed: the daemon probe confirmed PipeWire is up, so an `Err` from
    // the host is a defect — never a benign "possible daemon absence".
    if let Err(e) = host_result {
        panic!("run_pipewire_host failed while the PipeWire daemon is up: {e:#}");
    }

    assert!(
        last_n_samples > 0,
        "no audio quantum was processed (last_n_samples == 0); \
         LIVE_PW must reflect real DSP execution, not merely daemon presence"
    );

    println!(
        "Integration test completed: host ran, {} samples processed in the last quantum.",
        last_n_samples
    )
}

/// Opt-in acceptance: a momentary PipeWire daemon restart
/// must be recovered by the bounded reconnect cycle with the internal state
/// (models/IRs/recording) intact.
///
/// This test **restarts the user's PipeWire daemon** (`systemctl --user restart
/// pipewire`) and is therefore fully disruptive: it only runs when the operator
/// explicitly opts in via `NAM_DAEMON_BOUNCE_TEST=1` (never in the default or
/// quick test loops).
///
/// To make the graph schedule the NAM capture node deterministically, a silent
/// WAV is played into the `NAM-Audio-Pipe-input` sink for the whole acceptance
/// (a silent stream still produces real quantums and advances the stream clock
/// — without any audible tone reaching the hardware). The daemon restart
/// happens *during* the active session; the host must observe the disconnect,
/// re-instantiate its streams inside the bounded backoff, and resume DSP —
/// proven by the fresh capture sink re-registering in the graph and the
/// reconnected stream clock advancing again.
#[test]
#[ignore = "opt-in disruptive test: restarts the user's PipeWire daemon; run with NAM_DAEMON_BOUNCE_TEST=1"]
fn test_pipewire_bounded_reconnect_recovers_audio_after_daemon_restart() {
    if std::env::var("NAM_DAEMON_BOUNCE_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: NAM_DAEMON_BOUNCE_TEST != 1 (disruptive daemon-bounce acceptance).");
        return;
    }
    if !common::pw_play_available() {
        eprintln!("SKIP: pw-play unavailable; cannot drive the graph deterministically.");
        return;
    }

    common::init_test_logger();
    // Serializes against the exhaustion acceptance: both tests manipulate the
    // system PipeWire daemon and must never run concurrently.
    let _daemon_lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _shutdown = common::ShutdownGuard::new();
    assert_daemon_probe_consistent();

    // Silent 30 s stereo tone that keeps the NAM capture sink scheduled.
    let dir = common::temp_dir();
    let _dir_guard = common::DirGuard::new(dir.clone());
    let mut tone = ToneDriver::new(&dir);

    pipewire::init();

    let (_param_prod, param_cons) = RingBuffer::new(4);
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
                // Reconnect ENABLED: this is exactly what the bounce exercises.
                fail_fast: false,
                gate_config: cli::GateConfig::default_on(),
            },
            gc_cons,
            sl_cons,
            os_cons,
            None,
            None,
        )
    });

    // Phase 1: wait for the host's capture sink node, attach the silent tone
    // into it and wait for a healthy audio session (stream clock advancing).
    assert!(
        tone.wait_for_sink_and_attach(Duration::from_secs(5)),
        "host capture sink never registered in the graph"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut baseline_ticks = 0u64;
    while std::time::Instant::now() < deadline {
        baseline_ticks = rt_status.capture_host_ticks.load(Ordering::Relaxed);
        if baseline_ticks > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        baseline_ticks > 0,
        "audio session never started before the daemon bounce"
    );

    // Phase 2: bounce the daemon while the session is live. The guard restores
    // the full group (including a possibly-crashed wireplumber) on any exit.
    let bounce = std::process::Command::new("systemctl")
        .args(["--user", "restart", "pipewire"])
        .status()
        .expect("failed to spawn systemctl --user restart pipewire");
    assert!(
        bounce.success(),
        "systemctl --user restart pipewire failed — cannot drive the acceptance"
    );
    let _restore_on_drop = PipewireRestartGuard;

    // Phase 3: the tone process dies with the daemon. Wait for the bounded
    // reconnect to re-register the fresh capture sink (≤ 1.75 s backoff budget
    // plus graph settling; 20 s window), re-attach the tone, and prove DSP
    // resumed. The reconnected stream's clock restarts from ~0, so the resume
    // proof is *any change* of the published stream clock to a positive value
    // (the old frozen value is discarded once the fresh callback writes).
    assert!(
        common::wait_for_nam_sink(Duration::from_secs(20)),
        "host never re-registered its capture sink after the daemon bounce"
    );
    let resumption_deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut last_ticks = rt_status.capture_host_ticks.load(Ordering::Relaxed);
    loop {
        tone.attach();
        let now_ticks = rt_status.capture_host_ticks.load(Ordering::Relaxed);
        if now_ticks != last_ticks && now_ticks > 0 {
            // The fresh stream's clock changed to a positive value: real DSP
            // resumed with the internal state (models/IRs/recording) intact.
            println!(
                "Bounded reconnect recovered the daemon bounce: stream clock advanced \
                 ({last_ticks} -> {now_ticks} ticks), state survived."
            );
            tone.detach();
            spsc::SHUTDOWN.store(true, Ordering::Relaxed);
            let host_result = pw_thread
                .join()
                .expect("the PipeWire host thread suffered a fatal panic");
            assert!(
                host_result.is_ok(),
                "recovered host must shut down cleanly, got {host_result:?}"
            );
            return;
        }
        last_ticks = now_ticks;
        assert!(
            std::time::Instant::now() < resumption_deadline,
            "bounded reconnect did not resume audio within the acceptance window"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Spawns `pw-play` playing `wav` into the NAM capture sink (silent stream,
/// volume 0) so the graph deterministically schedules the capture node.
///
/// ⚠️ AVISO: estes helpers dependem do gate permanecer FECHADO para manter o grafo
/// agendado sem abrir. Se algum teste futuro reusá-los sob `--gate off`, o
/// comportamento estrutural muda (o "silêncio" passa a ser processado como sinal real).
/// Não usar com o gate desativado (`cli::GateConfig::Off`).
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

/// RAII silent-tone driver for the PipeWire acceptances.
///
/// The NAM capture sink only processes while a client streams into it, so the
/// graph scheduling of the capture node is non-deterministic without an active
/// stream. This driver plays a silent WAV into `NAM-Audio-Pipe-input` — a
/// silent stream still produces real quantums and advances the stream clock,
/// with no audible tone reaching the hardware. Re-attachable after a daemon
/// restart (the tone process dies with the daemon).
///
/// ⚠️ AVISO: estes helpers dependem do gate permanecer FECHADO para manter o grafo
/// agendado sem abrir. Se algum teste futuro reusá-los sob `--gate off`, o
/// comportamento estrutural muda (o "silêncio" passa a ser processado como sinal real).
/// Não usar com o gate desativado (`cli::GateConfig::Off`).
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
        kill_tone(self.child.take());
    }
}

/// Restarts the user's PipeWire daemon group (best-effort) and waits until the
/// audio graph is functional again.
///
/// Stopping the daemon abruptly can crash the `wireplumber` session manager (a
/// known GLib `invalid_closure_notify` bug on some builds), and starting it
/// *before* the daemon is stable makes it crash again. Recovery is therefore
/// staged: (1) reset failed-unit state (the "start request repeated too
/// quickly" trap) and restart the daemon + socket + pulse bridge, waiting for
/// the daemon to become reachable; (2) only then start `wireplumber` and wait
/// for a registered `Audio/Sink` node, retrying once if it crashes.
fn restore_pipewire_group() {
    // Stage 1: daemon reachable.
    for _round in 0..2 {
        let _ = std::process::Command::new("systemctl")
            .args([
                "--user",
                "reset-failed",
                "pipewire",
                "pipewire-pulse",
                "wireplumber",
                "pipewire.socket",
                "pipewire-pulse.socket",
            ])
            .status();
        let _ = std::process::Command::new("systemctl")
            .args([
                "--user",
                "restart",
                "pipewire.socket",
                "pipewire",
                "pipewire-pulse.socket",
                "pipewire-pulse",
            ])
            .status();
        if common::wait_for_pipewire_daemon(Duration::from_secs(10)) {
            break;
        }
    }
    // Stage 2: session manager after the daemon is stable, then the graph.
    for _round in 0..2 {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "reset-failed", "wireplumber"])
            .status();
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "start", "wireplumber"])
            .status();
        if common::wait_for_audio_graph(Duration::from_secs(15)) {
            return;
        }
    }
}

/// Restarts the user's PipeWire daemon on drop (best-effort), even on panic —
/// the safety net that makes the disruptive opt-in tests acceptable.
struct PipewireRestartGuard;

impl Drop for PipewireRestartGuard {
    fn drop(&mut self) {
        restore_pipewire_group();
    }
}

/// Opt-in acceptance: with the daemon inaccessible, the
/// bounded reconnect cycle must exhaust its retry budget and terminate the host
/// cleanly with an error (fail-fast fallback) — never spin forever.
///
/// Disruptive like the bounce test: it stops the user's PipeWire daemon while
/// the host runs and only restarts it via the RAII guard. Runs only with
/// `NAM_DAEMON_BOUNCE_TEST=1`.
#[test]
#[ignore = "opt-in disruptive test: stops and restarts the user's PipeWire daemon; run with NAM_DAEMON_BOUNCE_TEST=1"]
fn test_pipewire_reconnect_exhaustion_terminates_with_error() {
    if std::env::var("NAM_DAEMON_BOUNCE_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: NAM_DAEMON_BOUNCE_TEST != 1 (disruptive exhaustion acceptance).");
        return;
    }
    if !common::pw_play_available() {
        eprintln!("SKIP: pw-play unavailable; cannot drive the graph deterministically.");
        return;
    }

    common::init_test_logger();
    // Serializes against the bounce acceptance: both tests manipulate the
    // system PipeWire daemon and must never run concurrently.
    let _daemon_lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _shutdown = common::ShutdownGuard::new();
    assert_daemon_probe_consistent();

    // Silent tone that keeps the NAM capture sink scheduled (deterministic
    // audio session before the daemon stop).
    let dir = common::temp_dir();
    let _dir_guard = common::DirGuard::new(dir.clone());
    let mut tone = ToneDriver::new(&dir);

    pipewire::init();

    let (_param_prod, param_cons) = RingBuffer::new(4);
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
                fail_fast: false,
                gate_config: cli::GateConfig::default_on(),
            },
            gc_cons,
            sl_cons,
            os_cons,
            None,
            None,
        )
    });

    // Wait for a healthy session (tone-driven), then stop the daemon and keep
    // it down: every reconnect attempt must fail and the bounded budget
    // (3 × progressive backoff, 1.75 s of sleeps) must be exhausted before the
    // host errors out.
    assert!(
        tone.wait_for_sink_and_attach(Duration::from_secs(5)),
        "host capture sink never registered in the graph"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut last_n_samples = 0u32;
    while std::time::Instant::now() < deadline {
        last_n_samples = rt_status.last_n_samples.load(Ordering::Relaxed);
        if last_n_samples > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        last_n_samples > 0,
        "audio never started before the daemon stop"
    );

    // Stop the daemon AND every related unit that could respawn it (socket,
    // pulse bridge) so the reconnect attempts genuinely fail until the budget
    // is exhausted. `wireplumber` is deliberately left running — stopping it
    // makes it crash on restart under a live graph, and it does not revive the
    // daemon by itself.
    let stop = std::process::Command::new("systemctl")
        .args([
            "--user",
            "stop",
            "pipewire.socket",
            "pipewire",
            "pipewire-pulse.socket",
            "pipewire-pulse",
        ])
        .status()
        .expect("failed to spawn systemctl --user stop pipewire");
    assert!(stop.success(), "could not stop the PipeWire daemon");
    let _restart_on_drop = PipewireRestartGuard;

    // Bounded join: the host must exit with an error within the backoff budget
    // plus generous per-attempt connect timeouts (10 s acceptance window).
    let join_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !pw_thread.is_finished() {
        assert!(
            std::time::Instant::now() < join_deadline,
            "host did not terminate after the reconnect budget was exhausted"
        );
        thread::sleep(Duration::from_millis(50));
    }
    let host_result = pw_thread
        .join()
        .expect("the PipeWire host thread suffered a fatal panic");
    assert!(
        host_result.is_err(),
        "an inaccessible daemon must exhaust the bounded retries and return an \
         error (fail-fast fallback), got Ok: {host_result:?}"
    );
    println!(
        "Exhaustion acceptance passed: host terminated cleanly with error \
         {host_result:?} after exhausting the bounded reconnect budget."
    );
}

/// RAII guard that kills and reaps the spawned `nam-audio-pipe` child on drop
/// (even on panic) so a failed assertion can never leave an orphaned audio
/// host mutating the user's PipeWire graph.
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Opt-in acceptance: with
/// `--fail-fast`, a forced backend failure (the stream observing a
/// post-streaming disconnect when the daemon is stopped) must tear the host
/// down and exit with a **non-zero code inside the SLA** — the immediate
/// fail-fast path that the bounded reconnect rolls back to when no
/// retry is available.
///
/// This exercises the **compiled binary** as a black box (the real process exit
/// code — unlike the in-process `run_pipewire_host` acceptances, which only
/// return an `Err`). Disruptive like the other opt-in daemon tests: it stops
/// the user's PipeWire daemon while the child runs and restores the whole group
/// via the RAII guard. Runs only with `NAM_DAEMON_BOUNCE_TEST=1`.
#[test]
#[ignore = "opt-in disruptive test: stops and restarts the user's PipeWire daemon; run with NAM_DAEMON_BOUNCE_TEST=1"]
fn test_pipewire_fail_fast_stream_error_terminates_within_sla() {
    if std::env::var("NAM_DAEMON_BOUNCE_TEST").as_deref() != Ok("1") {
        eprintln!("SKIP: NAM_DAEMON_BOUNCE_TEST != 1 (disruptive fail-fast acceptance).");
        return;
    }
    if !common::pw_play_available() {
        eprintln!("SKIP: pw-play unavailable; cannot drive the graph deterministically.");
        return;
    }

    // Serializes against the other daemon-manipulating acceptances.
    let _daemon_lock = common::TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _shutdown = common::ShutdownGuard::new();
    assert_daemon_probe_consistent();

    let dir = common::temp_dir();
    let _dir_guard = common::DirGuard::new(dir.clone());
    let stderr_path = dir.join("fail-fast-stderr.log");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create child stderr log");

    let child = std::process::Command::new(env!("CARGO_BIN_EXE_nam-audio-pipe"))
        .arg("--fail-fast")
        .current_dir(&dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .expect("failed to spawn nam-audio-pipe --fail-fast");
    let mut child = ChildGuard(child);

    // Drive a healthy session first: the capture sink must register and a tone
    // must be attached so the stream reaches an operational (Streaming) state
    // — only then is a subsequent disconnect a real backend failure (and not
    // the initial not-yet-connected state).
    let mut tone = ToneDriver::new(&dir);
    assert!(
        tone.wait_for_sink_and_attach(Duration::from_secs(5)),
        "host capture sink never registered / tone never attached"
    );
    std::thread::sleep(Duration::from_millis(1500));

    // Force the backend failure: stop the daemon group (the minimal set that
    // keeps it down — same as the exhaustion acceptance). The stream observes a
    // post-streaming disconnect -> Failed -> immediate fail-fast teardown.
    let stop = std::process::Command::new("systemctl")
        .args([
            "--user",
            "stop",
            "pipewire.socket",
            "pipewire",
            "pipewire-pulse.socket",
            "pipewire-pulse",
        ])
        .status()
        .expect("failed to spawn systemctl --user stop pipewire");
    assert!(stop.success(), "could not stop the PipeWire daemon");
    let _restart_on_drop = PipewireRestartGuard;

    // The child must exit with a NON-ZERO code inside the SLA (< 5 s) — never
    // a zombie alive without audio, never a graceful 0.
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(5);
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                assert!(
                    std::time::Instant::now() < deadline,
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
        started.elapsed() < Duration::from_secs(5),
        "fail-fast shutdown must complete within the SLA (took {:?})",
        started.elapsed()
    );
    println!(
        "Fail-fast stream-error acceptance passed: child exited {status:?} after {:?} \
         (stderr: {})",
        started.elapsed(),
        std::fs::read_to_string(&stderr_path)
            .unwrap_or_default()
            .lines()
            .last()
            .unwrap_or("")
    );
}
