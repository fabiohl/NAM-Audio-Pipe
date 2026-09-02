#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Unified compiler-grade release build & packaging script for nam-audio-pipe (PGO + BOLT + Flatpak).
# Compiles the standalone binary with Profile-Guided Optimization (PGO),
# post-link BOLT binary reordering, and generates release distribution archives
# and standalone Flatpak application bundles.
#
# Deliverables:
#   - ~/.local/bin/nam-audio-pipe                     (PGO + BOLT optimized standalone binary)
#   - target/dsp_hotpath.asm                          (Disassembly hotspot report)
#   - target/logs/pgo-workload-receipt.json           (PGO workload coverage receipt)
#   - target/logs/release-receipt.json                (Release optimization status receipt)
#   - target/logs/release-provenance.json             (Cryptographic provenance receipt)
#   - ~/nam-audio-pipe-v<ver>-linux-x86_64-v3.tar.zst (Deterministic distribution tarball)
#   - ~/nam-audio-pipe-v<ver>-linux-x86_64-v3.flatpak (Flatpak application bundle)

set -euo pipefail

# Parse command line options
DO_INSTALL_FLATPAK=false
BUILD_FLATPAK=true
BUILD_TARBALL=true
USE_PGO=true
USE_BOLT=true
STRICT_RELEASE=false
RELEASE_CEREMONY=false

show_help() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Unified compiler-grade release build and packaging pipeline for nam-audio-pipe.

Options:
  --install              Automatically install the Flatpak bundle locally (flatpak install --user)
                         in addition to installing ~/.local/bin/nam-audio-pipe.
  --no-flatpak           Skip Phase 7 (Flatpak bundle creation). Required to run the release
                         pipeline when flatpak/appstreamcli are not available (otherwise the
                         missing tools abort the build fail-closed).
  --no-tarball           Skip Phase 6 (.tar.zst archive creation).
  --no-pgo               Skip Profile-Guided Optimization and compile directly with dist profile.
  --no-bolt              Skip Phase 4 (LLVM BOLT post-link optimization).
  --strict-release       Fail the release whenever the declared optimization cannot be proven:
                         BOLT failure/unavailability becomes fatal (no silent PGO-ONLY fallback)
                         and the release receipt must certify the applied optimization status.
  --release-ceremony     Official release ceremony mode: requires a pristine
                         git worktree (no modified/untracked files, Cargo.lock identical to HEAD,
                         coupled NeuralAmpModeler-rs tree clean), mandates --locked Cargo
                         resolution and requires the cryptographic provenance receipt
                         (release-provenance.json) to be emitted before the release is declared.
  -h, --help             Show this help message and exit.

Deliverables:
  - ~/.local/bin/nam-audio-pipe                     (Installed standalone binary)
  - target/dsp_hotpath.asm                          (Disassembly hotspot report)
  - target/logs/pgo-workload-receipt.json           (PGO workload coverage receipt)
  - target/logs/release-receipt.json                (Release optimization status receipt)
  - target/logs/release-provenance.json             (Cryptographic provenance receipt)
  - ~/nam-audio-pipe-v<ver>-linux-x86_64-v3.tar.zst (Deterministic distribution tarball)
  - ~/nam-audio-pipe-v<ver>-linux-x86_64-v3.flatpak (Flatpak application bundle)
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --install)
            DO_INSTALL_FLATPAK=true
            shift
            ;;
        --no-flatpak)
            BUILD_FLATPAK=false
            shift
            ;;
        --no-tarball)
            BUILD_TARBALL=false
            shift
            ;;
        --no-pgo)
            USE_PGO=false
            shift
            ;;
        --no-bolt)
            USE_BOLT=false
            shift
            ;;
        --strict-release)
            STRICT_RELEASE=true
            shift
            ;;
        --release-ceremony)
            RELEASE_CEREMONY=true
            STRICT_RELEASE=true
            shift
            ;;
        -h|--help)
            show_help
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            show_help
            exit 1
            ;;
    esac
done

# Resolve script location and enter project root before sourcing _lib.sh.
# NAM_LIB_NO_CD=1 disables the automatic cd inside _lib.sh so we retain full
# control over the working directory (required by this script's PGO/BOLT paths).
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR" || { echo "[FATAL] Cannot cd to project root: $PROJECT_DIR" >&2; exit 1; }
NAM_LIB_NO_CD=1 source "$SCRIPT_DIR/_lib.sh"

echo -e "${BLUE}${BOLD}========================================================================${NC}"
echo -e "${BLUE}${BOLD}   nam-audio-pipe Unified Release Build & Optimization Pipeline         ${NC}"
echo -e "${BLUE}${BOLD}========================================================================${NC}"

# State tracking for signal safety and cleanup
ORIG_PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")
PARANOID_MODIFIED=false
NAM_PID=""
PLAY_PID=""

# Dynamic isolated temporary directories for PGO & BOLT profiling and packaging
PGO_DIR="$(mktemp -d -t nam-audio-pipe-pgo.XXXXXX)"
BOLT_DIR="$(mktemp -d -t nam-audio-pipe-bolt.XXXXXX)"
PKG_DIR=""
FLATPAK_BUILD_DIR=""
FLATPAK_REPO_DIR=""
FLATPAK_SMOKE_REPO=""
STAGING_DIR=""
TARBALL_VERIFY_DIR=""
PROFRAW_DIR="$PGO_DIR/profraw"
MERGED_PROFILE="$PGO_DIR/merged.profdata"
ORIG_RUSTFLAGS="${RUSTFLAGS:-}"

# Structured receipts: PGO workload coverage + release optimization status.
PGO_RECEIPT="$PROJECT_DIR/target/logs/pgo-workload-receipt.json"
RELEASE_RECEIPT="$PROJECT_DIR/target/logs/release-receipt.json"
# Cryptographic chain of custody: release provenance receipt.
PROVENANCE_RECEIPT="$PROJECT_DIR/target/logs/release-provenance.json"

# Isolated target directory to avoid polluting standard build artifacts
PGO_BUILD_TARGET_DIR="$PROJECT_DIR/target/pgo-build"

# Clean prior target artifacts and initialize temporary directories
rm -rf "$PGO_BUILD_TARGET_DIR"
mkdir -p "$PROFRAW_DIR" "$BOLT_DIR" "$PROJECT_DIR/target"

# Signal handling and process/temporary file cleanup
cleanup() {
    if [ -n "$PLAY_PID" ] && kill -0 "$PLAY_PID" 2>/dev/null; then
        kill "$PLAY_PID" 2>/dev/null || true
        wait "$PLAY_PID" 2>/dev/null || true
    fi
    if [ -n "$NAM_PID" ] && kill -0 "$NAM_PID" 2>/dev/null; then
        kill "$NAM_PID" 2>/dev/null || true
        wait "$NAM_PID" 2>/dev/null || true
    fi
    if [ "${PARANOID_MODIFIED:-false}" = "true" ]; then
        echo -e "\nRestoring kernel.perf_event_paranoid to $ORIG_PARANOID..."
        sudo sysctl -q -w kernel.perf_event_paranoid="$ORIG_PARANOID" 2>/dev/null || true
    fi
    if [ -n "${PGO_DIR:-}" ] && [ -d "$PGO_DIR" ]; then rm -rf "$PGO_DIR"; fi
    if [ -n "${BOLT_DIR:-}" ] && [ -d "$BOLT_DIR" ]; then rm -rf "$BOLT_DIR"; fi
    if [ -n "${PKG_DIR:-}" ] && [ -d "$PKG_DIR" ]; then rm -rf "$PKG_DIR"; fi
    if [ -n "${FLATPAK_BUILD_DIR:-}" ] && [ -d "$FLATPAK_BUILD_DIR" ]; then rm -rf "$FLATPAK_BUILD_DIR"; fi
    if [ -n "${FLATPAK_REPO_DIR:-}" ] && [ -d "$FLATPAK_REPO_DIR" ]; then rm -rf "$FLATPAK_REPO_DIR"; fi
    if [ -n "${FLATPAK_SMOKE_REPO:-}" ] && [ -d "$FLATPAK_SMOKE_REPO" ]; then rm -rf "$FLATPAK_SMOKE_REPO"; fi
    if [ -n "${STAGING_DIR:-}" ] && [ -d "$STAGING_DIR" ]; then rm -rf "$STAGING_DIR"; fi
    if [ -n "${TARBALL_VERIFY_DIR:-}" ] && [ -d "$TARBALL_VERIFY_DIR" ]; then rm -rf "$TARBALL_VERIFY_DIR"; fi
    return 0
}

# ---------------------------------------------------------------------------
# Release ceremony gate: pristine-worktree requirement
# ---------------------------------------------------------------------------
# verify_clean_worktree
#   Official release ceremony check: the release must be built from a pristine
#   tree so the provenance receipt can bind the artifacts to an identifiable
#   clean commit. Any modified/untracked file in this repo — or a Cargo.lock
#   that diverges from HEAD — aborts immediately (fail-closed). The coupled
#   NeuralAmpModeler-rs tree is also part of the built artifact (patched path
#   dependency), so a dirty dependency tree invalidates the recorded commit.
verify_clean_worktree() {
    local porcelain nam_porcelain
    if ! git rev-parse --git-dir >/dev/null 2>&1; then
        die "Release ceremony requires a git repository to prove provenance, but $PROJECT_DIR is not one."
    fi
    porcelain="$(git status --porcelain || true)"
    if [ -n "$porcelain" ]; then
        echo -e "${RED}Error: release ceremony requires a clean worktree.${NC}" >&2
        printf '%s\n' "$porcelain" >&2
        die "Dirty worktree ($(printf '%s\n' "$porcelain" | grep -c .) changed/untracked path(s)); refusing to build an official release from an unidentifiable tree."
    fi
    if ! git diff --quiet HEAD -- Cargo.lock; then
        die "Cargo.lock diverges from HEAD; refusing to build an official release from an inconsistent lockfile."
    fi
    if [ -d "$PROJECT_DIR/../NeuralAmpModeler-rs/.git" ]; then
        nam_porcelain="$(git -C "$PROJECT_DIR/../NeuralAmpModeler-rs" status --porcelain || true)"
        if [ -n "$nam_porcelain" ]; then
            echo -e "${RED}Error: the coupled NeuralAmpModeler-rs worktree is dirty.${NC}" >&2
            printf '%s\n' "$nam_porcelain" >&2
            die "Dirty coupled NeuralAmpModeler-rs tree; the recorded dependency commit would not reproduce the built binary."
        fi
    fi
    echo -e "  ${GREEN}✓${NC} Worktree pristine: Cargo.lock identical to HEAD ($(git rev-parse --short HEAD)) and coupled NeuralAmpModeler-rs clean."
}
trap cleanup EXIT INT TERM HUP

# ---------------------------------------------------------------------------
# Release optimization receipt
# ---------------------------------------------------------------------------
# write_release_receipt <status> <cause>
#   Emits target/logs/release-receipt.json certifying the optimization applied
#   to the release binary, cross-referencing the PGO workload receipt. Status
#   is one of: PGO+BOLT, PGO-ONLY, BOLT-ONLY, PLAIN. `cause` is the detailed
#   BOLT failure/unavailability reason (empty when BOLT was proven/applied).
write_release_receipt() {
    local status="$1"
    local cause="${2:-}"
    mkdir -p "$PROJECT_DIR/target/logs"
    if python3 - "$status" "$cause" "$PGO_RECEIPT" > "$RELEASE_RECEIPT" <<'PY'
import json
import sys

status, cause, pgo_receipt = sys.argv[1], sys.argv[2], sys.argv[3]
doc = {
    "schema_version": 1,
    "tool": "build-release.sh",
    "optimization": {"status": status},
}
if cause:
    doc["optimization"]["cause"] = cause
doc["pgo"] = {"receipt": pgo_receipt}
try:
    with open(pgo_receipt, "r", encoding="utf-8") as f:
        r = json.load(f)
    doc["pgo"]["topology_blocks"] = r.get("topology_blocks")
    doc["pgo"]["oversampling_blocks"] = r.get("oversampling_blocks")
    doc["pgo"]["cabsim_frames"] = (r.get("cabsim") or {}).get("stereo_convolved_frames")
    doc["pgo"]["no_stage_skipped"] = r.get("no_stage_skipped")
    # T5.2: the matrix receipt declares per-topology minimum progress per DSP
    # group (G-PERF-003) — carried into the release receipt so the certification
    # never reduces the coverage to an aggregated global number.
    doc["pgo"]["progress"] = r.get("progress") or {}
    doc["pgo"]["coverage"] = r.get("coverage") or {}
    doc["pgo"]["matrix"] = r.get("matrix") or {}
except FileNotFoundError:
    doc["pgo"]["parse_error"] = "receipt not found"
except Exception as e:  # noqa: BLE001 - receipt is auxiliary evidence
    doc["pgo"]["parse_error"] = str(e)
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
    then
        echo -e "  ${GREEN}✓${NC} Release receipt written: $RELEASE_RECEIPT"
    else
        warn "Failed to write release receipt to $RELEASE_RECEIPT"
    fi
}

export CARGO_TARGET_DIR="$PGO_BUILD_TARGET_DIR"

# Extract rustflags from .cargo/config.toml using tomllib (or regex fallback)
CONFIG_RUSTFLAGS=$(python3 -c '
import sys
try:
    import tomllib
    with open(".cargo/config.toml", "rb") as f:
        data = tomllib.load(f)
    flags = data.get("build", {}).get("rustflags", [])
    if isinstance(flags, list) and flags:
        print(" ".join(flags))
        sys.exit(0)
except Exception:
    pass

import re
try:
    with open(".cargo/config.toml", "r") as f:
        content = f.read()
    match = re.search(r"rustflags\s*=\s*\[(.*?)\n\]", content, re.DOTALL)
    if match:
        block = match.group(1)
        flags = []
        for line in block.splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            flag_match = re.search(r"\"([^\"]+)\"", stripped)
            if flag_match:
                flags.append(flag_match.group(1))
        print(" ".join(flags))
except Exception:
    pass
' 2>/dev/null || echo "")

# Deliverable Targets
BIN_INSTALL_DIR="$HOME/.local/bin"
BIN_TARGET="$BIN_INSTALL_DIR/nam-audio-pipe"

# Official release ceremony: provenance can only bind artifacts to a clean,
# identifiable commit. Enforced before any build or receipt is produced.
if [ "$RELEASE_CEREMONY" = true ]; then
    echo -e "\n${BLUE}${BOLD}Verifying release ceremony prerequisites (pristine worktree, quick & long receipts)...${NC}"
    verify_clean_worktree

    echo -e "  → Executing quick QA suite in strict mode (NAM_QUICK_STRICT=1)..."
    NAM_QUICK_STRICT=1 "$SCRIPT_DIR/tests-quick.sh"

    LONG_RECEIPT_PATH="$PROJECT_DIR/target/logs/long-receipt.txt"
    if [ -f "$LONG_RECEIPT_PATH" ]; then
        echo -e "  → Verifying existing long suite receipt (semantic strict certification)..."
        # T5.1 / T8.1: the strict receipt is verified by the shared semantic
        # parser (src/bin/long_receipt_check.rs + nam_audio_pipe::receipt::long)
        # — never a substring search. The gate accepts only a real strict
        # passed run (SUITE: tests-long, STRICT: 1, NAM_RT_STRICT: 1, MODE:
        # full, OVERALL: PASSED); simulate/partial/legacy receipts are rejected
        # fail-closed.
        if ! cargo run --quiet --locked --bin long_receipt_check -- "$LONG_RECEIPT_PATH"; then
            die "Release ceremony requires a real, strict long suite receipt on disk (SUITE: tests-long, STRICT: 1, NAM_RT_STRICT: 1, MODE: full, OVERALL: PASSED).\nReceipt at target/logs/long-receipt.txt failed semantic verification (SIMULATED, STRICT: 0, COMPLETED_WITH_GAPS, FAILED, missing NAM_RT_STRICT propagation or simulate mode are strictly rejected)."
        fi
        echo -e "  ${GREEN}✓${NC} Real strict long suite receipt verified (STRICT: 1, NAM_RT_STRICT: 1, MODE: full, OVERALL: PASSED)."
    else
        die "Release ceremony requires a real, strict long suite receipt on disk (target/logs/long-receipt.txt is missing).\nPlease ask the human operator to execute:\n  ./utils/tests-long.sh --strict-pre-release\n(AI agents MUST NEVER execute the long suite directly)."
    fi
fi

# -----------------------------------------------------------------------------
# PHASE 1: Environment & Dependency Verification
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 1/7] Verifying dependencies and environment...${NC}"

# Verify core dependencies
REQUIRED_CMDS=(rustc cargo python3 tar zstd)
if [ "$BUILD_FLATPAK" = true ]; then
    REQUIRED_CMDS+=(flatpak appstreamcli)
fi

for cmd in "${REQUIRED_CMDS[@]}"; do
    if ! command -v "$cmd" &>/dev/null; then
        echo -e "${RED}Error: '$cmd' is not installed or available in PATH.${NC}"
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} '$cmd' found."
done

# Ensure non-empty rustflags were extracted from .cargo/config.toml
if [ -z "${CONFIG_RUSTFLAGS:-}" ]; then
    echo -e "${RED}Error: Could not extract rustflags from .cargo/config.toml or they are empty!${NC}"
    echo -e "${YELLOW}The release build requires optimizations like '-Ctarget-cpu=x86-64-v3'.${NC}"
    exit 1
fi
echo -e "  ${GREEN}✓${NC} rustflags from config.toml verified: ${BOLD}$CONFIG_RUSTFLAGS${NC}"

# Locate llvm-profdata from Rustup toolchain (if PGO is active)
LLVM_PROFDATA=""
if [ "$USE_PGO" = true ]; then
    RUST_SYSROOT="$(rustc --print sysroot)"
    RUST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
    LLVM_PROFDATA="$RUST_SYSROOT/lib/rustlib/$RUST_TARGET/bin/llvm-profdata"
    if [ ! -x "$LLVM_PROFDATA" ]; then
        echo -e "${RED}Error: llvm-profdata not found at $LLVM_PROFDATA${NC}"
        echo -e "${YELLOW}Install LLVM tools via rustup:${NC}"
        echo -e "  rustup component add llvm-tools-preview"
        exit 1
    fi
    echo -e "  ${GREEN}✓${NC} llvm-profdata found: $LLVM_PROFDATA"
fi

# Locate LLVM BOLT binary (if BOLT is active)
LLVM_BOLT=""
PERF2BOLT=""
HAS_PERF=false

if [ "$USE_BOLT" = true ]; then
    for candidate in \
        /usr/lib/llvm-22/bin/llvm-bolt \
        /usr/lib/llvm-21/bin/llvm-bolt \
        /usr/lib/llvm-20/bin/llvm-bolt \
        /usr/lib/llvm-19/bin/llvm-bolt \
        /usr/lib/llvm-18/bin/llvm-bolt \
        /usr/bin/llvm-bolt-22 \
        /usr/bin/llvm-bolt-21 \
        /usr/bin/llvm-bolt; do
        if [ -x "$candidate" ]; then
            LLVM_BOLT="$candidate"
            break
        fi
    done

    if [ -n "$LLVM_BOLT" ]; then
        echo -e "  ${GREEN}✓${NC} llvm-bolt found: $LLVM_BOLT"
        PERF2BOLT="$(dirname "$LLVM_BOLT")/perf2bolt"
        if [ ! -x "$PERF2BOLT" ]; then
            PERF2BOLT="perf2bolt"
        fi
    else
        echo -e "${YELLOW}Warning: llvm-bolt was not found. The build will continue with PGO only.${NC}"
        echo -e "${YELLOW}To enable BOLT, install: sudo apt install llvm-22-tools${NC}"
    fi

    # Check perf_event_paranoid requirement for BOLT profiling
    if [ "$ORIG_PARANOID" -gt 1 ]; then
        echo -e "  kernel.perf_event_paranoid is $ORIG_PARANOID. Attempting to set to 1..."
        if command -v sudo &>/dev/null; then
            if sudo -n sysctl -q -w kernel.perf_event_paranoid=1 2>/dev/null; then
                PARANOID_MODIFIED=true
                echo -e "  ${GREEN}✓${NC} paranoid level set to 1 (passwordless sudo)."
            elif [ -t 0 ]; then
                echo -e "${YELLOW}Warning: Passwordless sudo not available. Prompting for password...${NC}"
                if sudo sysctl -q -w kernel.perf_event_paranoid=1; then
                    PARANOID_MODIFIED=true
                    echo -e "  ${GREEN}✓${NC} paranoid level set to 1."
                else
                    warn "Failed to set paranoid level to 1. BOLT profiling might be skipped."
                fi
            else
                warn "Non-interactive shell and no passwordless sudo. BOLT profiling might be skipped."
            fi
        else
            warn "'sudo' command not found. BOLT profiling might be skipped."
        fi
    fi

    if command -v perf &>/dev/null; then
        PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")
        if [ "$PARANOID" -le 1 ]; then
            HAS_PERF=true
            echo -e "  ${GREEN}✓${NC} perf is available (paranoid level: $PARANOID)"
        else
            echo -e "${YELLOW}Warning: perf is installed but kernel.perf_event_paranoid=$PARANOID (>1).${NC}"
            echo -e "${YELLOW}BOLT requires paranoid <= 1 for unprivileged sampling.${NC}"
            echo -e "${YELLOW}Run: sudo sysctl -w kernel.perf_event_paranoid=1${NC}"
        fi
    else
        echo -e "${YELLOW}Warning: perf not found. The build will continue with PGO only.${NC}"
    fi
fi

# -----------------------------------------------------------------------------
# PHASE 2: Profile-Guided Optimization (PGO) - Offline DSP Workload
# -----------------------------------------------------------------------------
if [ "$USE_PGO" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 2/7] Generating PGO profiles via offline DSP workload...${NC}"

    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Cprofile-generate=$PROFRAW_DIR"
    export LLVM_PROFILE_FILE="$PROFRAW_DIR/default_%m_%p.profraw"
    echo -e "  Using RUSTFLAGS: ${BOLD}$RUSTFLAGS${NC}"

    echo -e "  Compiling and running PGO profiling workload (pgo_workload)..."
    cargo run --locked --profile dist --features testing --bin pgo_workload || {
        echo -e "${RED}Error: pgo_workload failed. Cannot generate PGO profiles.${NC}"
        exit 1
    }

    # Fail-closed PGO coverage gate: the workload receipt must
    # prove every mandatory DSP topology reached >= 1000 total blocks and >= 4 blocks per cell,
    # all oversampling modes ran, the stereo CabSim convolution executed and no stage was skipped.
    if [ ! -f "$PGO_RECEIPT" ]; then
        echo -e "${RED}Error: pgo_workload did not emit its receipt at $PGO_RECEIPT.${NC}"
        echo -e "${RED}The PGO profile cannot be certified as representative; aborting.${NC}"
        exit 1
    fi
    if ! python3 - "$PGO_RECEIPT" <<'PY'
import json
import sys

# T5.2 matrix gate (G-PERF-003): the receipt must prove per-topology minimum
# progress (frames) per DSP group — never an aggregated global number — and
# every matrix dimension value must have been exercised.
min_total_blocks = 1000
min_cell_blocks = 4
path = sys.argv[1]
try:
    with open(path, "r", encoding="utf-8") as f:
        receipt = json.load(f)
except Exception as e:
    print(f"[FATAL] cannot parse PGO receipt {path}: {e}", file=sys.stderr)
    sys.exit(1)

if receipt.get("schema_version") != 2:
    print(
        f"[FATAL] PGO receipt schema_version must be 2 (matrix), got "
        f"{receipt.get('schema_version')}.",
        file=sys.stderr,
    )
    sys.exit(1)

progress = receipt.get("progress") or {}
total_blocks_by_topo = receipt.get("topology_blocks") or (receipt.get("coverage") or {}).get("topologies") or {}
min_blocks_by_topo = progress.get("min_blocks_per_topology") or {}
min_frames_by_topo = progress.get("min_frames_per_topology") or {}
groups = progress.get("groups") or {}

for topology in ("wavenet_a1", "wavenet_a2", "lstm"):
    total = total_blocks_by_topo.get(topology, 0)
    if total < min_total_blocks:
        print(
            f"[FATAL] PGO receipt: topology '{topology}' total blocks = {total} "
            f"(required >= {min_total_blocks}); the profile is not representative.",
            file=sys.stderr,
        )
        sys.exit(1)
    min_cell = min_blocks_by_topo.get(topology, 0)
    if min_cell < min_cell_blocks:
        print(
            f"[FATAL] PGO receipt: topology '{topology}' min cell blocks = {min_cell} "
            f"(required >= {min_cell_blocks}); matrix coverage incomplete.",
            file=sys.stderr,
        )
        sys.exit(1)
    frames = min_frames_by_topo.get(topology, 0)
    if frames <= 0:
        print(
            f"[FATAL] PGO receipt: topology '{topology}' advanced 0 frames.",
            file=sys.stderr,
        )
        sys.exit(1)
    for group in ("resampler", "inference", "oversample", "cabsim", "bridge", "recording"):
        gmin = ((groups.get(group) or {}).get("min_frames_per_topology") or {}).get(topology, 0)
        if gmin <= 0:
            print(
                f"[FATAL] PGO receipt: DSP group '{group}' advanced 0 frames for "
                f"topology '{topology}'.",
                file=sys.stderr,
            )
            sys.exit(1)

coverage = receipt.get("coverage") or {}
for dim, required in (
    ("rates", ("44100", "48000", "96000")),
    ("quantums", ("64", "256", "512")),
    ("oversampling", ("Off", "2x", "4x")),
    ("cabsim", ("ir", "bypass")),
    ("recording", ("no", "yes")),
    ("gate", ("on", "off")),
):
    counts = coverage.get(dim) or {}
    for value in required:
        if not (counts.get(value, 0) > 0):
            print(
                f"[FATAL] PGO receipt: matrix dimension '{dim}' value '{value}' "
                f"was not exercised.",
                file=sys.stderr,
            )
            sys.exit(1)

if receipt.get("no_stage_skipped") is not True:
    print("[FATAL] PGO receipt: no_stage_skipped is not true; a mandatory DSP stage was skipped.", file=sys.stderr)
    sys.exit(1)

print(
    f"  OK PGO matrix receipt valid: total_blocks={ {k: v for k, v in sorted(total_blocks_by_topo.items())} }, "
    f"min_cell_blocks={ {k: v for k, v in sorted(min_blocks_by_topo.items())} }, "
    f"min_frames={ {k: v for k, v in sorted(min_frames_by_topo.items())} }, "
    f"coverage={ {d: sorted(c.items()) for d, c in sorted(coverage.items())} }"
)
PY
    then
        echo -e "${RED}Error: PGO coverage gate failed (see messages above). Cannot certify the profile.${NC}"
        exit 1
    fi

    PROFRAW_COUNT=$(find "$PROFRAW_DIR" -name "*.profraw" 2>/dev/null | wc -l)
    if [ "$PROFRAW_COUNT" -eq 0 ]; then
        echo -e "${RED}Error: No .profraw profile files were generated in $PROFRAW_DIR!${NC}"
        echo -e "${RED}PGO profiling failed — check that pgo_workload exercised the DSP pipeline.${NC}"
        exit 1
    fi

    echo -e "  ${GREEN}✓${NC} Collected $PROFRAW_COUNT .profraw profiles. Merging..."
    "$LLVM_PROFDATA" merge -sparse -o "$MERGED_PROFILE" "$PROFRAW_DIR"/*.profraw
    echo -e "  ${GREEN}✓${NC} Merged profile generated at: $MERGED_PROFILE ($(du -h "$MERGED_PROFILE" | cut -f1))"

    # Clean raw profiles after merging
    rm -rf "$PROFRAW_DIR"
else
    echo -e "\n${YELLOW}[Phase 2/7] Skipping PGO trace generation (--no-pgo).${NC}"
fi

# -----------------------------------------------------------------------------
# PHASE 3: Compile Optimized Standalone Binary
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 3/7] Compiling optimized standalone binary...${NC}"

if [ "$USE_PGO" = true ] && [ -f "$MERGED_PROFILE" ]; then
    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Cprofile-use=$MERGED_PROFILE"
else
    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS"
    warn "Compiling without PGO profile."
fi
echo -e "  Using RUSTFLAGS: ${BOLD}$RUSTFLAGS${NC}"

echo -e "  Building standalone executable..."
# -C strip=none retains symbol tables for LLVM BOLT. Stripping is applied in Phase 5.
# RELEASE_RUSTFLAGS is captured verbatim for the provenance receipt.
RELEASE_RUSTFLAGS="$RUSTFLAGS -C strip=none -Clink-arg=-Wl,-q"
RUSTFLAGS="$RELEASE_RUSTFLAGS" cargo build --locked --profile dist

PGO_BIN="$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe"
if [ ! -f "$PGO_BIN" ]; then
    echo -e "${RED}Error: Failed to find compiled standalone binary at $PGO_BIN${NC}"
    exit 1
fi
echo -e "  ${GREEN}✓${NC} Compilation completed successfully."

# -----------------------------------------------------------------------------
# PHASE 4: BOLT Post-Link Optimization
# -----------------------------------------------------------------------------
BOLT_APPLIED=false
BOLT_STATUS="PGO-ONLY"
BOLT_CAUSE=""

if [ "$USE_BOLT" = true ] && [ -n "$LLVM_BOLT" ] && [ "$HAS_PERF" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 4/7] Applying BOLT post-link optimization...${NC}"

    PW_RUNNING=false
    if command -v pw-cli &>/dev/null && (pw-cli info 0 &>/dev/null || pgrep -x pipewire &>/dev/null); then
        PW_RUNNING=true
    fi

    if [ "$PW_RUNNING" != true ]; then
        BOLT_CAUSE="BOLT_UNAVAILABLE: PipeWire daemon is not running (BOLT requires live graph profiling)."
        warn "$BOLT_CAUSE"
    else
        MODEL_FILES_STR=""
        if MODEL_FILES_STR=$(python3 -c '
import os

search_dirs = []
if os.environ.get("NAM_FIXTURES_DIR"):
    search_dirs.append(os.environ["NAM_FIXTURES_DIR"])
search_dirs.append("tests/fixtures/models")

categories = [
    ("WaveNet A1 Standard", ["wavenet_a1_standard.nam"]),
    ("WaveNet A2", ["a2_example.nam"]),
    ("LSTM", ["lstm.nam"]),
]

resolved = []
if os.environ.get("NAM_MODEL") and os.path.isfile(os.environ["NAM_MODEL"]):
    resolved.append(os.environ["NAM_MODEL"])

for cat, filenames in categories:
    found = False
    for d in search_dirs:
        for fname in filenames:
            path = os.path.join(d, fname)
            if os.path.isfile(path) and path not in resolved:
                resolved.append(path)
                found = True
                break
        if found:
            break

if not resolved:
    for d in search_dirs:
        if os.path.isdir(d):
            for f in sorted(os.listdir(d)):
                if f.endswith(".nam"):
                    p = os.path.join(d, f)
                    if p not in resolved:
                        resolved.append(p)
                        break
        if resolved:
            break

print("\n".join(resolved))
'); then
            :
        else
            BOLT_CAUSE="BOLT_FAILED: model file resolution failed."
        fi
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        MODEL_FILES=()
        if [ -n "$MODEL_FILES_STR" ]; then
            mapfile -t MODEL_FILES <<< "$MODEL_FILES_STR"
        fi
        if [ ${#MODEL_FILES[@]} -eq 0 ]; then
            BOLT_CAUSE="BOLT_UNAVAILABLE: no model files found for live profiling."
            warn "$BOLT_CAUSE"
        fi
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        echo -e "  PipeWire detected! Starting multi-model profiling across ${#MODEL_FILES[@]} topology families..."
        for path in "${MODEL_FILES[@]}"; do
            echo -e "    - $(basename "$path")"
        done

        # Verified test-signal WAV generation (fail-closed, no error suppression).
        TEST_WAV="$BOLT_DIR/test_signal.wav"
        if ! python3 - "$TEST_WAV" <<'PY'
import math
import struct
import sys
import wave

out = sys.argv[1]
rate = 48000
duration = 3
n = rate * duration
frames = bytearray()
for i in range(n):
    val = int(32767 * 0.5 * math.sin(2 * math.pi * 440 * i / rate))
    frames += struct.pack("<h", val)
with wave.open(out, "wb") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(rate)
    w.writeframes(bytes(frames))
PY
        then
            BOLT_CAUSE="BOLT_FAILED: test signal WAV generation failed."
        fi
    fi

    if [ -z "$BOLT_CAUSE" ] && ! python3 - "$TEST_WAV" <<'PY'
import sys
import wave

path = sys.argv[1]
try:
    with wave.open(path, "rb") as w:
        assert w.getnchannels() == 1, "test signal must be mono"
        assert w.getsampwidth() == 2, "test signal must be PCM16"
        assert w.getframerate() == 48000, "test signal must be 48 kHz"
        assert w.getnframes() == 3 * 48000, "test signal must be exactly 3 seconds"
except Exception as e:
    print(f"[FATAL] BOLT test signal invalid: {e}", file=sys.stderr)
    sys.exit(1)
print("  OK BOLT test signal verified: 3 s mono 48 kHz PCM16")
PY
    then
        BOLT_CAUSE="BOLT_FAILED: test signal WAV verification failed."
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        USE_LBR=false
        if perf record -e cycles:u -j any,u -F 99 -o /dev/null -- true >/dev/null 2>&1; then
            USE_LBR=true
        fi

        rm -f "$BOLT_DIR/perf.data"

        # Single perf record session covering every topology family. The
        # recorded command is a driver that sequentially starts nam-audio-pipe
        # for each model, proves PipeWire node + CPU sample-consumption
        # readiness, plays the test signal and exits. perf's default
        # inheritance records the whole process tree, aggregating samples from
        # every model into one perf.data (`perf record --append` no longer
        # exists on modern perf, so per-model files cannot be merged).
        PROFILE_DRIVER="$BOLT_DIR/profile_driver.sh"
        cat > "$PROFILE_DRIVER" <<EOF
#!/bin/bash
set -u
PGO_BIN="$PGO_BIN"
TEST_WAV="$TEST_WAV"

node_ready() {
    if command -v pw-dump >/dev/null 2>&1; then
        pw-dump 2>/dev/null | python3 -c '
import json
import sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for obj in data:
    if obj.get("type") != "PipeWire:Interface:Node":
        continue
    info = obj.get("info", {})
    if info.get("props", {}).get("node.name") != "NAM-Audio-Pipe-input":
        continue
    state = info.get("state", "")
    if state not in ("error", "unconnected"):
        sys.exit(0)
sys.exit(1)
'
    elif command -v pw-cli >/dev/null 2>&1; then
        pw-cli ls Node 2>/dev/null | grep -q 'node.name = "NAM-Audio-Pipe-input"'
    else
        return 1
    fi
}

cpu_ticks() { awk '{print \$14 + \$15}' "/proc/\$1/stat" 2>/dev/null || echo 0; }

index=0
for model_file in "\$@"; do
    index=\$((index + 1))
    echo "[model \$index] \$model_file"
    # Pass --gate off via CLI: NAM_DISABLE_GATE is only honoured under
    # #[cfg(feature = "testing")], which is absent in release builds.
    "\$PGO_BIN" -m "\$model_file" -b 64 --gate off &
    pid=\$!
    ready=0
    for i in \$(seq 1 30); do
        kill -0 "\$pid" 2>/dev/null || break
        if node_ready; then
            ready=1
            break
        fi
        sleep 0.5
    done
    if [ "\$ready" != 1 ]; then
        echo "ERROR: PipeWire capture node not registered for \$model_file" >&2
        kill "\$pid" 2>/dev/null || true
        wait "\$pid" 2>/dev/null || true
        exit 1
    fi
    # Start pw-play BEFORE probing CPU ticks: without an audio signal the
    # noise gate closes the DSP path and the process consumes near-zero
    # cycles, causing the tick-delta check to always fail.
    play=""
    if command -v pw-play >/dev/null 2>&1; then
        pw-play --target="NAM-Audio-Pipe-input" "\$TEST_WAV" &
        play=\$!
        # Brief stabilisation so PipeWire wires up the loopback before we
        # start measuring.
        sleep 0.3
    fi
    advancing=0
    for i in \$(seq 1 6); do
        t1=\$(cpu_ticks "\$pid")
        sleep 0.5
        t2=\$(cpu_ticks "\$pid")
        if [ "\$t2" -gt "\$t1" ]; then
            advancing=1
            break
        fi
    done
    if [ -n "\$play" ]; then
        kill "\$play" 2>/dev/null || true
        wait "\$play" 2>/dev/null || true
    fi
    if [ "\$advancing" != 1 ]; then
        echo "ERROR: no sample consumption (CPU not advancing) for \$model_file" >&2
        kill "\$pid" 2>/dev/null || true
        wait "\$pid" 2>/dev/null || true
        exit 1
    fi
    sleep 2
    kill "\$pid" 2>/dev/null || true
    wait "\$pid" 2>/dev/null || true
done
exit 0
EOF
        chmod +x "$PROFILE_DRIVER"

        if [ "$USE_LBR" = "true" ]; then
            PERF_ARGS=(-F 99 -e cycles:u -j any,u)
        else
            PERF_ARGS=(-F 4000 -e cycles:u)
        fi
        if ! perf record "${PERF_ARGS[@]}" -o "$BOLT_DIR/perf.data" -- "$PROFILE_DRIVER" "${MODEL_FILES[@]}"; then
            BOLT_CAUSE="BOLT_FAILED: perf record session failed (driver or profiling error)."
        fi
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        # perf.data must have minimum size and a usable sample count.
        if [ ! -s "$BOLT_DIR/perf.data" ]; then
            BOLT_CAUSE="BOLT_FAILED: perf.data is empty or missing."
        else
            PERF_DATA_SIZE=$(stat -c%s "$BOLT_DIR/perf.data" 2>/dev/null || echo 0)
            if [ "$PERF_DATA_SIZE" -lt 8192 ]; then
                BOLT_CAUSE="BOLT_FAILED: perf.data only ${PERF_DATA_SIZE} bytes (< 8 KiB); no usable samples."
            else
                PERF_SAMPLES=$(perf script -i "$BOLT_DIR/perf.data" 2>/dev/null | wc -l || true)
                PERF_SAMPLES="${PERF_SAMPLES:-0}"
                if [ "$PERF_SAMPLES" -lt 500 ]; then
                    BOLT_CAUSE="BOLT_FAILED: perf.data has only ${PERF_SAMPLES} samples (< 500); workload did not run."
                else
                    echo -e "  ${GREEN}✓${NC} perf.data valid: ${PERF_DATA_SIZE} bytes, ${PERF_SAMPLES} samples."
                fi
            fi
        fi
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        # Strict Build-ID confirmation: never convert traces from a
        # non-identical binary (removes the old --ignore-build-id escape hatch).
        # LC_ALL=C keeps readelf output locale-independent and the token is
        # captured at full width (sha1 = 40 hex, sha256 = 64 hex, ...) and
        # lowercased so the comparison with `perf buildid-list` never truncates.
        ELF_BID=$(LC_ALL=C readelf -n "$PGO_BIN" 2>/dev/null | grep -oP 'Build ID:\s+\K[0-9a-fA-F]+' | head -n1 | tr 'A-F' 'a-f' || true)
        PERF_BID=$(LC_ALL=C perf buildid-list -i "$BOLT_DIR/perf.data" 2>/dev/null | grep -F "$PGO_BIN" | head -n1 | sed -E 's/.*=//' | awk '{print $1}' || true)
        if [ -z "$ELF_BID" ] || [ -z "$PERF_BID" ] || [ "$PERF_BID" != "$ELF_BID" ]; then
            BOLT_CAUSE="BOLT_FAILED: Build-ID mismatch (perf=${PERF_BID:-none}, elf=${ELF_BID:-none}); refusing to optimize a non-identical binary."
        else
            echo -e "  ${GREEN}✓${NC} Build-ID confirmed: $ELF_BID"
        fi
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        PERF2BOLT_FLAGS=()
        if [ "$USE_LBR" = "false" ]; then
            if "$PERF2BOLT" --help 2>&1 | grep -q -- '--basic-events'; then
                PERF2BOLT_FLAGS+=("--basic-events")
            else
                warn "perf2bolt --basic-events not supported by this toolchain; omitting flag."
            fi
        fi

        if "$PERF2BOLT" "${PERF2BOLT_FLAGS[@]}" -p "$BOLT_DIR/perf.data" "$PGO_BIN" -o "$BOLT_DIR/perf.fdata" > "$BOLT_DIR/perf2bolt.log" 2>&1; then
            FDATA_SIZE=$(stat -c%s "$BOLT_DIR/perf.fdata" 2>/dev/null || echo 0)
            FDATA_SAMPLE_LINES=$(awk '$1 ~ /^[0-9]+$/ {n++} END {print n+0}' "$BOLT_DIR/perf.fdata" 2>/dev/null || echo 0)
            FDATA_DSP_HITS=0
            for sym in capture_dsp_pipeline wavenet lstm cabsim; do
                if grep -qi "$sym" "$BOLT_DIR/perf.fdata"; then
                    FDATA_DSP_HITS=$((FDATA_DSP_HITS + 1))
                fi
            done
            if [ "$FDATA_SIZE" -lt 1024 ]; then
                BOLT_CAUSE="BOLT_FAILED: perf.fdata too small (${FDATA_SIZE} bytes)."
            elif [ "$FDATA_SAMPLE_LINES" -lt 20 ]; then
                BOLT_CAUSE="BOLT_FAILED: perf.fdata has only ${FDATA_SAMPLE_LINES} sampled function entries (< 20)."
            elif [ "$FDATA_DSP_HITS" -eq 0 ]; then
                BOLT_CAUSE="BOLT_FAILED: perf.fdata has no DSP hot-path symbol samples (capture_dsp_pipeline/wavenet/lstm/cabsim)."
            else
                echo -e "  ${GREEN}✓${NC} perf.fdata valid: ${FDATA_SIZE} bytes, ${FDATA_SAMPLE_LINES} sampled entries, ${FDATA_DSP_HITS} DSP symbol groups."
            fi
        else
            BOLT_CAUSE="BOLT_FAILED: perf2bolt conversion failed. $(tail -n1 "$BOLT_DIR/perf2bolt.log" 2>/dev/null || true)"
        fi
    fi

    if [ -z "$BOLT_CAUSE" ]; then
        if "$LLVM_BOLT" "$PGO_BIN" \
            -o "$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe.bolt" \
            -data "$BOLT_DIR/perf.fdata" \
            --reorder-blocks=ext-tsp \
            --reorder-functions=hfsort \
            --split-functions \
            --split-all-cold \
            -hugify \
            --relocs \
            --lite > "$BOLT_DIR/llvm-bolt.log" 2>&1; then
            BOLT_APPLIED=true
            echo -e "  ${GREEN}✓${NC} BOLT applied successfully."
        else
            BOLT_CAUSE="BOLT_FAILED: llvm-bolt command failed. $(tail -n1 "$BOLT_DIR/llvm-bolt.log" 2>/dev/null || true)"
        fi
    fi
else
    echo -e "\n${YELLOW}[Phase 4/7] Skipping BOLT optimization.${NC}"
    if [ "$USE_BOLT" = true ]; then
        if [ -z "$LLVM_BOLT" ]; then
            BOLT_CAUSE="BOLT_UNAVAILABLE: llvm-bolt not found."
        elif [ "$HAS_PERF" != true ]; then
            BOLT_CAUSE="BOLT_UNAVAILABLE: perf/PMU not available (missing tool or kernel.perf_event_paranoid > 1)."
        fi
    else
        BOLT_CAUSE="BOLT_UNAVAILABLE: --no-bolt requested."
    fi
fi

# Degradation policy: BOLT absence must be typed and explicit,
# never silently masked as "PGO + BOLT applied". Under --strict-release, any
# unproven BOLT aborts the release.
if [ "$BOLT_APPLIED" = true ]; then
    if [ "$USE_PGO" = true ]; then
        BOLT_STATUS="PGO+BOLT"
    else
        BOLT_STATUS="BOLT-ONLY"
    fi
elif [ "$USE_PGO" = true ]; then
    BOLT_STATUS="PGO-ONLY"
else
    BOLT_STATUS="PLAIN"
fi

if [ "$BOLT_APPLIED" != true ]; then
    warn "Optimization status: ${BOLT_STATUS} (${BOLT_CAUSE:-BOLT unavailable})."
    if [ "$STRICT_RELEASE" = true ]; then
        die "BOLT is mandatory under --strict-release but could not be proven: ${BOLT_CAUSE:-unknown cause}. Aborting the release."
    fi
fi

write_release_receipt "$BOLT_STATUS" "$BOLT_CAUSE"


# -----------------------------------------------------------------------------
# PHASE 4.5: Assembly Hotspot Disassembly Report
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 4.5/7] Generating AI-ready assembly hotspot report...${NC}"

ASM_TARGET="$PROJECT_DIR/target/dsp_hotpath.asm"

if [ "$BOLT_APPLIED" = true ] && [ -f "$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe.bolt" ]; then
    ASM_BIN="$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe.bolt"
elif [ -f "$PGO_BIN" ]; then
    ASM_BIN="$PGO_BIN"
fi

if [ -n "${ASM_BIN:-}" ]; then
    if command -v llvm-objdump &>/dev/null; then
        llvm-objdump -d --no-show-raw-insn "$ASM_BIN" > "$ASM_TARGET" 2>/dev/null || true
    elif command -v objdump &>/dev/null; then
        objdump -d --no-show-raw-insn "$ASM_BIN" > "$ASM_TARGET" 2>/dev/null || true
    fi

    if [ -s "$ASM_TARGET" ]; then
        echo -e "  ${GREEN}✓${NC} Assembly report generated at target/dsp_hotpath.asm ($(wc -l < "$ASM_TARGET") lines)"
    else
        echo -e "  ${YELLOW}Warning: Assembly disassembly failed or produced empty output.${NC}"
    fi
else
    echo -e "  ${YELLOW}Warning: No optimized binary found for disassembly.${NC}"
fi

# ---------------------------------------------------------------------------
# Phase 5 helpers: functional smoke test of the stripped ELF
# ---------------------------------------------------------------------------
# run_live_smoke <staging_bin> <model_abs> <smoke_dir> <out_file>
#   Runs the stripped staging binary against a real fixture model with
#   `--record` (distribution profile; panic = "unwind" so the F-RB-020
#   catch_unwind containment is effective in the shipped artifact), drives the
#   PipeWire graph with a silent tone so the capture node consumes real audio
#   quantums, requests graceful shutdown via SIGTERM and validates the final
#   exit code, the absence of runtime crash artifacts and the production of a
#   coherent, non-empty recording WAV. Sets SMOKE_CAUSE="" on success,
#   otherwise a "SMOKE_FAILED: ..." defect description (returns 1).
run_live_smoke() {
    local bin="$1" model="$2" dir="$3" out="$4"
    local cab="$PROJECT_DIR/tests/fixtures/models/cabsim_ir_pgo.wav"
    local silent_wav="$dir/silence.wav"
    local pid="" play_pid="" node_ready=0 i exit_code=0 stopped=0 wav=""

    # Deterministic graph scheduling: a silent 48 kHz PCM16 signal played into
    # the NAM capture sink keeps the node processing real quantums (the same
    # pattern as pw_integration's tone driver and the BOLT profile driver).
    if ! python3 - "$silent_wav" <<'PY'
import struct
import sys
import wave

out = sys.argv[1]
rate = 48000
n = rate * 2
with wave.open(out, "wb") as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(rate)
    w.writeframes(struct.pack(f"<{n}h", *([0] * n)))
PY
    then
        SMOKE_CAUSE="SMOKE_FAILED: could not generate the silent test-signal WAV."
        return 1
    fi

    local -a args=("$bin" -m "$model" --record)
    if [ -f "$cab" ]; then
        args+=(-c "$cab")
    fi
    (cd "$dir" && exec "${args[@]}") >"$out" 2>&1 &
    pid=$!

    # Wait (bounded) for the capture node to register in the graph — proves the
    # stripped ELF linked, allocated its PipeWire streams and registered them.
    for i in $(seq 1 40); do
        if ! kill -0 "$pid" 2>/dev/null; then
            break
        fi
        if pw-cli ls Node 2>/dev/null | grep -q 'node.name = "NAM-Audio-Pipe-input"'; then
            node_ready=1
            break
        fi
        sleep 0.5
    done

    if [ "$node_ready" != 1 ]; then
        if kill -0 "$pid" 2>/dev/null; then
            kill -TERM "$pid" 2>/dev/null || true
            sleep 1
            kill -KILL "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
            SMOKE_CAUSE="SMOKE_FAILED: staging binary did not register its capture node within 20 s (startup stall)."
        else
            wait "$pid" 2>/dev/null && exit_code=0 || exit_code=$?
            SMOKE_CAUSE="SMOKE_FAILED: staging binary exited early (code ${exit_code}) before registering its capture node — startup failure under the stripped profile."
        fi
        return 1
    fi

    # Drive the graph with the silent tone so the capture node is scheduled.
    if command -v pw-play >/dev/null 2>&1; then
        pw-play --target="NAM-Audio-Pipe-input" "$silent_wav" >/dev/null 2>&1 &
        play_pid=$!
    fi

    # Let the DSP (model + CabSim) + recording pipeline run for a bounded window.
    sleep 6

    # Cooperative shutdown: the first SIGTERM is handled gracefully by the
    # async-signal-safe handler (clean audio stop, WAV finalization + fsync).
    kill -TERM "$pid" 2>/dev/null || true
    for i in $(seq 1 40); do
        if ! kill -0 "$pid" 2>/dev/null; then
            stopped=1
            break
        fi
        sleep 0.25
    done
    if [ "$stopped" != 1 ]; then
        kill -KILL "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        SMOKE_CAUSE="SMOKE_FAILED: staging binary hung after SIGTERM (shutdown deadlock)."
        return 1
    fi
    wait "$pid" 2>/dev/null && exit_code=0 || exit_code=$?

    if [ -n "$play_pid" ]; then
        kill "$play_pid" 2>/dev/null || true
        wait "$play_pid" 2>/dev/null || true
    fi

    if [ "$exit_code" -ne 0 ]; then
        SMOKE_CAUSE="SMOKE_FAILED: staging binary exited with code ${exit_code} after SIGTERM (expected 0)."
        return 1
    fi

    # Runtime crash artifact detection (panics, segfaults and double-frees must
    # never be silent in a certified release).
    if grep -qiE "panicked|Segmentation fault|double free|core dumped" "$out"; then
        SMOKE_CAUSE="SMOKE_FAILED: runtime crash artifact detected in the smoke output."
        return 1
    fi

    # The recording pipeline must have produced a real, finalized WAV with a
    # coherent header (data size > 0) — proof the model loaded, DSP ran and the
    # recording worker shut down cleanly.
    wav="$(find "$dir" -maxdepth 1 -name 'capture_*.wav' -print -quit 2>/dev/null || true)"
    if [ -z "$wav" ] || [ ! -s "$wav" ]; then
        SMOKE_CAUSE="SMOKE_FAILED: no recording WAV was produced during the live smoke run (DSP/recording pipeline did not execute)."
        return 1
    fi
    if ! python3 - "$wav" <<'PY'
import struct
import sys

path = sys.argv[1]
with open(path, "rb") as f:
    data = f.read()
assert data[:4] == b"RIFF", "not a RIFF container"
assert data[8:12] == b"WAVE", "not a WAVE file"
pos = 12
size = None
while pos + 8 <= len(data):
    cid = data[pos : pos + 4]
    (csize,) = struct.unpack_from("<I", data, pos + 4)
    if cid == b"data":
        size = csize
        break
    pos += 8 + csize + (csize & 1)
assert size is not None, "'data' chunk missing"
assert size > 0, "WAV 'data' size is 0 (no audio captured)"
print(f"  OK smoke recording WAV valid: {size} data bytes")
PY
    then
        SMOKE_CAUSE="SMOKE_FAILED: recorded WAV header is incoherent or empty (data size 0)."
        return 1
    fi

    SMOKE_CAUSE=""
    return 0
}

# write_smoke_receipt <status> <cause>
#   Enriches the release receipt (target/logs/release-receipt.json) with the
#   functional smoke-test certification: status LIVE or
#   DIAGNOSE_ONLY, plus the detailed cause when degraded/failed. The base
#   optimization receipt written after Phase 4 is preserved.
write_smoke_receipt() {
    local status="$1" cause="${2:-}"
    mkdir -p "$PROJECT_DIR/target/logs"
    if python3 - "$RELEASE_RECEIPT" "$status" "$cause" > "$RELEASE_RECEIPT.tmp" <<'PY'
import json
import sys

path, status, cause = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    with open(path, "r", encoding="utf-8") as f:
        doc = json.load(f)
except Exception:
    doc = {"schema_version": 1, "tool": "build-release.sh"}
doc["smoke"] = {"status": status}
if cause:
    doc["smoke"]["cause"] = cause
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
    then
        mv -f "$RELEASE_RECEIPT.tmp" "$RELEASE_RECEIPT"
        echo -e "  ${GREEN}✓${NC} Release receipt updated with smoke status: $SMOKE_STATUS"
    else
        rm -f "$RELEASE_RECEIPT.tmp"
        warn "Failed to enrich release receipt with the smoke status."
    fi
}

# ---------------------------------------------------------------------------
# Provenance receipt: cryptographic chain of custody
# ---------------------------------------------------------------------------
# write_provenance_receipt
#   Emits target/logs/release-provenance.json certifying every element that
#   determines the distributed bytes: clean source commit + UTC timestamp,
#   exact rustc/cargo versions, build identity (profile `dist` vs `testing`,
#   active features, RUSTFLAGS, optimization status, explicit opt-out of
#   harness-measured performance claims for the final ELF — T5.1), build
#   environment (`uname -r`, `pw-cli --version` — T5.1), dependency
#   traceability (Cargo.lock SHA-256 + coupled NeuralAmpModeler-rs commit) and
#   SHA-256 + size + Build-ID of each delivery artifact (installed stripped
#   ELF, .tar.zst, .flatpak, AppStream metainfo). Failure to emit the receipt
#   is fatal under --release-ceremony (a release without provenance is not a
#   release); elsewhere it degrades to a typed warning.
write_provenance_receipt() {
    mkdir -p "$PROJECT_DIR/target/logs"
    local ts commit tree_sha rustc_ver cargo_ver base_rustflags cpu_baseline \
        lock_path nam_rs_dir nam_commit bin_bid bin_path tar_path \
        flatpak_path appstream_path ceremony_status quick_receipt long_receipt \
        pgo_receipt release_receipt features_json kernel_release pw_cli_version
    ts="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    commit="$(git rev-parse HEAD 2>/dev/null || echo "unknown")"
    tree_sha="$(git rev-parse HEAD^{tree} 2>/dev/null || echo "unknown")"
    rustc_ver="$(rustc --version)"
    cargo_ver="$(cargo --version)"
    base_rustflags="${RELEASE_RUSTFLAGS:-$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS}"
    case "$CONFIG_RUSTFLAGS" in
        *x86-64-v3*) cpu_baseline="x86-64-v3 (AVX2+FMA+BMI2)" ;;
        *native*) cpu_baseline="native ($(uname -m))" ;;
        *) cpu_baseline="default" ;;
    esac
    # T5.1: active features of the release ELF = the resolved default feature
    # set (the release build passes no --features flags). Derived from
    # Cargo.toml so the receipt can never drift from the manifest.
    features_json=$(python3 -c '
import json

features = []
try:
    import tomllib
    with open("Cargo.toml", "rb") as f:
        data = tomllib.load(f)
    default = data.get("features", {}).get("default", [])
    features = [f.split("/")[0] for f in default if isinstance(f, str)]
except Exception:
    import re
    try:
        with open("Cargo.toml", "r") as f:
            content = f.read()
        match = re.search(r"\[features\]\s*default\s*=\s*\[(.*?)\]", content, re.DOTALL)
        if match:
            features = [
                x.strip().strip("\"").split("/")[0]
                for x in match.group(1).split(",")
                if x.strip()
            ]
    except Exception:
        pass
print(json.dumps(features))
' 2>/dev/null || echo "[]")
    # T5.1: build environment identity — kernel release + PipeWire CLI version.
    kernel_release="$(uname -r 2>/dev/null || echo 'unknown')"
    # T5.1: build environment identity — PipeWire version.
    # NOTE: `pw-cli --version` emits only the binary name ("pw-cli") with no
    # version number on this PipeWire build. Use the package manager as the
    # authoritative source; fall back gracefully when running outside a deb/rpm
    # environment (e.g., in a container or nix derivation).
    pw_cli_version="$(dpkg-query -W -f='pipewire ${Version}' pipewire 2>/dev/null \
        || rpm -q --qf 'pipewire %{VERSION}-%{RELEASE}' pipewire 2>/dev/null \
        || (command -v pw-cli > /dev/null 2>&1 && pw-cli --version 2>/dev/null | grep -v '^pw-cli$' | head -n1) \
        || true)"
    pw_cli_version="${pw_cli_version:-unknown}"
    lock_path="$PROJECT_DIR/Cargo.lock"
    nam_rs_dir="$PROJECT_DIR/../NeuralAmpModeler-rs"
    if [ -d "$nam_rs_dir/.git" ]; then
        nam_commit="$(git -C "$nam_rs_dir" rev-parse HEAD 2>/dev/null || true)"
    fi
    nam_commit="${nam_commit:-not-a-git-repo}"
    # Full-width GNU build-id note: sha1 = 40 hex, sha256 = 64 hex, etc. The
    # token is captured at its real width (never truncated to 40) and
    # lowercased so the provenance validator can compare it byte-for-byte with
    # the value it recomputes from the ELF on disk.
    bin_bid="$(LC_ALL=C readelf -n "$BIN_TARGET" 2>/dev/null | grep -oP 'Build ID:\s+\K[0-9a-fA-F]+' | head -n1 | tr 'A-F' 'a-f' || true)"
    bin_path="$BIN_TARGET"
    tar_path=""
    if [ "$BUILD_TARBALL" = true ] && [ -f "$TARBALL" ]; then tar_path="$TARBALL"; fi
    flatpak_path=""
    if [ "$BUILD_FLATPAK" = true ] && [ -f "$FLATPAK_BUNDLE" ]; then flatpak_path="$FLATPAK_BUNDLE"; fi
    appstream_path="$PROJECT_DIR/packaging/flatpak/io.github.fabiohl.NAMAudioPipe.metainfo.xml"
    [ -f "$appstream_path" ] || appstream_path=""

    ceremony_status="uncertified"
    if [ "$RELEASE_CEREMONY" = true ] && [ "$STRICT_RELEASE" = true ]; then
        ceremony_status="certified_release"
    fi

    # F-RB-027 / T5.2: a certified release requires exact binary identity. If
    # readelf is absent or the ELF carries no GNU build-id note, bin_bid is
    # empty and the ceremony must fail fail-closed — a "certified_release"
    # receipt with `build_id: null` is rejected by tests/distribution_qa.rs
    # anyway, so refusing here prevents emitting an uncertifiable receipt.
    if [ "$RELEASE_CEREMONY" = true ] && [ -z "$bin_bid" ]; then
        die "Release ceremony requires a non-empty ELF Build-ID on $BIN_TARGET \
(readelf -n found no GNU build-id note, or readelf is unavailable). A \
certified release without exact binary identity is not a release (F-RB-027)."
    fi

    quick_receipt="$PROJECT_DIR/target/logs/quick-receipt.txt"
    long_receipt="$PROJECT_DIR/target/logs/long-receipt.txt"
    pgo_receipt="$PROJECT_DIR/target/logs/pgo-workload-receipt.json"
    release_receipt="$PROJECT_DIR/target/logs/release-receipt.json"

    if python3 - "$ts" "$commit" "$tree_sha" "$VERSION" "$rustc_ver" "$cargo_ver" \
        "$base_rustflags" "$BOLT_STATUS" "$cpu_baseline" "$lock_path" \
        "$nam_commit" "$bin_bid" "$bin_path" "$tar_path" "$flatpak_path" \
        "$appstream_path" "$ceremony_status" "$quick_receipt" "$long_receipt" \
        "$pgo_receipt" "$release_receipt" "$features_json" "$kernel_release" \
        "$pw_cli_version" > "$PROVENANCE_RECEIPT" <<'PY'
import hashlib
import json
import os
import sys


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


(ts, commit, tree_sha, version, rustc_ver, cargo_ver, rustflags, opt_status,
 cpu_baseline, lock_path, nam_commit, bin_bid, bin_path, tar_path,
 flatpak_path, appstream_path, ceremony_status, quick_receipt, long_receipt,
 pgo_receipt, release_receipt, features_json, kernel_release,
 pw_cli_version) = sys.argv[1:25]

try:
    features = json.loads(features_json) if features_json else []
    if not isinstance(features, list):
        features = []
except Exception:
    features = []

lock_sha = sha256(lock_path) if os.path.isfile(lock_path) else None


def artifact(path):
    if not path or not os.path.isfile(path):
        return None
    return {
        "path": path,
        "sha256": sha256(path),
        "size_bytes": os.path.getsize(path),
    }


artifacts = {}
bin_art = artifact(bin_path)
if bin_art is not None:
    bin_art["build_id"] = bin_bid or None
    artifacts["installed_binary"] = bin_art
for name, path in (
    ("tarball", tar_path),
    ("flatpak_bundle", flatpak_path),
    ("appstream_metainfo", appstream_path),
):
    art = artifact(path)
    if art is not None:
        artifacts[name] = art

# The agile suite (tests-quick.sh) regenerates quick-phase*.log and
# quick-receipt.txt on every pass. Pinning them in an UNCERTIFIED chain
# would make the very next quick run invalidate the provenance — the
# integrity gate could never go green while a release exists.
# The certified ceremony path still requires them; uncertified chains keep
# only the stable evidence (long-audit logs, PGO/release receipts).
phase_logs = {}
target_logs = os.path.join(os.path.dirname(lock_path), "target", "logs")
if os.path.isdir(target_logs):
    for fname in sorted(os.listdir(target_logs)):
        if fname.endswith(".log"):
            if ceremony_status != "certified_release" and fname.startswith("quick-phase"):
                continue
            lpath = os.path.join(target_logs, fname)
            art = artifact(lpath)
            if art:
                phase_logs[fname] = art

quick_art = artifact(quick_receipt)
if ceremony_status != "certified_release":
    quick_art = None
long_art = artifact(long_receipt)
pgo_art = artifact(pgo_receipt)
release_art = artifact(release_receipt)

if ceremony_status == "certified_release":
    missing = []
    if not commit or commit == "unknown":
        missing.append("project.commit")
    if not tree_sha or tree_sha == "unknown":
        missing.append("git_tree_sha256")
    if not lock_sha:
        missing.append("Cargo.lock sha256")
    if not nam_commit or nam_commit == "not-a-git-repo":
        missing.append("neural_amp_modeler_rs_commit")
    if not quick_art:
        missing.append("quick_receipt")
    if not long_art:
        missing.append("long_receipt")
    if not pgo_art:
        missing.append("pgo_receipt")
    if not release_art:
        missing.append("release_receipt")
    if not phase_logs:
        missing.append("phase_logs")
    if missing:
        raise RuntimeError(
            f"Release ceremony requires a complete provenance chain. Missing: {', '.join(missing)}"
        )

doc = {
    "schema_version": 2,
    "tool": "build-release.sh",
    "kind": "release-provenance",
    "project": {
        "name": "nam-audio-pipe",
        "version": version,
        "commit": commit,
        "git_tree_sha256": tree_sha,
        "timestamp_utc": ts,
    },
    "toolchain": {"rustc": rustc_ver, "cargo": cargo_ver},
    "build": {
        "profile": "dist",
        "features": features,
        "rustflags": rustflags,
        "optimizations": {
            "status": opt_status,
            "cpu_baseline": cpu_baseline,
            "pgo": opt_status in ("PGO+BOLT", "PGO-ONLY"),
            "bolt": opt_status in ("PGO+BOLT", "BOLT-ONLY"),
            # T5.1: the optimization status is a compilation-transform claim
            # only — no harness-measured performance metric (deadline/jitter/
            # throughput) is ever attributed to the final PGO+BOLT ELF here.
            "measured_performance_claims": False,
        },
    },
    "environment": {
        "kernel_release": kernel_release,
        "pw_cli_version": pw_cli_version or None,
    },
    "dependencies": {
        "cargo_lock_sha256": lock_sha,
        "neural_amp_modeler_rs_commit": nam_commit,
    },
    "ceremony_chain": {
        "certification_status": ceremony_status,
        "quick_receipt": quick_art,
        "long_receipt": long_art,
        "pgo_receipt": pgo_art,
        "release_receipt": release_art,
        "phase_logs": phase_logs,
    },
    "artifacts": artifacts,
}
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
    then
        echo -e "  ${GREEN}✓${NC} Provenance receipt written: $PROVENANCE_RECEIPT"
    else
        rm -f "$PROVENANCE_RECEIPT"
        if [ "$RELEASE_CEREMONY" = true ]; then
            die "Failed to write the release provenance receipt; a ceremony release without provenance is not a release."
        fi
        warn "Failed to write the release provenance receipt to $PROVENANCE_RECEIPT."
    fi
}

# -----------------------------------------------------------------------------
# PHASE 5: Staging, Functional Smoke Test & Atomic Installation
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 5/7] Staging, functionally smoke-testing and atomically installing the artifact...${NC}"

mkdir -p "$BIN_INSTALL_DIR"

# 1. ISOLATED STAGING: strip the build artifact inside a temporary staging dir
#    so a strip/linkage failure can never corrupt the previous installation or
#    the build output. The final installed binary is strictly the same stripped
#    ELF that passes the functional smoke test below.
STAGING_DIR="$(mktemp -d -t nam-pipe-staging.XXXXXX)"
STAGING_BIN="$STAGING_DIR/nam-audio-pipe"

if [ "$BOLT_APPLIED" = true ] && [ -f "$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe.bolt" ]; then
    BUILD_BIN="$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe.bolt"
    STAGING_LABEL="PGO + BOLT"
else
    BUILD_BIN="$PGO_BIN"
    STAGING_LABEL="PGO"
fi

if ! cp "$BUILD_BIN" "$STAGING_BIN"; then
    die "Failed to copy build artifact into staging ($STAGING_BIN); previous installation left intact."
fi
if ! strip --strip-all "$STAGING_BIN"; then
    die "strip --strip-all failed on the staged artifact; previous installation left intact."
fi
chmod +x "$STAGING_BIN"
echo -e "  ${GREEN}✓${NC} Staged stripped artifact (${STAGING_LABEL}): $STAGING_BIN"

# 2. FUNCTIONAL SMOKE TEST BATTERY against the stripped staging ELF. The
#    distribution profile carries no testing feature and is aggressively
#    transformed (PGO/BOLT + strip), so a static --diagnose
#    alone cannot prove model loading, PipeWire stream allocation, CabSim
#    convolution, recording-worker shutdown or symbol/linkage integrity.
SMOKE_STATUS="DIAGNOSE_ONLY"
SMOKE_CAUSE=""

# (a) CLI integrity + linkage check: --diagnose exercises argument parsing, CPU
#     detection and the diagnostic bundle against the exact final ELF.
if "$STAGING_BIN" --diagnose > "$STAGING_DIR/diagnose.out" 2>&1; then
    if grep -qE "NeuralAmpModeler-rs Diagnostic|NAM-rs Diagnostic|System Information|Runtime State" "$STAGING_DIR/diagnose.out"; then
        echo -e "  ${GREEN}✓${NC} [smoke a] --diagnose exited 0 and emitted the diagnostic bundle (CLI/linkage integrity)."
    else
        SMOKE_STATUS="FAILED"
        SMOKE_CAUSE="SMOKE_FAILED: --diagnose exited 0 but emitted no diagnostic bundle."
        warn "$SMOKE_CAUSE"
    fi
else
    SMOKE_STATUS="FAILED"
    SMOKE_CAUSE="SMOKE_FAILED: --diagnose exited non-zero on the stripped staging ELF."
    warn "$SMOKE_CAUSE"
fi

# (b)+(c) Live functional smoke: neural model load + autonomous DSP + recording
#         pipeline under the stripped profile. Requires a reachable PipeWire
#         daemon AND pw-play (to drive the graph deterministically) AND a
#         fixture model. Any defect in the staged artifact fails the release
#         immediately; only an environmental gap (no daemon/tool/model)
#         degrades to a typed DIAGNOSE_ONLY status — fatal under
#         --strict-release.
if [ -z "$SMOKE_CAUSE" ] && command -v pw-cli &>/dev/null && pw-cli info 0 &>/dev/null \
    && command -v pw-play &>/dev/null; then
    SMOKE_MODEL=""
    for cand in tests/fixtures/models/lstm.nam tests/fixtures/models/wavenet_a1_standard.nam tests/fixtures/models/a2_example.nam; do
        if [ -f "$cand" ]; then
            SMOKE_MODEL="$cand"
            break
        fi
    done
    if [ -z "$SMOKE_MODEL" ]; then
        SMOKE_MODEL="$(ls tests/fixtures/models/*.nam 2>/dev/null | head -n1 || true)"
    fi

    if [ -z "$SMOKE_MODEL" ]; then
        SMOKE_CAUSE="SMOKE_UNAVAILABLE: no fixture model found for the live functional smoke test."
        warn "$SMOKE_CAUSE"
    else
        SMOKE_MODEL_ABS="$PROJECT_DIR/$SMOKE_MODEL"
        SMOKE_DIR="$STAGING_DIR/smoke"
        mkdir -p "$SMOKE_DIR"
        if run_live_smoke "$STAGING_BIN" "$SMOKE_MODEL_ABS" "$SMOKE_DIR" "$STAGING_DIR/smoke.out"; then
            SMOKE_STATUS="LIVE"
            echo -e "  ${GREEN}✓${NC} [smoke b/c] Live functional smoke passed: model load + DSP + recording + clean shutdown (exit 0)."
        else
            SMOKE_STATUS="FAILED"
        fi
    fi
elif [ -z "$SMOKE_CAUSE" ]; then
    SMOKE_CAUSE="SMOKE_UNAVAILABLE: PipeWire daemon or pw-play unavailable for the live functional smoke test (pw-cli info 0 failed)."
    warn "$SMOKE_CAUSE"
fi

# 3. SMOKE GATE: a defect in the staged artifact always aborts (rollback keeps
#    the previous installation untouched); an environmental gap only aborts
#    under --strict-release.
if [ "$SMOKE_STATUS" = "FAILED" ]; then
    if [ -f "$STAGING_DIR/smoke.out" ]; then
        echo -e "\n${YELLOW}--- staged smoke output (tail) ---${NC}" >&2
        tail -n 30 "$STAGING_DIR/smoke.out" >&2 || true
    fi
    rm -rf "$STAGING_DIR"
    die "Functional smoke test FAILED against the stripped artifact: ${SMOKE_CAUSE}. Previous installation left intact."
fi
if [ "$SMOKE_STATUS" != "LIVE" ]; then
    warn "Smoke status: ${SMOKE_STATUS} (${SMOKE_CAUSE})."
    if [ "$STRICT_RELEASE" = true ]; then
        rm -rf "$STAGING_DIR"
        die "A live functional smoke test is mandatory under --strict-release but could not be proven: ${SMOKE_CAUSE}. Aborting the release."
    fi
fi

# Enrich the release receipt with the functional smoke certification.
write_smoke_receipt "$SMOKE_STATUS" "$SMOKE_CAUSE"

# 4. STRICTLY ATOMIC INSTALLATION: only after the staging smoke test approved
#    the artifact. The previous installation is preserved until the new stripped
#    ELF is fully in place rollback).
if [ -e "$BIN_TARGET" ] || [ -L "$BIN_TARGET" ]; then
    if ! mv -f "$BIN_TARGET" "$BIN_TARGET.old"; then
        rm -rf "$STAGING_DIR"
        die "Failed to back up the previous installation to $BIN_TARGET.old; aborting without touching it."
    fi
    echo -e "  ${YELLOW}ⓘ${NC} Backed up previous installation to $BIN_TARGET.old"
fi

if ! mv -T "$STAGING_BIN" "$BIN_TARGET"; then
    if [ -e "$BIN_TARGET.old" ]; then
        mv -f "$BIN_TARGET.old" "$BIN_TARGET" 2>/dev/null || true
    fi
    rm -rf "$STAGING_DIR"
    die "Atomic install of the staged binary failed; previous installation restored."
fi
chmod +x "$BIN_TARGET"
rm -f "$BIN_TARGET.old"
rm -rf "$STAGING_DIR"
STAGING_DIR=""
echo -e "  ${GREEN}✓${NC} Installed stripped artifact (${STAGING_LABEL}) atomically: $BIN_TARGET"

# Read version for archive naming
VERSION=$(cargo metadata --locked --no-deps --format-version 1 | python3 -c "import sys, json; print(json.load(sys.stdin)['packages'][0]['version'])")
ARCHIVE_NAME="nam-audio-pipe-v${VERSION}-linux-x86_64-v3"
TARBALL="$HOME/${ARCHIVE_NAME}.tar.zst"
FLATPAK_BUNDLE="$HOME/${ARCHIVE_NAME}.flatpak"

# -----------------------------------------------------------------------------
# PHASE 6: Release Packaging (.tar.zst)
# -----------------------------------------------------------------------------
if [ "$BUILD_TARBALL" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 6/7] Generating deterministic distribution tarball...${NC}"

    # Reproducible builds: anchor every packaged mtime to the
    # source commit timestamp so the archive is byte-identical across rebuilds
    # of the exact same tree.
    SOURCE_DATE_EPOCH="$(git log -1 --pretty=%ct)"
    export SOURCE_DATE_EPOCH
    echo -e "  SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH ($(date -u -d "@$SOURCE_DATE_EPOCH" +'%Y-%m-%d %H:%M:%SZ' 2>/dev/null || echo 'n/a'))"

    PKG_DIR="$(mktemp -d -t nam-audio-pipe-pkg.XXXXXX)"
    mkdir -p "$PKG_DIR/$ARCHIVE_NAME"

    cp "$BIN_TARGET" "$PKG_DIR/$ARCHIVE_NAME/nam-audio-pipe"
    cp README.md LICENSE.txt "$PKG_DIR/$ARCHIVE_NAME/" 2>/dev/null || true

    # Generate 1-click install script for end-users
    cat << 'EOF' > "$PKG_DIR/$ARCHIVE_NAME/install.sh"
#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
set -e
BIN_DIR="$HOME/.local/bin"
mkdir -p "$BIN_DIR"
cp nam-audio-pipe "$BIN_DIR/"
chmod +x "$BIN_DIR/nam-audio-pipe"
echo "✅ Installed nam-audio-pipe to $BIN_DIR/nam-audio-pipe"
EOF
    chmod +x "$PKG_DIR/$ARCHIVE_NAME/install.sh"

    # Deterministic archive: sorted member order, normalized mtime (commit
    # timestamp), root-owned with numeric ids — the OS-local UID/GID and current
    # mtimes that previously made every tarball hash differ are pinned out.
    tar --sort=name \
        --mtime="@$SOURCE_DATE_EPOCH" \
        --owner=0 --group=0 --numeric-owner \
        -C "$PKG_DIR" -I "zstd -6 -T0" -cf "$TARBALL" "$ARCHIVE_NAME"
    rm -rf "$PKG_DIR"
    PKG_DIR=""

    echo -e "  ${GREEN}✓${NC} Distribution package generated at: ${BOLD}$TARBALL${NC}"

    # Post-packaging validation: extract into a temporary
    # dir, prove the packaged ELF is byte-for-byte the installed artifact and
    # functionally healthy (--diagnose) — the distributed bytes are exactly
    # the tested bytes.
    TARBALL_VERIFY_DIR="$(mktemp -d -t nam-audio-pipe-tarball-verify.XXXXXX)"
    if ! tar -C "$TARBALL_VERIFY_DIR" -I "zstd -d" -xf "$TARBALL"; then
        rm -rf "$TARBALL_VERIFY_DIR"
        die "Tarball verification failed: could not extract $TARBALL."
    fi
    EXTRACTED_BIN="$TARBALL_VERIFY_DIR/$ARCHIVE_NAME/nam-audio-pipe"
    EXTRACTED_SHA=$(sha256sum "$EXTRACTED_BIN" | awk '{print $1}')
    INSTALLED_SHA=$(sha256sum "$BIN_TARGET" | awk '{print $1}')
    if [ "$EXTRACTED_SHA" != "$INSTALLED_SHA" ]; then
        rm -rf "$TARBALL_VERIFY_DIR"
        die "Tarball verification failed: packaged binary SHA-256 ($EXTRACTED_SHA) differs from the installed artifact ($INSTALLED_SHA)."
    fi
    if ! "$EXTRACTED_BIN" --diagnose > "$TARBALL_VERIFY_DIR/diagnose.out" 2>&1; then
        rm -rf "$TARBALL_VERIFY_DIR"
        die "Tarball verification failed: extracted binary --diagnose exited non-zero."
    fi
    rm -rf "$TARBALL_VERIFY_DIR"
    echo -e "  ${GREEN}✓${NC} Tarball verified: extracted ELF byte-identical to installed binary (SHA-256 $INSTALLED_SHA) and --diagnose passed."
else
    echo -e "\n${YELLOW}[Phase 6/7] Skipping tarball packaging (--no-tarball).${NC}"
fi

# -----------------------------------------------------------------------------
# PHASE 7: Release Packaging (.flatpak)
# -----------------------------------------------------------------------------
if [ "$BUILD_FLATPAK" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 7/7] Generating Flatpak Standalone App Bundle (.flatpak)...${NC}"

    FLATPAK_BUILD_DIR="$(mktemp -d -t nam-audio-pipe-flatpak-build.XXXXXX)"
    FLATPAK_REPO_DIR="$(mktemp -d -t nam-audio-pipe-flatpak-repo.XXXXXX)"

    SDK_NAME="org.freedesktop.Sdk"
    if ! flatpak info org.freedesktop.Sdk//26.08 &>/dev/null; then
        SDK_NAME="org.freedesktop.Platform"
    fi

    echo -e "  Initializing Flatpak application build environment (26.08 using $SDK_NAME)..."
    flatpak build-init \
        "$FLATPAK_BUILD_DIR" \
        io.github.fabiohl.NAMAudioPipe \
        "$SDK_NAME" \
        org.freedesktop.Platform \
        26.08

    mkdir -p "$FLATPAK_BUILD_DIR/files/bin"
    mkdir -p "$FLATPAK_BUILD_DIR/files/share/applications"
    mkdir -p "$FLATPAK_BUILD_DIR/files/share/metainfo"
    mkdir -p "$FLATPAK_BUILD_DIR/files/share/licenses/io.github.fabiohl.NAMAudioPipe"

    cp "$BIN_TARGET" "$FLATPAK_BUILD_DIR/files/bin/nam-audio-pipe"
    chmod +x "$FLATPAK_BUILD_DIR/files/bin/nam-audio-pipe"
    echo -e "  ${GREEN}✓${NC} Installed nam-audio-pipe -> application directory"

    # Packaging metadata (desktop launcher, AppStream metainfo and hicolor icon
    # theme) is mandatory: a Flatpak without indexable AppStream metadata must
    # never be declared a successful release (fail-closed).
    DESKTOP_SRC="packaging/flatpak/io.github.fabiohl.NAMAudioPipe.desktop"
    if [ -f "$DESKTOP_SRC" ]; then
        cp "$DESKTOP_SRC" "$FLATPAK_BUILD_DIR/files/share/applications/"
        echo -e "  ${GREEN}✓${NC} Installed desktop launcher file"
    else
        die "Desktop entry file missing at $DESKTOP_SRC (Flatpak release requires it)."
    fi

    METAINFO_SRC="packaging/flatpak/io.github.fabiohl.NAMAudioPipe.metainfo.xml"
    if [ -f "$METAINFO_SRC" ]; then
        cp "$METAINFO_SRC" "$FLATPAK_BUILD_DIR/files/share/metainfo/"
        echo -e "  ${GREEN}✓${NC} Installed AppStream metainfo XML"
    else
        die "AppStream metainfo file missing at $METAINFO_SRC (Flatpak release requires it)."
    fi

    # Copy hicolor icon theme hierarchy
    ICONS_SRC="packaging/flatpak/icons/hicolor"
    if [ -d "$ICONS_SRC" ]; then
        for size_dir in "$ICONS_SRC"/*; do
            if [ -d "$size_dir" ]; then
                size_name=$(basename "$size_dir")
                target_icon_dir="$FLATPAK_BUILD_DIR/files/share/icons/hicolor/$size_name/apps"
                mkdir -p "$target_icon_dir"
                if [ -d "$size_dir/apps" ]; then
                    cp "$size_dir/apps"/* "$target_icon_dir/" 2>/dev/null || true
                fi
            fi
        done
        echo -e "  ${GREEN}✓${NC} Installed hicolor icon theme files"
    else
        die "Icon directory missing at $ICONS_SRC (Flatpak release requires it)."
    fi

    if [ -f "LICENSE.txt" ]; then
        cp "LICENSE.txt" "$FLATPAK_BUILD_DIR/files/share/licenses/io.github.fabiohl.NAMAudioPipe/"
    elif [ -f "LICENSE" ]; then
        cp "LICENSE" "$FLATPAK_BUILD_DIR/files/share/licenses/io.github.fabiohl.NAMAudioPipe/"
    fi

    # Fail-closed AppStream validation of the official metainfo before export.
    echo -e "  ${YELLOW}${BOLD}appstreamcli validate --no-net --strict $METAINFO_SRC${NC}"
    if ! appstreamcli validate --no-net --strict "$METAINFO_SRC"; then
        die "AppStream metainfo validation failed; refusing to export a Flatpak with invalid metadata."
    fi
    echo -e "  ${GREEN}✓${NC} AppStream metainfo validated (zero errors/warnings)."

    # Synthesize the per-app appstream catalog payload (share/app-info) that
    # `flatpak build-export --update-appstream` aggregates into the OSTree
    # AppStream branch — the equivalent of what appstream-compose produces
    # during a Flatpak module build. Without it, the export emits
    # "No appstream data" and ships an empty, non-indexable catalog.
    APP_INFO_DIR="$FLATPAK_BUILD_DIR/files/share/app-info"
    APP_INFO_XMLS="$APP_INFO_DIR/xmls"
    APP_INFO_ICONS="$APP_INFO_DIR/icons"
    mkdir -p "$APP_INFO_XMLS"
    python3 - "$METAINFO_SRC" "$APP_INFO_XMLS/io.github.fabiohl.NAMAudioPipe.xml.gz" <<'PY'
import gzip
import sys

src_path, dst_path = sys.argv[1], sys.argv[2]
with open(src_path, "r", encoding="utf-8") as f:
    xml_data = f.read()

body = xml_data.split("?>", 1)[-1].strip()
collection_xml = f"""<?xml version="1.0" encoding="utf-8"?>
<components version="1.0">
{body}
</components>
"""
with gzip.open(dst_path, "wt", encoding="utf-8") as f:
    f.write(collection_xml)
PY

    if [ -d "$ICONS_SRC" ]; then
        for size_dir in "$ICONS_SRC"/*; do
            if [ -d "$size_dir" ]; then
                size_name=$(basename "$size_dir")
                if [ "$size_name" != "scalable" ]; then
                    target_appstream_icon_dir="$APP_INFO_ICONS/flatpak/$size_name"
                    mkdir -p "$target_appstream_icon_dir"
                    if [ -d "$size_dir/apps" ]; then
                        cp "$size_dir/apps"/* "$target_appstream_icon_dir/" 2>/dev/null || true
                    fi
                fi
            fi
        done
    fi
    echo -e "  ${GREEN}✓${NC} Generated AppStream catalog payload (share/app-info)."

    echo -e "  Finalizing Flatpak application sandbox parameters..."
    flatpak build-finish "$FLATPAK_BUILD_DIR" \
        --command=nam-audio-pipe \
        --socket=pulseaudio \
        --filesystem=xdg-run/pipewire-0 \
        --share=ipc \
        --device=all \
        --filesystem=home:ro \
        --socket=fallback-x11 \
        --socket=wayland

    echo -e "  Exporting application to temporary OSTree repository..."
    FLATPAK_EXPORT_LOG="$(mktemp -t nam-audio-pipe-flatpak-export.XXXXXX)"
    # LC_ALL=C keeps flatpak's diagnostics locale-independent so the fail-closed
    # "No appstream data" check below cannot be masked by localization.
    # The log is kept OUTSIDE the repo dir: a foreign file inside the OSTree
    # repository root makes flatpak treat it as an invalid existing repo.
    if ! LC_ALL=C flatpak build-export --update-appstream "$FLATPAK_REPO_DIR" "$FLATPAK_BUILD_DIR" stable >"$FLATPAK_EXPORT_LOG" 2>&1; then
        cat "$FLATPAK_EXPORT_LOG" >&2
        rm -f "$FLATPAK_EXPORT_LOG"
        die "flatpak build-export failed (see output above)."
    fi
    if grep -q "No appstream data" "$FLATPAK_EXPORT_LOG"; then
        cat "$FLATPAK_EXPORT_LOG" >&2
        rm -f "$FLATPAK_EXPORT_LOG"
        die "Flatpak export did not synthesize the AppStream catalog (no appstream data for the app)."
    fi
    rm -f "$FLATPAK_EXPORT_LOG"
    FLATPAK_EXPORT_LOG=""

    # Verify the AppStream catalog refs were actually generated inside the OSTree repo.
    if ! flatpak repo "$FLATPAK_REPO_DIR" --branches 2>/dev/null | grep -Eq '^appstream[0-9]*/'; then
        die "AppStream catalog ref(s) missing from the OSTree repository after export."
    fi
    echo -e "  ${GREEN}✓${NC} AppStream catalog synthesized and exported into the OSTree repository."

    echo -e "  Building Flatpak bundle: $FLATPAK_BUNDLE..."
    mkdir -p "$(dirname "$FLATPAK_BUNDLE")"
    flatpak build-bundle "$FLATPAK_REPO_DIR" "$FLATPAK_BUNDLE" io.github.fabiohl.NAMAudioPipe stable

    echo -e "  ${GREEN}✓${NC} Flatpak bundle generated successfully: ${BOLD}$FLATPAK_BUNDLE${NC} ($(du -h "$FLATPAK_BUNDLE" | cut -f1))"

    # -----------------------------------------------------------------------
    # Post-build smoke test: verify the bundle integrity and its manifest by
    # importing it into a fresh temporary OSTree repository (bundle corruption,
    # missing refs or wrong metadata fail the release here).
    # -----------------------------------------------------------------------
    echo -e "  Running post-build Flatpak bundle smoke test..."
    FLATPAK_SMOKE_REPO="$(mktemp -d -t nam-audio-pipe-flatpak-smoke.XXXXXX)"
    FLATPAK_APP_REF="app/io.github.fabiohl.NAMAudioPipe/x86_64/stable"
    mkdir -p "$FLATPAK_SMOKE_REPO/objects" "$FLATPAK_SMOKE_REPO/refs/heads" \
        "$FLATPAK_SMOKE_REPO/refs/remotes" "$FLATPAK_SMOKE_REPO/refs/mirrors" \
        "$FLATPAK_SMOKE_REPO/tmp" "$FLATPAK_SMOKE_REPO/state"
    printf '[core]\nrepo_version=1\nmode=archive-z2\n' > "$FLATPAK_SMOKE_REPO/config"

    if ! flatpak build-import-bundle --update-appstream "$FLATPAK_SMOKE_REPO" "$FLATPAK_BUNDLE" >/dev/null 2>&1; then
        die "Flatpak bundle integrity check failed: could not import $FLATPAK_BUNDLE into the smoke-test repository."
    fi
    if ! flatpak repo "$FLATPAK_SMOKE_REPO" --branches 2>/dev/null | grep -q "$FLATPAK_APP_REF"; then
        die "Flatpak bundle smoke test failed: app ref $FLATPAK_APP_REF not found after bundle import."
    fi
    if ! flatpak repo "$FLATPAK_SMOKE_REPO" --branches 2>/dev/null | grep -Eq '^appstream[0-9]*/'; then
        die "Flatpak bundle smoke test failed: AppStream catalog branch not found after bundle import."
    fi
    if ! flatpak repo "$FLATPAK_SMOKE_REPO" --metadata="$FLATPAK_APP_REF" 2>/dev/null \
        | grep -q '^command=nam-audio-pipe$'; then
        die "Flatpak bundle smoke test failed: manifest does not declare command=nam-audio-pipe."
    fi
    echo -e "  ${GREEN}✓${NC} Flatpak bundle integrity verified (import + manifest inspection)."

    echo -e "  Running in-sandbox Flatpak smoke test..."
    FLATPAK_BIN_SHA=$(sha256sum "$FLATPAK_BUILD_DIR/files/bin/nam-audio-pipe" | awk '{print $1}')
    INSTALLED_SHA=$(sha256sum "$BIN_TARGET" | awk '{print $1}')
    if [ "$FLATPAK_BIN_SHA" != "$INSTALLED_SHA" ]; then
        die "Flatpak smoke test failed: packaged binary SHA-256 ($FLATPAK_BIN_SHA) differs from provenanced binary ($INSTALLED_SHA)."
    fi
    if ! flatpak build "$FLATPAK_BUILD_DIR" nam-audio-pipe --diagnose >/dev/null 2>&1; then
        die "In-sandbox Flatpak smoke test failed (flatpak build nam-audio-pipe --diagnose returned non-zero)."
    fi
    echo -e "  ${GREEN}✓${NC} In-sandbox Flatpak smoke test passed (binary SHA matched provenanced ELF, --diagnose succeeded)."

    if [ "$DO_INSTALL_FLATPAK" = true ]; then
        echo -e "  Installing Flatpak application locally for current user..."
        flatpak install --user --reinstall -y "$FLATPAK_BUNDLE"
        echo -e "  ${GREEN}✓${NC} Flatpak application installed successfully."
    fi

    rm -rf "$FLATPAK_SMOKE_REPO"
    FLATPAK_SMOKE_REPO=""
    rm -rf "$FLATPAK_BUILD_DIR" "$FLATPAK_REPO_DIR"
    FLATPAK_BUILD_DIR=""
    FLATPAK_REPO_DIR=""
else
    echo -e "\n${YELLOW}[Phase 7/7] Skipping Flatpak packaging (--no-flatpak).${NC}"
fi

# -----------------------------------------------------------------------------
# PROVENANCE: cryptographic chain of custody
# -----------------------------------------------------------------------------
# Generated after every delivery artifact exists so the receipt can bind the
# installed stripped ELF, tarball, Flatpak bundle and AppStream metadata to the
# clean commit, exact toolchain and build flags that produced them.
echo -e "\n${BLUE}${BOLD}Generating release provenance receipt...${NC}"
write_provenance_receipt

echo -e "\n${GREEN}${BOLD}================================================================================${NC}"
echo -e "${GREEN}${BOLD}   Pipeline completed! Artifacts ready for distribution:                ${NC}"
echo -e "  ${BOLD}Artifacts saved:${NC}"
echo -e "    - Binary Executable: ${CYAN}$BIN_TARGET${NC}"
if [ "$BUILD_TARBALL" = true ]; then
    echo -e "    - Tarball:           ${CYAN}$TARBALL${NC}"
fi
if [ "$BUILD_FLATPAK" = true ]; then
    echo -e "    - Flatpak Bundle:    ${CYAN}$FLATPAK_BUNDLE${NC}"
fi
if [ -f "$PROVENANCE_RECEIPT" ]; then
    echo -e "    - Provenance:        ${CYAN}$PROVENANCE_RECEIPT${NC}"
fi
if [ -f "$PROJECT_DIR/target/dsp_hotpath.asm" ]; then
    echo -e "    - Assembly ASM:      ${CYAN}$PROJECT_DIR/target/dsp_hotpath.asm${NC}"
fi
echo -e "${GREEN}${BOLD}================================================================================${NC}\n"
