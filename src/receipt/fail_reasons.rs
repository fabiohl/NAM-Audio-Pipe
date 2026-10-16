// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Sibling-file carrier for typed long-audit FAIL causes (D3).
//!
//! The unified-schema receipt (`target/logs/long-audit-receipt.jsonl`) is
//! engine-owned (`neural_amp_modeler_rs::testing::receipt::LongPhaseReceipt`)
//! and carries no `fail_reason` field. Rather than fork or pollute that schema,
//! a FAILED phase writes its typed cause to a sibling JSONL file next to the
//! unified receipt — `<out with .jsonl stripped>.fail-reasons.json` — so the
//! unified JSONL stays byte-compatible with the engine parser (unknown fields
//! are never injected) and old receipts remain valid when the sibling is
//! absent. A generic upstream proposal can later promote the field into the
//! shared schema.
//!
//! Each line: `{"phase_id":"phase4","fail_reason":"...","timestamp":"..."}`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Sibling fail-reasons path derived from the unified JSONL path:
/// `target/logs/long-audit-receipt.jsonl` →
/// `target/logs/long-audit-receipt.fail-reasons.json`.
pub fn path(out: &str) -> PathBuf {
    let p = Path::new(out);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("long-audit-receipt");
    p.with_file_name(format!("{stem}.fail-reasons.json"))
}

/// One typed FAIL cause for a long-audit phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailReasonRecord {
    /// Phase identifier (e.g. `phase4`).
    pub phase_id: String,
    /// First diagnostic line extracted from the phase log.
    pub fail_reason: String,
    /// ISO-8601 emission timestamp (UTC).
    pub timestamp: String,
}

impl FailReasonRecord {
    /// Serializes this record as one JSONL line (no trailing newline).
    pub fn render_jsonl_line(&self) -> String {
        serde_json::json!({
            "phase_id": self.phase_id,
            "fail_reason": self.fail_reason,
            "timestamp": self.timestamp,
        })
        .to_string()
    }

    /// Parses one JSONL line back into a record (fail-closed on missing fields).
    pub fn parse_jsonl_line(line: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("invalid JSON: {e}"))?;
        let phase_id = value
            .get("phase_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing string field `phase_id`".to_string())?;
        let fail_reason = value
            .get("fail_reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing string field `fail_reason`".to_string())?;
        let timestamp = value
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        Ok(FailReasonRecord {
            phase_id: phase_id.to_string(),
            fail_reason: fail_reason.to_string(),
            timestamp: timestamp.to_string(),
        })
    }
}

/// Appends one fail-reason record to the sibling file of `out`, creating parent
/// directories on demand.
pub fn append(out: &str, record: &FailReasonRecord) -> std::io::Result<()> {
    let target = path(out);
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&target)?;
    file.write_all(record.render_jsonl_line().as_bytes())?;
    file.write_all(b"\n")
}

/// Loads every record from the sibling file. A missing sibling is not an error
/// (no FAIL occurred, or the receipt predates this carrier) and yields an empty
/// vector; a malformed line is a hard error so the validator fails closed.
pub fn load(out: &str) -> Result<Vec<FailReasonRecord>, String> {
    let target = path(out);
    if !target.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&target)
        .map_err(|e| format!("cannot read {}: {e}", target.display()))?;
    let mut records = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record = FailReasonRecord::parse_jsonl_line(line)
            .map_err(|e| format!("{} line {}: {e}", target.display(), idx + 1))?;
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_out(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "pipe-fail-reasons-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        dir.join("long-audit-receipt.jsonl")
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn path_is_the_jsonl_sibling() {
        assert_eq!(
            path("target/logs/long-audit-receipt.jsonl"),
            PathBuf::from("target/logs/long-audit-receipt.fail-reasons.json")
        );
    }

    #[test]
    fn render_parse_roundtrip_escapes_special_characters() {
        let record = FailReasonRecord {
            phase_id: "phase4".to_string(),
            fail_reason: "panicked at \"x\": assertion `a` != \"b\"\\n".to_string(),
            timestamp: "2026-10-16T00:00:00Z".to_string(),
        };
        let line = record.render_jsonl_line();
        assert!(!line.contains('\n'), "JSONL line must be single-line");
        let parsed = FailReasonRecord::parse_jsonl_line(&line).expect("roundtrip");
        assert_eq!(parsed, record);
    }

    #[test]
    fn fail_with_reason_is_recorded_and_reloaded() {
        let out = temp_out("with-reason");
        append(
            &out,
            &FailReasonRecord {
                phase_id: "phase4".to_string(),
                fail_reason: "TEST_RESULT[rt_jitter]=FAIL max_jitter_us=999".to_string(),
                timestamp: "2026-10-16T00:00:00Z".to_string(),
            },
        )
        .expect("append");
        let loaded = load(&out).expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].phase_id, "phase4");
        assert!(loaded[0].fail_reason.contains("rt_jitter"));
        let _ = fs::remove_dir_all(Path::new(&out).parent().unwrap());
    }

    #[test]
    fn absent_sibling_is_accepted_as_empty() {
        let out = temp_out("absent");
        // No append: a receipt without FAILs (or a legacy receipt) stays valid.
        assert!(load(&out).expect("load").is_empty());
    }

    #[test]
    fn malformed_line_fails_closed() {
        let out = temp_out("malformed");
        append(
            &out,
            &FailReasonRecord {
                phase_id: "phase4".to_string(),
                fail_reason: "boom".to_string(),
                timestamp: String::new(),
            },
        )
        .expect("append");
        let sibling = path(&out);
        fs::write(&sibling, "{\"phase_id\":\"phase4\"}\n").expect("write malformed");
        let err = load(&out).expect_err("missing fail_reason must be rejected");
        assert!(err.contains("fail_reason"), "got: {err}");
        let _ = fs::remove_dir_all(Path::new(&out).parent().unwrap());
    }
}
