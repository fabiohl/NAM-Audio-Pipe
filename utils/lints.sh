#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# lints.sh — formatação, análise estática e conformidade textual. Classe A.
# Uso: utils/lints.sh [-h|--help]
# Orçamento: 100
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh
case "${1:-}" in "") ;; -h|--help) echo "Uso: utils/lints.sh"; exit 0 ;; *) echo "RESULT: FAIL (uso)"; exit 2 ;; esac
trap 'emit "RESULT: FAIL ($((SECONDS - T0))s) fails=${PHASE_NAME:-step} gaps=none"' ERR
run_step() {
  local name=$1; shift; PHASE_NAME=$name; phase "$name"
  local log="$LOG_DIR/lints-$name.log" rc
  set +e; ( set -e; "$@" ) 2>&1 | tee "$log"; rc=${PIPESTATUS[0]}; set -e
  [ "$rc" -eq 0 ] || { FAILS+=("$name"); emit "PHASE: $name FAIL rc=$rc log=$log"; finish || true; exit 1; }
  emit "PHASE: $name PASS log=$log"
}

# N-5: formatting is applied locally; CI only verifies it.
phase_fmt() {
  if [ "${CI:-}" = "true" ]; then cargo fmt --all -- --check; else cargo fmt --all && git diff --stat; fi
}
run_step fmt phase_fmt

phase_check() {
  cargo check --locked --all-targets --all-features
  cargo check --locked --lib --no-default-features
  cargo check --locked --all-targets --no-default-features
}
run_step check phase_check

phase_clippy() {
  cargo clippy --locked --all-targets --all-features -- -D warnings
  cargo clippy --locked --lib --no-default-features -- -D warnings
  cargo clippy --locked --all-targets --no-default-features -- -D warnings
}
run_step clippy phase_clippy
phase_doc() { RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features; }
run_step doc phase_doc

phase_spdx() {
  local files=() missing=() f rc
  mapfile -t files < <(git ls-files '*.rs' '*.sh' '*.py' 'build.rs' 'Cargo.toml')
  for f in "${files[@]}"; do
    grep -qE "SPDX-License-Identifier: (GPL-3\.0-or-later|MIT)" "$f"; rc=$?
    [ "$rc" -eq 0 ] && continue
    [ "$rc" -eq 1 ] || { echo "SPDX scan failed: $f" >&2; return 1; }
    missing+=("$f")
  done
  [ ${#missing[@]} -eq 0 ] || { echo "Missing or invalid SPDX header:" >&2; printf '  %s\n' "${missing[@]}" >&2; return 1; }
}
run_step spdx phase_spdx

# Suppressions need a justification comment; the global log level is centralized
# in test_logger (a direct set_max_level races with parallel tests).
phase_policies() {
  local bad
  bad=$(git ls-files '*.rs' | xargs awk '
    FNR == 1 { ok = 0 }
    /^[[:space:]]*#\[allow\(clippy::/ { if (!ok) print FILENAME ":" FNR ": " $0; ok = 0; next }
    /^[[:space:]]*(\/\/|\/\*)/ { ok = 1; next }
    /^[[:space:]]*$/ { next }
    { ok = 0 }')
  if [ -n "$bad" ]; then
    echo "Undocumented #[allow(clippy::)] (add a justification comment above):" >&2
    echo "$bad" >&2; return 1
  fi
  bad=$(grep -rnE '\bset_max_level\s*\(' src tests 2>/dev/null \
    | grep -vE ':[0-9]+:\s*(//|/\*|\*)' \
    | grep -v 'src/standalone/test_logger.rs:' \
    | grep -v 'src/main.rs:' || true)
  if [ -n "$bad" ]; then
    echo "Unauthorized log::set_max_level (use test_logger::with_logger_level):" >&2
    echo "$bad" >&2; return 1
  fi
}
run_step policies phase_policies

# Product metadata is validated by the official AppStream/desktop tools; the
# crate/product tests additionally enforce well-formedness and version sync.
phase_packaging() {
  local d=packaging/flatpak
  local m="$d/io.github.fabiohl.NAMAudioPipe.metainfo.xml"
  local x="$d/io.github.fabiohl.NAMAudioPipe.desktop"
  local p
  for p in "$m" "$x" "$d/icons/hicolor"; do
    [ -e "$p" ] || { echo "Missing mandatory packaging path: $p" >&2; return 1; }
  done
  command -v appstreamcli >/dev/null 2>&1 || { echo "appstreamcli is required (appstream package)" >&2; return 1; }
  command -v desktop-file-validate >/dev/null 2>&1 || { echo "desktop-file-validate is required (desktop-file-utils)" >&2; return 1; }
  appstreamcli validate --no-net --strict "$m"
  desktop-file-validate "$x"
}
run_step packaging phase_packaging
phase_script_budget() { local f b; for f in $(find utils -name '*.sh'); do b=$(sed -n 's/^# Orçamento: *\([0-9]*\).*/\1/p;T;q' "$f"); [ -n "$b" ] && [ "$(wc -l < "$f")" -le "$b" ] || { echo "ORÇAMENTO: $f teto=${b:-AUSENTE} linhas=$(wc -l < "$f")" >&2; return 1; }; done; }
run_step script-budget phase_script_budget

finish
