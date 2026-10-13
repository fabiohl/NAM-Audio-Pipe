#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# LEGACY performance regression gate for NAM-Audio-Pipe (pre engine-gate
# adoption). Preserved verbatim for the `--legacy` rollback window: it parses a
# single `RT_METRICS rt_deadline …` line from the `rt_metrics` long-suite test
# and compares `{mean_ns,p99_ns,max_ns}` against a persisted snapshot. The
# canonical gate is now `utils/tests-performance-regression.sh`, which drives the
# engine `nam_perf_gate` over the Criterion `rt_gate` suite.
#
# Original header:
# Performance regression gate for NAM-Audio-Pipe.
# Relative performance defense comparing current runs against a persisted baseline.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/_lib.sh"

MODE="--check"
SIMULATE=0

for arg in "$@"; do
    case "$arg" in
        --bootstrap-baseline)
            MODE="--bootstrap-baseline"
            ;;
        --check)
            MODE="--check"
            ;;
        --simulate|--dry-run)
            SIMULATE=1
            ;;
        --help|-h)
            echo "Usage: $0 [--check|--bootstrap-baseline] [--simulate]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            exit 2
            ;;
    esac
done

BENCH_CORE="${NAM_BENCH_CORE:-$(pick_bench_core 2>/dev/null || echo "0")}"
TASKSET=()
if command -v taskset >/dev/null 2>&1; then
    TASKSET=(taskset -c "${BENCH_CORE}")
fi

HOSTNAME_STR=$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo "localhost")
CPU_HASH=$(grep -m1 "model name" /proc/cpuinfo 2>/dev/null | sha256sum | awk '{print substr($1,1,8)}' || echo "generic")
BASELINE_DIR=".performance-baselines"
BASELINE_FILE="${BASELINE_DIR}/${HOSTNAME_STR}-${CPU_HASH}.json"
mkdir -p "$BASELINE_DIR"

is_calibrated_environment() {
    if [ "${NAM_BENCH_FORCE_UNCALIBRATED:-0}" = "1" ]; then
        return 1
    fi
    if [ "${NAM_BENCH_FORCE_CALIBRATED:-0}" = "1" ]; then
        return 0
    fi
    local iso="/sys/devices/system/cpu/isolated"
    if [ -f "$iso" ]; then
        local raw
        raw=$(cat "$iso" 2>/dev/null | tr -d '[:space:]')
        if [ -n "$raw" ]; then
            return 0
        fi
    fi
    return 1
}

run_single_measurement() {
    if [ "$SIMULATE" = "1" ]; then
        echo "4500 5200 6800"
        return 0
    fi
    local out
    out=$("${TASKSET[@]}" cargo test --release --features testing --test rt_metrics -- --nocapture --test-threads=1 2>&1)
    local line
    line=$(echo "$out" | grep -m1 '^RT_METRICS rt_deadline ' || true)
    if [ -z "$line" ]; then
        echo "ERROR: RT_METRICS rt_deadline line not found in run output" >&2
        return 1
    fi
    local m p99 mx
    m=$(echo "$line" | sed -n 's/.*mean_ns=\([0-9.]\+\).*/\1/p')
    p99=$(echo "$line" | sed -n 's/.*p99_ns=\([0-9.]\+\).*/\1/p')
    mx=$(echo "$line" | sed -n 's/.*max_ns=\([0-9.]\+\).*/\1/p')
    echo "$m $p99 $mx"
}

if [ "$MODE" = "--bootstrap-baseline" ]; then
    echo "Bootstrapping baseline for NAM-Audio-Pipe on core ${BENCH_CORE} (5 iterations)..."
    means=()
    p99s=()
    maxs=()
    for i in {1..5}; do
        res=$(run_single_measurement)
        read -r m p mx <<< "$res"
        echo "  [Run $i/5] mean_ns=$m, p99_ns=$p, max_ns=$mx"
        means+=("$m")
        p99s+=("$p")
        maxs+=("$mx")
    done

    sorted_means=($(printf '%s\n' "${means[@]}" | sort -n))
    sorted_p99s=($(printf '%s\n' "${p99s[@]}" | sort -n))
    sorted_maxs=($(printf '%s\n' "${maxs[@]}" | sort -n))
    med_mean="${sorted_means[2]}"
    med_p99="${sorted_p99s[2]}"
    med_max="${sorted_maxs[2]}"

    cat << EOF > "$BASELINE_FILE"
{
  "hostname": "$HOSTNAME_STR",
  "cpu_model_hash": "$CPU_HASH",
  "bench_target": "rt_metrics",
  "bench_core": "$BENCH_CORE",
  "runs": 5,
  "mean_ns": $med_mean,
  "p99_ns": $med_p99,
  "max_ns": $med_max
}
EOF
    echo "✓ Baseline saved to $BASELINE_FILE: mean_ns=$med_mean, p99_ns=$med_p99, max_ns=$med_max"
    exit 0
fi

# Mode: --check
if ! is_calibrated_environment; then
    echo "TEST_RESULT[perf_regression]=GAP:uncalibrated_environment"
    echo "WARN GAP: uncalibrated environment (isolated CPU core required; exit 2)" >&2
    exit 2
fi

if [ ! -f "$BASELINE_FILE" ]; then
    echo "TEST_RESULT[perf_regression]=FAIL baseline_missing"
    echo "ERROR: baseline file $BASELINE_FILE does not exist. Run with --bootstrap-baseline first." >&2
    exit 1
fi

base_mean=$(sed -n 's/.*"mean_ns":[[:space:]]*\([0-9.]\+\).*/\1/p' "$BASELINE_FILE")
base_p99=$(sed -n 's/.*"p99_ns":[[:space:]]*\([0-9.]\+\).*/\1/p' "$BASELINE_FILE")
echo "Evaluating against baseline $BASELINE_FILE (base_mean=$base_mean ns, base_p99=$base_p99 ns)..."

regress_count=0
for i in {1..5}; do
    res=$(run_single_measurement)
    read -r m p mx <<< "$res"
    is_regressed=$(awk -v m="$m" -v bm="$base_mean" -v p="$p" -v bp="$base_p99" 'BEGIN {
        if (p > bp * 1.10 || m > bm * 1.05) print 1; else print 0;
    }')
    if [ "$is_regressed" -eq 1 ]; then
        echo "  [Run $i/5] mean_ns=$m, p99_ns=$p — REGRESSION (p99 > +10% or mean > +5%)"
        regress_count=$((regress_count + 1))
    else
        echo "  [Run $i/5] mean_ns=$m, p99_ns=$p — OK"
    fi
done

if [ "$regress_count" -ge 3 ]; then
    echo "TEST_RESULT[perf_regression]=FAIL ($regress_count/5 runs regressed)"
    exit 1
fi

echo "TEST_RESULT[perf_regression]=PASS"
exit 0
