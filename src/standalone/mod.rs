// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Modules specific to standalone execution (PipeWire, CLI).

pub mod banner;
pub mod cli;
pub mod colors;
pub mod exit;
pub mod launcher;
pub mod pw_host;
pub mod rt_setup;
pub mod setup;
pub mod signals;
#[cfg(test)]
pub(crate) mod test_logger;

/// Serializes tests that read or write the process-global `SHUTDOWN` flag
/// (signals_test, status_test, recording/disk_test) so concurrent test
/// threads in the same `--lib` binary can never interleave.
#[cfg(test)]
pub(crate) static SHUTDOWN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes tests that mutate the process-global PipeWire log handler /
/// log-ring states (`HANDLER_INSTALLED`, ring routing mode, drop counter) or
/// log levels so parallel test threads in the same `--lib` binary cannot interleave.
///
/// Aliased directly to [`test_logger::LOGGER_STATE_LOCK`] to guarantee that
/// PipeWire redirect/ring state tests and logger state acquisitions share the
/// same underlying mutex and cannot deadlock or interleave.
#[cfg(test)]
pub(crate) static PW_LOG_REDIR_TEST_LOCK: &std::sync::Mutex<()> = &test_logger::LOGGER_STATE_LOCK;
