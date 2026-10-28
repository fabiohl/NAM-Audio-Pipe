#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# tests-quick.sh — quick test discipline: cargo test in dev, skipping #[ignore]. Classe A.
# Uso: utils/tests-quick.sh [-h|--help]
# Orçamento: 40
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/tests-quick.sh"; exit 0 ;; *) echo "RESULT: FAIL (uso)"; exit 2 ;; esac
trap 'emit "RESULT: FAIL ($((SECONDS - T0))s) fails=${PHASE_NAME:-step} gaps=${GAPS[*]:-none}"' ERR

PHASE_NAME=tests; phase tests
log_tests="$LOG_DIR/quick-tests.log"; rc_tests=0
cargo test --locked --features testing --no-fail-fast > "$log_tests" 2>&1 || rc_tests=$?
if [ "$rc_tests" -ne 0 ]; then
  grep -C 3 -E '(panicked at|FAILED)' "$log_tests" | head -n 50 || true
fi
awk '/test result: / { p+=$4; f+=$6; i+=$8 } END { printf "Tests summary: %d passed, %d failed, %d ignored\n", p, f, i }' "$log_tests"
if [ "$rc_tests" -eq 0 ] && ran_tests "$log_tests"; then
  emit "PHASE: tests PASS log=$log_tests"
else
  FAILS+=("tests"); emit "PHASE: tests FAIL rc=$rc_tests log=$log_tests"; exit 1
fi

finish
