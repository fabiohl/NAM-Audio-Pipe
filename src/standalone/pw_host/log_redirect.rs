// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Pure Rust PipeWire log handler via C-ABI variadics (`...` + `VaList`).
//!
//! Intercepts internal log messages from PipeWire / SPA and redirects them into
//! the unified `log::*` facade, automatically routing them into `NamLogger`'s
//! in-memory `LogBuffer` for diagnostic bundle capture and crash reporting.
//!
//! # Architecture & Rust 1.99 Variadics
//!
//! PipeWire logging uses the Simple Plugin Architecture (SPA) `spa_log` interface,
//! whose method table (`spa_log_methods`) defines variadic callbacks:
//!
//! ```c
//! void (*log) (void *object, enum spa_log_level level, const char *file,
//!              int line, const char *func, const char *fmt, ...);
//! void (*logt) (void *object, enum spa_log_level level,
//!               const struct spa_log_topic *topic, const char *file,
//!               int line, const char *func, const char *fmt, ...);
//! ```
//!
//! Prior to Rust 1.99, exporting C-ABI variadic functions from Rust required a C
//! shim file (`.c`) compiled via `cc` or `cmake`. Rust 1.99 stabilized C-variadic
//! function definitions (`...` + `std::ffi::VaList`), allowing these callbacks to
//! be defined natively in pure Rust with zero C glue code.
//!
//! # RT Safety & Allocations
//!
//! Formatting uses POSIX `vsnprintf` into a fixed 1024-byte stack buffer, ensuring
//! zero heap allocations during log formatting.

use std::ffi::{CStr, VaList, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use pipewire::spa::sys::{
    SPA_LOG_LEVEL_DEBUG, SPA_LOG_LEVEL_ERROR, SPA_LOG_LEVEL_INFO, SPA_LOG_LEVEL_NONE,
    SPA_LOG_LEVEL_TRACE, SPA_LOG_LEVEL_WARN, spa_callbacks, spa_interface, spa_log, spa_log_level,
    spa_log_methods, spa_log_topic,
};
use pipewire::sys::{pw_log_set, pw_log_set_level};

use super::rt_log_ring;

unsafe extern "C" {
    fn vsnprintf(str: *mut c_char, size: usize, format: *const c_char, ap: VaList) -> c_int;

    fn __vsnprintf(
        str: *mut c_char,
        size: usize,
        format: *const c_char,
        ap: *mut pipewire::spa::sys::__va_list_tag,
    ) -> c_int;
}

/// Dispatches formatted PipeWire log message to the unified `log::*` facade.
fn dispatch_pipewire_log(
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    _func: *const c_char,
    msg: &str,
) {
    // Scoped decodes shared with the off-RT ring drain (`rt_log_ring`), so
    // both paths print with the exact same shape by construction.
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

/// Shared emit of an already-decoded PW log event — the single source of
/// truth for the message shape used by the synchronous dispatch and the
/// off-RT ring drain, so the two paths can never drift apart.
pub(crate) fn emit_decoded_pipewire_log(
    level: spa_log_level,
    topic_str: &str,
    file_str: &str,
    line: c_int,
    msg: &str,
) {
    let trimmed = msg.trim_end();
    match level {
        SPA_LOG_LEVEL_ERROR => {
            if topic_str.is_empty() {
                log::error!("[PipeWire] {} ({}:{})", trimmed, file_str, line);
            } else {
                log::error!(
                    "[PipeWire:{}] {} ({}:{})",
                    topic_str,
                    trimmed,
                    file_str,
                    line
                );
            }
        }
        SPA_LOG_LEVEL_WARN => {
            if topic_str.is_empty() {
                log::warn!("[PipeWire] {} ({}:{})", trimmed, file_str, line);
            } else {
                log::warn!(
                    "[PipeWire:{}] {} ({}:{})",
                    topic_str,
                    trimmed,
                    file_str,
                    line
                );
            }
        }
        SPA_LOG_LEVEL_INFO => {
            if topic_str.is_empty() {
                log::info!("[PipeWire] {}", trimmed);
            } else {
                log::info!("[PipeWire:{}] {}", topic_str, trimmed);
            }
        }
        SPA_LOG_LEVEL_DEBUG => {
            if topic_str.is_empty() {
                log::debug!("[PipeWire] {} ({}:{})", trimmed, file_str, line);
            } else {
                log::debug!(
                    "[PipeWire:{}] {} ({}:{})",
                    topic_str,
                    trimmed,
                    file_str,
                    line
                );
            }
        }
        SPA_LOG_LEVEL_TRACE => {
            if topic_str.is_empty() {
                log::trace!("[PipeWire] {} ({}:{})", trimmed, file_str, line);
            } else {
                log::trace!(
                    "[PipeWire:{}] {} ({}:{})",
                    topic_str,
                    trimmed,
                    file_str,
                    line
                );
            }
        }
        _ => {}
    }
}

/// Converts the `vsnprintf` stack buffer into a `&str` with zero allocation.
///
/// Valid UTF-8 borrows the buffer directly; a payload containing invalid
/// bytes degrades to the static `"<non-utf8>"` fallback (the lossy bytes are
/// never materialized into a heap string — the formatter must stay
/// allocation-free even on hostile input, F-APRT-01). `None` when the buffer
/// carries no NUL terminator, which a completed `vsnprintf` cannot produce
/// but stays fail-closed anyway.
fn stack_msg_to_str(buf: &[u8; 1024]) -> Option<&str> {
    match CStr::from_bytes_until_nul(buf) {
        Ok(cstr) => Some(std::str::from_utf8(cstr.to_bytes()).unwrap_or("<non-utf8>")),
        Err(_) => None,
    }
}

/// Formats variadic C arguments into a stack buffer and dispatches to logger.
#[inline]
unsafe fn format_and_dispatch_variadic(
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: VaList,
) {
    if fmt.is_null() {
        return;
    }
    let mut buf = [0u8; 1024];
    // SAFETY: vsnprintf writes at most buf.len() bytes into buf.
    // fmt and args are valid C variadic parameters passed by caller.
    let written = unsafe { vsnprintf(buf.as_mut_ptr() as *mut c_char, buf.len(), fmt, args) };
    if written < 0 {
        return;
    }
    let Some(msg) = stack_msg_to_str(&buf) else {
        return;
    };
    // Off-RT ring for the marked RT thread (F-APRT-01): `msg` is consumed by
    // the ring (queued or discarded+counted) or the sync dispatch follows as
    // before on unmarked threads / rollback mode. O(1), zero alloc, no locks.
    if rt_log_ring::try_offload(level, topic, file, line, msg) {
        return;
    }
    dispatch_pipewire_log(level, topic, file, line, func, msg);
}

/// Formats `va_list` tag pointer into a stack buffer and dispatches to logger.
#[inline]
unsafe fn format_and_dispatch_va_tag(
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: *mut pipewire::spa::sys::__va_list_tag,
) {
    if fmt.is_null() || args.is_null() {
        return;
    }
    let mut buf = [0u8; 1024];
    // SAFETY: __vsnprintf writes at most buf.len() bytes into buf.
    let written = unsafe { __vsnprintf(buf.as_mut_ptr() as *mut c_char, buf.len(), fmt, args) };
    if written < 0 {
        return;
    }
    let Some(msg) = stack_msg_to_str(&buf) else {
        return;
    };
    // Off-RT ring for the marked RT thread (F-APRT-01), same contract as the
    // `vsnprintf` formatter above.
    if rt_log_ring::try_offload(level, topic, file, line, msg) {
        return;
    }
    dispatch_pipewire_log(level, topic, file, line, func, msg);
}

/// # Safety
/// Called by PipeWire SPA logging system for untopic'd variadic logging (v0).
pub unsafe extern "C" fn nam_pw_log(
    _object: *mut c_void,
    level: spa_log_level,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: ...
) {
    // SAFETY: Caller provides valid format string and variadic arguments.
    unsafe {
        format_and_dispatch_variadic(level, std::ptr::null(), file, line, func, fmt, args);
    }
}

/// # Safety
/// Called by PipeWire SPA logging system for topic'd variadic logging (v1+).
pub unsafe extern "C" fn nam_pw_logt(
    _object: *mut c_void,
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: ...
) {
    // SAFETY: Caller provides valid format string and variadic arguments.
    unsafe {
        format_and_dispatch_variadic(level, topic, file, line, func, fmt, args);
    }
}

/// # Safety
/// Called by PipeWire SPA logging system for untopic'd `va_list` logging (v0).
pub unsafe extern "C" fn nam_pw_logv(
    _object: *mut c_void,
    level: spa_log_level,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: *mut pipewire::spa::sys::__va_list_tag,
) {
    // SAFETY: Caller provides valid format string and va_list pointer.
    unsafe {
        format_and_dispatch_va_tag(level, std::ptr::null(), file, line, func, fmt, args);
    }
}

/// # Safety
/// Called by PipeWire SPA logging system for topic'd `va_list` logging (v1+).
pub unsafe extern "C" fn nam_pw_logtv(
    _object: *mut c_void,
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: *mut pipewire::spa::sys::__va_list_tag,
) {
    // SAFETY: Caller provides valid format string and va_list pointer.
    unsafe {
        format_and_dispatch_va_tag(level, topic, file, line, func, fmt, args);
    }
}

/// # Safety
/// Called by PipeWire to initialize log topics.
pub unsafe extern "C" fn nam_pw_topic_init(_object: *mut c_void, _topic: *mut spa_log_topic) {
    // Topics inherit global log level by default.
}

/// SPA method table containing our pure Rust function pointers.
static SPA_LOG_METHODS: spa_log_methods = spa_log_methods {
    version: 1, // SPA_VERSION_LOG_METHODS
    log: Some(nam_pw_log),
    logv: Some(nam_pw_logv),
    logt: Some(nam_pw_logt),
    logtv: Some(nam_pw_logtv),
    topic_init: Some(nam_pw_topic_init),
};

static SPA_INTERFACE_TYPE: &[u8] = b"Spa:Interface:Log\0";

/// Static `spa_log` instance registered globally with PipeWire.
static mut NAM_SPA_LOG: spa_log = spa_log {
    iface: spa_interface {
        type_: SPA_INTERFACE_TYPE.as_ptr() as *const c_char,
        version: 0, // SPA_VERSION_LOG
        cb: spa_callbacks {
            funcs: &raw const SPA_LOG_METHODS as *const c_void,
            data: std::ptr::null_mut(),
        },
    },
    level: SPA_LOG_LEVEL_INFO,
};

static HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Converts a standard `log::LevelFilter` to its corresponding `spa_log_level`.
#[must_use]
pub fn level_filter_to_spa(filter: log::LevelFilter) -> spa_log_level {
    match filter {
        log::LevelFilter::Off => SPA_LOG_LEVEL_NONE,
        log::LevelFilter::Error => SPA_LOG_LEVEL_ERROR,
        log::LevelFilter::Warn => SPA_LOG_LEVEL_WARN,
        log::LevelFilter::Info => SPA_LOG_LEVEL_INFO,
        log::LevelFilter::Debug => SPA_LOG_LEVEL_DEBUG,
        log::LevelFilter::Trace => SPA_LOG_LEVEL_TRACE,
    }
}

/// Initializes PipeWire logging redirect, registering our pure-Rust C-ABI variadic handler.
///
/// All internal PipeWire logs (`pw_log_*`, `spa_log_*`) will be routed through
/// Rust's unified `log::*` facade at the matching level.
pub fn init_pipewire_logging(level_filter: log::LevelFilter) {
    let spa_level = level_filter_to_spa(level_filter);

    // SAFETY: NAM_SPA_LOG is static and lives for the entire process lifetime.
    // SPA_LOG_METHODS is static and contains valid extern "C" function pointers.
    // pw_log_set installs the pointer in libpipewire's global logging slot.
    unsafe {
        let log_ptr = std::ptr::addr_of_mut!(NAM_SPA_LOG);
        (*log_ptr).level = spa_level;
        pw_log_set(log_ptr);
        pw_log_set_level(spa_level);
    }

    HANDLER_INSTALLED.store(true, Ordering::Release);
    // Off-RT ring install (F-APRT-01): cold path, main thread; reads
    // `NAM_PW_LOG_SYNC` and refuses re-install; the marked data thread will
    // start routing through the ring instead of the synchronous dispatch.
    rt_log_ring::install();
    log::debug!(
        "[PipeWire] Pure Rust C-ABI variadic log handler installed (level: {:?})",
        level_filter
    );
}

/// Restores PipeWire's default internal logger and unregisters the custom handler.
pub fn restore_pipewire_logging() {
    // Deactivate the ring routing FIRST so no marked-thread event attempts
    // en route while the global handler is being removed.
    rt_log_ring::restore_deactivate();
    if HANDLER_INSTALLED.swap(false, Ordering::AcqRel) {
        // SAFETY: Passing null to pw_log_set resets PipeWire to its internal default logger.
        unsafe {
            pw_log_set(std::ptr::null_mut());
        }
        log::debug!("[PipeWire] Restored default PipeWire logger");
    }
}

/// Returns whether the custom PipeWire log handler is currently active.
#[must_use]
pub fn is_pipewire_logging_installed() -> bool {
    HANDLER_INSTALLED.load(Ordering::Acquire)
}

#[cfg(test)]
#[path = "log_redirect_test.rs"]
mod tests;
