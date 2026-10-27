// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Receipt tooling for the NAM-Audio-Pipe QA and release chain.
//!
//! `utils/tests-long.sh` appends one machine-readable line per completed phase
//! to `target/logs/long-audit-receipt.jsonl` using the engine receipt schema;
//! the human-readable `target/logs/long-receipt.txt` remains the derived view.
//! The suite-level `overall` line is derived in Rust by `summary`.
//!
//! `build-receipt` emits `target/dist/build-receipt.json` (artifact SHA, engine
//! provenance from `Cargo.lock`, PGO/BOLT evidence); `certify` reconciles that
//! receipt with the strict long audit and writes `release-receipt.json`.
//!
//! Subcommands: `append`, `summary`, `validate [--strict]`, `pgo-check`,
//! `bolt-proof`, `build-receipt`, `certify`.
//! Exit codes: 0 success; 1 I/O or verification error; 2 usage error.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::str::FromStr;

use nam_audio_pipe::receipt::LONG_ROSTER;
use nam_audio_pipe::receipt::build::{
    BuildArtifacts, BuildOptimizations, BuildProvenance, BuildReceipt, PackageInfo,
    extract_engine_provenance, hash_file, parse_ab_opt_ceremony_receipt,
    parse_pgo_workload_receipt, validate_pgo_matrix,
};
use nam_audio_pipe::receipt::fail_reasons::{self, FailReasonRecord};
use nam_audio_pipe::receipt::jsonl::{verify_long_jsonl, verify_long_jsonl_strict};
use nam_audio_pipe::receipt::release::{evaluate_certification, verify_receipt_artifact};
use neural_amp_modeler_rs::testing::receipt::{
    LongAuditReceipt, LongPhaseReceipt, LongPhaseStatus, count_tests_executed_from_log,
    detect_gap_markers, now_iso8601,
};

const DEFAULT_OUT: &str = "target/logs/long-audit-receipt.jsonl";

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        exit(2);
    }
    match args[0].as_str() {
        "append" => cmd_append(&args[1..]),
        "summary" => cmd_summary(&args[1..]),
        "validate" => cmd_validate(&args[1..]),
        "pgo-check" => cmd_pgo_check(&args[1..]),
        "bolt-proof" => cmd_bolt_proof(&args[1..]),
        "build-receipt" => cmd_build_receipt(&args[1..]),
        "certify" => cmd_certify(&args[1..]),
        "verify-artifact" => cmd_verify_artifact(&args[1..]),
        "-h" | "--help" => {
            print_help();
            exit(0);
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            print_help();
            exit(2);
        }
    }
}

fn print_help() {
    println!("NAM-Audio-Pipe Long-Audit Receipt (unified engine schema)");
    println!("Usage:");
    println!("  pipe_long_receipt append --phase-id <id> --name <name> --status <STATUS>");
    println!("                      --duration-ms <ms> [--tests-executed <n>] [--log <path>]");
    println!("                      [--gaps <a,b,...>] [--out <file>] [--fail-reason <text>]");
    println!("                      [--oracle-sha256 <hex>] [--fixture-sha256 <hex>]");
    println!("                      [--artifact-sha256 <hex>]");
    println!("  pipe_long_receipt summary [--out <file>]");
    println!("  pipe_long_receipt validate [--out <file>] [--strict]");
    println!("  pipe_long_receipt pgo-check --receipt <file>");
    println!("  pipe_long_receipt bolt-proof --receipt <file>");
    println!(
        "  pipe_long_receipt build-receipt --artifact <path> [--out <file>] [--strict] [options...]"
    );
    println!(
        "  pipe_long_receipt certify [--artifact <path>] [--build-receipt <path>] [--long-receipt <path>] [--text-receipt <path>] [--out <file>] [--strict]"
    );
    println!("  pipe_long_receipt verify-artifact --out <file> --artifact <path>");
    println!();
    println!(
        "Status values: PASSED, FAILED, SKIPPED, INCONCLUSIVE, SKIP_CAPABILITY, NOT_RUN, SIMULATED"
    );
    println!("Canonical roster: {}", LONG_ROSTER.join(", "));
    println!("Default output file: {DEFAULT_OUT}");
}

const VALUE_LESS_FLAGS: &[&str] = &["strict"];
const DEFAULT_ARTIFACT: &str = "target/dist/nam-audio-pipe";
const DEFAULT_BUILD_RECEIPT: &str = "target/dist/build-receipt.json";
const DEFAULT_RELEASE_RECEIPT: &str = "target/dist/release-receipt.json";
const DEFAULT_TEXT_RECEIPT: &str = "target/logs/long-receipt.txt";

fn parse_flags(args: &[String]) -> Result<std::collections::HashMap<String, String>, String> {
    let mut flags = std::collections::HashMap::new();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let name = flag.strip_prefix("--").unwrap_or(flag.as_str()).to_string();
        if name.is_empty() {
            return Err(format!("{flag}: expected a flag name"));
        }
        if VALUE_LESS_FLAGS.contains(&name.as_str()) {
            flags.insert(name, String::new());
            continue;
        }
        let value = iter
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        flags.insert(name, value.clone());
    }
    Ok(flags)
}

fn required<'a>(
    flags: &'a std::collections::HashMap<String, String>,
    name: &str,
) -> Result<&'a String, String> {
    flags
        .get(name)
        .ok_or_else(|| format!("missing required flag --{name}"))
}

fn cmd_append(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("append", &e),
    };
    let phase_id = match required(&flags, "phase-id") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("append", &e),
    };
    if !LONG_ROSTER.contains(&phase_id.as_str()) {
        die_usage(
            "append",
            &format!(
                "unknown phase id '{phase_id}' (canonical: {})",
                LONG_ROSTER.join(", ")
            ),
        );
    }
    let name = match required(&flags, "name") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("append", &e),
    };
    let status = match required(&flags, "status").and_then(|s| LongPhaseStatus::from_str(s)) {
        Ok(s) => s,
        Err(e) => die_usage("append", &e),
    };
    let duration_ms: u64 = match required(&flags, "duration-ms") {
        Ok(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => die_usage("append", "--duration-ms must be a non-negative integer"),
        },
        Err(e) => die_usage("append", &e),
    };
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| DEFAULT_OUT.to_string());

    let log_path = flags.get("log").map(PathBuf::from);
    let tests_executed = match flags.get("tests-executed") {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => die_usage("append", "--tests-executed must be a non-negative integer"),
        },
        None => log_path
            .as_deref()
            .map(count_tests_executed_from_log)
            .unwrap_or(0),
    };

    let mut gaps: Vec<String> = Vec::new();
    if let Some(list) = flags.get("gaps") {
        for g in list.split(',') {
            let g = g.trim();
            if !g.is_empty() {
                gaps.push(g.to_string());
            }
        }
    }
    if let Some(id) = status.gap_id() {
        gaps.push(id.to_string());
    }
    if let Some(path) = log_path.as_deref() {
        for marker in detect_gap_markers(path) {
            if !gaps.contains(&marker) {
                gaps.push(marker);
            }
        }
    }

    let receipt = LongPhaseReceipt {
        phase_id,
        name,
        status,
        duration_ms,
        tests_executed,
        gaps,
        timestamp: now_iso8601(),
        oracle_sha256: flags.get("oracle-sha256").cloned(),
        fixture_sha256: flags.get("fixture-sha256").cloned(),
        artifact_sha256: flags.get("artifact-sha256").cloned(),
    };
    if let Err(e) = append_line(&out, &receipt.render_jsonl_line()) {
        eprintln!("append: failed to write receipt to {out}: {e}");
        exit(1);
    }
    // Typed FAIL cause: the engine-owned unified schema has no `fail_reason`
    // field, so it is written to a sibling JSONL (see receipt::fail_reasons).
    if let Some(reason) = flags.get("fail-reason") {
        let record = FailReasonRecord {
            phase_id: receipt.phase_id.clone(),
            fail_reason: reason.clone(),
            timestamp: receipt.timestamp.clone(),
        };
        if let Err(e) = fail_reasons::append(&out, &record) {
            eprintln!(
                "append: failed to write fail reason to {}: {e}",
                fail_reasons::path(&out).display()
            );
            exit(1);
        }
    }
    eprintln!(
        "✓ pipe long-audit receipt: {} {} ({} tests, {} ms)",
        receipt.phase_id, receipt.status, receipt.tests_executed, receipt.duration_ms
    );
    exit(0);
}

fn cmd_summary(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("summary", &e),
    };
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| DEFAULT_OUT.to_string());
    let mut audit = match LongAuditReceipt::parse_jsonl_file(Path::new(&out)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("summary: cannot build summary from {out}: {e}");
            exit(1);
        }
    };
    if audit.phases.is_empty() {
        eprintln!("summary: no phase entries in {out} — nothing to summarize");
        exit(1);
    }
    audit.push_summary();
    // Propagate the artifact declared by every phase into the derived overall
    // line so release certification can bind the whole campaign to one SHA.
    let mut declared: Option<String> = None;
    let mut complete = true;
    for phase in &audit.phases {
        if phase.phase_id == "overall" {
            continue;
        }
        match (&declared, &phase.artifact_sha256) {
            (None, Some(sha)) => declared = Some(sha.clone()),
            (Some(expected), Some(sha)) if expected != sha => exit(1),
            (Some(_), None) => {
                complete = false;
            }
            _ => {}
        }
    }
    if complete
        && let Some(sha) = declared
        && let Some(overall) = audit.phases.last_mut()
    {
        overall.artifact_sha256 = Some(sha);
    }
    let overall = audit
        .phases
        .last()
        .expect("push_summary always appends the overall line");
    if let Err(e) = write_file(&out, &audit.render_jsonl()) {
        eprintln!("summary: failed to write overall line to {out}: {e}");
        exit(1);
    }
    eprintln!(
        "✓ pipe long-audit receipt: overall {} ({} tests, {} ms, {} gaps)",
        overall.status,
        overall.tests_executed,
        overall.duration_ms,
        overall.gaps.len()
    );
    println!("OVERALL: {}", overall.status.as_str());
    exit(0);
}

fn cmd_validate(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("validate", &e),
    };
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| DEFAULT_OUT.to_string());
    let strict = flags.contains_key("strict");
    let path = Path::new(&out);
    if !path.exists() {
        eprintln!("validate: receipt file not found: {out}");
        exit(1);
    }
    let result = if strict {
        verify_long_jsonl_strict(path)
    } else {
        verify_long_jsonl(path)
    };
    match result {
        Ok(audit) => {
            // The typed FAIL causes live in a sibling file; absence is valid
            // (no FAIL, or a legacy receipt), a malformed sibling fails closed.
            let fail_reasons = match fail_reasons::load(&out) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("validate: {e}");
                    exit(1);
                }
            };
            let label = if strict { "STRICT: PASSED" } else { "VALID" };
            println!(
                "{label}: {} receipt line(s) in {out} ({} tests, {} ms, {} fail_reason(s)) | OVERALL: {}",
                audit.phases.len(),
                audit.tests_executed_total(),
                audit.duration_ms_total(),
                fail_reasons.len(),
                audit.summary_receipt().status.as_str()
            );
            exit(0);
        }
        Err(e) => {
            eprintln!("validate: {e}");
            exit(1);
        }
    }
}

fn cmd_pgo_check(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("pgo-check", &e),
    };
    let receipt = match required(&flags, "receipt") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("pgo-check", &e),
    };
    match validate_pgo_matrix(&receipt) {
        Ok(summary) => {
            println!("PGO_RECEIPT: PASS {summary}");
            exit(0);
        }
        Err(e) => {
            eprintln!("PGO_RECEIPT: FAIL {e}");
            exit(1);
        }
    }
}

fn cmd_bolt_proof(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("bolt-proof", &e),
    };
    let receipt = match required(&flags, "receipt") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("bolt-proof", &e),
    };
    match parse_ab_opt_ceremony_receipt(&receipt) {
        Some((proof_id, _, _)) => {
            println!("BOLT_PROOF: {proof_id}");
            exit(0);
        }
        None => {
            println!("BOLT_PROOF: MISSING");
            exit(1);
        }
    }
}

fn cmd_build_receipt(args: &[String]) {
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

    let mut gaps: Vec<String> = Vec::new();
    if let Some(list) = flags.get("gaps") {
        for item in list.split(',') {
            let item = item.trim();
            if !item.is_empty() {
                gaps.push(item.to_string());
            }
        }
    }

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
    let engine_provenance = extract_engine_provenance(cargo_lock_path);

    let tarball_sha256 = tarball_path
        .as_ref()
        .and_then(|p| hash_file(Path::new(p)).ok());
    let flatpak_sha256 = flatpak_path
        .as_ref()
        .and_then(|p| hash_file(Path::new(p)).ok());
    let installed_sha256 = installed_path
        .as_ref()
        .and_then(|p| hash_file(Path::new(p)).ok());

    let bolt_proof = if let Some(path) = ceremony_receipt.as_deref() {
        match parse_ab_opt_ceremony_receipt(path) {
            Some((proof_id, _, _)) => proof_id,
            None => {
                if bolt_status == "applied" {
                    gaps.push("bolt_proof_unverified".to_string());
                }
                "unverified".to_string()
            }
        }
    } else {
        if bolt_status == "applied" {
            gaps.push("bolt_proof_unverified".to_string());
        }
        "skipped".to_string()
    };

    let bolt_profile_sha256 = bolt_profile
        .as_ref()
        .and_then(|p| hash_file(Path::new(p)).ok());
    let pgo_workload = pgo_workload_receipt
        .as_deref()
        .and_then(parse_pgo_workload_receipt);

    let pgo_applied = pgo_status == "applied";
    let bolt_applied = bolt_status == "applied";
    if !pgo_applied {
        gaps.push("pgo_not_applied".to_string());
    }
    if !bolt_applied {
        gaps.push("bolt_not_applied".to_string());
    }
    if codegen_status != "pass" {
        gaps.push(format!("codegen_gate_{codegen_status}"));
    }
    gaps.sort();
    gaps.dedup();

    let status = if !gaps.is_empty() && strict {
        "FAILED"
    } else if !gaps.is_empty() {
        "PASS_WITH_GAPS"
    } else {
        "PASSED"
    }
    .to_string();

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

fn cmd_verify_artifact(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("verify-artifact", &e),
    };
    let out = match required(&flags, "out") {
        Ok(v) => v.clone(),
        Err(e) => die_usage("verify-artifact", &e),
    };
    let artifact = match required(&flags, "artifact") {
        Ok(v) => PathBuf::from(v),
        Err(e) => die_usage("verify-artifact", &e),
    };

    let verdict = verify_receipt_artifact(Path::new(&out), &artifact);
    println!("ARTIFACT_CONSISTENCY: {}", verdict.as_str());
    println!(
        "ARTIFACT_SHA256: {}",
        verdict.declared_sha256().unwrap_or("-")
    );
    if let Some(reason) = verdict.problem() {
        eprintln!("verify-artifact: {reason}");
    }
    exit(verdict.exit_code() as i32);
}

fn cmd_certify(args: &[String]) {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(e) => die_usage("certify", &e),
    };
    let artifact = flags
        .get("artifact")
        .cloned()
        .unwrap_or_else(|| DEFAULT_ARTIFACT.to_string());
    let build_receipt = flags
        .get("build-receipt")
        .cloned()
        .unwrap_or_else(|| DEFAULT_BUILD_RECEIPT.to_string());
    let long_receipt = flags
        .get("long-receipt")
        .cloned()
        .unwrap_or_else(|| DEFAULT_OUT.to_string());
    let text_receipt = flags
        .get("text-receipt")
        .cloned()
        .unwrap_or_else(|| DEFAULT_TEXT_RECEIPT.to_string());
    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| DEFAULT_RELEASE_RECEIPT.to_string());
    let strict = flags.contains_key("strict");

    let release_receipt = evaluate_certification(
        Path::new(&artifact),
        Path::new(&build_receipt),
        Path::new(&long_receipt),
        Path::new(&text_receipt),
        strict,
    );
    let json = match serde_json::to_string_pretty(&release_receipt) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("certify: failed to serialize release receipt: {e}");
            exit(1);
        }
    };
    if let Err(e) = write_file(&out, &json) {
        eprintln!("certify: failed to write {out}: {e}");
        exit(1);
    }
    if release_receipt.status == "CERTIFIED" {
        println!(
            "CERTIFIED: PASS log={out} sha256={}",
            release_receipt.artifacts.artifact_sha256
        );
        exit(0);
    }
    eprintln!(
        "REJECTED: FAIL log={out} reasons={}",
        release_receipt.reasons.join("; ")
    );
    exit(1);
}

fn append_line(out: &str, line: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(out).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(out)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")
}

fn write_file(out: &str, content: &str) -> std::io::Result<()> {
    if let Some(parent) = Path::new(out).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(out, content)
}

fn die_usage(subcommand: &str, message: &str) -> ! {
    eprintln!("{subcommand}: {message}");
    eprintln!("Run `pipe_long_receipt --help` for usage.");
    exit(2);
}
