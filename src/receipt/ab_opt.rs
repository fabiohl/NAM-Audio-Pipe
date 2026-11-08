// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

//! Pure decision logic and statistical aggregation for the A/B optimization ceremony.
//!
//! Evaluates plain, PGO and (when applied) BOLT benchmark receipts against
//! promotion invariants and derives the structured receipt and machine verdict.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

pub const TOPOLOGIES: [&str; 3] = ["wavenet_a1", "wavenet_a2", "lstm"];
pub const MIN_RUNS: u64 = 3;
pub const PROMOTION_MARGIN_PCT: f64 = 1.0;
pub const MARGIN_EPSILON: f64 = 1e-9;

/// Retained for the receipt only, never used to decide a promotion.
/// A BOLT promotion requires a strict non-tie win plus complete evidence,
/// independently of this margin.
pub const BOLT_GAIN_THRESHOLD_PCT_PENDING_PO: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunMetrics {
    pub p99: f64,
    pub max: f64,
    pub samples: u64,
}

pub fn run_metrics(r: &Value) -> Option<RunMetrics> {
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

pub fn extract_metrics_map(
    runs: &[Value],
    name: &str,
) -> Result<BTreeMap<(String, u64), RunMetrics>, String> {
    let mut map = BTreeMap::new();
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
                "{name} run {run_idx} of topology '{topo}' has non-finite, non-positive or missing cycles/metrics"
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

pub fn require_full_coverage(
    map: &BTreeMap<(String, u64), RunMetrics>,
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

pub fn envelope(runs: &[Value], name: &str, expected: u64) -> Result<(f64, f64, f64), String> {
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

pub fn median_num(runs: &[Value], field: &str) -> Option<f64> {
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

pub fn median_topology_num(runs: &[Value], topology: &str, field: &str) -> Option<f64> {
    let mut values: Vec<f64> = runs
        .iter()
        .filter(|r| r.get("topology").and_then(Value::as_str) == Some(topology))
        .filter_map(|run| run.get("cycles")?.get(field)?.as_f64())
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    values.get(values.len() / 2).copied()
}

pub fn median_cycles(runs: &[Value], field: &str) -> Option<u64> {
    median_num(runs, field).map(|value| value.round() as u64)
}

pub fn median_pmu(runs: &[Value], key: &str) -> Option<u64> {
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

pub fn pct(new: Option<f64>, old: Option<f64>) -> Option<f64> {
    let (new, old) = (new?, old?);
    if old == 0.0 {
        return None;
    }
    Some((new - old) / old * 100.0)
}

pub fn variant_doc(runs: &[Value]) -> Value {
    json!({
        "deprecated": "aggregated across topologies; see 'topologies' for authoritative metrics",
        "median_p99_cycles": median_cycles(runs, "p99"),
        "median_max_cycles": median_cycles(runs, "max"),
        "median_mean_cycles": median_num(runs, "mean"),
    })
}

pub fn gains(from: &[Value], to: &[Value]) -> Map<String, Value> {
    let mut map = Map::new();
    for field in ["p99", "max", "mean"] {
        map.insert(
            field.to_string(),
            json!(pct(median_num(to, field), median_num(from, field))),
        );
    }
    map
}

pub fn pmu_gains(from: &[Value], to: &[Value]) -> Map<String, Value> {
    let mut map = Map::new();
    for key in ["cycles", "instructions", "itlb_misses", "icache_misses"] {
        let new = median_pmu(to, key).map(|v| v as f64);
        let old = median_pmu(from, key).map(|v| v as f64);
        map.insert(key.to_string(), json!(pct(new, old)));
    }
    map
}

pub fn fmt(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_string(), |v| format!("{v:.2}%"))
}

pub fn fmt_num(value: Option<f64>) -> String {
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

pub fn read_runs(dir: &Path, variant: &str) -> Result<Vec<Value>, String> {
    let mut path = dir.join(format!("ab-opt-{variant}.json"));
    if !path.exists() && variant.contains('+') {
        let alt_dash = dir.join(format!("ab-opt-{}.json", variant.replace('+', "-")));
        let alt_underscore = dir.join(format!("ab-opt-{}.json", variant.replace('+', "_")));
        if alt_dash.exists() {
            path = alt_dash;
        } else if alt_underscore.exists() {
            path = alt_underscore;
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CeremonyProvenance {
    pub git_sha: String,
    pub git_dirty: bool,
    pub cargo_lock_sha256: String,
    pub rustc_vv_sha256: String,
    pub engine_commit: String,
    pub run_id: String,
    pub proof_subject: String,
    pub ts: String,
    pub cpu_bench: String,
    pub isolated: String,
    pub governor: String,
    pub perf_event_paranoid_before: i32,
    pub perf_event_paranoid_after: i32,
}

impl CeremonyProvenance {
    pub fn to_json(&self) -> Value {
        json!({
            "git_sha": self.git_sha,
            "git_dirty": self.git_dirty,
            "cargo_lock_sha256": self.cargo_lock_sha256,
            "rustc_vv_sha256": self.rustc_vv_sha256,
            "engine_commit": self.engine_commit,
            "run_id": self.run_id,
            "proof_subject": self.proof_subject,
            "ts": self.ts,
            "cpu_bench": self.cpu_bench,
            "isolated": self.isolated,
            "governor": self.governor,
            "perf_event_paranoid_before": self.perf_event_paranoid_before,
            "perf_event_paranoid_after": self.perf_event_paranoid_after,
        })
    }

    pub fn from_json(val: &Value) -> Option<Self> {
        Some(Self {
            git_sha: val.get("git_sha")?.as_str()?.to_string(),
            git_dirty: val.get("git_dirty")?.as_bool()?,
            cargo_lock_sha256: val.get("cargo_lock_sha256")?.as_str()?.to_string(),
            rustc_vv_sha256: val.get("rustc_vv_sha256")?.as_str()?.to_string(),
            engine_commit: val.get("engine_commit")?.as_str()?.to_string(),
            run_id: val.get("run_id")?.as_str()?.to_string(),
            proof_subject: val.get("proof_subject")?.as_str()?.to_string(),
            ts: val.get("ts")?.as_str()?.to_string(),
            cpu_bench: val.get("cpu_bench")?.as_str()?.to_string(),
            isolated: val.get("isolated")?.as_str()?.to_string(),
            governor: val.get("governor")?.as_str()?.to_string(),
            perf_event_paranoid_before: val.get("perf_event_paranoid_before")?.as_i64()? as i32,
            perf_event_paranoid_after: val.get("perf_event_paranoid_after")?.as_i64()? as i32,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CeremonyBoltLbr {
    pub samples: u64,
    pub fdata_sha256: String,
    pub mode: String,
}

impl CeremonyBoltLbr {
    pub fn to_json(&self) -> Value {
        json!({
            "samples": self.samples,
            "fdata_sha256": self.fdata_sha256,
            "mode": self.mode,
        })
    }

    pub fn from_json(val: &Value) -> Option<Self> {
        Some(Self {
            samples: val.get("samples")?.as_u64()?,
            fdata_sha256: val.get("fdata_sha256")?.as_str()?.to_string(),
            mode: val.get("mode")?.as_str()?.to_string(),
        })
    }
}

pub fn derive_provenance(dir: &Path, bench_core: Option<&str>) -> CeremonyProvenance {
    let prov_file = dir.join("ab-opt-provenance.json");
    let cached_prov = fs::read_to_string(&prov_file)
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .and_then(|v| CeremonyProvenance::from_json(&v));
    if let Some(prov) = cached_prov {
        return prov;
    }

    let git_sha = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let git_dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    let cargo_lock_sha256 = std::process::Command::new("sha256sum")
        .arg("Cargo.lock")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.split_whitespace().next().map(|h| h.to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    let rustc_vv_sha256 = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .ok()
        .and_then(|o| {
            let mut child = std::process::Command::new("sha256sum")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .ok()?;
            use std::io::Write;
            child.stdin.take()?.write_all(&o.stdout).ok()?;
            let out = child.wait_with_output().ok()?;
            String::from_utf8(out.stdout)
                .ok()
                .and_then(|s| s.split_whitespace().next().map(|h| h.to_string()))
        })
        .unwrap_or_else(|| "unknown".to_string());

    let engine_commit = std::env::var("ENGINE_COMMIT").unwrap_or_else(|_| "unknown".to_string());
    let run_id = std::env::var("NAM_RUN_ID").unwrap_or_else(|_| "none".to_string());
    let proof_subject = "ab_opt_bench".to_string();

    let ts = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let cpu_bench = bench_core
        .map(|s| s.to_string())
        .or_else(|| std::env::var("NAM_BENCH_CORE").ok())
        .unwrap_or_else(|| "none".to_string());

    let isolated = fs::read_to_string("/sys/devices/system/cpu/isolated")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let governor = fs::read_to_string(format!(
        "/sys/devices/system/cpu/cpu{cpu_bench}/cpufreq/scaling_governor"
    ))
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| "unknown".to_string());

    let paranoid_before = fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(2);

    CeremonyProvenance {
        git_sha,
        git_dirty,
        cargo_lock_sha256,
        rustc_vv_sha256,
        engine_commit,
        run_id,
        proof_subject,
        ts,
        cpu_bench,
        isolated,
        governor,
        perf_event_paranoid_before: paranoid_before,
        perf_event_paranoid_after: paranoid_before,
    }
}

pub fn derive_bolt_lbr(dir: &Path) -> Option<CeremonyBoltLbr> {
    let bolt_lbr_file = dir.join("ab-opt-bolt-lbr.json");
    let content = fs::read_to_string(&bolt_lbr_file).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    CeremonyBoltLbr::from_json(&val)
}

#[derive(Debug, Clone)]
pub struct TopologyEval {
    pub topology: String,
    pub plain_p99: f64,
    pub plain_max: f64,
    pub plain_mean: f64,
    pub pgo_p99: f64,
    pub pgo_max: f64,
    pub pgo_mean: f64,
    pub bolt_p99: Option<f64>,
    pub bolt_max: Option<f64>,
    pub bolt_mean: Option<f64>,
    pub pgo_gain_pct_p99: f64,
    pub pgo_gain_pct_max: f64,
    pub pgo_gain_pct_mean: f64,
    pub bolt_gain_pct_p99: Option<f64>,
    pub bolt_gain_pct_max: Option<f64>,
    pub bolt_gain_pct_mean: Option<f64>,
    pub pgo_promoted: bool,
    pub bolt_promoted: bool,
}

#[derive(Debug, Clone)]
pub struct AbOptDecision {
    pub doc: Map<String, Value>,
    pub verdict: String,
    pub verdict_reason: String,
    pub rollback: String,
    pub plain_p99: f64,
    pub plain_max: f64,
    pub pgo_p99: f64,
    pub pgo_max: f64,
    pub bolt_p99: Option<f64>,
    pub bolt_max: Option<f64>,
    pub pgo_gain: Map<String, Value>,
    pub pmu_pgo_gain: Map<String, Value>,
    pub bolt_gain: Option<Map<String, Value>>,
    pub pmu_bolt_gain: Option<Map<String, Value>>,
    pub min_bolt_gain_pct: Option<f64>,
    pub min_bolt_max_gain_pct: Option<f64>,
    pub effective_reproducible_gain: bool,
    pub bolt_reproducible_gain: bool,
    pub topologies: Vec<TopologyEval>,
    pub provenance: Option<CeremonyProvenance>,
    pub bolt_lbr: Option<CeremonyBoltLbr>,
}

pub fn evaluate_ceremony(
    plain: &[Value],
    pgo: &[Value],
    bolt: &[Value],
    runs: u64,
    bolt_cause: Option<&str>,
    provenance: Option<CeremonyProvenance>,
    bolt_lbr: Option<CeremonyBoltLbr>,
) -> Result<AbOptDecision, String> {
    if plain.is_empty() || pgo.is_empty() {
        return Err("plain/pgo receipts are empty; cannot compare".to_string());
    }
    let bolt_applied = !bolt.is_empty();

    let (plain_p99, plain_max, _) = envelope(plain, "plain", runs)?;
    let (pgo_p99, pgo_max, _) = envelope(pgo, "pgo", runs)?;
    let (bolt_p99, bolt_max) = if bolt_applied {
        let env = envelope(bolt, "pgo+bolt", runs)?;
        (Some(env.0), Some(env.1))
    } else {
        (None, None)
    };

    let mut variants = Map::new();
    variants.insert("plain".to_string(), variant_doc(plain));
    variants.insert("pgo".to_string(), variant_doc(pgo));
    if bolt_applied {
        variants.insert("pgo+bolt".to_string(), variant_doc(bolt));
    }

    let pgo_gain = gains(plain, pgo);
    let pmu_pgo_gain = pmu_gains(plain, pgo);
    let bolt_gain = bolt_applied.then(|| gains(pgo, bolt));
    let pmu_bolt_gain = bolt_applied.then(|| pmu_gains(pgo, bolt));

    let plain_map = extract_metrics_map(plain, "plain")?;
    let pgo_map = extract_metrics_map(pgo, "pgo")?;
    let bolt_map = extract_metrics_map(bolt, "pgo+bolt")?;
    require_full_coverage(&plain_map, "plain", runs)?;
    require_full_coverage(&pgo_map, "pgo", runs)?;
    if bolt_applied {
        require_full_coverage(&bolt_map, "pgo+bolt", runs)?;
    }

    // Per-topology aggregation and formal promotion evaluation (1.0% margin)
    let mut topologies_doc = Map::new();
    let mut topology_evals = Vec::new();

    for topo in TOPOLOGIES {
        let pl_p99 = median_topology_num(plain, topo, "p99")
            .ok_or_else(|| format!("plain receipt is missing topology '{topo}' p99"))?;
        let pl_max = median_topology_num(plain, topo, "max")
            .ok_or_else(|| format!("plain receipt is missing topology '{topo}' max"))?;
        let pl_mean = median_topology_num(plain, topo, "mean")
            .ok_or_else(|| format!("plain receipt is missing topology '{topo}' mean"))?;

        let pg_p99 = median_topology_num(pgo, topo, "p99")
            .ok_or_else(|| format!("pgo receipt is missing topology '{topo}' p99"))?;
        let pg_max = median_topology_num(pgo, topo, "max")
            .ok_or_else(|| format!("pgo receipt is missing topology '{topo}' max"))?;
        let pg_mean = median_topology_num(pgo, topo, "mean")
            .ok_or_else(|| format!("pgo receipt is missing topology '{topo}' mean"))?;

        let bt_p99 = bolt_applied
            .then(|| median_topology_num(bolt, topo, "p99"))
            .flatten();
        let bt_max = bolt_applied
            .then(|| median_topology_num(bolt, topo, "max"))
            .flatten();
        let bt_mean = bolt_applied
            .then(|| median_topology_num(bolt, topo, "mean"))
            .flatten();

        // gain_pct = (new - old) / old * 100: negative means improvement
        // (fewer cycles). improvement_pct = (old - new) / old * 100 and must
        // reach PROMOTION_MARGIN_PCT on every topology to promote.
        let pgo_gain_pct_p99 = (pg_p99 - pl_p99) / pl_p99 * 100.0;
        let pgo_gain_pct_max = (pg_max - pl_max) / pl_max * 100.0;
        let pgo_gain_pct_mean = (pg_mean - pl_mean) / pl_mean * 100.0;

        let pgo_improvement_p99 = (pl_p99 - pg_p99) / pl_p99 * 100.0;
        let pgo_promoted = pgo_improvement_p99 >= (PROMOTION_MARGIN_PCT - MARGIN_EPSILON);

        let (bolt_gain_pct_p99, bolt_gain_pct_max, bolt_gain_pct_mean, bolt_promoted) =
            if let (Some(bp99), Some(bmax), Some(bmean)) = (bt_p99, bt_max, bt_mean) {
                let gain_p99 = (bp99 - pg_p99) / pg_p99 * 100.0;
                let gain_max = (bmax - pg_max) / pg_max * 100.0;
                let gain_mean = (bmean - pg_mean) / pg_mean * 100.0;
                let improvement_p99 = (pg_p99 - bp99) / pg_p99 * 100.0;
                let promoted = improvement_p99 >= (PROMOTION_MARGIN_PCT - MARGIN_EPSILON);
                (Some(gain_p99), Some(gain_max), Some(gain_mean), promoted)
            } else {
                (None, None, None, false)
            };

        let mut topo_entry = Map::new();
        let mut topo_vars = Map::new();
        topo_vars.insert(
            "plain".to_string(),
            json!({
                "median_p99_cycles": pl_p99.round() as u64,
                "median_max_cycles": pl_max.round() as u64,
                "median_mean_cycles": pl_mean,
            }),
        );
        topo_vars.insert(
            "pgo".to_string(),
            json!({
                "median_p99_cycles": pg_p99.round() as u64,
                "median_max_cycles": pg_max.round() as u64,
                "median_mean_cycles": pg_mean,
            }),
        );
        if bolt_applied {
            topo_vars.insert(
                "pgo+bolt".to_string(),
                json!({
                    "median_p99_cycles": bt_p99.map(|v| v.round() as u64),
                    "median_max_cycles": bt_max.map(|v| v.round() as u64),
                    "median_mean_cycles": bt_mean,
                }),
            );
        }
        topo_entry.insert("variants".to_string(), Value::Object(topo_vars));
        topo_entry.insert(
            "pgo_gain_pct".to_string(),
            json!({
                "p99": pgo_gain_pct_p99,
                "max": pgo_gain_pct_max,
                "mean": pgo_gain_pct_mean,
            }),
        );
        if bolt_applied {
            topo_entry.insert(
                "bolt_gain_pct".to_string(),
                json!({
                    "p99": bolt_gain_pct_p99,
                    "max": bolt_gain_pct_max,
                    "mean": bolt_gain_pct_mean,
                }),
            );
        }
        topo_entry.insert("pgo_promoted".to_string(), json!(pgo_promoted));
        topo_entry.insert("bolt_promoted".to_string(), json!(bolt_promoted));

        topologies_doc.insert(topo.to_string(), Value::Object(topo_entry));
        topology_evals.push(TopologyEval {
            topology: topo.to_string(),
            plain_p99: pl_p99,
            plain_max: pl_max,
            plain_mean: pl_mean,
            pgo_p99: pg_p99,
            pgo_max: pg_max,
            pgo_mean: pg_mean,
            bolt_p99: bt_p99,
            bolt_max: bt_max,
            bolt_mean: bt_mean,
            pgo_gain_pct_p99,
            pgo_gain_pct_max,
            pgo_gain_pct_mean,
            bolt_gain_pct_p99,
            bolt_gain_pct_max,
            bolt_gain_pct_mean,
            pgo_promoted,
            bolt_promoted,
        });
    }

    let all_pgo_promoted = topology_evals.iter().all(|e| e.pgo_promoted);
    let all_bolt_promoted = bolt_applied && topology_evals.iter().all(|e| e.bolt_promoted);

    // Ledger records per-run diagnostic information
    let mut ledger = Vec::new();
    let mut min_bolt_gain_pct: Option<f64> = None;
    let mut min_bolt_max_gain_pct: Option<f64> = None;

    let gain_pct = |new: f64, old: f64| -> f64 {
        if old > 0.0 {
            (old - new) / old * 100.0
        } else {
            f64::NEG_INFINITY
        }
    };

    for topo in TOPOLOGIES {
        for run_idx in 1..=runs {
            let key = (topo.to_string(), run_idx);
            let plain_m = plain_map.get(&key).copied().ok_or_else(|| {
                format!(
                    "plain receipt is missing topology '{topo}' run {run_idx} (INCONCLUSIVE/GAP)"
                )
            })?;
            let pgo_m = pgo_map.get(&key).copied().ok_or_else(|| {
                format!("pgo receipt is missing topology '{topo}' run {run_idx} (INCONCLUSIVE/GAP)")
            })?;
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
                (Some(p99_gain), Some(max_gain), promoted)
            } else {
                (None, None, false)
            };

            let effective_m = if bolt_promoted_this_run {
                bolt_m.unwrap_or(pgo_m)
            } else {
                pgo_m
            };

            let effective_gain_pct = gain_pct(effective_m.p99, plain_m.p99);
            let effective_max_gain_pct = gain_pct(effective_m.max, plain_m.max);

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

    let verdict: String;
    let verdict_reason: String;
    let rollback: String;

    if !all_pgo_promoted {
        verdict = "UNOPTIMIZED".to_string();
        verdict_reason = "pgo_margin_not_met".to_string();
        let unpromoted: Vec<String> = topology_evals
            .iter()
            .filter(|e| !e.pgo_promoted)
            .map(|e| format!("{}: p99 {:+.2}%", e.topology, e.pgo_gain_pct_p99))
            .collect();
        rollback = format!(
            "PGO did not achieve the 1.0% p99 margin across all topologies ({}); plain release binary is the verified optimal version.",
            unpromoted.join(", ")
        );
    } else if all_bolt_promoted {
        verdict = "PGO+BOLT".to_string();
        verdict_reason = "bolt_promoted".to_string();
        rollback = String::new();
    } else if bolt_applied {
        verdict = "PGO-ONLY".to_string();
        verdict_reason = "bolt_margin_not_met".to_string();
        let unpromoted: Vec<String> = topology_evals
            .iter()
            .filter(|e| !e.bolt_promoted)
            .map(|e| {
                format!(
                    "{}: p99 {:+.2}%",
                    e.topology,
                    e.bolt_gain_pct_p99.unwrap_or(0.0)
                )
            })
            .collect();
        rollback = format!(
            "BOLT did not achieve the 1.0% p99 margin across all topologies (reason: bolt_margin_not_met; non-promoted: {}); PGO-only is the explicit fallback.",
            unpromoted.join(", ")
        );
    } else {
        verdict = "PGO-ONLY".to_string();
        verdict_reason = "bolt_not_applied".to_string();
        rollback = "BOLT not applied on this host (PGO-only fallback).".to_string();
    }

    let effective_reproducible_gain = all_pgo_promoted;
    let bolt_reproducible_gain = all_bolt_promoted;

    let mut doc = Map::new();
    doc.insert("schema_version".to_string(), json!(2));
    doc.insert("tool".to_string(), json!("ab-opt-ceremony"));
    doc.insert("runs".to_string(), json!(runs));
    doc.insert("bolt_applied".to_string(), json!(bolt_applied));
    doc.insert(
        "promotion_margin_pct".to_string(),
        json!(PROMOTION_MARGIN_PCT),
    );
    doc.insert("verdict".to_string(), json!(verdict));
    doc.insert("verdict_reason".to_string(), json!(verdict_reason));
    doc.insert("topologies".to_string(), Value::Object(topologies_doc));
    doc.insert("variants".to_string(), Value::Object(variants));
    doc.insert("pgo_gain_pct".to_string(), Value::Object(pgo_gain.clone()));
    doc.insert(
        "pmu_pgo_gain_pct".to_string(),
        Value::Object(pmu_pgo_gain.clone()),
    );
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
    if bolt_cause.is_some() || !bolt_applied {
        doc.insert(
            "bolt_cause".to_string(),
            json!(bolt_cause.unwrap_or("BOLT not requested")),
        );
    }
    if let Some(ref p) = provenance {
        doc.insert("provenance".to_string(), p.to_json());
    }
    if let Some(ref l) = bolt_lbr {
        doc.insert("bolt_lbr".to_string(), l.to_json());
    }
    doc.insert("rollback".to_string(), json!(rollback));

    Ok(AbOptDecision {
        doc,
        verdict,
        verdict_reason,
        rollback,
        plain_p99,
        plain_max,
        pgo_p99,
        pgo_max,
        bolt_p99,
        bolt_max,
        pgo_gain,
        pmu_pgo_gain,
        bolt_gain,
        pmu_bolt_gain,
        min_bolt_gain_pct,
        min_bolt_max_gain_pct,
        effective_reproducible_gain,
        bolt_reproducible_gain,
        topologies: topology_evals,
        provenance,
        bolt_lbr,
    })
}

#[cfg(test)]
#[path = "ab_opt_test.rs"]
mod tests;
