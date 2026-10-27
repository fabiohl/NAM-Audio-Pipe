// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Fail-closed typed parsers for the release audit receipts.
//!
//! Single source of truth for the semantic receipt formats so the test
//! harnesses, the release pipeline and the CLI verifiers never drift apart:
//!
//! - [`long`] — the legacy human-readable long-audit receipt
//!   (`target/logs/long-receipt.txt`), retained as the derived view.
//! - [`jsonl`] — the unified-schema machine-readable receipt
//!   (`target/logs/long-audit-receipt.jsonl`) and the source of truth for
//!   certification. Testing-only (it reuses engine test scaffolding).
//! - [`build`]/[`release`] — typed build provenance and fail-closed release
//!   certification (`target/dist/{build,release}-receipt.json`).

pub mod long;

#[cfg(feature = "testing")]
pub mod build;

#[cfg(feature = "testing")]
pub mod jsonl;

#[cfg(feature = "testing")]
pub mod release;

/// Sibling-file carrier for typed long-audit FAIL causes. Testing-only:
/// it is written by `src/bin/pipe_long_receipt.rs` and the long suite.
#[cfg(feature = "testing")]
pub mod fail_reasons;

/// Canonical phase roster of the long-audit suite: `phase1`..`phase10` plus the
/// terminal `overall` line. Shared by the JSONL emitter/verifier and the tests.
pub const LONG_ROSTER: [&str; 11] = [
    "phase1", "phase2", "phase3", "phase4", "phase5", "phase6", "phase7", "phase8", "phase9",
    "phase10", "overall",
];
