#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# tests-long.sh — long audit suite: soak, RT, live PipeWire, io_uring, offline parity and artifact certification. Classe B.
# Uso: utils/tests-long.sh [--strict] [--artifact] [--only <nome>[,<nome>]] [-h|--help]
# Orçamento: 300
# AI AGENTS MUST NEVER EXECUTE THIS SCRIPT DIRECTLY: execution is reserved for the human operator.
# Structural validation is limited to --help.
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -o pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh
LONG_TARGET_DIR="${CARGO_TARGET_DIR:-target/long}"
CARGO_TARGET_DIR="$LONG_TARGET_DIR"
export CARGO_TARGET_DIR
RECEIPT_BIN=""
PERF_GATE_BIN=""
ARTIFACT_PATH="target/dist/nam-audio-pipe"
ARTIFACT_SHA256=""
USE_ARTIFACT=0
TEXT_RECEIPT="$LOG_DIR/long-receipt.txt"
RECEIPT="$LOG_DIR/long-audit-receipt.jsonl"
FAIL_REASONS="$LOG_DIR/long-audit-receipt.fail-reasons.json"
LONG_RECEIPT_FAILED=0
CUR_LOG=""
PRE_GAPS=()
TGTS=()
REQS=()
NOT_RUN=0
ONLY_PHASES=()
# Fonte única da tabela de fases: nome canônico + descrição curta (usada por --help).
PHASES=(
    "catch-all|Catch-all em release (suíte não-ignorada: e2e, distribution, stereo, --lib)"
    "soak|Soak acelerado e concorrência de swap"
    "heap-audit|RT-Safety heap-audit (zero alocação)"
    "rt-deadline|RT deadline (orçamento em nanossegundos)"
    "rt-jitter|RT jitter (dispersão entre callbacks)"
    "concurrency|Stress de concorrência e throughput SPSC"
    "endurance|Endurance real e throughput da máquina de estados"
    "pw-resilience|Resiliência PipeWire (daemon privado)"
    "pw-live|PipeWire ao vivo, SIGTERM e double-signal"
    "recording|Recording io_uring e fault injection"
    "offline-parity|Paridade offline f64 e smoke do artifact"
)
usage() {
    cat <<'EOF'
Uso: utils/tests-long.sh [OPÇÕES]
  --strict       GAP converte-se em falha; modo de certificação de release
  --artifact     Audita target/dist/nam-audio-pipe e amarra todo o receipt ao SHA-256 exato
  --only <nome>[,<nome>]  Roda apenas as fases nomeadas; preserva target/long; recibo parcial (sem overall)
  -h, --help     Exibe esta ajuda
Fases (log em target/logs/long-<nome>.log):
EOF
    for p in "${PHASES[@]}"; do printf '  %-15s %s\n' "${p%%|*}" "${p#*|}"; done
}
while [ $# -gt 0 ]; do
    case "$1" in
        --strict) STRICT=1; shift ;;
        --artifact) USE_ARTIFACT=1; shift ;;
        --only) IFS=',' read -r -a ONLY_PHASES <<<"${2:?--only requer lista}"; shift 2 ;;
        --only=*) IFS=',' read -r -a ONLY_PHASES <<<"${1#*=}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "ERRO: flag não reconhecida: $1" >&2; usage >&2; exit 2 ;;
    esac
done
for o in ${ONLY_PHASES[@]+"${ONLY_PHASES[@]}"}; do
    o=$(printf '%s' "$o" | tr '[:upper:]_' '[:lower:]-'); valid=0
    for p in "${PHASES[@]}"; do [ "${p%%|*}" = "$o" ] && valid=1; done
    [ "$valid" = 1 ] || { echo "ERRO: fase desconhecida: $o" >&2; exit 2; }
done
# Seleção somente por nome canônico de fase.
is_phase_selected() {
    local o
    [ ${#ONLY_PHASES[@]} -eq 0 ] && return 0
    for o in "${ONLY_PHASES[@]}"; do
        o=$(printf '%s' "$o" | tr '[:upper:]_' '[:lower:]-')
        [ "$o" = "$1" ] && return 0
    done
    return 1
}
if [ "$STRICT" = 1 ]; then export NAM_RT_STRICT=1; export NAM_REQUIRE_CPP_ORACLE=1; fi
if [ ${#ONLY_PHASES[@]} -eq 0 ] && [ "$LONG_TARGET_DIR" = "target/long" ]; then rm -rf target/long; fi
rm -f "$RECEIPT" "$FAIL_REASONS" "$TEXT_RECEIPT"
if [ "$USE_ARTIFACT" = 1 ]; then
    if [ ! -f "$ARTIFACT_PATH" ]; then
        echo "ERRO: --artifact especificado mas $ARTIFACT_PATH não existe." >&2
        exit 2
    fi
    ARTIFACT_SHA256=$(sha256sum "$ARTIFACT_PATH" | awk '{print $1}')
fi
HAS_CHRT_FIFO=0
if command -v chrt >/dev/null 2>&1 && chrt -f 80 true >/dev/null 2>&1; then HAS_CHRT_FIFO=1; fi
ensure_receipt_bin() {
    if [ -n "$RECEIPT_BIN" ] && [ -x "$RECEIPT_BIN" ]; then return 0; fi
    cargo build --quiet --release --features testing --bin pipe_long_receipt >/dev/null 2>&1 || return 1
    RECEIPT_BIN="$CARGO_TARGET_DIR/release/pipe_long_receipt"
    [ -x "$RECEIPT_BIN" ]
}
ensure_perf_gate_bin() {
    if [ -n "$PERF_GATE_BIN" ] && [ -x "$PERF_GATE_BIN" ]; then return 0; fi
    cargo build --quiet --release --features testing --bin pipe_perf_gate >/dev/null 2>&1 || return 1
    PERF_GATE_BIN="$CARGO_TARGET_DIR/release/pipe_perf_gate"
    [ -x "$PERF_GATE_BIN" ]
}
emit_text() { printf '%s\n' "$*" | tee -a "$TEXT_RECEIPT"; }
run_cmd() { local to="$1"; shift; timeout --kill-after=30 "$to" "$@"; }
run_rt_cmd() {
    local to="$1"; shift
    if ! ensure_perf_gate_bin; then
        pre_gap rt_perf_gate_unavailable
        run_cmd "$to" "$@"
        return
    fi
    if [ "$HAS_CHRT_FIFO" = 1 ]; then
        run_cmd "$to" "$PERF_GATE_BIN" pin -- chrt -f 80 "$@"
    else
        run_cmd "$to" "$PERF_GATE_BIN" pin -- "$@"
    fi
}
# Pre-run gaps are declared by the phase body before any test executes.
pre_gap() { PRE_GAPS+=("$1"); echo "GAP: $1"; }
# run_phase <n> <name> <timeout_s> <body>; log = target/logs/long-<name>.log.
run_phase() {
    local n="$1" name="$2" to="${NAM_PHASE_TIMEOUT_S:-$3}" body="$4"
    local id="phase$n" log="$LOG_DIR/long-$name.log" rc=0 start=$SECONDS dur pre tgt req app st gaps_out g
    local -a phase_gaps=()
    if ! is_phase_selected "$name"; then
        emit "PHASE: $name SKIPPED"
        "$RECEIPT_BIN" append --phase-id "$id" --name "$name" --status SKIPPED --duration-ms 0 \
            --out "$RECEIPT" ${ARTIFACT_SHA256:+--artifact-sha256 "$ARTIFACT_SHA256"} 2>/dev/null \
            || { LONG_RECEIPT_FAILED=1; FAILS+=("$id"); }
        return 0
    fi
    CUR_LOG="$log"; PRE_GAPS=(); TGTS=(); REQS=(); NOT_RUN=0
    emit "PHASE: $name"
    "$body" "$to" >"$log" 2>&1 || rc=$?
    dur=$(( (SECONDS - start) * 1000 ))
    pre=$(IFS=,; printf '%s' "${PRE_GAPS[*]-}")
    if [ "$NOT_RUN" = 1 ]; then
        gaps_out=$("$RECEIPT_BIN" append --phase-id "$id" --name "$name" --status SKIPPED \
            --duration-ms "$dur" --gaps "$pre" --log "$log" --out "$RECEIPT" \
            ${ARTIFACT_SHA256:+--artifact-sha256 "$ARTIFACT_SHA256"} 2>/dev/null)
    else
        tgt=$(IFS=,; printf '%s' "${TGTS[*]-}")
        req=$(IFS=,; printf '%s' "${REQS[*]-}")
        gaps_out=$("$RECEIPT_BIN" append --phase-id "$id" --name "$name" --from-log "$log" \
            --rc "$rc" --duration-ms "$dur" --gaps "$pre" --require "$req" \
            --require-target "$tgt" --out "$RECEIPT" \
            ${ARTIFACT_SHA256:+--artifact-sha256 "$ARTIFACT_SHA256"} 2>/dev/null)
    fi
    app=$?
    case "$app" in
        0) st=PASS ;;
        10) st=GAP ;;
        11) st=FAIL ;;
        *) st=ERROR; LONG_RECEIPT_FAILED=1; FAILS+=("$id") ;;
    esac
    while IFS= read -r g; do
        if [ -n "$g" ]; then phase_gaps+=("${g#GAP }"); GAPS+=("${g#GAP }"); fi
    done <<<"$gaps_out"
    case "$st" in
        PASS) emit "PHASE: $name PASS log=$log" ;;
        GAP) emit "PHASE: $name GAP $(IFS=,; printf '%s' "${phase_gaps[*]}")" ;;
        FAIL) FAILS+=("$id"); emit "PHASE: $name FAIL rc=$rc log=$log"; tail -n 40 "$log" >&2 ;;
    esac
    if [ "$st" != ERROR ]; then
        local env_suffix
        env_suffix=$(grep -oE '\benv=[^ ]+' "$log" | head -n1 || true)
        emit_text "PHASE${n}: $st log=$log duration_ms=$dur${env_suffix:+ $env_suffix}"
    fi
}
# ── Phase bodies (return 0 when the commands succeeded, 1 otherwise) ─────────
body_catch_all() { run_cmd "$1" cargo test --features testing --release --no-fail-fast; }
body_soak() {
    local to="$1" rc=0
    if [ -f tests/soak_extended.rs ]; then
        run_cmd "$to" cargo test --features testing --release --no-fail-fast \
            --test soak_extended -- --ignored --nocapture --test-threads=1 || rc=1
        TGTS+=(tests/soak_extended.rs)
    else
        pre_gap soak_extended_harness_missing
    fi
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test swap_stress -- --ignored --nocapture --test-threads=1 || rc=1
    TGTS+=(tests/swap_stress.rs)
    return "$rc"
}
body_heap_audit() {
    local to="$1" rc=0
    run_cmd "$to" cargo test --features "testing heap-audit" --release --no-fail-fast \
        --lib -- --nocapture || rc=1
    run_cmd "$to" cargo test --features "testing heap-audit" --release --no-fail-fast \
        --test swap_stress -- --nocapture || rc=1
    TGTS+=(tests/swap_stress.rs)
    return "$rc"
}
body_rt_deadline() {
    local to="$1"
    if [ ! -f tests/rt_metrics.rs ]; then NOT_RUN=1; pre_gap rt_metrics_harness_missing; return 0; fi
    run_cmd "$to" cargo test --features testing --release --no-run --test rt_metrics || return 1
    run_rt_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test rt_metrics -- deadline --ignored --nocapture --test-threads=1 || return 1
    REQS+=(rt_deadline rt_deadline_full)
}
body_rt_jitter() {
    local to="$1"
    if [ ! -f tests/rt_metrics.rs ]; then NOT_RUN=1; pre_gap rt_metrics_harness_missing; return 0; fi
    run_rt_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test rt_metrics -- jitter --ignored --nocapture --test-threads=1 || return 1
    REQS+=(rt_jitter)
}
body_concurrency() {
    local to="$1"
    if [ ! -f tests/rt_metrics.rs ]; then NOT_RUN=1; pre_gap rt_metrics_harness_missing; return 0; fi
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test rt_metrics -- concurrent --ignored --nocapture --test-threads=1 || return 1
    REQS+=("concurrency_stress|model_check|concurrent|spsc_throughput")
}
body_endurance() {
    local to="$1"
    if [ ! -f tests/endurance.rs ]; then NOT_RUN=1; pre_gap endurance_harness_missing; return 0; fi
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test endurance -- --ignored --nocapture --test-threads=1 || return 1
    TGTS+=(tests/endurance.rs)
}
body_pw_resilience() {
    local to="$1"
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test pw_integration -- --nocapture --test-threads=1 || return 1
    REQS+=("daemon_bounce_reconnect|daemon_bounce_exhaustion|daemon_bounce_failfast|pw_private_daemon")
}
body_pw_live() {
    local to="$1"
    if ! command -v pw-cli >/dev/null 2>&1 || ! timeout 5 pw-cli info 0 >/dev/null 2>&1; then
        NOT_RUN=1; pre_gap pw_integration_daemon_unavailable; return 0
    fi
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test pw_integration --test service_resilience -- --ignored --test-threads=1 --nocapture || return 1
    TGTS+=(tests/pw_integration.rs tests/service_resilience.rs)
    REQS+=("sigterm_acceptance:2" double_signal)
}
body_recording() {
    local to="$1" probe_rc=0 probe_bin="${CARGO_TARGET_DIR}/debug/io_uring_probe"
    cargo build --quiet --bin io_uring_probe >/dev/null 2>&1 || probe_rc=2
    if [ "$probe_rc" -eq 0 ]; then "$probe_bin" >/dev/null 2>&1 || probe_rc=$?; fi
    case "$probe_rc" in
        0) ;;
        1) NOT_RUN=1; pre_gap recording_io_uring_kernel_unsupported; return 0 ;;
        *) NOT_RUN=1; pre_gap recording_io_uring_probe_tool_missing; return 0 ;;
    esac
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test recording --test recording_fault_injection -- --ignored --test-threads=1 --nocapture || return 1
    TGTS+=(tests/recording.rs tests/recording_fault_injection.rs)
    REQS+=(record_e2e)
}
body_offline_parity() {
    local to="$1"
    run_cmd "$to" cargo test --features testing --release --no-fail-fast \
        --test pipe_offline_parity -- --ignored --nocapture --test-threads=1 || return 1
    TGTS+=(tests/pipe_offline_parity.rs)
    REQS+=("offline_parity_*")
    if [ "$USE_ARTIFACT" = 1 ]; then
        run_cmd "$to" "$ARTIFACT_PATH" --diagnose || return 1
        echo "# artifact smoke: $ARTIFACT_PATH sha256=$ARTIFACT_SHA256"
    fi
}
ensure_receipt_bin || { echo "ERRO: pipe_long_receipt indisponível" >&2; LONG_RECEIPT_FAILED=1; }
emit_text "SUITE: tests-long"
emit_text "STRICT: $STRICT"
emit_text "NAM_RT_STRICT: ${NAM_RT_STRICT:-0}"
emit_text "MODE: full"
emit_text "SOAK_PURPOSE: accelerated_timeline — compressed timeline (320k blocks), fail-closed validation windows"
emit_text "ENDURANCE_PURPOSE: real_wall_clock — wall-clock (NAM_ENDURANCE_SECONDS), fail-closed windows"
run_phase 0 catch-all 900 body_catch_all
run_phase 1 soak 1800 body_soak
run_phase 2 heap-audit 1800 body_heap_audit
run_phase 3 rt-deadline 900 body_rt_deadline
run_phase 4 rt-jitter 900 body_rt_jitter
run_phase 5 concurrency 900 body_concurrency
run_phase 6 endurance 1800 body_endurance
run_phase 7 pw-resilience 900 body_pw_resilience
run_phase 8 pw-live 900 body_pw_live
run_phase 9 recording 1200 body_recording
run_phase 10 offline-parity 900 body_offline_parity
if [ "$LONG_RECEIPT_FAILED" -eq 0 ] && [ "$USE_ARTIFACT" = 1 ]; then
    if ! "$RECEIPT_BIN" verify-artifact --out "$RECEIPT" --artifact "$ARTIFACT_PATH" \
        >>"$LOG_DIR/tests-long-verify-artifact.log" 2>&1; then
        GAPS+=("artifact:sha_mismatch"); FAILS+=(artifact_binding)
    fi
fi
if [ ${#ONLY_PHASES[@]} -eq 0 ] && [ "$LONG_RECEIPT_FAILED" -eq 0 ] && [ ${#FAILS[@]} -eq 0 ]; then
    "$RECEIPT_BIN" summary --out "$RECEIPT" >/dev/null 2>&1 || FAILS+=(summary)
fi
for g in ${GAPS[@]+"${GAPS[@]}"}; do emit_text "GAP: $g"; done
if [ ${#FAILS[@]} -gt 0 ]; then emit_text "OVERALL: FAILED"
elif [ ${#ONLY_PHASES[@]} -gt 0 ]; then emit_text "OVERALL: PARTIAL (--only: recibo sem overall, não certificável)"
elif [ ${#GAPS[@]} -gt 0 ]; then emit_text "OVERALL: COMPLETED_WITH_GAPS"
else emit_text "OVERALL: PASSED"; fi
finish
