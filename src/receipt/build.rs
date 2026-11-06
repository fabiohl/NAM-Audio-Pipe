// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Typed build-provenance receipt schema for the canonical Pipe artifact
//! (`target/dist/build-receipt.json`).
//!
//! `utils/build-release.sh` owns the build; the receipt is emitted by
//! `src/bin/pipe_long_receipt.rs build-receipt` so the shell never fabricates
//! JSON and the engine provenance is always read from `Cargo.lock` (never from
//! a sibling checkout).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BuildReceipt {
    pub schema_version: String,
    pub timestamp_utc: String,
    pub status: String,
    pub strict_mode: bool,
    #[serde(default)]
    pub gaps: Vec<String>,
    pub package: PackageInfo,
    pub provenance: BuildProvenance,
    pub engine_provenance: EngineProvenance,
    pub optimizations: BuildOptimizations,
    #[serde(default)]
    pub pgo_workload: Option<PgoWorkloadInfo>,
    pub artifacts: BuildArtifacts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildProvenance {
    pub git_commit: String,
    pub git_branch: String,
    pub git_dirty: bool,
    pub cargo_lock_sha256: String,
    pub rustc_version: String,
    pub rustflags: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EngineProvenance {
    pub dependency_type: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildOptimizations {
    pub pgo_applied: bool,
    pub bolt_applied: bool,
    pub bolt_proof: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bolt_profile_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bolt_version: Option<String>,
    pub codegen_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PgoWorkloadInfo {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excitation_gain: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_scenarios: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_callbacks: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BuildArtifacts {
    pub artifact_path: String,
    pub artifact_sha256: String,
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

fn digest_to_hex(digest: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;
    let bytes = digest.as_ref();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(&mut hex, "{b:02x}");
    }
    hex
}

/// Compute the hex SHA-256 digest of a file.
pub fn hash_file<P: AsRef<Path>>(path: P) -> Result<String, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Ok(digest_to_hex(hasher.finalize()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CeremonyProofRejection {
    MissingReceipt,
    InvalidJson(String),
    SchemaMismatch,
    ToolMismatch,
    CeremonyStaleCommit { expected: String, actual: String },
    CeremonyDirtyTree,
    CeremonyLockMismatch { expected: String, actual: String },
    VerdictNotOptimized(String),
    CeremonyBoltFailed(String),
}

impl CeremonyProofRejection {
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::MissingReceipt => "ceremony_missing_receipt",
            Self::InvalidJson(_) => "ceremony_invalid_json",
            Self::SchemaMismatch => "ceremony_schema_mismatch",
            Self::ToolMismatch => "ceremony_tool_mismatch",
            Self::CeremonyStaleCommit { .. } => "ceremony_stale_commit",
            Self::CeremonyDirtyTree => "ceremony_dirty_tree",
            Self::CeremonyLockMismatch { .. } => "ceremony_lock_mismatch",
            Self::VerdictNotOptimized(_) => "verdict_not_optimized",
            Self::CeremonyBoltFailed(_) => "ceremony_bolt_failed",
        }
    }
}

impl std::fmt::Display for CeremonyProofRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingReceipt => write!(f, "ceremony_missing_receipt"),
            Self::InvalidJson(msg) => write!(f, "ceremony_invalid_json: {msg}"),
            Self::SchemaMismatch => write!(f, "ceremony_schema_mismatch"),
            Self::ToolMismatch => write!(f, "ceremony_tool_mismatch"),
            Self::CeremonyStaleCommit { expected, actual } => {
                write!(
                    f,
                    "ceremony_stale_commit: expected {expected}, got {actual}"
                )
            }
            Self::CeremonyDirtyTree => write!(f, "ceremony_dirty_tree"),
            Self::CeremonyLockMismatch { expected, actual } => {
                write!(
                    f,
                    "ceremony_lock_mismatch: expected {expected}, got {actual}"
                )
            }
            Self::VerdictNotOptimized(v) => write!(f, "ceremony_verdict_not_optimized: {v}"),
            Self::CeremonyBoltFailed(cause) => write!(f, "ceremony_bolt_failed: {cause}"),
        }
    }
}

pub fn validate_ab_opt_ceremony_receipt<P: AsRef<Path>>(
    path: P,
    expected_git_sha: Option<&str>,
    expected_lock_sha: Option<&str>,
) -> Result<(String, Option<String>, Option<String>, String), CeremonyProofRejection> {
    let p = path.as_ref();
    if !p.exists() {
        return Err(CeremonyProofRejection::MissingReceipt);
    }
    let content =
        fs::read_to_string(p).map_err(|e| CeremonyProofRejection::InvalidJson(e.to_string()))?;
    let doc: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| CeremonyProofRejection::InvalidJson(e.to_string()))?;

    if doc.get("schema_version").and_then(|v| v.as_u64()) != Some(2) {
        return Err(CeremonyProofRejection::SchemaMismatch);
    }
    if doc.get("tool").and_then(|v| v.as_str()) != Some("ab-opt-ceremony") {
        return Err(CeremonyProofRejection::ToolMismatch);
    }

    if let Some(prov) = doc.get("provenance") {
        if prov.get("git_dirty").and_then(|v| v.as_bool()) == Some(true) {
            return Err(CeremonyProofRejection::CeremonyDirtyTree);
        }
        if let Some(expected_sha) = expected_git_sha {
            let actual = prov.get("git_sha").and_then(|v| v.as_str()).unwrap_or("");
            if actual != expected_sha {
                return Err(CeremonyProofRejection::CeremonyStaleCommit {
                    expected: expected_sha.to_string(),
                    actual: actual.to_string(),
                });
            }
        }
        if let Some(expected_lock) = expected_lock_sha {
            let actual = prov
                .get("cargo_lock_sha256")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if actual != expected_lock {
                return Err(CeremonyProofRejection::CeremonyLockMismatch {
                    expected: expected_lock.to_string(),
                    actual: actual.to_string(),
                });
            }
        }
    }

    let verdict = doc
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("UNOPTIMIZED");
    if verdict != "PGO+BOLT" && verdict != "PGO-ONLY" {
        return Err(CeremonyProofRejection::VerdictNotOptimized(
            verdict.to_string(),
        ));
    }
    if verdict == "PGO-ONLY" && doc.get("bolt_applied").and_then(|v| v.as_bool()) != Some(true) {
        let cause = doc
            .get("bolt_cause")
            .and_then(|v| v.as_str())
            .unwrap_or("BOLT not applied")
            .to_string();
        return Err(CeremonyProofRejection::CeremonyBoltFailed(cause));
    }

    let lbr = doc.get("bolt_lbr");
    let samples = lbr.and_then(|l| l.get("samples")).and_then(|s| {
        s.as_str()
            .map(|v| v.to_string())
            .or_else(|| s.as_u64().map(|u| u.to_string()))
    });
    let fdata_sha = lbr
        .and_then(|l| l.get("fdata_sha256"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());

    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let hex = digest_to_hex(hasher.finalize());
    let proof_id = format!("ab-opt-ceremony:{}", &hex[..16.min(hex.len())]);

    Ok((proof_id, samples, fdata_sha, verdict.to_string()))
}

/// Parse `ab-opt-ceremony-receipt.json` into `(proof_id, samples, fdata_sha)`.
pub fn parse_ab_opt_ceremony_receipt<P: AsRef<Path>>(
    path: P,
) -> Option<(String, Option<String>, Option<String>)> {
    validate_ab_opt_ceremony_receipt(path, None, None)
        .ok()
        .map(|(proof_id, samples, fdata_sha, _)| (proof_id, samples, fdata_sha))
}

/// Parse the PGO workload receipt summary.
pub fn parse_pgo_workload_receipt<P: AsRef<Path>>(path: P) -> Option<PgoWorkloadInfo> {
    let content = fs::read_to_string(path).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&content).ok()?;
    let no_stage_skipped = doc
        .get("no_stage_skipped")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let status = if no_stage_skipped { "PASSED" } else { "FAILED" }.to_string();
    let cells = doc.get("cells").and_then(|v| v.as_array());
    let total_scenarios = cells.map(|c| c.len() as u64);
    let total_callbacks = cells.map(|c| {
        c.iter()
            .filter_map(|cell| cell.get("blocks").and_then(|v| v.as_u64()))
            .sum()
    });
    Some(PgoWorkloadInfo {
        status,
        excitation_gain: doc.get("excitation_gain").and_then(|g| g.as_f64()),
        total_scenarios,
        total_callbacks,
    })
}

const MIN_TOTAL_BLOCKS: u64 = 1000;
const MIN_CELL_BLOCKS: u64 = 4;
const TOPOLOGIES: [&str; 3] = ["wavenet_a1", "wavenet_a2", "lstm"];
const DSP_GROUPS: [&str; 6] = [
    "resampler",
    "inference",
    "oversample",
    "cabsim",
    "bridge",
    "recording",
];
const MATRIX_DIMENSIONS: [(&str, &[&str]); 6] = [
    ("rates", &["44100", "48000", "96000"]),
    ("quantums", &["32", "64", "128", "256", "512", "1024"]),
    ("oversampling", &["Off", "2x", "4x"]),
    ("cabsim", &["ir", "bypass"]),
    ("recording", &["no", "yes"]),
    ("gate", &["on", "off"]),
];

fn get_u64(doc: &serde_json::Value, path: &[&str]) -> u64 {
    let mut node = doc;
    for key in path {
        node = match node.get(*key) {
            Some(v) => v,
            None => return 0,
        };
    }
    node.as_u64().unwrap_or(0)
}

/// Fail-closed validation of the PGO coverage matrix receipt.
///
/// Mirrors the worked contract of `pgo_workload`: every topology must advance
/// at least `MIN_TOTAL_BLOCKS` and `MIN_CELL_BLOCKS` per matrix cell, every
/// DSP group must advance frames, every matrix dimension value must be
/// exercised and no mandatory stage may be skipped.
pub fn validate_pgo_matrix<P: AsRef<Path>>(path: P) -> Result<String, String> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .map_err(|e| format!("cannot read PGO receipt {}: {e}", path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("cannot parse PGO receipt {}: {e}", path.display()))?;
    if doc.get("schema_version").and_then(|v| v.as_u64()) != Some(2) {
        return Err("PGO receipt schema_version must be 2 (matrix)".to_string());
    }
    for topology in TOPOLOGIES {
        let total = get_u64(&doc, &["topology_blocks", topology]);
        if total < MIN_TOTAL_BLOCKS {
            return Err(format!(
                "topology '{topology}' total blocks = {total} (required >= {MIN_TOTAL_BLOCKS})"
            ));
        }
        let min_cell = get_u64(&doc, &["progress", "min_blocks_per_topology", topology]);
        if min_cell < MIN_CELL_BLOCKS {
            return Err(format!(
                "topology '{topology}' min cell blocks = {min_cell} (required >= {MIN_CELL_BLOCKS})"
            ));
        }
        let frames = get_u64(&doc, &["progress", "min_frames_per_topology", topology]);
        if frames == 0 {
            return Err(format!("topology '{topology}' advanced 0 frames"));
        }
        for group in DSP_GROUPS {
            let group_frames = get_u64(
                &doc,
                &[
                    "progress",
                    "groups",
                    group,
                    "min_frames_per_topology",
                    topology,
                ],
            );
            if group_frames == 0 {
                return Err(format!(
                    "DSP group '{group}' advanced 0 frames for topology '{topology}'"
                ));
            }
        }
    }
    for (dimension, required) in MATRIX_DIMENSIONS {
        for value in required {
            if get_u64(&doc, &["coverage", dimension, value]) == 0 {
                return Err(format!(
                    "matrix dimension '{dimension}' value '{value}' was not exercised"
                ));
            }
        }
    }
    if doc.get("no_stage_skipped").and_then(|v| v.as_bool()) != Some(true) {
        return Err("no_stage_skipped is not true; a mandatory DSP stage was skipped".to_string());
    }
    Ok(format!(
        "PGO matrix valid: topologies={} cells_min={MIN_CELL_BLOCKS} blocks_min={MIN_TOTAL_BLOCKS}",
        TOPOLOGIES.join(",")
    ))
}

/// Extract the `NeuralAmpModeler-rs` provenance from `Cargo.lock`.
///
/// The lockfile is the single source of truth: a crates.io dependency declares
/// `source = "registry+..."` plus a checksum; a path dependency has neither.
pub fn extract_engine_provenance(cargo_lock_path: &Path) -> EngineProvenance {
    let Ok(content) = fs::read_to_string(cargo_lock_path) else {
        return EngineProvenance {
            dependency_type: "missing_lockfile".to_string(),
            version: "unknown".to_string(),
            checksum: None,
        };
    };
    let mut in_pkg = false;
    let mut version = "unknown".to_string();
    let mut checksum = None;
    let mut dep_type = "workspace_or_path".to_string();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            in_pkg = false;
        } else if trimmed == "name = \"NeuralAmpModeler-rs\"" {
            in_pkg = true;
        } else if in_pkg {
            if let Some(v) = trimmed.strip_prefix("version = \"") {
                version = v.trim_end_matches('"').to_string();
            } else if let Some(s) = trimmed.strip_prefix("source = \"") {
                let src = s.trim_end_matches('"');
                if src.starts_with("registry+") {
                    dep_type = "crates.io".to_string();
                } else if src.starts_with("git+") {
                    dep_type = "git".to_string();
                } else {
                    dep_type = src.to_string();
                }
            } else if let Some(c) = trimmed.strip_prefix("checksum = \"") {
                checksum = Some(c.trim_end_matches('"').to_string());
            } else if trimmed.starts_with("[[") {
                break;
            }
        }
    }
    EngineProvenance {
        dependency_type: dep_type,
        version,
        checksum,
    }
}

#[cfg(test)]
#[path = "build_test.rs"]
mod tests;
