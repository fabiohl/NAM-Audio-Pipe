#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Out-of-band Miri Concurrency & Aliasing Audit Script for NAM-Audio-Pipe.
#
# GOVERNANCE & POLICY NOTICE:
#   In accordance with the project compliance policy (rules/testing.md §2),
#   all CI and standard release verification gates (lints.sh, tests-quick.sh,
#   tests-long.sh) use strictly the stable Rust toolchain.
#
#   This script is an OPTIONAL, out-of-tree periodic audit tool for developers
#   investigating unsafe invariants, data races, and Stacked/Tree Borrows aliasing
#   soundness under the Miri MIR interpreter.
#
# SCOPE & TARGET MATRIX:
#   - TARGET: `recording::pool::pool_test` (RecordingPool SPSC ring & descriptors)
#     Soundness proof for lock-free slot reuse, atomic barriers, and drop leaks (11/11 pass under Miri).
#   - TARGET: `standalone::pw_host::pw_host_test` (DspBridge concurrent access)
#     Interior-mutability narrowing landed upstream: NeuralAmpModeler-rs 1.0.0 wraps
#     `DspBridge::buffers` in `UnsafeCell` and the writer/reader never form a whole-struct
#     `&DspBridge` reference, so the concurrent writer/reader handshake is now auditable.
#     Waiver `F-MIRI-BRIDGE-01` is RESOLVED.
#   - EXCLUDED: `rt_callback/harness.rs` (Full offline DSP pipeline)
#     Excluded from full-pipeline Miri interpretation due to x86-64-v3 AVX2/FMA
#     vector intrinsics and severe simulation overhead (~10,000x slowdown).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/_lib.sh"

echo -e "${BLUE}${BOLD}====================================================${NC}"
echo -e "${BLUE}${BOLD}   NAM-Audio-Pipe Miri Concurrency & UB Audit Gate  ${NC}"
echo -e "${BLUE}${BOLD}====================================================${NC}"

# Check for nightly toolchain and miri
if ! command -v rustup >/dev/null 2>&1; then
    warn "rustup not found. Miri requires a nightly Rust toolchain with the miri component."
    exit 0
fi

if ! rustup toolchain list | grep -q "nightly"; then
    warn "Nightly toolchain not installed."
    echo -e "  To run Miri audits on demand, install via:"
    echo -e "    ${BOLD}rustup toolchain install nightly --profile minimal${NC}"
    echo -e "    ${BOLD}rustup component add miri --toolchain nightly${NC}"
    exit 0
fi

if ! cargo +nightly miri --version >/dev/null 2>&1; then
    warn "Miri component not found on nightly toolchain."
    echo -e "  To install miri, run:"
    echo -e "    ${BOLD}rustup component add miri --toolchain nightly${NC}"
    exit 0
fi

echo -e "${GREEN}Miri environment detected:${NC}"
cargo +nightly miri --version
echo ""

# Ensure Miri sysroot is pre-compiled and cached. We override [build] warnings = "deny"
# from .cargo/config.toml to allow compilation warnings in the standard sysroot generator.
CARGO_BUILD_WARNINGS=allow cargo +nightly miri setup >/dev/null 2>&1 || true

# Configuration flags:
# -Zmiri-tree-borrows : modern aliasing model (or Stacked Borrows by default)
# -Zmiri-disable-isolation : allow host clock / pseudo-sleeps
export MIRIFLAGS="${MIRIFLAGS:--Zmiri-disable-isolation}"

echo -e "${YELLOW}${BOLD}Target: RecordingPool SPSC lock-free ring (pool_test)...${NC}"
MIRI_LOG=$(mktemp)
BRIDGE_MIRI_LOG=$(mktemp)
trap 'rm -f "$MIRI_LOG" "$BRIDGE_MIRI_LOG"' EXIT
cargo +nightly miri test --lib recording::pool::pool_test 2>&1 | tee "$MIRI_LOG"
assert_ran_tests "$MIRI_LOG" 11

echo ""
echo -e "${YELLOW}${BOLD}Target: DspBridge concurrent handshake (pw_host_test)...${NC}"
cargo +nightly miri test --lib standalone::pw_host::pw_host_test 2>&1 | tee "$BRIDGE_MIRI_LOG"
assert_ran_tests "$BRIDGE_MIRI_LOG" 1

echo ""
ok "Miri concurrency and aliasing audit completed successfully (pool 11/11 + bridge handshake)!"
