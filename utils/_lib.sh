# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

# _lib.sh — Common bash utilities for nam-audio-pipe scripts.
#
# Source with:
#   PHASE_TOTAL=<N>; source "$(dirname "$0")/_lib.sh"
# or for scripts that manage their own working directory (e.g. build-release.sh):
#   NAM_LIB_NO_CD=1 PHASE_TOTAL=<N>; source "$(dirname "$0")/_lib.sh"
#
# Then call:
#   phase "Description of the current step"
#   die  "Fatal error message"    # prints to stderr and exits 1
#   ok   "Success message"        # prints indented green OK line
#   warn "Warning message"        # prints indented yellow notice

# ---------------------------------------------------------------------------
# ANSI style helpers
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

# ---------------------------------------------------------------------------
# Phase counter (global — not safe for concurrent subshells)
# ---------------------------------------------------------------------------
PHASE_NUM=0

# phase <description>
#   Increments and prints a phase header: [N/TOTAL] description
phase() {
    PHASE_NUM=$((PHASE_NUM + 1))
    echo -e "\n${BLUE}${BOLD}[${PHASE_NUM}/${PHASE_TOTAL:-?}]${NC} $*"
}

# die <message>
#   Prints a fatal error to stderr and exits with code 1.
die() {
    echo -e "${RED}${BOLD}[FATAL]${NC} $*" >&2
    exit 1
}

# ok <message>
#   Prints an indented green OK confirmation line.
ok() {
    echo -e "  ${GREEN}OK${NC} $*"
}

# warn <message>
#   Prints an indented yellow informational/warning line.
warn() {
    echo -e "  ${YELLOW}ⓘ${NC} $*"
}

# ---------------------------------------------------------------------------
# Resolve project root dynamically relative to this helper script.
# ---------------------------------------------------------------------------
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$LIB_DIR")"

if [ -z "$PROJECT_DIR" ]; then
    echo -e "${RED}${BOLD}[FATAL]${NC} _lib.sh: could not resolve PROJECT_DIR." >&2
    exit 1
fi

# Automatically enter the project root directory, unless the caller has set
# NAM_LIB_NO_CD=1 (used by scripts that manage their own working directory,
# such as build-release.sh which sets up its own cd before sourcing this lib).
if [ "${NAM_LIB_NO_CD:-0}" != "1" ]; then
    cd "$PROJECT_DIR" || {
        echo -e "${RED}${BOLD}[FATAL]${NC} _lib.sh: failed to cd into project root: $PROJECT_DIR" >&2
        exit 1
    }
fi

# ---------------------------------------------------------------------------
# Quick-runner gate infrastructure (typed receipt + fail-closed assertion)
# ---------------------------------------------------------------------------

# emit <receipt-line>
#   Appends one structured line to the typed quick-run receipt
#   (target/logs/quick-receipt.txt), mirroring it on stdout. Single-line
#   appends are atomic under O_APPEND.
emit() {
    mkdir -p target/logs
    printf '%s\n' "$1" | tee -a target/logs/quick-receipt.txt
}

# assert_ran_tests <log_file> [min_count]
#   Verifies that a test log proves real execution: sums the "X passed" and
#   "X measured" counters of every libtest summary line (plus a benchmark
#   fallback) and fails the gate when the total falls below min_count.
#   Fail-closed against typo'd targets, empty filters and 100% skip selection.
assert_ran_tests() {
    local log_file="$1"
    local min_count="${2:-1}"

    local total_passed=0

    local passed
    if passed=$(grep -oP 'test result: ok\.\s+\K\d+(?=\s+passed)' "$log_file" 2>/dev/null); then
        for p in $passed; do
            total_passed=$((total_passed + p))
        done
    fi

    local measured
    if measured=$(grep -oP '\K\d+(?=\s+measured)' "$log_file" 2>/dev/null); then
        for m in $measured; do
            total_passed=$((total_passed + m))
        done
    fi

    if [ "$total_passed" -eq 0 ]; then
        local bench_count
        bench_count=$(grep -cP '^\S.*time:\s+\[' "$log_file" 2>/dev/null || true)
        bench_count="${bench_count:-0}"
        total_passed=$bench_count
    fi

    if [ "$total_passed" -lt "$min_count" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: phase executed 0 tests/benchmarks (empty selection or filter mismatch).${NC}"
        return 1
    fi
    echo -e "  Gate: ${total_passed} test(s)/benchmark(s) executed ≥ ${min_count}  ✓"
    return 0
}
