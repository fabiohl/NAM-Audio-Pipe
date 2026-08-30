// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Fail-closed semantic parser for the long-suite receipt
//! (`target/logs/long-receipt.txt`, emitted by `utils/tests-long.sh`).
//!
//! This is the single source of truth for the long-receipt format, shared by:
//!
//! - `tests/distribution_qa.rs` — the ER-6 certification audit of the runner
//!   and its receipt (`parse_long_receipt` + [`LongReceipt::audit`]);
//! - `src/bin/long_receipt_check.rs` — the strict-release semantic gate
//!   invoked by `utils/build-release.sh --release-ceremony` (T5.1 / T8.1),
//!   which verifies the receipt via [`verify_release_certification_file`]
//!   instead of a substring search.
//!
//! The format:
//!
//! ```text
//! SUITE: tests-long
//! STRICT: 0|1
//! NAM_RT_STRICT: 0|1               (T5.1: strict-mode propagation evidence; optional)
//! MODE: simulate|full
//! SOAK_PURPOSE: accelerated_timeline ...     (T5.3: purpose of the accelerated soak; optional)
//! ENDURANCE_PURPOSE: real_wall_clock ...     (T5.3: purpose of the real endurance; optional)
//! PHASE1: PASS|FAIL|GAP|SIMULATED log=target/logs/... [duration_ms=N]
//! ... (PHASE2..PHASE6; PHASE6 = real wall-clock endurance, T5.3)
//! GAP: <typed reason>            (zero or more)
//! OVERALL: PASSED|FAILED|COMPLETED_WITH_GAPS|SIMULATED
//! ```
//!
//! Unknown line types, duplicated mandatory fields, invalid values, a missing
//! mandatory phase or a missing `OVERALL:` verdict are all rejected — a receipt
//! that cannot be parsed fail-closed must never certify anything. The T5.3
//! purpose lines and `PHASE6` are optional at parse level (receipts written
//! before the field existed remain parseable) but are **required** by strict
//! release certification (T5.3 / G-PERF-004): a certified receipt must declare
//! the purpose of each soak suite and close the real endurance phase.

use std::path::Path;

/// Status of one long-audit phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongPhaseStatus {
    Pass,
    Fail,
    Gap,
    Simulated,
}

/// One `PHASEn: <status> log=... duration_ms=...` line of the long receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LongPhaseResult {
    /// Phase id, e.g. `PHASE3`.
    pub id: String,
    /// Typed phase verdict.
    pub status: LongPhaseStatus,
    /// Log file path declared by the line (`log=...`).
    pub log: String,
    /// Optional measured duration (`duration_ms=...`).
    pub duration_ms: Option<u64>,
}

/// Fail-closed parsed representation of `target/logs/long-receipt.txt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LongReceipt {
    /// `SUITE:` value (must be `tests-long` for certification).
    pub suite: String,
    /// `STRICT:` flag (release gate only passes with `1`).
    pub strict: bool,
    /// `MODE:` value (`simulate` or `full`).
    pub mode: String,
    /// `NAM_RT_STRICT:` propagation evidence (T5.1). `None` when the line is
    /// absent (receipts written before the field existed remain parseable).
    pub nam_rt_strict: Option<bool>,
    /// `SOAK_PURPOSE:` declaration (T5.3) — the accelerated-timeline soak's
    /// purpose. `None` when absent (optional at parse level).
    pub soak_purpose: Option<String>,
    /// `ENDURANCE_PURPOSE:` declaration (T5.3) — the real wall-clock
    /// endurance's purpose. `None` when absent (optional at parse level).
    pub endurance_purpose: Option<String>,
    /// Parsed phase lines (`PHASE1..PHASE5` mandatory, `PHASE6` since T5.3).
    pub phases: Vec<LongPhaseResult>,
    /// Typed `GAP:` reasons.
    pub gaps: Vec<String>,
    /// `OVERALL:` verdict.
    pub overall: String,
}

/// Canonical prefix every `SOAK_PURPOSE:` line must carry (T5.3).
pub const SOAK_PURPOSE_TOKEN: &str = "accelerated_timeline";
/// Canonical prefix every `ENDURANCE_PURPOSE:` line must carry (T5.3).
pub const ENDURANCE_PURPOSE_TOKEN: &str = "real_wall_clock";

/// Fail-closed whole-token validation for the purpose declarations (T5.3).
///
/// The runner emits `accelerated_timeline — <description>`, so the canonical
/// token must be followed by whitespace, an em-dash or end-of-line — a crafted
/// value that merely *starts with* the token (e.g. `accelerated_timelineX`)
/// must never certify. Mirrors the strict value-checking of `STRICT`/`MODE`.
pub fn purpose_token_valid(value: &str, token: &str) -> bool {
    value == token
        || value.starts_with(token)
            && value[token.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace() || c == '—' || c == '-')
}

/// Parses one `PHASEn: <status> log=... duration_ms=...` line. `None` when the
/// line is not a phase line or is structurally malformed (unknown status
/// token, missing `log=` attribute).
fn parse_long_phase(line: &str) -> Option<(String, LongPhaseStatus, String, Option<u64>)> {
    let rest = line.strip_prefix("PHASE")?;
    let (num, tail) = rest.split_once(':')?;
    let id = format!("PHASE{num}");
    let mut tokens = tail.split_whitespace();
    let status = match tokens.next()? {
        "PASS" => LongPhaseStatus::Pass,
        "FAIL" => LongPhaseStatus::Fail,
        "GAP" => LongPhaseStatus::Gap,
        "SIMULATED" => LongPhaseStatus::Simulated,
        _ => return None,
    };
    let mut log = String::new();
    let mut duration_ms = None;
    for tok in tokens {
        if let Some(v) = tok.strip_prefix("log=") {
            log = v.to_string();
        } else if let Some(v) = tok.strip_prefix("duration_ms=") {
            duration_ms = v.parse().ok();
        }
    }
    Some((id, status, log, duration_ms))
}

/// Fail-closed parser for the long-suite receipt format
/// (`target/logs/long-receipt.txt`, emitted by `utils/tests-long.sh`).
///
/// Unknown line types, duplicated mandatory fields, invalid values, a missing
/// mandatory phase or a missing `OVERALL:` verdict are all rejected — a receipt
/// that cannot be parsed fail-closed must never certify anything.
pub fn parse_long_receipt(text: &str) -> Result<LongReceipt, String> {
    let mut suite: Option<String> = None;
    let mut strict: Option<bool> = None;
    let mut mode: Option<String> = None;
    let mut nam_rt_strict: Option<bool> = None;
    let mut soak_purpose: Option<String> = None;
    let mut endurance_purpose: Option<String> = None;
    let mut phases: Vec<LongPhaseResult> = Vec::new();
    let mut gaps: Vec<String> = Vec::new();
    let mut overall: Option<String> = None;

    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let lineno = idx + 1;
        if let Some(v) = line.strip_prefix("SUITE:") {
            let v = v.trim();
            if suite.replace(v.to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate SUITE: line"));
            }
        } else if let Some(v) = line.strip_prefix("STRICT:") {
            let parsed = match v.trim() {
                "0" => false,
                "1" => true,
                other => {
                    return Err(format!(
                        "line {lineno}: invalid STRICT: value {other:?} (expected 0 or 1)"
                    ));
                }
            };
            if strict.replace(parsed).is_some() {
                return Err(format!("line {lineno}: duplicate STRICT: line"));
            }
        } else if let Some(v) = line.strip_prefix("NAM_RT_STRICT:") {
            let parsed = match v.trim() {
                "0" => false,
                "1" => true,
                other => {
                    return Err(format!(
                        "line {lineno}: invalid NAM_RT_STRICT: value {other:?} (expected 0 or 1)"
                    ));
                }
            };
            if nam_rt_strict.replace(parsed).is_some() {
                return Err(format!("line {lineno}: duplicate NAM_RT_STRICT: line"));
            }
        } else if let Some(v) = line.strip_prefix("SOAK_PURPOSE:") {
            let v = v.trim();
            if !purpose_token_valid(v, SOAK_PURPOSE_TOKEN) {
                return Err(format!(
                    "line {lineno}: invalid SOAK_PURPOSE: value {v:?} (must start with the \
                     whole token {SOAK_PURPOSE_TOKEN:?} followed by a separator)"
                ));
            }
            if soak_purpose.replace(v.to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate SOAK_PURPOSE: line"));
            }
        } else if let Some(v) = line.strip_prefix("ENDURANCE_PURPOSE:") {
            let v = v.trim();
            if !purpose_token_valid(v, ENDURANCE_PURPOSE_TOKEN) {
                return Err(format!(
                    "line {lineno}: invalid ENDURANCE_PURPOSE: value {v:?} (must start with the \
                     whole token {ENDURANCE_PURPOSE_TOKEN:?} followed by a separator)"
                ));
            }
            if endurance_purpose.replace(v.to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate ENDURANCE_PURPOSE: line"));
            }
        } else if let Some(v) = line.strip_prefix("MODE:") {
            let v = v.trim();
            if v != "simulate" && v != "full" {
                return Err(format!(
                    "line {lineno}: invalid MODE: value {v:?} (expected simulate or full)"
                ));
            }
            if mode.replace(v.to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate MODE: line"));
            }
        } else if let Some(v) = line.strip_prefix("OVERALL:") {
            let v = v.trim();
            if !matches!(v, "PASSED" | "FAILED" | "COMPLETED_WITH_GAPS" | "SIMULATED") {
                return Err(format!("line {lineno}: invalid OVERALL: verdict {v:?}"));
            }
            if overall.replace(v.to_string()).is_some() {
                return Err(format!("line {lineno}: duplicate OVERALL: line"));
            }
        } else if let Some(v) = line.strip_prefix("GAP:") {
            gaps.push(v.trim().to_string());
        } else if line.starts_with("PHASE") {
            let (id, status, log, duration_ms) = parse_long_phase(line)
                .ok_or_else(|| format!("line {lineno}: malformed phase line: {line:?}"))?;
            phases.push(LongPhaseResult {
                id,
                status,
                log,
                duration_ms,
            });
        } else {
            return Err(format!(
                "line {lineno}: unrecognized receipt line: {line:?}"
            ));
        }
    }

    let suite = suite.ok_or("missing SUITE: line")?;
    let strict = strict.ok_or("missing STRICT: line")?;
    let mode = mode.ok_or("missing MODE: line")?;
    let overall = overall.ok_or("missing OVERALL: line")?;

    for n in 1..=5 {
        let id = format!("PHASE{n}");
        if !phases.iter().any(|p| p.id == id) {
            return Err(format!("missing mandatory phase {id} in receipt"));
        }
    }

    Ok(LongReceipt {
        suite,
        strict,
        mode,
        nam_rt_strict,
        soak_purpose,
        endurance_purpose,
        phases,
        gaps,
        overall,
    })
}

impl LongReceipt {
    /// Semantic consistency audit (fail-closed): the `OVERALL:` verdict must
    /// match the phase/gap evidence collected in the same receipt, mirroring
    /// the runner's verdict logic — a `PASSED` receipt may not hide a GAP
    /// phase, a `COMPLETED_WITH_GAPS` must be backed by gap evidence, and a
    /// simulate run must close as `SIMULATED` with only simulated phases.
    /// Additionally (T5.1) a receipt claiming `STRICT: 1` must not record
    /// `NAM_RT_STRICT: 0` — a strict run without propagation evidence is
    /// internally inconsistent.
    pub fn audit(&self) -> Result<(), String> {
        if self.strict && self.nam_rt_strict == Some(false) {
            return Err(
                "inconsistent receipt: STRICT: 1 but NAM_RT_STRICT: 0 (strict propagation not wired)"
                    .into(),
            );
        }
        if self.mode == "simulate" {
            if self.overall != "SIMULATED" {
                return Err(format!(
                    "simulate receipt must close with OVERALL: SIMULATED, got {:?}",
                    self.overall
                ));
            }
            if !self
                .phases
                .iter()
                .all(|p| p.status == LongPhaseStatus::Simulated)
            {
                return Err("simulate receipt must carry only SIMULATED phase statuses".into());
            }
            return Ok(());
        }

        let has_fail = self
            .phases
            .iter()
            .any(|p| p.status == LongPhaseStatus::Fail);
        let has_gap =
            self.phases.iter().any(|p| p.status == LongPhaseStatus::Gap) || !self.gaps.is_empty();
        match self.overall.as_str() {
            "PASSED" if !has_fail && !has_gap => Ok(()),
            "FAILED" if has_fail => Ok(()),
            "COMPLETED_WITH_GAPS" if has_gap && !has_fail => Ok(()),
            other => Err(format!(
                "inconsistent receipt: OVERALL={other} (fail_phase={has_fail}, gap_evidence={has_gap})"
            )),
        }
    }

    /// Verifies if the receipt meets strict release certification requirements
    /// (T8.1 + T5.1 + T5.3): `SUITE: tests-long`, `STRICT: 1`,
    /// `NAM_RT_STRICT: 1` (propagation evidence), `MODE: full`,
    /// `OVERALL: PASSED`, a PASSED `PHASE6` (real wall-clock endurance, T5.3)
    /// and the declared purpose of each soak suite (`SOAK_PURPOSE:`
    /// `accelerated_timeline` + `ENDURANCE_PURPOSE:` `real_wall_clock`).
    pub fn verify_release_certification(&self) -> Result<(), String> {
        self.audit()?;
        if self.suite != "tests-long" {
            return Err(format!("expected SUITE: tests-long, got {:?}", self.suite));
        }
        if !self.strict {
            return Err("release certification requires STRICT: 1 (got STRICT: 0)".into());
        }
        if self.mode != "full" {
            return Err(format!(
                "release certification requires MODE: full (got {:?})",
                self.mode
            ));
        }
        if self.overall != "PASSED" {
            return Err(format!(
                "release certification requires OVERALL: PASSED (got {:?})",
                self.overall
            ));
        }
        if self.nam_rt_strict != Some(true) {
            return Err(
                "release certification requires NAM_RT_STRICT: 1 (the strict run must propagate NAM_RT_STRICT=1 to the RT harness — T5.1)"
                    .into(),
            );
        }
        let phase6 = self
            .phases
            .iter()
            .find(|p| p.id == "PHASE6")
            .ok_or("release certification requires PHASE6 (real wall-clock endurance, T5.3)")?;
        if phase6.status != LongPhaseStatus::Pass {
            return Err(format!(
                "release certification requires PHASE6: PASS (got {:?})",
                phase6.status
            ));
        }
        let soak = self.soak_purpose.as_deref().unwrap_or_default();
        if !purpose_token_valid(soak, SOAK_PURPOSE_TOKEN) {
            return Err(
                "release certification requires SOAK_PURPOSE: accelerated_timeline (T5.3 — the \
                 accelerated soak's purpose must be declared in the receipt)"
                    .into(),
            );
        }
        let endurance = self.endurance_purpose.as_deref().unwrap_or_default();
        if !purpose_token_valid(endurance, ENDURANCE_PURPOSE_TOKEN) {
            return Err(
                "release certification requires ENDURANCE_PURPOSE: real_wall_clock (T5.3 — the \
                 real endurance's purpose must be declared in the receipt)"
                    .into(),
            );
        }
        Ok(())
    }
}

/// Reads `path`, parses the long-suite receipt fail-closed and verifies strict
/// release certification. Entry point for `src/bin/long_receipt_check.rs` and
/// any non-executing structural validation of `utils/tests-long.sh`.
pub fn verify_release_certification_file(path: &Path) -> Result<LongReceipt, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read long-suite receipt {}: {e}", path.display()))?;
    let receipt = parse_long_receipt(&text).map_err(|e| {
        format!(
            "long-suite receipt {} is not parseable: {e}",
            path.display()
        )
    })?;
    receipt.verify_release_certification().map_err(|e| {
        format!(
            "long-suite receipt {} failed strict release certification: {e}",
            path.display()
        )
    })?;
    Ok(receipt)
}
