#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Quick QA Suite for NAM-Audio-Pipe — agile first line of defense.
#
# Division of responsibility among QA scripts:
#   * utils/lints.sh          — Static quality gate (fmt, SPDX, cargo check, clippy).
#   * utils/tests-quick.sh    — THIS script. Agile test suite (cargo test).
#   * utils/run-standalone.sh — Manual testing for standalone binary.
#
# NAM-Audio-Pipe is a binary crate with inline tests in src/ and integration tests in tests/.
#
# Phases:
#   1. Structural (debug)   — unit + integration tests with debug assertions ON.
#   2. Release verification — tests production release codegen path.
#   3. PipeWire Live        — live end-to-end integration (requires running daemon).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

PHASE_TOTAL=3
source "$SCRIPT_DIR/_lib.sh"

# Re-execute with low CPU and I/O priority to prevent overloading the system.
if [ "${NAM_LOW_PRIORITY:-0}" != "1" ] && [ "${NAM_NO_LOW_PRIORITY:-0}" != "1" ]; then
    export NAM_LOW_PRIORITY=1
    CMD_PREFIX=""
    if command -v nice > /dev/null 2>&1; then
        CMD_PREFIX="nice -n 19"
    fi
    if command -v ionice > /dev/null 2>&1; then
        CMD_PREFIX="$CMD_PREFIX ionice -c 3"
    fi
    if [ -n "$CMD_PREFIX" ]; then
        warn "Restarting script with low priority (CPU/IO) to prevent system overload..."
        exec $CMD_PREFIX "$SCRIPT_PATH" "$@"
    fi
fi

trap 'echo -e "\n${RED}${BOLD}❌ Unexpected error: Command \"$BASH_COMMAND\" failed at line $LINENO with status $?. Aborting test suite.${NC}"; exit 1' ERR

echo -e "${BLUE}${BOLD}===============================${NC}"
echo -e "${BLUE}${BOLD} NAM-Audio-Pipe Test Suite     ${NC}"
echo -e "${BLUE}${BOLD}===============================${NC}"

# ── Phase 1: Structural unit & integration tests (debug) ─────────────────────
phase "Structural: unit & integration tests (debug)..."
# Use --bin nam-audio-pipe instead of --bins to avoid accidentally triggering
# pgo_workload (a profiling-only binary) in the standard test path. The
# pgo_workload binary is exercised exclusively by build-release.sh.
timeout 300 cargo test --features testing \
    --lib \
    --bin nam-audio-pipe \
    --test recording \
    --test e2e_cli \
    -- --skip ignored

# ── Phase 2: Release verification (release) ─────────────────────────────────
phase "Release verification: unit & integration tests (release)..."
timeout 300 cargo test --features testing \
    --lib \
    --bin nam-audio-pipe \
    --test recording \
    --test e2e_cli \
    --release \
    -- --skip ignored --test-threads=1 --nocapture

# ── Phase 3: PipeWire Live Integration (release, daemon probe) ───────────────
phase "PipeWire Live Integration (release)..."
echo -e "  Checking PipeWire daemon..."
if timeout 5 pw-cli info 0 > /dev/null 2>&1; then
    echo -e "  ${GREEN}PipeWire detected.${NC} Executing live integration test..."
    timeout 60 cargo test --features testing --release \
        --test pw_integration \
        -- --ignored --test-threads=1 --nocapture
else
    warn "PipeWire unavailable (pw-cli info 0 timed out or failed). Skipping live integration test."
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo -e "${GREEN}${BOLD}=========================================${NC}"
echo -e "${GREEN}${BOLD} All tests completed successfully!       ${NC}"
echo -e "${GREEN}${BOLD}=========================================${NC}"
