#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# =============================================================================
# Performance Regression Gate — thin wrapper over the engine `nam_perf_gate`
# =============================================================================
#
# Unified performance governance (Sprint 3 / EC-03; orchestration shared since
# Sprint 9 / T9.3): this host no longer parses a single `RT_METRICS rt_deadline …`
# log line. It runs the Criterion `rt_gate` suite (benches/rt_gate.rs promotes
# the offline rt_deadline real load to per-scenario benchmarks) and delegates
# every interpretation (environment fingerprint, baseline persistence,
# per-scenario coverage and the machine regression verdict over Criterion
# `change/estimates.json`) to the engine binary `nam_perf_gate`, the single
# statistical authority across the ecosystem. The shared orchestration body lives
# in the canonical `_lib_core.sh` (`run_perf_regression_gate`), drift-pinned by
# `utils/test-lib-drift.sh`; this file only carries the repository-specific
# configuration (bench suite + expected-id manifest) and the `--legacy` delegate.
#
# Fail-closed: a missing baseline, an incomparable environment, a scenario
# without a saved series, an unexecuted expected scenario, or a blind comparison
# artifact all fail non-zero. Expected scenarios are declared in
# `benches/rt_gate.manifest` (trailing-`*` group patterns; a pattern certifies
# the group ran, not exhaustive per-id coverage).
#
# Modes
#   --check (default)     Compare the current build against the saved baseline.
#   --bootstrap-baseline  Create a new baseline + environment fingerprint
#                         (human operator only).
#   --legacy              Delegate to utils/tests-performance-regression-legacy.sh.
#
# Environment
#   NAM_PERF_GATE          Path to a prebuilt engine `nam_perf_gate` (else a
#                          PATH install, else a sibling engine checkout build).
#   NAM_ENGINE_DIR         Engine checkout used to build the gate when unset
#                          (default: ../NeuralAmpModeler-rs).
#   NAM_BENCH_CORE         CPU core pinned via taskset (default pick_bench_core).
#   NAM_BASELINE_NAME      Criterion baseline name (default ci-baseline).
#   NAM_BENCH_MANIFEST     Expected-id manifest (default benches/rt_gate.manifest).
#   NAM_THERMAL_COOLDOWN_S Cooldown before benchmarking (default 180, 0 to skip).
#
# Usage
#   utils/tests-performance-regression.sh [--check|--bootstrap-baseline|--legacy]
# =============================================================================

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/_lib.sh"

# Single source of truth for the Criterion suite name: the expected-id manifest
# default is derived from it, so the two can never drift apart.
BENCH_SUITE="rt_gate"

run_perf_regression_gate \
    "$SCRIPT_DIR" \
    "$BENCH_SUITE" \
    "$PROJECT_DIR/benches/${BENCH_SUITE}.manifest" \
    "$@"
