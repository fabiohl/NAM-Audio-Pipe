#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# miri-audit.sh — optional Miri concurrency/aliasing audit (RecordingPool + DspBridge). Classe D.
# Uso: utils/miri-audit.sh [-h|--help]
# Orçamento: 75
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/miri-audit.sh"; exit 0 ;; *) usage_fail "miri-audit.sh aceita apenas -h/--help" ;; esac
lib_init
trap 'trap_fail $?' ERR

echo "Miri concurrency & UB audit gate"
echo "  TARGET: recording::pool::pool_test (RecordingPool SPSC ring, 13/13)"
echo "  TARGET: standalone::pw_host::pw_host_test (DspBridge handshake)"
echo "  EXCLUDED: rt_callback/harness.rs (AVX2/FMA intrinsics, ~10,000x slowdown)"

# Fail-closed prerequisites: an unavailable toolchain is a typed usage
# precondition (exit 2), never a silent success.
command -v rustup >/dev/null 2>&1 || {
    echo "MIRI: rustup not found — install a nightly toolchain with the miri component." >&2
    exit 2
}
rustup toolchain list 2>/dev/null | grep -q "nightly" || {
    echo "MIRI: no nightly toolchain — rustup toolchain install nightly --profile minimal" >&2
    exit 2
}
cargo +nightly miri --version >/dev/null 2>&1 || {
    echo "MIRI: component missing — rustup component add miri --toolchain nightly" >&2
    exit 2
}

PHASE_NAME=preflight; CUR_LOG="$LOG_DIR/miri-audit-preflight.log"; phase preflight
# A failed sysroot build invalidates the whole audit: never ignore it.
CARGO_BUILD_WARNINGS=allow cargo +nightly miri setup >"$CUR_LOG" 2>&1
emit "PHASE: preflight PASS log=$CUR_LOG"

# -Zmiri-disable-isolation: allow host clock / pseudo-sleeps.
export MIRIFLAGS="${MIRIFLAGS:--Zmiri-disable-isolation}"

PHASE_NAME=pool; CUR_LOG="$LOG_DIR/miri-audit-pool.log"; phase pool
cargo +nightly miri test --lib recording::pool::pool_test 2>&1 | tee "$CUR_LOG"
assert_ran_tests "$CUR_LOG" 13

PHASE_NAME=bridge; CUR_LOG="$LOG_DIR/miri-audit-bridge.log"; phase bridge
cargo +nightly miri test --lib standalone::pw_host::pw_host_test 2>&1 | tee "$CUR_LOG"
assert_ran_tests "$CUR_LOG" 1

ok "Miri concurrency and aliasing audit completed successfully (pool 13/13 + bridge handshake)!"
finish || exit 1
