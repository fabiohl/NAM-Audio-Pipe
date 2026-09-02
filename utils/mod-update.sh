#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Supply chain update utility for NAM-Audio-Pipe.
# Updates the Rust toolchain, Cargo package indexes, and dependencies in Cargo.toml/Cargo.lock.

set -euo pipefail

PHASE_TOTAL=3
source "$(dirname "$0")/_lib.sh"

echo -e "${BLUE}${BOLD}================================================================${NC}"
echo -e "${BLUE}${BOLD}      NAM-Audio-Pipe Supply Chain Update Pipeline               ${NC}"
echo -e "${BLUE}${BOLD}================================================================${NC}"

# Every stage tracks a typed status (UPDATED, SKIPPED_TOOL_MISSING or FAILED)
# so a partial/failed update can never be falsely declared as fully updated.
STATUS_RUSTUP="PENDING"
STATUS_CARGO_UPGRADE="PENDING"
STATUS_CARGO_UPDATE="PENDING"

# 1. Update Rust Toolchain
phase "Updating active Rust toolchain (rustup)..."
if command -v rustup &>/dev/null; then
    if rustup update; then
        STATUS_RUSTUP="UPDATED"
        ok "rustup toolchain updated."
    else
        STATUS_RUSTUP="FAILED"
        echo -e "${RED}${BOLD}Error: rustup update failed.${NC}" >&2
    fi
else
    STATUS_RUSTUP="SKIPPED_TOOL_MISSING"
    echo -e "${YELLOW}Notice: rustup not found. Skipping toolchain update.${NC}"
fi

# 2. Upgrade dependencies in Cargo.toml
phase "Updating dependency specifications (Cargo.toml)..."
if cargo --list | grep -q "upgrade"; then
    if cargo upgrade --verbose; then
        STATUS_CARGO_UPGRADE="UPDATED"
        ok "Cargo.toml updated via cargo-upgrade."
    else
        STATUS_CARGO_UPGRADE="FAILED"
        echo -e "${RED}${BOLD}Error: cargo upgrade failed.${NC}" >&2
    fi
else
    STATUS_CARGO_UPGRADE="SKIPPED_TOOL_MISSING"
    echo -e "${YELLOW}Notice: cargo-edit (cargo-upgrade) not found.${NC}"
    echo -e "${YELLOW}Install with: cargo install cargo-edit${NC}"
fi

# 3. Update Cargo.lock
phase "Updating resolved versions in Cargo.lock..."
if cargo update --verbose; then
    STATUS_CARGO_UPDATE="UPDATED"
    ok "Cargo.lock updated."
else
    STATUS_CARGO_UPDATE="FAILED"
    echo -e "${RED}${BOLD}Error: cargo update failed.${NC}" >&2
fi

# ---------------------------------------------------------------------------
# Typed summary + fail-closed exit policy
# ---------------------------------------------------------------------------
echo ""
echo -e "${BLUE}${BOLD}Supply chain update summary:${NC}"
printf '  %-22s %s\n' "Stage" "Status"
printf '  %-22s %s\n' "-----" "------"
printf '  %-22s %s\n' "rustup" "$STATUS_RUSTUP"
printf '  %-22s %s\n' "cargo-upgrade" "$STATUS_CARGO_UPGRADE"
printf '  %-22s %s\n' "cargo-update" "$STATUS_CARGO_UPDATE"

if [ "$STATUS_RUSTUP" = "FAILED" ] \
    || [ "$STATUS_CARGO_UPGRADE" = "FAILED" ] \
    || [ "$STATUS_CARGO_UPDATE" = "FAILED" ]; then
    echo -e "${RED}${BOLD}================================================================${NC}"
    echo -e "${RED}${BOLD}       Supply chain update failed (see summary above)           ${NC}"
    echo -e "${RED}${BOLD}================================================================${NC}"
    exit 1
fi

if [ "$STATUS_RUSTUP" != "UPDATED" ] \
    || [ "$STATUS_CARGO_UPGRADE" != "UPDATED" ] \
    || [ "$STATUS_CARGO_UPDATE" != "UPDATED" ]; then
    echo -e "${YELLOW}${BOLD}================================================================${NC}"
    echo -e "${YELLOW}${BOLD}  Supply chain partially updated (some steps were skipped)      ${NC}"
    echo -e "${YELLOW}${BOLD}================================================================${NC}"
    exit 0
fi

echo -e "${GREEN}${BOLD}================================================================${NC}"
echo -e "${GREEN}${BOLD}          Entire supply chain was successfully updated!         ${NC}"
echo -e "${GREEN}${BOLD}================================================================${NC}"
