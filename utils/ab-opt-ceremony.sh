#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# utils/ab-opt-ceremony.sh — T5.2 A/B optimization ceremony (HUMAN OPERATOR ONLY).
#
# Builds the A/B harness (src/bin/ab_opt_bench.rs) three ways and compares their
# measured DSP hot-path envelope:
#
#   plain → PGO → PGO+BOLT
#
# Each variant is built and then run with `--runs N` (default 3; the T5.2
# acceptance requires >= 3 runs). The per-variant receipts
# (`target/logs/ab-opt-<variant>.json`) retain per-block cycle counts
# (min/mean/p50/p99/p999/max + tail latency in ns) and, when the kernel permits
# (perf_event_open), the PMU counters cycles/instructions/iTLB misses/I-cache
# misses with typed availability.
#
# Verdict (T5.2 invariant: announce an optimization only when it improves or
# preserves the measured envelope without fidelity regression):
#
#   PGO+BOLT  — BOLT improves or preserves the p99 tail-latency envelope vs PGO
#               across all runs (no regression), and PMU counters (when
#               available on this host) do not regress.
#   PGO-ONLY  — explicit T5.2 rollback: BOLT did not prove gain (regression) or
#               the BOLT toolchain/perf was unavailable. The release continues
#               PGO-only; PGO+BOLT is never announced without proof.
#
# Fidelity (NAMCore parity, f64 oracle, spectral fidelity) is NOT re-checked
# here — the operator must also run utils/quality-dashboard.sh --check before
# certifying any optimization (the A/B only proves the envelope).
#
# AI agents MUST NOT execute this script; it is part of the operator-only
# release ceremony (see .agents/TODO-sprints.md T5.2 acceptance).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"
NAM_LIB_NO_CD=1 source "$SCRIPT_DIR/_lib.sh"

RUNS=3
BLOCKS=20000
USE_PGO=true
USE_BOLT=true
AB_DIR="$PROJECT_DIR/target/logs"
AB_BUILD_DIR="$PROJECT_DIR/target/ab-opt-build"
RECEIPT="$AB_DIR/ab-opt-ceremony-receipt.json"
AB_BENCH_FLAGS=(--blocks "$BLOCKS")

show_help() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

T5.2 A/B optimization ceremony: plain → PGO → PGO+BOLT on the DSP hot-path
harness. OPERATOR ONLY (never run by AI agents).

Options:
  --runs N         Number of measured runs per variant (default 3, min 3).
  --blocks N       DSP blocks per measured run (default 20000).
  --no-pgo         Skip PGO and BOLT; compare PLAIN only (diagnostic).
  --no-bolt        Build PGO but skip BOLT (compare plain → PGO only).
  --bench-extra 'ARGS'  Extra flags forwarded to ab_opt_bench (e.g.
                   '--quantum 256 --oversample 4x').
  -h, --help       Show this help.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        --blocks) BLOCKS="$2"; shift 2 ;;
        --no-pgo) USE_PGO=false; USE_BOLT=false; shift ;;
        --no-bolt) USE_BOLT=false; shift ;;
        --bench-extra)
            IFS=' ' read -r -a EXTRA <<< "$2"
            AB_BENCH_FLAGS+=("${EXTRA[@]}")
            shift 2
            ;;
        -h|--help) show_help; exit 0 ;;
        *) die "Unknown option: $1 (see --help)" ;;
    esac
done

if [ "$RUNS" -lt 3 ]; then
    die "T5.2 acceptance requires >= 3 runs per variant (got --runs $RUNS)."
fi

echo -e "${BLUE}${BOLD}========================================================================${NC}"
echo -e "${BLUE}${BOLD}   T5.2 A/B Optimization Ceremony (plain → PGO → PGO+BOLT)              ${NC}"
echo -e "${BLUE}${BOLD}========================================================================${NC}"
echo -e "  runs=${RUNS} blocks=${BLOCKS} bench-flags=${AB_BENCH_FLAGS[*]:-}"

# ── Toolchain discovery (mirrors build-release.sh) ───────────────────────────
REQUIRED_CMDS=(rustc cargo python3)
for cmd in "${REQUIRED_CMDS[@]}"; do
    command -v "$cmd" >/dev/null 2>&1 || die "'$cmd' not found in PATH."
done

CONFIG_RUSTFLAGS=$(python3 -c '
import sys
try:
    import tomllib
    with open(".cargo/config.toml", "rb") as f:
        flags = tomllib.load(f).get("build", {}).get("rustflags", [])
    if flags:
        print(" ".join(flags))
        sys.exit(0)
except Exception:
    pass
import re
try:
    with open(".cargo/config.toml", "r") as f:
        content = f.read()
    m = re.search(r"rustflags\s*=\s*\[(.*?)\n\]", content, re.DOTALL)
    if m:
        flags = []
        for line in m.group(1).splitlines():
            s = line.strip()
            if not s or s.startswith("#"):
                continue
            fm = re.search(r"\"([^\"]+)\"", s)
            if fm:
                flags.append(fm.group(1))
        print(" ".join(flags))
except Exception:
    pass
' 2>/dev/null || echo "")
[ -n "$CONFIG_RUSTFLAGS" ] || die "Could not extract rustflags from .cargo/config.toml."

LLVM_PROFDATA=""
if [ "$USE_PGO" = true ]; then
    RUST_SYSROOT="$(rustc --print sysroot)"
    RUST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
    LLVM_PROFDATA="$RUST_SYSROOT/lib/rustlib/$RUST_TARGET/bin/llvm-profdata"
    [ -x "$LLVM_PROFDATA" ] || die "llvm-profdata not found at $LLVM_PROFDATA (rustup component add llvm-tools-preview)."
fi

LLVM_BOLT=""
HAS_PERF=false
if [ "$USE_BOLT" = true ]; then
    for candidate in \
        /usr/lib/llvm-22/bin/llvm-bolt /usr/lib/llvm-21/bin/llvm-bolt \
        /usr/lib/llvm-20/bin/llvm-bolt /usr/lib/llvm-19/bin/llvm-bolt \
        /usr/lib/llvm-18/bin/llvm-bolt /usr/bin/llvm-bolt-22 /usr/bin/llvm-bolt-21 \
        /usr/bin/llvm-bolt; do
        if [ -x "$candidate" ]; then
            LLVM_BOLT="$candidate"
            break
        fi
    done
    if [ -n "$LLVM_BOLT" ]; then
        PERF2BOLT="$(dirname "$LLVM_BOLT")/perf2bolt"
        [ -x "$PERF2BOLT" ] || PERF2BOLT="perf2bolt"
    else
        warn "llvm-bolt not found; the ceremony will compare PLAIN → PGO only (BOLT_UNAVAILABLE)."
    fi
    if command -v perf >/dev/null 2>&1; then
        PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")
        if [ "$PARANOID" -le 1 ]; then
            HAS_PERF=true
        else
            warn "kernel.perf_event_paranoid=$PARANOID (>1): perf record for BOLT unavailable. Set it to 1 with: sudo sysctl -w kernel.perf_event_paranoid=1"
        fi
    else
        warn "perf not found; BOLT profiling unavailable."
    fi
fi

mkdir -p "$AB_DIR"
rm -rf "$AB_BUILD_DIR"
mkdir -p "$AB_BUILD_DIR"
export CARGO_TARGET_DIR="$AB_BUILD_DIR"

# ── Build + run one variant ──────────────────────────────────────────────────
# build_and_run <name> <rustflags> [<bin>]
#   Compiles ab_opt_bench with the given RUSTFLAGS (or reuses the passed binary
#   for the post-BOLT artifact) and runs `RUNS` measured passes, writing
#   target/logs/ab-opt-<name>.json.
build_and_run() {
    local name="$1" flags="$2" bin="${3:-}"
    if [ -z "$bin" ]; then
        eprintln_banner "building variant '$name'"
        RUSTFLAGS="$flags" cargo build --locked --release --features testing --bin ab_opt_bench
        bin="$AB_BUILD_DIR/release/ab_opt_bench"
    fi
    [ -x "$bin" ] || die "variant '$name' binary not found: $bin"
    eprintln_banner "running variant '$name' (runs=$RUNS)"
    "$bin" --variant "$name" --runs "$RUNS" "${AB_BENCH_FLAGS[@]}" \
        --receipt "$AB_DIR/ab-opt-$name.json"
}

eprintln_banner() { echo -e "  ${CYAN}$*${NC}"; }

# ── PLAIN ────────────────────────────────────────────────────────────────────
build_and_run plain "$CONFIG_RUSTFLAGS"

# ── PGO ──────────────────────────────────────────────────────────────────────
PGO_DIR="$AB_BUILD_DIR/pgo"
mkdir -p "$PGO_DIR/profraw"
if [ "$USE_PGO" = true ]; then
    # Build the profile-generate binary (no run), then run it once with
    # LLVM_PROFILE_FILE pointed at the profraw dir so no stray .profraw lands
    # in the repo root.
    echo -e "  ${CYAN}building variant 'pgo-gen'${NC}"
    RUSTFLAGS="$CONFIG_RUSTFLAGS -Cprofile-generate=$PGO_DIR/profraw" \
        cargo build --locked --release --features testing --bin ab_opt_bench
    LLVM_PROFILE_FILE="$PGO_DIR/profraw/default_%m_%p.profraw" \
        "$AB_BUILD_DIR/release/ab_opt_bench" --variant pgo-gen --runs 1 \
        --blocks "$BLOCKS" --receipt "$AB_DIR/ab-opt-pgo-gen.json"
    PROFRAW_COUNT=$(find "$PGO_DIR/profraw" -name "*.profraw" 2>/dev/null | wc -l)
    [ "$PROFRAW_COUNT" -gt 0 ] || die "PGO: no .profraw generated."
    "$LLVM_PROFDATA" merge -sparse -o "$PGO_DIR/merged.profdata" "$PGO_DIR/profraw"/*.profraw
    build_and_run pgo "$CONFIG_RUSTFLAGS -Cprofile-use=$PGO_DIR/merged.profdata"
else
    warn "--no-pgo: comparing PLAIN only."
fi

# ── PGO+BOLT ─────────────────────────────────────────────────────────────────
BOLT_APPLIED=false
BOLT_CAUSE=""
if [ "$USE_BOLT" = true ] && [ -n "$LLVM_BOLT" ] && [ "$HAS_PERF" = true ] && [ "$USE_PGO" = true ]; then
    echo -e "\n${BLUE}${BOLD}Building & running variant 'pgo+bolt'...${NC}"
    # The bench is offline DSP (no PipeWire needed): perf the hot path directly.
    rm -f "$PGO_DIR/perf.data"
    if perf record -F 999 -e cycles:u -o "$PGO_DIR/perf.data" -- \
        "$AB_BUILD_DIR/release/ab_opt_bench" --variant pgo-gen --runs 1 \
        --blocks "$BLOCKS" --receipt "$AB_DIR/ab-opt-bolt-probe.json" >/dev/null 2>&1; then
        :
    else
        BOLT_CAUSE="BOLT_FAILED: perf record session failed."
    fi

    if [ -z "$BOLT_CAUSE" ] && [ ! -s "$PGO_DIR/perf.data" ]; then
        BOLT_CAUSE="BOLT_FAILED: perf.data empty or missing."
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        if "$PERF2BOLT" -p "$PGO_DIR/perf.data" "$AB_BUILD_DIR/release/ab_opt_bench" \
            -o "$PGO_DIR/perf.fdata" > "$PGO_DIR/perf2bolt.log" 2>&1; then
            :
        else
            BOLT_CAUSE="BOLT_FAILED: perf2bolt conversion failed: $(tail -n1 "$PGO_DIR/perf2bolt.log" 2>/dev/null || true)"
        fi
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        if "$LLVM_BOLT" "$AB_BUILD_DIR/release/ab_opt_bench" \
            -o "$AB_BUILD_DIR/release/ab_opt_bench.bolt" \
            -data "$PGO_DIR/perf.fdata" \
            --reorder-blocks=ext-tsp --reorder-functions=hfsort --split-functions \
            --split-all-cold -hugify --relocs --lite > "$PGO_DIR/llvm-bolt.log" 2>&1; then
            BOLT_APPLIED=true
        else
            BOLT_CAUSE="BOLT_FAILED: llvm-bolt failed: $(tail -n1 "$PGO_DIR/llvm-bolt.log" 2>/dev/null || true)"
        fi
    fi

    if [ "$BOLT_APPLIED" = true ]; then
        build_and_run pgo+bolt "$CONFIG_RUSTFLAGS" "$AB_BUILD_DIR/release/ab_opt_bench.bolt"
    else
        warn "BOLT not applied (${BOLT_CAUSE}); the ceremony will compare PLAIN → PGO only."
    fi
elif [ "$USE_BOLT" = true ]; then
    BOLT_CAUSE="BOLT_UNAVAILABLE: $([ -z "$LLVM_BOLT" ] && echo llvm-bolt-not-found || ([ "$HAS_PERF" != true ] && echo perf-unavailable || echo pgo-disabled))"
    warn "BOLT skipped: ${BOLT_CAUSE}"
fi

# ── Compare receipts & emit verdict ──────────────────────────────────────────
if [ "$USE_PGO" = true ]; then
    python3 - "$AB_DIR" "$RECEIPT" "$RUNS" "$BOLT_APPLIED" "$BOLT_CAUSE" <<'PY'
import json
import os
import sys

ab_dir, receipt_path, runs, bolt_applied, bolt_cause = sys.argv[1:6]
bolt_applied = bolt_applied == "True"

def read_variant(name):
    path = os.path.join(ab_dir, f"ab-opt-{name}.json")
    runs_data = []
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            if not line.strip():
                continue
            obj = json.loads(line)
            if obj.get("event") == "run":
                runs_data.append(obj)
    return runs_data

variants = {"plain": read_variant("plain"), "pgo": read_variant("pgo")}
if bolt_applied:
    variants["pgo+bolt"] = read_variant("pgo+bolt")

def median_of(runs_data, key):
    if not runs_data:
        return None
    vals = sorted(r[key] for r in runs_data)
    return vals[len(vals) // 2]

def median_cycles(runs_data, field):
    if not runs_data:
        return None
    vals = sorted(r["cycles"][field] for r in runs_data)
    return vals[len(vals) // 2]

def median_pmu(runs_data, key):
    avail = [r for r in runs_data if r.get("pmu_availability", {}).get(key) == "ok"]
    if not avail or len(avail) < len(runs_data):
        return None  # not available on every run → cannot attribute
    vals = sorted(r["pmu"][key] for r in runs_data)
    return vals[len(vals) // 2]

def pct(new, old):
    if not old:
        return None
    return (new - old) / old * 100.0

def fmt(v):
    return f"{v:.2f}%" if v is not None else "n/a"

def gain(from_name, to_name, field):
    a = median_cycles(variants[from_name], field)
    b = median_cycles(variants[to_name], field)
    return pct(b, a)

def pmu_gain(from_name, to_name, key):
    a = median_pmu(variants[from_name], key)
    b = median_pmu(variants[to_name], key)
    return pct(b, a)

if not variants["plain"] or not variants["pgo"]:
    print("[FATAL] plain/pgo receipts are empty; cannot compare.", file=sys.stderr)
    sys.exit(1)

doc = {
    "schema_version": 1,
    "tool": "ab-opt-ceremony.sh",
    "task": "T5.2",
    "runs": int(runs),
    "variants": {
        name: {
            "median_p99_cycles": median_cycles(data, "p99"),
            "median_max_cycles": median_cycles(data, "max"),
            "median_mean_cycles": median_cycles(data, "mean"),
        }
        for name, data in variants.items()
    },
    "pgo_gain_pct": {
        "p99": gain("plain", "pgo", "p99"),
        "max": gain("plain", "pgo", "max"),
        "mean": gain("plain", "pgo", "mean"),
    },
    "pmu_pgo_gain_pct": {
        k: pmu_gain("plain", "pgo", k)
        for k in ("cycles", "instructions", "itlb_misses", "icache_misses")
    },
}

# Verdict: invariant = an optimization is announced only when it improves or
# preserves the envelope without regression.
if bolt_applied:
    doc["bolt_gain_pct"] = {
        "p99": gain("pgo", "pgo+bolt", "p99"),
        "max": gain("pgo", "pgo+bolt", "max"),
        "mean": gain("pgo", "pgo+bolt", "mean"),
    }
    doc["pmu_bolt_gain_pct"] = {
        k: pmu_gain("pgo", "pgo+bolt", k)
        for k in ("cycles", "instructions", "itlb_misses", "icache_misses")
    }

    plain_p99 = median_cycles(variants["plain"], "p99")
    pgo_p99 = median_cycles(variants["pgo"], "p99")
    bolt_p99 = median_cycles(variants["pgo+bolt"], "p99")
    pgo_regression = pgo_p99 is not None and plain_p99 is not None and pgo_p99 > plain_p99
    bolt_regression = bolt_p99 is not None and pgo_p99 is not None and bolt_p99 > pgo_p99
    if bolt_regression:
        doc["verdict"] = "PGO-ONLY"
        doc["rollback"] = "T5.2: BOLT regressed the p99 tail-latency envelope vs PGO; PGO-only is the explicit fallback."
    elif pgo_regression and bolt_p99 is not None and plain_p99 is not None and bolt_p99 > plain_p99:
        doc["verdict"] = "PGO-ONLY"
        doc["rollback"] = "T5.2: neither PGO nor BOLT improved the plain envelope (both regressed p99)."
    else:
        doc["verdict"] = "PGO+BOLT"
        doc["rollback"] = ""
else:
    plain_p99 = median_cycles(variants["plain"], "p99")
    pgo_p99 = median_cycles(variants["pgo"], "p99")
    if pgo_p99 is not None and plain_p99 is not None and pgo_p99 > plain_p99:
        doc["verdict"] = "UNOPTIMIZED"
        doc["rollback"] = "T5.2: PGO regressed the envelope vs PLAIN; do not announce any optimization."
    else:
        doc["verdict"] = "PGO-ONLY"
        doc["rollback"] = "BOLT not proven/applied on this host (PGO-only fallback)."
    doc["bolt_cause"] = bolt_cause or "BOLT not requested"

with open(receipt_path, "w", encoding="utf-8") as f:
    json.dump(doc, f, indent=2, sort_keys=True)
    f.write("\n")

verdict = doc["verdict"]
print()
print(f"VERDICT: {verdict}")
if doc.get("rollback"):
    print(f"  {doc['rollback']}")
print()
if bolt_applied:
    print(
        f"  // Medido: BOLT ganho p99={fmt(doc['bolt_gain_pct']['p99'])} "
        f"(PGO p99={median_cycles(variants['pgo'], 'p99')} → BOLT p99={median_cycles(variants['pgo+bolt'], 'p99')} cyc), "
        f"max={fmt(doc['bolt_gain_pct']['max'])}, mean={fmt(doc['bolt_gain_pct']['mean'])}"
    )
    print(
        "  // Medido: BOLT iTLB=" + fmt(doc["pmu_bolt_gain_pct"]["itlb_misses"])
        + ", I-cache=" + fmt(doc["pmu_bolt_gain_pct"]["icache_misses"])
    )
    print(
        "  // Medido: PGO ganho p99=" + fmt(doc["pgo_gain_pct"]["p99"])
        + ", max=" + fmt(doc["pgo_gain_pct"]["max"]) + ", mean=" + fmt(doc["pgo_gain_pct"]["mean"])
    )
else:
    print(
        "  // Medido: PGO ganho p99=" + fmt(doc["pgo_gain_pct"]["p99"])
        + ", max=" + fmt(doc["pgo_gain_pct"]["max"]) + ", mean=" + fmt(doc["pgo_gain_pct"]["mean"])
    )
print()
print(f"  Receipt: {receipt_path}")
sys.exit(0 if verdict in ("PGO+BOLT", "PGO-ONLY") else 1)
PY
else
    echo -e "\n${YELLOW}PLAIN-only diagnostic run; no PGO/BOLT verdict.${NC}"
fi
