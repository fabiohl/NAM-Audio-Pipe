// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "nam_pipe_rel_test_{tag}_{}_{}",
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

fn create_mock_release_env(
    tag: &str,
    pgo_applied: bool,
    bolt_applied: bool,
    bolt_proof: &str,
    build_status: &str,
    git_dirty: bool,
    gaps: Vec<String>,
) -> (TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = TempDir::new(tag);

    // 1. Artifact
    let artifact_path = dir.path.join("nam-audio-pipe");
    let artifact_bytes = b"MOCK_ARTIFACT_ELF_BINARY";
    fs::write(&artifact_path, artifact_bytes).expect("write artifact");

    let mut hasher = Sha256::new();
    hasher.update(artifact_bytes);
    let artifact_sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    // 2. Build receipt
    let build_receipt_path = dir.path.join("build-receipt.json");
    let build_receipt = json!({
        "schema_version": "1.0.0",
        "timestamp_utc": "2026-11-06T00:00:00Z",
        "status": build_status,
        "strict_mode": true,
        "gaps": gaps,
        "package": {
            "name": "nam-audio-pipe",
            "version": "1.1.0"
        },
        "provenance": {
            "git_commit": "0123456789abcdef0123456789abcdef01234567",
            "git_branch": "dev",
            "git_dirty": git_dirty,
            "cargo_lock_sha256": "4e724b172a15f019688461bfa2a74c43f11e9f2df486be77fb238385d03e5c77",
            "rustc_version": "rustc 1.99.0",
            "rustflags": "-C target-cpu=x86-64-v3",
            "run_id": "test-run-123"
        },
        "engine_provenance": {
            "dependency_type": "workspace_or_path",
            "version": "2.0.0",
            "checksum": null
        },
        "optimizations": {
            "pgo_applied": pgo_applied,
            "bolt_applied": bolt_applied,
            "bolt_proof": bolt_proof,
            "bolt_profile_sha256": if bolt_applied { Some("bolt_profile_sha".to_string()) } else { None },
            "bolt_version": if bolt_applied { Some("llvm-bolt 19".to_string()) } else { None },
            "codegen_status": "pass"
        },
        "artifacts": {
            "artifact_path": artifact_path.display().to_string(),
            "artifact_sha256": artifact_sha256,
            "tarball_path": null,
            "tarball_sha256": null,
            "flatpak_path": null,
            "flatpak_sha256": null,
            "installed_path": null,
            "installed_sha256": null
        }
    });
    fs::write(
        &build_receipt_path,
        serde_json::to_string_pretty(&build_receipt).unwrap(),
    )
    .expect("write build receipt");

    // 3. Long JSONL receipt
    let long_receipt_path = dir.path.join("long-audit-receipt.jsonl");
    let mut jsonl_file = fs::File::create(&long_receipt_path).expect("create jsonl");
    for phase in LONG_ROSTER {
        let entry = json!({
            "phase_id": phase,
            "name": format!("phase_{phase}"),
            "status": "PASSED",
            "artifact_sha256": artifact_sha256,
            "run_id": "test-run-123",
            "strict_mode": true,
            "tests_executed": 10,
            "duration_ms": 100,
            "gaps": [],
            "timestamp": "2026-11-06T00:00:00Z"
        });
        writeln!(jsonl_file, "{entry}").expect("write jsonl line");
    }

    // 4. Long text receipt
    let text_receipt_path = dir.path.join("tests-long-strict.txt");
    let text_content = "\
SUITE: tests-long
STRICT: 1
NAM_RT_STRICT: 1
MODE: full
SOAK_PURPOSE: accelerated_timeline - automated soak verification
ENDURANCE_PURPOSE: real_wall_clock - automated endurance verification
PHASE1: PASS log=target/logs/1.log
PHASE2: PASS log=target/logs/2.log
PHASE3: PASS log=target/logs/3.log env=pinned:1,isolated:1,fifo:1
PHASE4: PASS log=target/logs/4.log env=pinned:1,isolated:1,fifo:1
PHASE5: PASS log=target/logs/5.log
PHASE6: PASS log=target/logs/6.log
OVERALL: PASSED
";
    fs::write(&text_receipt_path, text_content).expect("write text receipt");

    (
        dir,
        artifact_path,
        build_receipt_path,
        long_receipt_path,
        text_receipt_path,
    )
}

#[test]
fn test_certify_pgo_bolt_clean_head_certified() {
    let (_dir, art, bld, lng, txt) = create_mock_release_env(
        "pgo_bolt",
        true,
        true,
        "ab-opt-ceremony:proof-abc-123",
        "PASSED",
        false,
        vec![],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, &txt, true);
    assert_eq!(receipt.status, "CERTIFIED");
    assert_eq!(receipt.verdict, Some("PGO+BOLT".to_string()));
    assert!(receipt.fails.is_empty(), "fails: {:?}", receipt.fails);
}

#[test]
fn test_certify_pgo_only_fallback_certified() {
    let (_dir, art, bld, lng, txt) = create_mock_release_env(
        "pgo_only",
        true,
        false,
        "fallback_pgo_only",
        "PASS_WITH_GAPS",
        false,
        vec!["bolt_not_proven".to_string()],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, &txt, true);
    assert_eq!(receipt.status, "CERTIFIED");
    assert_eq!(receipt.verdict, Some("PGO-ONLY".to_string()));
    assert!(receipt.fails.is_empty(), "fails: {:?}", receipt.fails);
}

#[test]
fn test_certify_unoptimized_rejected() {
    let (_dir, art, bld, lng, txt) =
        create_mock_release_env("unopt", false, false, "none", "PASSED", false, vec![]);

    let receipt = evaluate_certification(&art, &bld, &lng, &txt, true);
    assert_eq!(receipt.status, "REJECTED");
    assert!(
        receipt.fails.contains(&"unoptimized_verdict".to_string()),
        "fails: {:?}",
        receipt.fails
    );
}

#[test]
fn test_certify_dirty_worktree_rejected() {
    let (_dir, art, bld, lng, txt) = create_mock_release_env(
        "dirty",
        true,
        true,
        "ab-opt-ceremony:proof-abc-123",
        "PASSED",
        true, // dirty worktree
        vec![],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, &txt, true);
    assert_eq!(receipt.status, "REJECTED");
    assert!(
        receipt.fails.contains(&"build_worktree_dirty".to_string()),
        "fails: {:?}",
        receipt.fails
    );
}

#[test]
fn test_certify_verdict_mismatch_rejected() {
    let (_dir, art, bld, lng, txt) = create_mock_release_env(
        "mismatch",
        true,
        true,
        "fallback_pgo_only", // mismatch: bolt_applied is true but proof is fallback_pgo_only
        "PASSED",
        false,
        vec![],
    );

    let receipt = evaluate_certification(&art, &bld, &lng, &txt, true);
    assert_eq!(receipt.status, "REJECTED");
    assert!(
        receipt
            .fails
            .contains(&"build_verdict_mismatch".to_string()),
        "fails: {:?}",
        receipt.fails
    );
}
