// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Unified-schema machine-readable long-audit receipt
//! (`target/logs/long-audit-receipt.jsonl`).
//!
//! `utils/tests-long.sh` emits this JSONL as the **source of truth** using the
//! same [`neural_amp_modeler_rs::testing::receipt`] schema the engine and
//! NAM-Plug emit; the human `.txt` receipt (`receipt::long`) is the derived
//! view kept for compatibility during the transition. Both readers must agree —
//! the release ceremony verifies both and rejects any divergence or tampering.

use std::path::Path;

pub use neural_amp_modeler_rs::testing::receipt::LongAuditReceipt;

use crate::receipt::LONG_ROSTER;

/// Fail-closed semantic verification of the unified-schema JSONL receipt against
/// the Pipe roster (structure + declared/derived `OVERALL` consistency).
pub fn verify_long_jsonl(path: &Path) -> Result<LongAuditReceipt, String> {
    let audit = LongAuditReceipt::parse_jsonl_file(path).map_err(|e| {
        format!(
            "unified long receipt {} is not parseable: {e}",
            path.display()
        )
    })?;
    audit
        .semantic_verdict_with_roster(&LONG_ROSTER)
        .map_err(|e| {
            format!(
                "unified long receipt {} failed semantic verification: {e}",
                path.display()
            )
        })?;
    Ok(audit)
}

/// Strict (release-ceremony) semantic verification of the unified JSONL receipt:
/// additionally requires `OVERALL: PASSED` with no declared gap or failure.
pub fn verify_long_jsonl_strict(path: &Path) -> Result<LongAuditReceipt, String> {
    let audit = LongAuditReceipt::parse_jsonl_file(path).map_err(|e| {
        format!(
            "unified long receipt {} is not parseable: {e}",
            path.display()
        )
    })?;
    audit
        .campaign_strict_verdict_with_roster(&LONG_ROSTER)
        .map_err(|e| {
            format!(
                "unified long receipt {} failed strict release certification: {e}",
                path.display()
            )
        })?;
    Ok(audit)
}
