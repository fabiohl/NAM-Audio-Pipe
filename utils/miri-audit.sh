#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# miri-audit.sh — optional Miri concurrency/aliasing audit (RecordingPool + DspBridge). Classe D.
# Uso: utils/miri-audit.sh [-h|--help]
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    echo "Uso: utils/miri-audit.sh"
    exit 0
fi

echo "Miri concurrency & UB audit gate"
echo "  TARGET: recording::pool::pool_test (RecordingPool SPSC ring, 11/11)"
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

cargo +nightly miri --version
# A failed sysroot build invalidates the whole audit: never ignore it.
CARGO_BUILD_WARNINGS=allow cargo +nightly miri setup

# -Zmiri-disable-isolation: allow host clock / pseudo-sleeps.
export MIRIFLAGS="${MIRIFLAGS:--Zmiri-disable-isolation}"

echo "Target: RecordingPool SPSC lock-free ring (pool_test)..."
MIRI_LOG=$(mktemp)
BRIDGE_MIRI_LOG=$(mktemp)
trap 'rm -f "$MIRI_LOG" "$BRIDGE_MIRI_LOG"' EXIT
cargo +nightly miri test --lib recording::pool::pool_test 2>&1 | tee "$MIRI_LOG"
assert_ran_tests "$MIRI_LOG" 11

echo ""
echo "Target: DspBridge concurrent handshake (pw_host_test)..."
cargo +nightly miri test --lib standalone::pw_host::pw_host_test 2>&1 | tee "$BRIDGE_MIRI_LOG"
assert_ran_tests "$BRIDGE_MIRI_LOG" 1

ok "Miri concurrency and aliasing audit completed successfully (pool 11/11 + bridge handshake)!"
