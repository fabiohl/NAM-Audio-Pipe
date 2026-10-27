# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# _lib.sh — local QA helpers for NAM-Audio-Pipe. No pinned copy, no hash, no host policy.
# Source with: PHASE_TOTAL=<N>; source utils/_lib.sh (NAM_LIB_NO_CD=1 keeps the caller's cwd).
LOG_DIR=target/logs
mkdir -p "$LOG_DIR"
export NAM_RUN_ID="${NAM_RUN_ID:-$(date -u +%Y%m%dT%H%M%SZ)-$$}"
T0=$SECONDS
FAILS=()
GAPS=()
gap() { GAPS+=("$1"); echo "PHASE: $1 GAP"; }
STRICT=0

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$LIB_DIR")"
if [ "${NAM_LIB_NO_CD:-0}" != "1" ]; then
    cd "$PROJECT_DIR" || {
        echo -e "${RED}${BOLD}[FATAL]${NC} _lib.sh: failed to cd into project root: $PROJECT_DIR" >&2
        exit 1
    }
fi

warn() { echo -e "  ${YELLOW}ⓘ${NC} $*"; }
die() {
    echo -e "${RED}${BOLD}[FATAL]${NC} $*" >&2
    exit 1
}
ok() { echo -e "  ${GREEN}OK${NC} $*"; }

# Canonical phase line and portable elapsed time ($SECONDS; no date +%s%N).
PHASE_NUM=0
PHASE_START=$SECONDS
phase() {
    PHASE_NUM=$((PHASE_NUM + 1))
    PHASE_START=$SECONDS
    echo "PHASE: $1"
}
format_duration_ms() {
    local ms="${1:-0}"
    if [ "$ms" -lt 1000 ]; then
        echo "${ms}ms"
    elif [ "$ms" -lt 10000 ]; then
        printf "%d.%02ds\n" "$((ms / 1000))" "$(((ms % 1000) / 10))"
    else
        printf "%d.%ds\n" "$((ms / 1000))" "$(((ms % 1000) / 100))"
    fi
}
phase_elapsed_str() { format_duration_ms "$(((SECONDS - PHASE_START) * 1000))"; }

# ran_tests <log>: every cargo test binary ran and none reported failures.
# ANSI escapes (CARGO_TERM_COLOR=always) are stripped; grep -c uses "|| true"
# so zero matches never turn into the "0\n0" integer-comparison trap.
ran_tests() {
    local log="$1" clean n_run n_start n_res
    clean=$(sed -E 's/\x1b\[[0-9;]*[a-zA-Z]//g' "$log" 2>/dev/null || true)
    n_run=$(printf '%s\n' "$clean" | grep -c -E '^\s*(Running|Doc-tests)\b' || true)
    n_start=$(printf '%s\n' "$clean" | grep -c -E '^\s*running [0-9]+ tests?' || true)
    n_res=$(printf '%s\n' "$clean" | grep -c -E '^test result: (ok|FAILED)\.' || true)
    [ "$n_run" -gt 0 ] && [ "$n_res" -ge "$n_run" ] && [ "$n_start" -eq "$n_res" ] \
        && ! printf '%s\n' "$clean" | grep -q 'test result: FAILED' \
        && printf '%s\n' "$clean" | grep -Eq 'test result: ok\. [1-9][0-9]* passed'
}

# assert_ran_tests <log> [min_count]: fail-closed gate against empty selections,
# filter typos and fully skipped targets.
assert_ran_tests() {
    local log="$1" min="${2:-1}" total
    total=$(grep -oE '[0-9]+ (passed|measured)' "$log" 2>/dev/null \
        | awk '{s += $1} END {print s + 0}' || true)
    if [ "${total:-0}" -lt "$min" ] || ! ran_tests "$log"; then
        echo -e "${RED}${BOLD}❌ Gate failed: $log executed ${total:-0} test(s) < $min.${NC}" >&2
        return 1
    fi
    echo -e "  Gate: ${total} test(s) ≥ $min  ✓"
}

# assert_ran_target <log_file> <target_name> [min_count]
#   Verifies that a mandatory cargo test target executed at least min_count
#   tests/benchmarks and failed closed when the target was removed, renamed,
#   filtered out or fully skipped.
assert_ran_target() {
    local log="$1" target="$2" min="${3:-1}" line lineno passed measured executed
    if [ ! -f "$log" ] || [ -z "$target" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: missing log or target name ($log, '$target').${NC}" >&2
        return 1
    fi
    line=$(grep -n -m1 -F "Running $target " "$log" 2>/dev/null || true)
    if [ -z "$line" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '$target' was not executed ($log).${NC}" >&2
        return 1
    fi
    lineno="${line%%:*}"
    line=$(sed -n "$((lineno + 1)),\$p" "$log" | grep -m1 -E 'test result:' 2>/dev/null || true)
    if [ -z "$line" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '$target' has no 'test result:' summary.${NC}" >&2
        return 1
    fi
    passed=$(printf '%s\n' "$line" | grep -oE '[0-9]+ passed' | grep -oE '^[0-9]+' | head -n1 || true)
    measured=$(printf '%s\n' "$line" | grep -oE '[0-9]+ measured' | grep -oE '^[0-9]+' | head -n1 || true)
    executed=$(( ${passed:-0} + ${measured:-0} ))
    if [ "$executed" -lt "$min" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '$target' executed $executed < $min.${NC}" >&2
        return 1
    fi
    echo -e "  Gate: target '$target' executed $executed test(s) ≥ $min  ✓"
}

finish() {
    local s=PASS
    [ ${#GAPS[@]} -gt 0 ] && s=PASS_WITH_GAPS
    { [ ${#FAILS[@]} -gt 0 ] || { [ "$STRICT" = 1 ] && [ ${#GAPS[@]} -gt 0 ]; }; } && s=FAIL
    echo "RESULT: $s ($((SECONDS - T0))s) fails=${FAILS[*]:-none} gaps=${GAPS[*]:-none}"
    [ "$s" != FAIL ]
}
