#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# tests-long.sh — long audit suite: soak, RT, live PipeWire, io_uring, offline parity and artifact certification. Classe B.
# Uso: utils/tests-long.sh [--strict] [--artifact] [-h|--help]
# AI AGENTS MUST NEVER EXECUTE THIS SCRIPT DIRECTLY: execution is reserved for the human operator.
# Structural validation is limited to --help.
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -o pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh
SCRIPT_NAME=tests-long
LOG_ROOT="${NAM_LOG_ROOT:-target/logs}"
LONG_TARGET_DIR="${CARGO_TARGET_DIR:-target/long}"
CARGO_TARGET_DIR="$LONG_TARGET_DIR"
export CARGO_TARGET_DIR
RECEIPT_BIN=""
STRICT=0
USE_ARTIFACT=0
ARTIFACT_PATH="target/dist/nam-audio-pipe"
ARTIFACT_SHA256=""
T0=$SECONDS
declare -a GAPS=()
declare -a PHASE_STATUSES=()
PHASE_NUM=0
PHASE_RC=0
PHASE_DUR=0
CUR_LOG=""
LONG_RECEIPT_FAILED=0
usage() {
    cat <<'EOF'
Uso: utils/tests-long.sh [OPÇÕES]
  --strict       GAP converte-se em falha; modo de certificação de release
  --artifact     Audita target/dist/nam-audio-pipe e amarra todo o receipt ao SHA-256 exato
  -h, --help     Exibe esta ajuda
Fases:
  PHASE1  Soak acelerado e concorrência de swap
  PHASE2  RT-Safety heap-audit (zero alocação)
  PHASE3  RT deadline (orçamento em nanossegundos)
  PHASE4  RT jitter (dispersão entre callbacks)
  PHASE5  Stress de concorrência e throughput SPSC
  PHASE6  Endurance real e throughput da máquina de estados
  PHASE7  Resiliência PipeWire (daemon privado)
  PHASE8..10  PipeWire ao vivo, io_uring e paridade offline f64
EOF
}
while [ $# -gt 0 ]; do
    case "$1" in
        --strict) STRICT=1; shift ;;
        --artifact) USE_ARTIFACT=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "ERRO: flag não reconhecida: $1" >&2; usage >&2; exit 2 ;;
    esac
done
# --strict propagates NAM_RT_STRICT=1 to tests/rt_metrics.rs.
if [ "$STRICT" = 1 ]; then export NAM_RT_STRICT=1; fi
LOG_DIR="$LOG_ROOT"
TEXT_RECEIPT="$LOG_DIR/long-receipt.txt"
RECEIPT="$LOG_DIR/long-audit-receipt.jsonl"
FAIL_REASONS="$LOG_DIR/long-audit-receipt.fail-reasons.json"
mkdir -p "$LOG_DIR"
# The long build tree is disposable. Only the script-owned default is removed:
# a caller-provided CARGO_TARGET_DIR may be a shared cache and is never deleted.
if [ "$LONG_TARGET_DIR" = "target/long" ]; then rm -rf target/long; fi
if [ "$USE_ARTIFACT" = 1 ]; then
    if [ ! -f "$ARTIFACT_PATH" ]; then
        echo "ERRO: --artifact especificado mas $ARTIFACT_PATH não existe." >&2
        exit 2
    fi
    ARTIFACT_SHA256=$(sha256sum "$ARTIFACT_PATH" | awk '{print $1}')
fi
# RT core resolution lives in the typed `pipe_perf_gate` binary (engine policy:
# NAM_BENCH_CORE → first online isolated CPU → first permitted CPU → GAP).
PERF_GATE_BIN=""
HAS_CHRT_FIFO=0
if command -v chrt >/dev/null 2>&1 && chrt -f 80 true >/dev/null 2>&1; then HAS_CHRT_FIFO=1; fi
PHASE_LOGS=(phase1-soak.log phase2-heap-audit.log phase3-rt-deadline.log phase4-rt-jitter.log
    phase5-concurrency.log phase6-endurance.log phase7-pw-resilience.log phase8-pw-live.log
    phase9-recording-io-uring.log phase10-offline-parity.log)
rotate_real_logs() {
    local prev_root="$LOG_ROOT/prev" dest ts n f
    local -a owned=()
    for f in "${PHASE_LOGS[@]}"; do [ -e "$LOG_ROOT/$f" ] && owned+=("$LOG_ROOT/$f"); done
    [ -e "$LOG_ROOT/long-receipt.txt" ] && owned+=("$LOG_ROOT/long-receipt.txt")
    [ -e "$LOG_ROOT/long-audit-receipt.jsonl" ] && owned+=("$LOG_ROOT/long-audit-receipt.jsonl")
    [ -e "$LOG_ROOT/long-audit-receipt.fail-reasons.json" ] && owned+=("$LOG_ROOT/long-audit-receipt.fail-reasons.json")
    [ "${#owned[@]}" -eq 0 ] && return 0
    ts="$(date -u +%Y%m%dT%H%M%SZ)"; dest="$prev_root/$ts"; n=1
    while [ -e "$dest" ]; do n=$((n + 1)); dest="$prev_root/${ts}_${n}"; done
    mkdir -p "$dest"
    for f in "${owned[@]}"; do mv -f "$f" "$dest/" 2>/dev/null || true; done
    ls -1dt "$prev_root"/*/ 2>/dev/null | tail -n +4 | xargs -r rm -rf -- || true
}
rotate_real_logs
record_gap() { GAPS+=("$1"); echo "  GAP: $1"; }
extract_fail_reason() {
    [ -f "$1" ] || return 1
    grep -m1 -E 'panicked at|assertion|TEST_RESULT\[[^]]+\]=FAIL' "$1" 2>/dev/null || return 1
}
emit_fail_tail() {
    local log_path="$LOG_DIR/$1" out
    [ -f "$log_path" ] || return 0
    out=$(tail -n 40 "$log_path" 2>/dev/null | head -c 4096 || true)
    [ -n "$out" ] || return 0
    echo "  [PHASE${PHASE_NUM} FAIL-TAIL] ${log_path}" >&2
    printf '  [PHASE%s FAIL-TAIL] %s\n' "$PHASE_NUM" "$out" >&2
}
ensure_receipt_bin() {
    if [ -n "$RECEIPT_BIN" ] && [ -x "$RECEIPT_BIN" ]; then return 0; fi
    if ! cargo build --quiet --release --features testing --bin pipe_long_receipt >/dev/null 2>&1; then
        return 1
    fi
    RECEIPT_BIN="$CARGO_TARGET_DIR/release/pipe_long_receipt"
    [ -x "$RECEIPT_BIN" ]
}
ensure_perf_gate_bin() {
    if [ -n "$PERF_GATE_BIN" ] && [ -x "$PERF_GATE_BIN" ]; then return 0; fi
    if ! cargo build --quiet --release --features testing --bin pipe_perf_gate >/dev/null 2>&1; then
        return 1
    fi
    PERF_GATE_BIN="$CARGO_TARGET_DIR/release/pipe_perf_gate"
    [ -x "$PERF_GATE_BIN" ]
}
append_receipt() {
    local id="$1" name="$2" status="$3" log="$4" duration="$5" gaps="$6" fail_reason="$7"
    local -a args=(append --phase-id "$id" --name "$name" --status "$status"
        --duration-ms "$duration" --out "$RECEIPT")
    [ -n "$log" ] && args+=(--log "$log")
    [ -n "$gaps" ] && args+=(--gaps "$gaps")
    [ -n "$fail_reason" ] && args+=(--fail-reason "$fail_reason")
    if [ -n "$ARTIFACT_SHA256" ]; then args+=(--artifact-sha256 "$ARTIFACT_SHA256"); fi
    "$RECEIPT_BIN" "${args[@]}" >/dev/null 2>&1 || { echo "ERRO: falha ao gravar receipt JSONL" >&2; LONG_RECEIPT_FAILED=1; return 1; }
}
emit_text() { printf '%s\n' "$*" | tee -a "$TEXT_RECEIPT"; }
emit_text "SUITE: tests-long"
emit_text "STRICT: $STRICT"
emit_text "NAM_RT_STRICT: ${NAM_RT_STRICT:-0}"
emit_text "MODE: full"
emit_text "SOAK_PURPOSE: accelerated_timeline — compressed timeline (320k blocks), fail-closed validation windows"
emit_text "ENDURANCE_PURPOSE: real_wall_clock — wall-clock (NAM_ENDURANCE_SECONDS), fail-closed windows"
run_cmd() { local to="$1"; shift; timeout --kill-after=30 "$to" "$@"; }
run_rt_cmd() {
    local to="$1"; shift
    if ! ensure_perf_gate_bin; then
        record_gap "rt:pipe_perf_gate_unavailable"
        run_cmd "$to" "$@"
        return
    fi
    if [ "$HAS_CHRT_FIFO" = 1 ]; then
        run_cmd "$to" "$PERF_GATE_BIN" pin -- chrt -f 80 "$@"
    else
        run_cmd "$to" "$PERF_GATE_BIN" pin -- "$@"
    fi
}
# run_phase <id> <name> <timeout_s> <function>
run_phase() {
    local id="$1" name="$2" to="${NAM_PHASE_TIMEOUT_S:-$3}" fn="$4" log
    log="${PHASE_LOGS[$PHASE_NUM]}"
    PHASE_NUM=$((PHASE_NUM + 1))
    echo -e "\n[${PHASE_NUM}/10] ${name}"
    echo "  Log: ${LOG_DIR}/${log}"
    local start=$SECONDS
    CUR_LOG="${LOG_DIR}/${log}"
    "$fn" "$to" >"$CUR_LOG" 2>&1
    PHASE_RC=$?
    PHASE_DUR=$(( (SECONDS - start) * 1000 ))
    case "$PHASE_RC" in
        0) echo "  ✓ phase completed" ;;
        1) echo "  ❌ phase FAILED"; emit_fail_tail "$log" ;;
        2) echo "  ⚠ phase GAP" ;;
        *) echo "  ❌ phase failed with status ${PHASE_RC}"; emit_fail_tail "$log"; PHASE_RC=1 ;;
    esac
    return 0
}
finish_phase() {
    local id="$1" log="$2" rc="$3" pid_lc status_line jsonl_status="" gaps="" fail_reason="" env_suffix=""
    pid_lc=$(printf '%s' "$id" | tr '[:upper:]' '[:lower:]')
    if [ -f "${LOG_DIR}/${log}" ]; then
        env_suffix=$(grep -oE '\benv=[^ ]+' "${LOG_DIR}/${log}" | head -n1 || true)
    fi
    case "$rc" in
        0) status_line="${id}: PASS log=${LOG_DIR}/${log} duration_ms=${PHASE_DUR}${env_suffix:+ $env_suffix}"
           PHASE_STATUSES+=("PASS") ;;
        2) status_line="${id}: GAP log=${LOG_DIR}/${log} duration_ms=${PHASE_DUR}${env_suffix:+ $env_suffix}"
           PHASE_STATUSES+=("GAP") ;;
        *) status_line="${id}: FAIL log=${LOG_DIR}/${log} duration_ms=${PHASE_DUR}${env_suffix:+ $env_suffix}"
           PHASE_STATUSES+=("FAIL") ;;
    esac
    emit_text "$status_line"
    if ! ensure_receipt_bin; then
        echo "ERRO: pipe_long_receipt indisponível" >&2
        LONG_RECEIPT_FAILED=1
        return 1
    fi
    case "$status_line" in
        *": PASS "*) jsonl_status="PASSED" ;;
        *": GAP "*) jsonl_status="SKIPPED"; gaps="${pid_lc}:declared_gap" ;;
        *) jsonl_status="FAILED"
           fail_reason=$(extract_fail_reason "${LOG_DIR}/${log}" || true)
           fail_reason="${fail_reason:0:512}" ;;
    esac
    append_receipt "$pid_lc" "$id" "$jsonl_status" "${LOG_DIR}/${log}" "${PHASE_DUR:-0}" "$gaps" "$fail_reason"
}
phase_passed_target() { assert_ran_target "$CUR_LOG" "$1" "${2:-1}"; }
# ── Phase bodies (return 0 PASS / 1 FAIL / 2 GAP) ───────────────────────────
run_soak_phase() {
    local to="$1" rc=0
    local gaps_before=${#GAPS[@]}
    if [ -f tests/soak_extended.rs ]; then
        run_cmd "$to" cargo test --features testing --release --no-fail-fast \
            --test soak_extended -- --ignored --nocapture --test-threads=1 || rc=1
    else
        record_gap "phase1:soak_extended_harness_missing"
    fi
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test swap_stress -- --ignored --nocapture --test-threads=1 || rc=1
    [ "$rc" -eq 0 ] || return 1
    [ -f tests/soak_extended.rs ] && { phase_passed_target "tests/soak_extended.rs" || return 1; }
    phase_passed_target "tests/swap_stress.rs" || return 1
    [ "${#GAPS[@]}" -gt "$gaps_before" ] && return 2
    return 0
}
run_heap_audit_phase() {
    local to="$1" rc=0
    run_cmd "$to" cargo test --features "testing heap-audit" --release --no-fail-fast \
        --lib -- --nocapture || rc=1
    run_cmd "$to" cargo test --features "testing heap-audit" --release --no-fail-fast \
        --test swap_stress -- --nocapture || rc=1
    [ "$rc" -eq 0 ] || return 1
    assert_ran_tests "$CUR_LOG" 1 || return 1
    phase_passed_target "tests/swap_stress.rs" || return 1
    if ! grep -qE "swap_soak_heap_audit|heap_audit_" "$CUR_LOG"; then
        record_gap "phase2:no_heap_audit_tests_executed"; return 2
    fi
    return 0
}
run_rt_deadline_phase() {
    local to="$1" rc=0
    [ -f tests/rt_metrics.rs ] || { record_gap "phase3:rt_metrics_harness_missing"; return 2; }
    run_cmd "$to" cargo test --features testing --release --no-run --test rt_metrics || rc=1
    [ "$rc" -eq 0 ] || return 1
    run_rt_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test rt_metrics -- deadline --ignored --nocapture --test-threads=1 || rc=1
    [ "$rc" -eq 0 ] || return 1
    if grep -qF "TEST_RESULT[rt_deadline]=GAP" "$CUR_LOG"; then
        record_gap "phase3:$(grep -oE 'TEST_RESULT\[rt_deadline\]=GAP:[^ ]+' "$CUR_LOG" | head -n1)"; return 2
    fi
    if grep -qF "TEST_RESULT[rt_deadline_full]=GAP" "$CUR_LOG"; then
        record_gap "phase3:$(grep -oE 'TEST_RESULT\[rt_deadline_full\]=GAP:[^ ]+' "$CUR_LOG" | head -n1)"; return 2
    fi
    grep -qF "TEST_RESULT[rt_deadline]=PASS" "$CUR_LOG" || { record_gap "phase3:no_typed_deadline_result"; return 2; }
    grep -qF "TEST_RESULT[rt_deadline_full]=PASS" "$CUR_LOG" || { record_gap "phase3:no_typed_deadline_full_result"; return 2; }
    return 0
}
run_rt_jitter_phase() {
    local to="$1" rc=0
    [ -f tests/rt_metrics.rs ] || { record_gap "phase4:rt_metrics_harness_missing"; return 2; }
    run_rt_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test rt_metrics -- jitter --ignored --nocapture --test-threads=1 || rc=1
    [ "$rc" -eq 0 ] || return 1
    if grep -qF "TEST_RESULT[rt_jitter]=GAP" "$CUR_LOG"; then
        record_gap "phase4:$(grep -oE 'TEST_RESULT\[rt_jitter\]=GAP:[^ ]+' "$CUR_LOG" | head -n1)"; return 2
    fi
    grep -qF "TEST_RESULT[rt_jitter]=PASS" "$CUR_LOG" || { record_gap "phase4:no_typed_jitter_result"; return 2; }
    return 0
}
run_concurrency_phase() {
    local to="$1" rc=0
    [ -f tests/rt_metrics.rs ] || { record_gap "phase5:rt_metrics_harness_missing"; return 2; }
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test rt_metrics -- concurrent --ignored --nocapture --test-threads=1 || rc=1
    [ "$rc" -eq 0 ] || return 1
    if grep -qE "TEST_RESULT\[(concurrency_stress|model_check|concurrent|spsc_throughput)\]=GAP" "$CUR_LOG"; then
        record_gap "phase5:$(grep -oE 'TEST_RESULT\[(concurrency_stress|model_check|concurrent|spsc_throughput)\]=GAP:[^ ]+' "$CUR_LOG" | head -n1)"; return 2
    fi
    grep -qE "TEST_RESULT\[(concurrency_stress|model_check|concurrent|spsc_throughput)\]=PASS" "$CUR_LOG" \
        || { record_gap "phase5:no_typed_concurrency_stress_result"; return 2; }
    return 0
}
run_endurance_phase() {
    local to="$1" rc=0
    local gaps_before=${#GAPS[@]}
    if [ ! -f tests/endurance.rs ]; then record_gap "phase6:endurance_harness_missing"; return 2; fi
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test endurance -- --ignored --nocapture --test-threads=1 || rc=1
    [ "$rc" -eq 0 ] || return 1
    phase_passed_target "tests/endurance.rs" || return 1
    [ "${#GAPS[@]}" -gt "$gaps_before" ] && return 2
    return 0
}
run_pw_resilience_phase() {
    local to="$1" rc=0
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test pw_integration -- --nocapture --test-threads=1 || rc=1
    [ "$rc" -eq 0 ] || return 1
    if grep -qE "TEST_RESULT\[.*\]=GAP" "$CUR_LOG"; then
        record_gap "phase7:$(grep -oE 'TEST_RESULT\[[^]]+\]=GAP:[^ ]+' "$CUR_LOG" | head -n1)"; return 2
    fi
    grep -qE "TEST_RESULT\[(daemon_bounce_reconnect|daemon_bounce_exhaustion|daemon_bounce_failfast|pw_private_daemon)\]=PASS" "$CUR_LOG" \
        || { record_gap "phase7:no_typed_resilience_result"; return 2; }
    return 0
}
# Absorbed from the former quick-suite Phase 3 (live subprocess acceptances).
run_pw_live_phase() {
    local to="$1" rc=0 gaps_before=${#GAPS[@]} marker
    if ! command -v pw-cli >/dev/null 2>&1 || ! timeout 5 pw-cli info 0 >/dev/null 2>&1; then
        record_gap "phase8:pw_integration:daemon_unavailable"; return 2
    fi
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test pw_integration --test service_resilience -- --ignored --test-threads=1 --nocapture || rc=1
    [ "$rc" -eq 0 ] || return 1
    phase_passed_target "tests/pw_integration.rs" || return 1
    phase_passed_target "tests/service_resilience.rs" || return 1
    while IFS= read -r marker; do
        [ -n "$marker" ] && record_gap "phase8:$marker"
    done < <(grep -oE 'TEST_RESULT\[[^]]+\]=(GAP|SKIP):[^ ()]+' "$CUR_LOG" | sort -u || true)
    local sigterm_pass
    sigterm_pass=$(grep -cF "TEST_RESULT[sigterm_acceptance]=PASS" "$CUR_LOG" || true)
    [ "${sigterm_pass:-0}" -ge 2 ] || { record_gap "phase8:sigterm_acceptance_incomplete"; return 2; }
    grep -qF "TEST_RESULT[double_signal]=PASS" "$CUR_LOG" \
        || { record_gap "phase8:double_signal_not_certified"; return 2; }
    [ "${#GAPS[@]}" -gt "$gaps_before" ] && return 2
    return 0
}
# Absorbed from the former quick-suite Phase 4 (recording disk writer).
run_recording_phase() {
    local to="$1" rc=0 probe_rc=0 probe_bin="${CARGO_TARGET_DIR}/debug/io_uring_probe"
    cargo build --quiet --bin io_uring_probe >/dev/null 2>&1 || probe_rc=2
    if [ "$probe_rc" -eq 0 ]; then "$probe_bin" >/dev/null 2>&1 || probe_rc=$?; fi
    case "$probe_rc" in
        0) ;;
        1) record_gap "phase9:recording_io_uring:kernel_unsupported"; return 2 ;;
        *) record_gap "phase9:recording_io_uring:probe_tool_missing"; return 2 ;;
    esac
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test recording --test recording_fault_injection -- --ignored --test-threads=1 --nocapture || rc=1
    [ "$rc" -eq 0 ] || return 1
    phase_passed_target "tests/recording.rs" || return 1
    phase_passed_target "tests/recording_fault_injection.rs" || return 1
    if grep -qF "TEST_RESULT[record_e2e]=SKIP:" "$CUR_LOG"; then
        record_gap "phase9:record_e2e_daemon_or_io_uring_unavailable"; return 2
    fi
    grep -qF "TEST_RESULT[record_e2e]=PASS" "$CUR_LOG" \
        || { record_gap "phase9:no_typed_record_e2e_result"; return 2; }
    return 0
}
run_offline_parity_phase() {
    local to="$1" rc=0
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test pipe_offline_parity -- --ignored --nocapture --test-threads=1 || rc=1
    [ "$rc" -eq 0 ] || return 1
    phase_passed_target "tests/pipe_offline_parity.rs" || return 1
    grep -qE 'TEST_RESULT\[offline_parity_' "$CUR_LOG" \
        || { record_gap "phase10:no_typed_offline_parity_result"; return 2; }
    if [ "$USE_ARTIFACT" = 1 ]; then
        run_cmd "$to" "$ARTIFACT_PATH" --diagnose || return 1
        echo "# artifact smoke: $ARTIFACT_PATH sha256=$ARTIFACT_SHA256"
    fi
    return 0
}
run_phase "PHASE1" "Phase 1: Accelerated soak (compressed timeline) & swap concurrency" 1800 run_soak_phase
finish_phase "PHASE1" "phase1-soak.log" "$PHASE_RC"
run_phase "PHASE2" "Phase 2: RT-Safety heap-audit (zero-alloc)" 1800 run_heap_audit_phase
finish_phase "PHASE2" "phase2-heap-audit.log" "$PHASE_RC"
run_phase "PHASE3" "Phase 3: RT Deadline gate (nanosecond budget)" 900 run_rt_deadline_phase
finish_phase "PHASE3" "phase3-rt-deadline.log" "$PHASE_RC"
run_phase "PHASE4" "Phase 4: RT Jitter gate (inter-callback dispersion)" 900 run_rt_jitter_phase
finish_phase "PHASE4" "phase4-rt-jitter.log" "$PHASE_RC"
run_phase "PHASE5" "Phase 5: Concurrency interleaving stress & state resilience" 900 run_concurrency_phase
finish_phase "PHASE5" "phase5-concurrency.log" "$PHASE_RC"
run_phase "PHASE6" "Phase 6: Real endurance & state-machine throughput" 1800 run_endurance_phase
finish_phase "PHASE6" "phase6-endurance.log" "$PHASE_RC"
run_phase "PHASE7" "Phase 7: PipeWire resilience (private daemon)" 900 run_pw_resilience_phase
finish_phase "PHASE7" "phase7-pw-resilience.log" "$PHASE_RC"
run_phase "PHASE8" "Phase 8: Live PipeWire and SIGTERM acceptances" 900 run_pw_live_phase
finish_phase "PHASE8" "phase8-pw-live.log" "$PHASE_RC"
run_phase "PHASE9" "Phase 9: Recording io_uring and fault injection" 1200 run_recording_phase
finish_phase "PHASE9" "phase9-recording-io-uring.log" "$PHASE_RC"
run_phase "PHASE10" "Phase 10: Offline f64 parity and canonical artifact smoke" 900 run_offline_parity_phase
finish_phase "PHASE10" "phase10-offline-parity.log" "$PHASE_RC"
if [ "$LONG_RECEIPT_FAILED" -ne 0 ]; then
    echo "ERRO: emissão do receipt JSONL falhou — auditoria não certificável" >&2
    exit 1
fi
# Artifact binding is checked before the JSONL `overall` line is derived. A
# mismatch is fatal in every mode: a receipt that cannot be tied to the bytes
# it audited must never be certified. Tool output goes to a dedicated log — the
# strict text receipt accepts only its canonical line types.
if [ "$USE_ARTIFACT" = 1 ]; then
    if ! "$RECEIPT_BIN" verify-artifact --out "$RECEIPT" --artifact "$ARTIFACT_PATH" \
        >>"$LOG_DIR/tests-long-verify-artifact.log" 2>&1; then
        record_gap "artifact:sha_mismatch"
        for g in ${GAPS[@]+"${GAPS[@]}"}; do emit_text "GAP: $g"; done
        emit_text "OVERALL: COMPLETED_WITH_GAPS"
        echo "RESULT: FAIL ($((SECONDS - T0))s) fails=artifact_binding gaps=${GAPS[*]:-artifact:sha_mismatch}" >&2
        exit 1
    fi
fi
# Derive the JSONL `overall` line in Rust; a missing emitter fails closed.
if ! ensure_receipt_bin || ! "$RECEIPT_BIN" summary --out "$RECEIPT" >/dev/null 2>&1; then
    echo "ERRO: falha ao emitir o receipt JSONL unificado ($RECEIPT)" >&2
    exit 1
fi
for g in ${GAPS[@]+"${GAPS[@]}"}; do emit_text "GAP: $g"; done
HAS_FAIL=0
HAS_GAP=0
for st in ${PHASE_STATUSES[@]+"${PHASE_STATUSES[@]}"}; do
    case "$st" in FAIL) HAS_FAIL=1 ;; GAP) HAS_GAP=1 ;; esac
done
if [ "$HAS_FAIL" -eq 1 ]; then
    emit_text "OVERALL: FAILED"
    echo "RESULT: FAIL ($((SECONDS - T0))s) fails=${PHASE_STATUSES[*]} gaps=${GAPS[*]:-none}"
    exit 1
fi
if [ "$HAS_GAP" -eq 1 ] || [ "${#GAPS[@]}" -gt 0 ]; then
    emit_text "OVERALL: COMPLETED_WITH_GAPS"
    if [ "$STRICT" = 1 ]; then
        echo "RESULT: FAIL ($((SECONDS - T0))s) fails=none gaps=${GAPS[*]:-declared_phase_gap}"
        exit 1
    fi
    echo "RESULT: PASS_WITH_GAPS ($((SECONDS - T0))s) fails=none gaps=${GAPS[*]:-declared_phase_gap}"
    exit 0
fi
emit_text "OVERALL: PASSED"
echo "RESULT: PASS ($((SECONDS - T0))s) fails=none gaps=none"
exit 0
