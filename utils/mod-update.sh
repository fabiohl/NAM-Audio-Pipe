#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# mod-update.sh — atualiza toolchain, dependências e lockfile. Classe D.
# Uso: utils/mod-update.sh [-h|--help]
# Orçamento: 40
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh
case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/mod-update.sh"; exit 0 ;; *) echo "RESULT: FAIL (uso)"; exit 2 ;; esac

phase rustup
if command -v rustup >/dev/null 2>&1; then rustup update || FAILS+=(rustup)
else GAPS+=(rustup_missing); echo "PHASE: rustup GAP rustup_missing"; fi

phase cargo-upgrade
if command -v cargo-upgrade >/dev/null 2>&1; then cargo upgrade --verbose || FAILS+=(cargo-upgrade)
else GAPS+=(cargo_edit_missing); echo "PHASE: cargo-upgrade GAP cargo_edit_missing"; fi

phase cargo-update
cargo update --verbose || FAILS+=(cargo-update)

finish
