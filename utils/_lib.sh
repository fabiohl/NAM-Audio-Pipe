# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# utils/_lib.sh — execução, logs e carrier tipado de NAM-Audio-Pipe (fases, gates, RESULT). Classe D (biblioteca). Sem cor, sem cópia pinada.
# Uso: chamar lib_init após o parsing de help; concluir com trap_fail + finish.
# Orçamento: 120
LOG_DIR=target/logs
NAM_RUN_ID="${NAM_RUN_ID:-}"
PROJECT_DIR="$PWD"
SCRIPT_NAME="$(basename "${0%.sh}")"
STATUS_NAME="$SCRIPT_NAME"
T0=$SECONDS; FAILS=(); GAPS=(); STRICT=0; EMIT_RC=0

warn()  { echo "WARN: $*" >&2; }
die()   { echo "ERROR: $*" >&2; exit 1; }
ok()    { echo "OK: $*"; }
emit()  {
    local line=$1
    printf '%s\n' "$line"
    if [ "${lib_quiet:-}" != "1" ]; then
        printf '%s\n' "$line" >> "$LOG_DIR/summary.txt" 2>/dev/null || { echo "ERROR: cannot append summary.txt" >&2; EMIT_RC=1; }
    fi
}
phase() { emit "PHASE: $1"; }
gap()   { GAPS+=("$1"); emit "PHASE: ${PHASE_NAME:-$1} GAP $1"; }

# lib_init — ÚNICO ponto que cria target/logs e trunca summary.txt. Depois do help.
# lib_quiet (filho dentro de fase do pai) só inibe summary/marker: o carrier obsoleto do
# próprio filho continua sendo removido, então crash antes de finish nunca deixa veredito velho.
lib_init() {
    mkdir -p "$LOG_DIR" || die "cannot create $LOG_DIR"
    if [ -z "$NAM_RUN_ID" ]; then NAM_RUN_ID="$(date -u +%Y%m%dT%H%M%SZ)-$$"; fi
    export NAM_RUN_ID
    rm -f "$LOG_DIR/${STATUS_NAME}-status.tsv"
    if [ "${lib_quiet:-}" = "1" ]; then return 0; fi
    if [ "$(cat "$LOG_DIR/.summary-run" 2>/dev/null)" != "$NAM_RUN_ID" ]; then
        : > "$LOG_DIR/summary.txt" || die "cannot reset $LOG_DIR/summary.txt"
        printf '%s\n' "$NAM_RUN_ID" > "$LOG_DIR/.summary-run" || die "cannot write $LOG_DIR/.summary-run"
    fi
}

# usage_fail <msg> — flag inválida: exatamente um RESULT e exit 2, sem tocar em evidência.
usage_fail() {
    echo "RESULT: FAIL (0s) fails=uso gaps=none ($1)"
    exit 2
}

# run_step <fase> <comando…> — Classe A: captura PIPESTATUS[0] e status do tee.
run_step() {
    local name=$1; shift; PHASE_NAME=$name; phase "$name"
    local log="$LOG_DIR/${SCRIPT_NAME}-$name.log" rc trc pst
    set +e; ( set -e; "$@" ) 2>&1 | tee "$log"; pst=("${PIPESTATUS[@]}"); set -e; rc=${pst[0]}; trc=${pst[1]}
    if [ "$rc" -ne 0 ]; then
        FAILS+=("$name"); emit "PHASE: $name FAIL rc=$rc log=$log"; finish || true; exit 1
    elif [ "$trc" -ne 0 ]; then
        FAILS+=("log-$name"); emit "PHASE: $name FAIL rc=$trc log=$log (tee)"; finish || true; exit 1
    fi
    emit "PHASE: $name PASS log=$log"
}

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

# trap_fail — último comando do ERR trap: o carrier pertence ao pai, nunca a um filho.
trap_fail() {
    local rc=$1
    FAILS+=("${PHASE_NAME:-step}")
    emit "PHASE: ${PHASE_NAME:-step} FAIL rc=$rc log=${CUR_LOG:-none}" || true
    finish || true
}

# carrier_write <veredito> — uma linha, quatro colunas TAB. Falha de escrita é FAIL.
carrier_write() {
    local sf="$LOG_DIR/${STATUS_NAME}-status.tsv"
    printf '%s\t%s\t%s\t%s\n' "$STATUS_NAME" "$1" \
        "$(IFS=,; printf '%s' "${GAPS[*]:-none}")" "$(IFS=,; printf '%s' "${FAILS[*]:-none}")" > "$sf" 2>/dev/null && return 0
    echo "ERROR: cannot write status carrier $sf" >&2
    FAILS+=("carrier"); return 1
}

finish() {
    local s=PASS e=0 line
    [ ${#GAPS[@]} -gt 0 ] && s=PASS_WITH_GAPS
    { [ ${#FAILS[@]} -gt 0 ] || { [ "$STRICT" = 1 ] && [ ${#GAPS[@]} -gt 0 ]; }; } && s=FAIL
    [ "$EMIT_RC" -ne 0 ] && { FAILS+=("emit"); s=FAIL; }
    carrier_write "$s" || s=FAIL
    line="RESULT: $s ($((SECONDS - T0))s) fails=${FAILS[*]:-none} gaps=${GAPS[*]:-none}"
    if [ "${lib_quiet:-}" != "1" ] && ! printf '%s\n' "$line" >> "$LOG_DIR/summary.txt" 2>/dev/null; then
        e=1; echo "ERROR: cannot append summary.txt" >&2
        if [ "$s" != FAIL ]; then
            FAILS+=("emit"); s=FAIL; carrier_write "$s" || true
            line="RESULT: $s ($((SECONDS - T0))s) fails=${FAILS[*]:-none} gaps=${GAPS[*]:-none}"
        fi
    fi
    printf '%s\n' "$line"
    [ "$s" != FAIL ] && [ "$e" -eq 0 ]
}
