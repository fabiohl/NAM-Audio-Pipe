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
//!   pipe_ab_opt compare --dir <dir> --out <file> --runs <N> [--bolt-applied 0|1]
//!                       [--bolt-cause <text>]
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

    let (plain_p99, _, _) = envelope(&plain, "plain", cfg.runs).unwrap_or_else(|e| fail(&e));
    let (pgo_p99, _, _) = envelope(&pgo, "pgo", cfg.runs).unwrap_or_else(|e| fail(&e));
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
    let mut doc = Map::new();
    doc.insert("schema_version".to_string(), json!(1));
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

    let effective = match (bolt_applied, bolt_p99) {
        (true, Some(bolt)) if bolt <= pgo_p99 => ("pgo+bolt", bolt),
        _ => ("pgo", pgo_p99),
    };
    let verdict;
    let rollback: String;
    if effective.1 > plain_p99 {
        verdict = "UNOPTIMIZED";
        rollback = format!(
            "The effective artifact ({}) regressed the p99 envelope vs PLAIN; do not announce any optimization.",
            effective.0
        );
    } else if effective.0 == "pgo+bolt" {
        verdict = "PGO+BOLT";
        rollback = String::new();
    } else if bolt_applied {
        verdict = "PGO-ONLY";
        rollback = "BOLT regressed the p99 tail-latency envelope vs PGO; PGO-only is the explicit fallback.".to_string();
    } else {
        verdict = "PGO-ONLY";
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
