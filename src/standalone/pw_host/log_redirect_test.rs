// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Tests for `log_redirect.rs`: level mapping, layout guards, variadic
//! handler invocations, registration lifecycle and (for F-APRT-01) the
//! zero-allocation formatting contract of the PipeWire log formatters.

use super::*;

#[test]
fn test_level_conversion() {
    assert_eq!(
        level_filter_to_spa(log::LevelFilter::Off),
        SPA_LOG_LEVEL_NONE
    );
    assert_eq!(
        level_filter_to_spa(log::LevelFilter::Error),
        SPA_LOG_LEVEL_ERROR
    );
    assert_eq!(
        level_filter_to_spa(log::LevelFilter::Warn),
        SPA_LOG_LEVEL_WARN
    );
    assert_eq!(
        level_filter_to_spa(log::LevelFilter::Info),
        SPA_LOG_LEVEL_INFO
    );
    assert_eq!(
        level_filter_to_spa(log::LevelFilter::Debug),
        SPA_LOG_LEVEL_DEBUG
    );
    assert_eq!(
        level_filter_to_spa(log::LevelFilter::Trace),
        SPA_LOG_LEVEL_TRACE
    );
}

#[test]
fn test_spa_log_struct_layout() {
    assert_eq!(std::mem::size_of::<spa_log>(), 40);
    assert_eq!(std::mem::align_of::<spa_log>(), 8);
    assert_eq!(std::mem::size_of::<spa_log_methods>(), 48);
    assert_eq!(std::mem::align_of::<spa_log_methods>(), 8);
}

#[test]
fn test_direct_variadic_log_invocations() {
    let _ring_lock = crate::standalone::PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let fmt = b"Test message with int=%d and str=%s\0";
    let str_arg = b"rust199\0";
    let file = b"log_redirect_test.rs\0";
    let func = b"test_direct_variadic_log_invocations\0";

    // SAFETY: Calling our own extern "C" variadic function with valid null-terminated strings.
    unsafe {
        nam_pw_log(
            std::ptr::null_mut(),
            SPA_LOG_LEVEL_INFO,
            file.as_ptr() as *const c_char,
            42,
            func.as_ptr() as *const c_char,
            fmt.as_ptr() as *const c_char,
            199 as c_int,
            str_arg.as_ptr() as *const c_char,
        );
    }
}

#[test]
fn test_direct_topic_variadic_log_invocations() {
    let _ring_lock = crate::standalone::PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let topic_name = b"nam.test\0";
    let topic = spa_log_topic {
        version: 0,
        topic: topic_name.as_ptr() as *const c_char,
        level: SPA_LOG_LEVEL_DEBUG,
        has_custom_level: false,
    };
    let fmt = b"Topic log with float=%.2f\0";
    let file = b"log_redirect_test.rs\0";
    let func = b"test_direct_topic_variadic_log_invocations\0";

    // SAFETY: Calling our own extern "C" variadic function with valid topic and format arguments.
    unsafe {
        nam_pw_logt(
            std::ptr::null_mut(),
            SPA_LOG_LEVEL_DEBUG,
            &raw const topic,
            file.as_ptr() as *const c_char,
            100,
            func.as_ptr() as *const c_char,
            fmt.as_ptr() as *const c_char,
            std::f64::consts::PI,
        );
    }
}

#[test]
fn test_registration_and_restore_lifecycle() {
    let _ring_lock = crate::standalone::PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    pipewire::init();

    init_pipewire_logging(log::LevelFilter::Debug);
    assert!(is_pipewire_logging_installed());

    // Emit through PipeWire's C API directly: pw_log_log
    let fmt = b"E2E PipeWire C API test: status=%s\0";
    let ok_str = b"SUCCESS\0";
    let file = b"log_redirect_test.rs\0";
    let func = b"test_registration_and_restore_lifecycle\0";

    // SAFETY: Calling pw_log_log from PipeWire C library with valid null-terminated strings.
    unsafe {
        pipewire::sys::pw_log_log(
            SPA_LOG_LEVEL_INFO,
            file.as_ptr() as *const c_char,
            123,
            func.as_ptr() as *const c_char,
            fmt.as_ptr() as *const c_char,
            ok_str.as_ptr() as *const c_char,
        );
    }

    restore_pipewire_logging();
    assert!(!is_pipewire_logging_installed());
}

#[test]
fn test_pipewire_log_captured_in_log_buffer() {
    let _ring_lock = crate::standalone::PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    pipewire::init();
    let _ = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::init(
        neural_amp_modeler_rs::common::diagnostics::logger::LoggerConfig {
            level_filter: log::LevelFilter::Trace,
            emit_stderr: false,
        },
    );
    log::set_max_level(log::LevelFilter::Trace);

    init_pipewire_logging(log::LevelFilter::Debug);

    let unique_marker = "PW_LOG_TEST_MARKER_998877";
    let fmt = b"Testing LogBuffer capture: %s\0";
    let marker_bytes = b"PW_LOG_TEST_MARKER_998877\0";
    let file = b"log_redirect_test.rs\0";
    let func = b"test_pipewire_log_captured_in_log_buffer\0";

    unsafe {
        pipewire::sys::pw_log_log(
            SPA_LOG_LEVEL_INFO,
            file.as_ptr() as *const c_char,
            456,
            func.as_ptr() as *const c_char,
            fmt.as_ptr() as *const c_char,
            marker_bytes.as_ptr() as *const c_char,
        );
    }

    restore_pipewire_logging();

    if let Some(buf) = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer() {
        let entries = buf.snapshot();
        let found = entries
            .iter()
            .any(|rec| rec.message.contains(unique_marker));
        assert!(
            found,
            "Expected marker {unique_marker} to be captured in NamLogger::log_buffer()"
        );
    }
}

#[test]
fn non_utf8_payload_falls_back_without_allocation() {
    let _ring_lock = crate::standalone::PW_LOG_REDIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    pipewire::init();
    let _ = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::init(
        neural_amp_modeler_rs::common::diagnostics::logger::LoggerConfig {
            level_filter: log::LevelFilter::Trace,
            emit_stderr: false,
        },
    );
    log::set_max_level(log::LevelFilter::Trace);
    init_pipewire_logging(log::LevelFilter::Debug);

    // Invalid UTF-8 bytes (%s copy): 0xff/0xfe are never valid UTF-8 lead bytes
    // and 0x80 without continuation is invalid too — vsnprintf copies them
    // byte-wise, so the formatter must degrade to "<non-utf8>" without
    // allocating a replacement string.
    let bad_payload: &[u8] = b"\xff\xfe\x9f broken-bytes \x80 payload\0";
    let fmt = b"pw-utf8-probe %s\0";
    let file = b"log_redirect_test.rs\0";
    let func = b"non_utf8_payload_falls_back_without_allocation\0";

    // SAFETY: Calling our own extern "C" variadic function with valid null-terminated strings.
    unsafe {
        nam_pw_log(
            std::ptr::null_mut(),
            SPA_LOG_LEVEL_INFO,
            file.as_ptr() as *const c_char,
            77,
            func.as_ptr() as *const c_char,
            fmt.as_ptr() as *const c_char,
            bad_payload.as_ptr() as *const c_char,
        );
    }

    restore_pipewire_logging();

    if let Some(buf) = neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer() {
        let entries = buf.snapshot();
        // The fallback replaces the whole message (task spec), so the probe
        // is identified by the static fallback token itself (no other test
        // produces an intercepted PW event with this shape).
        let entry = entries
            .iter()
            .find(|rec| rec.message.contains("<non-utf8>"));
        assert!(
            entry.is_some(),
            "non-UTF-8 probe message missing from LogBuffer"
        );
        let message = &entry.expect("entry checked above").message;
        assert!(
            message.starts_with("[PipeWire]"),
            "probe did not flow through the PW dispatch: {message}"
        );
        assert!(
            !message.contains('\u{fffd}'),
            "lossy replacement character leaked through: {message}"
        );
    }
}

// Zero-allocation proof of the formatter contract: runs only with the
// `heap-audit` feature (alloc_audit is consumer-gated), exercised by the
// `--lib` budget of suites that enable it.
#[cfg(feature = "heap-audit")]
#[test]
fn stack_msg_to_str_is_zero_alloc_on_valid_invalid_and_corrupt_buffers() {
    use neural_amp_modeler_rs::common::alloc_audit::{
        TrackingGuard, get_alloc_count, get_dealloc_count, get_realloc_count,
    };

    let (allocs, deallocs, reallocs) = {
        let _guard = TrackingGuard::new();
        let mut buf = [0u8; 1024];

        // Valid UTF-8 payload: borrowed &str, no allocation.
        let valid_len = b"plain utf8 message\0".len();
        buf[..valid_len].copy_from_slice(b"plain utf8 message\0");
        let got = stack_msg_to_str(&buf);
        assert_eq!(got, Some("plain utf8 message"));

        // Invalid UTF-8 bytes: static fallback, no allocation.
        let bad_len = b"\xff\xfe\x9f broken \x80 bytes\0".len();
        buf[..bad_len].copy_from_slice(b"\xff\xfe\x9f broken \x80 bytes\0");
        let got = stack_msg_to_str(&buf);
        assert_eq!(got, Some("<non-utf8>"));

        // Corrupt buffer without any NUL: fail-closed `None`, no allocation.
        buf.fill(b'x');
        let got = stack_msg_to_str(&buf);
        assert_eq!(got, None);

        (get_alloc_count(), get_dealloc_count(), get_realloc_count())
    };

    // Medido: alloc=0, dealloc=0, realloc=0 (stack_msg_to_str over 3 payload
    // shapes incl. non-UTF-8; formatter contract of F-APRT-01).
    assert_eq!(allocs, 0, "allocations in the log formatter");
    assert_eq!(deallocs, 0, "deallocation in the log formatter");
    assert_eq!(reallocs, 0, "reallocations in the log formatter");
}
