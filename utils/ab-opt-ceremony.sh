#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# ab-opt-ceremony.sh — A/B optimization ceremony (operator-only: plain → PGO → PGO+BOLT). Classe D.
# Uso: utils/ab-opt-ceremony.sh [--runs N] [--blocks N] [--no-pgo] [--no-bolt] [--bench-extra 'ARGS'] [-h|--help]
# AI agents must not execute this script; it belongs to the operator release ceremony.
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh
SCRIPT_NAME=ab-opt-ceremony

RUNS=3
BLOCKS=20000
USE_PGO=1
USE_BOLT=1
VARIANT_TIMEOUT_S=1800
COOLDOWN_S="${NAM_THERMAL_COOLDOWN_S:-0}"
AB_DIR="$PWD/target/logs"
AB_BUILD_DIR="$PWD/target/ab-opt-build"
RECEIPT="$AB_DIR/ab-opt-ceremony-receipt.json"
AB_BENCH_FLAGS=()

usage() {
    cat <<'EOF'
Uso: utils/ab-opt-ceremony.sh [OPÇÕES]
  --runs N            Execuções medidas por variante (padrão 3, mínimo 3)
  --blocks N          Blocos DSP por execução medida (padrão 20000)
  --no-pgo            Compara apenas PLAIN (diagnóstico, sem veredito)
  --no-bolt           Compila PGO mas não BOLT (compara plain → PGO)
  --bench-extra 'ARGS'  Flags extras para ab_opt_bench (ex.: '--quantum 256 --oversample 4x')
  -h, --help          Exibe esta ajuda
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --runs) RUNS="$2"; shift 2 ;;
        --blocks) BLOCKS="$2"; shift 2 ;;
        --no-pgo) USE_PGO=0; USE_BOLT=0; shift ;;
        --no-bolt) USE_BOLT=0; shift ;;
        --bench-extra)
            IFS=' ' read -r -a EXTRA <<< "$2"
            AB_BENCH_FLAGS+=(${EXTRA[@]+"${EXTRA[@]}"})
            shift 2
            ;;
        -h|--help) usage; exit 0 ;;
        *) echo "ERRO: opção desconhecida: $1" >&2; usage >&2; exit 2 ;;
    esac
done

[ "$RUNS" -ge 3 ] 2>/dev/null || { echo "ERRO: --runs exige inteiro >= 3 (recebido: $RUNS)" >&2; exit 2; }
[ "$BLOCKS" -ge 1 ] 2>/dev/null || { echo "ERRO: --blocks exige inteiro >= 1 (recebido: $BLOCKS)" >&2; exit 2; }
AB_BENCH_FLAGS=(--blocks "$BLOCKS" ${AB_BENCH_FLAGS[@]+"${AB_BENCH_FLAGS[@]}"})
[[ "$COOLDOWN_S" =~ ^[0-9]+$ ]] || { echo "ERRO: NAM_THERMAL_COOLDOWN_S inválido: $COOLDOWN_S" >&2; exit 2; }

need() { command -v "$1" >/dev/null 2>&1 || { echo "ERRO: '$1' não encontrado no PATH." >&2; exit 2; }; }
need rustc; need cargo; need timeout
mkdir -p "$AB_DIR"
rm -f "$AB_DIR"/ab-opt-plain.json "$AB_DIR"/ab-opt-pgo.json "$AB_DIR"/ab-opt-pgo+bolt.json
rm -f "$AB_DIR"/ab-opt-pgo-gen.json "$AB_DIR"/ab-opt-bolt-probe.json "$AB_DIR"/ab-opt-ceremony-receipt.json
rm -rf "$AB_BUILD_DIR"
mkdir -p "$AB_BUILD_DIR"
export CARGO_TARGET_DIR="$AB_BUILD_DIR"
cargo build --quiet --locked --release --bin pipe_perf_gate --bin pipe_ab_opt
PERF_GATE_BIN="$AB_BUILD_DIR/release/pipe_perf_gate"
COMPARE_BIN="$AB_BUILD_DIR/release/pipe_ab_opt"

# §4.2: explicit NAM_BENCH_CORE → first online isolated CPU → first permitted
# CPU → unpinned GAP. The typed helper owns the topology policy.
PIN=()
BENCH_CORE=""
if BENCH_CORE=$("$PERF_GATE_BIN" pick-core 2>/dev/null); then
    PIN=("$PERF_GATE_BIN" pin --bench-core "$BENCH_CORE" --)
    echo "bench core: $BENCH_CORE"
else
    gap bench_core_unavailable; echo "  reason: no explicit/isolated/permitted core (running unpinned)"
fi

cooldown() {
    [ "$COOLDOWN_S" -eq 0 ] && return 0
    echo "cooldown: ${COOLDOWN_S}s"
    sleep "$COOLDOWN_S"
}
run_timed() { local t="$1"; shift; timeout --kill-after=30 "$t" "$@"; }

# Preserve [build] rustflags from .cargo/config.toml: an explicit RUSTFLAGS
# replaces the config value, so the ceremony re-injects it around the PGO flags.
CONFIG_RUSTFLAGS=$(awk '
    /^[[:space:]]*rustflags[[:space:]]*=/ { active = 1 }
    active {
        line = $0; sub(/#.*/, "", line)
        while (match(line, /"[^"]*"/)) {
            printf "%s ", substr(line, RSTART + 1, RLENGTH - 2)
            line = substr(line, RSTART + RLENGTH)
        }
        if (index(line, "]")) active = 0
    }
' .cargo/config.toml)
CONFIG_RUSTFLAGS="${CONFIG_RUSTFLAGS% }"
[ -n "$CONFIG_RUSTFLAGS" ] || die "Não foi possível extrair rustflags de .cargo/config.toml."
# LLVM BOLT needs retained symbols and relocation sections (--relocs).
CONFIG_RUSTFLAGS="$CONFIG_RUSTFLAGS -C strip=none -Clink-arg=-Wl,-q"

LLVM_PROFDATA=""
if [ "$USE_PGO" = 1 ]; then
    SYSROOT="$(rustc --print sysroot)"
    HOST="$(rustc -vV | sed -n 's/^host: //p')"
    LLVM_PROFDATA="$SYSROOT/lib/rustlib/$HOST/bin/llvm-profdata"
    [ -x "$LLVM_PROFDATA" ] || die "llvm-profdata ausente em $LLVM_PROFDATA (rustup component add llvm-tools-preview)."
fi

LLVM_BOLT=""
HAS_PERF=0
if [ "$USE_BOLT" = 1 ]; then
    for candidate in /usr/lib/llvm-{22,21,20,19,18}/bin/llvm-bolt \
        /usr/bin/llvm-bolt-{22,21} /usr/bin/llvm-bolt; do
        [ -x "$candidate" ] && { LLVM_BOLT="$candidate"; break; }
    done
    if [ -n "$LLVM_BOLT" ]; then
        PERF2BOLT="$(dirname "$LLVM_BOLT")/perf2bolt"
        [ -x "$PERF2BOLT" ] || PERF2BOLT="perf2bolt"
    else
        gap llvm_bolt_unavailable; echo "  reason: llvm-bolt not found (comparing plain → PGO only)"
    fi
    if command -v perf >/dev/null 2>&1; then
        PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
        if [ "$PARANOID" -gt 1 ] && sudo -n sysctl -q -w kernel.perf_event_paranoid=1 2>/dev/null; then
            PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
        fi
        if [ "$PARANOID" -le 1 ]; then
            HAS_PERF=1
        else
            gap perf_event_paranoid; echo "  reason: kernel.perf_event_paranoid=$PARANOID (sudo sysctl -w kernel.perf_event_paranoid=1)"
        fi
    else
        gap perf_unavailable; echo "  reason: perf not found"
    fi
fi

build_variant() {
    local name="$1" flags="$2"
    echo "building variant '$name'"
    run_timed "$VARIANT_TIMEOUT_S" env RUSTFLAGS="$flags" \
        cargo build --locked --release --features testing --bin ab_opt_bench
}
run_variant() {
    local name="$1" bin="$2"
    [ -x "$bin" ] || die "binário da variante '$name' ausente: $bin"
    echo "running variant '$name' (runs=$RUNS, blocks=$BLOCKS)"
    run_timed "$VARIANT_TIMEOUT_S" ${PIN[@]+"${PIN[@]}"} "$bin" \
        --variant "$name" --runs "$RUNS" "${AB_BENCH_FLAGS[@]}" \
        --receipt "$AB_DIR/ab-opt-$name.json"
}

echo "A/B optimization ceremony: runs=$RUNS blocks=$BLOCKS flags=${AB_BENCH_FLAGS[*]}"
build_variant plain "$CONFIG_RUSTFLAGS"
run_variant plain "$AB_BUILD_DIR/release/ab_opt_bench"
cooldown

PGO_DIR="$AB_BUILD_DIR/pgo"
BOLT_APPLIED=0
BOLT_CAUSE=""
if [ "$USE_PGO" = 1 ]; then
    mkdir -p "$PGO_DIR/profraw"
    echo "building variant 'pgo-gen'"
    run_timed "$VARIANT_TIMEOUT_S" env \
        RUSTFLAGS="$CONFIG_RUSTFLAGS -Cprofile-generate=$PGO_DIR/profraw" \
        cargo build --locked --release --features testing --bin ab_opt_bench
    echo "running variant 'pgo-gen' (profile capture)"
    run_timed "$VARIANT_TIMEOUT_S" env \
        LLVM_PROFILE_FILE="$PGO_DIR/profraw/default_%m_%p.profraw" \
        timeout --kill-after=30 "$VARIANT_TIMEOUT_S" ${PIN[@]+"${PIN[@]}"} \
        "$AB_BUILD_DIR/release/ab_opt_bench" --variant pgo-gen --runs 1 \
        --blocks "$BLOCKS" --receipt "$AB_DIR/ab-opt-pgo-gen.json"
    PROFRAW_COUNT=$(find "$PGO_DIR/profraw" -name '*.profraw' | wc -l)
    [ "$PROFRAW_COUNT" -gt 0 ] || die "PGO: nenhum .profraw gerado."
    run_timed "$VARIANT_TIMEOUT_S" "$LLVM_PROFDATA" merge -sparse \
        -o "$PGO_DIR/merged.profdata" "$PGO_DIR"/profraw/*.profraw
    build_variant pgo "$CONFIG_RUSTFLAGS -Cprofile-use=$PGO_DIR/merged.profdata"
    run_variant pgo "$AB_BUILD_DIR/release/ab_opt_bench"
    cooldown

    if [ "$USE_BOLT" = 1 ] && [ -n "$LLVM_BOLT" ] && [ "$HAS_PERF" = 1 ]; then
        echo "building & running variant 'pgo+bolt'"
        rm -f "$PGO_DIR/perf.data"
        PERF_ARGS=(-F 4000 -e cycles:u)
        if run_timed 60 perf record -e cycles:u -j any,u -F 99 -o /dev/null -- true >/dev/null 2>&1; then
            PERF_ARGS=(-F 99 -e cycles:u -j any,u)
            LBR=1
        else
            LBR=0
        fi
        if ! run_timed "$VARIANT_TIMEOUT_S" perf record "${PERF_ARGS[@]}" -o "$PGO_DIR/perf.data" -- \
            ${PIN[@]+"${PIN[@]}"} "$AB_BUILD_DIR/release/ab_opt_bench" --variant pgo-gen --runs 1 \
            --blocks "$BLOCKS" --receipt "$AB_DIR/ab-opt-bolt-probe.json" >/dev/null 2>&1; then
            BOLT_CAUSE="BOLT_FAILED: perf record session failed."
        elif [ ! -s "$PGO_DIR/perf.data" ]; then
            BOLT_CAUSE="BOLT_FAILED: perf.data empty or missing."
        fi
        if [ -z "$BOLT_CAUSE" ]; then
            chmod 644 "$PGO_DIR/perf.data" 2>/dev/null || true
            P2B_FLAGS=()
            if [ "$LBR" = 0 ]; then
                if "$PERF2BOLT" --help 2>&1 | grep -q -- '--basic-events'; then
                    P2B_FLAGS=(--basic-events)
                else
                    gap perf2bolt_without_basic_events
                fi
            fi
            if ! run_timed "$VARIANT_TIMEOUT_S" "$PERF2BOLT" ${P2B_FLAGS[@]+"${P2B_FLAGS[@]}"} \
                -p "$PGO_DIR/perf.data" "$AB_BUILD_DIR/release/ab_opt_bench" \
                -o "$PGO_DIR/perf.fdata" >"$PGO_DIR/perf2bolt.log" 2>&1; then
                BOLT_CAUSE="BOLT_FAILED: perf2bolt conversion failed: $(tail -n1 "$PGO_DIR/perf2bolt.log" 2>/dev/null || true)"
            fi
        fi
        if [ -z "$BOLT_CAUSE" ] && run_timed "$VARIANT_TIMEOUT_S" "$LLVM_BOLT" \
            "$AB_BUILD_DIR/release/ab_opt_bench" -o "$AB_BUILD_DIR/release/ab_opt_bench.bolt" \
            -data "$PGO_DIR/perf.fdata" --reorder-blocks=ext-tsp --reorder-functions=hfsort \
            --split-functions --split-all-cold -hugify --relocs --lite >"$PGO_DIR/llvm-bolt.log" 2>&1; then
            BOLT_APPLIED=1
        elif [ -z "$BOLT_CAUSE" ]; then
            BOLT_CAUSE="BOLT_FAILED: llvm-bolt failed: $(tail -n1 "$PGO_DIR/llvm-bolt.log" 2>/dev/null || true)"
        fi
        if [ "$BOLT_APPLIED" = 1 ]; then
            run_variant pgo+bolt "$AB_BUILD_DIR/release/ab_opt_bench.bolt"
            cooldown
        else
            gap bolt_not_applied; echo "  reason: $BOLT_CAUSE"
        fi
    elif [ "$USE_BOLT" = 1 ]; then
        BOLT_CAUSE="BOLT_UNAVAILABLE: ${LLVM_BOLT:-llvm-bolt-not-found}/perf=$HAS_PERF"
        gap bolt_skipped; echo "  reason: $BOLT_CAUSE"
    fi
fi

if [ "$USE_PGO" = 1 ]; then
    CAUSE=()
    [ "$BOLT_APPLIED" = 1 ] || CAUSE=(--bolt-cause "${BOLT_CAUSE:-BOLT not requested}")
    RC=0
    run_timed 120 "$COMPARE_BIN" compare --dir "$AB_DIR" --out "$RECEIPT" --runs "$RUNS" \
        --bolt-applied "$BOLT_APPLIED" ${CAUSE[@]+"${CAUSE[@]}"} || RC=$?
    [ "$RC" -eq 0 ] || FAILS+=(verdict)
    finish || exit 1
    [ "$RC" -eq 0 ] || exit 1
else
    echo "PLAIN-only diagnostic run; no PGO/BOLT verdict."
    finish
fi
