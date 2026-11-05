// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Typed comparison stage of the A/B optimization ceremony.
//!
//! Reads the per-variant JSONL receipts produced by `ab_opt_bench`
//! (`target/logs/ab-opt-{plain,pgo,pgo+bolt}.json`), derives the median p99/max/
//! mean cycle envelope, applies the promotion rule (an optimization is
//! announced only when it preserves or improves the measured envelope) and
//! writes `target/logs/ab-opt-ceremony-receipt.json` for the release chain.
//!
//! Usage:
//!   pipe_ab_opt compare --dir `<dir>` --out `<file>` --runs `<N>` [--bolt-applied 0|1]
//!                       [--bolt-cause `<text>`]
//!
//! Exit codes: 0 proven/fallback verdict; 1 unoptimized or unusable receipts;
//! 2 usage error.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

use serde_json::{Map, Value, json};

const USAGE: &str = "usage: pipe_ab_opt compare --dir <dir> --out <file> --runs <N> \
    [--bolt-applied 0|1] [--bolt-cause <text>]";

const TOPOLOGIES: [&str; 3] = ["wavenet_a1", "wavenet_a2", "lstm"];
const MIN_RUNS: u64 = 3;
/// Pending PO approval: retained for the receipt only, never used to decide a
/// promotion. A BOLT promotion requires a strict non-tie win plus complete
/// evidence, independently of this margin.
const BOLT_GAIN_THRESHOLD_PCT_PENDING_PO: f64 = 5.0;

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(action) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        exit(2);
    };
    match action {
        "compare" => compare(&args[1..]),
        "-h" | "--help" => {
            println!("{USAGE}");
            exit(0);
        }
        _ => {
            eprintln!("{USAGE}");
            exit(2);
        }
    }
}

struct Cfg {
    dir: PathBuf,
    out: PathBuf,
    runs: u64,
    bolt_applied: Option<bool>,
    bolt_cause: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Cfg, String> {
    let mut cfg = Cfg {
        dir: PathBuf::from("target/logs"),
        out: PathBuf::from("target/logs/ab-opt-ceremony-receipt.json"),
        runs: 3,
        bolt_applied: None,
        bolt_cause: None,
    };
    let mut i = 0;
    while i < args.len() {
        let value = |i: usize| {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{} requires a value", args[i]))
        };
        match args[i].as_str() {
            "--dir" => {
                cfg.dir = PathBuf::from(value(i)?);
                i += 2;
            }
            "--out" => {
                cfg.out = PathBuf::from(value(i)?);
                i += 2;
            }
            "--runs" => {
                cfg.runs = value(i)?
                    .parse()
                    .map_err(|_| "--runs requires an integer".to_string())?;
                i += 2;
            }
            "--bolt-applied" => {
                cfg.bolt_applied = Some(match value(i)?.as_str() {
                    "0" => false,
                    "1" => true,
                    other => return Err(format!("--bolt-applied expects 0 or 1, got {other:?}")),
                });
                i += 2;
            }
            "--bolt-cause" => {
                cfg.bolt_cause = Some(value(i)?);
                i += 2;
            }
            flag => return Err(format!("unknown flag: {flag}")),
        }
    }
    Ok(cfg)
}

fn read_runs(dir: &Path, variant: &str) -> Result<Vec<Value>, String> {
    let path = dir.join(format!("ab-opt-{variant}.json"));
    let content =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut runs = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
        if value.get("event").and_then(Value::as_str) == Some("run") {
            runs.push(value);
        }
    }
    Ok(runs)
}

fn fail(message: &str) -> ! {
    eprintln!("pipe_ab_opt: {message}");
    exit(1);
}

fn envelope(runs: &[Value], name: &str, expected: u64) -> Result<(f64, f64, f64), String> {
    let actual = runs.len() as u64;
    if actual < expected {
        return Err(format!(
            "{name} receipt has {actual} run events, expected at least {expected}"
        ));
    }
    let get = |field: &str| {
        median_num(runs, field).ok_or_else(|| format!("{name} receipt is missing cycles.{field}"))
    };
    Ok((get("p99")?, get("max")?, get("mean")?))
}

fn median_num(runs: &[Value], field: &str) -> Option<f64> {
    if runs.is_empty() {
        return None;
    }
    let mut values: Vec<f64> = runs
        .iter()
        .filter_map(|run| run.get("cycles")?.get(field)?.as_f64())
        .collect();
    if values.len() != runs.len() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    values.get(values.len() / 2).copied()
}

fn median_cycles(runs: &[Value], field: &str) -> Option<u64> {
    median_num(runs, field).map(|value| value.round() as u64)
}

fn median_pmu(runs: &[Value], key: &str) -> Option<u64> {
    if runs.is_empty() {
        return None;
    }
    let available = runs.iter().all(|run| {
        run.get("pmu_availability")
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
            == Some("ok")
    });
    if !available {
        return None;
    }
    let mut values: Vec<u64> = runs
        .iter()
        .filter_map(|run| run.get("pmu")?.get(key)?.as_u64())
        .collect();
    if values.len() != runs.len() {
        return None;
    }
    values.sort_unstable();
    values.get(values.len() / 2).copied()
}

fn pct(new: Option<f64>, old: Option<f64>) -> Option<f64> {
    let (new, old) = (new?, old?);
    if old == 0.0 {
        return None;
    }
    Some((new - old) / old * 100.0)
}

fn variant_doc(runs: &[Value]) -> Value {
    json!({
        "median_p99_cycles": median_cycles(runs, "p99"),
        "median_max_cycles": median_cycles(runs, "max"),
        "median_mean_cycles": median_num(runs, "mean"),
    })
}

fn gains(from: &[Value], to: &[Value]) -> Map<String, Value> {
    let mut map = Map::new();
    for field in ["p99", "max", "mean"] {
        map.insert(
            field.to_string(),
            json!(pct(median_num(to, field), median_num(from, field))),
        );
    }
    map
}

fn pmu_gains(from: &[Value], to: &[Value]) -> Map<String, Value> {
    let mut map = Map::new();
    for key in ["cycles", "instructions", "itlb_misses", "icache_misses"] {
        let new = median_pmu(to, key).map(|v| v as f64);
        let old = median_pmu(from, key).map(|v| v as f64);
        map.insert(key.to_string(), json!(pct(new, old)));
    }
    map
}

fn fmt(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |v| format!("{v:.2}%"))
}

fn fmt_num(value: Option<f64>) -> String {
    value.map_or_else(
        || "n/a".to_string(),
        |v| {
            if v.fract() == 0.0 {
                format!("{v:.0}")
            } else {
                format!("{v:.2}")
            }
        },
    )
}

#[derive(Clone, Copy)]
struct RunMetrics {
    p99: f64,
    max: f64,
    samples: u64,
}

fn run_metrics(r: &Value) -> Option<RunMetrics> {
    let cycles = r.get("cycles")?;
    let p99 = cycles.get("p99")?.as_f64()?;
    let max = cycles.get("max")?.as_f64()?;
    let mean = cycles.get("mean")?.as_f64()?;
    if !p99.is_finite() || !max.is_finite() || !mean.is_finite() {
        return None;
    }
    if p99 <= 0.0 || max <= 0.0 || max < p99 {
        return None;
    }
    let samples = r.get("frames_processed")?.as_u64()?;
    if samples == 0 {
        return None;
    }
    Some(RunMetrics { p99, max, samples })
}

fn extract_metrics_map(
    runs: &[Value],
    name: &str,
) -> Result<std::collections::BTreeMap<(String, u64), RunMetrics>, String> {
    let mut map = std::collections::BTreeMap::new();
    for r in runs {
        let topo = r
            .get("topology")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("{name} run event is missing a topology"))?;
        let run_idx = r
            .get("run")
            .and_then(Value::as_u64)
            .filter(|run| *run >= 1)
            .ok_or_else(|| format!("{name} run event for topology '{topo}' is missing run"))?;
        let metrics = run_metrics(r).ok_or_else(|| {
            format!(
                "{name} run {} of topology '{topo}' has non-finite, non-positive or missing cycles/metrics",
                run_idx
            )
        })?;
        if map.insert((topo.to_string(), run_idx), metrics).is_some() {
            return Err(format!(
                "{name} receipt repeats topology '{topo}' run {run_idx}"
            ));
        }
    }
    Ok(map)
}

fn require_full_coverage(
    map: &std::collections::BTreeMap<(String, u64), RunMetrics>,
    name: &str,
    runs: u64,
) -> Result<(), String> {
    if runs < MIN_RUNS {
        return Err(format!(
            "--runs must be at least {MIN_RUNS} (got {runs}); tail comparison below that is not certifiable"
        ));
    }
    for topo in TOPOLOGIES {
        for run_idx in 1..=runs {
            if !map.contains_key(&(topo.to_string(), run_idx)) {
                return Err(format!(
                    "{name} receipt is missing topology '{topo}' run {run_idx} (INCONCLUSIVE/GAP)"
                ));
            }
        }
    }
    let expected = TOPOLOGIES.len() * runs as usize;
    if map.len() != expected {
        return Err(format!(
            "{name} receipt has {} topology/run pairs, expected exactly {expected}",
            map.len()
        ));
    }
    Ok(())
}

fn compare(args: &[String]) {
    let cfg = match parse_args(args) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("pipe_ab_opt: {e}");
            eprintln!("{USAGE}");
            exit(2);
        }
    };
    let plain = match read_runs(&cfg.dir, "plain") {
        Ok(runs) => runs,
        Err(e) => {
            eprintln!("pipe_ab_opt: {e}");
            exit(1);
        }
    };
    let pgo = match read_runs(&cfg.dir, "pgo") {
        Ok(runs) => runs,
        Err(e) => {
            eprintln!("pipe_ab_opt: {e}");
            exit(1);
        }
    };
    if plain.is_empty() || pgo.is_empty() {
        eprintln!("pipe_ab_opt: plain/pgo receipts are empty; cannot compare");
        exit(1);
    }
    let bolt = match cfg.bolt_applied {
        Some(false) => Vec::new(),
        Some(true) => read_runs(&cfg.dir, "pgo+bolt").unwrap_or_else(|e| fail(&e)),
        None => {
            if cfg.dir.join("ab-opt-pgo+bolt.json").exists() {
                read_runs(&cfg.dir, "pgo+bolt").unwrap_or_else(|e| fail(&e))
            } else {
                Vec::new()
            }
        }
    };
    if cfg.bolt_applied == Some(true) && bolt.is_empty() {
        fail("BOLT marked applied but ab-opt-pgo+bolt.json has no run events");
    }
    let bolt_applied = !bolt.is_empty();

    let (plain_p99, plain_max, _) =
        envelope(&plain, "plain", cfg.runs).unwrap_or_else(|e| fail(&e));
    let (pgo_p99, pgo_max, _) = envelope(&pgo, "pgo", cfg.runs).unwrap_or_else(|e| fail(&e));
    let bolt_p99 = if bolt_applied {
        Some(
            envelope(&bolt, "pgo+bolt", cfg.runs)
                .unwrap_or_else(|e| fail(&e))
                .0,
        )
    } else {
        None
    };

    let mut variants = Map::new();
    variants.insert("plain".to_string(), variant_doc(&plain));
    variants.insert("pgo".to_string(), variant_doc(&pgo));
    if bolt_applied {
        variants.insert("pgo+bolt".to_string(), variant_doc(&bolt));
    }

    let pgo_gain = gains(&plain, &pgo);
    let pmu_pgo_gain = pmu_gains(&plain, &pgo);
    let bolt_gain = bolt_applied.then(|| gains(&pgo, &bolt));
    let pmu_bolt_gain = bolt_applied.then(|| pmu_gains(&pgo, &bolt));

    // Per-run ledger and reproducible gain evaluation. Every topology × run
    // must be present in plain, PGO and (when applied) BOLT; defaults and
    // silent drops are treated as INCONCLUSIVE/GAP.
    let plain_map = extract_metrics_map(&plain, "plain").unwrap_or_else(|e| fail(&e));
    let pgo_map = extract_metrics_map(&pgo, "pgo").unwrap_or_else(|e| fail(&e));
    let bolt_map = extract_metrics_map(&bolt, "pgo+bolt").unwrap_or_else(|e| fail(&e));
    require_full_coverage(&plain_map, "plain", cfg.runs).unwrap_or_else(|e| fail(&e));
    require_full_coverage(&pgo_map, "pgo", cfg.runs).unwrap_or_else(|e| fail(&e));
    if bolt_applied {
        require_full_coverage(&bolt_map, "pgo+bolt", cfg.runs).unwrap_or_else(|e| fail(&e));
    }

    let mut ledger = Vec::new();
    let mut min_bolt_gain_pct: Option<f64> = None;
    let mut min_bolt_max_gain_pct: Option<f64> = None;
    let mut all_bolt_promoted = bolt_applied;
    let mut all_effective_better_than_plain = true;

    let gain_pct = |new: f64, old: f64| -> f64 {
        if old > 0.0 {
            (old - new) / old * 100.0
        } else {
            f64::NEG_INFINITY
        }
    };

    for topo in TOPOLOGIES {
        for run_idx in 1..=cfg.runs {
            let key = (topo.to_string(), run_idx);
            let plain_m = plain_map.get(&key).copied().unwrap_or_else(|| {
                fail(&format!(
                    "plain receipt is missing topology '{topo}' run {run_idx} (INCONCLUSIVE/GAP)"
                ))
            });
            let pgo_m = pgo_map.get(&key).copied().unwrap_or_else(|| {
                fail(&format!(
                    "pgo receipt is missing topology '{topo}' run {run_idx} (INCONCLUSIVE/GAP)"
                ))
            });
            let bolt_m = bolt_map.get(&key).copied();

            let pgo_gain_pct = gain_pct(pgo_m.p99, plain_m.p99);
            let pgo_max_gain_pct = gain_pct(pgo_m.max, plain_m.max);

            let (bolt_gain_pct, bolt_max_gain_pct, bolt_promoted_this_run) = if let Some(bm) =
                bolt_m
            {
                let p99_gain = gain_pct(bm.p99, pgo_m.p99);
                let max_gain = gain_pct(bm.max, pgo_m.max);
                min_bolt_gain_pct = Some(min_bolt_gain_pct.map_or(p99_gain, |m| m.min(p99_gain)));
                min_bolt_max_gain_pct =
                    Some(min_bolt_max_gain_pct.map_or(max_gain, |m| m.min(max_gain)));
                let promoted = p99_gain.is_finite()
                    && max_gain.is_finite()
                    && p99_gain > 0.0
                    && max_gain > 0.0;
                if !promoted {
                    all_bolt_promoted = false;
                }
                (Some(p99_gain), Some(max_gain), promoted)
            } else {
                if bolt_applied {
                    all_bolt_promoted = false;
                }
                (None, None, false)
            };

            let effective_m = if bolt_promoted_this_run {
                bolt_m.unwrap_or(pgo_m)
            } else {
                pgo_m
            };

            let effective_gain_pct = gain_pct(effective_m.p99, plain_m.p99);
            let effective_max_gain_pct = gain_pct(effective_m.max, plain_m.max);

            if effective_gain_pct <= 0.0 || effective_max_gain_pct <= 0.0 {
                all_effective_better_than_plain = false;
            }

            let mut entry = Map::new();
            entry.insert("topology".to_string(), json!(topo));
            entry.insert("run".to_string(), json!(run_idx));
            entry.insert(
                "samples".to_string(),
                json!(pgo_m.samples.min(plain_m.samples)),
            );
            entry.insert("plain_p99_cycles".to_string(), json!(plain_m.p99));
            entry.insert("pgo_p99_cycles".to_string(), json!(pgo_m.p99));
            entry.insert("bolt_p99_cycles".to_string(), json!(bolt_m.map(|m| m.p99)));
            entry.insert("effective_p99_cycles".to_string(), json!(effective_m.p99));
            entry.insert("plain_max_cycles".to_string(), json!(plain_m.max));
            entry.insert("pgo_max_cycles".to_string(), json!(pgo_m.max));
            entry.insert("bolt_max_cycles".to_string(), json!(bolt_m.map(|m| m.max)));
            entry.insert("effective_max_cycles".to_string(), json!(effective_m.max));
            entry.insert("pgo_gain_pct".to_string(), json!(pgo_gain_pct));
            entry.insert("pgo_max_gain_pct".to_string(), json!(pgo_max_gain_pct));
            entry.insert("bolt_gain_pct".to_string(), json!(bolt_gain_pct));
            entry.insert("bolt_max_gain_pct".to_string(), json!(bolt_max_gain_pct));
            entry.insert("effective_gain_pct".to_string(), json!(effective_gain_pct));
            entry.insert(
                "effective_max_gain_pct".to_string(),
                json!(effective_max_gain_pct),
            );
            entry.insert(
                "bolt_promoted_this_run".to_string(),
                json!(bolt_promoted_this_run),
            );
            ledger.push(Value::Object(entry));
        }
    }

    let bolt_reproducible_gain = bolt_applied
        && all_bolt_promoted
        && min_bolt_gain_pct.is_some_and(|g| g.is_finite() && g > 0.0)
        && min_bolt_max_gain_pct.is_some_and(|g| g.is_finite() && g > 0.0);

    let bolt_max = if bolt_applied {
        Some(
            envelope(&bolt, "pgo+bolt", cfg.runs)
                .unwrap_or_else(|e| fail(&e))
                .1,
        )
    } else {
        None
    };

    let effective = if bolt_applied && bolt_reproducible_gain {
        (
            "pgo+bolt",
            bolt_p99.unwrap_or(pgo_p99),
            bolt_max.unwrap_or(plain_p99),
        )
    } else {
        ("pgo", pgo_p99, pgo_max)
    };

    let effective_reproducible_gain =
        all_effective_better_than_plain && effective.1 <= plain_p99 && effective.2 <= plain_max;

    let mut doc = Map::new();
    doc.insert("schema_version".to_string(), json!(2));
    doc.insert("tool".to_string(), json!("ab-opt-ceremony"));
    doc.insert("runs".to_string(), json!(cfg.runs));
    doc.insert("bolt_applied".to_string(), json!(bolt_applied));
    doc.insert("variants".to_string(), Value::Object(variants));
    doc.insert("pgo_gain_pct".to_string(), Value::Object(pgo_gain.clone()));
    doc.insert("pmu_pgo_gain_pct".to_string(), Value::Object(pmu_pgo_gain));
    if let (Some(gain), Some(pmu_gain)) = (&bolt_gain, &pmu_bolt_gain) {
        doc.insert("bolt_gain_pct".to_string(), Value::Object(gain.clone()));
        doc.insert(
            "pmu_bolt_gain_pct".to_string(),
            Value::Object(pmu_gain.clone()),
        );
    }

    doc.insert("ledger".to_string(), Value::Array(ledger));
    doc.insert(
        "bolt_gain_threshold_pct".to_string(),
        json!(BOLT_GAIN_THRESHOLD_PCT_PENDING_PO),
    );
    doc.insert(
        "bolt_gain_threshold_status".to_string(),
        json!("pending_po_approval"),
    );
    doc.insert(
        "bolt_gain_threshold_used_for_promotion".to_string(),
        json!(false),
    );
    doc.insert("min_bolt_gain_pct".to_string(), json!(min_bolt_gain_pct));
    doc.insert(
        "min_bolt_max_gain_pct".to_string(),
        json!(min_bolt_max_gain_pct),
    );
    doc.insert(
        "bolt_reproducible_gain".to_string(),
        json!(bolt_reproducible_gain),
    );
    doc.insert(
        "effective_reproducible_gain".to_string(),
        json!(effective_reproducible_gain),
    );

    let verdict: String;
    let rollback: String;
    if effective.1 > plain_p99 || !effective_reproducible_gain {
        verdict = "UNOPTIMIZED".to_string();
        rollback = format!(
            "The effective artifact ({}) regressed or failed margin vs PLAIN; do not announce any optimization.",
            effective.0
        );
    } else if effective.0 == "pgo+bolt" {
        verdict = "PGO+BOLT".to_string();
        rollback = String::new();
    } else if bolt_applied {
        verdict = "PGO-ONLY".to_string();
        rollback = format!(
            "BOLT did not achieve a reproducible non-tie win (p99 and max) across all runs and topologies (min_bolt_gain = {}, min_bolt_max_gain = {}); PGO-only is the explicit fallback.",
            min_bolt_gain_pct.map_or_else(|| "n/a".to_string(), |g| format!("{g:.2}%")),
            min_bolt_max_gain_pct.map_or_else(|| "n/a".to_string(), |g| format!("{g:.2}%"))
        );
    } else {
        verdict = "PGO-ONLY".to_string();
        rollback = "BOLT not proven/applied on this host (PGO-only fallback).".to_string();
        doc.insert(
            "bolt_cause".to_string(),
            json!(
                cfg.bolt_cause
                    .clone()
                    .unwrap_or_else(|| "BOLT not requested".to_string())
            ),
        );
    }
    doc.insert("verdict".to_string(), json!(verdict));
    doc.insert("rollback".to_string(), json!(rollback));

    let pretty =
        serde_json::to_string_pretty(&Value::Object(doc)).unwrap_or_else(|_| "{}".to_string());
    if let Err(e) = fs::write(&cfg.out, format!("{pretty}\n")) {
        eprintln!("pipe_ab_opt: cannot write {}: {e}", cfg.out.display());
        exit(1);
    }

    println!();
    println!("VERDICT: {verdict}");
    if !rollback.is_empty() {
        println!("  {rollback}");
    }
    println!();
    if let (Some(bolt_gain), Some(pmu_bolt_gain)) = (bolt_gain, pmu_bolt_gain) {
        let read = |map: &Map<String, Value>, key: &str| map.get(key).and_then(Value::as_f64);
        println!(
            "  // Measured: BOLT gain p99={} (PGO p99={} → BOLT p99={} cyc), max={}, mean={}",
            fmt(read(&bolt_gain, "p99")),
            fmt_num(Some(pgo_p99)),
            fmt_num(median_num(&bolt, "p99")),
            fmt(read(&bolt_gain, "max")),
            fmt(read(&bolt_gain, "mean"))
        );
        println!(
            "  // Measured: BOLT iTLB={}, I-cache={}",
            fmt(read(&pmu_bolt_gain, "itlb_misses")),
            fmt(read(&pmu_bolt_gain, "icache_misses"))
        );
    }
    let read = |map: &Map<String, Value>, key: &str| map.get(key).and_then(Value::as_f64);
    println!(
        "  // Measured: PGO gain p99={}, max={}, mean={}",
        fmt(read(&pgo_gain, "p99")),
        fmt(read(&pgo_gain, "max")),
        fmt(read(&pgo_gain, "mean"))
    );
    println!();
    println!("  Receipt: {}", cfg.out.display());

    if verdict == "UNOPTIMIZED" {
        exit(1);
    }
}
