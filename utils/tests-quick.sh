#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# tests-quick.sh — quick test discipline: cargo test in dev, skipping #[ignore]. Classe A.
# Uso: utils/tests-quick.sh [-h|--help]
# Orçamento: 50   # Margem segura para formatação e captura de sumários
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/tests-quick.sh"; exit 0 ;; *) usage_fail "tests-quick.sh aceita apenas -h/--help" ;; esac
lib_init
SCOPE=quick
PHASE_NAME=init; CUR_LOG="$LOG_DIR/quick-init.log"
trap 'trap_fail $?' ERR

capture_fail() {
  local log=$1 excerpt="${1%.log}-fail.log"
  grep -C 3 -E '(panicked at|FAILED)' "$log" | head -n 240 > "$excerpt" 2>/dev/null || true
  if [ -s "$excerpt" ]; then sed -n '1,50p' "$excerpt"
  else rm -f "$excerpt"; echo "--- Cargo/build failure (tail of $log) ---" >&2; tail -n 25 "$log" >&2; fi
}

phase tests "$LOG_DIR/quick-tests.log"
rc_tests=0
cargo test --locked --features testing --no-fail-fast > "$CUR_LOG" 2>&1 || rc_tests=$?
[ "$rc_tests" -eq 0 ] || capture_fail "$CUR_LOG"
summary=$(awk '/test result: / { p+=$4; f+=$6; i+=$8 } END { printf "passed=%d failed=%d ignored=%d", p, f, i }' "$CUR_LOG"); echo "Tests summary: $summary"; emit "PHASE: tests-summary $summary"
[ "$rc_tests" -eq 0 ] && ran_tests "$CUR_LOG" || { FAILS+=(tests); emit "PHASE: tests FAIL rc=$rc_tests log=$CUR_LOG"; finish || exit 1; exit 1; }
emit "PHASE: tests PASS log=$CUR_LOG"

phase tests-mono "$LOG_DIR/quick-tests-mono.log"
rc_mono=0
cargo test --locked --no-default-features --features testing --no-fail-fast > "$CUR_LOG" 2>&1 || rc_mono=$?
[ "$rc_mono" -eq 0 ] || capture_fail "$CUR_LOG"
summary=$(awk '/test result: / { p+=$4; f+=$6; i+=$8 } END { printf "passed=%d failed=%d ignored=%d", p, f, i }' "$CUR_LOG"); echo "Mono tests summary: $summary"; emit "PHASE: tests-mono-summary $summary"
[ "$rc_mono" -eq 0 ] && ran_tests "$CUR_LOG" || { FAILS+=(tests-mono); emit "PHASE: tests-mono FAIL rc=$rc_mono log=$CUR_LOG"; finish || exit 1; exit 1; }
emit "PHASE: tests-mono PASS log=$CUR_LOG"

finish || exit 1
