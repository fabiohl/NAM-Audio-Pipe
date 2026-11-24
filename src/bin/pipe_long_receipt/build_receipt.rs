// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Build receipt creation and status query subcommands (`build-receipt`, `build-status`).

use std::env;
use std::fs;
use std::path::Path;
use std::process::exit;

use nam_audio_pipe::receipt::build::{
    BuildArtifacts, BuildOptimizations, BuildProvenance, BuildReceipt, PackageInfo,
    extract_engine_provenance, hash_file, parse_pgo_workload_receipt, resolve_engine_identity,
    validate_ab_opt_ceremony_receipt,
};
use neural_amp_modeler_rs::testing::receipt::now_iso8601;

use super::{
    DEFAULT_ARTIFACT, DEFAULT_BUILD_RECEIPT, die_usage, parse_flags, required, write_file,
};

pub(super) fn cmd_build_receipt(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("build-receipt", &e),
    };
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| DEFAULT_BUILD_RECEIPT.to_string());
    let artifact_path = flags
        .get("artifact")
        .cloned()
        .unwrap_or_else(|| DEFAULT_ARTIFACT.to_string());
    let package_name = flags
        .get("package-name")
        .cloned()
        .unwrap_or_else(|| "nam-audio-pipe".to_string());
    let package_version = flags
        .get("package-version")
        .cloned()
        .unwrap_or_else(|| "1.0.0".to_string());
    let git_commit = flags
        .get("git-commit")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let git_branch = flags
        .get("git-branch")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let git_dirty = flags
        .get("git-dirty")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let cargo_lock = flags
        .get("cargo-lock")
        .cloned()
        .unwrap_or_else(|| "Cargo.lock".to_string());
    let rustc_version = flags
        .get("rustc-version")
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let rustflags = flags.get("rustflags").cloned().unwrap_or_default();
    let pgo_status = flags
        .get("pgo-status")
        .cloned()
        .unwrap_or_else(|| "skipped".to_string());
    let bolt_status = flags
        .get("bolt-status")
        .cloned()
        .unwrap_or_else(|| "skipped".to_string());
    let bolt_version = flags.get("bolt-version").cloned();
    let bolt_profile = flags.get("bolt-profile").cloned();
    let ceremony_receipt = flags.get("ceremony-receipt").cloned();
    let pgo_workload_receipt = flags.get("pgo-workload-receipt").cloned();
    let codegen_status = flags
        .get("codegen-status")
        .cloned()
        .unwrap_or_else(|| "unavailable".to_string());
    let tarball_path = flags.get("tarball-path").cloned();
    let flatpak_path = flags.get("flatpak-path").cloned();
    let installed_path = flags.get("installed-path").cloned();
    let strict = flags.contains_key("strict");
    let run_id = flags
        .get("run-id")
        .cloned()
        .or_else(|| env::var("NAM_RUN_ID").ok());

    let mut gaps: Vec<String> = super::split_list(flags.get("gaps"));

    let artifact = Path::new(&artifact_path);
    if !artifact.exists() {
        eprintln!("build-receipt: artifact not found: {artifact_path}");
        exit(1);
    }
    let artifact_sha256 = match hash_file(artifact) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("build-receipt: failed to hash artifact {artifact_path}: {e}");
            exit(1);
        }
    };

    let cargo_lock_path = Path::new(&cargo_lock);
    let cargo_lock_sha256 = hash_file(cargo_lock_path).unwrap_or_else(|_| "absent".to_string());
    let project_root = cargo_lock_path.parent().unwrap_or(Path::new("."));
    let engine_provenance = extract_engine_provenance(cargo_lock_path)
        .with_identity(resolve_engine_identity(project_root));

    let hash_opt = |opt: &Option<String>| opt.as_ref().and_then(|p| hash_file(Path::new(p)).ok());
    let tarball_sha256 = hash_opt(&tarball_path);
    let flatpak_sha256 = hash_opt(&flatpak_path);
    let installed_sha256 = hash_opt(&installed_path);

    let bolt_proof_flag = flags.get("bolt-proof").cloned();
    let bolt_proof = if let Some(flag_proof) = bolt_proof_flag {
        flag_proof
    } else if let Some(path) = ceremony_receipt.as_deref() {
        match validate_ab_opt_ceremony_receipt(path, Some(&git_commit), Some(&cargo_lock_sha256)) {
            Ok((proof_id, _, _, verdict)) => {
                if verdict == "UNOPTIMIZED" {
                    "fallback_plain".to_string()
                } else if verdict == "PGO-ONLY" || bolt_status == "not_proven" {
                    "fallback_pgo_only".to_string()
                } else {
                    proof_id
                }
            }
            Err(e) => {
                if bolt_status == "applied" {
                    gaps.push("bolt_proof_unverified".to_string());
                }
                format!("unverified:{}", e.reason_code())
            }
        }
    } else if bolt_status == "not_proven" {
        "fallback_pgo_only".to_string()
    } else {
        if bolt_status == "applied" {
            gaps.push("bolt_proof_unverified".to_string());
        }
        "skipped".to_string()
    };

    let bolt_profile_sha256 = hash_opt(&bolt_profile);
    let pgo_workload = pgo_workload_receipt
        .as_deref()
        .and_then(parse_pgo_workload_receipt);

    let pgo_applied = pgo_status == "applied";
    let bolt_applied = bolt_status == "applied";
    if !pgo_applied {
        gaps.push("pgo_not_applied".to_string());
    }
    if bolt_status == "not_proven" {
        gaps.push("bolt_not_proven".to_string());
    } else if !bolt_applied {
        gaps.push("bolt_not_applied".to_string());
    }
    if codegen_status != "pass" {
        gaps.push(format!("codegen_gate_{codegen_status}"));
    }
    if !engine_provenance.is_pinned() {
        gaps.push("engine_unpinned".to_string());
    }
    gaps.sort();
    gaps.dedup();

    let (status, _fatal_strict_gaps) = compute_build_status(&gaps, &bolt_proof, strict);

    let receipt = BuildReceipt {
        schema_version: "1.0.0".to_string(),
        timestamp_utc: now_iso8601(),
        status: status.clone(),
        strict_mode: strict,
        gaps,
        package: PackageInfo {
            name: package_name,
            version: package_version,
        },
        provenance: BuildProvenance {
            git_commit,
            git_branch,
            git_dirty,
            cargo_lock_sha256,
            rustc_version,
            rustflags,
            run_id,
        },
        engine_provenance,
        optimizations: BuildOptimizations {
            pgo_applied,
            bolt_applied,
            bolt_proof,
            bolt_profile_sha256,
            bolt_version,
            codegen_status,
        },
        pgo_workload,
        artifacts: BuildArtifacts {
            artifact_path,
            artifact_sha256: artifact_sha256.clone(),
            tarball_path,
            tarball_sha256,
            flatpak_path,
            flatpak_sha256,
            installed_path,
            installed_sha256,
        },
    };

    let json = match serde_json::to_string_pretty(&receipt) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("build-receipt: failed to serialize json: {e}");
            exit(1);
        }
    };
    if let Err(e) = write_file(&out, &json) {
        eprintln!("build-receipt: failed to write {out}: {e}");
        exit(1);
    }
    if status == "FAILED" {
        eprintln!("BUILD_RECEIPT: FAIL log={out}");
        exit(1);
    }
    println!("BUILD_RECEIPT: {status} log={out} sha256={artifact_sha256}");
    exit(0);
}

pub(super) fn cmd_build_status(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("build-status", &e),
    };
    let receipt_path_str = match required(&flags, "receipt") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("build-status", &e),
    };
    let receipt_path = Path::new(&receipt_path_str);
    if !receipt_path.exists() {
        eprintln!("build-status: receipt file not found: {receipt_path_str}");
        exit(1);
    }
    let content = match fs::read_to_string(receipt_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("build-status: failed to read {receipt_path_str}: {e}");
            exit(1);
        }
    };
    let receipt: BuildReceipt = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("build-status: invalid build receipt schema: {e}");
            exit(1);
        }
    };

    let expected_run_id = flags
        .get("run-id")
        .cloned()
        .or_else(|| env::var("NAM_RUN_ID").ok())
        .filter(|s| !s.is_empty());

    if let Some(expected) = expected_run_id {
        match &receipt.provenance.run_id {
            Some(rid) if rid == &expected => {}
            Some(rid) => {
                eprintln!("build-status: run_id mismatch: receipt={rid} != expected={expected}");
                exit(1);
            }
            None => {
                eprintln!("build-status: receipt missing run_id, expected={expected}");
                exit(1);
            }
        }
    }

    println!("BUILD_STATUS: {}", receipt.status);
    if let Some(rid) = &receipt.provenance.run_id {
        println!("BUILD_RUN_ID: {rid}");
    }
    let (_, non_fallback_gaps) = compute_build_status(
        &receipt.gaps,
        &receipt.optimizations.bolt_proof,
        receipt.strict_mode,
    );
    for gap in &receipt.gaps {
        if non_fallback_gaps.contains(gap) {
            println!("GAP: {gap}");
        } else {
            println!("FALLBACK_GAP: {gap}");
        }
    }
    exit(0);
}

pub(super) fn compute_build_status(
    gaps: &[String],
    bolt_proof: &str,
    strict: bool,
) -> (String, Vec<String>) {
    let fatal_strict_gaps: Vec<String> = gaps
        .iter()
        .filter(|g| {
            if bolt_proof == "fallback_plain" {
                g.as_str() != "bolt_not_proven"
                    && g.as_str() != "pgo_not_applied"
                    && g.as_str() != "bolt_not_applied"
            } else if bolt_proof == "fallback_pgo_only" {
                g.as_str() != "bolt_not_proven" && g.as_str() != "bolt_not_applied"
            } else {
                true
            }
        })
        .cloned()
        .collect();

    let status = if !fatal_strict_gaps.is_empty() && strict {
        "FAILED".to_string()
    } else if !gaps.is_empty() {
        "PASS_WITH_GAPS".to_string()
    } else {
        "PASSED".to_string()
    };

    (status, fatal_strict_gaps)
}
