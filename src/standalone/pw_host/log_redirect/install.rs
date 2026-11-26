// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! FFI handler installation and PipeWire logging lifecycle.

use pipewire::spa::sys::{
    SPA_LOG_LEVEL_DEBUG, SPA_LOG_LEVEL_ERROR, SPA_LOG_LEVEL_INFO, SPA_LOG_LEVEL_NONE,
    SPA_LOG_LEVEL_TRACE, SPA_LOG_LEVEL_WARN, spa_callbacks, spa_interface, spa_log, spa_log_level,
    spa_log_methods, spa_log_topic,
};
use pipewire::sys::{pw_log_set, pw_log_set_level};
use std::cell::UnsafeCell;
use std::ffi::{VaList, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};

use super::super::rt_log_ring;
use super::render::{dispatch_pipewire_log, record_format_call, stack_msg_to_str};

unsafe extern "C" {
    fn vsnprintf(str: *mut c_char, size: usize, format: *const c_char, ap: VaList) -> c_int;
}

// `spa_log_methods.logv`/`logtv` receive the platform `va_list` by value on
// AAPCS64 (aarch64) but as a pointer to the SysV register-save area on x86_64.
// The bindgen-generated type follows the target ABI, so mirror it exactly.
#[cfg(target_arch = "x86_64")]
type SpaVaList = *mut pipewire::spa::sys::__va_list_tag;
#[cfg(target_arch = "aarch64")]
type SpaVaList = pipewire::spa::sys::va_list;

// The same libc `vsnprintf` symbol is declared with two ABI-identical views:
// the C-variadic entry point above receives Rust's `VaList`, while SPA hands us
// the bindgen-generated platform `va_list`. Keep both for zero-copy dispatch.
#[expect(clashing_extern_declarations)]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
unsafe extern "C" {
    #[link_name = "vsnprintf"]
    fn vsnprintf_va(str: *mut c_char, size: usize, format: *const c_char, ap: SpaVaList) -> c_int;
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
    // Marked RT thread: never run the libc variadic formatter (glibc may
    // allocate for positional/wide arguments). Queue the raw format literal
    // instead; variadic arguments are dropped and flagged at drain time.
    // SAFETY: `fmt` is null or a valid NUL-terminated format string supplied by
    // the SPA caller for the duration of this call.
    if unsafe { rt_log_ring::try_offload_unformatted(level, topic, file, line, fmt) } {
        return;
    }
    if fmt.is_null() {
        return;
    }
    record_format_call();
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
    // Off-RT ring for the marked RT thread: `msg` is consumed by
    // the ring (queued or discarded+counted) or the sync dispatch follows as
    // before on unmarked threads / rollback mode. O(1), zero alloc, no locks.
    if rt_log_ring::try_offload(level, topic, file, line, msg) {
        return;
    }
    dispatch_pipewire_log(level, topic, file, line, func, msg);
}

/// Formats a `va_list` into a stack buffer and dispatches to logger.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[inline]
unsafe fn format_and_dispatch_va_tag(
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: SpaVaList,
) {
    // Marked RT thread: same unformatted offload contract as the variadic
    // entry point, applied before any `vsnprintf` call.
    // SAFETY: `fmt` is null or a valid NUL-terminated format string supplied by
    // the SPA caller for the duration of this call.
    if unsafe { rt_log_ring::try_offload_unformatted(level, topic, file, line, fmt) } {
        return;
    }
    if fmt.is_null() {
        return;
    }
    // On x86_64 the SysV `va_list` arrives as a pointer that may be null;
    // on aarch64 AAPCS64 passes the state struct by value.
    #[cfg(target_arch = "x86_64")]
    if args.is_null() {
        return;
    }
    record_format_call();
    let mut buf = [0u8; 1024];
    // SAFETY: `vsnprintf_va` writes at most `buf.len()` bytes; `fmt` and `args`
    // are the valid C variadic state supplied by PipeWire's SPA log dispatch.
    let written = unsafe { vsnprintf_va(buf.as_mut_ptr() as *mut c_char, buf.len(), fmt, args) };
    if written < 0 {
        return;
    }
    let Some(msg) = stack_msg_to_str(&buf) else {
        return;
    };
    // Off-RT ring for the marked RT thread, same contract as the
    // `vsnprintf` formatter above.
    if rt_log_ring::try_offload(level, topic, file, line, msg) {
        return;
    }
    dispatch_pipewire_log(level, topic, file, line, func, msg);
}

/// # Safety
/// Called only by the SPA logging system. `file`, `func` and `fmt` must be null
/// or valid NUL-terminated C strings; the variadic arguments must match the
/// conversion specifiers in `fmt`. The function never stores any of these
/// pointers, so they only need to be valid for the call.
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
/// Same contract as [`nam_pw_log`], plus `topic` must be null or point at a
/// valid `spa_log_topic` whose `topic` member (when non-null) is a
/// NUL-terminated string valid for the call. Nothing is retained.
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
/// Called only by SPA. On x86_64 `args` must be null or point at the live
/// SysV `va_list` register-save state; on aarch64 it is the platform `va_list`
/// value. It must describe the arguments matching `fmt`, and `file`/`func` must
/// be null or valid NUL-terminated C strings for the call. Nothing is retained.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub unsafe extern "C" fn nam_pw_logv(
    _object: *mut c_void,
    level: spa_log_level,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: SpaVaList,
) {
    // SAFETY: Caller provides valid format string and va_list state.
    unsafe {
        format_and_dispatch_va_tag(level, std::ptr::null(), file, line, func, fmt, args);
    }
}

/// # Safety
/// Same contract as [`nam_pw_logv`], plus `topic` must be null or point at a
/// valid `spa_log_topic` with a NUL-terminated `topic` member when non-null.
/// Nothing is retained.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub unsafe extern "C" fn nam_pw_logtv(
    _object: *mut c_void,
    level: spa_log_level,
    topic: *const spa_log_topic,
    file: *const c_char,
    line: c_int,
    func: *const c_char,
    fmt: *const c_char,
    args: SpaVaList,
) {
    // SAFETY: Caller provides valid format string and va_list state.
    unsafe {
        format_and_dispatch_va_tag(level, topic, file, line, func, fmt, args);
    }
}

/// # Safety
/// Called only by SPA. `_topic` must be null or point at a live
/// `spa_log_topic` for the duration of the call; the default policy leaves it
/// untouched and retains no pointer.
pub unsafe extern "C" fn nam_pw_topic_init(_object: *mut c_void, _topic: *mut spa_log_topic) {
    // Topics inherit global log level by default.
}

/// SPA method table containing our pure Rust function pointers.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
static SPA_LOG_METHODS: spa_log_methods = spa_log_methods {
    version: 1, // SPA_VERSION_LOG_METHODS
    log: Some(nam_pw_log),
    logv: Some(nam_pw_logv),
    logt: Some(nam_pw_logt),
    logtv: Some(nam_pw_logtv),
    topic_init: Some(nam_pw_topic_init),
};

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
static SPA_LOG_METHODS: spa_log_methods = spa_log_methods {
    version: 1, // SPA_VERSION_LOG_METHODS
    log: Some(nam_pw_log),
    logv: None,
    logt: Some(nam_pw_logt),
    logtv: None,
    topic_init: Some(nam_pw_topic_init),
};

static SPA_INTERFACE_TYPE: &[u8] = b"Spa:Interface:Log\0";

/// Wrapper providing interior mutability for the process-global `spa_log`.
///
/// PipeWire stores the pointer in its global logging slot, so the address must
/// remain stable for the whole process. The struct is initialized once as a
/// `const` static and only its `level` field is rewritten by
/// [`init_pipewire_logging`]; that write happens on the main thread during
/// logging install/restore, outside any data-loop callback.
struct SpaLogSlot(UnsafeCell<spa_log>);

// SAFETY: `spa_log` is a C struct of plain data and function pointers. The
// only writer is the main thread inside `init_pipewire_logging`, which runs
// before the streams are created and again after the host loop stops; the C
// side only reads the stable address. No Rust aliasing of the cell exists.
unsafe impl Sync for SpaLogSlot {}

/// Static `spa_log` instance registered globally with PipeWire.
static NAM_SPA_LOG: SpaLogSlot = SpaLogSlot(UnsafeCell::new(spa_log {
    iface: spa_interface {
        type_: SPA_INTERFACE_TYPE.as_ptr() as *const c_char,
        version: 0, // SPA_VERSION_LOG
        cb: spa_callbacks {
            funcs: &raw const SPA_LOG_METHODS as *const c_void,
            data: std::ptr::null_mut(),
        },
    },
    level: SPA_LOG_LEVEL_INFO,
}));

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

    // SAFETY: NAM_SPA_LOG is a `const` static with a stable address for the
    // entire process lifetime. This is the single writer of its `level` field,
    // running on the main thread during install/restore (outside the data
    // loops). SPA_LOG_METHODS is a static table of valid extern "C" pointers;
    // `pw_log_set` only stores the pointer in libpipewire's global slot.
    unsafe {
        let log_ptr = NAM_SPA_LOG.0.get();
        (*log_ptr).level = spa_level;
        pw_log_set(log_ptr);
        pw_log_set_level(spa_level);
    }

    HANDLER_INSTALLED.store(true, Ordering::Release);
    // Off-RT ring install: cold path, main thread; reads
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
