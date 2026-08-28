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

# T5.2 / F-RB-015: every stage tracks a typed status (UPDATED,
# SKIPPED_TOOL_MISSING or FAILED) so a partial/failed update can never be
# declared as "Toda a cadeia de suprimentos foi atualizada!".
STATUS_RUSTUP="PENDING"
STATUS_CARGO_UPGRADE="PENDING"
STATUS_CARGO_UPDATE="PENDING"

# 1. Update Rust Toolchain
phase "Atualizando a toolchain ativa do Rust (rustup)..."
if command -v rustup &>/dev/null; then
    if rustup update; then
        STATUS_RUSTUP="UPDATED"
        ok "rustup toolchain atualizada."
    else
        STATUS_RUSTUP="FAILED"
        echo -e "${RED}${BOLD}Erro: rustup update falhou.${NC}" >&2
    fi
else
    STATUS_RUSTUP="SKIPPED_TOOL_MISSING"
    echo -e "${YELLOW}Aviso: rustup não encontrado. Pulando atualização da toolchain.${NC}"
fi

# 2. Upgrade dependencies in Cargo.toml
phase "Atualizando definições de dependências (Cargo.toml)..."
if cargo --list | grep -q "upgrade"; then
    if cargo upgrade --verbose; then
        STATUS_CARGO_UPGRADE="UPDATED"
        ok "Cargo.toml atualizado via cargo-upgrade."
    else
        STATUS_CARGO_UPGRADE="FAILED"
        echo -e "${RED}${BOLD}Erro: cargo upgrade falhou.${NC}" >&2
    fi
else
    STATUS_CARGO_UPGRADE="SKIPPED_TOOL_MISSING"
    echo -e "${YELLOW}Aviso: cargo-edit (cargo-upgrade) não encontrado.${NC}"
    echo -e "${YELLOW}Instale com: cargo install cargo-edit${NC}"
fi

# 3. Update Cargo.lock
phase "Atualizando versões resolvidas no Cargo.lock..."
if cargo update --verbose; then
    STATUS_CARGO_UPDATE="UPDATED"
    ok "Cargo.lock atualizado."
else
    STATUS_CARGO_UPDATE="FAILED"
    echo -e "${RED}${BOLD}Erro: cargo update falhou.${NC}" >&2
fi

# ---------------------------------------------------------------------------
# Typed summary + fail-closed exit policy
# ---------------------------------------------------------------------------
echo ""
echo -e "${BLUE}${BOLD}Resumo da cadeia de suprimentos:${NC}"
printf '  %-22s %s\n' "Estágio" "Status"
printf '  %-22s %s\n' "-------" "------"
printf '  %-22s %s\n' "rustup" "$STATUS_RUSTUP"
printf '  %-22s %s\n' "cargo-upgrade" "$STATUS_CARGO_UPGRADE"
printf '  %-22s %s\n' "cargo-update" "$STATUS_CARGO_UPDATE"

if [ "$STATUS_RUSTUP" = "FAILED" ] \
    || [ "$STATUS_CARGO_UPGRADE" = "FAILED" ] \
    || [ "$STATUS_CARGO_UPDATE" = "FAILED" ]; then
    echo -e "${RED}${BOLD}================================================================${NC}"
    echo -e "${RED}${BOLD}  Falha na atualização da cadeia de suprimentos (veja o resumo)  ${NC}"
    echo -e "${RED}${BOLD}================================================================${NC}"
    exit 1
fi

if [ "$STATUS_RUSTUP" != "UPDATED" ] \
    || [ "$STATUS_CARGO_UPGRADE" != "UPDATED" ] \
    || [ "$STATUS_CARGO_UPDATE" != "UPDATED" ]; then
    echo -e "${YELLOW}${BOLD}================================================================${NC}"
    echo -e "${YELLOW}${BOLD}  Cadeia de suprimentos parcialmente atualizada (etapas puladas)${NC}"
    echo -e "${YELLOW}${BOLD}================================================================${NC}"
    exit 0
fi

echo -e "${GREEN}${BOLD}================================================================${NC}"
echo -e "${GREEN}${BOLD}          Toda a cadeia de suprimentos foi atualizada!          ${NC}"
echo -e "${GREEN}${BOLD}================================================================${NC}"
