#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
# build-release.sh — builds the canonical Pipe artifact (PGO+BOLT), packages it and emits the typed build receipt. Classe C.
# Uso: utils/build-release.sh [--strict] [--package] [--install] [--certify] [-h|--help]
# Orçamento: 900
set -x; PS4='+ ${BASH_SOURCE##*/}:${LINENO}: '
set -euo pipefail
cd "$(dirname "$0")/.."
. utils/_lib.sh

SCRIPT_NAME=build-release
SCRIPT_DIR="$PWD/utils"
LOG_DIR=target/logs
SCOPE=build
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

# gap deduplicado: uma lacuna repetida é um registro único no carrier.
gap() {
    local p="${PHASE_NAME:-$1}" g="$1"
    if [ $# -ge 2 ]; then p="$1"; g="$2"; fi
    local known
    for known in ${GAPS[@]+"${GAPS[@]}"}; do
        if [ "$known" = "$g" ]; then return 0; fi
    done
    GAPS+=("$g")
    if [ "$p" != "env_verify" ]; then
        emit "PHASE: $p GAP $g"
    fi
}
fail_phase() {
    FAILS+=("$1")
    emit "PHASE: $1 FAIL rc=$2 log=$3"
}
PARANOID_ORIG=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
restore_paranoid() {
    local cur
    cur=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
    if [ -n "${PARANOID_ORIG:-}" ] && [ "$cur" != "$PARANOID_ORIG" ]; then
        if ! sudo -n sysctl -q -w kernel.perf_event_paranoid="$PARANOID_ORIG" 2>/dev/null; then
            if [ "${FINISHED:-0}" = 1 ]; then
                echo "WARN: perf_event_paranoid not restored after RESULT" >&2
            else
                gap bolt_optimize perf_paranoid_not_restored
            fi
        fi
    fi
}
trap restore_paranoid EXIT

# Every fatal path publishes exactly one RESULT: finish runs before exit, unless already done.
die() {
    echo "ERROR: $*" >&2
    restore_paranoid
    if [ "${FINISHED:-0}" != 1 ]; then finish || true; fi
    exit 1
}

show_help() {
    cat <<'EOF'
NAM-Audio-Pipe Release Build (Class C)

Usage:
  utils/build-release.sh [flags]

Canonical flags:
  --strict     Fail-closed: PGO is mandatory; BOLT requires ceremony proof; any unproven gap aborts.
  --package    Also produce the .tar.zst and Flatpak bundle in target/dist/.
  --certify    Do not build: reconcile the artifact, build receipt and long audit.
  --install    Only with --certify: install the certified binary (and the bundle, if declared).
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
        *) usage_fail "build-release.sh: flag desconhecida '$1'" ;;
    esac
done
if [ "$DO_INSTALL" = 1 ] && [ "$DO_CERTIFY" != 1 ]; then
    usage_fail "build-release.sh: --install exige --certify"
fi

lib_init
trap 'trap_fail $?' ERR
mkdir -p "$DIST_DIR"
if [ "$DO_CERTIFY" != 1 ]; then
    # Never let a failed build inherit the previous build's run id.
    rm -f "$LOG_DIR/build-run-id"
fi
VERSION=$(awk -F'"' '/^version = / {print $2; exit}' Cargo.toml)
ARCHIVE_NAME="nam-audio-pipe-v${VERSION}-linux-x86_64-v3"
TARBALL="$DIST_DIR/$ARCHIVE_NAME.tar.zst"
FLATPAK_BUNDLE="$DIST_DIR/$ARCHIVE_NAME.flatpak"
GIT_COMMIT=$(git rev-parse HEAD 2>/dev/null || echo unknown)
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo unknown)
GIT_DIRTY=false
if [ -n "$(git status --porcelain 2>/dev/null || true)" ]; then GIT_DIRTY=true; fi

# install_certified <receipt-bin> — instala somente após verify-release (CERTIFIED, SHA e pacotes).
install_certified() {
    local bin=$1 log="$LOG_DIR/build-release-install_artifact.log" sha stage frc=0 fpath dest="$HOME/.local/bin/nam-audio-pipe"
    phase install_artifact
    if ! sha=$("$bin" verify-release --release-receipt "$RELEASE_RECEIPT" --artifact "$DIST_BIN" 2>"$log"); then
        fail_phase install_artifact 1 "$log"; finish || true; exit 1
    fi
    mkdir -p "$HOME/.local/bin"
    stage=$(mktemp "$HOME/.local/bin/.nam-audio-pipe.XXXXXX")
    if ! { cp "$DIST_BIN" "$stage" && chmod 755 "$stage" && mv -T "$stage" "$dest"; } >> "$log" 2>&1; then
        rm -f "$stage"; fail_phase install_artifact 1 "$log"; finish || true; exit 1
    fi
    if [ "$(sha256sum "$dest" | awk '{print $1}')" != "$sha" ]; then
        fail_phase install_artifact 1 "$log"; finish || true; exit 1
    fi
    echo "installed: $dest sha256=$sha" >> "$log"
    fpath=$("$bin" flatpak-path --release-receipt "$RELEASE_RECEIPT" 2>>"$log") || frc=$?
    if [ "$frc" -eq 0 ]; then
        if ! flatpak install --user --reinstall -y "$fpath" >> "$log" 2>&1; then
            fail_phase install_artifact 1 "$log"; finish || true; exit 1
        fi
        echo "flatpak installed: $fpath" >> "$log"
    elif [ "$frc" -ne 3 ]; then
        fail_phase install_artifact "$frc" "$log"; finish || true; exit 1
    fi
    ok "installed: $dest (sha256=$sha)"
    emit "PHASE: install_artifact PASS log=$log"
}

find_receipt_bin() {
    local candidate
    for candidate in target/long/release/pipe_long_receipt target/release/pipe_long_receipt target/debug/pipe_long_receipt; do
        if [ -x "$candidate" ]; then
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
    # A failed certification must not leave a previous CERTIFIED receipt behind.
    rm -f "$RELEASE_RECEIPT"
    phase certify
    CERT_LOG="$LOG_DIR/build-release-certify.log"
    if [ -f "$LOG_DIR/tests-long-status.tsv" ] && grep -q 'scope=partial' "$LOG_DIR/tests-long-status.tsv"; then
        fail_phase certify 1 "$CERT_LOG (tests-long ran with partial scope)"
        finish || true
        exit 1
    fi
    CERT_ARGS=(--artifact "$DIST_BIN" --build-receipt "$BUILD_RECEIPT" --long-receipt "$LONG_JSONL" --text-receipt "$LONG_TEXT" --out "$RELEASE_RECEIPT")
    if [ "$DO_STRICT" = 1 ]; then CERT_ARGS+=(--strict); fi
    if CERT_BIN=$(find_receipt_bin); then
        set +e
        "$CERT_BIN" certify "${CERT_ARGS[@]}" 2>&1 | tee "$CERT_LOG"
        pipestatus=("${PIPESTATUS[@]}")
        set -e
        rc=${pipestatus[0]}
        trc=${pipestatus[1]}
        if [ "$rc" -ne 0 ]; then
            fail_phase certify "$rc" "$CERT_LOG"
            rm -f "$RELEASE_RECEIPT"
        elif [ "$trc" -ne 0 ]; then
            fail_phase certify "$trc" "$CERT_LOG (tee write failed)"
            rm -f "$RELEASE_RECEIPT"
        else
            emit "PHASE: certify PASS log=$CERT_LOG"
            ok "release certified: $RELEASE_RECEIPT"
            if [ "$DO_INSTALL" = 1 ]; then install_certified "$CERT_BIN"; fi
        fi
    else
        fail_phase certify 2 "$CERT_LOG (pipe_long_receipt unavailable; --certify never recompiles)"
        finish
        exit 1
    fi
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
ENV_GAPS=${#GAPS[@]}
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
    gap env_verify pgo_profdata_unavailable
    if [ "$DO_STRICT" = 1 ]; then
        finish || true
        die "llvm-profdata is mandatory under --strict (rustup component add llvm-tools-preview)"
    fi
fi

LLVM_BOLT=""
for candidate in llvm-bolt llvm-bolt-22 llvm-bolt-21 llvm-bolt-20 llvm-bolt-19 llvm-bolt-18; do
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
    if [ ! -x "$PERF2BOLT" ]; then
        for candidate in perf2bolt perf2bolt-22 perf2bolt-21 perf2bolt-20 perf2bolt-19 perf2bolt-18; do
            if command -v "$candidate" >/dev/null 2>&1; then PERF2BOLT=$(command -v "$candidate"); break; fi
        done
    fi
    if [ ! -x "$PERF2BOLT" ]; then
        for candidate in /usr/lib/llvm-*/bin/perf2bolt; do
            if [ -x "$candidate" ]; then PERF2BOLT="$candidate"; break; fi
        done
    fi
fi
MERGE_FDATA=""
if [ -n "$LLVM_BOLT" ]; then
    MERGE_FDATA="$(dirname "$LLVM_BOLT")/merge-fdata"
    if [ ! -x "$MERGE_FDATA" ]; then
        for candidate in merge-fdata merge-fdata-22 merge-fdata-21 merge-fdata-20 merge-fdata-19 merge-fdata-18; do
            if command -v "$candidate" >/dev/null 2>&1; then MERGE_FDATA=$(command -v "$candidate"); break; fi
        done
    fi
    if [ ! -x "$MERGE_FDATA" ]; then
        for candidate in /usr/lib/llvm-*/bin/merge-fdata; do
            if [ -x "$candidate" ]; then MERGE_FDATA="$candidate"; break; fi
        done
    fi
fi
if command -v perf >/dev/null 2>&1; then
    PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
    if [ "$PARANOID" -gt 1 ] && sudo -n sysctl -q -w kernel.perf_event_paranoid=1 2>/dev/null; then
        PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)
    fi
    if [ "$PARANOID" -le 1 ]; then HAS_PERF=true; fi
fi
BOLT_AVAILABLE=false
if [ -n "$LLVM_BOLT" ] && { [ -n "$MERGE_FDATA" ] || { [ -n "$PERF2BOLT" ] && [ "$HAS_PERF" = true ]; }; }; then BOLT_AVAILABLE=true; fi
if [ "$BOLT_AVAILABLE" != true ]; then
    gap env_verify bolt_tools_unavailable
fi

if ! cargo build --locked --features testing --bin pipe_long_receipt >/dev/null 2>&1; then
    fail_phase env_verify 1 "$ENV_LOG"
    finish
    die "cannot build the typed receipt tool (pipe_long_receipt)"
fi
RECEIPT_BIN=target/debug/pipe_long_receipt
# --strict is the operator release path: the A/B ceremony receipt is an input,
# not something this script can synthesize. Fail during env_verify instead of
# after the PGO/BOLT work when the proof is absent or invalid.
if [ "$DO_STRICT" = 1 ]; then
    CEREMONY_PREFLIGHT_LOG="$LOG_DIR/build-release-ceremony_preflight.log"
    if [ -f "$CEREMONY_RECEIPT" ]; then
        if ! CEREMONY_PREFLIGHT_VERDICT=$("$RECEIPT_BIN" ceremony-proof --receipt "$CEREMONY_RECEIPT" --git-sha "$GIT_COMMIT" 2>"$CEREMONY_PREFLIGHT_LOG"); then
            echo "ERROR: --strict rejected the A/B ceremony receipt (see $CEREMONY_PREFLIGHT_LOG)" | tee -a "$CEREMONY_PREFLIGHT_LOG" >&2
            fail_phase env_verify 1 "$CEREMONY_PREFLIGHT_LOG"
            finish || true
            exit 1
        fi
        case "$CEREMONY_PREFLIGHT_VERDICT" in
            PGO+BOLT|PGO-ONLY|UNOPTIMIZED) ;;
            *)
                echo "ERROR: --strict requires a PGO+BOLT, PGO-ONLY or UNOPTIMIZED ceremony verdict, got '$CEREMONY_PREFLIGHT_VERDICT'" | tee -a "$CEREMONY_PREFLIGHT_LOG" >&2
                fail_phase env_verify 1 "$CEREMONY_PREFLIGHT_LOG"
                finish || true
                exit 1
                ;;
        esac
    fi
fi
ok "environment ready (rustflags: $CONFIG_RUSTFLAGS)"
if [ ${#GAPS[@]} -eq "$ENV_GAPS" ]; then
    emit "PHASE: env_verify PASS log=$ENV_LOG"
else
    emit "PHASE: env_verify PASS_WITH_GAPS log=$ENV_LOG"
fi

# ---------------------------------------------------------------------------
# Phase 2: PGO workload and profile merge.
# ---------------------------------------------------------------------------
PGO_APPLIED=false
rm -f "$PGO_RECEIPT"
PGO_DIR="$PWD/target/pgo-profile"
PROFRAW_DIR="$PGO_DIR/profraw"
MERGED_PROFILE="$PGO_DIR/merged.profdata"
PGO_LOG="$LOG_DIR/build-release-pgo_profile.log"
if [ "${CEREMONY_PREFLIGHT_VERDICT:-}" = "UNOPTIMIZED" ]; then
    phase pgo_profile
    echo "A/B ceremony proved PGO unoptimized (verdict: UNOPTIMIZED); skipping PGO to preserve optimal Plain performance" | tee "$PGO_LOG"
    emit "PHASE: pgo_profile PASS log=$PGO_LOG"
elif [ -n "$LLVM_PROFDATA" ]; then
    phase pgo_profile
    rm -rf "$PGO_DIR"
    mkdir -p "$PROFRAW_DIR"
    PGO_GEN_RUSTFLAGS="$BASE_RUSTFLAGS -Cprofile-generate=$PROFRAW_DIR"
    if {
        RUSTFLAGS="$PGO_GEN_RUSTFLAGS" LLVM_PROFILE_FILE="$PROFRAW_DIR/default_%m_%p.profraw" \
            cargo run --locked --profile dist --features testing --target-dir target/pgo-build \
            --bin pgo_workload -- --receipt "$PGO_RECEIPT" &&
        "$RECEIPT_BIN" pgo-check --receipt "$PGO_RECEIPT" &&
        PROFRAW_COUNT=$(find "$PROFRAW_DIR" -name '*.profraw' | wc -l) &&
        [ "$PROFRAW_COUNT" -gt 0 ] &&
        "$LLVM_PROFDATA" merge -sparse -o "$MERGED_PROFILE" "$PROFRAW_DIR"/*.profraw &&
        [ -s "$MERGED_PROFILE" ]
    } 2>&1 | tee "$PGO_LOG"; then
        PGO_APPLIED=true
        ok "PGO profile merged: $MERGED_PROFILE"
        emit "PHASE: pgo_profile PASS log=$PGO_LOG"
    else
        rm -rf "$PGO_DIR"
        fail_phase pgo_profile "${PIPESTATUS[0]:-1}" "$PGO_LOG"
        finish
        exit 1
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
emit "PHASE: compile_binary PASS log=$COMPILE_LOG"

# ---------------------------------------------------------------------------
# Phase 4: BOLT post-link optimization (live PipeWire profiling).
# ---------------------------------------------------------------------------
phase bolt_optimize
BOLT_LOG="$LOG_DIR/build-release-bolt_optimize.log"
BOLT_DIR=target/bolt-profile
BOLT_APPLIED=false
BOLT_CAUSE=""
BOLT_MODE=""
LBR_USED=false
BOLT_VERSION=""
BOLT_PROFILE=""
BOLT_STATUS=not_applied
BOLT_PROOF=none

CEREMONY_VERDICT=""
CEREMONY_REJECTION="no ceremony receipt"
if [ -f "$CEREMONY_RECEIPT" ]; then
    CEREMONY_VERDICT=$("$RECEIPT_BIN" ceremony-proof --receipt "$CEREMONY_RECEIPT" --git-sha "$GIT_COMMIT" 2>"$LOG_DIR/build-release-ceremony_proof.err" || true)
    CEREMONY_REJECTION=$(cat "$LOG_DIR/build-release-ceremony_proof.err")
    cat "$LOG_DIR/build-release-ceremony_proof.err" >> "$BOLT_LOG"
fi

if [ "$CEREMONY_VERDICT" != "PGO+BOLT" ]; then
    if [ "$CEREMONY_VERDICT" = "UNOPTIMIZED" ]; then
        echo "A/B ceremony proved UNOPTIMIZED (neither PGO nor BOLT met margin); fallback_plain per promotion policy" | tee -a "$BOLT_LOG"
        BOLT_STATUS=not_proven
        BOLT_PROOF=fallback_plain
        emit "PHASE: bolt_optimize PASS log=$BOLT_LOG"
    elif [ "$CEREMONY_VERDICT" = "PGO-ONLY" ]; then
        echo "A/B ceremony proved PGO-ONLY (BOLT applied, margin not met); fallback_pgo_only per promotion policy" | tee -a "$BOLT_LOG"
        BOLT_STATUS=not_proven
        BOLT_PROOF=fallback_pgo_only
        emit "PHASE: bolt_optimize PASS log=$BOLT_LOG"
    else
        echo "No valid PGO+BOLT ceremony proof at $CEREMONY_RECEIPT (${CEREMONY_REJECTION:-rejected})" | tee -a "$BOLT_LOG"
        if [ "$DO_STRICT" = 1 ]; then
            fail_phase bolt_optimize 1 "$BOLT_LOG"
            restore_paranoid
            finish || true
            exit 1
        fi
        gap bolt_not_proven
        BOLT_STATUS=not_proven
        BOLT_PROOF=none
    fi
else
    rm -rf "$BOLT_DIR"
    mkdir -p "$BOLT_DIR"

if [ -z "$LLVM_BOLT" ] || { [ -z "$MERGE_FDATA" ] && { [ "$HAS_PERF" != true ] || [ -z "$PERF2BOLT" ]; }; }; then
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
ticks() {
    local p="$1" s
    s=$(awk '{s+=$1} END {print (s+0)}' /proc/"$p"/task/*/schedstat 2>/dev/null)
    if [ -n "$s" ] && [ "$s" -gt 0 ] 2>/dev/null; then
        echo "$s"
    else
        awk '{print $14 + $15}' "/proc/$p/stat" 2>/dev/null || echo 0
    fi
}
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
    sleep .3
    t1=$(ticks "$pid"); sleep .8; t2=$(ticks "$pid")
    if [ "$t2" -le "$t1" ]; then
        sleep .5; t2=$(ticks "$pid")
    fi
    kill "$play" "$pid" 2>/dev/null || true
    wait "$play" 2>/dev/null || true; wait "$pid" 2>/dev/null || true
    if [ "$t2" -le "$t1" ]; then
        echo "no sample consumption for $model (t1=$t1, t2=$t2)" >&2
        exit 1
    fi
done
EOF
    chmod +x "$PROFILE_DRIVER"
    PERF_ARGS=(-F 4000 -e cycles:u)
    if [ "$HAS_PERF" = true ] && [ -n "$PERF2BOLT" ] && perf record -e cycles:u -j any,u -F 99 -o /dev/null -- true >/dev/null 2>&1; then
        LBR_USED=true
        PERF_ARGS=(-F 99 -e cycles:u -j any,u)
    fi
    LBR_SAMPLES=0
    if [ "$LBR_USED" = true ]; then
        printf 'BOLT perf profiling %s model(s)\n' "${#MODEL_FILES[@]}" >> "$BOLT_LOG"
        if ! perf record "${PERF_ARGS[@]}" -o "$BOLT_DIR/perf.data" -- \
            "$PROFILE_DRIVER" "$PGO_BIN" "$TEST_WAV" "${MODEL_FILES[@]}" >> "$BOLT_LOG" 2>&1; then
            BOLT_CAUSE="perf record failed"
        else
            # The -j probe can succeed on hosts that still record zero branch
            # stacks (cloud VMs). Trust only real LBR entries in the profile.
            LBR_SAMPLES=$(perf script -i "$BOLT_DIR/perf.data" -F brstack 2>/dev/null | grep -cE '0x[0-9a-fA-F]+/0x[0-9a-fA-F]+' || true)
            if [ "$LBR_SAMPLES" -eq 0 ]; then LBR_USED=false; fi
        fi
    fi
    if [ -z "$BOLT_CAUSE" ] && [ "$LBR_USED" != true ] && [ -n "$MERGE_FDATA" ]; then
        # No usable LBR: BOLT instrumentation collects function/edge counters
        # without PMU facilities and is preferred over timer-only samples.
        BOLT_MODE=instrumented
        INSTRUMENTED_BIN="$BOLT_DIR/nam-audio-pipe.instrumented"
        rm -f "$BOLT_DIR"/bolt.fdata.*
        printf 'BOLT instrumentation profiling %s model(s)\n' "${#MODEL_FILES[@]}" >> "$BOLT_LOG"
        if ! "$LLVM_BOLT" "$PGO_BIN" -instrument -o "$INSTRUMENTED_BIN" \
            --instrumentation-file="$BOLT_DIR/bolt.fdata" --instrumentation-file-append-pid >> "$BOLT_LOG" 2>&1; then
            BOLT_CAUSE="llvm-bolt instrumentation failed"
        elif ! "$PROFILE_DRIVER" "$INSTRUMENTED_BIN" "$TEST_WAV" "${MODEL_FILES[@]}" >> "$BOLT_LOG" 2>&1; then
            BOLT_CAUSE="instrumented workload failed"
        else
            FDATA_FILES=()
            while IFS= read -r -d '' f; do if [ -s "$f" ]; then FDATA_FILES+=("$f"); fi; done < <(find "$BOLT_DIR" -maxdepth 1 -name 'bolt.fdata.*' -print0 2>/dev/null || true)
            if [ ${#FDATA_FILES[@]} -eq 0 ] || ! "$MERGE_FDATA" "${FDATA_FILES[@]}" > "$BOLT_DIR/perf.fdata" 2>>"$BOLT_LOG"; then
                BOLT_CAUSE="merge-fdata produced no representative profile"
            fi
        fi
    elif [ -z "$BOLT_CAUSE" ]; then
        # LBR profile already recorded; otherwise perf2bolt receives a
        # timer-sampled profile in no-LBR mode (-nl).
        BOLT_MODE=perf2bolt
        if [ ! -f "$BOLT_DIR/perf.data" ]; then
            printf 'BOLT no-LBR profiling %s model(s)\n' "${#MODEL_FILES[@]}" >> "$BOLT_LOG"
            if ! perf record "${PERF_ARGS[@]}" -o "$BOLT_DIR/perf.data" -- \
                "$PROFILE_DRIVER" "$PGO_BIN" "$TEST_WAV" "${MODEL_FILES[@]}" >> "$BOLT_LOG" 2>&1; then
                BOLT_CAUSE="perf record failed"
            fi
        fi
    fi
fi

PERF_SAMPLES=0
if [ -z "$BOLT_CAUSE" ] && [ -f "$BOLT_DIR/perf.data" ]; then
    PERF_SIZE=$(stat -c%s "$BOLT_DIR/perf.data" 2>/dev/null || echo 0)
    PERF_SAMPLES=$(perf script -i "$BOLT_DIR/perf.data" 2>/dev/null | wc -l || echo 0)
    if [ "$PERF_SIZE" -lt 8192 ] || [ "$PERF_SAMPLES" -lt 500 ]; then
        BOLT_CAUSE="perf.data has ${PERF_SAMPLES} samples (${PERF_SIZE} bytes); workload did not run"
    fi
fi

if [ -z "$BOLT_CAUSE" ] && [ -f "$BOLT_DIR/perf.data" ]; then
    ELF_BID=$(LC_ALL=C readelf -n "$PGO_BIN" 2>/dev/null | grep -oP 'Build ID:\s+\K[0-9a-fA-F]+' | head -n1 | tr 'A-F' 'a-f' || true)
    PERF_BID=$(LC_ALL=C perf buildid-list -i "$BOLT_DIR/perf.data" 2>/dev/null | grep -F "$PGO_BIN" | head -n1 | sed -E 's/.*=//' | awk '{print $1}' || true)
    if [ -z "$ELF_BID" ] || [ "$PERF_BID" != "$ELF_BID" ]; then
        BOLT_CAUSE="Build-ID mismatch (perf=${PERF_BID:-none}, elf=${ELF_BID:-none})"
    fi
fi

if [ -z "$BOLT_CAUSE" ] && [ "$BOLT_MODE" = perf2bolt ]; then
    PERF2BOLT_FLAGS=()
    if [ "$LBR_USED" != true ]; then PERF2BOLT_FLAGS+=("-nl"); fi
    if ! "$PERF2BOLT" "${PERF2BOLT_FLAGS[@]}" -p "$BOLT_DIR/perf.data" "$PGO_BIN" -o "$BOLT_DIR/perf.fdata" >> "$BOLT_LOG" 2>&1; then
        BOLT_CAUSE="perf2bolt conversion failed"
    fi
fi

if [ -z "$BOLT_CAUSE" ]; then
    FDATA_SIZE=$(stat -c%s "$BOLT_DIR/perf.fdata" 2>/dev/null || echo 0)
    FDATA_LINES=$(awk '$1 ~ /^[0-9]+$/ {n++} END {print n+0}' "$BOLT_DIR/perf.fdata")
    DSP_HITS=0
    for sym in setup_capture_stream capture_dsp_pipeline wavenet lstm cabsim; do
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
            restore_paranoid
            finish
            exit 1
        fi
        gap bolt_not_applied
    else
        ok "BOLT applied (${BOLT_MODE})"
        BOLT_STATUS=applied
        emit "PHASE: bolt_optimize PASS log=$BOLT_LOG"
    fi
fi
restore_paranoid

# ---------------------------------------------------------------------------
# Phase 5: static DSP codegen gates on the selected binary.
# ---------------------------------------------------------------------------
phase codegen_report
CODEGEN_LOG="$LOG_DIR/build-release-codegen_report.log"
CODEGEN_DIR="$LOG_DIR/codegen"; mkdir -p "$CODEGEN_DIR"
ASM_FULL="$CODEGEN_DIR/dsp_hotpath.full.asm"
ASM_TARGET="$CODEGEN_DIR/dsp_hotpath.asm"
ASM_SUMMARY="$CODEGEN_DIR/dsp_hotpath_summary.txt"
CODEGEN_STATUS=unavailable
ASM_BIN="$PGO_BIN"
if [ "$BOLT_APPLIED" = true ] && [ -f target/pgo-build/dist/nam-audio-pipe.bolt ]; then
    ASM_BIN=target/pgo-build/dist/nam-audio-pipe.bolt
fi
OBJDUMP_TOOL=""
if command -v llvm-objdump >/dev/null 2>&1; then OBJDUMP_TOOL=llvm-objdump; elif command -v objdump >/dev/null 2>&1; then OBJDUMP_TOOL=objdump; fi
if [ -z "$OBJDUMP_TOOL" ]; then
    CODEGEN_STATUS=unavailable
    gap codegen_objdump_unavailable
elif [ ! -f "$ASM_BIN" ]; then
    CODEGEN_STATUS=fail
    echo "selected binary missing: $ASM_BIN" >> "$CODEGEN_LOG"
    fail_phase codegen_report 1 "$CODEGEN_LOG"
    finish
    exit 1
else
    CODEGEN_STATUS=fail
    CODEGEN_ERR=0
    if ! LC_ALL=C "$OBJDUMP_TOOL" -d --demangle --no-show-raw-insn "$ASM_BIN" > "$ASM_FULL" 2> "$CODEGEN_LOG"; then
        CODEGEN_ERR=1
    elif [ ! -s "$ASM_FULL" ]; then
        echo "Disassembly output is empty" >> "$CODEGEN_LOG"
        CODEGEN_ERR=1
    elif ! "$SCRIPT_DIR/dsp-hotpath-filter.py" --full "$ASM_FULL" --filtered "$ASM_TARGET" \
        --summary "$ASM_SUMMARY" --profile "$SCRIPT_DIR/codegen-profile.json" 2>&1 | tee -a "$CODEGEN_LOG"; then
        CODEGEN_ERR=1
    fi

    if [ "$CODEGEN_ERR" -eq 0 ]; then
        CODEGEN_STATUS=pass
        cp -p "$ASM_TARGET" target/ 2>/dev/null || true; cp -p "$ASM_SUMMARY" target/ 2>/dev/null || true
        ok "codegen static gates passed"
        emit "PHASE: codegen_report PASS log=$CODEGEN_LOG"
    else
        fail_phase codegen_report "$CODEGEN_ERR" "$CODEGEN_LOG"
        finish
        exit 1
    fi
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

deliver_step_fail() {
    local step_msg="$1" rc="${2:-1}"
    echo "ERROR in deliver_artifact: $step_msg" >> "$DELIVER_LOG"
    rm -rf "$STAGE_DIR"
    fail_phase deliver_artifact "$rc" "$DELIVER_LOG"
    finish
    exit 1
}

# Explicit verified delivery pipeline (fail-closed atomic delivery)
cp "$BUILD_BIN" "$STAGE_BIN" >> "$DELIVER_LOG" 2>&1 || deliver_step_fail "cp failed" $?
strip --strip-all "$STAGE_BIN" >> "$DELIVER_LOG" 2>&1 || deliver_step_fail "strip failed" $?
chmod 0755 "$STAGE_BIN" >> "$DELIVER_LOG" 2>&1 || deliver_step_fail "chmod failed" $?
"$STAGE_BIN" --diagnose > "$LOG_DIR/build-release-diagnose.out" 2>> "$DELIVER_LOG" || deliver_step_fail "diagnose failed" $?
[ -s "$STAGE_BIN" ] || deliver_step_fail "stage binary empty or missing" 1

DIST_SHA=$(sha256sum "$STAGE_BIN" | awk '{print $1}')
mv -T "$STAGE_BIN" "$DIST_BIN" || deliver_step_fail "mv to dist failed" $?
rmdir "$STAGE_DIR" || true
[ "$(sha256sum "$DIST_BIN" | awk '{print $1}')" = "$DIST_SHA" ] || deliver_step_fail "published SHA-256 differs from staged binary" 1
echo "Delivered canonical binary: $DIST_BIN (sha256=$DIST_SHA)" >> "$DELIVER_LOG"
ok "canonical artifact: $DIST_BIN (sha256=$DIST_SHA)"
emit "PHASE: deliver_artifact PASS log=$DELIVER_LOG"

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
    cleanup_packaging() { rm -rf "$PKG_DIR" "$VERIFY_DIR" "$FLATPAK_BUILD_DIR" "$FLATPAK_REPO_DIR" "$FLATPAK_SMOKE_DIR"; restore_paranoid; }
    trap cleanup_packaging EXIT
    SOURCE_DATE_EPOCH=$(git log -1 --pretty=%ct 2>/dev/null || date +%s)
    mkdir -p "$PKG_DIR/$ARCHIVE_NAME"
    cp "$DIST_BIN" "$PKG_DIR/$ARCHIVE_NAME/nam-audio-pipe"
    cp README.md "$PKG_DIR/$ARCHIVE_NAME/" 2>/dev/null || true
    if [ -f LICENSE.txt ]; then cp LICENSE.txt "$PKG_DIR/$ARCHIVE_NAME/"; else echo "ERROR: LICENSE.txt missing" >&2; exit 1; fi
    if [ -f NOTICE ]; then cp NOTICE "$PKG_DIR/$ARCHIVE_NAME/"; else echo "ERROR: NOTICE missing" >&2; exit 1; fi
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
    SDK_NAME=org.freedesktop.Sdk
    if ! flatpak info org.freedesktop.Sdk//26.08 >/dev/null 2>&1; then SDK_NAME=org.freedesktop.Platform; fi
    if ! flatpak build-init "$FLATPAK_BUILD_DIR" "$APP_ID" "$SDK_NAME" org.freedesktop.Platform 26.08 >> "$PKG_LOG" 2>&1; then
        fail_phase packaging 1 "$PKG_LOG"
        die "flatpak build-init failed"
    fi
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
    if [ -f LICENSE.txt ]; then cp LICENSE.txt "$FLATPAK_BUILD_DIR/files/share/licenses/$APP_ID/"; else echo "ERROR: LICENSE.txt missing" >&2; exit 1; fi
    if [ -f NOTICE ]; then cp NOTICE "$FLATPAK_BUILD_DIR/files/share/licenses/$APP_ID/"; else echo "ERROR: NOTICE missing" >&2; exit 1; fi
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
    ok "flatpak bundle: $FLATPAK_BUNDLE"
    emit "PHASE: packaging PASS log=$PKG_LOG"
fi

# ---------------------------------------------------------------------------
# Phase 8: typed build receipt (failure to write it is a failure).
# ---------------------------------------------------------------------------
restore_paranoid
phase build_receipt
RECEIPT_LOG="$LOG_DIR/build-release-build_receipt.log"
PGO_STATUS=skipped
BOLT_STATUS="${BOLT_STATUS:-skipped}"
if [ "$PGO_APPLIED" = true ]; then PGO_STATUS=applied; fi
if [ "$BOLT_APPLIED" = true ]; then BOLT_STATUS=applied; fi
RCPT_ARGS=(--out "$BUILD_RECEIPT" --artifact "$DIST_BIN" --package-name nam-audio-pipe
    --package-version "$VERSION" --git-commit "$GIT_COMMIT" --git-branch "$GIT_BRANCH"
    --git-dirty "$GIT_DIRTY" --cargo-lock Cargo.lock --rustc-version "$(rustc --version)"
    --rustflags "$RELEASE_RUSTFLAGS" --pgo-status "$PGO_STATUS" --bolt-status "$BOLT_STATUS"
    --codegen-status "$CODEGEN_STATUS")
if [ -n "$BOLT_PROOF" ] && [ "$BOLT_PROOF" != "none" ]; then RCPT_ARGS+=(--bolt-proof "$BOLT_PROOF"); fi
if [ "$PGO_APPLIED" = true ] && [ -f "$PGO_RECEIPT" ]; then RCPT_ARGS+=(--pgo-workload-receipt "$PGO_RECEIPT"); fi
if [ -n "$BOLT_VERSION" ]; then RCPT_ARGS+=(--bolt-version "$BOLT_VERSION"); fi
if [ -n "$BOLT_PROFILE" ] && [ -f "$BOLT_PROFILE" ]; then RCPT_ARGS+=(--bolt-profile "$BOLT_PROFILE"); fi
if [ -f "$CEREMONY_RECEIPT" ]; then RCPT_ARGS+=(--ceremony-receipt "$CEREMONY_RECEIPT"); fi
if [ -n "$TARBALL_PATH" ]; then RCPT_ARGS+=(--tarball-path "$TARBALL_PATH"); fi
if [ -n "$FLATPAK_PATH" ]; then RCPT_ARGS+=(--flatpak-path "$FLATPAK_PATH"); fi
if [ ${#GAPS[@]} -gt 0 ]; then RCPT_ARGS+=(--gaps "$(IFS=,; echo "${GAPS[*]}")"); fi
if [ "$DO_STRICT" = 1 ]; then RCPT_ARGS+=(--strict); fi
if ! "$RECEIPT_BIN" build-receipt "${RCPT_ARGS[@]}" 2>&1 | tee "$RECEIPT_LOG"; then
    rm -f "$BUILD_RECEIPT"
    fail_phase build_receipt "${PIPESTATUS[0]}" "$RECEIPT_LOG"
    finish
    exit 1
fi
ok "build receipt: $BUILD_RECEIPT"
# The long audit reuses this id so build and long receipts reconcile even when
# the three commands run as independent processes.
printf '%s\n' "$NAM_RUN_ID" > "$LOG_DIR/build-run-id"
emit "PHASE: build_receipt PASS log=$RECEIPT_LOG"
finish
