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
    let topic_str = if topic.is_null() {
        ""
    } else {
        // SAFETY: `topic` is checked for non-null; `(*topic).topic` is a null-terminated
        // static string pointer provided by PipeWire.
        unsafe {
            let topic_ptr = (*topic).topic;
            if topic_ptr.is_null() {
                ""
            } else {
                CStr::from_ptr(topic_ptr).to_str().unwrap_or("")
            }
        }
    };

    let file_str = if file.is_null() {
        "<pipewire>"
    } else {
        // SAFETY: `file` is verified non-null and points to null-terminated C string from caller.
        unsafe { CStr::from_ptr(file) }
            .to_str()
            .unwrap_or("<invalid>")
    };

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
    let msg = match CStr::from_bytes_until_nul(&buf) {
        Ok(cstr) => cstr.to_string_lossy(),
        Err(_) => return,
    };
    dispatch_pipewire_log(level, topic, file, line, func, &msg);
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
    let msg = match CStr::from_bytes_until_nul(&buf) {
        Ok(cstr) => cstr.to_string_lossy(),
        Err(_) => return,
    };
    dispatch_pipewire_log(level, topic, file, line, func, &msg);
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
    log::debug!(
        "[PipeWire] Pure Rust C-ABI variadic log handler installed (level: {:?})",
        level_filter
    );
}

/// Restores PipeWire's default internal logger and unregisters the custom handler.
pub fn restore_pipewire_logging() {
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
mod tests {
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

        if let Some(buf) =
            neural_amp_modeler_rs::common::diagnostics::logger::NamLogger::log_buffer()
        {
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
}
