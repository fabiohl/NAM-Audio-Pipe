// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Shared helpers for NAM-Audio-Pipe integration test binaries.
//!
//! Single source of truth for the PipeWire daemon probe so Phase 3
//! (`pw_integration`) and Phase 4 (`recording`) can never drift apart —
//! R-17's script↔test probe-consistency guarantee depends on exactly one
//! definition of "daemon reachable".

/// Probes for a reachable PipeWire daemon via `pw-cli info 0`.
///
/// `true` only when the command succeeds — the same check
/// `utils/tests-quick.sh` uses to gate Phase 3.
pub fn probe_pipewire_daemon() -> bool {
    std::process::Command::new("pw-cli")
        .args(["info", "0"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
