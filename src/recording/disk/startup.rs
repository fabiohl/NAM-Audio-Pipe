// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Recording worker startup, validation, and result publication.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::recording::probe::{IoUringSupport, probe_io_uring};
use crate::recording::space::StatvfsFreeSpace;
use crate::recording::status::{
    RecordingInit, RecordingStatus, SharedRecordingStatus, mark_active, record_failure,
};
use crate::recording::transport::RecordingReceiver;
use crate::recording::writer::TokioUringSink;

use super::session::disk_writer_loop_inner;

/// Monotonic sequence suffix for the writability probe file.
static OUTPUT_PROBE_SEQ: AtomicU64 = AtomicU64::new(0);

// Intentional silence vs. real loss. When the `Silence` noise gate suppresses a
// quantum (no cabinet tail to drain), the RT thread publishes an
// intentional-silence marker carrying the suppressed frame count — it never
// copies zeros. The writer materializes exactly that many zero frames, so the
// WAV duration equals every frame the gate processed while closed. A *real*
// transport loss (pool exhaustion / full ring) is never padded: it increments
// `OVERRUN_COUNT`/`OVERRUN_FRAMES_COUNT` off the RT path and is reported as a
// lossy capture. The gate remains configurable via CLI (`--gate off`, see
// `cli::GateConfig::default_on()` in `CaptureState::init`).

/// Main entry point for the Disk I/O thread.
/// Consumes the lock-free ring buffer and writes WAV files fully asynchronously via `io_uring`.
///
/// Before consuming anything it completes the startup handshake: validates
/// that the output directory is a real writable directory
/// (`validate_output_dir`), publishes `Active` on the handshake, then runs
/// the drain loop. The drain loop ignores the process-global `SHUTDOWN` flag:
/// it terminates only when the [`crate::recording::buffer::ControlPayload::StreamStop`] token is consumed
/// or the ring `Producer` is dropped with the ring fully drained — never
/// while the RT producer can still emit. Every terminal path drains all
/// remaining data, rewrites the WAV header with the final byte count and
/// `fsync`s before returning. Any fatal error transitions the observable
/// [`RecordingStatus`] to `Failed` and raises the RT-observable failure flag.
pub async fn disk_writer_loop(mut receiver: RecordingReceiver, init: RecordingInit) -> Result<()> {
    let RecordingInit {
        status,
        handshake,
        failed_flag,
        base_dir,
        ..
    } = init;

    // Fail-fast startup: abort BEFORE the main thread starts PipeWire.
    if let Err(e) = validate_output_dir(&base_dir) {
        let reason = format!(
            "Recording output directory {} is not usable: {e:#}",
            base_dir.display()
        );
        return Err(fail_startup(&status, &failed_flag, handshake, reason));
    }

    let _ = handshake.send(Ok(base_dir.clone()));
    mark_active(&status, base_dir.clone());
    log::info!(
        "🎙️  Recording worker ready — capture directory: {}",
        base_dir.display()
    );

    let space = StatvfsFreeSpace;
    let sink = TokioUringSink {
        base_dir: &base_dir,
        space: &space,
    };
    let result = disk_writer_loop_inner(&sink, &mut receiver, &status).await;

    publish_worker_result(&result, &status, &failed_flag);

    result
}

/// Publishes the terminal worker result on the observable status.
pub(super) fn publish_worker_result(
    result: &anyhow::Result<()>,
    status: &SharedRecordingStatus,
    failed_flag: &AtomicBool,
) {
    match result {
        Ok(()) => {
            if let Ok(mut guard) = status.lock() {
                *guard = RecordingStatus::Stopped;
            }
        }
        Err(e) => {
            let reason = format!("{e:#}");
            record_failure(status, failed_flag, &reason);
        }
    }
}

/// Validates that `base_dir` is a real directory and that the worker can
/// actually create, write, `fsync` and delete a file in it.
pub(super) fn validate_output_dir(base_dir: &Path) -> Result<()> {
    if !base_dir.is_dir() {
        anyhow::bail!(
            "output directory {} does not exist or is not a directory",
            base_dir.display()
        );
    }
    let probe = base_dir.join(format!(
        ".nam-io-probe-{}-{}.tmp",
        std::process::id(),
        OUTPUT_PROBE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .with_context(|| format!("output directory {} is not writable", base_dir.display()))?;
    let write_res = file.write_all(b"io");
    let sync_res = file.sync_all();
    drop(file);
    let _ = std::fs::remove_file(&probe);
    write_res.with_context(|| format!("write probe failed in {}", base_dir.display()))?;
    sync_res.with_context(|| format!("fsync probe failed in {}", base_dir.display()))?;
    Ok(())
}

/// Records a startup failure on the observable status and failure flag, sends
/// `Err` over the startup handshake (consuming the one-shot sender), and
/// returns the error to propagate.
fn fail_startup(
    status: &SharedRecordingStatus,
    failed_flag: &AtomicBool,
    handshake: tokio::sync::oneshot::Sender<anyhow::Result<PathBuf>>,
    reason: String,
) -> anyhow::Error {
    record_failure(status, failed_flag, &reason);
    let _ = handshake.send(Err(anyhow::anyhow!(reason.clone())));
    anyhow::anyhow!(reason)
}

/// Spawns the `nam-recording-io` worker thread.
pub fn spawn_recording_worker(
    receiver: RecordingReceiver,
    init: RecordingInit,
    housekeeping_cpus: Vec<usize>,
) -> std::io::Result<std::thread::JoinHandle<anyhow::Result<()>>> {
    std::thread::Builder::new()
        .name("nam-recording-io".into())
        .spawn(move || {
            let _ = crate::standalone::rt_setup::apply_housekeeping_affinity(&housekeeping_cpus);
            let probe = init.io_uring_probe.unwrap_or(probe_io_uring);
            let verdict = probe();
            if verdict != IoUringSupport::Available {
                let reason = format!(
                    "io_uring is unavailable on this kernel/security policy \
                     (verdict: {verdict:?}); recording cannot start"
                );
                record_failure(&init.status, &init.failed_flag, &reason);
                let _ = init.handshake.send(Err(anyhow::anyhow!(reason.clone())));
                log::error!("🛑 {reason}");
                Err(anyhow::anyhow!(reason))
            } else {
                tokio_uring::start(async move {
                    let result = disk_writer_loop(receiver, init).await;
                    if let Err(e) = &result {
                        log::error!("🛑 Disk writer error: {e:#}");
                    }
                    result
                })
            }
        })
}
