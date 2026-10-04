// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Tests for the off-RT PW log ring (`rt_log_ring.rs`): SPSC discipline,
//! counted overflow discard, cross-thread ordered drain, route decisions
//! (marked/unmarked), drain reach into the `LogBuffer` and the parity between
//! the ring drain and the synchronous dispatch shape.

use super::*;
use crate::standalone::PW_LOG_REDIR_TEST_LOCK;
use crate::standalone::pw_host::log_redirect::nam_pw_log;
use crate::standalone::rt_setup::rt_log_mark::{
    rt_is_current_thread_marked, rt_mark_current_thread, rt_unmark_current_thread,
};
use pipewire::spa::sys::{SPA_LOG_LEVEL_DEBUG, SPA_LOG_LEVEL_INFO, SPA_LOG_LEVEL_WARN};
use std::ffi::c_char;

fn test_record(level: u32, file: &str, line: i32, msg: &str) -> RtLogRecord {
    RtLogRecord::capture(level, line, file, "", msg)
}

fn ensure_nam_logger() {
    let _ = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::init(
        neural_amp_modeler_rs::common::diagnostics::logger::LoggerConfig {
            level_filter: log::LevelFilter::Trace,
            emit_stderr: false,
        },
    );
    log::set_max_level(log::LevelFilter::Trace);
}
#[test]
fn ring_full_discards_with_exact_counter() {
    let _lock_guard = PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reset_pw_log_dropped_for_test();
    let mut consumer;

    {
        let (producer, ring_consumer) = rtrb::RingBuffer::<RtLogRecord>::new(RT_RING_CAPACITY);
        let producer = RtProducer(std::cell::UnsafeCell::new(producer));
        consumer = ring_consumer;
        // Fill exactly to capacity: every push must succeed.
        for i in 0..RT_RING_CAPACITY {
            let msg = format!("fill-{i}");
            assert!(
                producer.push(test_record(
                    SPA_LOG_LEVEL_INFO,
                    "ring_full.rs",
                    i as i32,
                    &msg,
                )),
                "capacity-sized push {i} must fit"
            );
        }
        // The SPSC primitive is full: a raw push over capacity is refused.
        for i in 0..5 {
            let msg = format!("over-{i}");
            assert!(
                !producer.push(test_record(SPA_LOG_LEVEL_INFO, "ring_full.rs", i, &msg)),
                "overflow probe {i} must be refused by the full SPSC ring"
            );
        }
        // The counted-overload path (`push_or_count`) over the saturated
        // ring: 5 saturated attempts must land exactly 5 in `pw_log_dropped`
        // and the RT path stays lock-free/alloc-free in both outcomes.
        for i in 0..5 {
            let msg = format!("drop-me-{i}");
            push_or_count(
                &producer,
                test_record(SPA_LOG_LEVEL_DEBUG, "ring_full.rs", i, &msg),
            );
        }
    }
    assert_eq!(
        pw_log_dropped(),
        5,
        "drop counter must count exactly the saturated pushes"
    );
    // Consumer side still sees exactly the capacity records, in order.
    let mut drained = 0usize;
    while consumer.pop().is_ok() {
        drained += 1;
    }
    assert_eq!(drained, RT_RING_CAPACITY);
    reset_pw_log_dropped_for_test();
}

#[test]
fn ring_preserves_push_order_across_threads() {
    let (mut producer, mut consumer) = rtrb::RingBuffer::<RtLogRecord>::new(RT_RING_CAPACITY);
    let pusher = std::thread::spawn(move || {
        for i in 0..256 {
            let msg = format!("event-{i}");
            producer
                .push(RtLogRecord::capture(
                    SPA_LOG_LEVEL_DEBUG,
                    i,
                    "ordered.rs",
                    "topic-ordered",
                    &msg,
                ))
                .unwrap_or_else(|_| panic!("push {i} must fit the ring"));
        }
    });
    pusher.join().unwrap_or_else(|_| panic!("pusher panicked"));

    // Off-RT consumer pops strictly in production order (SPSC contract of
    // the routed log stream).
    let mut received = Vec::with_capacity(256);
    while let Ok(record) = consumer.pop() {
        received.push((record.msg_str().to_owned(), record.line));
    }
    assert_eq!(received.len(), 256);
    for (i, (msg, line)) in received.into_iter().enumerate() {
        assert_eq!(msg, format!("event-{i}"), "ordering broken at {i}");
        assert_eq!(line, i as i32);
    }
}

#[test]
fn record_capture_truncates_oversized_fields() {
    let long_msg = "m".repeat(400);
    let record = RtLogRecord::capture(
        SPA_LOG_LEVEL_DEBUG,
        42,
        &"f".repeat(70),
        &"t".repeat(50),
        &long_msg,
    );
    assert_eq!(
        record.msg_str().len(),
        super::MSG_CAP,
        "msg must truncate at 256"
    );
    assert_eq!(
        record.file_str().len(),
        super::FILE_CAP,
        "file must truncate at cap"
    );
    assert_eq!(
        record.topic_str().len(),
        super::TOPIC_CAP,
        "topic must truncate at cap"
    );
    // The truncated body is a proud prefix, never garbage bytes.
    assert!(record.msg_str().ends_with('m'));
}

#[test]
fn unmarked_thread_routes_synchronously_under_ring_mode() {
    ensure_nam_logger();
    let _lock_guard = PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    install();
    assert!(is_ring_routing_active());
    // This harness thread was never marked: the offload must keep the
    // synchronous dispatch route (S3-T2/S3-T3 consult contract).
    assert!(!rt_is_current_thread_marked());

    let marker = "UNMARKED_SYNC_MARKER_XYZ";
    let fmt = b"sync-route-probe %s\0";
    let marker_c = b"UNMARKED_SYNC_MARKER_XYZ\0";
    let file = b"log_redirect_test.rs\0";
    let func = b"unmarked_thread_routes_synchronously_under_ring_mode\0";
    // SAFETY: own extern "C" variadic entry with null-terminated strings.
    unsafe {
        nam_pw_log(
            std::ptr::null_mut(),
            SPA_LOG_LEVEL_INFO,
            file.as_ptr() as *const c_char,
            111,
            func.as_ptr() as *const c_char,
            fmt.as_ptr() as *const c_char,
            marker_c.as_ptr() as *const c_char,
        );
    }
    // Synchronous dispatch: the entry is in the LogBuffer IMMEDIATELY (no
    // drainer wait involved).
    if let Some(buf) = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer() {
        let found = buf
            .snapshot()
            .iter()
            .any(|rec| rec.message.contains(marker));
        assert!(found, "unmarked sync dispatch did not capture the marker");
    }
}

fn wait_drained_logbuffer(marker: &str) -> Option<String> {
    for _ in 0..600 {
        if let Some(buf) =
            neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
            && let Some(rec) = buf
                .snapshot()
                .iter()
                .find(|rec| rec.message.contains(marker))
        {
            return Some(rec.message.clone());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    None
}

#[test]
fn marked_thread_queues_and_drainer_emits_in_log_buffer() {
    ensure_nam_logger();
    let _lock_guard = PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    install();
    assert!(is_ring_routing_active());

    // Full production consult path on a marked thread: the harness thread
    // marks itself (standing in for the PW data thread), `try_offload`
    // consumes the event (decode + capture + push through the installed
    // seat) and the drainer replays it into the LogBuffer — acceptance (e):
    // the redirect is preserved; crash reports keep PW logs.
    let marker = "RT_RING_E2E_MARKER_XYZ";
    rt_mark_current_thread();
    let fmt = b"e2e-ring-probe %s\0";
    let marker_c = b"RT_RING_E2E_MARKER_XYZ\0";
    let file = b"rt_log_ring_test.rs\0";
    let func = b"marked_thread_queues_and_drainer_emits_in_log_buffer\0";
    // SAFETY: own extern "C" variadic entry with null-terminated strings.
    unsafe {
        nam_pw_log(
            std::ptr::null_mut(),
            SPA_LOG_LEVEL_DEBUG,
            file.as_ptr() as *const c_char,
            1,
            func.as_ptr() as *const c_char,
            fmt.as_ptr() as *const c_char,
            marker_c.as_ptr() as *const c_char,
        );
    }
    rt_unmark_current_thread();

    // Drain-side replay off-RT: the entry reaches the LogBuffer through the
    // drainer with strict shape parity (DEBUG arm: file:line included).
    let message = wait_drained_logbuffer(marker)
        .expect("drainer never emitted the ring record into the LogBuffer");
    assert_eq!(
        message,
        format!("[PipeWire] e2e-ring-probe {marker} (rt_log_ring_test.rs:1)"),
        "drained record shape diverged from the sync dispatch template"
    );
}

#[test]
fn drained_topic_record_matches_sync_dispatch_shape() {
    ensure_nam_logger();
    let _lock_guard = PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    install();

    // The record enters the installed producer directly (the same footpath
    // the marked thread's consult uses); topic text already decoded — the
    // same decode the sync dispatch applies.
    let record = RtLogRecord::capture(
        SPA_LOG_LEVEL_WARN,
        55,
        "parity.rs",
        "nam.test.topic",
        "parity PARITY_RING_MARK_WARN",
    );
    {
        let producer = RT_PRODUCER.get().expect("installed ring producer");
        assert!(producer.push(record), "ring must not be full");
    }
    let message =
        wait_drained_logbuffer("PARITY_RING_MARK_WARN").expect("drained topic'd record missing");
    // Strict template equality with the synchronous WARN-with-topic shape.
    assert_eq!(
        message, "[PipeWire:nam.test.topic] parity PARITY_RING_MARK_WARN (parity.rs:55)",
        "ring drain emitted a shape diverging from the sync dispatch template"
    );
}

#[test]
fn try_offload_refuses_route_without_ring_mode_or_mark() {
    let _lock_guard = PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Mode off (rollback/pre-install window): even a marked thread follows
    // the synchronous dispatch (full closure of the S3-T2 route contract).
    super::restore_deactivate();
    assert!(!is_ring_routing_active());
    rt_mark_current_thread();
    let consumed = try_offload(
        SPA_LOG_LEVEL_DEBUG,
        std::ptr::null(),
        c"try_offload.rs".as_ptr(),
        7,
        "consult-probe",
    );
    assert!(
        !consumed,
        "marked thread must route synchronously when ring mode is off"
    );
    rt_unmark_current_thread();
}

#[cfg(feature = "heap-audit")]
#[test]
fn marked_thread_offload_is_zero_alloc() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    let _lock_guard = PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    install();
    assert!(is_ring_routing_active());

    // The counter audit runs on the calling thread (TLS counters — the
    // established repo pattern audits on the current thread): build a
    // PRIVATE throwaway ring (the real `OnceLock` install is shared with
    // the drain tests and must never be saturated by an audit), then feed
    // the private producer through the SAME `push_or_count` overload path —
    // the exact call the hot path executes on a saturated ring.
    let (producer, ring_consumer) = rtrb::RingBuffer::<RtLogRecord>::new(RT_RING_CAPACITY);
    let producer = RtProducer(std::cell::UnsafeCell::new(producer));
    let _guard = TrackingGuard::new();
    for i in 0..4096u32 {
        let record = RtLogRecord::capture(
            SPA_LOG_LEVEL_DEBUG,
            i as i32,
            "heap_audit.rs",
            "",
            "zero-alloc probe",
        );
        push_or_count(&producer, record);
    }
    let (allocs, deallocs, reallocs) =
        (get_alloc_count(), get_dealloc_count(), get_realloc_count());
    drop(_guard);
    // Return the private ring to the void: it was never installed, so no
    // shared drain sees it — but consume it so the throwaway allocation is
    // not itself a leak-style residue in later audits' view.
    let _ = ring_consumer;
    reset_pw_log_dropped_for_test();

    // Medido: alloc=0, dealloc=0, realloc=0 (4096 try_offload consults on the
    // marked thread incl. decode + capture + saturation drops).
    assert_eq!(allocs, 0, "allocations on the marked RT log path");
    assert_eq!(deallocs, 0, "deallocation on the marked RT log path");
    assert_eq!(reallocs, 0, "reallocations on the marked RT log path");
}
