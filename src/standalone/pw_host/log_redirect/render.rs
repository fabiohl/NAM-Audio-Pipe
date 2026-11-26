// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Decode, render, and emit internals for intercepted PipeWire logs.

use pipewire::spa::sys::{
    SPA_LOG_LEVEL_DEBUG, SPA_LOG_LEVEL_ERROR, SPA_LOG_LEVEL_INFO, SPA_LOG_LEVEL_NONE,
    SPA_LOG_LEVEL_WARN, spa_log_level, spa_log_topic,
};
use std::ffi::{CStr, c_char, c_int};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

/// Count of real libc formatter invocations, compiled only for test/test-harness
/// builds. It proves that the marked RT thread never reaches `vsnprintf`.
#[cfg(test)]
static FORMAT_CALLS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(super) fn record_format_call() {
    FORMAT_CALLS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(not(test))]
#[inline(always)]
pub(super) fn record_format_call() {}

/// Snapshot of [`FORMAT_CALLS`] for tests that pin the RT path contract.
#[cfg(test)]
pub(crate) fn format_calls_for_test() -> u64 {
    FORMAT_CALLS.load(Ordering::Relaxed)
}

/// Dispatches formatted PipeWire log message to the unified `log::*` facade.
pub(super) fn dispatch_pipewire_log(
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    _func: *const c_char,
    msg: &str,
) {
    // Scoped decodes shared with the off-RT ring drain (`rt_log_ring`), so
    // both paths print with the exact same shape by construction.
    // SAFETY: `topic`/`file` are NUL-terminated C strings supplied by PipeWire for
    // the duration of this log call; the decode helpers read them inside scoped
    // borrows only.
    unsafe {
        decode_with_topic(topic, |topic_str| {
            decode_with_file(file, |file_str| {
                emit_decoded_pipewire_log(level, topic_str, file_str, line, msg);
            });
        });
    }
}

/// Shared topic decode of an intercepted PW log event. Scoped borrow: the
/// decoded text is only usable inside the closure, so no lifetime claim
/// beyond the call is formed.
///
/// # Safety
///
/// `topic`, when non-null, must point at a `spa_log_topic` whose `topic`
/// member, when non-null, points at a valid null-terminated C string for the
/// call's lifetime — the PipeWire introspection contract upheld by every
/// caller (`dispatch_pipewire_log` and the off-RT ring drain).
#[inline]
pub(crate) unsafe fn decode_with_topic<R>(
    topic: *const spa_log_topic,
    f: impl FnOnce(&str) -> R,
) -> R {
    if topic.is_null() {
        f("")
    } else {
        // SAFETY: documented precondition (non-null topic dereference whose
        // topic text is a null-terminated C string).
        unsafe {
            let topic_ptr = (*topic).topic;
            if topic_ptr.is_null() {
                f("")
            } else {
                f(CStr::from_ptr(topic_ptr).to_str().unwrap_or(""))
            }
        }
    }
}

/// Shared file decode of an intercepted PW log event (null → `"<pipewire>"`,
/// invalid UTF-8 → `"<invalid>"`). Scoped borrow, same as the topic decode.
///
/// # Safety
///
/// `file`, when non-null, must point at a valid null-terminated C string
/// (PipeWire caller-provided log metadata).
#[inline]
pub(crate) unsafe fn decode_with_file<R>(file: *const c_char, f: impl FnOnce(&str) -> R) -> R {
    if file.is_null() {
        f("<pipewire>")
    } else {
        // SAFETY: documented precondition (null-terminated C string).
        f(unsafe { CStr::from_ptr(file) }
            .to_str()
            .unwrap_or("<invalid>"))
    }
}

/// Formats an already-decoded PW log event into a log level and formatted message string.
///
/// This is the single source of truth for the message shape used by the synchronous
/// dispatch, off-RT ring drain, and unit tests, ensuring the paths never drift apart.
pub(crate) fn render_decoded_pipewire_log(
    level: spa_log_level,
    topic_str: &str,
    file_str: &str,
    line: c_int,
    msg: &str,
) -> (log::Level, String) {
    let trimmed = msg.trim_end();
    match level {
        SPA_LOG_LEVEL_ERROR => (
            log::Level::Error,
            if topic_str.is_empty() {
                format!("[PipeWire] {} ({}:{})", trimmed, file_str, line)
            } else {
                format!(
                    "[PipeWire:{}] {} ({}:{})",
                    topic_str, trimmed, file_str, line
                )
            },
        ),
        SPA_LOG_LEVEL_WARN => (
            log::Level::Warn,
            if topic_str.is_empty() {
                format!("[PipeWire] {} ({}:{})", trimmed, file_str, line)
            } else {
                format!(
                    "[PipeWire:{}] {} ({}:{})",
                    topic_str, trimmed, file_str, line
                )
            },
        ),
        SPA_LOG_LEVEL_INFO => (
            log::Level::Info,
            if topic_str.is_empty() {
                format!("[PipeWire] {}", trimmed)
            } else {
                format!("[PipeWire:{}] {}", topic_str, trimmed)
            },
        ),
        SPA_LOG_LEVEL_DEBUG => (
            log::Level::Debug,
            if topic_str.is_empty() {
                format!("[PipeWire] {} ({}:{})", trimmed, file_str, line)
            } else {
                format!(
                    "[PipeWire:{}] {} ({}:{})",
                    topic_str, trimmed, file_str, line
                )
            },
        ),
        _ => (
            log::Level::Trace,
            if topic_str.is_empty() {
                format!("[PipeWire] {} ({}:{})", trimmed, file_str, line)
            } else {
                format!(
                    "[PipeWire:{}] {} ({}:{})",
                    topic_str, trimmed, file_str, line
                )
            },
        ),
    }
}

/// Shared emit of an already-decoded PW log event — the single source of
/// truth for dispatching into the unified `log::*` facade.
pub(crate) fn emit_decoded_pipewire_log(
    level: spa_log_level,
    topic_str: &str,
    file_str: &str,
    line: c_int,
    msg: &str,
) {
    if level == SPA_LOG_LEVEL_NONE {
        return;
    }
    let (lvl, rendered) = render_decoded_pipewire_log(level, topic_str, file_str, line, msg);
    match lvl {
        log::Level::Error => log::error!("{}", rendered),
        log::Level::Warn => log::warn!("{}", rendered),
        log::Level::Info => log::info!("{}", rendered),
        log::Level::Debug => log::debug!("{}", rendered),
        log::Level::Trace => log::trace!("{}", rendered),
    }
}

/// Converts the `vsnprintf` stack buffer into a `&str` with zero allocation.
///
/// Valid UTF-8 borrows the buffer directly; a payload containing invalid
/// bytes degrades to the static `"<non-utf8>"` fallback (the lossy bytes are
/// never materialized into a heap string — the formatter must stay
/// allocation-free even on hostile input). `None` when the buffer
/// carries no NUL terminator, which a completed `vsnprintf` cannot produce
/// but stays fail-closed anyway.
pub(super) fn stack_msg_to_str(buf: &[u8; 1024]) -> Option<&str> {
    match CStr::from_bytes_until_nul(buf) {
        Ok(cstr) => Some(std::str::from_utf8(cstr.to_bytes()).unwrap_or("<non-utf8>")),
        Err(_) => None,
    }
}
