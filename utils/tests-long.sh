#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Nightly / pre-release long audit suite of NAM-Audio-Pipe (G-RB-002, T6.3).
#
# Certifies the claims that the agile `utils/tests-quick.sh` deliberately
# leaves out: prolonged soak + swap concurrency, strict zero-allocation on the
# RT thread (heap-audit), nanosecond RT deadline budget, inter-callback jitter
# dispersion and concurrent state-model checking. Every phase below
# cross-references its planned harness so scope drift is visible at a glance:
#
#   PHASE1 — Soak acelerado (timeline comprimida)           phase1-soak.log
#            tests/swap_stress.rs (--ignored soak battery; T6.4 registers the
#            extended soak cases) + tests/soak_extended.rs (T6.4, T5.3:
#            320k blocks / ~426s of compressed timeline, fail-closed windows).
#   PHASE2 — RT-Safety heap-audit (zero-alloc)            phase2-heap-audit.log
#            `--features "testing heap-audit"`: lib unit gates
#            (cabsim_swap::heap_audit_tests) + swap_stress heap-audit battery.
#   PHASE3 — RT Deadline gate (ns budget)                 phase3-rt-deadline.log
#            tests/rt_metrics.rs deadline filter (T6.5, planned harness).
#   PHASE4 — RT Jitter gate (inter-callback dispersion)   phase4-rt-jitter.log
#            tests/rt_metrics.rs jitter filter (T6.5, planned harness).
#   PHASE5 — Concurrency model checking & throughput      phase5-concurrency.log
#            tests/rt_metrics.rs concurrent filter (T6.5): the 16-thread
#            interleaving stress + the production-SPSC throughput gate
#            (T5.3 / G-PERF-004 — no global mutex on the measured path).
#   PHASE6 — Endurance real & state-machine throughput    phase6-endurance.log
#            tests/endurance.rs (T5.3 / G-PERF-004): real wall-clock
#            endurance, fail-closed validation windows, periodic raw
#            RSS/faults/threads/FD telemetry.
#
#   T5.3 (G-PERF-004): the receipt declares each soak suite's purpose — the
#   accelerated timeline soak (`SOAK_PURPOSE:`) and the real wall-clock
#   endurance (`ENDURANCE_PURPOSE:`) are separate suites with distinct
#   purposes, and the throughput marker never labels harness throughput as
#   "audio callbacks".
#
#   Under --strict-pre-release, NAM_RT_STRICT=1 is exported so Phases 3/4/5
#   (tests/rt_metrics.rs) fail hard on an uncalibrated environment instead of
#   emitting a silent pass (T5.1); the receipt records `NAM_RT_STRICT:` as the
#   propagation evidence.
#
# Failure isolation: every phase runs independently through `run_phase` so a
# failure in one test never interrupts the remaining phases — a nightly run
# that dies on a shell bug would cost a full day of blind spots before the
# next window. Each completed phase emits one typed line to
# target/logs/long-receipt.txt (PHASEn: PASS|FAIL|GAP ...) and the suite
# closes with the `GAP:` reasons plus the `OVERALL:` verdict
# (PASSED / COMPLETED_WITH_GAPS / FAILED).
#
# ────────────────────────────────────────────────────────────────────────────
# AI GOVERNANCE WARNING (mandatory compliance — rules/testing.md §2, G-RB-002)
#
#   This script is the NIGHTLY / PRE-RELEASE long audit suite of
#   NAM-Audio-Pipe. Its full runtime is approximately 30–60 minutes and it
#   requires an isolated, calibrated real-time environment (dedicated CPU
#   affinity, low background load, optionally SCHED_FIFO permissions).
#
#   AI AGENTS MUST NEVER EXECUTE THIS SCRIPT DIRECTLY. Execution is reserved
#   for the HUMAN OPERATOR. If structural validation of the suite is needed,
#   use the non-executing surfaces instead and ask the operator to run the
#   full audit and report the results:
#       ./utils/tests-long.sh --help       # usage & phase inventory
#       ./utils/tests-long.sh --simulate   # registers the 6 planned phases in
#                                          # target/logs/ without running tests
# ────────────────────────────────────────────────────────────────────────────

set -euo pipefail

STRICT_PRE_RELEASE=0
SIMULATE=0
for arg in "$@"; do
    case "$arg" in
        --strict-pre-release)
            STRICT_PRE_RELEASE=1
            ;;
        --simulate|--dry-run)
            SIMULATE=1
            ;;
        --help|-h)
            echo "Usage: $0 [--strict-pre-release] [--simulate|--dry-run]"
            echo
            echo "NAM-Audio-Pipe nightly / pre-release long audit suite"
            echo "(~30-60 min, HUMAN OPERATOR ONLY — AI agents must never execute it directly)."
            echo
            echo "Options:"
            echo "  --strict-pre-release   Promote every GAP to a hard failure (release gate)."
            echo "                         Certifies the audit only on a calibrated RT machine."
            echo "                         Propagates NAM_RT_STRICT=1 to tests/rt_metrics.rs (T5.1)."
            echo "  --simulate, --dry-run  Dry run: register the 6 planned phases and the"
            echo "                         structured receipt in target/logs/ without executing"
            echo "                         any test. Safe for AI/CI structural validation."
            echo "  -h, --help             Show this help and exit."
            echo
            echo "Phases (logs in target/logs/):"
            echo "  PHASE1 — Soak acelerado (timeline comprimida)   phase1-soak.log"
            echo "  PHASE2 — RT-Safety heap-audit (zero-alloc)      phase2-heap-audit.log"
            echo "  PHASE3 — RT Deadline gate (ns budget)           phase3-rt-deadline.log"
            echo "  PHASE4 — RT Jitter gate (dispersion)            phase4-rt-jitter.log"
            echo "  PHASE5 — Concurrency model checking             phase5-concurrency.log"
            echo "  PHASE6 — Endurance real & state-machine throughput (T5.3)"
            echo "                                                  phase6-endurance.log"
            echo
            echo "T5.3 (G-PERF-004): the receipt declares each suite's purpose —"
            echo "  SOAK_PURPOSE:      accelerated_timeline (timeline comprimida)"
            echo "  ENDURANCE_PURPOSE: real_wall_clock (parede, NAM_ENDURANCE_SECONDS)"
            echo
            echo "Receipt: target/logs/long-receipt.txt"
            exit 0
            ;;
        *)
            echo -e "\033[0;31m[FATAL]\033[0m Unknown option: $arg" >&2
            echo "Usage: $0 [--strict-pre-release] [--simulate|--dry-run]" >&2
            exit 2
            ;;
    esac
done

# T5.1: --strict-pre-release propagates NAM_RT_STRICT=1 to the RT metric
# harness (tests/rt_metrics.rs Phases 3/4/5). Exported before any phase so the
# child cargo processes inherit it; the value is also recorded in the receipt
# as `NAM_RT_STRICT:` (the propagation evidence the release ceremony verifies
# semantically via src/bin/long_receipt_check.rs).
if [ "$STRICT_PRE_RELEASE" = "1" ]; then
    export NAM_RT_STRICT=1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_PATH="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"

# Shared style helpers (RED/GREEN/YELLOW/BLUE/BOLD/NC), cd to project root and
# the fail-closed executed-test gates (assert_ran_tests / assert_ran_target).
source "$SCRIPT_DIR/_lib.sh"

# CPU core pinning for the RT measurement phases (Deadline, Jitter). Override
# with NAM_BENCH_CORE; defaults to the middle physical core. The Rust harness
# itself preflights the environment and emits typed GAP markers when it cannot
# certify (see tests/rt_metrics.rs, T6.5) — the shell never reclassifies.
NUM_CORES=$(nproc 2>/dev/null || echo 1)
DEFAULT_CORE=$(( ${NUM_CORES:-1} / 2 ))
BENCH_CORE="${NAM_BENCH_CORE:-$DEFAULT_CORE}"
HAS_TASKSET=0
if command -v taskset >/dev/null 2>&1; then
    HAS_TASKSET=1
fi

# Defensive error trap (message + abort). Phase failures are isolated via
# `run_phase ... || true` below and never reach this trap: a command inside a
# function invoked under `||` is executed with errexit/ERR suppressed.
trap 'echo -e "\n${RED}${BOLD}❌ Unexpected error: command \"$BASH_COMMAND\" failed at line $LINENO with status $? (phase ${PHASE_NUM:-?}/${PHASE_TOTAL:-?}). Aborting audit suite.${NC}"; exit 1' ERR

PHASE_TOTAL=6

LONG_RECEIPT="target/logs/long-receipt.txt"

mkdir -p target/logs
# Targeted cleanup: only the artifacts owned by this suite are removed, so
# quick receipts, release provenance and other tools' logs are preserved.
rm -f target/logs/phase1-soak.log \
      target/logs/phase2-heap-audit.log \
      target/logs/phase3-rt-deadline.log \
      target/logs/phase4-rt-jitter.log \
      target/logs/phase5-concurrency.log \
      target/logs/phase6-endurance.log \
      target/logs/long-receipt.txt

echo -e "${BLUE}${BOLD}=============================================================${NC}"
echo -e "${BLUE}${BOLD}  NAM-Audio-Pipe Long-Duration Stress & Audit Suite           ${NC}"
echo -e "${BLUE}${BOLD}  (~30-60 min · human operator only · G-RB-002/T6.3)          ${NC}"
echo -e "${BLUE}${BOLD}=============================================================${NC}"

if [ "$SIMULATE" = "1" ]; then
    echo -e "${YELLOW}${BOLD}SIMULATION MODE: no test will be executed.${NC}"
fi

emit_to "$LONG_RECEIPT" "SUITE: tests-long"
emit_to "$LONG_RECEIPT" "STRICT: ${STRICT_PRE_RELEASE:-0}"
emit_to "$LONG_RECEIPT" "NAM_RT_STRICT: ${NAM_RT_STRICT:-0}"
emit_to "$LONG_RECEIPT" "MODE: $([ "$SIMULATE" = "1" ] && echo simulate || echo full)"
# T5.3 (G-PERF-004): each soak suite declares its purpose in the receipt so the
# accelerated timeline soak and the real wall-clock endurance are never
# conflated.
emit_to "$LONG_RECEIPT" "SOAK_PURPOSE: accelerated_timeline — timeline comprimida (320k blocos ≈426s nominais), janelas de validação fail-closed, swaps periódicos"
emit_to "$LONG_RECEIPT" "ENDURANCE_PURPOSE: real_wall_clock — parede (NAM_ENDURANCE_SECONDS), janelas fail-closed, RSS bruto/faults/threads/FDs periódicos"

# ── Global phase trackers ──────────────────────────────────────────────────
declare -a GAPS=()
declare -a PHASE_STATUSES=()
PHASE_RC=0
PHASE_DUR=0
PHASE_NUM=0

# record_gap <reason>
#   Registers a typed environment/implementation gap (never a silent skip).
record_gap() {
    GAPS+=("$1")
    echo -e "  ${YELLOW}ⓘ GAP: $1${NC}"
}

# run_phase <name> <cmd> <logfile>
#   Executes <cmd> (a phase function name or a command string) with all
#   output captured to target/logs/<logfile>. The phase runs independently:
#   callers must use `|| true` and inspect $PHASE_RC afterwards, so a failure
#   never aborts the remaining phases. In --simulate mode the phase is only
#   registered (placeholder log) and no command is executed.
run_phase() {
    local name="$1"
    local cmd="$2"
    local log_file="$3"
    local start_time status

    PHASE_NUM=$((PHASE_NUM + 1))
    echo -e "\n${BLUE}${BOLD}[${PHASE_NUM}/${PHASE_TOTAL}] ${name}${NC}"
    echo -e "  Executing: ${YELLOW}${cmd}${NC}"
    echo -e "  Log:       ${YELLOW}target/logs/${log_file}${NC}"

    if [ "$SIMULATE" = "1" ]; then
        {
            echo "[SIMULATION] Phase not executed — dry run requested."
            echo "[SIMULATION] Phase:  ${name}"
            echo "[SIMULATION] Command: ${cmd}"
            echo "[SIMULATION] Planned log file: target/logs/${log_file}"
        } > "target/logs/${log_file}"
        PHASE_RC=0
        PHASE_DUR=0
        return 0
    fi

    start_time=$(date +%s%N)
    # The phase body captures its own outcome (cargo exit codes, gates and
    # typed GAP markers) and returns 0 (PASS) / 1 (FAIL) / 2 (GAP).
    eval "$cmd" > "target/logs/${log_file}" 2>&1
    status=$?
    PHASE_RC=$status
    PHASE_DUR=$(( ($(date +%s%N) - start_time) / 1000000 ))

    case "$status" in
        0) echo -e "  ${GREEN}✓ phase completed (${PHASE_DUR} ms)${NC}" ;;
        1) echo -e "  ${RED}❌ phase FAILED (${PHASE_DUR} ms)${NC}" ;;
        2) echo -e "  ${YELLOW}⚠ phase GAP (${PHASE_DUR} ms)${NC}" ;;
        *) echo -e "  ${RED}❌ phase failed with status ${status} (${PHASE_DUR} ms)${NC}" ;;
    esac
    return 0
}

# finish_phase <phase_id> <log_file> <rc>
#   Emits the typed receipt line for the phase just completed. Status follows
#   <rc> (0=PASS, 1=FAIL, 2=GAP); --simulate always records SIMULATED.
#   <rc> is passed explicitly by the caller (never read from a stale global).
finish_phase() {
    local phase_id="$1"
    local log_file="$2"
    local phase_rc="$3"
    local status_line
    if [ "$SIMULATE" = "1" ]; then
        status_line="${phase_id}: SIMULATED log=target/logs/${log_file}"
        PHASE_STATUSES+=("SIMULATED")
        echo -e "  ${YELLOW}ⓘ ${phase_id}: simulated (no tests executed)${NC}"
    else
        case "$phase_rc" in
            0) status_line="${phase_id}: PASS log=target/logs/${log_file} duration_ms=${PHASE_DUR}"
               PHASE_STATUSES+=("PASS")
               echo -e "  ${GREEN}✓ ${phase_id}: PASS${NC}" ;;
            1) status_line="${phase_id}: FAIL log=target/logs/${log_file} duration_ms=${PHASE_DUR}"
               PHASE_STATUSES+=("FAIL")
               echo -e "  ${RED}❌ ${phase_id}: FAIL${NC}" ;;
            2) status_line="${phase_id}: GAP log=target/logs/${log_file} duration_ms=${PHASE_DUR}"
               PHASE_STATUSES+=("GAP")
               echo -e "  ${YELLOW}⚠ ${phase_id}: GAP${NC}" ;;
            *) status_line="${phase_id}: FAIL log=target/logs/${log_file} duration_ms=${PHASE_DUR}"
               PHASE_STATUSES+=("FAIL") ;;
        esac
    fi
    emit_to "$LONG_RECEIPT" "$status_line"
}

# empty_target_execution <log_file> <target>
#   Returns 0 when <target> ran (its "Running <target> " banner exists) but
#   its summary reports 0 executed tests. Distinguishes a planned-but-empty
#   soak battery (typed GAP — T6.4/T6.5 pending) from a removed/renamed
#   mandatory target (no banner — hard FAIL).
empty_target_execution() {
    local log_file="$1"
    local target="$2"
    local run_line run_lineno result_line
    run_line=$(grep -n -m1 -F "Running ${target} " "$log_file" 2>/dev/null || true)
    [ -n "$run_line" ] || return 1
    run_lineno="${run_line%%:*}"
    result_line=$(sed -n "$((run_lineno + 1)),\$p" "$log_file" | grep -m1 -E 'test result:' 2>/dev/null || true)
    [ -n "$result_line" ] || return 1
    printf '%s\n' "$result_line" | grep -qP '\b0\s+passed' && return 0
    return 1
}

# gated_target_status <log_file> <target> <min_exec> <gap_reason>
#   Fail-closed gate used by soak-type phases: verifies real execution of a
#   mandatory target. Returns 0 on success; when the target ran but executed
#   fewer than min_exec tests it records the typed GAP <gap_reason> and
#   returns 2; a missing banner returns 1 (hard failure — target
#   removed/renamed/filtered out).
gated_target_status() {
    local log_file="$1"
    local target="$2"
    local min_exec="$3"
    local gap_reason="$4"
    if assert_ran_target "$log_file" "$target" "$min_exec"; then
        return 0
    fi
    if empty_target_execution "$log_file" "$target"; then
        record_gap "$gap_reason"
        return 2
    fi
    return 1
}

# ═══════════════════════════════════════════════════════════════════════════
# Phase bodies — one function per phase, passed by name to run_phase so the
# suite stays easy to scan and extend in an unattended nightly job. Each
# returns 0 (PASS) / 1 (FAIL) / 2 (GAP).
# ═══════════════════════════════════════════════════════════════════════════

# --- Phase 1: Soak acelerado & concorrência de swaps (release, --ignored) ---
# tests-quick.sh runs the fast swap_stress pass (debug + release, non-ignored);
# every #[ignore]'d soak/endurance case lives here exclusively (rules/testing.md
# §1: soak MUST be #[ignore]d and run only in tests-long.sh). T6.4 registers the
# extended soak battery (soak_extended.rs + ignored swap_stress cases); T5.3
# (G-PERF-004) declares this phase's purpose as the accelerated timeline soak.
run_soak_phase() {
    local rc=0
    local gaps_before=${#GAPS[@]}
    if [ -f tests/soak_extended.rs ]; then
        echo "  → tests/soak_extended.rs — extended multi-model soak (T6.4)"
        cargo test --features testing --release --no-fail-fast \
            --test soak_extended -- --ignored --nocapture --test-threads=1 || rc=1
    else
        record_gap "phase1:soak_extended_harness_missing (T6.4 pending)"
    fi
    echo "  → tests/swap_stress.rs — ignored soak battery"
    cargo test --features testing --release --no-fail-fast \
        --test swap_stress -- --ignored --nocapture --test-threads=1 || rc=1
    if [ "$rc" -eq 0 ]; then
        if [ -f tests/soak_extended.rs ]; then
            gated_target_status "target/logs/phase1-soak.log" "tests/soak_extended.rs" 1 \
                "phase1:soak_extended_no_ignored_tests (T6.4 pending)" || rc=$?
        fi
        if [ "$rc" -eq 0 ]; then
            gated_target_status "target/logs/phase1-soak.log" "tests/swap_stress.rs" 1 \
                "phase1:swap_soak_no_ignored_tests (T6.4 pending)" || rc=$?
        fi
    fi
    if [ "$rc" -eq 0 ] && [ "${#GAPS[@]}" -gt "$gaps_before" ]; then
        return 2
    fi
    return "$rc"
}

# --- Phase 2: RT-Safety heap-audit (release, --features "testing heap-audit") ---
# Zero-allocation verification on the RT thread under the global counting
# allocator. No quick-suite equivalent — the heap-audit feature is exclusively
# a long-suite concern. The lib unit gates (cabsim_swap::heap_audit_tests,
# rt_callback) and the swap_stress battery assert get_alloc_count() == 0.
run_heap_audit_phase() {
    local rc=0
    local gaps_before=${#GAPS[@]}
    echo "  → lib unit heap-audit gates (cabsim_swap::heap_audit_tests + RT paths)"
    cargo test --features "testing heap-audit" --release --no-fail-fast \
        --lib -- --nocapture || rc=1
    echo "  → integration heap-audit battery (tests/swap_stress.rs)"
    cargo test --features "testing heap-audit" --release --no-fail-fast \
        --test swap_stress -- --nocapture || rc=1
    if [ "$rc" -eq 0 ]; then
        if ! assert_ran_tests "target/logs/phase2-heap-audit.log" 1; then
            rc=1
        fi
    fi
    if [ "$rc" -eq 0 ]; then
        if ! assert_ran_target "target/logs/phase2-heap-audit.log" "tests/swap_stress.rs" 1; then
            if empty_target_execution "target/logs/phase2-heap-audit.log" "tests/swap_stress.rs"; then
                record_gap "phase2:heap_audit_tests_not_registered"
                rc=2
            else
                rc=1
            fi
        fi
    fi
    if [ "$rc" -eq 0 ]; then
        if ! grep -qE "swap_soak_heap_audit|heap_audit_" "target/logs/phase2-heap-audit.log"; then
            record_gap "phase2:no_heap_audit_tests_executed"
            rc=2
        fi
    fi
    if [ "$rc" -eq 0 ] && [ "${#GAPS[@]}" -gt "$gaps_before" ]; then
        return 2
    fi
    return "$rc"
}

# --- Phase 3: RT Deadline Gate (deterministic, nanosecond budget) ---
# tests/rt_metrics.rs (T6.5) executes 10k quantums of real DSP load and emits
# typed markers: TEST_RESULT[rt_deadline]=PASS max_ns=... budget_ns=... or
# TEST_RESULT[rt_deadline]=GAP:uncalibrated_environment. The shell only maps
# markers; it never reclassifies logs.
run_rt_deadline_phase() {
    if [ ! -f tests/rt_metrics.rs ]; then
        echo "  → tests/rt_metrics.rs missing — planned by T6.5"
        record_gap "phase3:rt_metrics_harness_missing (T6.5 pending)"
        return 2
    fi
    local rc=0
    echo "  → rt_metrics deadline gate (nanosecond budget, core ${BENCH_CORE})"
    if [ "$HAS_TASKSET" = "1" ] && [ -n "${BENCH_CORE:-}" ]; then
        taskset -c "$BENCH_CORE" cargo test --features testing --release --no-fail-fast \
            --test rt_metrics -- deadline --ignored --nocapture --test-threads=1 || rc=1
    else
        cargo test --features testing --release --no-fail-fast \
            --test rt_metrics -- deadline --ignored --nocapture --test-threads=1 || rc=1
    fi
    # A real cargo failure is a hard FAIL and must never be masked by a typed
    # GAP marker lingering in the same log. Marker classification is
    # fail-closed (T6.5): the harness emits exactly one
    # TEST_RESULT[rt_deadline]=... marker per run — a GAP marker means the
    # measurement was bypassed/inconclusive and is never promoted to PASS.
    if [ "$rc" -ne 0 ]; then
        return 1
    fi
    if grep -qF "TEST_RESULT[rt_deadline]=GAP" "target/logs/phase3-rt-deadline.log"; then
        record_gap "phase3:$(grep -oP 'TEST_RESULT\[rt_deadline\]=GAP:[^ ]+' target/logs/phase3-rt-deadline.log | head -n1)"
        return 2
    fi
    if ! grep -qF "TEST_RESULT[rt_deadline]=PASS" "target/logs/phase3-rt-deadline.log"; then
        record_gap "phase3:no_typed_deadline_result"
        return 2
    fi
    return 0
}

# --- Phase 4: RT Jitter Gate (inter-callback dispersion under contention) ---
# Diagnostic telemetry: statistical dispersion of inter-callback intervals.
# tests/rt_metrics.rs (T6.5) emits TEST_RESULT[rt_jitter]=PASS max_jitter_us=...
# or a typed GAP when environment preconditions are not met.
run_rt_jitter_phase() {
    if [ ! -f tests/rt_metrics.rs ]; then
        echo "  → tests/rt_metrics.rs missing — planned by T6.5"
        record_gap "phase4:rt_metrics_harness_missing (T6.5 pending)"
        return 2
    fi
    local rc=0
    echo "  → rt_metrics jitter gate (inter-callback dispersion, core ${BENCH_CORE})"
    if [ "$HAS_TASKSET" = "1" ] && [ -n "${BENCH_CORE:-}" ]; then
        taskset -c "$BENCH_CORE" cargo test --features testing --release --no-fail-fast \
            --test rt_metrics -- jitter --ignored --nocapture --test-threads=1 || rc=1
    else
        cargo test --features testing --release --no-fail-fast \
            --test rt_metrics -- jitter --ignored --nocapture --test-threads=1 || rc=1
    fi
    # Same fail-closed precedence as the deadline gate: a real cargo failure
    # is a hard FAIL, never masked by a typed GAP marker in the same log.
    if [ "$rc" -ne 0 ]; then
        return 1
    fi
    if grep -qF "TEST_RESULT[rt_jitter]=GAP" "target/logs/phase4-rt-jitter.log"; then
        record_gap "phase4:$(grep -oP 'TEST_RESULT\[rt_jitter\]=GAP:[^ ]+' target/logs/phase4-rt-jitter.log | head -n1)"
        return 2
    fi
    if ! grep -qF "TEST_RESULT[rt_jitter]=PASS" "target/logs/phase4-rt-jitter.log"; then
        record_gap "phase4:no_typed_jitter_result"
        return 2
    fi
    return 0
}

# --- Phase 5: Concurrency interleaving stress, state resilience & throughput ---
# 16-thread stress over swap requests, stream-state transitions, simulated
# reconnect cycles and cooperative SHUTDOWN (T6.5) — zero deadlocks, no
# inconsistent sample-rate reads, no leaked failure state. Plus (T5.3 /
# G-PERF-004) the production-SPSC state-machine throughput gate
# (concurrent_spsc_throughput_swap_accounting): no global mutex on the measured
# path; complete attempted/enqueued/applied/dropped swap accounting. Emits
# typed markers TEST_RESULT[concurrency_stress]=PASS and
# TEST_RESULT[spsc_throughput]=PASS/GAP:...
run_concurrency_phase() {
    if [ ! -f tests/rt_metrics.rs ]; then
        echo "  → tests/rt_metrics.rs missing — planned by T6.5"
        record_gap "phase5:rt_metrics_harness_missing (T6.5 pending)"
        return 2
    fi
    local rc=0
    echo "  → rt_metrics concurrent state interleaving stress (16 threads)"
    echo "  → rt_metrics SPSC state-machine throughput (T5.3, no global mutex)"
    cargo test --features testing --release --no-fail-fast \
        --test rt_metrics -- concurrent --ignored --nocapture --test-threads=1 || rc=1
    # Same fail-closed precedence as the deadline gate: a real cargo failure
    # is a hard FAIL, never masked by a typed GAP marker in the same log.
    if [ "$rc" -ne 0 ]; then
        return 1
    fi
    if grep -qE "TEST_RESULT\[(concurrency_stress|model_check|concurrent|spsc_throughput)\]=GAP" "target/logs/phase5-concurrency.log"; then
        record_gap "phase5:$(grep -oP 'TEST_RESULT\[(concurrency_stress|model_check|concurrent|spsc_throughput)\]=GAP:[^ ]+' target/logs/phase5-concurrency.log | head -n1)"
        return 2
    fi
    if ! grep -qE "TEST_RESULT\[(concurrency_stress|model_check|concurrent|spsc_throughput)\]=PASS" "target/logs/phase5-concurrency.log"; then
        record_gap "phase5:no_typed_concurrency_stress_result"
        return 2
    fi
    return 0
}

# --- Phase 6: Endurance real & state-machine throughput (release, --ignored) ---
# tests/endurance.rs (T5.3 / G-PERF-004): real wall-clock endurance distinct
# from the accelerated soak — fail-closed validation windows, periodic raw
# RSS/faults/threads/FD telemetry. The phase purpose is declared in the receipt
# as `ENDURANCE_PURPOSE: real_wall_clock`. Override the window with
# NAM_ENDURANCE_SECONDS (default 30, floor 5).
run_endurance_phase() {
    local rc=0
    local gaps_before=${#GAPS[@]}
    if [ -f tests/endurance.rs ]; then
        echo "  → tests/endurance.rs — real wall-clock endurance (T5.3 / G-PERF-004)"
        echo "  → NAM_ENDURANCE_SECONDS=${NAM_ENDURANCE_SECONDS:-30} (default 30s wall clock)"
        cargo test --features testing --release --no-fail-fast \
            --test endurance -- --ignored --nocapture --test-threads=1 || rc=1
    else
        record_gap "phase6:endurance_harness_missing (T5.3 pending)"
        return 2
    fi
    if [ "$rc" -eq 0 ]; then
        gated_target_status "target/logs/phase6-endurance.log" "tests/endurance.rs" 1 \
            "phase6:endurance_no_ignored_tests (T5.3 pending)" || rc=$?
    fi
    if [ "$rc" -eq 0 ] && [ "${#GAPS[@]}" -gt "$gaps_before" ]; then
        return 2
    fi
    return "$rc"
}

# ═══════════════════════════════════════════════════════════════════════════
# Phase execution — every phase is isolated via `|| true`; the phase's own
# return code (PASS/FAIL/GAP) is consumed by finish_phase immediately after.
# ═══════════════════════════════════════════════════════════════════════════

run_phase "Phase 1: Soak acelerado (timeline comprimida) & concorrência de swaps" "run_soak_phase" "phase1-soak.log" || true
finish_phase "PHASE1" "phase1-soak.log" "$PHASE_RC"

run_phase "Phase 2: RT-Safety heap-audit (zero-alloc)" "run_heap_audit_phase" "phase2-heap-audit.log" || true
finish_phase "PHASE2" "phase2-heap-audit.log" "$PHASE_RC"

run_phase "Phase 3: RT Deadline gate (nanosecond budget)" "run_rt_deadline_phase" "phase3-rt-deadline.log" || true
finish_phase "PHASE3" "phase3-rt-deadline.log" "$PHASE_RC"

run_phase "Phase 4: RT Jitter gate (inter-callback dispersion)" "run_rt_jitter_phase" "phase4-rt-jitter.log" || true
finish_phase "PHASE4" "phase4-rt-jitter.log" "$PHASE_RC"

run_phase "Phase 5: Concurrency interleaving stress & state resilience" "run_concurrency_phase" "phase5-concurrency.log" || true
finish_phase "PHASE5" "phase5-concurrency.log" "$PHASE_RC"

run_phase "Phase 6: Endurance real & state-machine throughput" "run_endurance_phase" "phase6-endurance.log" || true
finish_phase "PHASE6" "phase6-endurance.log" "$PHASE_RC"

# ── Summary & verdict ──────────────────────────────────────────────────────
if [ "${#GAPS[@]}" -gt 0 ]; then
    for g in "${GAPS[@]}"; do
        emit_to "$LONG_RECEIPT" "GAP: $g"
        echo -e "${YELLOW}${BOLD}WARN GAP: $g${NC}"
    done
fi

HAS_FAIL=0
HAS_GAP=0
for st in "${PHASE_STATUSES[@]}"; do
    case "$st" in
        FAIL) HAS_FAIL=1 ;;
        GAP) HAS_GAP=1 ;;
    esac
done

echo -e "\n${BLUE}${BOLD}================ AUDIT SUMMARY ================${NC}"
for i in "${!PHASE_STATUSES[@]}"; do
    printf '  Phase %d: %s\n' "$((i + 1))" "${PHASE_STATUSES[$i]}"
done

if [ "$SIMULATE" = "1" ]; then
    emit_to "$LONG_RECEIPT" "OVERALL: SIMULATED"
    echo -e "\n${YELLOW}${BOLD}Simulation only — no test executed. Planned phases registered in target/logs/.${NC}"
    exit 0
fi

if [ "$HAS_FAIL" -eq 1 ]; then
    echo -e "\n${RED}${BOLD}❌ One or more phases failed. Check target/logs/phase*-*.log and target/logs/long-receipt.txt${NC}"
    emit_to "$LONG_RECEIPT" "OVERALL: FAILED"
    exit 1
fi

if [ "$HAS_GAP" -eq 1 ]; then
    echo -e "\n${YELLOW}${BOLD}⚠ Audit completed with declared gaps (inconclusive / skipped / unexecuted phases).${NC}"
    emit_to "$LONG_RECEIPT" "OVERALL: COMPLETED_WITH_GAPS"
    if [ "$STRICT_PRE_RELEASE" -eq 1 ]; then
        echo -e "${RED}${BOLD}❌ --strict-pre-release: failing audit due to declared gaps. Release requires certification on a calibrated RT machine.${NC}"
        exit 1
    fi
    exit 0
fi

echo -e "\n${GREEN}${BOLD}✓ All long-audit phases completed successfully!${NC}"
emit_to "$LONG_RECEIPT" "OVERALL: PASSED"
exit 0
