// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Coherence and tamper-rejection tests for the two long-receipt readers:
//! the unified-schema JSONL (engine `testing::receipt`) and the
//! human `.txt` receipt (`receipt::long`). Both must certify the same valid run
//! and both must reject a forged/tampered one.
#![cfg(feature = "testing")]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use nam_audio_pipe::receipt::LONG_ROSTER;
use nam_audio_pipe::receipt::jsonl::{derived_status, verify_long_jsonl, verify_long_jsonl_strict};
use nam_audio_pipe::receipt::long::parse_long_receipt;
use neural_amp_modeler_rs::testing::receipt::{
    LongAuditReceipt, LongPhaseReceipt, LongPhaseStatus as MotorStatus,
};

fn jsonl_phase(id: &str, status: MotorStatus, tests: u64, gaps: &[&str]) -> LongPhaseReceipt {
    LongPhaseReceipt {
        phase_id: id.to_string(),
        name: id.to_string(),
        status,
        duration_ms: 1000,
        tests_executed: tests,
        gaps: gaps.iter().map(|s| s.to_string()).collect(),
        timestamp: "2026-10-13T00:00:00Z".to_string(),
        ..Default::default()
    }
}

fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pipe-receipt-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path
}

fn clean_jsonl() -> String {
    let mut phases = Vec::new();
    for &id in &LONG_ROSTER {
        let tests = if id == "overall" { 70 } else { 10 };
        phases.push(jsonl_phase(id, MotorStatus::Passed, tests, &[]));
    }
    LongAuditReceipt { phases }.render_jsonl()
}

fn clean_text_receipt() -> String {
    let mut s = String::new();
    s.push_str("SUITE: tests-long\n");
    s.push_str("STRICT: 1\n");
    s.push_str("NAM_RT_STRICT: 1\n");
    s.push_str("MODE: full\n");
    s.push_str("SOAK_PURPOSE: accelerated_timeline — compressed timeline\n");
    s.push_str("ENDURANCE_PURPOSE: real_wall_clock — wall clock\n");
    for n in 1..=7 {
        let env = if n == 3 || n == 4 {
            " env=pinned:1,isolated:1,fifo:1"
        } else {
            ""
        };
        s.push_str(&format!(
            "PHASE{n}: PASS log=target/logs/phase{n}.log duration_ms=1000{env}\n"
        ));
    }
    s.push_str("OVERALL: PASSED\n");
    s
}

#[test]
fn both_readers_certify_the_same_clean_run() {
    let jsonl_path = write_temp("clean.jsonl", &clean_jsonl());
    let audit = verify_long_jsonl(&jsonl_path).unwrap();
    assert_eq!(derived_status(&audit), MotorStatus::Passed);
    assert!(verify_long_jsonl_strict(&jsonl_path).is_ok());

    let text = clean_text_receipt();
    let receipt = parse_long_receipt(&text).unwrap();
    assert!(receipt.verify_release_certification().is_ok());
    assert_eq!(receipt.overall, "PASSED");
}

#[test]
fn jsonl_forged_green_over_gap_is_rejected() {
    let mut phases = Vec::new();
    for &id in &LONG_ROSTER {
        if id == "phase5" {
            phases.push(jsonl_phase(
                id,
                MotorStatus::Skipped,
                0,
                &["phase5:declared_gap"],
            ));
        } else {
            let tests = if id == "overall" { 60 } else { 10 };
            // terminal overall is forged PASSED.
            phases.push(jsonl_phase(id, MotorStatus::Passed, tests, &[]));
        }
    }
    let path = write_temp("forged.jsonl", &LongAuditReceipt { phases }.render_jsonl());
    assert!(verify_long_jsonl(&path).is_err());
    assert!(verify_long_jsonl_strict(&path).is_err());
}

#[test]
fn jsonl_injected_overall_comment_is_rejected() {
    let path = write_temp(
        "injected.jsonl",
        &format!("{}# OVERALL: PASSED\n", clean_jsonl()),
    );
    assert!(verify_long_jsonl(&path).is_err());
}

#[test]
fn simulated_jsonl_is_rejected_strictly() {
    let mut phases = Vec::new();
    for &id in &LONG_ROSTER {
        phases.push(jsonl_phase(id, MotorStatus::Simulated, 0, &["simulated"]));
    }
    let path = write_temp("sim.jsonl", &LongAuditReceipt { phases }.render_jsonl());
    assert!(verify_long_jsonl(&path).is_ok());
    assert!(verify_long_jsonl_strict(&path).is_err());
}

#[test]
fn text_reader_rejects_injected_overall_comment() {
    let tampered = format!("{}# OVERALL: PASSED\n", clean_text_receipt());
    assert!(parse_long_receipt(&tampered).is_err());
}

#[test]
fn text_and_jsonl_verdicts_agree_on_a_gapped_run() {
    // A coherent gapped run: both readers accept it as non-strict and both
    // refuse it as a strict release.
    let mut phases = Vec::new();
    for &id in &LONG_ROSTER {
        if id == "phase7" {
            phases.push(jsonl_phase(
                id,
                MotorStatus::Skipped,
                0,
                &["phase7:declared_gap"],
            ));
        } else {
            let tests = if id == "overall" { 60 } else { 10 };
            phases.push(jsonl_phase(id, MotorStatus::Passed, tests, &[]));
        }
    }
    // Derive the overall line in the same way the emitter does.
    let mut audit = LongAuditReceipt { phases };
    audit.push_summary();
    let path = write_temp("gapped.jsonl", &audit.render_jsonl());
    assert!(verify_long_jsonl(&path).is_ok());
    assert!(verify_long_jsonl_strict(&path).is_err());
    assert_eq!(derived_status(&audit), MotorStatus::CompletedWithGaps);

    let text = clean_text_receipt()
        .replace(
            "PHASE7: PASS log=target/logs/phase7.log duration_ms=1000\n",
            "PHASE7: GAP log=target/logs/phase7.log duration_ms=1000\n",
        )
        .replace("OVERALL: PASSED", "OVERALL: COMPLETED_WITH_GAPS");
    let receipt = parse_long_receipt(&text).unwrap();
    assert!(receipt.audit().is_ok());
    assert!(receipt.verify_release_certification().is_err());
}

/// Long-audit evidence integrity (D1): `utils/tests-long.sh --simulate` is the
/// sanctioned non-executing structural surface and must NEVER destroy the
/// artifacts of a real audit. This test stages a synthetic real run under a
/// hermetic `NAM_LOG_ROOT`, invokes `--simulate`, and asserts the real phase
/// logs and both receipts are byte-identical while the planned placeholders are
/// registered only under `<root>/simulate/`.
#[test]
fn simulate_never_touches_real_audit_artifacts() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tmp = std::env::temp_dir().join(format!(
        "pipe-simulate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let logs = tmp.join("logs");
    std::fs::create_dir_all(&logs).unwrap();

    let artifacts = [
        "phase1-soak.log",
        "phase2-heap-audit.log",
        "phase3-rt-deadline.log",
        "phase4-rt-jitter.log",
        "phase5-concurrency.log",
        "phase6-endurance.log",
        "phase7-pw-resilience.log",
        "long-receipt.txt",
        "long-audit-receipt.jsonl",
    ];
    for name in artifacts {
        std::fs::write(logs.join(name), format!("REAL-EVIDENCE {name}\n")).unwrap();
    }
    let before: Vec<(&str, Vec<u8>)> = artifacts
        .iter()
        .map(|name| (*name, std::fs::read(logs.join(name)).unwrap()))
        .collect();

    let out = Command::new(root.join("utils/tests-long.sh"))
        .arg("--simulate")
        .current_dir(&root)
        .env("NAM_LOG_ROOT", &logs)
        .env_remove("NAM_RT_STRICT")
        .output()
        .expect("failed to execute utils/tests-long.sh --simulate");
    assert!(
        out.status.success(),
        "--simulate must exit 0 (got {:?}); stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    for (name, bytes) in &before {
        let now = std::fs::read(logs.join(name))
            .unwrap_or_else(|e| panic!("real artifact {name} vanished: {e}"));
        assert_eq!(
            &now, bytes,
            "real artifact {name} must be byte-identical after --simulate"
        );
    }

    // The planned phases are registered only in the isolated simulate dir.
    let sim_receipt =
        std::fs::read_to_string(logs.join("simulate/long-receipt.txt")).expect("simulate receipt");
    assert!(
        sim_receipt.contains("MODE: simulate"),
        "simulate receipt must record MODE: simulate"
    );
    for name in artifacts {
        let sim_path = logs.join("simulate").join(name);
        assert!(
            sim_path.exists(),
            "expected simulate placeholder {name} in the isolated dir"
        );
    }

    let _ = std::fs::remove_dir_all(&tmp);
}
