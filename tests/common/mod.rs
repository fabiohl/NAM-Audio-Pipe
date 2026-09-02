// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#![allow(dead_code)]

//! Shared helpers for NAM-Audio-Pipe integration test binaries.
//!
//! Single source of truth for the PipeWire daemon probe so
//! `pw_integration` and `recording` can never drift apart —
//! probe-consistency guarantees depend on exactly one
//! definition of "daemon reachable".
//!
//! Also the single source of truth for the recording worker handshake
//! helpers, the temporary-capture-directory factory, the `SHUTDOWN` guard and
//! the per-binary test mutex — `tests/recording.rs` and
//! `tests/recording_fault_injection.rs` must never drift apart on the
//! `RecordingInit`/`RecordingStatus::Active` contract.
//!
//! The `swap` submodule holds deterministic model builders and synthetic
//! signal factories shared by the swap-stress and extended-soak harnesses;
//! the `proc` submodule holds the `/proc` telemetry readers shared by
//! the accelerated-soak and real-endurance harnesses.

pub mod proc;
pub mod swap;

use nam_audio_pipe::recording::transport::RecordingReceiver;
use nam_audio_pipe::recording::{
    RecordingInit, RecordingStatus, SharedRecordingStatus, spawn_recording_worker,
    wait_for_recording_init,
};
use neural_amp_modeler_rs::common::spsc::SHUTDOWN;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Probes for a reachable PipeWire daemon via `pw-cli info 0`.
///
/// `true` only when the command succeeds — the same check
/// `utils/tests-quick.sh` uses to gate Phase 3.
pub fn probe_pipewire_daemon() -> bool {
    std::process::Command::new("pw-cli")
        .args(["info", "0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Waits until the PipeWire daemon is reachable (`pw-cli info 0`), polling
/// every 200 ms until `timeout`.
pub fn wait_for_pipewire_daemon(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if probe_pipewire_daemon() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

/// Waits until the PipeWire graph is fully functional again.
///
/// `true` once the daemon is reachable (`pw-cli info 0`) AND at least one
/// `Audio/Sink` node is registered in the graph — the sink the host's playback
/// stream binds to. After a full daemon teardown (the disruptive opt-in
/// bounce tests) the daemon may come back before the graph re-registers the
/// hardware sink; a test starting against a settling graph would observe
/// `no target node available`. Polls every 200 ms until `timeout`.
pub fn wait_for_audio_graph(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !probe_pipewire_daemon() {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        if graph_has_media_class("Audio/Sink") {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

/// Whether the PipeWire graph currently contains a node with the given
/// `media.class` (via `pw-dump`).
fn graph_has_media_class(media_class: &str) -> bool {
    let Ok(out) = std::process::Command::new("pw-dump")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains(&format!("\"media.class\": \"{media_class}\""))
}

/// Whether the NAM-Audio-Pipe capture sink node is currently registered in the
/// graph — i.e. the host reconnected and its fresh `Audio/Sink` is live.
pub fn graph_has_nam_sink() -> bool {
    let Ok(out) = std::process::Command::new("pw-dump")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains("\"node.name\": \"NAM-Audio-Pipe-input\"")
}

/// Waits until the NAM-Audio-Pipe capture sink node is registered in the
/// graph (the host reconnected after a daemon bounce).
pub fn wait_for_nam_sink(timeout: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if graph_has_nam_sink() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Writes a `seconds`-long silent stereo WAV (48 kHz, s16) at `path`.
///
/// The daemon-bounce acceptance plays it into the NAM capture sink so the
/// graph deterministically schedules the capture node — a silent stream still
/// produces real quantums (`last_n_samples` advances) without any audible
/// tone reaching the hardware.
///
/// ⚠️ AVISO: estes helpers dependem do gate permanecer FECHADO para manter o grafo
/// agendado sem abrir. Se algum teste futuro reusá-los sob `--gate off`, o
/// comportamento estrutural muda (o "silêncio" passa a ser processado como sinal real).
/// Não usar com `gate_enabled = false`.
pub fn generate_silent_wav(path: &Path, seconds: u32) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create silent wav");
    for _ in 0..seconds * spec.sample_rate {
        for _ in 0..spec.channels {
            writer.write_sample(0i16).expect("write silent sample");
        }
    }
    writer.finalize().expect("finalize silent wav");
}

/// Whether `pw-play` (PipeWire's CLI media player) is available.
pub fn pw_play_available() -> bool {
    std::process::Command::new("pw-play")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Initializes the `NamLogger` backend once per test binary (best-effort).
///
/// The opt-in daemon-bounce tests spawn the real `run_pipewire_host` and need
/// its off-RT `log::*` output to be diagnosable; without a backend the host's
/// reconnect warnings/errors are silently discarded. `NamLogger::init` is a
/// global one-shot, so repeated calls (multiple tests in one binary) simply
/// no-op.
pub fn init_test_logger() {
    use neural_amp_modeler_rs::common::diagnostics::logger::{LoggerConfig, NamLogger};
    let level_filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Info);
    let _ = NamLogger::init(LoggerConfig {
        level_filter,
        emit_stderr: true,
    });
}

/// Serializes tests that touch the shared filesystem, the process-wide
/// `SHUTDOWN` flag or a process-wide rlimit. Compiled per integration binary,
/// so each binary gets its own mutex instance.
pub static TEST_MUTEX: Mutex<()> = Mutex::new(());

/// Monotonic per-process sequence for temporary capture directories.
///
/// Keeps every `temp_dir()` call unique even within the same process, and is
/// paired with a plain `create_dir` (i.e. `mkdir(2)`) so a pre-existing entry
/// — directory, file or attacker-placed symlink (CWE-377/379) — is never
/// followed or reused; the factory simply moves to the next name.
static TEMP_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// Creates a fresh, exclusive temporary directory for capture files.
///
/// `mkdir(2)` fails with `EEXIST` when the name is already taken (including by
/// a symlink), so a pre-created `/tmp/nam-rs-test-<pid>-<seq>` entry can never
/// redirect the test's writes — the caller retries on the next sequence value.
pub fn temp_dir() -> PathBuf {
    loop {
        let dir = std::env::temp_dir().join(format!(
            "nam-rs-test-{}-{:x}",
            std::process::id(),
            TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("failed to create temp dir {}: {e}", dir.display()),
        }
    }
}

/// RAII guard that removes the temp directory on drop (even on panic).
pub struct DirGuard(PathBuf);

impl DirGuard {
    pub fn new(dir: PathBuf) -> Self {
        Self(dir)
    }
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// RAII guard restoring the previous process-global `SHUTDOWN` value.
///
/// Gives every test a clean slate (`false`) on construction and restores the
/// value observed before the test on drop — the single definition of "what
/// `SHUTDOWN` means after a test" shared by all integration binaries.
pub struct ShutdownGuard(bool);

impl ShutdownGuard {
    pub fn new() -> Self {
        Self(SHUTDOWN.swap(false, Ordering::AcqRel))
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        SHUTDOWN.store(self.0, Ordering::Release);
    }
}

/// Builds a `RecordingInit` bound to `dir` plus the handshake receiver, the
/// observable status and the RT failure flag, so a test can drive the worker's
/// startup handshake and assert on its outcome.
pub fn recording_init_for(
    dir: &Path,
) -> (
    RecordingInit,
    tokio::sync::oneshot::Receiver<anyhow::Result<PathBuf>>,
    SharedRecordingStatus,
    Arc<AtomicBool>,
) {
    let status: SharedRecordingStatus = Arc::new(Mutex::new(RecordingStatus::Starting));
    let failed_flag = Arc::new(AtomicBool::new(false));
    let (init_tx, init_rx) = tokio::sync::oneshot::channel();
    let init = RecordingInit::new(
        status.clone(),
        init_tx,
        Arc::clone(&failed_flag),
        dir.to_path_buf(),
    );
    (init, init_rx, status, failed_flag)
}

/// Spawns a recording worker for `receiver`, waits for the startup handshake
/// and returns the join handle plus the observable handles.
pub fn spawn_ready_worker(
    receiver: RecordingReceiver,
    dir: &Path,
) -> (
    std::thread::JoinHandle<anyhow::Result<()>>,
    SharedRecordingStatus,
    Arc<AtomicBool>,
) {
    let (init, init_rx, status, failed_flag) = recording_init_for(dir);
    let handle = spawn_recording_worker(receiver, None, init).expect("spawn recording worker");
    let ready_dir = wait_for_recording_init(init_rx, std::time::Duration::from_secs(5))
        .expect("recording worker must confirm readiness via the startup handshake");
    assert_eq!(
        ready_dir.as_path(),
        dir,
        "handshake must confirm the configured output directory"
    );
    match &*status.lock().unwrap() {
        RecordingStatus::Active { path } => assert_eq!(path.as_path(), dir),
        other => panic!("status must be Active after the handshake, got {other:?}"),
    }
    (handle, status, failed_flag)
}
