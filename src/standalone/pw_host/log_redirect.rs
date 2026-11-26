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

mod install;
mod render;

pub use install::{
    init_pipewire_logging, is_pipewire_logging_installed, level_filter_to_spa, nam_pw_log,
    nam_pw_logt, nam_pw_logtv, nam_pw_logv, nam_pw_topic_init, restore_pipewire_logging,
};
#[cfg(test)]
use pipewire as pw;
#[cfg(test)]
use pw::spa::sys::{
    SPA_LOG_LEVEL_DEBUG, SPA_LOG_LEVEL_ERROR, SPA_LOG_LEVEL_INFO, SPA_LOG_LEVEL_NONE,
    SPA_LOG_LEVEL_TRACE, SPA_LOG_LEVEL_WARN, spa_log, spa_log_methods, spa_log_topic,
};
#[cfg(test)]
pub(crate) use render::format_calls_for_test;
#[cfg(test)]
use std::ffi::{c_char, c_int};

#[cfg(test)]
pub(crate) use render::render_decoded_pipewire_log;
#[cfg(all(test, feature = "heap-audit"))]
use render::stack_msg_to_str;
pub(crate) use render::{decode_with_file, decode_with_topic, emit_decoded_pipewire_log};

#[cfg(test)]
#[path = "log_redirect_test.rs"]
mod tests;
