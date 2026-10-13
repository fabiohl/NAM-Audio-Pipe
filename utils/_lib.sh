# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

# _lib.sh — Common bash utilities for nam-audio-pipe scripts.
#
# Source with:
#   PHASE_TOTAL=<N>; source "$(dirname "$0")/_lib.sh"
# or for scripts that manage their own working directory (e.g. build-release.sh):
#   NAM_LIB_NO_CD=1 PHASE_TOTAL=<N>; source "$(dirname "$0")/_lib.sh"
#
# The project-neutral helpers (ANSI colors, phase/ok/warn/die,
# format_duration_ms/phase_elapsed_str, emit/emit_to, assert_ran_tests/
# assert_subphase_ran, pick_bench_core) live in the canonical, byte-identical
# `_lib_core.sh` block (see utils/test-lib-drift.sh). This file adds only the
# repository-specific glue on top.
#
# Then call:
#   phase "Description of the current step"
#   die  "Fatal error message"    # prints to stderr and exits 1
#   ok   "Success message"        # prints indented green OK line
#   warn "Warning message"        # prints indented yellow notice

# Canonical project-neutral core (drift-pinned; see utils/test-lib-drift.sh).
source "$(dirname "${BASH_SOURCE[0]}")/_lib_core.sh"

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
# Mandatory test-target execution gate
# ---------------------------------------------------------------------------

# assert_ran_target <log_file> <target_name> [min_count]
#   Verifies that a specific mandatory test target was actually executed in the
#   log and that its nominal executed-test count (passed + measured) reached
#   min_count. <target_name> is the text after the "Running " banner produced
#   by cargo test, e.g. "unittests src/lib.rs", "unittests src/main.rs" or
#   "tests/service_resilience.rs".
#   Fail-closed against removed/renamed targets (no "Running ..." section),
#   empty filters and full skips (executed count below min_count).
assert_ran_target() {
    local log_file="$1"
    local target_name="$2"
    local min_count="${3:-1}"

    if [ ! -f "$log_file" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: log file not found: $log_file${NC}"
        return 1
    fi
    if [ -z "$target_name" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: assert_ran_target called without a target name.${NC}"
        return 1
    fi

    local run_line run_lineno result_line passed measured executed
    run_line=$(grep -n -m1 -F "Running ${target_name} " "$log_file" 2>/dev/null || true)
    if [ -z "$run_line" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '${target_name}' was not executed (no 'Running ...' section in $log_file). Target removed, renamed or filtered out?${NC}"
        return 1
    fi
    run_lineno="${run_line%%:*}"

    result_line=$(sed -n "$((run_lineno + 1)),\$p" "$log_file" | grep -m1 -E 'test result:' 2>/dev/null || true)
    if [ -z "$result_line" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '${target_name}' has no 'test result:' summary in $log_file.${NC}"
        return 1
    fi

    passed=$(printf '%s\n' "$result_line" | grep -oE '[0-9]+ passed' | grep -oE '^[0-9]+' | head -n1 || true)
    passed="${passed:-0}"
    measured=$(printf '%s\n' "$result_line" | grep -oE '[0-9]+ measured' | grep -oE '^[0-9]+' | head -n1 || true)
    measured="${measured:-0}"
    executed=$((passed + measured))

    if [ "$executed" -lt "$min_count" ]; then
        echo -e "${RED}${BOLD}❌ Gate failed: target '${target_name}' executed $executed test(s)/benchmark(s) < $min_count (empty filter or full skip?).${NC}"
        return 1
    fi
    echo -e "  Gate: target '${target_name}' executed $executed test(s)/benchmark(s) ≥ $min_count  ✓"
    return 0
}
