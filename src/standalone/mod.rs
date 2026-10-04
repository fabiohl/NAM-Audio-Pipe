// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Modules specific to standalone execution (PipeWire, CLI).

pub mod cli;
pub mod colors;
pub mod pw_host;
pub mod rt_setup;
pub mod setup;
pub mod signals;

/// Serializes tests that read or write the process-global `SHUTDOWN` flag
/// (signals_test, status_test, recording/disk_test) so concurrent test
/// threads in the same `--lib` binary can never interleave.
#[cfg(test)]
pub(crate) static SHUTDOWN_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes tests that mutate the process-global PipeWire log handler /
/// log-ring states (`HANDLER_INSTALLED`, ring routing mode, drop counter) so
/// parallel test threads in the same `--lib` binary cannot interleave
/// install/restore cycles with ring-route assertions.
#[cfg(test)]
pub(crate) static PW_LOG_REDIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
