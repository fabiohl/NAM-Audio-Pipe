<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Automated Test Suite & Verification Architecture — `NAM-Audio-Pipe`

This document details the automated test suite, integration harness, verification scripts, and compiler optimization pipeline for **NAM-Audio-Pipe** ([`../`](../)).

---

## 1. Scope & Crate Features Taxonomy

`NAM-Audio-Pipe` testing relies on feature flags configured in [`Cargo.toml`](../Cargo.toml):

| Feature Flag  | Description                                                                                            | Test Usage Scope                                                |
|:------------- |:------------------------------------------------------------------------------------------------------ |:--------------------------------------------------------------- |
| **`default`** | Standard empty feature set for standalone production builds.                                           | Standard release builds and baseline compilation checks.        |
| **`testing`** | Propagates `NeuralAmpModeler-rs/testing`, enabling internal fixtures, mock controls, and test helpers. | Mandatory flag when executing unit and integration test suites. |

---

## 2. Automated Test Inventory

The test suite is partitioned into unit tests co-located with module sources and integration tests under [`tests/`](../tests/):

### 2.1 Unit Tests (Module-Level)

- **CLI Parsing ([`src/standalone/cli_test.rs`](../src/standalone/cli_test.rs)):** Validates command-line flag combinations, model and cabinet path parsing, gain limits (`-20.0` to `+20.0` dB), oversampling enum mappings, and diagnostic flags (`--diagnose`, `--diagnose-full`).
- **Real-Time System Setup & Hardware Sink Detection ([`src/standalone/rt_setup/rt_setup_test.rs`](../src/standalone/rt_setup/rt_setup_test.rs), [`src/standalone/rt_setup/pm_qos.rs`](../src/standalone/rt_setup/pm_qos.rs)):** Validates CPU affinity selection heuristics (`select_optimal_cpu`), interrupt load parsing from `/proc/interrupts` streaming per physical CPU (`parse_interrupts_per_cpu`), PM QoS `/dev/cpu_dma_latency` interaction, and default hardware sink detection via `pw-metadata` with a 500 ms watchdog timeout and self-capture node filtering.
- **Telemetry & Status Polling ([`src/standalone/rt_setup/telemetry_test.rs`](../src/standalone/rt_setup/telemetry_test.rs)):** Validates `PollState` struct lifecycle, hugepage flag synchronization, telemetry throttling, silent and fading transition states, and diagnostic RT status flag clearing during periodic monitoring (`poll_rt_status`).
- **Shared Memory Bridge Allocation ([`src/standalone/pw_host/bridge.rs`](../src/standalone/pw_host/bridge.rs)):** Validates page-aligned (4096-byte) memory layout allocation, `madvise` memory flags, and initial atomic generation counters in `allocate_dsp_bridge`.
- **PipeWire Capture Stream Setup ([`src/standalone/pw_host/capture/setup_test.rs`](../src/standalone/pw_host/capture/setup_test.rs)):** Tests capture property dictionary creation, latency string formatting (`node.latency`), and SPA Pod audio format generation.
- **Process Callback & Gain Staging ([`src/standalone/pw_host/rt_callback/process_test.rs`](../src/standalone/pw_host/rt_callback/process_test.rs)):** Tests the RT audio processing callback, channel extraction, gain multiplier smoothing, noise gate envelope evaluation, recording queue dispatch, and silence trimming.
- **SPSC Resource Swaps & Command Handling ([`src/standalone/pw_host/rt_callback/commands.rs`](../src/standalone/pw_host/rt_callback/commands.rs), [`src/standalone/pw_host/rt_callback/cabsim_swap.rs`](../src/standalone/pw_host/rt_callback/cabsim_swap.rs), [`src/standalone/pw_host/rt_callback/resampler_swap.rs`](../src/standalone/pw_host/rt_callback/resampler_swap.rs)):** Validates zero-alloc dynamic swaps for neural models, cabinet IR convolution engines (`drain_cabsims`), sample rate converters (`drain_resamplers`), and oversampling engines (`drain_os_engines`), asserting correct GC cascades and RT parking lot dirty flag updates.
- **Off-RT Rebuild Handlers ([`src/standalone/pw_host/handlers_test.rs`](../src/standalone/pw_host/handlers_test.rs)):** Validates dynamic resampler reconstruction, cabinet IR convolution updates, slimmable container swaps, and oversampling filter rebuilds triggered by sample rate changes.
- **Host Execution & Shutdown ([`src/standalone/pw_host/run_test.rs`](../src/standalone/pw_host/run_test.rs)):** Validates bounded retry on `push_stream_stop` and bounded thread join for `nam-recording-io` during host teardown.

### 2.2 Integration Test Suites ([`tests/`](../tests/))

- **CLI Black-Box Smoke Tests ([`tests/e2e_cli.rs`](../tests/e2e_cli.rs)):** Executes the compiled binary (`CARGO_BIN_EXE_nam-audio-pipe`) using `std::process::Command` to verify CLI help screens, diagnostic output formatting, and robust error handling for invalid or out-of-range options.
- **Asynchronous WAV Recording Suite ([`tests/recording.rs`](../tests/recording.rs)):**
  - `disk_writer_loop_creates_valid_wav`: Asserts that `disk_writer_loop` consumes ring buffer audio blocks and produces valid 32-bit float stereo WAV files.
  - `disk_writer_loop_metadata_then_stream_stop_creates_empty_wav`: Validates clean zero-sample WAV creation upon immediate stream stop.
  - `disk_writer_loop_discards_audio_before_metadata`: Verifies uninitialized audio blocks before stream metadata are safely discarded.
  - `record_e2e_pipewire_wav_header_matches_bytes` (R-13): Full end-to-end recording test under live PipeWire, asserting that the finalized WAV `data` chunk header exactly matches the PCM bytes written to disk.
- **PipeWire Live Pipeline Integration ([`tests/pw_integration.rs`](../tests/pw_integration.rs)):** Validates the full PipeWire host lifecycle (context creation, dual-stream setup, SPSC parameter injection, real audio quantum execution `last_n_samples > 0`, and atomic shutdown) against a running PipeWire daemon.

---

## 3. Verification Scripts & QA Suite (`utils/`)

### 3.1 Static Analysis Quality Gate ([`utils/lints.sh`](../utils/lints.sh))

Executes comprehensive static quality checks:

1. **Code Formatting:** Enforces standard Rust formatting via `cargo fmt --all`.
2. **Compilation Matrix:** Checks all targets with `--all-features` and `--no-default-features`.
3. **Strict Clippy:** Executes `cargo clippy -- -D warnings` across all feature permutations.
4. **SPDX License Headers:** Deterministically verifies that all `.rs` and `.sh` files contain valid `GPL-3.0-or-later` or `MIT` SPDX license identifier headers.
5. **Clippy Suppression Audit:** Flags any undocumented `#[allow(clippy::...)]` attribute that lacks an explanatory comment.

### 3.2 Agile First Line of Defense ([`utils/tests-quick.sh`](../utils/tests-quick.sh))

A 4-phase agile test runner executing under low CPU/IO priority (`nice -n 19`, `ionice -c 3`) to prevent audio system disruption:

```text
┌─────────────────────────────────────────────────────────────────────────────┐
│                       utils/tests-quick.sh Execution Flow                   │
├─────────────────────────────────────────────────────────────────────────────┤
│  Phase 1: Structural (Debug)                                                │
│    - cargo test --features testing --lib --bin nam-audio-pipe               │
│                 --test recording --test e2e_cli                             │
├─────────────────────────────────────────────────────────────────────────────┤
│  Phase 2: Release Verification (Release, S6-T04 / RES-04)                   │
│    - cargo test --features testing --test recording --test e2e_cli --release│
├─────────────────────────────────────────────────────────────────────────────┤
│  Phase 3: PipeWire Live Integration (Release, Daemon Probe)                 │
│    - Probes pw-cli info 0 -> Runs tests/pw_integration.rs (asserts DSP run) │
│    - Fail-closed: Daemon failure inside test causes immediate panic         │
├─────────────────────────────────────────────────────────────────────────────┤
│  Phase 4: Recording io_uring Capability (Release, Native Probe)             │
│    - Probes src/bin/io_uring_probe -> Runs ignored tests/recording.rs       │
│    - Asserts WAV data header byte-size match (R-13)                         │
└─────────────────────────────────────────────────────────────────────────────┘
```

- Outputs each phase log to `target/logs/quick-phaseN.log`.
- Generates a machine-readable summary receipt at `target/logs/quick-receipt.txt`.
- Supports strict mode (`NAM_QUICK_STRICT=1`) to promote any environment GAP to a hard test failure.

---

## 4. Compiler Optimization Pipeline ([`utils/build-release.sh`](../utils/build-release.sh))

A multi-stage optimization and packaging pipeline combining Profile-Guided Optimization (PGO), LLVM BOLT, and Flatpak distribution:

1. **Phase 1 (Environment Verification):** Validates required tools (`rustc`, `cargo`, `python3`, `tar`, `zstd`, `flatpak`, `llvm-profdata`, `llvm-bolt`, `perf`) and verifies `x86-64-v3` compiler flags.
2. **Phase 2 (PGO Workload Profiling):** Builds `src/bin/pgo_workload.rs` with `-Cprofile-generate`, executes synthetic neural DSP workloads, and merges execution counters into `merged.profdata`.
3. **Phase 3 (PGO Compilation):** Recompiles `nam-audio-pipe` using `-Cprofile-use=merged.profdata` and relocation symbols (`-Clink-arg=-Wl,-q`).
4. **Phase 4 (LLVM BOLT Layout Optimization):** Uses Linux `perf` to capture CPU branch and instruction samples, then executes `llvm-bolt` to optimize instruction cache layout and minimize TLB misses.
5. **Phase 4.5 (Assembly Hotspot Report):** Disassembles critical DSP loops into `target/dsp_hotpath.asm` for microarchitectural analysis.
6. **Phase 5 (Deployment):** Strips and installs the hyper-optimized binary to `~/.local/bin/nam-audio-pipe`.
7. **Phase 6 (Release Packaging):** Packages the release tarball `~/nam-audio-pipe-vx.y.z-linux-x86_64-v3.tar.zst` with installer script and documentation.
8. **Phase 7 (Flatpak Application Packaging):** Builds the standalone Flatpak bundle `~/nam-audio-pipe-vx.y.z-linux-x86_64-v3.flatpak` with low-latency PipeWire, PulseAudio, IPC permissions, and desktop metadata. Support `--install` flag for automated user installation.

---

## 5. Execution Commands & Developer Workflow

All test commands must be executed within `./NAM-Audio-Pipe/`:

```bash
# 1. Run static analysis quality gate
./utils/lints.sh

# 2. Run consolidated quick test suite
./utils/tests-quick.sh

# 3. Run unit tests only
cargo test --features testing --lib

# 4. Run CLI smoke tests
cargo test --features testing --test e2e_cli

# 5. Run live PipeWire integration test (requires running PipeWire daemon)
cargo test --features testing --release --test pw_integration -- --ignored --nocapture

# 6. Run full recording io_uring integration test
cargo test --features testing --release --test recording -- --ignored --nocapture

# 7. Run full PGO + LLVM BOLT release build
./utils/build-release.sh
```

---

## 6. Quality Gates & Architectural Constraints

| Metric / Test Gate              | Constraint / Threshold                                        | Enforced In                                  |
|:------------------------------- |:------------------------------------------------------------- |:-------------------------------------------- |
| **Real-Time Memory Safety**     | Exactly 0 heap allocations / deallocations in RT callback     | Architectural invariants & review            |
| **Lock-Free Concurrency**       | Exactly 0 mutex locks or blocking syscalls on audio thread    | `src/standalone/pw_host/`                    |
| **WAV Header Integrity (R-13)** | Finalized WAV header `data` size == exact PCM bytes written   | `tests/recording.rs`                         |
| **PipeWire Execution (R-17)**   | `LIVE_PW=RAN` emitted only after ≥1 real DSP quantum executed | `tests/pw_integration.rs` / `tests-quick.sh` |
| **Target CPU Baseline**         | Mandatory `x86-64-v3` (AVX2, FMA, BMI2) minimum target        | `.cargo/config.toml`                         |

---

## 7. References

- [NAM-Audio-Pipe Architecture](architecture.md) — Core host architecture and threading model.
- [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) — Core neural DSP engine.
- [PipeWire Official Documentation](https://docs.pipewire.org/) — Linux audio graph daemon.
