#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Standard quality control and static analysis script for nam-audio-pipe.
# Runs all cargo checks first (fmt, check, clippy) covering the maximum
# feature spectrum dynamically, followed by static textual checks.
#
# Dynamic feature matrix (broad and resilient to Cargo.toml changes):
#   All Features (catch-all) : --all-targets --all-features
#   No Default Features      : --all-targets --no-default-features

set -euo pipefail

PHASE_TOTAL=7
source "$(dirname "$0")/_lib.sh"

echo -e "${BLUE}${BOLD}========================================${NC}"
echo -e "${BLUE}${BOLD} NAM-Audio-Pipe Linting & Quality Suite${NC}"
echo -e "${BLUE}${BOLD}========================================${NC}"

# ---------------------------------------------------------------------------
# [1/7] Code formatting check (cargo fmt --check) — strictly read-only: the
# lint gate must never mutate the worktree; a style divergence fails the gate.
# ---------------------------------------------------------------------------
phase "Checking code formatting (cargo fmt --all -- --check)..."
cargo fmt --all -- --check

# ---------------------------------------------------------------------------
# [2/7] Compilation checks (cargo check) — broad feature matrix
# ---------------------------------------------------------------------------
phase "Executing compilation checks (cargo check)..."

echo -e "  ${YELLOW}${BOLD}Checking: All Targets + All Features (broad catch-all)...${NC}"
cargo check --all-targets --all-features

echo -e "  ${YELLOW}${BOLD}Checking: All Targets (no default features)...${NC}"
cargo check --all-targets --no-default-features

# ---------------------------------------------------------------------------
# [3/7] Static analysis (cargo clippy) — strict, broad feature matrix
# ---------------------------------------------------------------------------
phase "Executing strict static analysis (cargo clippy)..."

echo -e "  ${YELLOW}${BOLD}Clippy: All Targets + All Features (broad catch-all)...${NC}"
cargo clippy --all-targets --all-features -- -D warnings

echo -e "  ${YELLOW}${BOLD}Clippy: All Targets (no default features)...${NC}"
cargo clippy --all-targets --no-default-features -- -D warnings

# ---------------------------------------------------------------------------
# [4/7] SPDX license header validation (deterministic, no external tooling)
# ---------------------------------------------------------------------------
phase "Validating SPDX license headers..."

# Build the list of directories to search as an array to avoid fragile word
# splitting inside command substitution when benches/ is absent.
rs_dirs=( src tests )
[ -d benches ] && rs_dirs+=( benches )

# Enumeration is fail-closed: with `set -euo pipefail` and no `|| true`, any
# failure in the find/test commands aborts the gate instead of silently
# producing an empty (or partial) scope.
spdx_scope=$(
    {
        find "${rs_dirs[@]}" -type f -name '*.rs'
        find utils -maxdepth 1 -type f -name '*.sh'
        test -d packaging && find packaging -type f \( -name '*.py' -o -name '*.sh' \)
        test -f build.rs && echo build.rs
        test -f Cargo.toml && echo Cargo.toml
    }
)

missing=$(printf '%s\n' "$spdx_scope" | xargs grep -L "SPDX-License-Identifier" 2>/dev/null || true)
if [ -n "$missing" ]; then
    echo -e "  ${RED}${BOLD}Missing SPDX header in files:${NC}"
    echo "$missing" | sed 's/^/    /'
    exit 1
fi
invalid=$(printf '%s\n' "$spdx_scope" \
    | xargs grep -l "SPDX-License-Identifier" 2>/dev/null \
    | xargs grep -LE "SPDX-License-Identifier: (GPL-3\.0-or-later|MIT)" 2>/dev/null || true)
if [ -n "$invalid" ]; then
    echo -e "  ${RED}${BOLD}Invalid SPDX identifier (expected GPL-3.0-or-later or MIT):${NC}"
    echo "$invalid" | sed 's/^/    /'
    exit 1
fi
ok "All files have valid SPDX headers (GPL-3.0-or-later, MIT)."

# ---------------------------------------------------------------------------
# [5/7] Undocumented #[allow(clippy::)] check (enforce allow_attributes policy)
#
# The project sets `allow_attributes = "warn"` in [lints.clippy], meaning every
# #[allow(clippy::...)] must carry a justification comment immediately above it
# (using the standard `// REASON:` or `// #[allow]` comment convention).
# A bare #[allow(clippy::...)] with no preceding comment is flagged here as a
# policy violation to keep lint suppressions auditable.
# ---------------------------------------------------------------------------
phase "Checking for undocumented #[allow(clippy::)] suppressions..."

undocumented_allows=""
while IFS= read -r rs_file; do
    # Read the file line by line, tracking whether the previous non-blank line
    # was a comment. Flag any #[allow(clippy:: line whose preceding non-blank
    # line is not a comment (// or #).
    prev_was_comment=false
    while IFS= read -r line; do
        trimmed="${line#"${line%%[! ]*}"}"   # lstrip whitespace
        if [[ "$trimmed" =~ ^\#\[allow\(clippy:: ]]; then
            if ! $prev_was_comment; then
                undocumented_allows+="$rs_file: $trimmed"$'\n'
            fi
            prev_was_comment=false
        elif [[ "$trimmed" =~ ^//|^# ]]; then
            prev_was_comment=true
        elif [ -n "$trimmed" ]; then
            prev_was_comment=false
        fi
        # blank lines do not reset the comment flag (allow blank separator between
        # comment and attribute)
    done < "$rs_file"
done < <(printf '%s\n' "$spdx_scope" | grep '\.rs$')

if [ -n "$undocumented_allows" ]; then
    echo -e "  ${RED}${BOLD}ERROR: Undocumented #[allow(clippy::)] found (add a justification comment above):${NC}"
    echo "$undocumented_allows" | sed 's/^/    /'
    exit 1
fi
ok "All #[allow(clippy::)] suppressions are documented."

# ---------------------------------------------------------------------------
# [6/7] AppStream packaging metadata validation (mandatory, fail-closed)
# ---------------------------------------------------------------------------
phase "Validating AppStream packaging metadata (mandatory files + official validators)..."

packaging_dir="packaging/flatpak"
metainfo_file="$packaging_dir/io.github.fabiohl.NAMAudioPipe.metainfo.xml"
desktop_file="$packaging_dir/io.github.fabiohl.NAMAudioPipe.desktop"
icon_theme_dir="$packaging_dir/icons/hicolor"

# Presence of the packaging metadata is mandatory: a Flatpak-ready release
# requires the AppStream metainfo, the desktop launcher and the hicolor icon
# theme. Any missing file aborts the lint gate (fail-closed).
missing_pkg=0
for pkg_path in "$metainfo_file" "$desktop_file" "$icon_theme_dir"; do
    if [ ! -e "$pkg_path" ]; then
        echo -e "  ${RED}${BOLD}ERROR: Mandatory packaging file/dir missing: $pkg_path${NC}"
        missing_pkg=1
    fi
done
if [ "$missing_pkg" -eq 1 ]; then
    die "Packaging metadata is incomplete; Flatpak distribution requires metainfo, desktop entry and hicolor icons."
fi
ok "Mandatory packaging files present."

command -v appstreamcli >/dev/null 2>&1 \
    || die "appstreamcli is required to validate AppStream metadata (install appstream-compose/appstream-util)."
command -v desktop-file-validate >/dev/null 2>&1 \
    || die "desktop-file-validate is required to validate the .desktop launcher (install desktop-file-utils)."

echo -e "  ${YELLOW}${BOLD}appstreamcli validate --no-net --strict $metainfo_file${NC}"
if ! appstreamcli validate --no-net --strict "$metainfo_file"; then
    die "AppStream metainfo validation failed (structural or semantic errors, see output above)."
fi
ok "appstreamcli validation passed with zero errors/warnings."

echo -e "  ${YELLOW}${BOLD}desktop-file-validate $desktop_file${NC}"
if ! desktop-file-validate "$desktop_file"; then
    die "Desktop entry validation failed (see output above)."
fi
ok "Desktop entry validation passed."

# Semantic alignment: <id>, <name> and <launchable> must match the desktop file.
desktop_id="$(basename "$desktop_file")"
desktop_name="$(grep -m1 '^Name=' "$desktop_file" | cut -d= -f2-)"
meta_id="$(grep -m1 -oP '(?<=<id>)[^<]+' "$metainfo_file")"
meta_launchable="$(grep -m1 -oP '(?<=<launchable type="desktop-id">)[^<]+' "$metainfo_file")"
meta_name="$(grep -m1 -oP '(?<=<name>)[^<]+' "$metainfo_file")"

if [ "$meta_id" != "$desktop_id" ]; then
    die "Metainfo <id> ($meta_id) does not match desktop file ID ($desktop_id)."
fi
if [ "$meta_launchable" != "$desktop_id" ]; then
    die "Metainfo <launchable desktop-id> ($meta_launchable) does not match desktop file ID ($desktop_id)."
fi
if [ "$meta_name" != "$desktop_name" ]; then
    die "Metainfo <name> ($meta_name) does not match desktop Name= ($desktop_name)."
fi
ok "Metainfo id/name/launchable aligned with desktop entry."

# ---------------------------------------------------------------------------
# [7/7] AppStream release version sync check
# ---------------------------------------------------------------------------
phase "Checking AppStream release version sync with Cargo.toml..."

cargo_ver=$(grep -m1 '^version = ' Cargo.toml | cut -d '"' -f2)
# Extract version and date from the most recent <release> entry.
release_line=$(grep -m1 -oP '<release version="[^"]+"[^>]*>' "$metainfo_file")
if [ -z "$release_line" ]; then
    die "No <release version=\"...\"> entry found in $metainfo_file."
fi
xml_ver=$(printf '%s' "$release_line" | sed -E 's/.*version="([^"]+)".*/\1/')
release_date=$(printf '%s' "$release_line" | sed -nE 's/.*date="([^"]+)".*/\1/p')

if [ -z "$xml_ver" ]; then
    die "Could not extract version from <release> in $metainfo_file."
fi
if [ "$cargo_ver" != "$xml_ver" ]; then
    echo -e "  ${RED}${BOLD}ERROR: Version mismatch between Cargo.toml ($cargo_ver) and $metainfo_file ($xml_ver)!${NC}"
    exit 1
fi
if [ -z "$release_date" ]; then
    echo -e "  ${RED}${BOLD}ERROR: AppStream release $xml_ver is missing the mandatory date attribute!${NC}"
    exit 1
fi
ok "AppStream release $xml_ver ($release_date) matches Cargo.toml ($cargo_ver)."

echo -e "${GREEN}${BOLD}=======================================${NC}"
echo -e "${GREEN}${BOLD} Quality suite completed successfully!${NC}"
echo -e "${GREEN}${BOLD}=======================================${NC}"
