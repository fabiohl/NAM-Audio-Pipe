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

pub mod long;
