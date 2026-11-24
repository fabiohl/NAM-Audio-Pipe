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

#[test]
fn bolt_promoted_over_plain_when_pgo_alone_regresses_on_one_topology() {
    let mut plain = Vec::new();
    let mut pgo = Vec::new();
    let mut bolt = Vec::new();
    for run in 1..=3 {
        // wavenet_a1: PGO alone regresses (+0.27% p99), BOLT recovers (-1.94% vs Plain)
        plain.push(make_run("wavenet_a1", run, 616_698.0, 679_338.0, 589_377.0));
        pgo.push(make_run("wavenet_a1", run, 618_336.0, 840_996.0, 585_659.0));
        bolt.push(make_run("wavenet_a1", run, 604_710.0, 788_868.0, 576_965.0));

        // wavenet_a2 and lstm: both PGO and PGO+BOLT beat Plain >= 1.0%
        plain.push(make_run("wavenet_a2", run, 317_376.0, 333_738.0, 294_510.0));
        pgo.push(make_run("wavenet_a2", run, 311_598.0, 326_340.0, 289_914.0));
        bolt.push(make_run("wavenet_a2", run, 308_088.0, 320_706.0, 286_199.0));

        plain.push(make_run("lstm", run, 91_332.0, 99_558.0, 72_373.0));
        pgo.push(make_run("lstm", run, 84_294.0, 96_300.0, 65_687.0));
        bolt.push(make_run("lstm", run, 83_304.0, 94_698.0, 64_739.0));
    }

    let decision = evaluate_ceremony(&plain, &pgo, &bolt, 3, None, None, None).expect("evaluate");
    assert_eq!(decision.verdict, "PGO+BOLT");
    assert_eq!(decision.verdict_reason, "bolt_promoted_over_plain");
    assert!(decision.effective_reproducible_gain);
    assert!(decision.bolt_reproducible_gain);
    assert!(decision.rollback.is_empty());

    let a1 = decision
        .topologies
        .iter()
        .find(|t| t.topology == "wavenet_a1")
        .expect("wavenet_a1");
    assert!(!a1.pgo_promoted);
    assert!(a1.bolt_promoted);
    assert!(a1.bolt_vs_plain_promoted);

    // When PGO+BOLT also fails to beat Plain by >= 1.0%, fallback remains UNOPTIMIZED.
    let mut bolt_weak = bolt.clone();
    for r in &mut bolt_weak {
        if r["topology"] == "wavenet_a1" {
            r["cycles"]["p99"] = json!(615_000.0);
        }
    }
    let dec_weak =
        evaluate_ceremony(&plain, &pgo, &bolt_weak, 3, None, None, None).expect("evaluate weak");
    assert_eq!(dec_weak.verdict, "UNOPTIMIZED");
    assert_eq!(dec_weak.verdict_reason, "pgo_margin_not_met");
    assert!(!dec_weak.effective_reproducible_gain);
    assert!(!dec_weak.bolt_reproducible_gain);
}

#[test]
fn per_topology_pmu_gains_isolate_each_topology() {
    let mut plain = make_suite(100_000.0, 120_000.0, 95_000.0);
    let mut pgo = make_suite(90_000.0, 110_000.0, 85_000.0);
    for (runs, itlb_a1, itlb_a2, itlb_lstm) in
        [(&mut plain, 2u64, 200, 40), (&mut pgo, 250u64, 200, 10)]
    {
        for r in runs.iter_mut() {
            let itlb = match r["topology"].as_str().unwrap() {
                "wavenet_a1" => itlb_a1,
                "wavenet_a2" => itlb_a2,
                _ => itlb_lstm,
            };
            r["pmu"] = json!({
                "cycles": 1_000_000,
                "instructions": 2_000_000,
                "itlb_misses": itlb,
                "icache_misses": 100,
            });
            r["pmu_availability"] = json!({
                "cycles": "ok",
                "instructions": "ok",
                "itlb_misses": "ok",
                "icache_misses": "ok",
            });
        }
    }

    let decision = evaluate_ceremony(&plain, &pgo, &[], 3, None, None, None).expect("evaluate");
    let topos = decision.doc["topologies"].as_object().expect("topologies");
    let a1_itlb = topos["wavenet_a1"]["pmu_pgo_gain_pct"]["itlb_misses"]
        .as_f64()
        .expect("a1 itlb");
    let a2_itlb = topos["wavenet_a2"]["pmu_pgo_gain_pct"]["itlb_misses"]
        .as_f64()
        .expect("a2 itlb");
    let lstm_itlb = topos["lstm"]["pmu_pgo_gain_pct"]["itlb_misses"]
        .as_f64()
        .expect("lstm itlb");
    assert!((a1_itlb - 12400.0).abs() < 1e-6);
    assert!(a2_itlb.abs() < 1e-6);
    assert!((lstm_itlb - (-75.0)).abs() < 1e-6);
}
