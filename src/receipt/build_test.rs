// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "nam_pipe_test_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::create_dir_all(&path);
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn make_temp_receipt(tag: &str, prov: Value, verdict: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new(tag);
    let path = dir.path.join("ab-opt-ceremony-receipt.json");
    let doc = json!({
        "schema_version": 2,
        "tool": "ab-opt-ceremony",
        "verdict": verdict,
        "verdict_reason": "ok",
        "bolt_applied": true,
        "provenance": prov,
        "bolt_lbr": {
            "samples": 1000,
            "fdata_sha256": "abcdef",
            "mode": "lbr"
        }
    });
    let mut f = fs::File::create(&path).expect("create file");
    f.write_all(doc.to_string().as_bytes()).expect("write");
    (dir, path)
}

#[test]
fn test_ceremony_missing_receipt() {
    let res = validate_ab_opt_ceremony_receipt(
        Path::new("/non/existent/path/ab-opt-ceremony-receipt.json"),
        Some("deadbeef"),
        Some("1234"),
    );
    assert_eq!(res, Err(CeremonyProofRejection::MissingReceipt));
}

#[test]
fn test_ceremony_stale_commit_rejected() {
    let prov = json!({
        "git_sha": "stale_commit_sha",
        "git_dirty": false,
        "cargo_lock_sha256": "lock_1234",
    });
    let (_dir, path) = make_temp_receipt("stale", prov, "PGO-ONLY");
    let res = validate_ab_opt_ceremony_receipt(&path, Some("head_commit_sha"), Some("lock_1234"));
    assert_eq!(
        res,
        Err(CeremonyProofRejection::CeremonyStaleCommit {
            expected: "head_commit_sha".to_string(),
            actual: "stale_commit_sha".to_string(),
        })
    );
}

#[test]
fn test_ceremony_dirty_tree_rejected() {
    let prov = json!({
        "git_sha": "head_commit_sha",
        "git_dirty": true,
        "cargo_lock_sha256": "lock_1234",
    });
    let (_dir, path) = make_temp_receipt("dirty", prov, "PGO-ONLY");
    let res = validate_ab_opt_ceremony_receipt(&path, Some("head_commit_sha"), Some("lock_1234"));
    assert_eq!(res, Err(CeremonyProofRejection::CeremonyDirtyTree));
}

#[test]
fn test_ceremony_lock_mismatch_rejected() {
    let prov = json!({
        "git_sha": "head_commit_sha",
        "git_dirty": false,
        "cargo_lock_sha256": "lock_old",
    });
    let (_dir, path) = make_temp_receipt("lock", prov, "PGO-ONLY");
    let res = validate_ab_opt_ceremony_receipt(&path, Some("head_commit_sha"), Some("lock_new"));
    assert_eq!(
        res,
        Err(CeremonyProofRejection::CeremonyLockMismatch {
            expected: "lock_new".to_string(),
            actual: "lock_old".to_string(),
        })
    );
}

#[test]
fn test_ceremony_clean_head_accepted() {
    let prov = json!({
        "git_sha": "head_commit_sha",
        "git_dirty": false,
        "cargo_lock_sha256": "lock_1234",
    });
    let (_dir, path) = make_temp_receipt("clean", prov, "PGO-ONLY");
    let res = validate_ab_opt_ceremony_receipt(&path, Some("head_commit_sha"), Some("lock_1234"));
    assert!(res.is_ok());
    let (proof_id, samples, fdata_sha, verdict) = res.unwrap();
    assert!(proof_id.starts_with("ab-opt-ceremony:"));
    assert_eq!(samples.as_deref(), Some("1000"));
    assert_eq!(fdata_sha.as_deref(), Some("abcdef"));
    assert_eq!(verdict, "PGO-ONLY");
}

#[test]
fn test_ceremony_unoptimized_verdict_accepted() {
    let prov = json!({
        "git_sha": "head_commit_sha",
        "git_dirty": false,
        "cargo_lock_sha256": "lock_1234",
    });
    let (_dir, path) = make_temp_receipt("unopt", prov, "UNOPTIMIZED");
    let res = validate_ab_opt_ceremony_receipt(&path, Some("head_commit_sha"), Some("lock_1234"));
    assert!(res.is_ok());
    let (proof_id, samples, fdata_sha, verdict) = res.unwrap();
    assert!(proof_id.starts_with("ab-opt-ceremony:"));
    assert_eq!(samples.as_deref(), Some("1000"));
    assert_eq!(fdata_sha.as_deref(), Some("abcdef"));
    assert_eq!(verdict, "UNOPTIMIZED");
}

#[test]
fn test_ceremony_pgo_only_bolt_skipped_accepted() {
    let prov = json!({
        "git_sha": "head_commit_sha",
        "git_dirty": false,
        "cargo_lock_sha256": "lock_1234",
    });
    let dir = TempDir::new("pgo_skipped");
    let path = dir.path.join("ab-opt-ceremony-receipt.json");
    let doc = json!({
        "schema_version": 2,
        "tool": "ab-opt-ceremony",
        "verdict": "PGO-ONLY",
        "verdict_reason": "bolt_not_applied",
        "bolt_applied": false,
        "bolt_cause": "BOLT_SKIPPED: hardware LBR unavailable (-j any,u unsupported on AMD/VM)",
        "provenance": prov,
    });
    let mut f = fs::File::create(&path).expect("create file");
    f.write_all(doc.to_string().as_bytes()).expect("write");

    let res = validate_ab_opt_ceremony_receipt(&path, Some("head_commit_sha"), Some("lock_1234"));
    assert!(res.is_ok());
    let (proof_id, _samples, _fdata_sha, verdict) = res.unwrap();
    assert!(proof_id.starts_with("ab-opt-ceremony:"));
    assert_eq!(verdict, "PGO-ONLY");
}
