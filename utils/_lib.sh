# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# utils/_lib.sh — helpers locais de NAM-Audio-Pipe (fases, gates de teste, RESULT). Sem cor, sem cópia pinada.
# Orçamento: 75
LOG_DIR=target/logs; mkdir -p "$LOG_DIR"
export NAM_RUN_ID="${NAM_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
[ "$(cat "$LOG_DIR/.summary-run" 2>/dev/null)" = "$NAM_RUN_ID" ] || { : > "$LOG_DIR/summary.txt"; printf '%s\n' "$NAM_RUN_ID" > "$LOG_DIR/.summary-run"; }
T0=$SECONDS; FAILS=(); GAPS=(); STRICT=0

warn()  { echo "WARN: $*" >&2; }
die()   { echo "ERROR: $*" >&2; exit 1; }
ok()    { echo "OK: $*"; }
emit()  { echo "$*"; printf '%s\n' "$*" >> "$LOG_DIR/summary.txt"; }
phase() { emit "PHASE: $1"; }
gap()   { GAPS+=("$1"); emit "PHASE: $1 GAP"; }

# ran_tests <log>: every cargo test binary ran and none reported failures.
ran_tests() {
    local log="$1" clean n_run n_start n_res n_fail n_pass
    clean=$(sed -E 's/\x1b\[[0-9;]*[a-zA-Z]//g' "$log" 2>/dev/null || true)
    n_run=$(grep -c -E '^\s*(Running|Doc-tests)\b' <<<"$clean" || true)
    n_start=$(grep -c -E '^\s*running [0-9]+ tests?' <<<"$clean" || true)
    n_res=$(grep -c -E '^test result: (ok|FAILED)\.' <<<"$clean" || true)
    n_fail=$(grep -c 'test result: FAILED' <<<"$clean" || true)
    n_pass=$(grep -c -E 'test result: ok\. [1-9][0-9]* passed' <<<"$clean" || true)
    [ "$n_run" -gt 0 ] && [ "$n_res" -ge "$n_run" ] && [ "$n_start" -eq "$n_res" ] \
        && [ "$n_fail" -eq 0 ] && [ "$n_pass" -gt 0 ]
}

# assert_ran_tests <log> [min]: fail-closed gate against empty selections.
assert_ran_tests() {
    local log="$1" min="${2:-1}" total
    total=$(grep -oE '[0-9]+ (passed|measured)' "$log" 2>/dev/null | awk '{s += $1} END {print s + 0}' || true)
    if [ "${total:-0}" -lt "$min" ] || ! ran_tests "$log"; then
        echo "GATE FAIL: $log executed ${total:-0} test(s) < $min." >&2
        return 1
    fi
    echo "  Gate: ${total} test(s) >= $min"
}

# assert_ran_target <log> <target> [min]: a mandatory cargo test target executed
# at least min tests (fails closed when renamed, filtered out or fully skipped).
assert_ran_target() {
    local log="$1" target="$2" min="${3:-1}" line lineno passed measured executed
    if [ ! -f "$log" ] || [ -z "$target" ]; then
        echo "GATE FAIL: missing log or target name ($log, '$target')." >&2
        return 1
    fi
    line=$(grep -n -m1 -F "Running $target " "$log" 2>/dev/null || true)
    if [ -z "$line" ]; then
        echo "GATE FAIL: target '$target' was not executed ($log)." >&2
        return 1
    fi
    lineno="${line%%:*}"
    line=$(sed -n "$((lineno + 1)),\$p" "$log" | grep -m1 -E 'test result:' 2>/dev/null || true)
    passed=$(printf '%s\n' "$line" | grep -oE '[0-9]+ passed' | grep -oE '^[0-9]+' | head -n1 || true)
    measured=$(printf '%s\n' "$line" | grep -oE '[0-9]+ measured' | grep -oE '^[0-9]+' | head -n1 || true)
    executed=$(( ${passed:-0} + ${measured:-0} ))
    if [ "$executed" -lt "$min" ]; then
        echo "GATE FAIL: target '$target' executed $executed < $min." >&2
        return 1
    fi
    echo "  Gate: target '$target' executed $executed test(s) >= $min"
}

finish() {
    local s=PASS
    [ ${#GAPS[@]} -gt 0 ] && s=PASS_WITH_GAPS
    { [ ${#FAILS[@]} -gt 0 ] || { [ "$STRICT" = 1 ] && [ ${#GAPS[@]} -gt 0 ]; }; } && s=FAIL
    emit "RESULT: $s ($((SECONDS - T0))s) fails=${FAILS[*]:-none} gaps=${GAPS[*]:-none}"
    [ "$s" != FAIL ]
}
