#!/bin/bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
#
# Unified compiler-grade release build script for nam-audio-pipe (PGO + BOLT).
# Compiles the standalone binary with Profile-Guided Optimization (PGO)
# and post-link BOLT binary reordering.
#
# Deliverables:
#   - ~/.local/bin/nam-audio-pipe                     (PGO + BOLT optimized standalone binary)
#   - target/dsp_hotpath.asm                          (Disassembly hotspot report)
#   - ~/nam-audio-pipe-v<ver>-linux-x86_64-v3.tar.zst (Release distribution tarball)

set -euo pipefail

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

# (SCRIPT_DIR and PROJECT_DIR are already set and cd performed above.)

# State tracking for signal safety and cleanup
ORIG_PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "2")
PARANOID_MODIFIED=false
NAM_PID=""
PLAY_PID=""

# Dynamic isolated temporary directories for PGO & BOLT profiling
PGO_DIR="$(mktemp -d -t nam-audio-pipe-pgo.XXXXXX)"
BOLT_DIR="$(mktemp -d -t nam-audio-pipe-bolt.XXXXXX)"
PROFRAW_DIR="$PGO_DIR/profraw"
MERGED_PROFILE="$PGO_DIR/merged.profdata"
ORIG_RUSTFLAGS="${RUSTFLAGS:-}"

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
    if [ -n "${PGO_DIR:-}" ] && [ -d "$PGO_DIR" ]; then
        rm -rf "$PGO_DIR"
    fi
    if [ -n "${BOLT_DIR:-}" ] && [ -d "$BOLT_DIR" ]; then
        rm -rf "$BOLT_DIR"
    fi
}
trap cleanup EXIT INT TERM

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

# -----------------------------------------------------------------------------
# PHASE 1: Environment & Dependency Verification
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 1/6] Verifying dependencies and environment...${NC}"

# Verify core dependencies
for cmd in rustc cargo python3 tar zstd; do
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

# Locate llvm-profdata from Rustup toolchain
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

# Locate LLVM BOLT binary
LLVM_BOLT=""
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
        # Try passwordless sudo first; if it succeeds the value is already applied.
        if sudo -n sysctl -q -w kernel.perf_event_paranoid=1 2>/dev/null; then
            PARANOID_MODIFIED=true
            echo -e "  ${GREEN}✓${NC} paranoid level set to 1 (passwordless sudo)."
        elif [ -t 0 ]; then
            # Interactive terminal: prompt for password once.
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

HAS_PERF=false
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

# -----------------------------------------------------------------------------
# PHASE 2: Profile-Guided Optimization (PGO) - Offline DSP Workload
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 2/6] Generating PGO profiles via offline DSP workload...${NC}"

export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Cprofile-generate=$PROFRAW_DIR"
export LLVM_PROFILE_FILE="$PROFRAW_DIR/default_%m_%p.profraw"
echo -e "  Using RUSTFLAGS: ${BOLD}$RUSTFLAGS${NC}"

echo -e "  Compiling and running PGO profiling workload (pgo_workload)..."
cargo run --profile dist --features testing --bin pgo_workload || {
    echo -e "${RED}Error: pgo_workload failed. Cannot generate PGO profiles.${NC}"
    exit 1
}

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

# -----------------------------------------------------------------------------
# PHASE 3: Compile PGO-Optimized Standalone Binary
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 3/6] Compiling PGO-optimized binary...${NC}"

if [ -f "$MERGED_PROFILE" ]; then
    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS -Cprofile-use=$MERGED_PROFILE"
else
    export RUSTFLAGS="$CONFIG_RUSTFLAGS $ORIG_RUSTFLAGS"
    warn "No PGO profile available — compiling without PGO."
fi
echo -e "  Using RUSTFLAGS: ${BOLD}$RUSTFLAGS${NC}"

echo -e "  Building standalone executable..."
# -C strip=none overrides the [profile.dist] strip=true setting so that the
# binary retains symbol tables — LLVM BOLT requires these for function and
# basic-block reordering. Stripping is applied manually in Phase 5 after BOLT.
# -Clink-arg=-Wl,-q emits relocation sections required by BOLT.
RUSTFLAGS="$RUSTFLAGS -C strip=none -Clink-arg=-Wl,-q" cargo build --profile dist

PGO_BIN="$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe"
if [ ! -f "$PGO_BIN" ]; then
    echo -e "${RED}Error: Failed to find compiled standalone binary at $PGO_BIN${NC}"
    exit 1
fi
echo -e "  ${GREEN}✓${NC} PGO compilation completed successfully."

# -----------------------------------------------------------------------------
# PHASE 4: BOLT Post-Link Optimization
# -----------------------------------------------------------------------------
BOLT_APPLIED=false

if [ -n "$LLVM_BOLT" ] && [ "$HAS_PERF" = true ]; then
    echo -e "\n${BLUE}${BOLD}[Phase 4/6] Applying BOLT post-link optimization...${NC}"

    PW_RUNNING=false
    if command -v pw-cli &>/dev/null && (pw-cli info 0 &>/dev/null || pgrep -x pipewire &>/dev/null); then
        PW_RUNNING=true
    fi

    MODEL_FILES=()
    MODEL_FILES_STR=$(python3 -c '
import os, sys

search_dirs = []
if os.environ.get("NAM_FIXTURES_DIR"):
    search_dirs.append(os.environ["NAM_FIXTURES_DIR"])
search_dirs.append("tests/fixtures/models")

# License-safe in-repo fixtures only (tests/fixtures/models/).
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
            for f in os.listdir(d):
                if f.endswith(".nam"):
                    p = os.path.join(d, f)
                    if p not in resolved:
                        resolved.append(p)
                        break
        if resolved:
            break

print("\n".join(resolved))
' 2>/dev/null || echo "")

    if [ -n "$MODEL_FILES_STR" ]; then
        mapfile -t MODEL_FILES <<< "$MODEL_FILES_STR"
    fi

    if [ "$PW_RUNNING" = true ] && [ ${#MODEL_FILES[@]} -gt 0 ]; then
        echo -e "  PipeWire detected! Starting multi-model profiling across ${#MODEL_FILES[@]} topology families..."
        for path in "${MODEL_FILES[@]}"; do
            echo -e "    - $(basename "$path")"
        done

        TEST_WAV="$BOLT_DIR/test_signal.wav"
        if [ ! -f "$TEST_WAV" ]; then
            python3 -c "
import wave, struct, math
rate = 48000
duration = 3
n = rate * duration
with wave.open('$TEST_WAV', 'w') as w:
    w.setnchannels(1)
    w.setsampwidth(2)
    w.setframerate(rate)
    for i in range(n):
        val = int(32767 * 0.5 * math.sin(2 * math.pi * 440 * i / rate))
        w.writeframes(struct.pack('<h', val))
" &>/dev/null || true
        fi

        USE_LBR=false
        if perf record -e cycles:u -j any,u -F 99 -o /dev/null -- true &>/dev/null; then
            USE_LBR=true
        fi

        rm -f "$BOLT_DIR/perf.data"
        MODEL_COUNT=0

        for model_file in "${MODEL_FILES[@]}"; do
            MODEL_COUNT=$((MODEL_COUNT + 1))
            echo -e "  [Model $MODEL_COUNT/${#MODEL_FILES[@]}] Profiling with: ${BOLD}$(basename "$model_file")${NC}"

            NAM_DISABLE_GATE=1 "$PGO_BIN" -m "$model_file" -b 64 &
            NAM_PID=$!
            sleep 1.0

            if kill -0 "$NAM_PID" 2>/dev/null; then
                if [ "$USE_LBR" = "true" ]; then
                    PERF_ARGS=(-F 99 -e cycles:u -j any,u -p "$NAM_PID" -o "$BOLT_DIR/perf.data")
                else
                    PERF_ARGS=(-F 4000 -e cycles:u -p "$NAM_PID" -o "$BOLT_DIR/perf.data")
                fi

                if [ "$MODEL_COUNT" -gt 1 ]; then
                    PERF_ARGS+=("--append")
                fi

                if [ -f "$TEST_WAV" ] && command -v pw-play &>/dev/null; then
                    pw-play --target="NAM-rs-input" "$TEST_WAV" &
                    PLAY_PID=$!
                    perf record "${PERF_ARGS[@]}" -- sleep 2 &>/dev/null || true
                    kill "$PLAY_PID" 2>/dev/null || true
                    wait "$PLAY_PID" 2>/dev/null || true
                    PLAY_PID=""
                else
                    perf record "${PERF_ARGS[@]}" -- sleep 2 &>/dev/null || true
                fi

                kill "$NAM_PID" 2>/dev/null || true
                wait "$NAM_PID" 2>/dev/null || true
                NAM_PID=""
            else
                echo -e "${YELLOW}  Warning: nam-audio-pipe failed to start with $(basename "$model_file").${NC}"
                NAM_PID=""
            fi
        done

        if [ -f "$BOLT_DIR/perf.data" ] && [ -s "$BOLT_DIR/perf.data" ]; then
            PERF2BOLT_FLAGS=()
            if [ "$USE_LBR" = "false" ]; then
                # --basic-events was renamed across LLVM versions. Probe the flag
                # before use so the build degrades gracefully on older toolchains.
                if "$PERF2BOLT" --help 2>&1 | grep -q -- '--basic-events'; then
                    PERF2BOLT_FLAGS+=("--basic-events")
                else
                    warn "perf2bolt --basic-events not supported by this toolchain; omitting flag."
                fi
            fi

            if "$PERF2BOLT" "${PERF2BOLT_FLAGS[@]}" -p "$BOLT_DIR/perf.data" "$PGO_BIN" -o "$BOLT_DIR/perf.fdata" --ignore-build-id > "$BOLT_DIR/perf2bolt.log" 2>&1; then
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
                    echo -e "${YELLOW}  Warning: llvm-bolt command failed. Reverting to standard PGO binary.${NC}"
                    if [ -f "$BOLT_DIR/llvm-bolt.log" ]; then
                        echo -e "${YELLOW}  --- llvm-bolt log tail ---${NC}"
                        tail -n 10 "$BOLT_DIR/llvm-bolt.log"
                    fi
                fi
            else
                echo -e "${YELLOW}  Warning: perf2bolt failed to convert data. Reverting to standard PGO binary.${NC}"
                if [ -f "$BOLT_DIR/perf2bolt.log" ]; then
                    echo -e "${YELLOW}  --- perf2bolt log tail ---${NC}"
                    tail -n 10 "$BOLT_DIR/perf2bolt.log"
                fi
            fi
        fi
    else
        echo -e "${YELLOW}  Warning: PipeWire is not running or no model files found. Skipping BOLT.${NC}"
    fi
else
    echo -e "\n${YELLOW}[Phase 4/6] Skipping BOLT (llvm-bolt or perf not available/configured).${NC}"
fi

# -----------------------------------------------------------------------------
# PHASE 4.5: Assembly Hotspot Disassembly Report
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 4.5/6] Generating AI-ready assembly hotspot report...${NC}"

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

# -----------------------------------------------------------------------------
# PHASE 5: Deliverables Installation & Verification
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 5/6] Installing and validating artifact...${NC}"

mkdir -p "$BIN_INSTALL_DIR"

rm -f "$BIN_TARGET"
if [ "$BOLT_APPLIED" = true ] && [ -f "$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe.bolt" ]; then
    cp "$PGO_BUILD_TARGET_DIR/dist/nam-audio-pipe.bolt" "$BIN_TARGET"
    strip --strip-all "$BIN_TARGET"
    echo -e "  Installed executable (PGO + BOLT): $BIN_TARGET"
else
    cp "$PGO_BIN" "$BIN_TARGET"
    strip --strip-all "$BIN_TARGET"
    echo -e "  Installed executable (PGO): $BIN_TARGET"
fi
chmod +x "$BIN_TARGET"

# Validate the installed binary is functional before declaring success.
echo -e "  Validating installed binary integrity (--diagnose)..."
if "$BIN_TARGET" --diagnose > /dev/null 2>&1; then
    echo -e "  ${GREEN}✓${NC} Binary integrity verified (--diagnose exited 0)."
else
    echo -e "${RED}${BOLD}Error: Installed binary failed --diagnose check. The artifact may be corrupt.${NC}"
    echo -e "${YELLOW}  Check $BIN_TARGET manually and re-run the build pipeline.${NC}"
    exit 1
fi

# -----------------------------------------------------------------------------
# PHASE 6: Release Packaging (.tar.zst)
# -----------------------------------------------------------------------------
echo -e "\n${BLUE}${BOLD}[Phase 6/6] Generating distribution tarball...${NC}"

VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c "import sys, json; print(json.load(sys.stdin)['packages'][0]['version'])")
ARCHIVE_NAME="nam-audio-pipe-v${VERSION}-linux-x86_64-v3"
# mktemp creates a unique temporary directory — do not destroy it immediately;
# simply create the archive subdirectory inside it.
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

TARBALL="$HOME/${ARCHIVE_NAME}.tar.zst"
tar -C "$PKG_DIR" -I "zstd -6 -T0" -cf "$TARBALL" "$ARCHIVE_NAME"
rm -rf "$PKG_DIR"

echo -e "  ${GREEN}✓${NC} Distribution package generated at: ${BOLD}$TARBALL${NC}"

echo -e "${GREEN}${BOLD}================================================================${NC}"
echo -e "${GREEN}${BOLD}   Pipeline completed! Standalone binary ready for distribution.${NC}"
echo -e "${GREEN}${BOLD}================================================================${NC}"
ls -lath "$BIN_TARGET" "$TARBALL"
if [ -f "$PROJECT_DIR/target/dsp_hotpath.asm" ]; then
    echo -e "\n${YELLOW}${BOLD}💡 AI-Ready Assembly Hotspots generated at:${NC} ${BOLD}target/dsp_hotpath.asm${NC}"
fi
echo -e "${GREEN}${BOLD}================================================================${NC}"
