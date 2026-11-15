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

#[test]
fn test_engine_patch_path_follows_declared_path_only() {
    let manifest = "[dependencies]\nfoo = \"1\"\n\n[patch.crates-io]\nNeuralAmpModeler-rs = { path = \"../engine-src\" }\n";
    assert_eq!(
        engine_patch_path(manifest).as_deref(),
        Some("../engine-src")
    );
    assert_eq!(
        engine_patch_path("[dependencies]\nNeuralAmpModeler-rs = \"2\"\n"),
        None
    );
}

#[test]
fn test_pinned_engine_commit_parses_variables_env() {
    let env =
        "ENGINE_TAG=\"unreleased\"\nENGINE_COMMIT=\"8fb2e52176d47c92257fcf90bb9f574b9ae3706d\"\n";
    assert_eq!(
        pinned_engine_commit(env).as_deref(),
        Some("8fb2e52176d47c92257fcf90bb9f574b9ae3706d")
    );
    assert_eq!(pinned_engine_commit("ENGINE_TAG=\"x\"\n"), None);
}

fn path_engine(
    resolved: Option<&str>,
    dirty: Option<bool>,
    pinned: Option<&str>,
) -> EngineProvenance {
    EngineProvenance {
        dependency_type: "workspace_or_path".to_string(),
        version: "2.1.0".to_string(),
        checksum: None,
        resolved_commit: resolved.map(str::to_string),
        resolved_dirty: dirty,
        pinned_commit: pinned.map(str::to_string),
    }
}

#[test]
fn test_engine_pin_requires_clean_matching_source() {
    assert!(path_engine(Some("abc"), Some(false), Some("abc")).is_pinned());
    assert!(!path_engine(Some("abc"), Some(true), Some("abc")).is_pinned());
    assert!(!path_engine(Some("abc"), Some(false), Some("def")).is_pinned());
    assert!(!path_engine(None, None, Some("abc")).is_pinned());
    assert!(!path_engine(Some("abc"), None, Some("abc")).is_pinned());
}

#[test]
fn test_crates_io_engine_is_pinned_by_lockfile() {
    let prov = EngineProvenance {
        dependency_type: "crates.io".to_string(),
        version: "2.1.0".to_string(),
        checksum: Some("deadbeef".to_string()),
        resolved_commit: None,
        resolved_dirty: None,
        pinned_commit: None,
    };
    assert!(prov.is_pinned());
}

#[test]
fn test_with_identity_ignores_identity_for_crates_io() {
    let prov = EngineProvenance {
        dependency_type: "crates.io".to_string(),
        version: "2.1.0".to_string(),
        checksum: None,
        resolved_commit: None,
        resolved_dirty: None,
        pinned_commit: None,
    }
    .with_identity(EngineSourceIdentity {
        resolved_commit: Some("abc".to_string()),
        resolved_dirty: Some(true),
        pinned_commit: Some("abc".to_string()),
    });
    assert_eq!(prov.resolved_commit, None);
    assert!(prov.is_pinned());
}

#[test]
fn test_resolve_engine_identity_detects_pin_and_dirty_tree() {
    let root = TempDir::new("engine_identity");
    let engine = root.path.join("engine");
    fs::create_dir_all(&engine).expect("mkdir engine");
    let git = |dir: &Path, args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .expect("git")
            .success();
        assert!(ok, "git {args:?} failed");
    };
    git(&engine, &["init", "-q"]);
    fs::write(engine.join("lib.rs"), "x").expect("write");
    git(&engine, &["add", "."]);
    git(&engine, &["commit", "-q", "-m", "init"]);
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&engine)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("rev-parse");
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();

    fs::write(
        root.path.join("Cargo.toml"),
        "[patch.crates-io]\nNeuralAmpModeler-rs = { path = \"engine\" }\n",
    )
    .expect("write manifest");
    fs::write(
        root.path.join("variables.env"),
        format!("ENGINE_COMMIT=\"{head}\"\n"),
    )
    .expect("write env");

    let clean = resolve_engine_identity(&root.path);
    assert_eq!(clean.resolved_commit.as_deref(), Some(head.as_str()));
    assert_eq!(clean.resolved_dirty, Some(false));
    assert_eq!(clean.pinned_commit.as_deref(), Some(head.as_str()));

    fs::write(engine.join("lib.rs"), "changed").expect("modify");
    let dirty = resolve_engine_identity(&root.path);
    assert_eq!(dirty.resolved_dirty, Some(true));

    fs::write(root.path.join("variables.env"), "ENGINE_COMMIT=\"0000\"\n").expect("write env");
    let unpinned = resolve_engine_identity(&root.path);
    assert_eq!(unpinned.pinned_commit.as_deref(), Some("0000"));
}
