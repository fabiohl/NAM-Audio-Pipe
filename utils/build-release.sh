#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# build-release.sh — builds the canonical Pipe artifact (PGO+BOLT), packages it and emits the typed build receipt. Classe C.
# Uso: utils/build-release.sh [--strict] [--package] [--install] [--certify] [-h|--help]
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

SCRIPT_NAME=build-release
SCRIPT_DIR="$PWD/utils"
LOG_DIR=target/logs
DIST_DIR=target/dist
DIST_BIN="$DIST_DIR/nam-audio-pipe"
BUILD_RECEIPT="$DIST_DIR/build-receipt.json"
RELEASE_RECEIPT="$DIST_DIR/release-receipt.json"
LONG_JSONL="$LOG_DIR/long-audit-receipt.jsonl"
LONG_TEXT="$LOG_DIR/long-receipt.txt"
PGO_RECEIPT="$LOG_DIR/pgo-workload-receipt.json"
CEREMONY_RECEIPT="$LOG_DIR/ab-opt-ceremony-receipt.json"
STRICT=0
DO_STRICT=0
DO_PACKAGE=0
DO_INSTALL=0
DO_CERTIFY=0
FAILS=()
GAPS=()
T0=$SECONDS
mkdir -p "$LOG_DIR" "$DIST_DIR"

phase() { echo "PHASE: $1"; }
gap() {
    local known
    for known in ${GAPS[@]+"${GAPS[@]}"}; do
        if [ "$known" = "$1" ]; then return 0; fi
    done
    GAPS+=("$1")
    echo "PHASE: $1 GAP"
}
fail_phase() {
    FAILS+=("$1")
    echo "PHASE: $1 FAIL rc=$2 log=$3"
}
finish() {
    local status=PASS fails=none gaps=none
    if [ ${#FAILS[@]} -gt 0 ]; then fails="${FAILS[*]}"; fi
    if [ ${#GAPS[@]} -gt 0 ]; then gaps="${GAPS[*]}"; fi
    if [ ${#GAPS[@]} -gt 0 ]; then status=PASS_WITH_GAPS; fi
    if [ ${#FAILS[@]} -gt 0 ] || { [ "$STRICT" = 1 ] && [ ${#GAPS[@]} -gt 0 ]; }; then status=FAIL; fi
    echo "RESULT: $status ($((SECONDS - T0))s) fails=$fails gaps=$gaps"
    [ "$status" != FAIL ]
}

show_help() {
    cat <<'EOF'
NAM-Audio-Pipe Release Build (Class C)

Usage:
  utils/build-release.sh [flags]

Canonical flags:
  --strict     Fail-closed: PGO and a proven BOLT are mandatory; any gap aborts.
  --package    Also produce the .tar.zst and Flatpak bundle in target/dist/.
  --install    Install the canonical binary into ~/.local/bin (and the bundle, if packaged).
  --certify    Do not build: reconcile the artifact, build receipt and long audit.
  -h, --help   Show this help.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --strict) DO_STRICT=1; STRICT=1; shift ;;
        --package) DO_PACKAGE=1; shift ;;
        --install) DO_INSTALL=1; shift ;;
        --certify) DO_CERTIFY=1; shift ;;
        -h|--help) show_help; exit 0 ;;
        *) echo "build-release.sh: unknown flag '$1'" >&2; exit 2 ;;
    esac
done

VERSION=$(awk -F'"' '/^version = / {print $2; exit}' Cargo.toml)
ARCHIVE_NAME="nam-audio-pipe-v${VERSION}-linux-x86_64-v3"
TARBALL="$DIST_DIR/$ARCHIVE_NAME.tar.zst"
FLATPAK_BUNDLE="$DIST_DIR/$ARCHIVE_NAME.flatpak"
GIT_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo unknown)
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)
GIT_DIRTY=false
if [ -n "$(git status --porcelain 2>/dev/null || true)" ]; then GIT_DIRTY=true; fi

find_receipt_bin() {
    local candidate
    for candidate in target/long/release/pipe_long_receipt target/release/pipe_long_receipt target/debug/pipe_long_receipt; do
        if [ -x "$candidate" ] && "$candidate" --help 2>/dev/null | grep 'text-receipt' >/dev/null; then
            echo "$candidate"
            return 0
        fi
    done
    return 1
}

# ---------------------------------------------------------------------------
# --certify: validation only, never compiles or re-runs the artifact.
# ---------------------------------------------------------------------------
if [ "$DO_CERTIFY" = 1 ]; then
    phase certify
    CERT_LOG="$LOG_DIR/build-release-certify.log"
    CERT_ARGS=(--artifact "$DIST_BIN" --build-receipt "$BUILD_RECEIPT" --long-receipt "$LONG_JSONL" --text-receipt "$LONG_TEXT" --out "$RELEASE_RECEIPT")
    if [ "$DO_STRICT" = 1 ]; then CERT_ARGS+=(--strict); fi
    RC=0
    if CERT_BIN=$(find_receipt_bin); then
        if ! "$CERT_BIN" certify "${CERT_ARGS[@]}" 2>&1 | tee "$CERT_LOG"; then RC=${PIPESTATUS[0]}; fi
    else
        if ! cargo run --release --quiet --locked --features testing --bin pipe_long_receipt -- \
            certify "${CERT_ARGS[@]}" 2>&1 | tee "$CERT_LOG"; then RC=${PIPESTATUS[0]}; fi
    fi
    if [ "$RC" -eq 0 ]; then ok "release certified: $RELEASE_RECEIPT"; else fail_phase certify "$RC" "$CERT_LOG"; fi
    finish
    exit $?
fi

# A failed build must never leave a previous receipt that could be mistaken
# for this artifact's provenance.
rm -f "$BUILD_RECEIPT" "$RELEASE_RECEIPT"

# ---------------------------------------------------------------------------
# Phase 1: environment, toolchain and canonical paths.
# ---------------------------------------------------------------------------
phase env_verify
ENV_LOG="$LOG_DIR/build-release-env_verify.log"
if ! {
    for cmd in rustc cargo; do
        command -v "$cmd" >/dev/null || { echo "missing required tool: $cmd"; exit 1; }
    done
    if [ "$DO_PACKAGE" = 1 ]; then
        for cmd in tar zstd flatpak appstreamcli; do
            command -v "$cmd" >/dev/null || { echo "missing packaging tool: $cmd"; exit 1; }
        done
    fi
    rustc --version
    cargo --version
} 2>&1 | tee "$ENV_LOG"; then
    fail_phase env_verify 1 "$ENV_LOG"
    finish
    exit 1
fi

CONFIG_RUSTFLAGS=$(awk -F'"' '/^  "-C/ || /^  "-Wl/ || /^  "-/ {if ($2 != "") printf "%s ", $2}' .cargo/config.toml 2>/dev/null || true)
if [ -z "$CONFIG_RUSTFLAGS" ]; then
    fail_phase env_verify 1 "$ENV_LOG"
    die "cannot extract rustflags from .cargo/config.toml"
fi
BASE_RUSTFLAGS="${CONFIG_RUSTFLAGS}${RUSTFLAGS:+ $RUSTFLAGS}"

RUST_HOST=$(rustc -vV | sed -n 's/^host: //p')
RUST_SYSROOT=$(rustc --print sysroot)
LLVM_PROFDATA="$RUST_SYSROOT/lib/rustlib/$RUST_HOST/bin/llvm-profdata"
if [ ! -x "$LLVM_PROFDATA" ]; then LLVM_PROFDATA=$(command -v llvm-profdata 2>/dev/null || true); fi
if [ -n "$LLVM_PROFDATA" ] && [ ! -x "$LLVM_PROFDATA" ]; then LLVM_PROFDATA=""; fi
if [ -z "$LLVM_PROFDATA" ]; then
    gap pgo_profdata_unavailable
    if [ "$DO_STRICT" = 1 ]; then
        finish || true
        die "llvm-profdata is mandatory under --strict (rustup component add llvm-tools-preview)"
    fi
fi

LLVM_BOLT=""
for candidate in llvm-bolt llvm-bolt-19 llvm-bolt-18; do
    if command -v "$candidate" >/dev/null 2>&1; then LLVM_BOLT="$candidate"; break; fi
done
if [ -z "$LLVM_BOLT" ]; then
    for candidate in /usr/lib/llvm-*/bin/llvm-bolt; do
        if [ -x "$candidate" ]; then LLVM_BOLT="$candidate"; break; fi
    done
fi
PERF2BOLT=""
HAS_PERF=false
if [ -n "$LLVM_BOLT" ]; then
    PERF2BOLT="$(dirname "$LLVM_BOLT")/perf2bolt"
    if [ ! -x "$PERF2BOLT" ]; then PERF2BOLT=$(command -v perf2bolt 2>/dev/null || true); fi
fi
if command -v perf >/dev/null 2>&1; then
    PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
    if [ "$PARANOID" -le 1 ]; then HAS_PERF=true; fi
fi
if [ -z "$LLVM_BOLT" ] || [ -z "$PERF2BOLT" ] || [ "$HAS_PERF" != true ]; then
    gap bolt_tools_unavailable
    if [ "$DO_STRICT" = 1 ]; then
        finish || true
        die "BOLT (llvm-bolt + perf2bolt + perf with kernel.perf_event_paranoid<=1) is mandatory under --strict"
    fi
fi

if ! cargo build --locked --features testing --bin pipe_long_receipt >/dev/null 2>&1; then
    fail_phase env_verify 1 "$ENV_LOG"
    finish
    die "cannot build the typed receipt tool (pipe_long_receipt)"
fi
RECEIPT_BIN=target/debug/pipe_long_receipt
ok "environment ready (rustflags: $CONFIG_RUSTFLAGS)"

# ---------------------------------------------------------------------------
# Phase 2: PGO workload and profile merge.
# ---------------------------------------------------------------------------
PGO_APPLIED=false
rm -f "$PGO_RECEIPT"
PGO_DIR="$PWD/target/pgo-profile"
PROFRAW_DIR="$PGO_DIR/profraw"
MERGED_PROFILE="$PGO_DIR/merged.profdata"
PGO_LOG="$LOG_DIR/build-release-pgo_profile.log"
if [ -n "$LLVM_PROFDATA" ]; then
    phase pgo_profile
    rm -rf "$PGO_DIR"
    mkdir -p "$PROFRAW_DIR"
    PGO_GEN_RUSTFLAGS="$BASE_RUSTFLAGS -Cprofile-generate=$PROFRAW_DIR"
    if {
        RUSTFLAGS="$PGO_GEN_RUSTFLAGS" LLVM_PROFILE_FILE="$PROFRAW_DIR/default_%m_%p.profraw" \
            cargo run --locked --profile dist --features testing --target-dir target/pgo-build \
            --bin pgo_workload -- --receipt "$PGO_RECEIPT"
        "$RECEIPT_BIN" pgo-check --receipt "$PGO_RECEIPT"
        PROFRAW_COUNT=$(find "$PROFRAW_DIR" -name '*.profraw' | wc -l)
        [ "$PROFRAW_COUNT" -gt 0 ] || { echo "no .profraw profiles generated"; exit 1; }
        "$LLVM_PROFDATA" merge -sparse -o "$MERGED_PROFILE" "$PROFRAW_DIR"/*.profraw
        [ -s "$MERGED_PROFILE" ]
    } 2>&1 | tee "$PGO_LOG"; then
        PGO_APPLIED=true
        ok "PGO profile merged: $MERGED_PROFILE"
    else
        fail_phase pgo_profile "${PIPESTATUS[0]}" "$PGO_LOG"
        if [ "$DO_STRICT" = 1 ]; then finish; exit 1; fi
        gap pgo_not_applied
    fi
    rm -rf "$PROFRAW_DIR"
else
    phase pgo_profile
    echo "PGO disabled (llvm-profdata unavailable)"
    gap pgo_not_applied
fi

# ---------------------------------------------------------------------------
# Phase 3: optimized binary (never stripped here; BOLT needs symbols).
# ---------------------------------------------------------------------------
phase compile_binary
COMPILE_LOG="$LOG_DIR/build-release-compile_binary.log"
COMPILE_RUSTFLAGS="$BASE_RUSTFLAGS"
if [ "$PGO_APPLIED" = true ] && [ -s "$MERGED_PROFILE" ]; then
    COMPILE_RUSTFLAGS="$COMPILE_RUSTFLAGS -Cprofile-use=$MERGED_PROFILE"
fi
RELEASE_RUSTFLAGS="$COMPILE_RUSTFLAGS -C strip=none -Clink-arg=-Wl,-q"
if ! {
    RUSTFLAGS="$RELEASE_RUSTFLAGS" cargo build --locked --profile dist --target-dir target/pgo-build --bin nam-audio-pipe
} 2>&1 | tee "$COMPILE_LOG"; then
    fail_phase compile_binary "${PIPESTATUS[0]}" "$COMPILE_LOG"
    finish
    exit 1
fi
PGO_BIN=target/pgo-build/dist/nam-audio-pipe
[ -f "$PGO_BIN" ] || { fail_phase compile_binary 1 "$COMPILE_LOG"; finish; exit 1; }
ok "optimized binary: $PGO_BIN"

# ---------------------------------------------------------------------------
# Phase 4: BOLT post-link optimization (live PipeWire profiling).
# ---------------------------------------------------------------------------
phase bolt_optimize
BOLT_LOG="$LOG_DIR/build-release-bolt_optimize.log"
BOLT_DIR=target/bolt-profile
BOLT_APPLIED=false
BOLT_CAUSE=""
BOLT_VERSION=""
BOLT_PROFILE=""
rm -rf "$BOLT_DIR"
mkdir -p "$BOLT_DIR"

if [ "$HAS_PERF" != true ] || [ -z "$LLVM_BOLT" ] || [ -z "$PERF2BOLT" ]; then
    BOLT_CAUSE="BOLT tools or unprivileged perf unavailable"
elif ! command -v pw-cli >/dev/null 2>&1 || ! { pw-cli info 0 >/dev/null 2>&1 || pgrep -x pipewire >/dev/null 2>&1; }; then
    BOLT_CAUSE="PipeWire daemon is not running"
elif ! command -v pw-play >/dev/null 2>&1; then
    BOLT_CAUSE="pw-play is required to drive the profiling graph"
else
    MODEL_FILES=()
    for model in tests/fixtures/models/wavenet_a1_standard.nam tests/fixtures/models/a2_example.nam tests/fixtures/models/lstm.nam; do
        if [ -f "$model" ]; then MODEL_FILES+=("$model"); fi
    done
    if [ ${#MODEL_FILES[@]} -eq 0 ]; then
        mapfile -t MODEL_FILES < <(find tests/fixtures/models -maxdepth 1 -name '*.nam' | sort | head -n 3)
    fi
    TEST_WAV="$BOLT_DIR/profiling_signal.wav"
    if [ ${#MODEL_FILES[@]} -eq 0 ]; then
        BOLT_CAUSE="no fixture model found"
    elif ! {
        # 2 s of 48 kHz mono PCM16 silence: with --gate off the DSP still runs.
        printf 'RIFF\x24\xee\x02\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x80\xbb\x00\x00\x00\x77\x01\x00\x02\x00\x10\x00data\x00\xee\x02\x00'
        dd if=/dev/zero bs=192000 count=1 2>/dev/null
    } > "$TEST_WAV" || [ "$(stat -c%s "$TEST_WAV")" -ne 192044 ]; then
        BOLT_CAUSE="cannot generate the profiling signal"
    fi
fi

if [ -z "$BOLT_CAUSE" ]; then
    PROFILE_DRIVER="$BOLT_DIR/profile_driver.sh"
    cat > "$PROFILE_DRIVER" <<'EOF'
#!/usr/bin/env bash
set -u
bin="$1"; wav="$2"; shift 2
node_ready() { pw-cli ls Node 2>/dev/null | grep -q 'node.name = "NAM-Audio-Pipe-input"'; }
ticks() { awk '{print $14 + $15}' "/proc/$1/stat" 2>/dev/null || echo 0; }
for model in "$@"; do
    "$bin" -m "$model" -b 64 --gate off & pid=$!
    ready=0
    for _ in $(seq 1 40); do
        kill -0 "$pid" 2>/dev/null || break
        if node_ready; then ready=1; break; fi
        sleep .25
    done
    if [ "$ready" != 1 ]; then
        echo "capture node not registered for $model" >&2
        kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; exit 1
    fi
    pw-play --target="NAM-Audio-Pipe-input" "$wav" >/dev/null 2>&1 & play=$!
    sleep .5
    t1=$(ticks "$pid"); sleep .5; t2=$(ticks "$pid")
    kill "$play" "$pid" 2>/dev/null || true
    wait "$play" 2>/dev/null || true; wait "$pid" 2>/dev/null || true
    if [ "$t2" -le "$t1" ]; then
        echo "no sample consumption for $model" >&2
        exit 1
    fi
done
EOF
    chmod +x "$PROFILE_DRIVER"
    PERF_ARGS=(-F 4000 -e cycles:u)
    if perf record -e cycles:u -j any,u -F 99 -o /dev/null -- true >/dev/null 2>&1; then
        PERF_ARGS=(-F 99 -e cycles:u -j any,u)
    fi
    printf 'BOLT profiling %s model(s)\n' "${#MODEL_FILES[@]}" >> "$BOLT_LOG"
    if ! perf record "${PERF_ARGS[@]}" -o "$BOLT_DIR/perf.data" -- \
        "$PROFILE_DRIVER" "$PGO_BIN" "$TEST_WAV" "${MODEL_FILES[@]}" >> "$BOLT_LOG" 2>&1; then
        BOLT_CAUSE="perf record failed"
    fi
fi

PERF_SAMPLES=0
if [ -z "$BOLT_CAUSE" ]; then
    PERF_SIZE=$(stat -c%s "$BOLT_DIR/perf.data" 2>/dev/null || echo 0)
    PERF_SAMPLES=$(perf script -i "$BOLT_DIR/perf.data" 2>/dev/null | wc -l || echo 0)
    if [ "$PERF_SIZE" -lt 8192 ] || [ "$PERF_SAMPLES" -lt 500 ]; then
        BOLT_CAUSE="perf.data has ${PERF_SAMPLES} samples (${PERF_SIZE} bytes); workload did not run"
    fi
fi

if [ -z "$BOLT_CAUSE" ]; then
    ELF_BID=$(LC_ALL=C readelf -n "$PGO_BIN" 2>/dev/null | grep -oP 'Build ID:\s+\K[0-9a-fA-F]+' | head -n1 | tr 'A-F' 'a-f' || true)
    PERF_BID=$(LC_ALL=C perf buildid-list -i "$BOLT_DIR/perf.data" 2>/dev/null | grep -F "$PGO_BIN" | head -n1 | sed -E 's/.*=//' | awk '{print $1}' || true)
    if [ -z "$ELF_BID" ] || [ "$PERF_BID" != "$ELF_BID" ]; then
        BOLT_CAUSE="Build-ID mismatch (perf=${PERF_BID:-none}, elf=${ELF_BID:-none})"
    fi
fi

if [ -z "$BOLT_CAUSE" ]; then
    PERF2BOLT_FLAGS=()
    if [ "$PERF_SAMPLES" -gt 0 ] && ! perf record -e cycles:u -j any,u -F 99 -o /dev/null -- true >/dev/null 2>&1; then
        if "$PERF2BOLT" --help 2>&1 | grep -- '--basic-events' >/dev/null; then PERF2BOLT_FLAGS+=("--basic-events"); fi
    fi
    if ! "$PERF2BOLT" "${PERF2BOLT_FLAGS[@]}" -p "$BOLT_DIR/perf.data" "$PGO_BIN" -o "$BOLT_DIR/perf.fdata" >> "$BOLT_LOG" 2>&1; then
        BOLT_CAUSE="perf2bolt conversion failed"
    fi
fi

if [ -z "$BOLT_CAUSE" ]; then
    FDATA_SIZE=$(stat -c%s "$BOLT_DIR/perf.fdata" 2>/dev/null || echo 0)
    FDATA_LINES=$(awk '$1 ~ /^[0-9]+$/ {n++} END {print n+0}' "$BOLT_DIR/perf.fdata")
    DSP_HITS=0
    for sym in capture_dsp_pipeline wavenet lstm cabsim; do
        if grep -qi "$sym" "$BOLT_DIR/perf.fdata"; then DSP_HITS=$((DSP_HITS + 1)); fi
    done
    if [ "$FDATA_SIZE" -lt 1024 ] || [ "$FDATA_LINES" -lt 20 ] || [ "$DSP_HITS" -eq 0 ]; then
        BOLT_CAUSE="perf.fdata is not representative (${FDATA_SIZE} bytes, ${FDATA_LINES} entries, ${DSP_HITS} DSP groups)"
    fi
fi

if [ -z "$BOLT_CAUSE" ]; then
    BOLT_VERSION=$("$LLVM_BOLT" --version 2>&1 | head -n1 || echo unknown)
    if "$LLVM_BOLT" "$PGO_BIN" -o target/pgo-build/dist/nam-audio-pipe.bolt \
        -data "$BOLT_DIR/perf.fdata" --reorder-blocks=ext-tsp --reorder-functions=hfsort \
        --split-functions --split-all-cold -hugify --relocs --lite >> "$BOLT_LOG" 2>&1; then
        BOLT_APPLIED=true
        BOLT_PROFILE="$BOLT_DIR/perf.fdata"
        ok "BOLT applied"
    else
        BOLT_CAUSE="llvm-bolt failed"
    fi
fi

if [ -z "$BOLT_CAUSE" ] && [ "$BOLT_APPLIED" != true ]; then BOLT_CAUSE="BOLT was not applied"; fi
if [ -n "$BOLT_CAUSE" ]; then
    echo "BOLT unavailable: $BOLT_CAUSE" | tee -a "$BOLT_LOG"
    if [ "$DO_STRICT" = 1 ]; then
        fail_phase bolt_optimize 1 "$BOLT_LOG"
        finish
        exit 1
    fi
    gap bolt_not_applied
else
    ok "BOLT applied: ${PERF_SAMPLES} perf samples, build-id confirmed"
fi

if [ "$BOLT_APPLIED" = true ]; then
    if "$RECEIPT_BIN" bolt-proof --receipt "$CEREMONY_RECEIPT" >/dev/null 2>&1; then
        ok "BOLT win proof verified: $CEREMONY_RECEIPT"
    else
        gap bolt_proof_unverified
        if [ "$DO_STRICT" = 1 ]; then
            echo "BOLT applied but no valid ab-opt ceremony proof at $CEREMONY_RECEIPT" | tee -a "$BOLT_LOG"
            fail_phase bolt_optimize 1 "$BOLT_LOG"
            finish || true
            exit 1
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Phase 5: static DSP codegen gates on the selected binary.
# ---------------------------------------------------------------------------
phase codegen_report
CODEGEN_LOG="$LOG_DIR/build-release-codegen_report.log"
ASM_FULL=target/dsp_hotpath.full.asm
ASM_TARGET=target/dsp_hotpath.asm
ASM_SUMMARY=target/dsp_hotpath_summary.txt
CODEGEN_STATUS=unavailable
ASM_BIN="$PGO_BIN"
if [ "$BOLT_APPLIED" = true ] && [ -f target/pgo-build/dist/nam-audio-pipe.bolt ]; then
    ASM_BIN=target/pgo-build/dist/nam-audio-pipe.bolt
fi
OBJDUMP_TOOL=""
if command -v llvm-objdump >/dev/null 2>&1; then OBJDUMP_TOOL=llvm-objdump; elif command -v objdump >/dev/null 2>&1; then OBJDUMP_TOOL=objdump; fi
if [ -z "$OBJDUMP_TOOL" ] || [ ! -f "$ASM_BIN" ]; then
    gap codegen_objdump_unavailable
    CODEGEN_STATUS=unavailable
elif {
    LC_ALL=C "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn "$ASM_BIN" > "$ASM_FULL"
    [ -s "$ASM_FULL" ] || exit 1
    "$SCRIPT_DIR/dsp-hotpath-filter.py" --full "$ASM_FULL" --filtered "$ASM_TARGET" \
        --summary "$ASM_SUMMARY" --profile "$SCRIPT_DIR/codegen-profile.json"
} > "$CODEGEN_LOG" 2>&1; then
    CODEGEN_STATUS=pass
    ok "codegen static gates passed"
else
    CODEGEN_STATUS=fail
    if [ "$DO_STRICT" = 1 ]; then
        fail_phase codegen_report 1 "$CODEGEN_LOG"
        finish
        exit 1
    fi
    gap codegen_gate_fail
fi

# ---------------------------------------------------------------------------
# Phase 6: canonical artifact delivery and minimal load check.
# ---------------------------------------------------------------------------
phase deliver_artifact
DELIVER_LOG="$LOG_DIR/build-release-deliver_artifact.log"
BUILD_BIN="$PGO_BIN"
if [ "$BOLT_APPLIED" = true ] && [ -f target/pgo-build/dist/nam-audio-pipe.bolt ]; then
    BUILD_BIN=target/pgo-build/dist/nam-audio-pipe.bolt
fi
STAGE_DIR=$(mktemp -d "$DIST_DIR/.nam-pipe-stage.XXXXXX")
STAGE_BIN="$STAGE_DIR/nam-audio-pipe"
if ! {
    cp "$BUILD_BIN" "$STAGE_BIN"
    strip --strip-all "$STAGE_BIN"
    chmod +x "$STAGE_BIN"
    "$STAGE_BIN" --diagnose > "$LOG_DIR/build-release-diagnose.out"
} > "$DELIVER_LOG" 2>&1; then
    rm -rf "$STAGE_DIR"
    fail_phase deliver_artifact 1 "$DELIVER_LOG"
    finish
    exit 1
fi
mv -T "$STAGE_BIN" "$DIST_BIN"
rmdir "$STAGE_DIR"
DIST_SHA=$(sha256sum "$DIST_BIN" | awk '{print $1}')
ok "canonical artifact: $DIST_BIN (sha256=$DIST_SHA)"

INSTALLED_PATH=""
if [ "$DO_INSTALL" = 1 ]; then
    phase install
    INSTALL_LOG="$LOG_DIR/build-release-install.log"
    mkdir -p "$HOME/.local/bin"
    INSTALL_STAGE=$(mktemp "$HOME/.local/bin/.nam-audio-pipe.XXXXXX")
    if ! {
        cp "$DIST_BIN" "$INSTALL_STAGE"
        chmod 755 "$INSTALL_STAGE"
        mv -T "$INSTALL_STAGE" "$HOME/.local/bin/nam-audio-pipe"
    } > "$INSTALL_LOG" 2>&1; then
        rm -f "$INSTALL_STAGE"
        fail_phase install 1 "$INSTALL_LOG"
        finish
        exit 1
    fi
    INSTALLED_PATH="$HOME/.local/bin/nam-audio-pipe"
    ok "installed: $INSTALLED_PATH"
fi

# ---------------------------------------------------------------------------
# Phase 7: optional distribution packages (tarball + Flatpak).
# ---------------------------------------------------------------------------
TARBALL_PATH=""
FLATPAK_PATH=""
if [ "$DO_PACKAGE" = 1 ]; then
    phase packaging
    PKG_LOG="$LOG_DIR/build-release-packaging.log"
    PKG_DIR=$(mktemp -d -t nam-pipe-pkg.XXXXXX)
    VERIFY_DIR=$(mktemp -d -t nam-pipe-verify.XXXXXX)
    FLATPAK_BUILD_DIR=$(mktemp -d -t nam-pipe-flatpak-build.XXXXXX)
    FLATPAK_REPO_DIR=$(mktemp -d -t nam-pipe-flatpak-repo.XXXXXX)
    FLATPAK_SMOKE_DIR=$(mktemp -d -t nam-pipe-flatpak-smoke.XXXXXX)
    trap 'rm -rf "$PKG_DIR" "$VERIFY_DIR" "$FLATPAK_BUILD_DIR" "$FLATPAK_REPO_DIR" "$FLATPAK_SMOKE_DIR"' EXIT
    SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct 2>/dev/null || date +%s)
    mkdir -p "$PKG_DIR/$ARCHIVE_NAME"
    cp "$DIST_BIN" "$PKG_DIR/$ARCHIVE_NAME/nam-audio-pipe"
    cp README.md "$PKG_DIR/$ARCHIVE_NAME/" 2>/dev/null || true
    cp LICENSE.txt "$PKG_DIR/$ARCHIVE_NAME/" 2>/dev/null || cp LICENSE "$PKG_DIR/$ARCHIVE_NAME/" 2>/dev/null || true
    cat > "$PKG_DIR/$ARCHIVE_NAME/install.sh" <<'EOF'
#!/usr/bin/env bash
set -e
mkdir -p "$HOME/.local/bin"
cp nam-audio-pipe "$HOME/.local/bin/"
chmod +x "$HOME/.local/bin/nam-audio-pipe"
EOF
    chmod +x "$PKG_DIR/$ARCHIVE_NAME/install.sh"
    if ! {
        tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
            -C "$PKG_DIR" -I "zstd -6 -T0" -cf "$TARBALL" "$ARCHIVE_NAME"
        tar -C "$VERIFY_DIR" -I "zstd -d" -xf "$TARBALL"
        [ "$(sha256sum "$VERIFY_DIR/$ARCHIVE_NAME/nam-audio-pipe" | awk '{print $1}')" = "$DIST_SHA" ]
        "$VERIFY_DIR/$ARCHIVE_NAME/nam-audio-pipe" --diagnose > /dev/null
    } > "$PKG_LOG" 2>&1; then
        fail_phase packaging 1 "$PKG_LOG"
        finish
        exit 1
    fi
    TARBALL_PATH="$TARBALL"
    ok "tarball: $TARBALL"

    DESKTOP_SRC=packaging/flatpak/io.github.fabiohl.NAMAudioPipe.desktop
    METAINFO_SRC=packaging/flatpak/io.github.fabiohl.NAMAudioPipe.metainfo.xml
    ICONS_SRC=packaging/flatpak/icons/hicolor
    APP_ID=io.github.fabiohl.NAMAudioPipe
    mkdir -p "$FLATPAK_BUILD_DIR/files/bin" "$FLATPAK_BUILD_DIR/files/share/applications" \
        "$FLATPAK_BUILD_DIR/files/share/metainfo" "$FLATPAK_BUILD_DIR/files/share/licenses/$APP_ID"
    cp "$DIST_BIN" "$FLATPAK_BUILD_DIR/files/bin/nam-audio-pipe"
    chmod +x "$FLATPAK_BUILD_DIR/files/bin/nam-audio-pipe"
    cp "$DESKTOP_SRC" "$FLATPAK_BUILD_DIR/files/share/applications/"
    cp "$METAINFO_SRC" "$FLATPAK_BUILD_DIR/files/share/metainfo/"
    for size_dir in "$ICONS_SRC"/*; do
        [ -d "$size_dir/apps" ] || continue
        target_dir="$FLATPAK_BUILD_DIR/files/share/icons/hicolor/$(basename "$size_dir")/apps"
        mkdir -p "$target_dir"
        cp "$size_dir/apps"/* "$target_dir/"
    done
    cp LICENSE.txt "$FLATPAK_BUILD_DIR/files/share/licenses/$APP_ID/" 2>/dev/null || cp LICENSE "$FLATPAK_BUILD_DIR/files/share/licenses/$APP_ID/" 2>/dev/null || true
    APP_INFO_XMLS="$FLATPAK_BUILD_DIR/files/share/app-info/xmls"
    mkdir -p "$APP_INFO_XMLS"
    {
        printf '%s\n' '<?xml version="1.0" encoding="utf-8"?>' '<components version="1.0">'
        sed '1d' "$METAINFO_SRC"
        printf '%s\n' '</components>'
    } | gzip -9n > "$APP_INFO_XMLS/$APP_ID.xml.gz"
    APP_INFO_ICONS="$FLATPAK_BUILD_DIR/files/share/app-info/icons"
    if [ -d "$ICONS_SRC" ]; then
        for size_dir in "$ICONS_SRC"/*; do
            size_name=$(basename "$size_dir")
            if [ "$size_name" != scalable ] && [ -d "$size_dir/apps" ]; then
                mkdir -p "$APP_INFO_ICONS/flatpak/$size_name"
                cp "$size_dir/apps"/* "$APP_INFO_ICONS/flatpak/$size_name/" 2>/dev/null || true
            fi
        done
    fi
    if ! appstreamcli validate --no-net --strict "$METAINFO_SRC" >> "$PKG_LOG" 2>&1; then
        fail_phase packaging 1 "$PKG_LOG"
        die "AppStream metainfo validation failed"
    fi
    desktop_id=$(basename "$DESKTOP_SRC")
    desktop_name=$(grep -m1 '^Name=' "$DESKTOP_SRC" | cut -d= -f2- || true)
    meta_id=$(grep -m1 -oP '(?<=<id>)[^<]+' "$METAINFO_SRC" || true)
    meta_name=$(grep -m1 -oP '(?<=<name>)[^<]+' "$METAINFO_SRC" || true)
    meta_launch=$(grep -m1 -oP '(?<=<launchable type="desktop-id">)[^<]+' "$METAINFO_SRC" || true)
    release_line=$(grep -m1 -oP '<release version="[^"]+"[^>]*>' "$METAINFO_SRC" || true)
    meta_ver=$(printf '%s' "$release_line" | sed -nE 's/.*version="([^"]+)".*/\1/p')
    meta_date=$(printf '%s' "$release_line" | sed -nE 's/.*date="([^"]+)".*/\1/p')
    if [ "$meta_id" != "$desktop_id" ] || [ "$meta_launch" != "$desktop_id" ] || [ "$meta_name" != "$desktop_name" ]; then
        fail_phase packaging 1 "$PKG_LOG"
        die "AppStream id/name/launchable does not match $desktop_id"
    fi
    if [ "$meta_ver" != "$VERSION" ] || [ -z "$meta_date" ]; then
        fail_phase packaging 1 "$PKG_LOG"
        die "AppStream release version/date is out of sync with Cargo.toml ($VERSION)"
    fi
    SDK_NAME=org.freedesktop.Sdk
    if ! flatpak info org.freedesktop.Sdk//25.08 >/dev/null 2>&1; then SDK_NAME=org.freedesktop.Platform; fi
    flatpak build-init "$FLATPAK_BUILD_DIR" "$APP_ID" "$SDK_NAME" org.freedesktop.Platform 25.08
    flatpak build-finish "$FLATPAK_BUILD_DIR" --command=nam-audio-pipe --socket=pulseaudio \
        --filesystem=xdg-run/pipewire-0 --share=ipc --device=all --filesystem=home:ro \
        --filesystem=xdg-music --socket=fallback-x11 --socket=wayland
    if ! LC_ALL=C flatpak build-export --update-appstream "$FLATPAK_REPO_DIR" "$FLATPAK_BUILD_DIR" stable >> "$PKG_LOG" 2>&1; then
        fail_phase packaging 1 "$PKG_LOG"
        die "flatpak build-export failed"
    fi
    if ! flatpak repo "$FLATPAK_REPO_DIR" --branches 2>/dev/null | grep -E '^appstream[0-9]*/' >/dev/null; then
        fail_phase packaging 1 "$PKG_LOG"
        die "Flatpak export did not synthesize the AppStream catalog"
    fi
    flatpak build-bundle "$FLATPAK_REPO_DIR" "$FLATPAK_BUNDLE" "$APP_ID" stable
    FLATPAK_PATH="$FLATPAK_BUNDLE"
    APP_REF="app/$APP_ID/x86_64/stable"
    mkdir -p "$FLATPAK_SMOKE_DIR/objects" "$FLATPAK_SMOKE_DIR/refs/heads" "$FLATPAK_SMOKE_DIR/refs/remotes" \
        "$FLATPAK_SMOKE_DIR/refs/mirrors" "$FLATPAK_SMOKE_DIR/tmp" "$FLATPAK_SMOKE_DIR/state"
    printf '[core]\nrepo_version=1\nmode=archive-z2\n' > "$FLATPAK_SMOKE_DIR/config"
    if ! flatpak build-import-bundle --update-appstream "$FLATPAK_SMOKE_DIR" "$FLATPAK_BUNDLE" >/dev/null 2>&1 \
        || ! flatpak repo "$FLATPAK_SMOKE_DIR" --branches 2>/dev/null | grep "$APP_REF" >/dev/null \
        || ! flatpak repo "$FLATPAK_SMOKE_DIR" --metadata="$APP_REF" 2>/dev/null | grep '^command=nam-audio-pipe$' >/dev/null; then
        fail_phase packaging 1 "$PKG_LOG"
        die "Flatpak bundle integrity verification failed"
    fi
    FLATPAK_BIN_SHA=$(sha256sum "$FLATPAK_BUILD_DIR/files/bin/nam-audio-pipe" | awk '{print $1}')
    if [ "$FLATPAK_BIN_SHA" != "$DIST_SHA" ]; then
        fail_phase packaging 1 "$PKG_LOG"
        die "Flatpak binary SHA-256 differs from the canonical artifact"
    fi
    if ! flatpak build "$FLATPAK_BUILD_DIR" nam-audio-pipe --diagnose >/dev/null 2>&1; then
        fail_phase packaging 1 "$PKG_LOG"
        die "in-sandbox Flatpak smoke test failed"
    fi
    if [ "$DO_INSTALL" = 1 ]; then
        flatpak install --user --reinstall -y "$FLATPAK_BUNDLE" >> "$PKG_LOG" 2>&1
    fi
    ok "flatpak bundle: $FLATPAK_BUNDLE"
fi

# ---------------------------------------------------------------------------
# Phase 8: typed build receipt (failure to write it is a failure).
# ---------------------------------------------------------------------------
phase build_receipt
RECEIPT_LOG="$LOG_DIR/build-release-build_receipt.log"
PGO_STATUS=skipped
BOLT_STATUS=skipped
if [ "$PGO_APPLIED" = true ]; then PGO_STATUS=applied; fi
if [ "$BOLT_APPLIED" = true ]; then BOLT_STATUS=applied; fi
RCPT_ARGS=(--out "$BUILD_RECEIPT" --artifact "$DIST_BIN" --package-name nam-audio-pipe
    --package-version "$VERSION" --git-commit "$GIT_COMMIT" --git-branch "$GIT_BRANCH"
    --git-dirty "$GIT_DIRTY" --cargo-lock Cargo.lock --rustc-version "$(rustc --version)"
    --rustflags "$RELEASE_RUSTFLAGS" --pgo-status "$PGO_STATUS" --bolt-status "$BOLT_STATUS"
    --codegen-status "$CODEGEN_STATUS")
if [ "$PGO_APPLIED" = true ] && [ -f "$PGO_RECEIPT" ]; then RCPT_ARGS+=(--pgo-workload-receipt "$PGO_RECEIPT"); fi
if [ -n "$BOLT_VERSION" ]; then RCPT_ARGS+=(--bolt-version "$BOLT_VERSION"); fi
if [ -n "$BOLT_PROFILE" ] && [ -f "$BOLT_PROFILE" ]; then RCPT_ARGS+=(--bolt-profile "$BOLT_PROFILE"); fi
if [ -f "$CEREMONY_RECEIPT" ]; then RCPT_ARGS+=(--ceremony-receipt "$CEREMONY_RECEIPT"); fi
if [ -n "$TARBALL_PATH" ]; then RCPT_ARGS+=(--tarball-path "$TARBALL_PATH"); fi
if [ -n "$FLATPAK_PATH" ]; then RCPT_ARGS+=(--flatpak-path "$FLATPAK_PATH"); fi
if [ -n "$INSTALLED_PATH" ]; then RCPT_ARGS+=(--installed-path "$INSTALLED_PATH"); fi
if [ ${#GAPS[@]} -gt 0 ]; then RCPT_ARGS+=(--gaps "$(IFS=,; echo "${GAPS[*]}")"); fi
if [ "$DO_STRICT" = 1 ]; then RCPT_ARGS+=(--strict); fi
if ! "$RECEIPT_BIN" build-receipt "${RCPT_ARGS[@]}" 2>&1 | tee "$RECEIPT_LOG"; then
    fail_phase build_receipt "${PIPESTATUS[0]}" "$RECEIPT_LOG"
    finish
    exit 1
fi
ok "build receipt: $BUILD_RECEIPT"

finish
