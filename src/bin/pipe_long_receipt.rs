// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Emitter/verifier for the unified-schema long-audit receipt
//! (`target/logs/long-audit-receipt.jsonl`, S4-T4.4).
//!
//! `utils/tests-long.sh` appends one machine-readable line per completed phase
//! using the same engine receipt schema the engine and NAM-Plug emit; the
//! human-readable `target/logs/long-receipt.txt` remains the derived view. The
//! suite-level `overall` line is derived in Rust by `summary`.
//!
//! Subcommands: `append`, `summary`, `validate [--strict]`.
//! Exit codes: 0 success; 1 I/O or verification error; 2 usage error.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::str::FromStr;

use nam_audio_pipe::receipt::LONG_ROSTER;
use nam_audio_pipe::receipt::fail_reasons::{self, FailReasonRecord};
use nam_audio_pipe::receipt::jsonl::{verify_long_jsonl, verify_long_jsonl_strict};
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
    println!();
    println!(
        "Status values: PASSED, FAILED, SKIPPED, INCONCLUSIVE, SKIP_CAPABILITY, NOT_RUN, SIMULATED"
    );
    println!("Canonical roster: {}", LONG_ROSTER.join(", "));
    println!("Default output file: {DEFAULT_OUT}");
}

const VALUE_LESS_FLAGS: &[&str] = &["strict"];

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
