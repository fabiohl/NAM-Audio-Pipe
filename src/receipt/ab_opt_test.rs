// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

use super::*;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ab_opt")
}

fn make_run(topology: &str, run: u64, p99: f64, max: f64, mean: f64) -> Value {
    json!({
        "event": "run",
        "topology": topology,
        "run": run,
        "frames_processed": 5120000,
        "cycles": {
            "p99": p99,
            "max": max,
            "mean": mean,
        }
    })
}

fn make_suite(p99: f64, max: f64, mean: f64) -> Vec<Value> {
    let mut runs = Vec::new();
    for topo in TOPOLOGIES {
        for run_idx in 1..=3 {
            runs.push(make_run(topo, run_idx, p99, max, mean));
        }
    }
    runs
}

#[test]
fn fixture_runs_reproduce_table_2_2_and_pgo_only_verdict() {
    let dir = fixtures_dir();
    let plain = read_runs(&dir, "plain").expect("read plain runs");
    let pgo = read_runs(&dir, "pgo").expect("read pgo runs");
    let bolt = read_runs(&dir, "pgo+bolt").expect("read pgo+bolt runs");
    assert_eq!(plain.len(), 9);
    assert_eq!(pgo.len(), 9);
    assert_eq!(bolt.len(), 9);

    let prov = Some(derive_provenance(&dir, None));
    let lbr = derive_bolt_lbr(&dir);

    let decision =
        evaluate_ceremony(&plain, &pgo, &bolt, 3, None, prov, lbr).expect("evaluate ceremony");

    // Verdict is PGO-ONLY when BOLT fails to meet the 1.0% promotion margin.
    assert_eq!(decision.verdict, "PGO-ONLY");
    assert_eq!(decision.verdict_reason, "bolt_margin_not_met");
    assert!(decision.effective_reproducible_gain);
    assert!(!decision.bolt_reproducible_gain);

    // Provenance and bolt_lbr are emitted deterministically from fixtures.
    let p = decision.provenance.as_ref().expect("provenance present");
    assert_eq!(p.git_sha, "5f743cb000000000000000000000000000000000");
    assert!(!p.git_dirty);
    assert_eq!(p.proof_subject, "ab_opt_bench");
    assert_eq!(p.cpu_bench, "8");
    assert_eq!(p.isolated, "8-11");
    assert_eq!(p.governor, "performance");
    assert_eq!(p.perf_event_paranoid_before, 2);
    assert_eq!(p.perf_event_paranoid_after, 2);

    let l = decision.bolt_lbr.as_ref().expect("bolt_lbr present");
    assert_eq!(l.samples, 12500);
    assert_eq!(l.mode, "lbr");

    // Verify per-topology gains (tolerance 0.02%).
    let get_topo = |name: &str| {
        decision
            .topologies
            .iter()
            .find(|t| t.topology == name)
            .unwrap_or_else(|| panic!("missing {name}"))
    };

    let a1 = get_topo("wavenet_a1");
    assert!((a1.pgo_gain_pct_p99 - (-1.68)).abs() < 0.02);
    assert!((a1.pgo_gain_pct_max - 7.10).abs() < 0.02);
    assert!((a1.pgo_gain_pct_mean - (-1.90)).abs() < 0.02);
    assert!((a1.bolt_gain_pct_p99.unwrap() - 0.54).abs() < 0.02);
    assert!((a1.bolt_gain_pct_max.unwrap() - (-36.74)).abs() < 0.02);
    assert!((a1.bolt_gain_pct_mean.unwrap() - 0.54).abs() < 0.02);
    assert!(a1.pgo_promoted);
    assert!(!a1.bolt_promoted);

    let a2 = get_topo("wavenet_a2");
    assert!((a2.pgo_gain_pct_p99 - (-1.84)).abs() < 0.02);
    assert!((a2.pgo_gain_pct_max - 3.46).abs() < 0.02);
    assert!((a2.pgo_gain_pct_mean - (-2.05)).abs() < 0.02);
    assert!((a2.bolt_gain_pct_p99.unwrap() - (-0.87)).abs() < 0.02);
    assert!((a2.bolt_gain_pct_max.unwrap() - (-3.70)).abs() < 0.02);
    assert!((a2.bolt_gain_pct_mean.unwrap() - (-1.16)).abs() < 0.02);
    assert!(a2.pgo_promoted);
    assert!(!a2.bolt_promoted);

    let lstm = get_topo("lstm");
    assert!((lstm.pgo_gain_pct_p99 - (-7.77)).abs() < 0.02);
    assert!((lstm.pgo_gain_pct_max - (-7.89)).abs() < 0.02);
    assert!((lstm.pgo_gain_pct_mean - (-9.69)).abs() < 0.02);
    assert!((lstm.bolt_gain_pct_p99.unwrap() - 0.47).abs() < 0.02);
    assert!((lstm.bolt_gain_pct_max.unwrap() - 0.75).abs() < 0.02);
    assert!((lstm.bolt_gain_pct_mean.unwrap() - 0.84).abs() < 0.02);
    assert!(lstm.pgo_promoted);
    assert!(!lstm.bolt_promoted);
}

#[test]
fn test_provenance_and_bolt_lbr_constructor() {
    let prov = CeremonyProvenance {
        git_sha: "abc1234".to_string(),
        git_dirty: false,
        cargo_lock_sha256: "locksha".to_string(),
        rustc_vv_sha256: "rustcsha".to_string(),
        engine_commit: "engine123".to_string(),
        run_id: "run-test".to_string(),
        proof_subject: "ab_opt_bench".to_string(),
        ts: "2026-11-06T12:00:00Z".to_string(),
        cpu_bench: "4".to_string(),
        isolated: "4-7".to_string(),
        governor: "performance".to_string(),
        perf_event_paranoid_before: 2,
        perf_event_paranoid_after: 1,
    };
    let json_val = prov.to_json();
    let parsed = CeremonyProvenance::from_json(&json_val).expect("parse prov");
    assert_eq!(prov, parsed);

    let lbr = CeremonyBoltLbr {
        samples: 5000,
        fdata_sha256: "fdatasha".to_string(),
        mode: "lbr".to_string(),
    };
    let lbr_json = lbr.to_json();
    let lbr_parsed = CeremonyBoltLbr::from_json(&lbr_json).expect("parse lbr");
    assert_eq!(lbr, lbr_parsed);
}

#[test]
fn frontier_tests_0_9999_pct_does_not_promote_pgo() {
    let plain = make_suite(100_000.0, 120_000.0, 95_000.0);
    let pgo = make_suite(99_000.1, 118_800.0, 94_050.0);
    let decision = evaluate_ceremony(&plain, &pgo, &[], 3, None, None, None).expect("evaluate");
    assert_eq!(decision.verdict, "UNOPTIMIZED");
    assert_eq!(decision.verdict_reason, "pgo_margin_not_met");
    assert!(!decision.effective_reproducible_gain);
}

#[test]
fn frontier_tests_1_0001_pct_promotes_pgo() {
    let plain = make_suite(100_000.0, 120_000.0, 95_000.0);
    let pgo = make_suite(98_999.9, 118_800.0, 94_050.0);
    let decision = evaluate_ceremony(&plain, &pgo, &[], 3, None, None, None).expect("evaluate");
    assert_eq!(decision.verdict, "PGO-ONLY");
    assert_eq!(decision.verdict_reason, "bolt_not_applied");
    assert!(decision.effective_reproducible_gain);
}

#[test]
fn frontier_tests_bolt_margin_boundary() {
    let plain = make_suite(100_000.0, 120_000.0, 95_000.0);
    let pgo = make_suite(98_000.0, 115_000.0, 90_000.0);

    let bolt_sub = make_suite(97_020.1, 113_000.0, 89_000.0);
    let dec_sub =
        evaluate_ceremony(&plain, &pgo, &bolt_sub, 3, None, None, None).expect("evaluate");
    assert_eq!(dec_sub.verdict, "PGO-ONLY");
    assert_eq!(dec_sub.verdict_reason, "bolt_margin_not_met");

    let bolt_sup = make_suite(97_019.9, 113_000.0, 89_000.0);
    let dec_sup =
        evaluate_ceremony(&plain, &pgo, &bolt_sup, 3, None, None, None).expect("evaluate");
    assert_eq!(dec_sup.verdict, "PGO+BOLT");
    assert_eq!(dec_sup.verdict_reason, "bolt_promoted");
    assert!(dec_sup.bolt_reproducible_gain);
}

#[test]
fn noisy_max_does_not_block_pgo_promotion_under_topology_rule() {
    let plain = make_suite(100_000.0, 120_000.0, 95_000.0);
    let mut pgo = make_suite(98_000.0, 115_000.0, 90_000.0);
    for r in &mut pgo {
        if r["topology"] == "wavenet_a1" && r["run"] == 2 {
            r["cycles"]["max"] = json!(150_000.0);
        }
    }
    let decision = evaluate_ceremony(&plain, &pgo, &[], 3, None, None, None).expect("evaluate");
    assert_eq!(decision.verdict, "PGO-ONLY");
    assert!(decision.effective_reproducible_gain);
}

#[test]
fn missing_run_fails_coverage() {
    let mut plain = make_suite(100_000.0, 120_000.0, 95_000.0);
    plain.retain(|r| !(r["topology"] == "wavenet_a1" && r["run"] == 3));
    let pgo = make_suite(90_000.0, 110_000.0, 85_000.0);

    let err = evaluate_ceremony(&plain, &pgo, &[], 3, None, None, None).unwrap_err();
    assert!(err.contains("missing topology 'wavenet_a1' run 3"));
}

#[test]
fn empty_and_invalid_receipt_fails() {
    let plain = make_suite(100_000.0, 120_000.0, 95_000.0);
    let empty: Vec<Value> = Vec::new();
    let err = evaluate_ceremony(&empty, &plain, &[], 3, None, None, None).unwrap_err();
    assert!(err.contains("plain/pgo receipts are empty"));

    let mut invalid = make_suite(100_000.0, 120_000.0, 95_000.0);
    invalid[0]["cycles"].as_object_mut().unwrap().remove("max");
    let err = evaluate_ceremony(&invalid, &plain, &[], 3, None, None, None).unwrap_err();
    assert!(err.contains("missing cycles"));
}

#[test]
fn two_topologies_fails_coverage() {
    let mut plain = make_suite(100_000.0, 120_000.0, 95_000.0);
    plain.retain(|r| r["topology"] != "lstm");
    let mut pgo = make_suite(90_000.0, 110_000.0, 85_000.0);
    pgo.retain(|r| r["topology"] != "lstm");

    let err = evaluate_ceremony(&plain, &pgo, &[], 3, None, None, None).unwrap_err();
    assert!(err.contains("missing topology 'lstm' run 1"));
}
