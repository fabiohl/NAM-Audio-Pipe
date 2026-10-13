// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Coherence and tamper-rejection tests for the two long-receipt readers
//! (S4-T4.4): the unified-schema JSONL (engine `testing::receipt`) and the
//! human `.txt` receipt (`receipt::long`). Both must certify the same valid run
//! and both must reject a forged/tampered one.
#![cfg(feature = "testing")]

use std::io::Write;

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
