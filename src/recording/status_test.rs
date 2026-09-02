// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the recording startup handshake and failure propagation
//! (F-RB-009 / T3.3): the observable status state machine, the atomic
//! RT-suspending failure flag, and the main-thread handshake waiter with its
//! timeout / vanished-worker / reported-failure verdicts.

use super::*;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[test]
fn recording_status_derives_debug_clone_partial_eq() {
    let starting = RecordingStatus::Starting;
    let active = RecordingStatus::Active {
        path: PathBuf::from("/tmp/capture_20260826_000000.wav"),
    };
    let failed = RecordingStatus::Failed {
        reason: "ENOSPC".into(),
    };
    let stopped = RecordingStatus::Stopped;

    assert_eq!(starting.clone(), starting);
    assert_eq!(active.clone(), active);
    assert_eq!(failed.clone(), failed);
    assert_eq!(stopped.clone(), stopped);
    assert_eq!(
        active,
        RecordingStatus::Active {
            path: PathBuf::from("/tmp/capture_20260826_000000.wav")
        }
    );
    assert_eq!(
        failed,
        RecordingStatus::Failed {
            reason: "ENOSPC".into()
        }
    );
    assert_ne!(active, stopped);
    // The enum must be printable for observability logs.
    let _ = format!("{starting:?} {active:?} {failed:?} {stopped:?}");
}

#[test]
fn record_failure_transitions_status_and_raises_flag() {
    let status: SharedRecordingStatus = Arc::new(Mutex::new(RecordingStatus::Starting));
    let flag = AtomicBool::new(false);

    record_failure(&status, &flag, "EIO while writing block");

    assert!(
        flag.load(Ordering::Acquire),
        "RT failure flag must be raised"
    );
    match &*status.lock().unwrap() {
        RecordingStatus::Failed { reason } => {
            assert_eq!(reason, "EIO while writing block");
        }
        other => panic!("status must be Failed, got {other:?}"),
    }
}

#[test]
fn record_failure_is_idempotent_and_raises_flag_even_when_poisoned() {
    let status: SharedRecordingStatus = Arc::new(Mutex::new(RecordingStatus::Active {
        path: PathBuf::from("/tmp/x"),
    }));
    // Poison the mutex: panic while holding the guard.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = status.lock().unwrap();
        panic!("boom while holding the lock");
    }));
    assert!(status.is_poisoned());

    let flag = AtomicBool::new(false);
    record_failure(&status, &flag, "poisoned-but-failed");

    // The flag must still be raised even though the status mutex is poisoned.
    assert!(flag.load(Ordering::Acquire));
}

#[test]
fn wait_for_recording_init_returns_ok_on_success() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send(Ok(PathBuf::from("/tmp/captures"))).unwrap();

    let path = wait_for_recording_init(rx, Duration::from_secs(2)).expect("must succeed");
    assert_eq!(path, PathBuf::from("/tmp/captures"));
}

#[test]
fn wait_for_recording_init_reports_worker_failure() {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tx.send(Err(anyhow::anyhow!(
        "output directory /bad is not writable: Permission denied"
    )))
    .unwrap();

    let err = wait_for_recording_init(rx, Duration::from_secs(2)).expect_err("must fail");
    assert!(
        matches!(
            &err,
            RecordingStartupError::Failed { reason } if reason.contains("Permission denied")
        ),
        "unexpected error: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "recording worker failed to start: output directory /bad is not writable: Permission denied"
    );
}

#[test]
fn wait_for_recording_init_reports_vanished_worker() {
    let (_tx, rx) = tokio::sync::oneshot::channel::<anyhow::Result<PathBuf>>();
    // Sender dropped without a verdict (worker gone).
    drop(_tx);

    let err = wait_for_recording_init(rx, Duration::from_secs(2)).expect_err("must fail");
    assert!(matches!(err, RecordingStartupError::WorkerGone));
}

#[test]
fn wait_for_recording_init_times_out_when_worker_is_silent() {
    let (tx, rx) = tokio::sync::oneshot::channel::<anyhow::Result<PathBuf>>();
    // Keep the sender alive for the whole call but never send: the waiter must
    // time out. `_sender` (not bare `_`) keeps `tx` alive until the end of scope.
    let _sender = tx;

    let start = std::time::Instant::now();
    let err = wait_for_recording_init(rx, Duration::from_millis(80)).expect_err("must time out");
    let elapsed = start.elapsed();

    assert!(
        matches!(&err, RecordingStartupError::Timeout { timeout } if *timeout == Duration::from_millis(80)),
        "unexpected error: {err:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(80),
        "waiter returned before the timeout ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "waiter blocked far past the timeout ({elapsed:?})"
    );
}

#[test]
fn recording_init_constructor_defaults_to_real_probe() {
    let status: SharedRecordingStatus = Arc::new(Mutex::new(RecordingStatus::Starting));
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let flag = Arc::new(AtomicBool::new(false));
    let init = RecordingInit::new(status, tx, flag, PathBuf::from("/tmp/captures"));

    assert_eq!(init.base_dir, PathBuf::from("/tmp/captures"));
    assert!(init.io_uring_probe.is_none());
    // Debug output must be non-empty (observable diagnostics).
    assert!(!format!("{init:?}").is_empty());
}
