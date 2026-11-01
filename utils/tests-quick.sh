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

case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/tests-quick.sh"; exit 0 ;; *) usage_fail "tests-quick.sh aceita apenas -h/--help" ;; esac
lib_init
trap 'trap_fail $?' ERR

PHASE_NAME=tests; CUR_LOG="$LOG_DIR/quick-tests.log"; phase tests
rc_tests=0
cargo test --locked --features testing --no-fail-fast > "$CUR_LOG" 2>&1 || rc_tests=$?
if [ "$rc_tests" -ne 0 ]; then
  grep -C 3 -E '(panicked at|FAILED)' "$CUR_LOG" | head -n 50 || true
  grep -q -E '(panicked at|FAILED)' "$CUR_LOG" || { echo "--- Cargo/build failure (tail of $CUR_LOG) ---" >&2; tail -n 25 "$CUR_LOG" >&2; }
fi
awk '/test result: / { p+=$4; f+=$6; i+=$8 } END { printf "Tests summary: %d passed, %d failed, %d ignored\n", p, f, i }' "$CUR_LOG"
[ "$rc_tests" -eq 0 ] && ran_tests "$CUR_LOG" || { FAILS+=(tests); emit "PHASE: tests FAIL rc=$rc_tests log=$CUR_LOG"; finish || exit 1; exit 1; }
emit "PHASE: tests PASS log=$CUR_LOG"

finish || exit 1
