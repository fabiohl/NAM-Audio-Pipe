// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Test helper providing unified synchronization and RAII restoration for the
//! process-global `NamLogger` and `log::*` log-level filter.

use log::LevelFilter;
use neural_amp_modeler_rs::common::diagnostics::logger::{LoggerConfig, NamLogger};
use std::sync::{Mutex, MutexGuard};

/// Process-wide lock serializing all tests that mutate or depend on the
/// process-global logger level, PipeWire log redirection, or log ring state.
pub(crate) static LOGGER_STATE_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that restores previous logger levels and releases `LOGGER_STATE_LOCK` upon drop.
pub(crate) struct LoggerLevelGuard {
    _lock_guard: MutexGuard<'static, ()>,
    prev_log_level: LevelFilter,
}

impl Drop for LoggerLevelGuard {
    fn drop(&mut self) {
        if let Some(logger) = NamLogger::global() {
            logger.set_max_level(self.prev_log_level);
        }
        log::set_max_level(self.prev_log_level);
    }
}

/// Acquires the global test logger lock, initializes `NamLogger` if needed, sets
/// both `NamLogger` and `log::*` filters to `level`, and returns a RAII guard
/// restoring the prior filter on drop.
pub(crate) fn acquire_logger_level(level: LevelFilter) -> LoggerLevelGuard {
    let lock_guard = LOGGER_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev_log_level = log::max_level();

    let _ = NamLogger::init(LoggerConfig::new(level));
    if let Some(logger) = NamLogger::global() {
        logger.set_max_level(level);
    }
    log::set_max_level(level);

    LoggerLevelGuard {
        _lock_guard: lock_guard,
        prev_log_level,
    }
}

/// Executes closure `f` while holding `LOGGER_STATE_LOCK` with the logger filter
/// set to `level`. Restores the previous filter when `f` finishes or unwinds.
pub(crate) fn with_logger_level<R>(level: LevelFilter, f: impl FnOnce() -> R) -> R {
    let _guard = acquire_logger_level(level);
    f()
}
