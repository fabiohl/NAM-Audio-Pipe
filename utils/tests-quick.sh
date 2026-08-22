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
#   2. Release verification — integration tests only (release codegen surface;
#      unit logic is already covered by Phase 1 assertions — S6-T04 / RES-04).
#   3. PipeWire Live        — live end-to-end integration (requires running daemon).
#   4. Recording io_uring   — disk_writer_loop tests (requires kernel io_uring).
#
# Each phase persists its output to target/logs/quick-phaseN.log and the run
# closes with a typed receipt at target/logs/quick-receipt.txt.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

PHASE_TOTAL=4
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

trap 'status=$?
if [ "$status" -eq 124 ]; then
    echo -e "\n${RED}${BOLD}❌ TIMEOUT: command \"$BASH_COMMAND\" timed out at line $LINENO (phase ${PHASE_NUM:-?}/${PHASE_TOTAL:-?}). Aborting test suite.${NC}"
else
    echo -e "\n${RED}${BOLD}❌ Unexpected error: command \"$BASH_COMMAND\" failed at line $LINENO with status $status (phase ${PHASE_NUM:-?}/${PHASE_TOTAL:-?}). Aborting test suite.${NC}"
fi
exit 1' ERR

mkdir -p target/logs
rm -f target/logs/quick-phase1.log \
      target/logs/quick-phase2.log \
      target/logs/quick-phase3.log \
      target/logs/quick-phase4.log \
      target/logs/quick-receipt.txt

echo -e "${BLUE}${BOLD}===============================${NC}"
echo -e "${BLUE}${BOLD} NAM-Audio-Pipe Test Suite     ${NC}"
echo -e "${BLUE}${BOLD}===============================${NC}"

emit "SUITE: tests-quick"
emit "STRICT: ${NAM_QUICK_STRICT:-0}"

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
    2>&1 | tee target/logs/quick-phase1.log
assert_ran_tests target/logs/quick-phase1.log 1
emit "PHASE1: PASS log=target/logs/quick-phase1.log"

# ── Phase 2: Release verification (release, S6-T04 / RES-04) ─────────────────
# Only the integration tests run in release — the codegen-sensitive surface
# (e2e_cli + recording). Unit tests (--lib, --bin nam-audio-pipe) are already
# validated in Phase 1 debug with assertions ON; their release re-run was
# purely redundant wall-clock (RES-04 / SIB-03). The live PipeWire integration
# and the io_uring recording suite remain in Phases 3 and 4 respectively.
phase "Release verification: integration tests (release)..."
timeout 300 cargo test --features testing \
    --test recording \
    --test e2e_cli \
    --release \
    -- --test-threads=1 --nocapture \
    2>&1 | tee target/logs/quick-phase2.log
assert_ran_tests target/logs/quick-phase2.log 1
emit "PHASE2: PASS log=target/logs/quick-phase2.log"

# ── Phase 3: PipeWire Live Integration (release, daemon probe) ───────────────
declare -a GAPS=()
phase "PipeWire Live Integration (release)..."
echo -e "  Checking PipeWire daemon..."
if timeout 5 pw-cli info 0 > /dev/null 2>&1; then
    echo -e "  ${GREEN}PipeWire detected.${NC} Executing live integration test..."
    timeout 60 cargo test --features testing --release \
        --test pw_integration \
        -- --ignored --test-threads=1 --nocapture \
        2>&1 | tee target/logs/quick-phase3.log
    assert_ran_tests target/logs/quick-phase3.log 1
    emit "PHASE3: PASS log=target/logs/quick-phase3.log"
    emit "LIVE_PW=RAN"
else
    GAPS+=("pw_integration:daemon_unavailable")
    echo -e "${YELLOW}${BOLD}WARN GAP: pw_integration:daemon_unavailable — PipeWire daemon not reachable (pw-cli info 0 timed out or failed); live integration test SKIPPED.${NC}"
    emit "PHASE3: SKIP reason=daemon_unavailable"
    emit "LIVE_PW=SKIP"
fi

# ── Phase 4: Recording io_uring capability (release, --ignored) ──────────────
# Native Rust probe (src/bin/io_uring_probe.rs) — no python3 dependency. The
# probe maps to exit codes: 0 = available, 1 = kernel_unsupported,
# 2 = probe_tool_missing. Each is surfaced distinctly so a missing interpreter
# can never masquerade as an unsupported kernel.
io_uring_probe() {
    IO_URING_STATUS="probe_tool_missing"
    local probe_bin="${CARGO_TARGET_DIR:-target}/debug/io_uring_probe"
    if [ ! -x "$probe_bin" ]; then
        if ! cargo build --bin io_uring_probe >/dev/null 2>&1; then
            warn "io_uring native probe build failed"
            return 2
        fi
    fi
    local rc=0
    "$probe_bin" >/dev/null 2>&1 || rc=$?
    case "$rc" in
        0) IO_URING_STATUS="available" ;;
        1) IO_URING_STATUS="kernel_unsupported" ;;
        *) IO_URING_STATUS="probe_tool_missing" ;;
    esac
    return "$rc"
}

phase "Recording io_uring capability (release, --ignored)..."
if io_uring_probe; then
    echo -e "  ${GREEN}io_uring available.${NC} Executing recording disk-writer tests..."
    timeout 60 cargo test --features testing --release \
        --test recording \
        -- --ignored --test-threads=1 --nocapture \
        2>&1 | tee target/logs/quick-phase4.log
    assert_ran_tests target/logs/quick-phase4.log 1
    if grep -q "SKIP:" target/logs/quick-phase4.log; then
        # R-13 / S7.T3: the E2E --record test prints an honest SKIP: when the
        # PipeWire daemon is absent. A skip must never masquerade as RAN —
        # the receipt is SKIP with a GAP instead.
        GAPS+=("record_e2e:daemon_unavailable")
        echo -e "${YELLOW}${BOLD}WARN GAP: record_e2e:daemon_unavailable — PipeWire daemon not reachable; E2E recording test SKIPPED.${NC}"
        emit "PHASE4: SKIP reason=record_e2e_daemon_unavailable"
        emit "RECORDING_IO_URING=SKIP"
    else
        emit "PHASE4: PASS log=target/logs/quick-phase4.log"
        emit "RECORDING_IO_URING=RAN"
    fi
elif [ "$IO_URING_STATUS" = "kernel_unsupported" ]; then
    GAPS+=("recording_io_uring:kernel_unsupported")
    echo -e "${YELLOW}${BOLD}WARN GAP: recording_io_uring:kernel_unsupported — kernel/io_uring unavailable; recording disk-writer tests SKIPPED.${NC}"
    emit "PHASE4: SKIP reason=kernel_unsupported"
    emit "RECORDING_IO_URING=SKIP"
else
    GAPS+=("recording_io_uring:probe_tool_missing")
    echo -e "${YELLOW}${BOLD}WARN GAP: recording_io_uring:probe_tool_missing — native io_uring probe failed unexpectedly; recording disk-writer tests SKIPPED.${NC}"
    emit "PHASE4: SKIP reason=probe_tool_missing"
    emit "RECORDING_IO_URING=SKIP"
fi

# ── Receipt & summary ────────────────────────────────────────────────────────
if [ ${#GAPS[@]} -gt 0 ]; then
    for g in "${GAPS[@]}"; do
        emit "GAP: $g"
        echo -e "${YELLOW}${BOLD}WARN GAP: $g${NC}"
    done
    echo -e "\n${YELLOW}${BOLD}================================================================================${NC}"
    echo -e "  ${BOLD}Artifacts saved:${NC}"
    echo -e "    - Receipt:     ${CYAN}target/logs/quick-receipt.txt${NC}"
    echo -e "    - Phase 1 log: ${CYAN}target/logs/quick-phase1.log${NC}"
    echo -e "    - Phase 2 log: ${CYAN}target/logs/quick-phase2.log${NC}"
    echo -e "    - Phase 3 log: ${CYAN}target/logs/quick-phase3.log${NC}"
    echo -e "    - Phase 4 log: ${CYAN}target/logs/quick-phase4.log${NC}"
    echo -e "${YELLOW}${BOLD}================================================================================${NC}\n"
    if [ "${NAM_QUICK_STRICT:-0}" = "1" ]; then
        echo -e "${RED}${BOLD}OVERALL: FAIL reason=strict_gaps${NC}"
        emit "OVERALL: FAIL reason=strict_gaps"
        exit 1
    fi
    exit 0
fi

echo -e "\n${GREEN}${BOLD}================================================================================${NC}"
echo -e "${GREEN}${BOLD} All tests completed successfully! (NAM-Audio-Pipe)       ${NC}"
echo -e "  ${BOLD}Artifacts saved:${NC}"
echo -e "    - Receipt:     ${CYAN}target/logs/quick-receipt.txt${NC}"
echo -e "    - Phase 1 log: ${CYAN}target/logs/quick-phase1.log${NC}"
echo -e "    - Phase 2 log: ${CYAN}target/logs/quick-phase2.log${NC}"
echo -e "    - Phase 3 log: ${CYAN}target/logs/quick-phase3.log${NC}"
echo -e "    - Phase 4 log: ${CYAN}target/logs/quick-phase4.log${NC}"
echo -e "${GREEN}${BOLD}================================================================================${NC}\n"
emit "OVERALL: PASSED"
