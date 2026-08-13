# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.

# _lib.sh — Common bash utilities for nam-audio-pipe scripts.
#
# Source with:
#   PHASE_TOTAL=<N>; source "$(dirname "$0")/_lib.sh"
# or for scripts that manage their own working directory (e.g. build-release.sh):
#   NAM_LIB_NO_CD=1 PHASE_TOTAL=<N>; source "$(dirname "$0")/_lib.sh"
#
# Then call:
#   phase "Description of the current step"
#   die  "Fatal error message"    # prints to stderr and exits 1
#   ok   "Success message"        # prints indented green OK line
#   warn "Warning message"        # prints indented yellow notice

# ---------------------------------------------------------------------------
# ANSI style helpers
# ---------------------------------------------------------------------------
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
BOLD='\033[1m'
NC='\033[0m'

# ---------------------------------------------------------------------------
# Phase counter (global — not safe for concurrent subshells)
# ---------------------------------------------------------------------------
PHASE_NUM=0

# phase <description>
#   Increments and prints a phase header: [N/TOTAL] description
phase() {
    PHASE_NUM=$((PHASE_NUM + 1))
    echo -e "\n${BLUE}${BOLD}[${PHASE_NUM}/${PHASE_TOTAL:-?}]${NC} $*"
}

# die <message>
#   Prints a fatal error to stderr and exits with code 1.
die() {
    echo -e "${RED}${BOLD}[FATAL]${NC} $*" >&2
    exit 1
}

# ok <message>
#   Prints an indented green OK confirmation line.
ok() {
    echo -e "  ${GREEN}OK${NC} $*"
}

# warn <message>
#   Prints an indented yellow informational/warning line.
warn() {
    echo -e "  ${YELLOW}ⓘ${NC} $*"
}

# ---------------------------------------------------------------------------
# Resolve project root dynamically relative to this helper script.
# ---------------------------------------------------------------------------
LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$LIB_DIR")"

if [ -z "$PROJECT_DIR" ]; then
    echo -e "${RED}${BOLD}[FATAL]${NC} _lib.sh: could not resolve PROJECT_DIR." >&2
    exit 1
fi

# Automatically enter the project root directory, unless the caller has set
# NAM_LIB_NO_CD=1 (used by scripts that manage their own working directory,
# such as build-release.sh which sets up its own cd before sourcing this lib).
if [ "${NAM_LIB_NO_CD:-0}" != "1" ]; then
    cd "$PROJECT_DIR" || {
        echo -e "${RED}${BOLD}[FATAL]${NC} _lib.sh: failed to cd into project root: $PROJECT_DIR" >&2
        exit 1
    }
fi
