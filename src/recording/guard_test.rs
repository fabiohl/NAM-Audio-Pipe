// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unit tests for the RAII recording worker guard and the observable join
//! (F-RB-009 / T3.5): premature-drop cleanup, formal join-result inspection
//! (worker error, panic, timeout) and the ordered StreamStop → producer drop
//! → bounded join teardown.

use super::*;
use crate::recording::buffer::{MAX_BLOCK_SIZE, RingPayload, create_audio_ring_buffer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

fn dummy_meta() -> RingPayload<MAX_BLOCK_SIZE> {
    RingPayload::Metadata(crate::recording::buffer::AudioMetadata {
        sample_rate: 48000.0,
        bit_depth: 32,
        channels: 2,
    })
}

#[test]
fn push_stream_stop_succeeds_when_capacity_frees() {
    let (mut prod, mut cons) = rtrb::RingBuffer::new(1);
    prod.push(dummy_meta()).unwrap();

    let handle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        let _ = cons.pop();
    });

    push_stream_stop(&mut prod, Duration::from_millis(200));
    handle.join().unwrap();
}

#[test]
fn push_stream_stop_times_out_when_ring_stays_full() {
    let (mut prod, _cons) = rtrb::RingBuffer::new(1);
    prod.push(dummy_meta()).unwrap();

    let start = std::time::Instant::now();
    push_stream_stop(&mut prod, Duration::from_millis(30));
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
/// `StreamStop` token, `2` on producer drop + drained ring, `0` on deadline.
/// Returns `Ok(())` on every exit path so only the termination *reason* is
/// observable.
fn spawn_mock_recording_worker(
    mut consumer: rtrb::Consumer<RingPayload<MAX_BLOCK_SIZE>>,
    exit_reason: Arc<AtomicU8>,
) -> std::thread::JoinHandle<anyhow::Result<()>> {
    std::thread::Builder::new()
        .name("guard-mock-recording-io".into())
        .spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
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
            }
            Ok(())
        })
        .expect("spawn mock recording worker")
}

#[test]
fn shutdown_pushes_stream_stop_then_drops_producer_and_joins() {
    let (producer, consumer) = rtrb::RingBuffer::<RingPayload<MAX_BLOCK_SIZE>>::new(8);
    let exit_reason = Arc::new(AtomicU8::new(0));

    // Ordering contract (F-RB-009 / T3.4): StreamStop first, producer drop
    // second, bounded join last. On an empty ring the token lands immediately,
    // so the worker must terminate on it (reason 1) and the join must return
    // promptly — a join-before-drop bug would block the mock forever.
    let worker = spawn_mock_recording_worker(consumer, Arc::clone(&exit_reason));
    let guard = RecordingWorkerGuard::new(worker, Some(producer), None);
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
}

#[test]
fn shutdown_skips_stream_stop_after_failure_but_still_drops_producer() {
    let (producer, consumer) = rtrb::RingBuffer::<RingPayload<MAX_BLOCK_SIZE>>::new(8);
    let failed = Arc::new(AtomicBool::new(true));
    let exit_reason = Arc::new(AtomicU8::new(0));

    // With a failed worker there is no consumer left: StreamStop must be
    // skipped (a full ring would never drain) but the producer must still be
    // dropped, arming the abandoned+drained terminal condition.
    let worker = spawn_mock_recording_worker(consumer, Arc::clone(&exit_reason));
    let guard = RecordingWorkerGuard::new(worker, Some(producer), Some(failed));
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
        "worker must terminate via the producer drop + drained ring"
    );
}

/// T3.5 acceptance — induced abort during host initialization must produce a
/// clean shutdown: a premature `Drop` (the equivalent of an early `?` return
/// or a panic unwinding in `run_pipewire_host` before the explicit shutdown
/// path) signals the worker and joins it with a bounded timeout, leaving no
/// zombie thread behind.
#[test]
fn premature_drop_signals_termination_and_joins_cleanly() {
    let (producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(8);
    let exit_reason = Arc::new(AtomicU8::new(0));

    let worker = spawn_mock_recording_worker(consumer, Arc::clone(&exit_reason));
    let start = std::time::Instant::now();
    {
        let guard = RecordingWorkerGuard::new(worker, Some(producer), None);
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
    let (producer, consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(8);
    let failed = Arc::new(AtomicBool::new(true));
    let exit_reason = Arc::new(AtomicU8::new(0));

    let worker = spawn_mock_recording_worker(consumer, Arc::clone(&exit_reason));
    {
        let guard = RecordingWorkerGuard::new(worker, Some(producer), Some(failed));
        drop(guard);
    }
    assert_eq!(
        exit_reason.load(Ordering::Acquire),
        2,
        "a failed worker must still be terminated by the producer drop"
    );
}

/// Proves the run.rs plumbing contract: the guard exposes a stable producer
/// slot that the RT callback (simulated here as the test) can push into, and
/// the same guard still owns the channel for the shutdown path.
#[test]
fn producer_slot_is_a_stable_mut_slot() {
    let (producer, mut consumer) = create_audio_ring_buffer::<MAX_BLOCK_SIZE>(8);
    let worker = std::thread::spawn(|| {
        // Stand-in for the real worker: outlives the push below and exits
        // cleanly on its own — only the slot plumbing is under test here.
        std::thread::sleep(Duration::from_millis(20));
        anyhow::Result::<()>::Ok(())
    });

    let mut guard = RecordingWorkerGuard::new(worker, Some(producer), None);
    let slot = guard.producer_slot();
    slot.as_mut()
        .expect("recording producer must be inside the guard")
        .push(dummy_meta())
        .expect("push through the guard's slot must succeed");

    match consumer.pop() {
        Ok(RingPayload::Metadata(_)) => {}
        other => panic!("expected the pushed metadata, got {other:?}"),
    }
}

#[test]
fn outcome_display_is_non_empty_for_all_variants() {
    for outcome in [
        RecordingWorkerOutcome::Success,
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
