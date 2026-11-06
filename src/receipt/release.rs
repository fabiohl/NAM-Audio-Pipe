// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Release certification for the canonical Pipe artifact.
//!
//! `utils/build-release.sh` builds `target/dist/nam-audio-pipe` and emits
//! `target/dist/build-receipt.json`; `utils/tests-long.sh --artifact` audits
//! exactly those bytes and records the artifact SHA in every JSONL phase line.
//! `utils/build-release.sh --certify` reconciles all three and writes
//! `target/dist/release-receipt.json` with `status: CERTIFIED|REJECTED`.
//!
//! Certification is fail-closed: the artifact bytes must match the build
//! receipt, every long-audit line must declare the same SHA, both receipts must
//! be strict and gap-free, and the long-audit campaign must pass.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::receipt::LONG_ROSTER;
use crate::receipt::build::{
    BuildArtifacts, BuildOptimizations, BuildProvenance, BuildReceipt, EngineProvenance,
    PackageInfo, PgoWorkloadInfo, hash_file,
};
use crate::receipt::jsonl::verify_long_jsonl_strict;
use crate::receipt::long::verify_release_certification_file;
use neural_amp_modeler_rs::testing::receipt::now_iso8601;

const ARTIFACT_SHA256_KEY: &str = "artifact_sha256";
const ARTIFACT_PATH_KEY: &str = "artifact_path";
const ARTIFACT_EXERCISED_KEY: &str = "artifact_exercised";

/// Authoritative release certification receipt (`target/dist/release-receipt.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseReceipt {
    pub schema_version: String,
    pub timestamp_utc: String,
    pub status: String,
    pub strict_mode: bool,
    #[serde(default)]
    pub fails: Vec<String>,
    #[serde(default)]
    pub gaps: Vec<String>,
    #[serde(default)]
    pub reasons: Vec<String>,
    pub package: PackageInfo,
    pub provenance: BuildProvenance,
    pub engine_provenance: EngineProvenance,
    pub optimizations: BuildOptimizations,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgo_workload: Option<PgoWorkloadInfo>,
    pub artifacts: ReleaseArtifacts,
    pub long_audit: ReleaseLongAudit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifacts {
    pub artifact_path: String,
    pub artifact_sha256: String,
    pub artifact_consistency: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tarball_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tarball_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flatpak_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flatpak_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseLongAudit {
    pub receipt_path: String,
    pub status: String,
    pub tests_executed: u64,
    pub duration_ms: u64,
    #[serde(default)]
    pub phases: Vec<ReleaseLongPhaseSummary>,
    /// Mode evidence from the human-readable receipt: `STRICT: 1`,
    /// `NAM_RT_STRICT: 1` and `MODE: full` must be present for certification.
    #[serde(default)]
    pub strict_evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseLongPhaseSummary {
    pub phase_id: String,
    pub status: String,
    pub tests_executed: u64,
    #[serde(default)]
    pub gaps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_exercised: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactConsistency {
    Match { declared: String },
    Mismatch { reason: String },
    Absent { reason: String },
    Incomplete { reason: String },
}

impl ArtifactConsistency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Match { .. } => "match",
            Self::Mismatch { .. } => "mismatch",
            Self::Absent { .. } => "absent",
            Self::Incomplete { .. } => "incomplete",
        }
    }

    pub fn problem(&self) -> Option<String> {
        match self {
            Self::Match { .. } => None,
            Self::Mismatch { reason } | Self::Absent { reason } | Self::Incomplete { reason } => {
                Some(reason.clone())
            }
        }
    }

    /// SHA declared by the receipt, when the lines agree on one value.
    pub fn declared_sha256(&self) -> Option<&str> {
        match self {
            Self::Match { declared } => Some(declared.as_str()),
            _ => None,
        }
    }

    /// Exit code contract of `pipe_long_receipt verify-artifact`.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Match { .. } => 0,
            Self::Mismatch { .. } => 1,
            Self::Absent { .. } => 3,
            Self::Incomplete { .. } => 4,
        }
    }
}

/// Verify that every long-audit line declares the same artifact.
pub fn verify_receipt_artifact(receipt_path: &Path, artifact_path: &Path) -> ArtifactConsistency {
    let content = match fs::read_to_string(receipt_path) {
        Ok(c) => c,
        Err(e) => {
            return ArtifactConsistency::Absent {
                reason: format!("cannot read {}: {e}", receipt_path.display()),
            };
        }
    };
    let mut shas = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut declared_count = 0usize;
    let mut total = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        total += 1;
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return ArtifactConsistency::Mismatch {
                    reason: format!("invalid JSON line in {}: {e}", receipt_path.display()),
                };
            }
        };
        if let Some(sha) = value
            .get(ARTIFACT_SHA256_KEY)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            declared_count += 1;
            shas.insert(sha.to_string());
        }
        if let Some(path) = value
            .get(ARTIFACT_PATH_KEY)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            paths.insert(path.to_string());
        }
    }
    if total == 0 || declared_count == 0 {
        return ArtifactConsistency::Absent {
            reason: "long audit declares no artifact SHA".to_string(),
        };
    }
    if declared_count != total {
        return ArtifactConsistency::Incomplete {
            reason: format!("only {declared_count}/{total} receipt lines declare an artifact SHA"),
        };
    }
    if shas.len() != 1 || paths.len() > 1 {
        return ArtifactConsistency::Mismatch {
            reason: "long audit declares conflicting artifacts".to_string(),
        };
    }
    let declared = shas.into_iter().next().unwrap_or_default();
    match hash_file(artifact_path) {
        Ok(actual) if actual == declared => ArtifactConsistency::Match { declared },
        Ok(actual) => ArtifactConsistency::Mismatch {
            reason: format!("long audit declares {declared} but artifact is {actual}"),
        },
        Err(e) => ArtifactConsistency::Mismatch {
            reason: format!("cannot hash artifact {}: {e}", artifact_path.display()),
        },
    }
}

fn summarize_long_audit(
    content: &str,
    expected_run_id: Option<&str>,
    fails: &mut Vec<String>,
    gaps: &mut Vec<String>,
    reasons: &mut Vec<String>,
) -> ReleaseLongAudit {
    let mut phases = Vec::new();
    let mut status = "UNKNOWN".to_string();
    let mut tests_executed = 0u64;
    let mut duration_ms = 0u64;
    for (idx, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                fails.push(format!("long_receipt_line_{}_invalid", idx + 1));
                reasons.push(format!("long audit line {} invalid JSON: {e}", idx + 1));
                continue;
            }
        };
        let phase_id = value
            .get("phase_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let phase_status = value
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let phase_tests = value
            .get("tests_executed")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let phase_duration = value
            .get("duration_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let mut phase_gaps = Vec::new();
        if let Some(array) = value.get("gaps").and_then(|v| v.as_array()) {
            for gap in array {
                if let Some(gap) = gap.as_str().filter(|g| !g.is_empty()) {
                    phase_gaps.push(gap.to_string());
                    gaps.push(format!("{phase_id}:{gap}"));
                    reasons.push(format!(
                        "long audit phase '{phase_id}' declares gap: '{gap}'"
                    ));
                }
            }
        }
        if let Some(expected) = expected_run_id
            && phase_id != "overall"
        {
            match value.get("run_id").and_then(|v| v.as_str()) {
                Some(rid) if !rid.is_empty() && rid != expected => {
                    fails.push("run_id_mismatch_long".to_string());
                    reasons.push(format!(
                        "long audit line {} run_id ({rid}) != build receipt ({expected})",
                        idx + 1
                    ));
                }
                Some(rid) if !rid.is_empty() => {}
                _ => {
                    fails.push("run_id_missing_long".to_string());
                    reasons.push(format!(
                        "long audit line {} has no run_id to reconcile with the build receipt",
                        idx + 1
                    ));
                }
            }
        }
        if phase_id == "overall" {
            status = phase_status.clone();
            tests_executed = phase_tests;
            duration_ms = phase_duration;
        }
        if phase_status != "PASSED" {
            fails.push(format!("long_phase_{phase_id}_status_{phase_status}"));
            reasons.push(format!(
                "long audit phase '{phase_id}' status is '{phase_status}', expected 'PASSED'"
            ));
        }
        phases.push(ReleaseLongPhaseSummary {
            phase_id,
            status: phase_status,
            tests_executed: phase_tests,
            gaps: phase_gaps,
            artifact_sha256: value
                .get(ARTIFACT_SHA256_KEY)
                .and_then(|v| v.as_str())
                .map(ToString::to_string),
            artifact_exercised: value.get(ARTIFACT_EXERCISED_KEY).and_then(|v| v.as_bool()),
        });
    }
    ReleaseLongAudit {
        receipt_path: String::new(),
        status,
        tests_executed,
        duration_ms,
        phases,
        strict_evidence: "missing".to_string(),
    }
}

fn verify_declared_artifact(
    label: &str,
    path: &Option<String>,
    expected: &Option<String>,
    fails: &mut Vec<String>,
    reasons: &mut Vec<String>,
) {
    match (path, expected) {
        (None, None) => {}
        (Some(path), Some(expected)) => match hash_file(path) {
            Ok(actual) if &actual == expected => {}
            Ok(actual) => {
                fails.push(format!("{label}_sha_mismatch"));
                reasons.push(format!(
                    "{label} SHA-256 mismatch at {path}: disk ({actual}) != build receipt ({expected})"
                ));
            }
            Err(e) => {
                fails.push(format!("{label}_unreadable"));
                reasons.push(format!("cannot hash {label} at {path}: {e}"));
            }
        },
        (Some(path), None) => {
            fails.push(format!("{label}_sha_missing"));
            reasons.push(format!(
                "{label} at {path} has no SHA-256 in the build receipt"
            ));
        }
        (None, Some(_)) => {
            fails.push(format!("{label}_path_missing"));
            reasons.push(format!("{label} SHA-256 is present without a path"));
        }
    }
}

fn verify_text_strict_evidence(
    path: &Path,
    long_audit: &mut ReleaseLongAudit,
    fails: &mut Vec<String>,
    reasons: &mut Vec<String>,
) {
    if !path.exists() {
        fails.push("long_text_receipt_missing".to_string());
        reasons.push(format!(
            "strict long text receipt not found: {}",
            path.display()
        ));
        return;
    }
    match verify_release_certification_file(path) {
        Ok(_) => long_audit.strict_evidence = "text-receipt:verified".to_string(),
        Err(e) => {
            fails.push("long_text_receipt_not_strict".to_string());
            reasons.push(format!(
                "strict long text receipt {} failed verification: {e}",
                path.display()
            ));
        }
    }
}

/// Reconciles the canonical artifact, the build receipt and the long audit.
pub fn evaluate_certification(
    artifact_path: &Path,
    build_receipt_path: &Path,
    long_receipt_path: &Path,
    text_receipt_path: &Path,
    strict_requested: bool,
) -> ReleaseReceipt {
    let mut fails = Vec::new();
    let mut gaps = Vec::new();
    let mut reasons = Vec::new();

    let (actual_sha, art_exists) = if artifact_path.exists() {
        match hash_file(artifact_path) {
            Ok(h) => (h, true),
            Err(e) => {
                fails.push("artifact_hash_failed".to_string());
                reasons.push(format!("failed to compute artifact SHA-256: {e}"));
                ("unknown".to_string(), true)
            }
        }
    } else {
        fails.push("artifact_missing".to_string());
        reasons.push(format!("artifact not found: {}", artifact_path.display()));
        ("absent".to_string(), false)
    };

    let build_receipt: Option<BuildReceipt> = if build_receipt_path.exists() {
        match fs::read_to_string(build_receipt_path)
            .map_err(|e| e.to_string())
            .and_then(|c| serde_json::from_str::<BuildReceipt>(&c).map_err(|e| e.to_string()))
        {
            Ok(receipt) => {
                if art_exists && receipt.artifacts.artifact_sha256 != actual_sha {
                    fails.push("artifact_sha_mismatch_build".to_string());
                    reasons.push(format!(
                        "artifact SHA-256 mismatch: disk ({actual_sha}) != build receipt ({})",
                        receipt.artifacts.artifact_sha256
                    ));
                }
                if !receipt.strict_mode {
                    fails.push("build_not_strict".to_string());
                    reasons.push("build receipt strict_mode is false".to_string());
                }
                let is_proven_pgo_only = receipt.optimizations.pgo_applied
                    && !receipt.optimizations.bolt_applied
                    && receipt.optimizations.bolt_proof == "fallback_pgo_only";

                let valid_status = receipt.status == "PASSED"
                    || (is_proven_pgo_only && receipt.status == "PASS_WITH_GAPS");

                if !valid_status {
                    fails.push("build_status_not_passed".to_string());
                    reasons.push(format!(
                        "build receipt status is '{}', expected 'PASSED'",
                        receipt.status
                    ));
                }
                if receipt.provenance.git_dirty {
                    fails.push("build_worktree_dirty".to_string());
                    reasons.push(
                        "build receipt provenance records git_dirty=true; a certified release must come from a clean worktree"
                            .to_string(),
                    );
                }
                verify_declared_artifact(
                    "tarball",
                    &receipt.artifacts.tarball_path,
                    &receipt.artifacts.tarball_sha256,
                    &mut fails,
                    &mut reasons,
                );
                verify_declared_artifact(
                    "flatpak",
                    &receipt.artifacts.flatpak_path,
                    &receipt.artifacts.flatpak_sha256,
                    &mut fails,
                    &mut reasons,
                );
                verify_declared_artifact(
                    "installed",
                    &receipt.artifacts.installed_path,
                    &receipt.artifacts.installed_sha256,
                    &mut fails,
                    &mut reasons,
                );
                for gap in &receipt.gaps {
                    if gap == "bolt_not_proven" && is_proven_pgo_only {
                        continue;
                    }
                    gaps.push(format!("build:{gap}"));
                    reasons.push(format!("build receipt declares gap: '{gap}'"));
                }
                if receipt.optimizations.bolt_applied
                    && receipt.optimizations.bolt_proof == "fallback_pgo_only"
                {
                    fails.push("build_verdict_mismatch".to_string());
                    reasons.push(
                        "build receipt has bolt_applied=true but bolt_proof=fallback_pgo_only"
                            .to_string(),
                    );
                }
                if !receipt.optimizations.pgo_applied {
                    fails.push("unoptimized_verdict".to_string());
                    reasons.push(
                        "build receipt optimizations reflect unoptimized verdict (pgo_applied=false)"
                            .to_string(),
                    );
                }
                Some(receipt)
            }
            Err(e) => {
                fails.push("build_receipt_invalid_schema".to_string());
                reasons.push(format!("build receipt schema validation failed: {e}"));
                None
            }
        }
    } else {
        fails.push("build_receipt_missing".to_string());
        reasons.push(format!(
            "build receipt not found: {}",
            build_receipt_path.display()
        ));
        None
    };

    let mut consistency = ArtifactConsistency::Absent {
        reason: format!(
            "long audit receipt not found: {}",
            long_receipt_path.display()
        ),
    };
    let mut long_audit = ReleaseLongAudit {
        receipt_path: long_receipt_path.display().to_string(),
        status: "UNKNOWN".to_string(),
        tests_executed: 0,
        duration_ms: 0,
        phases: Vec::new(),
        strict_evidence: "missing".to_string(),
    };

    if long_receipt_path.exists() {
        consistency = verify_receipt_artifact(long_receipt_path, artifact_path);
        if let Some(problem) = consistency.problem() {
            fails.push(format!("long_artifact_{}", consistency.as_str()));
            reasons.push(format!(
                "long audit artifact provenance {}: {problem}",
                consistency.as_str()
            ));
        }
        if let Err(e) = verify_long_jsonl_strict(long_receipt_path) {
            fails.push("long_receipt_not_strict_passed".to_string());
            reasons.push(format!("long audit strict verification failed: {e}"));
        }
        match fs::read_to_string(long_receipt_path) {
            Ok(content) => {
                let expected_run_id = build_receipt
                    .as_ref()
                    .and_then(|receipt| receipt.provenance.run_id.as_deref());
                long_audit = summarize_long_audit(
                    &content,
                    expected_run_id,
                    &mut fails,
                    &mut gaps,
                    &mut reasons,
                );
                long_audit.receipt_path = long_receipt_path.display().to_string();
                let seen: BTreeSet<&str> = long_audit
                    .phases
                    .iter()
                    .map(|p| p.phase_id.as_str())
                    .collect();
                for required in LONG_ROSTER {
                    if !seen.contains(required) {
                        fails.push(format!("long_phase_{required}_missing"));
                        reasons.push(format!(
                            "canonical phase '{required}' is missing from long audit receipt"
                        ));
                    }
                }
            }
            Err(e) => {
                fails.push("long_receipt_read_failed".to_string());
                reasons.push(format!("failed to read long audit receipt: {e}"));
            }
        }
    } else {
        fails.push("long_receipt_missing".to_string());
        reasons.push(format!(
            "long audit receipt not found: {}",
            long_receipt_path.display()
        ));
    }

    verify_text_strict_evidence(text_receipt_path, &mut long_audit, &mut fails, &mut reasons);

    fails.sort();
    fails.dedup();
    gaps.sort();
    gaps.dedup();
    reasons.sort();
    reasons.dedup();

    let status = if fails.is_empty() && gaps.is_empty() {
        "CERTIFIED".to_string()
    } else {
        "REJECTED".to_string()
    };

    let (package, provenance, engine_provenance, optimizations, pgo_workload, artifacts) =
        if let Some(receipt) = build_receipt {
            let BuildArtifacts {
                artifact_path: built_path,
                artifact_sha256,
                tarball_path,
                tarball_sha256,
                flatpak_path,
                flatpak_sha256,
                installed_path,
                installed_sha256,
            } = receipt.artifacts;
            (
                receipt.package,
                receipt.provenance,
                receipt.engine_provenance,
                receipt.optimizations,
                receipt.pgo_workload,
                ReleaseArtifacts {
                    artifact_path: built_path,
                    artifact_sha256,
                    artifact_consistency: consistency.as_str().to_string(),
                    tarball_path,
                    tarball_sha256,
                    flatpak_path,
                    flatpak_sha256,
                    installed_path,
                    installed_sha256,
                },
            )
        } else {
            (
                PackageInfo {
                    name: "nam-audio-pipe".to_string(),
                    version: "unknown".to_string(),
                },
                BuildProvenance {
                    git_commit: "unknown".to_string(),
                    git_branch: "unknown".to_string(),
                    git_dirty: true,
                    cargo_lock_sha256: "unknown".to_string(),
                    rustc_version: "unknown".to_string(),
                    rustflags: String::new(),
                    run_id: None,
                },
                EngineProvenance {
                    dependency_type: "unknown".to_string(),
                    version: "unknown".to_string(),
                    checksum: None,
                },
                BuildOptimizations {
                    pgo_applied: false,
                    bolt_applied: false,
                    bolt_proof: "none".to_string(),
                    bolt_profile_sha256: None,
                    bolt_version: None,
                    codegen_status: "none".to_string(),
                },
                None,
                ReleaseArtifacts {
                    artifact_path: artifact_path.display().to_string(),
                    artifact_sha256: actual_sha,
                    artifact_consistency: consistency.as_str().to_string(),
                    tarball_path: None,
                    tarball_sha256: None,
                    flatpak_path: None,
                    flatpak_sha256: None,
                    installed_path: None,
                    installed_sha256: None,
                },
            )
        };

    let verdict = if optimizations.bolt_applied {
        Some("PGO+BOLT".to_string())
    } else if optimizations.pgo_applied && optimizations.bolt_proof == "fallback_pgo_only" {
        Some("PGO-ONLY".to_string())
    } else {
        None
    };

    ReleaseReceipt {
        schema_version: "1.0.0".to_string(),
        timestamp_utc: now_iso8601(),
        status,
        strict_mode: strict_requested,
        fails,
        gaps,
        reasons,
        package,
        provenance,
        engine_provenance,
        optimizations,
        pgo_workload,
        artifacts,
        long_audit,
        verdict,
    }
}

/// Re-checks a `CERTIFIED` release receipt against the bytes about to be installed.
/// Returns the verified artifact SHA-256, or every reconciliation problem found.
pub fn verify_release_receipt(
    release_path: &Path,
    artifact_path: &Path,
) -> Result<String, Vec<String>> {
    let receipt: ReleaseReceipt = fs::read_to_string(release_path)
        .map_err(|e| e.to_string())
        .and_then(|c| serde_json::from_str(&c).map_err(|e| e.to_string()))
        .map_err(|e| {
            vec![format!(
                "release receipt {} unreadable or invalid: {e}",
                release_path.display()
            )]
        })?;
    let actual = hash_file(artifact_path).map_err(|e| {
        vec![format!(
            "cannot hash artifact {}: {e}",
            artifact_path.display()
        )]
    })?;

    let mut problems = Vec::new();
    if receipt.status != "CERTIFIED" {
        problems.push(format!(
            "release receipt status is '{}', expected 'CERTIFIED'",
            receipt.status
        ));
    }
    if !receipt.fails.is_empty() || !receipt.gaps.is_empty() {
        problems.push("release receipt declares fails or gaps".to_string());
    }
    if receipt.artifacts.artifact_consistency != "match" {
        problems.push(format!(
            "release receipt artifact consistency is '{}', expected 'match'",
            receipt.artifacts.artifact_consistency
        ));
    }
    if actual != receipt.artifacts.artifact_sha256 {
        problems.push(format!(
            "artifact SHA-256 {actual} differs from release receipt {}",
            receipt.artifacts.artifact_sha256
        ));
    }
    match verify_receipt_artifact(Path::new(&receipt.long_audit.receipt_path), artifact_path) {
        ArtifactConsistency::Match { declared } if declared == actual => {}
        other => problems.push(format!(
            "long audit artifact provenance {}: {}",
            other.as_str(),
            other
                .problem()
                .unwrap_or_else(|| "declared SHA differs from artifact".to_string())
        )),
    }
    let run_reconciled = match (
        receipt.provenance.run_id.as_deref(),
        fs::read_to_string(&receipt.long_audit.receipt_path),
    ) {
        (Some(expected), Ok(content)) if !expected.is_empty() => content
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
            .filter(|v| v.get("phase_id").and_then(|p| p.as_str()) != Some("overall"))
            .all(|v| v.get("run_id").and_then(|r| r.as_str()) == Some(expected)),
        _ => false,
    };
    if !run_reconciled {
        problems.push("long audit run_id does not reconcile with the release receipt".to_string());
    }

    let mut fails = Vec::new();
    verify_declared_artifact(
        "tarball",
        &receipt.artifacts.tarball_path,
        &receipt.artifacts.tarball_sha256,
        &mut fails,
        &mut problems,
    );
    verify_declared_artifact(
        "flatpak",
        &receipt.artifacts.flatpak_path,
        &receipt.artifacts.flatpak_sha256,
        &mut fails,
        &mut problems,
    );

    if problems.is_empty() {
        Ok(actual)
    } else {
        Err(problems)
    }
}

/// Flatpak bundle path declared by a release receipt; `None` when no bundle was packaged.
pub fn declared_flatpak_path(release_path: &Path) -> Result<Option<String>, String> {
    let content = fs::read_to_string(release_path).map_err(|e| e.to_string())?;
    let receipt: ReleaseReceipt = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    Ok(receipt.artifacts.flatpak_path)
}

#[cfg(test)]
#[path = "release_test.rs"]
mod tests;
