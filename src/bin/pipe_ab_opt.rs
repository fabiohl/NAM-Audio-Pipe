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
//! ```text
//!   pipe_ab_opt compare --dir <dir> --out <file> --runs <N> [--bolt-applied 0|1]
//!                       [--bolt-cause <text>]
//! ```
//!
//! Exit codes: 0 proven/fallback verdict; 1 unoptimized or unusable receipts;
//! 2 usage error.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::exit;

use nam_audio_pipe::receipt::ab_opt::{
    derive_bolt_lbr, derive_provenance, evaluate_ceremony, fmt, fmt_num, median_num, read_runs,
};
use serde_json::{Map, Value};

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

fn fail(message: &str) -> ! {
    eprintln!("pipe_ab_opt: {message}");
    exit(1);
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
            if cfg.dir.join("ab-opt-pgo+bolt.json").exists()
                || cfg.dir.join("ab-opt-pgo-bolt.json").exists()
            {
                read_runs(&cfg.dir, "pgo+bolt").unwrap_or_else(|e| fail(&e))
            } else {
                Vec::new()
            }
        }
    };
    if cfg.bolt_applied == Some(true) && bolt.is_empty() {
        fail("BOLT marked applied but ab-opt-pgo+bolt.json has no run events");
    }

    let provenance = Some(derive_provenance(&cfg.dir, None));
    let bolt_applied = cfg.bolt_applied.unwrap_or(!bolt.is_empty());
    let bolt_lbr = if bolt_applied {
        derive_bolt_lbr(&cfg.dir)
    } else {
        None
    };

    let decision = match evaluate_ceremony(
        &plain,
        &pgo,
        &bolt,
        cfg.runs,
        cfg.bolt_cause.as_deref(),
        provenance,
        bolt_lbr,
    ) {
        Ok(d) => d,
        Err(e) => fail(&e),
    };

    let pretty = serde_json::to_string_pretty(&Value::Object(decision.doc))
        .unwrap_or_else(|_| "{}".to_string());
    if let Err(e) = fs::write(&cfg.out, format!("{pretty}\n")) {
        eprintln!("pipe_ab_opt: cannot write {}: {e}", cfg.out.display());
        exit(1);
    }

    println!();
    println!("VERDICT: {}", decision.verdict);
    if !decision.rollback.is_empty() {
        println!("  {}", decision.rollback);
    }
    println!();
    if let (Some(bolt_gain), Some(pmu_bolt_gain)) = (&decision.bolt_gain, &decision.pmu_bolt_gain) {
        let read = |map: &Map<String, Value>, key: &str| map.get(key).and_then(Value::as_f64);
        println!(
            "  // Measured: BOLT gain p99={} (PGO p99={} → BOLT p99={} cyc), max={}, mean={}",
            fmt(read(bolt_gain, "p99")),
            fmt_num(Some(decision.pgo_p99)),
            fmt_num(median_num(&bolt, "p99")),
            fmt(read(bolt_gain, "max")),
            fmt(read(bolt_gain, "mean"))
        );
        println!(
            "  // Measured: BOLT iTLB={}, I-cache={}",
            fmt(read(pmu_bolt_gain, "itlb_misses")),
            fmt(read(pmu_bolt_gain, "icache_misses"))
        );
    }
    let read = |map: &Map<String, Value>, key: &str| map.get(key).and_then(Value::as_f64);
    println!(
        "  // Measured: PGO gain p99={}, max={}, mean={}",
        fmt(read(&decision.pgo_gain, "p99")),
        fmt(read(&decision.pgo_gain, "max")),
        fmt(read(&decision.pgo_gain, "mean"))
    );
    println!();
    println!("  Receipt: {}", cfg.out.display());
}
