// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the RAII recording worker guard and the observable join
//! (F-RB-009 / T3.5): premature-drop cleanup, formal join-result inspection
//! (worker error, panic, timeout) and the ordered StreamStop → sender drop
//! → bounded join teardown.

use super::*;
use crate::recording::buffer::{
    AudioMetadata, ControlPayload, OVERRUN_COUNT, OVERRUN_COUNT_LOCK, OVERRUN_FRAMES_COUNT,
    RingPayload,
};
use crate::recording::transport::{RecordingReceiver, RecordingSender, create_recording_transport};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

/// Runs `f` with the process-wide overrun counters pinned to zero (under the
/// test-only `OVERRUN_COUNT_LOCK` that serializes mutation of the globals), so
/// assertions on `Success` vs `SuccessWithLoss` are deterministic regardless of
/// parallel overrun-accounting tests in the same `--lib` binary (F-RB-024 /
/// T5.1).
fn with_zeroed_overruns<R>(f: impl FnOnce() -> R) -> R {
    let _guard = OVERRUN_COUNT_LOCK.lock().unwrap();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);
    let result = f();
    OVERRUN_COUNT.store(0, Ordering::Relaxed);
    OVERRUN_FRAMES_COUNT.store(0, Ordering::Relaxed);
    result
}

fn dummy_meta() -> AudioMetadata {
    AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    }
}

#[test]
fn push_stream_stop_succeeds_when_capacity_frees() {
    // A single-slot control channel pre-filled with a Metadata: the retry loop
    // must land the StreamStop as soon as the consumer frees the slot.
    let (control_p, mut control_c) = crate::recording::buffer::create_control_ring_buffer(1);
    let mut sender = RecordingSender::Pool {
        control: Some(control_p),
        pool: None,
    };
    sender
        .control_producer_mut()
        .unwrap()
        .push(ControlPayload::Metadata(dummy_meta()))
        .unwrap();

    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        let _ = control_c.pop();
    });

    push_stream_stop(&mut sender, Duration::from_millis(200));
    handle.join().unwrap();
}

#[test]
fn push_stream_stop_times_out_when_ring_stays_full() {
    // A single-slot control channel with no consumer: the bounded retry must
    // give up at the timeout — never spin forever.
    let (control_p, _control_c) = crate::recording::buffer::create_control_ring_buffer(1);
    let mut sender = RecordingSender::Pool {
        control: Some(control_p),
        pool: None,
    };
    sender
        .control_producer_mut()
        .unwrap()
        .push(ControlPayload::Metadata(dummy_meta()))
        .unwrap();

    let start = std::time::Instant::now();
    push_stream_stop(&mut sender, Duration::from_millis(30));
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(30),
        "retry returned before the timeout ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "retry blocked past the bounded timeout ({elapsed:?})"
    );
}

#[test]
fn join_recording_io_returns_success_promptly_when_thread_finishes() {
    let mut handle = Some(std::thread::spawn(|| Ok(())));
    let start = std::time::Instant::now();
    let outcome = join_recording_io(&mut handle, Duration::from_secs(5));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "join must return promptly for a finished thread"
    );
    assert!(matches!(outcome, RecordingWorkerOutcome::Success));
    assert!(
        handle.is_none(),
        "a joined handle must be consumed from the slot"
    );
}

#[test]
fn join_recording_io_captures_worker_error() {
    let mut handle = Some(std::thread::spawn(|| {
        anyhow::Result::<()>::Err(anyhow::anyhow!("ENOSPC while writing audio block"))
    }));
    let outcome = join_recording_io(&mut handle, Duration::from_secs(5));
    match &outcome {
        RecordingWorkerOutcome::Failed { reason } => {
            assert!(
                reason.contains("ENOSPC"),
                "the error chain must be preserved: {reason}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn join_recording_io_captures_panic_payload() {
    let mut handle = Some(std::thread::spawn(|| -> anyhow::Result<()> {
        panic!("boom in the recording worker");
    }));
    let outcome = join_recording_io(&mut handle, Duration::from_secs(5));
    match &outcome {
        RecordingWorkerOutcome::Panicked { message } => {
            assert!(
                message.contains("boom in the recording worker"),
                "the panic message must be preserved: {message}"
            );
        }
        other => panic!("expected Panicked, got {other:?}"),
    }
}

#[test]
fn join_recording_io_times_out_and_never_reports_success() {
    let mut handle = Some(std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(500));
        Ok(())
    }));
    let start = std::time::Instant::now();
    let outcome = join_recording_io(&mut handle, Duration::from_millis(50));
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(50),
        "join returned before the timeout ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_millis(450),
        "join blocked past the bounded timeout ({elapsed:?})"
    );
    assert!(
        matches!(
            &outcome,
            RecordingWorkerOutcome::TimedOut { timeout } if *timeout == Duration::from_millis(50)
        ),
        "expected TimedOut, got {outcome:?}"
    );
}

/// Spawns a mock "disk worker" that records how it terminated: `1` on the
/// `StreamStop` token, `2` on sender drop + drained channels, `0` on deadline.
/// Returns `Ok(())` on every exit path so only the termination *reason* is
/// observable.
fn spawn_mock_recording_worker(
    mut receiver: RecordingReceiver,
    exit_reason: Arc<AtomicU8>,
) -> std::thread::JoinHandle<anyhow::Result<()>> {
    std::thread::Builder::new()
        .name("guard-mock-recording-io".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            match &mut receiver {
                RecordingReceiver::Pool { control, pool } => loop {
                    match control.pop() {
                        Ok(ControlPayload::StreamStop) => {
                            exit_reason.store(1, Ordering::Release);
                            break;
                        }
                        Ok(_) => {}
                        Err(_)
                            if control.is_abandoned()
                                && pool.work_is_abandoned()
                                && pool.work_is_empty() =>
                        {
                            exit_reason.store(2, Ordering::Release);
                            break;
                        }
                        Err(_) => {
                            if std::time::Instant::now() >= deadline {
                                exit_reason.store(0, Ordering::Release);
                                break;
                            }
                            std::thread::yield_now();
                        }
                    }
                },
                RecordingReceiver::Inline(consumer) => loop {
                    match consumer.pop() {
                        Ok(RingPayload::StreamStop) => {
                            exit_reason.store(1, Ordering::Release);
                            break;
                        }
                        Ok(_) => {}
                        Err(_) if consumer.is_abandoned() => {
                            exit_reason.store(2, Ordering::Release);
                            break;
                        }
                        Err(_) => {
                            if std::time::Instant::now() >= deadline {
                                exit_reason.store(0, Ordering::Release);
                                break;
                            }
                            std::thread::yield_now();
                        }
                    }
                },
            }
            Ok(())
        })
        .expect("spawn mock recording worker")
}

#[test]
fn shutdown_pushes_stream_stop_then_drops_producer_and_joins() {
    with_zeroed_overruns(|| {
        let (sender, receiver) = create_recording_transport();
        let exit_reason = Arc::new(AtomicU8::new(0));

        // Ordering contract (F-RB-009 / T3.4): StreamStop first, sender drop
        // second, bounded join last. On an empty control channel the token lands
        // immediately, so the worker must terminate on it (reason 1) and the join
        // must return promptly — a join-before-drop bug would block the mock
        // forever.
        let worker = spawn_mock_recording_worker(receiver, Arc::clone(&exit_reason));
        let guard = RecordingWorkerGuard::new(worker, Some(sender), None);
        let start = std::time::Instant::now();
        let outcome = guard.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "shutdown must return promptly"
        );

        assert!(matches!(outcome, RecordingWorkerOutcome::Success));
        assert_eq!(
            exit_reason.load(Ordering::Acquire),
            1,
            "worker must terminate on the StreamStop token"
        );
    });
}

#[test]
fn shutdown_skips_stream_stop_after_failure_but_still_drops_producer() {
    with_zeroed_overruns(|| {
        let (sender, receiver) = create_recording_transport();
        let failed = Arc::new(AtomicBool::new(true));
        let exit_reason = Arc::new(AtomicU8::new(0));

        // With a failed worker there is no consumer left: StreamStop must be
        // skipped (a full channel would never drain) but the sender must still be
        // dropped, arming the abandoned+drained terminal condition.
        let worker = spawn_mock_recording_worker(receiver, Arc::clone(&exit_reason));
        let guard = RecordingWorkerGuard::new(worker, Some(sender), Some(failed));
        let start = std::time::Instant::now();
        let outcome = guard.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "shutdown must return promptly"
        );

        assert!(matches!(outcome, RecordingWorkerOutcome::Success));
        assert_eq!(
            exit_reason.load(Ordering::Acquire),
            2,
            "worker must terminate via the sender drop + drained channels"
        );
    });
}

/// T5.1 (F-RB-024, D1): a clean join with ring overruns counted on the capture
/// path must surface as `SuccessWithLoss` with the exact block/frame counts —
/// never as a pristine `Success` — and the D1 exit code must be non-zero.
#[test]
fn shutdown_reports_success_with_loss_when_overruns_were_counted() {
    with_zeroed_overruns(|| {
        let (sender, receiver) = create_recording_transport();
        let exit_reason = Arc::new(AtomicU8::new(0));

        // Simulate the RT producer's fail-closed telemetry (process.rs
        // increments these globals on pool exhaustion / oversize blocks).
        OVERRUN_COUNT.store(7, Ordering::Relaxed);
        OVERRUN_FRAMES_COUNT.store(2560, Ordering::Relaxed);

        let worker = spawn_mock_recording_worker(receiver, Arc::clone(&exit_reason));
        let guard = RecordingWorkerGuard::new(worker, Some(sender), None);
        let outcome = guard.shutdown();

        assert_eq!(
            outcome,
            RecordingWorkerOutcome::SuccessWithLoss {
                blocks: 7,
                frames: 2560,
            },
            "a clean join with overruns must become SuccessWithLoss"
        );
        assert_eq!(
            outcome.exit_code(),
            1,
            "SuccessWithLoss must map to a non-zero exit (D1)"
        );
        assert_eq!(
            exit_reason.load(Ordering::Acquire),
            1,
            "worker must terminate on the StreamStop token"
        );
    });
}

/// T5.1 (F-RB-024, D1): a clean join with zero overruns keeps `Success` — the
/// baseline policy is unchanged and the exit code stays 0.
#[test]
fn shutdown_with_zero_overruns_stays_success() {
    with_zeroed_overruns(|| {
        let (sender, receiver) = create_recording_transport();
        let exit_reason = Arc::new(AtomicU8::new(0));

        let worker = spawn_mock_recording_worker(receiver, Arc::clone(&exit_reason));
        let guard = RecordingWorkerGuard::new(worker, Some(sender), None);
        let outcome = guard.shutdown();

        assert_eq!(
            outcome,
            RecordingWorkerOutcome::Success,
            "zero overruns must keep the outcome a clean Success"
        );
        assert_eq!(
            outcome.exit_code(),
            0,
            "a lossless recording keeps exit code 0 (D1)"
        );
    });
}

/// T3.5 acceptance — induced abort during host initialization must produce a
/// clean shutdown: a premature `Drop` (the equivalent of an early `?` return
/// or a panic unwinding in `run_pipewire_host` before the explicit shutdown
/// path) signals the worker and joins it with a bounded timeout, leaving no
/// zombie thread behind.
#[test]
fn premature_drop_signals_termination_and_joins_cleanly() {
    let (sender, receiver) = create_recording_transport();
    let exit_reason = Arc::new(AtomicU8::new(0));

    let worker = spawn_mock_recording_worker(receiver, Arc::clone(&exit_reason));
    let start = std::time::Instant::now();
    {
        let guard = RecordingWorkerGuard::new(worker, Some(sender), None);
        // Premature drop — simulates the host failing before the shutdown path.
        drop(guard);
    }
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "premature drop must not block past the bounded teardown"
    );
    assert_eq!(
        exit_reason.load(Ordering::Acquire),
        1,
        "the worker must be terminated by the guard's StreamStop signal"
    );
}

#[test]
fn premature_drop_with_failed_worker_still_terminates_via_producer_drop() {
    let (sender, receiver) = create_recording_transport();
    let failed = Arc::new(AtomicBool::new(true));
    let exit_reason = Arc::new(AtomicU8::new(0));

    let worker = spawn_mock_recording_worker(receiver, Arc::clone(&exit_reason));
    {
        let guard = RecordingWorkerGuard::new(worker, Some(sender), Some(failed));
        drop(guard);
    }
    assert_eq!(
        exit_reason.load(Ordering::Acquire),
        2,
        "a failed worker must still be terminated by the sender drop"
    );
}

/// Proves the run.rs plumbing contract: the guard exposes a stable sender
/// slot that the RT callback (simulated here as the test) can push into, and
/// the same guard still owns the channel for the shutdown path.
#[test]
fn sender_slot_is_a_stable_mut_slot() {
    let (sender, mut receiver) = create_recording_transport();
    let worker = std::thread::spawn(|| {
        // Stand-in for the real worker: outlives the push below and exits
        // cleanly on its own — only the slot plumbing is under test here.
        std::thread::sleep(Duration::from_millis(20));
        anyhow::Result::<()>::Ok(())
    });

    let mut guard = RecordingWorkerGuard::new(worker, Some(sender), None);
    let slot = guard.sender_slot();
    assert!(
        slot.try_push_metadata(dummy_meta()),
        "push through the guard's sender slot must succeed"
    );

    match &mut receiver {
        RecordingReceiver::Pool { control, .. } => match control.pop() {
            Ok(ControlPayload::Metadata(_)) => {}
            other => panic!("expected the pushed metadata, got {other:?}"),
        },
        RecordingReceiver::Inline(_) => panic!("pool transport expected"),
    }
}

#[test]
fn outcome_display_is_non_empty_for_all_variants() {
    for outcome in [
        RecordingWorkerOutcome::Success,
        RecordingWorkerOutcome::SuccessWithLoss {
            blocks: 7,
            frames: 2560,
        },
        RecordingWorkerOutcome::Failed {
            reason: "EIO".into(),
        },
        RecordingWorkerOutcome::Panicked {
            message: "boom".into(),
        },
        RecordingWorkerOutcome::TimedOut {
            timeout: Duration::from_secs(5),
        },
    ] {
        let rendered = format!("{outcome}");
        assert!(!rendered.is_empty(), "Display must be informative");
    }
}
