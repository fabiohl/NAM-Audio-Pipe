<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Automated Test Suite & Verification Architecture - `NAM-Audio-Pipe`

This document details the automated testing architecture, verification suites, regression gates, and compiler optimization pipeline for **NAM-Audio-Pipe** ([`../`](../)).

> **Documentation Philosophy**: Source code is the primordial source of truth. This guide does not transcribe implementation details line-by-line; instead, it establishes the testing architecture, explains the *why* behind design invariants, defines critical real-time contracts, and guides developers on running and extending test suites safely.

---

## 1. Scope & Crate Features Taxonomy

`NAM-Audio-Pipe` testing relies on feature flags defined in [`Cargo.toml`](../Cargo.toml):

| Feature Flag     | Purpose                                                                | Operational Scope                                                  |
|:---------------- |:---------------------------------------------------------------------- |:------------------------------------------------------------------ |
| **`default`**    | Empty feature set for standalone production binaries.                  | Production builds and baseline compilation checks.                 |
| **`testing`**    | Propagates `NeuralAmpModeler-rs/testing`, exposing mocks and fixtures. | Mandatory for executing unit, integration, and verification tests. |
| **`heap-audit`** | Enables counting allocators to verify 0 heap allocations in RT paths.  | Used in the long suite's `heap-audit` phase and RT callback safety tests.    |

---

## 2. Automated Test Inventory

The test architecture separates fast unit tests (verifying isolated module contracts) from integration tests (exercising the full audio graph, disk I/O, IPC, and operating system interfaces).

```text
NAM-Audio-Pipe Test Hierarchy
├── Unit Tests (src/...)
│   ├── CLI & Validation           -> Argument parsing, buffer size & gate domains
│   ├── Real-Time Setup & Hardware -> Thread priority, CPU affinity, PM QoS
│   ├── Audio Callback (RT)        -> DSP staging, panic containment, zero-alloc
│   ├── SPSC Resource Swaps        -> Lock-free dynamic model/IR/resampler updates
│   ├── Host FSM & Reconnect       -> Stream state transitions, bounded exponential backoff
│   └── Asynchronous Recording     -> Lock-free slot pool, worker lifecycle, disk serialization
└── Integration Tests (tests/...)
    ├── e2e_cli.rs                 -> Black-box subprocess CLI validation & exit codes
    ├── recording*.rs              -> Bit-exact WAV persistence, fault-injection & io_uring
    ├── pw_integration.rs          -> Live PipeWire context, stream binding & reconnect
    ├── service_resilience.rs      -> POSIX signal bursts, graceful finalization & fail-closed FSM
    ├── stereo_fidelity.rs         -> Bit-identical L/R channel processing & SNR oracles
    ├── swap_stress.rs             -> Multi-producer lock-free swap stress under RT load
    ├── soak_extended.rs           -> Accelerated-timeline soak (320k blocks, RSS stability)
    ├── endurance.rs               -> Real wall-clock endurance (fail-closed validation windows)
    ├── rt_metrics.rs              -> Nanosecond deadline & inter-callback jitter profiling
    └── distribution_qa.rs         -> AppStream XML & dist-binary packaging smoke
```

---

### 2.1 Unit Tests (Module-Level)

Each unit test verifies strict architectural invariants directly within the relevant module:

#### CLI & Argument Parsing

- **Location**: [`src/standalone/cli_test.rs`](../src/standalone/cli_test.rs)
- **Contract**: Parses command-line flags, model/IR paths, gain stages (`[-20.0, +20.0]` dB), oversampling factors, and diagnostic modes (`--diagnose`, `--diagnose-full`).
- **Key Invariants**:
  - **Buffer Size Domain**: Strictly `{0} ∪ {2^k | 16 ≤ 2^k ≤ 8192}`. Valid values (`0, 16, 32, ..., 8192`) are accepted; invalid values (non-powers-of-two, below 16, above 8192) fail fast with a typed `BufferSizeError` before any PipeWire connection.
  - **Silence Gate Domain**: Polymorphic mode handling:
    - Literals `on`/`true`/`default` map to `GateConfig::default_on()` (open at `-70.0 dBFS`, close at `-80.0 dBFS`).
    - Literals `off`/`false`/`0` map to `GateConfig::Off` (linear threshold `0.0`, permanently open FSM).
    - Numeric thresholds `[-96.0, -20.0]` dBFS (with optional `db`/`dbfs` suffix) derive the close threshold via 10 dB Schmitt hysteresis: `close = max(open − 10 dB, −96 dB)`.
    - Positive finite inputs auto-normalize with a logged warning (`60` → `-60.0 dBFS`).
    - Out-of-domain or malformed inputs reject immediately with exit code 2.

#### Real-Time Thread Setup & PM QoS

- **Location**: [`src/standalone/rt_setup/rt_setup_test.rs`](../src/standalone/rt_setup/rt_setup_test.rs), [`src/standalone/rt_setup/pm_qos.rs`](../src/standalone/rt_setup/pm_qos.rs)
- **Contract**: Configures thread scheduling, CPU affinity, and power management constraints.
- **Key Invariants**: Interacts with `/dev/cpu_dma_latency` to prevent CPU C-state entry latency spikes during audio streaming. Detects default hardware sinks via `pw-metadata` with a 500 ms watchdog timeout and filters out self-capture loopback nodes. Re-exports core affinity heuristics from the engine's verified `rt_hardening` module.

#### Telemetry & Status Polling

- **Location**: [`src/standalone/rt_setup/telemetry_test.rs`](../src/standalone/rt_setup/telemetry_test.rs)
- **Contract**: Periodically reads atomic bitmasks (`RtStatusFlags`) from the RT thread and publishes throttled off-RT logs.
- **Key Invariants**:
  - **Episode Latches**: Flag and counter signals emit at most once per anomaly episode and re-arm only after the condition clears.
  - **Gate Log Throttling**: Limits gate transition logs to at most one entry per `GATE_LOG_MIN_INTERVAL`, folding rapid chatters into `(×N)` counters.
  - **Diagnostic Format**: Emits concise `[Exxxx | MNEMONIC]` lines without dumping bloated log traces.

#### First-Run Launcher & Terminal Presentation

- **Location**: [`src/standalone/launcher.rs`](../src/standalone/launcher.rs), [`src/standalone/banner.rs`](../src/standalone/banner.rs)
- **Contract**: Controls non-interactive vs interactive execution and terminal UI formatting.
- **Key Invariants**:
  - `startup_mode(args_empty)` exits immediately with code 0 when called without arguments, enforcing a strict non-interactive CLI contract.
  - Interactive pause (`--pause`) prompts for Enter only when standard input is an active TTY.
  - Banner rendering produces deterministic session summaries across all model/IR/rate/quantum configurations, with and without ANSI escape sequences.

#### Shared Memory Bridge

- **Location**: [`src/standalone/pw_host/bridge.rs`](../src/standalone/pw_host/bridge.rs)
- **Contract**: Allocates shared memory for zero-copy lock-free ping-pong transfer between PipeWire capture and playback streams.
- **Key Invariants**: Enforces 4096-byte page alignment, applies `madvise` memory hints, and verifies atomic generation counter progression in `allocate_dsp_bridge`.

#### PipeWire Capture Stream Setup

- **Location**: [`src/standalone/pw_host/capture/setup_test.rs`](../src/standalone/pw_host/capture/setup_test.rs)
- **Contract**: Builds SPA Pod audio format negotiation objects and stream property dictionaries (`node.latency`, media role, node name).

#### Real-Time Audio Callback & Gain Staging

- **Location**: [`src/standalone/pw_host/rt_callback/process_test.rs`](../src/standalone/pw_host/rt_callback/process_test.rs)
- **Contract**: Executes per-quantum audio processing on the PipeWire real-time thread.
- **Key Invariants**:
  - Zero heap allocation/deallocation on the hot-path (verified under `feature = "heap-audit"`).
  - Handles buffer quantum sizes up to `MAX_BRIDGE_BUF` (8192 frames) without dropping or truncating blocks.
  - Tracks overruns in both blocks and frames (`OVERRUN_COUNT` and `OVERRUN_FRAMES_COUNT`), preserving the reconciliation invariant: `captured_frames == enqueued_frames + lost_frames`.
  - Classifies SPA chunk windows (`Valid`/`Absent`/`Malformed`): corrupt chunks (`SPA_CHUNK_FLAG_CORRUPTED`) or invalid strides raise `RT_STATUS_HOST_CONTRACT_VIOLATION` (E2304) and mute output fail-closed.

#### Real-Time Panic Containment

- **Location**: [`src/standalone/pw_host/rt_callback/mod.rs`](../src/standalone/pw_host/rt_callback/mod.rs)
- **Contract**: Isolates unexpected panics within the RT callback to prevent crashing the host daemon or triggering C-FFI aborts.
- **Key Invariants**:
  - Wraps RT closure bodies in `catch_unwind(AssertUnwindSafe)` once per quantum.
  - On panic, latches the fatal atomic flag `RT_STATUS_PANIC_CAPTURED` (bit 31) and immediately mutes audio.
  - Subsequent callback invocations check the latch via a single `Relaxed` load and exit instantly, preventing repetitive panic loops while awaiting graceful teardown.
  - Release profile inherits `panic = "unwind"`, ensuring unwinding containment remains functional in shipped production artifacts.
  - The control loop treats a captured panic as terminal, bypassing reconnect cycles and performing clean WAV finalization.

#### Lock-Free SPSC Resource Swaps & Rebuild Handlers

- **Location**: [`src/standalone/pw_host/rt_callback/commands.rs`](../src/standalone/pw_host/rt_callback/commands.rs), [`cabsim_swap.rs`](../src/standalone/pw_host/rt_callback/cabsim_swap.rs), [`resampler_swap.rs`](../src/standalone/pw_host/rt_callback/resampler_swap.rs), [`handlers_test.rs`](../src/standalone/pw_host/handlers_test.rs)
- **Contract**: Enables non-blocking, zero-allocation runtime swaps of neural models, cabinet IRs, sample rate converters, and oversampling engines.
- **Key Invariants**: Command rings use bounded SPSC queues. The audio thread drains pending updates without blocking, while old resources are returned to an off-RT garbage collection (GC) queue for safe deallocation.

#### Backend State Machine & Fail-Fast SLA

- **Location**: [`src/standalone/pw_host/status_test.rs`](../src/standalone/pw_host/status_test.rs)
- **Contract**: Manages host operational state transitions (`Starting` → `Running` → `Degraded`/`Failed` → `Terminated`).
- **Key Invariants**:
  - **Sticky Failure**: Once marked `Failed`, the state cannot be reverted to `Running` except through a formal reconnection handshake (`begin_reconnect`).
  - **Fail-Fast SLA**: The main control loop observes a stream failure and initiates teardown in under 200 ms.
  - A captured RT panic transitions the backend to `Failed`, allowing clean resource release instead of an abrupt abort.

#### Bounded Reconnection Policy

- **Location**: [`src/standalone/pw_host/reconnect_test.rs`](../src/standalone/pw_host/reconnect_test.rs)
- **Contract**: Implements automated reconnection to the PipeWire daemon when interrupted.
- **Key Invariants**:
  - **Default Policy**: 6 attempts with progressive backoff (250 → 500 → 1000 → 2000 → 4000 → 4000 ms), capped at a total budget of 11,750 ms.
  - **Fail-Fast Mode**: `--fail-fast` sets attempts to 0, terminating immediately on stream disconnect.
  - **Stability Reset**: After 60 seconds (`STABLE_RESET`) of uninterrupted healthy streaming, the attempt budget fully replenishes. Isolated transient drops do not exhaust the retry budget over time.

#### Asynchronous Recording Engine & Guard Lifecycle

- **Location**: [`src/recording/`](../src/recording/)
- **Contract**: Manages lock-free audio capture to disk without blocking the real-time processing thread.
- **Key Invariants**:
  - **RAII Teardown**: `RecordingWorkerGuard` guarantees bounded worker joins, stopping worker threads and flushing headers safely on drop.
  - **Loss Accounting**: If overruns occurred during recording, the process exits with code 6 (`SuccessWithLoss`), detailing lost frames without hiding dropped data.
  - **Memory Subsystem Efficiency**: The promoted `RecordingPool` slot transport reduces memory subsystem traffic by ~57% compared to traditional inline SPSC rings.
  - **Disk Space Transparency**: Periodically queries filesystem headroom, emitting warnings when free space drops below 1 GiB or 5 minutes of remaining audio.

#### PipeWire Log Redirection & Off-RT Log Ring

- **Location**: [`src/standalone/pw_host/log_redirect.rs`](../src/standalone/pw_host/log_redirect.rs), [`rt_log_ring.rs`](../src/standalone/pw_host/rt_log_ring.rs), [`rt_log_mark.rs`](../src/standalone/rt_setup/rt_log_mark.rs)
- **Contract**: Intercepts C-ABI PipeWire logs and routes them safely away from the real-time audio thread.
- **Key Invariants**:
  - Uses a fixed 1024-byte stack buffer with allocation-free UTF-8 conversion.
  - Audio threads marked via TLS (`rt_log_mark`) divert log messages into a pre-allocated SPSC ring (~344 KiB), avoiding blocking `log::*` macros on the audio thread.
  - An off-RT drainer thread consumes the ring and replays messages into the application logger in original order.

#### Signal Handling & Process Lifecycle

- **Location**: [`src/standalone/signals_test.rs`](../src/standalone/signals_test.rs)
- **Contract**: Coordinates POSIX signal handling (`SIGINT`, `SIGTERM`) for graceful process termination.
- **Key Invariants**:
  - Implements a staged 500 ms grace window. Repeated signals within this burst window are coalesced into a single cooperative shutdown.
  - Hard escalation (`_exit(1)`) occurs only if cleanup hangs beyond the 500 ms window.

---

### 2.2 Integration Test Suites ([`tests/`](../tests/))

Integration tests validate full-system behavior against real external interfaces, subprocesses, and filesystems:

```text
tests/
├── e2e_cli.rs                   -> Subprocess CLI smoke tests, argument parsing & exit codes
├── recording.rs                 -> Asynchronous disk writer, zero-sample streams, io_uring
├── recording_fault_injection.rs -> Byte-exact PCM roundtrips, RIFF chunk walker, leak sweeps
├── pw_integration.rs            -> Live PipeWire streaming, tone driver & reconnect cycles
├── service_resilience.rs        -> Real SIGTERM handling, grace windows, fail-closed FSM
├── stereo_fidelity.rs           -> Bit-exact dual-mono vs stereo pipeline equality (SNR > 120 dB)
├── swap_stress.rs               -> Concurrent multi-producer swap bursts under RT load
├── soak_extended.rs             -> Accelerated timeline soak (320k blocks, RSS leak checks)
├── endurance.rs                 -> Wall-clock real-time endurance testing
├── rt_metrics.rs                -> Nanosecond deadline & jitter benchmarking (SCHED_FIFO)
└── distribution_qa.rs           -> AppStream validation & dist-binary packaging smoke
```

#### CLI End-to-End Black-Box Suite

- **File**: [`tests/e2e_cli.rs`](../tests/e2e_cli.rs)
- **Objective**: Validates user-facing CLI behavior by invoking the compiled binary (`CARGO_BIN_EXE_nam-audio-pipe`).
- **Verifications**:
  - Rejection of invalid `--buffer-size` flags (`-b 100`, `-b 1`, `-b 16384`) with clear argument error messages on stderr *before* attempting PipeWire initialization.
  - Rejection of invalid `--gate` modes (`--gate invalid`, `--gate -120`) with exit code 2.
  - Typed exit code classification: missing/corrupt model exits 3 (`Model`), invalid IR exits 4 (`Cab IR`), unreachable PipeWire exits 5 (`PipeWire`), and recording setup failure exits 6 (`Recording`).
  - `--cab-optional` allows startup when `--cab` is missing by issuing a warning and bypassing cabinet simulation.

#### Asynchronous WAV Recording Suite

- **File**: [`tests/recording.rs`](../tests/recording.rs)
- **Objective**: Asserts that `disk_writer_loop` safely drains ring buffers into valid 32-bit float stereo WAV files.
- **Verifications**: Correct RIFF/WAVE header formation, clean zero-sample WAV finalization on immediate stop, discard of uninitialized blocks, fail-fast behavior on unusable output directories, and end-to-end byte-size equality (`data` header chunk matches written PCM bytes).

#### Recording Fault-Injection & Anti-Leak Harness

- **File**: [`tests/recording_fault_injection.rs`](../tests/recording_fault_injection.rs)
- **Objective**: Stress-tests recording persistence under adverse filesystem conditions and abnormal shutdowns.
- **Verifications**:
  - **Independent Chunk Parser**: Validates RIFF structures using an internal chunk walker cross-checked against `hound`.
  - **Bit-Exact Roundtrips**: Verifies sample-by-sample fidelity for sine, deterministic LCG noise, and ramp signals, including mid-stream sample rate changes split into `_partN` files.
  - **Capacity Boundaries**: Persists full 2048, 2049, and 8192-frame quantums without truncation.
  - **Fault Resilience**: Mid-stream disk space exhaustion (`EFBIG`/`ENOSPC`) preserves partial WAV files cleanly up to the fault point.
  - **Resource Leak Sweeps**: Verifies that 100 consecutive start/stop cycles release all file descriptors and threads back to baseline.

#### Live PipeWire Host Pipeline

- **File**: [`tests/pw_integration.rs`](../tests/pw_integration.rs)
- **Objective**: Tests full audio graph execution against an active PipeWire daemon.
- **Verifications**: Context creation, dual-stream setup, parameter injection, real audio execution (`last_n_samples > 0`), and atomic shutdown. Uses an inaudible tone driver to keep the capture node actively scheduled. Includes `#[ignore]` private-daemon disruption tests (`daemon_bounce_*`, run by the long audit) proving automatic recovery after daemon restarts.

#### Service Resilience & Signal Harness

- **File**: [`tests/service_resilience.rs`](../tests/service_resilience.rs)
- **Objective**: Exercises live subprocess termination and degraded stream handling.
- **Verifications**:
  - Sends real `SIGTERM` signals to a running recording process, asserting exit code 0 and 100% valid, closed WAV files.
  - Rapid double-signal bursts within the 500 ms grace window are coalesced into a single graceful exit.
  - Daemon-free tests verify that playback buffer starvation produces exact analytical silence (`0.0f32`), and unsupported SPA audio formats trigger fail-closed muting with typed contract violations (E2304).

#### Stereo Fidelity & Channel Independence

- **File**: [`tests/stereo_fidelity.rs`](../tests/stereo_fidelity.rs)
- **Objective**: Verifies that the stereo DSP pipeline processes left and right channels with complete independence and bit-exact fidelity.
- **Verifications**: Feeds complex test signals (uncorrelated noise, quadrature chirps, Karplus-Strong string synthesis) through both stereo and isolated mono pipelines. Asserts exact bitwise identity (`MSE == 0.0`, `SNR > 120 dB`).

#### Concurrency & Swap Stress Harness

- **File**: [`tests/swap_stress.rs`](../tests/swap_stress.rs)
- **Objective**: Drives the real-time callback under heavy multi-producer concurrent swap load.
- **Verifications**: Floods the SPSC command queues with model reloads, CabSim IR updates, oversampling switches, and gain changes while processing audio. Proves that channel assignments never invert, atomic pairings remain consistent, and zero allocations occur on the audio thread.

#### Accelerated Soak & Wall-Clock Endurance Suites

- **Files**: [`tests/soak_extended.rs`](../tests/soak_extended.rs), [`tests/endurance.rs`](../tests/endurance.rs)
- **Objective**: Certifies memory and state stability over prolonged execution.
- **Verifications**:
  - **Accelerated Soak**: Processes 320,000 continuous audio blocks (~426 seconds of compressed audio) with thousands of concurrent swaps. Asserts zero page faults and memory drift within OS page margins (< 256 KiB raw `VmRSS` delta).
  - **Real Wall-Clock Endurance**: Runs at nominal hardware speed (48 kHz / 64 frames) for a configurable duration (`NAM_ENDURANCE_SECONDS`, default 30 s), asserting fail-closed validation windows and zero descriptor leaks.

#### Nanosecond Real-Time Metrics & Jitter Harness

- **File**: [`tests/rt_metrics.rs`](../tests/rt_metrics.rs)
- **Objective**: Evaluates deterministic execution timing using monotonic clocks (`CLOCK_MONOTONIC_RAW`).
- **Verifications**:
  - **RT Deadline Gate**: 10,000 consecutive quantums under full DSP load (WaveNet + CabSim). Worst-case execution must remain strictly below 85% of quantum budget (`(N/SR) * 10^9 * 0.85`).
  - **RT Jitter Gate**: Measures callback dispersion under thread contention, verifying tail jitter $p99.99 \le 0.75 \cdot T$ on calibrated real-time kernels.
  - **Concurrency Stress**: Validates lock-free state transitions and SPSC throughput across 16 competing threads.

#### Distribution QA Harness

- **File**: [`tests/distribution_qa.rs`](../tests/distribution_qa.rs)
- **Objective**: Certifies shipping metadata and the distributed binary artifact.
- **Verifications**: Strict AppStream XML validation (`io.github.fabiohl.NAMAudioPipe.metainfo.xml`) and stripped production binary smoke tests (`--diagnose`/`--help`). Release receipts and long-audit certification are verified by the release chain, not by this harness.

---

## 3. Verification Scripts & QA Suite (`utils/`)

Executable verification scripts in [`utils/`](../utils/) automate testing across different developer and CI phases:

### 3.1 Static Quality Gate ([`utils/lints.sh`](../utils/lints.sh))

Class A. It runs the following steps in order and aborts on the first failure:

1. **Formatting:** `cargo fmt --all` is applied during local development (N-5); with `CI=true` it only verifies with `--check`.
2. **Compilation matrix:** `cargo check --locked` for all-targets/all-features, lib/no-default-features, and all-targets/no-default-features.
3. **Strict Clippy:** the same matrix with `-- -D warnings`.
4. **Documentation:** `cargo doc --locked --no-deps --all-features` must build. Rustdoc warnings are capped while the pre-existing intra-doc link debt is addressed.
5. **SPDX headers:** every tracked `.rs`, `.sh`, `.py`, `build.rs`, and `Cargo.toml` must carry `GPL-3.0-or-later` or `MIT`.
6. **Text policies:** documented `#[allow(clippy::...)]` suppressions and the centralized `log::set_max_level` rule.
7. **Packaging:** mandatory Flatpak/AppStream/desktop paths, plus `appstreamcli validate --no-net --strict` and `desktop-file-validate`.

### 3.2 Quick Discipline ([`utils/tests-quick.sh`](../utils/tests-quick.sh))

Class A with a single invocation: `cargo test --locked --features testing --no-fail-fast` over all targets in the development profile. `#[ignore]` tests are skipped by construction. The discipline never builds release, never provisions infrastructure, never emits receipts, and never applies manual `--skip` lists. It is the suite expected at the end of each sprint and must stay within 5 minutes with a warm cache (measured: ≈45 s on the reference host).

### 3.3 Long Audit Suite ([`utils/tests-long.sh`](../utils/tests-long.sh))

Class B, operator-only. It records every phase result instead of aborting on the first failure, runs all `#[ignore]` product suites in release, and, with `--artifact`, binds the machine-readable receipt to the exact SHA-256 of `target/dist/nam-audio-pipe`.

> [!CAUTION]
> **Human Operator Exclusive:** AI agents may inspect `--help` but must never execute this suite.

`--strict` promotes any `GAP` to `FAIL` and exports `NAM_RT_STRICT=1`; `NAM_PHASE_TIMEOUT_S` overrides the timeout used by every phase command. The phases are:

| Receipt `phase_id` | Canonical phase | Harness & Invariant Evaluated                                                                                              | Output Log File                       |
|:------------------ |:--------------- |:-------------------------------------------------------------------------------------------------------------------------- |:------------------------------------- |
| **`phase0`**  | `catch-all`    | `cargo test --features testing --release --no-fail-fast` (non-ignored: `e2e_cli`, `distribution_qa`, `stereo_fidelity`, `--lib`). | `target/logs/long-catch-all.log`      |
| **`phase1`**  | `soak`         | `tests/soak_extended.rs` (320k blocks, compressed timeline) + `tests/swap_stress.rs`; asserts stable RSS.                  | `target/logs/long-soak.log`           |
| **`phase2`**  | `heap-audit`   | `cargo test --features "testing heap-audit" --release` over `--lib` and `tests/swap_stress.rs`; asserts zero heap allocations on RT paths. | `target/logs/long-heap-audit.log`     |
| **`phase3`**  | `rt-deadline`  | `tests/rt_metrics.rs` (`deadline`, 10k quantums, DSP load < 85% of the quantum budget; `SCHED_FIFO` when available).      | `target/logs/long-rt-deadline.log`    |
| **`phase4`**  | `rt-jitter`    | `tests/rt_metrics.rs` (`jitter`, 10k callbacks under contention; bounded inter-callback dispersion).                       | `target/logs/long-rt-jitter.log`      |
| **`phase5`**  | `concurrency`  | `tests/rt_metrics.rs` (`concurrent`; 16 threads racing swap requests, stream transitions, and SPSC rings without deadlocks). | `target/logs/long-concurrency.log`    |
| **`phase6`**  | `endurance`    | `tests/endurance.rs` (real timeline at 48 kHz/64 frames, `NAM_ENDURANCE_SECONDS`; fail-closed validation windows and zero leaks). | `target/logs/long-endurance.log`      |
| **`phase7`**  | `pw-resilience` | `tests/pw_integration.rs` against a private daemon; asserts reconnection and exhaustion handling.                          | `target/logs/long-pw-resilience.log`  |
| **`phase8`**  | `pw-live`      | Live daemon probe, `tests/pw_integration.rs` and `tests/service_resilience.rs` (`SIGTERM`/double-signal acceptances).     | `target/logs/long-pw-live.log`        |
| **`phase9`**  | `recording`    | `io_uring_probe` capability gate, then `tests/recording.rs` and `tests/recording_fault_injection.rs` (`--ignored`).        | `target/logs/long-recording.log`      |
| **`phase10`** | `offline-parity` | `tests/pipe_offline_parity.rs` plus `--diagnose` on the canonical artifact when `--artifact` is used.                      | `target/logs/long-offline-parity.log` |

Every phase writes `target/logs/long-<name>.log`. The human-readable text receipt is `target/logs/long-receipt.txt` and the typed JSONL receipt is `target/logs/long-audit-receipt.jsonl`.

`--only <name>[,<name>]` accepts only canonical phase names. It preserves `target/long` and produces a partial receipt: non-selected phases are recorded as `SKIPPED`, no overall line is written, and the text receipt ends with `OVERALL: PARTIAL (--only: recibo sem overall, não certificável)` — or `OVERALL: FAILED` if any selected phase failed. A partial receipt is not certifiable. Without `--only`, `target/long` is removed and the receipt can be summarized and certified.

### 3.4 Real-Time Criterion Bench

The orphan `utils/tests-performance-regression.sh` wrapper was removed (D-03). [`benches/rt_gate.rs`](../benches/rt_gate.rs) remains available for operator-run Criterion measurements of the RT harness; it is not part of the quick or long disciplines.

- **Coverage Manifest**: [`benches/rt_gate.manifest`](../benches/rt_gate.manifest) declares the scenario groups (`PIPE_Host/*`, `PIPE_AbOpt/*`).
- **Baseline Storage**: Baselines, when an operator keeps them, live in `.performance-baselines/` (gitignored) and are never updated automatically.

---

### 3.5 Manual Desktop Environment Checklist (E-04)

Because desktop environments (GNOME, KDE Plasma, XFCE) handle terminal emulators differently, GUI launcher behavior is validated via a manual checklist after Flatpak installation:

1. **No-Argument Launch**: Launch *NAM Audio Pipe* from the application menu. Expected: Displays usage instructions and exits immediately with code 0.
2. **Bypass Action**: Trigger the *Run in bypass (no model)* action. Expected: Audio streams dry with real-time pass-through.
3. **Diagnostics Action**: Trigger the *Diagnostics* action. Expected: Diagnostic bundle is printed and the terminal remains open until Enter is pressed (`--diagnose --pause`).
4. **Non-Blocking Redirection**: Execute `nam-audio-pipe </dev/null | cat` in a terminal. Expected: Prints help text and exits immediately without blocking.
5. **Flatpak Recording**: Launch with `--record`. Expected: Audio captures save to `$XDG_MUSIC_DIR/NAM-Audio-Pipe` without filesystem permission errors.

---

### 3.6 Empirical Calibration & Evidence Registry (E-01…E-04)

The following empirical measurements require physical hardware calibration by a human operator:

| ID   | Measured Item                                               | Calibration Procedure                                                                 | Status |
|:---- |:----------------------------------------------------------- |:------------------------------------------------------------------------------------- |:------ |
| E-01 | Full-suite test receipts                                    | Operator executes `./utils/tests-long.sh` and submits `target/logs/long-receipt.txt`. | Open   |
| E-02 | WAV finalization latency for 1 GiB capture on SSD / HDD     | Measured via `recording_ab_bench`; documented as `// Measured: finalize=... ms`.      | Open   |
| E-03 | PipeWire daemon resurrection latency (p95 over 20 restarts) | Measured by restarting PipeWire; adjusts reconnect budget if $p95 > 10\text{ s}$.     | Open   |
| E-04 | Desktop launcher integration across desktop environments    | Executed via manual checklist in §3.5.                                                | Open   |

---

### 3.7 Host Memory Protection (Operator-Owned)

The repository never hardcodes compiler parallelism and never re-executes itself under a memory limiter:

- **Parallelism** is controlled exclusively from the host shell profile (`~/.bashrc`), for example by exporting `CARGO_BUILD_JOBS` on memory-constrained machines. No script and no `.cargo/config.toml` sets a default.
- **Heavy release/PGO builds** may be wrapped by the operator in a host-level cgroup limiter. Containment is an external, opt-in decision; the QA disciplines do not provision it.
- **IDE protection**: lowering the IDE process `oom_score_adj` (requires privileges) keeps the kernel from targeting the editor before compiler workers.

---

## 4. Build & Release Chain ([`utils/build-release.sh`](../utils/build-release.sh))

`NAM-Audio-Pipe` employs a multi-stage Profile-Guided Optimization (PGO) and LLVM BOLT pipeline to maximize cache locality and execution speed. The build discipline **only builds**: it never runs tests, and it always deposits the canonical artifact at `target/dist/nam-audio-pipe` with `target/dist/build-receipt.json`.

```mermaid
flowchart TD
    A["Source Code (x86-64-v3)"] --> B["env_verify"]
    B --> C["pgo_profile: instrument & capture workload"]
    C --> D["compile_binary: PGO recompilation (-Cprofile-use)"]
    D --> E["bolt_optimize: LLVM BOLT layout (strict: proof required)"]
    E --> F["codegen_report: filtered DSP hotspot gates"]
    F --> G["deliver_artifact: target/dist + build receipt"]
    G --> H["install: ~/.local/bin (optional)"]
    G --> I["packaging: tarball / Flatpak (optional)"]
    G --> J["certify: reconcile artifact + receipts (no recompile)"]
```

### 4.0 Release Chain

The operator (or the external release ceremony) runs exactly three steps:

1. `./utils/build-release.sh --strict [--package] [--install]` — builds and emits `build-receipt.json`.
2. `./utils/tests-long.sh --strict --artifact` — audits the canonical artifact and binds every receipt line to its SHA-256.
3. `./utils/build-release.sh --certify` — hashes the artifact again, reconciles the build and long receipts, and writes `target/dist/release-receipt.json` with `CERTIFIED` or `REJECTED`. It never recompiles.

> **Prerequisite for BOLT-capable hosts:** run `./utils/ab-opt-ceremony.sh` before the strict build. `build-release.sh --strict` refuses BOLT certification without `target/logs/ab-opt-ceremony-receipt.json` carrying a valid `PGO+BOLT` proof.

### 4.1 PGO Workload Coverage Matrix

During Phase 2, `src/bin/pgo_workload.rs` executes a comprehensive training matrix across all DSP kernels:

| Matrix Dimension     | Evaluated Values                   | Weight Distribution                  |
|:-------------------- |:---------------------------------- |:------------------------------------ |
| **Sample Rates**     | 44.1 kHz, 48.0 kHz, 96.0 kHz       | 25% / 50% / 25%                      |
| **Quantum Sizes**    | 32, 64, 128, 256, 512, 1024 frames | 10% / **40%** / 15% / 20% / 10% / 5% |
| **Model Topologies** | WaveNet A1, WaveNet A2, LSTM       | Equal (33.3% each)                   |
| **Oversampling**     | Off, 2×, 4×                        | 60% / 20% / 20%                      |
| **Cabinet IR**       | Enabled (Convolution), Bypass      | 70% / 30%                            |
| **Noise Gate**       | Enabled, Disabled                  | 50% / 50%                            |
| **Recording Pool**   | Enabled, Disabled                  | 50% / 50%                            |

The workload generates `target/logs/pgo-workload-receipt.json`, verifying that every model topology processes at least 1,000 blocks and no DSP stage is skipped.

### 4.2 Static Assembly & Microarchitectural Gates (Phase 4.5)

After BOLT layout optimization, `utils/dsp-hotpath-filter.py` analyzes the disassembled hot-path (`target/dsp_hotpath.asm`), enforcing strict static architectural rules:

- **Zero AVX-512 `zmm` registers**: Enforces baseline `x86-64-v3` compatibility.
- **Zero `libm` calls in RT paths**: Prohibits scalar standard library math calls in audio loops.
- **Zero `alloc` / `__rust_alloc` symbols**: Prohibits heap allocation in DSP kernels.

### 4.3 A/B Optimization Ceremony ([`utils/ab-opt-ceremony.sh`](../utils/ab-opt-ceremony.sh))

Operator-only class D tool. It builds and measures three variants: `plain` → `PGO` → `PGO+BOLT`. Each measured run is pinned through the typed `pipe_perf_gate` (explicit `NAM_BENCH_CORE` → first online isolated CPU → first permitted CPU → unpinned `GAP`) and bounded by a per-variant timeout. `NAM_THERMAL_COOLDOWN_S` (default `0`, integer) inserts the optional settling time between variants. The comparison stage is the typed `pipe_ab_opt` binary, which writes `target/logs/ab-opt-ceremony-receipt.json`. If BOLT degrades the p99 tail latency against PGO, the verdict is the explicit **PGO-ONLY** fallback; a BOLT gain is never announced without measured proof.

---

## 5. Developer Workflow & Execution Guide

All test commands must be executed from the root of the `./NAM-Audio-Pipe/` repository:

### 5.1 Utility Scripts Reference

| Script                         | Primary Responsibility                                                                 | Target Audience / Frequency    |
|:------------------------------ |:-------------------------------------------------------------------------------------- |:------------------------------ |
| **`utils/lints.sh`**           | Class A: formatting applied (verified in CI), check/clippy matrix, doc, SPDX, text policies, packaging. | Continuous / Every commit |
| **`utils/tests-quick.sh`**     | Class A: `cargo test --features testing` in dev, skipping `#[ignore]`.                 | End of sprint                  |
| **`utils/tests-long.sh`**      | Class B: 10-phase audit (soak, zero-alloc, RT, endurance, live PipeWire, io_uring, f64 parity). | Operator / Nightly / Release |
| **`utils/build-release.sh`**   | Class C: builds the PGO/BOLT artifact and receipt; `--certify` reconciles without recompiling. | Production releases |
| **`utils/ab-opt-ceremony.sh`** | Class D: operator A/B ceremony (`plain → PGO → PGO+BOLT`) with typed comparison.      | Release optimization validation |
| **`utils/miri-audit.sh`**      | Class D: out-of-band formal aliasing and data-race audit for lock-free rings.          | Maintenance / Unsafe code audit |

### 5.2 Direct Command Cheat Sheet

```bash
# -----------------------------------------------------------------------------
# 1. Static Quality Gate
# -----------------------------------------------------------------------------
./utils/lints.sh

# -----------------------------------------------------------------------------
# 2. Quick Discipline (development profile, non-ignored targets)
# -----------------------------------------------------------------------------
./utils/tests-quick.sh

# -----------------------------------------------------------------------------
# 3. Unit tests only
# -----------------------------------------------------------------------------
cargo test --features testing --lib

# -----------------------------------------------------------------------------
# 4. CLI end-to-end black-box tests
# -----------------------------------------------------------------------------
cargo test --features testing --test e2e_cli

# -----------------------------------------------------------------------------
# 5. Live PipeWire integration (requires a running daemon)
# -----------------------------------------------------------------------------
cargo test --features testing --release --test pw_integration -- --ignored --nocapture

# -----------------------------------------------------------------------------
# 6. Asynchronous recording integration (io_uring)
# -----------------------------------------------------------------------------
cargo test --features testing --release --test recording -- --ignored --nocapture

# -----------------------------------------------------------------------------
# 7. Distribution QA & packaging audit
# -----------------------------------------------------------------------------
cargo test --features testing --test distribution_qa

# -----------------------------------------------------------------------------
# 8. Release chain (human operator only): build -> long audit -> certify
# -----------------------------------------------------------------------------
./utils/build-release.sh --strict --install
./utils/tests-long.sh --strict --artifact
./utils/build-release.sh --certify
```
```

---

## 6. Architectural Contracts & Quality Gates

The following contractual rules are enforced across test suites and CI checks:

| Quality Gate                | Target Contract / Boundary Threshold                                                                                                       | Enforcement Location                                       |
|:--------------------------- |:------------------------------------------------------------------------------------------------------------------------------------------ |:---------------------------------------------------------- |
| **Real-Time Memory Safety** | Exactly 0 heap allocations / deallocations in the RT callback.                                                                             | Verified under `feature = "heap-audit"`                    |
| **Lock-Free Concurrency**   | Zero mutex locks or blocking system calls on the audio thread.                                                                             | Architecture review & static analysis                      |
| **WAV Header Integrity**    | Finalized WAV `data` chunk header size must match the exact PCM payload bytes written to disk.                                             | `tests/recording.rs`, `tests/recording_fault_injection.rs` |
| **Buffer Size Domain**      | Strictly `{0} ∪ {2^k \| 16 ≤ 2^k ≤ 8192}`. Out-of-domain inputs reject fail-fast with typed `BufferSizeError`.                             | `src/standalone/cli.rs`, `tests/e2e_cli.rs`                |
| **Silence Gate Domain**     | Polymorphic mode: `on`/`off` or `[-96.0, -20.0]` dBFS threshold with 10 dB Schmitt hysteresis. Rejected fail-fast on malformed inputs.     | `src/standalone/cli.rs`, `tests/e2e_cli.rs`                |
| **RT Quantum Bounds**       | Maximum supported quantum is `MAX_BRIDGE_BUF` (8192 frames / 32,768 bytes). Larger descriptors reject fail-closed.                         | `src/standalone/pw_host/rt_callback/process.rs`            |
| **RT Deadline Processing**  | Worst-case processing per quantum must be < 85% of quantum budget `(N/SR)` over 10,000 quantums.                                           | `tests/rt_metrics.rs`, `utils/tests-long.sh` (`rt-deadline` phase)     |
| **Target CPU Baseline**     | Strictly `x86-64-v3` (AVX2, FMA, BMI2). No scalar runtime fallbacks.                                                                       | `.cargo/config.toml`                                       |
| **AppStream XML Validity**  | Metainfo parses under strict XML; exactly one `<release>` tag matching `CARGO_PKG_VERSION` with a valid `date`; id/name/launchable align with the desktop entry. | `tests/distribution_qa.rs`, `utils/lints.sh`, `utils/build-release.sh` |
| **Release Provenance**      | `target/dist/build-receipt.json` records the artifact SHA-256, git provenance, engine provenance from `Cargo.lock` and PGO/BOLT evidence; `release-receipt.json` records `CERTIFIED`/`REJECTED`. | `src/receipt/build.rs`, `src/receipt/release.rs`, `utils/build-release.sh` |

---

### 6.1 Out-of-Band Miri Concurrency & Aliasing Audit

Standard builds and CI pipelines use the stable Rust toolchain. To formally verify pointer provenance and absence of data races under the Stacked Borrows and Tree Borrows models, an out-of-band Miri script is provided:

```bash
# Optional manual verification (requires nightly toolchain + miri):
./utils/miri-audit.sh
```

**Miri Audit Scope**:

1. **`src/recording/pool_test.rs` (`RecordingPool`)**: 11/11 tests pass under Miri, verifying lock-free SPSC ring ordering and atomic descriptor ownership without data races.
2. **`src/standalone/pw_host/pw_host_test.rs` (`test_dsp_bridge_concurrent_access`)**: Exercises the shared ping-pong memory bridge under concurrent writer and reader threads. `DspBridge::buffers` wraps storage in `UnsafeCell`, accessing payloads through raw pointers to avoid invalid whole-struct retags.
3. **Exclusions**: Full neural network SIMD evaluation kernels are excluded from Miri due to vector intrinsic emulation overhead (~10,000× slowdown).

---

## 7. Script Contract & Automation Disciplines

All automation scripts in `utils/` follow a lean, predictable contract designed for deterministic automation, reproducible CI, and transparent developer workflows.

### 7.1 Script Classes & Error Handling

| Class | Scripts | Error Handling | Rationale |
|:----- |:------- |:-------------- |:--------- |
| **Class A — Interactive** | `lints.sh`, `tests-quick.sh` | `set -euo pipefail`; aborts on the first failure; `ERR` trap emits `RESULT: FAIL` | Real-time developer loops; immediate failure conserves cycles. |
| **Class B — Decoupled Audit** | `tests-long.sh` | `set -o pipefail` (no `-e`, no `-u`); every phase records its status; exit `1` at the end if any phase failed | Nightly/pre-release audits must collect all phase evidence without aborting. |
| **Class C — Build** | `build-release.sh` | `set -euo pipefail`; aborts on the first failure | An intermediate build failure invalidates all downstream artifacts. |
| **Class D — Manual Tool** | `ab-opt-ceremony.sh`, `miri-audit.sh`, `mod-update.sh` | `set -euo pipefail` | Operator-supervised, opt-in tools. |

### 7.2 Output Protocol & Exit Codes

1. **Phase announcement:** `PHASE: <phase_name>`
2. **Phase completion:** `PHASE: <phase_name> PASS|FAIL|GAP ... log=<path>`
3. **Unified final line:** `RESULT: PASS|PASS_WITH_GAPS|FAIL (<seconds>s) fails=<list|none> gaps=<list|none>`
4. **Exit codes:** `0` success or `PASS_WITH_GAPS` outside strict mode; `1` failure or a gap under `--strict`; `2` usage error or missing precondition.

Detailed logs go to `target/logs/<script>-<phase>.log` through `tee`; the long suite uses `target/logs/long-<name>.log`. The canonical `PHASE:`/`RESULT:` lines are also appended to `target/logs/summary.txt` and published by the local `.github/actions/qa-summary` composite action; the ARM CI job now invokes `utils/lints.sh` (with the packaging dependencies installed) instead of an inline clippy command. The quick discipline never emits receipts; the long discipline emits the JSONL receipt consumed by the release chain.

### 7.3 Core Automation Principles

- **No tests of test tooling:** suites validate the audio product (DSP, inference, PipeWire host, CLAP/standalone CLI, packaging). Meta-tests over scripts, receipt schemas, or internal QA tools are forbidden.
- **Quick never provisions:** `tests-quick.sh` uses only fixtures and tooling already present; it never clones, downloads, or builds release artifacts.
- **Long collects all failures:** `tests-long.sh` completes every preflight and phase, aggregating passes, gaps, and failures without early termination.
- **Convention over parameterization:** defaults are the host's natural behavior (all cores, zero thermal cooldown, unpinned). Variables only override.
- **Scripts stay self-contained:** no relative references to sibling repositories and no `source` outside the repository; shell logic is minimal and typed policy lives in Rust binaries.

### 7.4 Canonical Command-Line Flags

| Script | Supported Flags | Notes |
|:------ |:--------------- |:----- |
| **All scripts** | `-h`, `--help` | Concise usage; exit 0. |
| `lints.sh` | None | Zero-flag policy; formatting is applied locally and verified with `CI=true`. |
| `tests-quick.sh` | None | No gap-allowance or simulation flags. |
| `tests-long.sh` | `--strict`, `--artifact`, `--only <name>[,<name>]` | `--strict` turns any `GAP` into `FAIL`; `--artifact` binds the receipt to `target/dist/nam-audio-pipe`; `--only` selects canonical phases and yields a partial, non-certifiable receipt. |
| `build-release.sh` | `--strict`, `--package`, `--install`, `--certify` | `--package` adds tarball/Flatpak; `--install` installs locally; `--certify` validates receipts without recompiling. |
| `ab-opt-ceremony.sh` | `--runs`, `--blocks`, `--no-pgo`, `--no-bolt`, `--bench-extra` | Operator-only ceremony controls. |
| `miri-audit.sh`, `mod-update.sh` | None | Manual tools. |

### 7.5 Canonical Environment Variables

#### Interface Variables (User & CI Control)

| Variable | Default | Scope | Purpose & Semantics |
|:-------- |:------- |:----- |:------------------- |
| `CI` | Unset | CI runner | When `true`, `lints.sh` verifies formatting with `--check` instead of applying it. |
| `NAM_THERMAL_COOLDOWN_S` | `0` | A/B ceremony | Non-negative integer seconds between measured variants. An invalid value exits `2`. |
| `NAM_BENCH_CORE` | `auto` | A/B / RT | Bench-core precedence: explicit CPU → first online isolated CPU → first permitted CPU → unpinned `GAP`. |
| `NAM_PHASE_TIMEOUT_S` | Per-command default | `tests-long.sh` | Overrides the timeout applied to each long-audit phase command. |
| `NAM_ENDURANCE_SECONDS` | Harness default | `tests/endurance.rs` | Wall-clock endurance window. |

#### Harness Variables (Internal Script → Rust Communication)

| Variable | Configured By | Read By | Purpose |
|:-------- |:------------- |:------- |:------- |
| `NAM_RUN_ID` | `utils/_lib.sh` | Test harnesses | Unique run identifier for logs and receipt isolation. |
| `NAM_RT_STRICT` | `tests-long.sh --strict` | `tests/rt_metrics.rs` | Promotes RT GAP conditions to hard failures. |

### 7.6 Measured Durations

| Discipline / Script | Measured Duration | Budget Ceiling | Notes |
|:------------------- |:----------------- |:-------------- |:----- |
| `utils/lints.sh` | ≈13 s (warm cache, 2026-10-27) a ≈20 s | ≤ 2 min | fmt + check/clippy matrix + doc + SPDX + policies + packaging. |
| `utils/tests-quick.sh` | ≈41 s (warm cache, 2026-10-27) | ≤ 5 min | All non-ignored targets in the development profile. |
| `utils/tests-long.sh` | Not measured (per-phase timeouts) | — | Operator-run; collects all failures. |
| `utils/build-release.sh` | ≈204 s (PGO-only reference run) | — | Full BOLT time depends on the host toolchain and `perf` permissions. |

#### QA footprint (métrica F-2, 2026-10-27)

`wc -l`: shell **1.502**, py/awk **362**, Rust QA (`src/bin/*`) **6.024** →
**7.888 linhas** de QA total. O bin órfão `long_receipt_check` (39 linhas) foi
removido; os demais bins têm chamador ou são instrumento manual documentado.

---

## 8. References

- [NAM-Audio-Pipe Architecture](architecture.md) — Comprehensive threading model, host lifecycle, and memory layout.
- [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) — Agnostic neural DSP engine.
- [PipeWire Official Documentation](https://docs.pipewire.org/) — Linux audio graph daemon and SPA API reference.
