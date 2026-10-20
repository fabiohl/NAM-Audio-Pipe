// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Fail-closed typed parsers for the release audit receipts.
//!
//! Single source of truth for the semantic receipt formats so the test
//! harnesses, the release pipeline and the CLI verifiers never drift apart:
//!
//! - [`long`] — the nightly / pre-release long-audit receipt
//!   (`target/logs/long-receipt.txt`, emitted by `utils/tests-long.sh`),
//!   consumed by `tests/distribution_qa.rs` (audit suite) and by
//!   `src/bin/long_receipt_check.rs` (the strict-release semantic gate invoked
//!   by `utils/build-release.sh --release-ceremony`).
//! - [`jsonl`] — the unified-schema machine-readable receipt
//!   (`target/logs/long-audit-receipt.jsonl`), emitted alongside the `.txt`
//!   derived view and validated with the engine's canonical
//!   `testing::receipt` parser. Testing-only (it reuses engine test scaffolding).

pub mod long;

#[cfg(feature = "testing")]
pub mod jsonl;

/// Sibling-file carrier for typed long-audit FAIL causes. Testing-only:
/// it is written by `src/bin/pipe_long_receipt.rs` and the long suite.
#[cfg(feature = "testing")]
pub mod fail_reasons;

/// Canonical phase roster of the long-audit suite: `phase1`..`phase7` plus the
/// terminal `overall` line. Shared by the JSONL emitter/verifier and the tests.
pub const LONG_ROSTER: [&str; 8] = [
    "phase1", "phase2", "phase3", "phase4", "phase5", "phase6", "phase7", "overall",
];
