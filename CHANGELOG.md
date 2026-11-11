<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Flatpak Runtime Migration to Freedesktop SDK 26.08:** Upgraded `packaging/flatpak/io.github.fabiohl.NAMAudioPipe.yml` and `utils/build-release.sh` to target `org.freedesktop.Platform//26.08` and `org.freedesktop.Sdk//26.08`, leveraging glibc 2.44, PipeWire 1.6.x client IPC, and Wayland 1.26.

### Fixed

- **Release Ceremony Strict Certification:** Enforced `--strict` flag propagation in `.agents/scripts/release-ceremony.sh` when invoking `utils/build-release.sh --strict --certify --install`, ensuring that `release-receipt.json` accurately asserts `strict_mode: true`.
- **Hardware PMU Counters in Long Audit Suite:** Added automatic elevation (`kernel.perf_event_paranoid = 1`) and fail-safe restoration via EXIT trap in `utils/tests-long.sh`, preventing `EACCES (Permission denied)` when accessing PMU cycle and instruction counters for real-time IPC calculation.
- **Zero Page Faults in RT Measurement Loops:** Hardened real-time deadline and jitter harnesses in `tests/rt_metrics.rs` by calling `configure_process_wide()` (`mlockall` + `disable_thp` + `mallopt` heap trimming disable), touching the RT thread stack, and pre-allocating/prefaulting sample buffers to eliminate minor page faults (`minflt`) during nanosecond timing loops.
- **Release Chain Documentation:** Aligned `README.md`, `docs/testing.md`, and `docs/architecture.md` to consistently document `./utils/build-release.sh --strict --certify --install` as the canonical non-recompiling reconciliation step.

## [1.1.0] - 2026-11-07

Version 1.1.0 integrates the `NeuralAmpModeler-rs` v2.0.0 engine core, implements real-time PipeWire SPA latency reporting with live graph feedback, and introduces an automated A/B optimization ceremony (Plain vs. PGO vs. LLVM BOLT) with formal promotion proofs. This release also decouples the host control plane into deterministic event-driven modules, hardens the asynchronous recording pool by eliminating nullable raw pointers (#2), establishes Rust-based QA receipts with end-to-end execution provenance, and documents native Linux on ARM64 platform gates.

### Added [1.1.0]

- **PipeWire SPA Latency Architecture:** Integrated native PipeWire SPA latency reporting and stream properties synchronized with the live multimedia graph (`docs/latency-spa-adr.md`). Implements bidirectional `SPA_PARAM_Latency` parsing and publishing, sample-rate vs. nanosecond evaluation invariants, and constant partition UPOLS cab-sim latency alignment.
- **A/B Optimization Ceremony Tooling:** Added `utils/ab-opt-ceremony.sh` and the `pipe_ab_opt` comparison binary backed by `src/receipt/ab_opt.rs`, generating `ab-opt-ceremony-receipt.json` with median P99 cycle evaluation across neural topologies (`wavenet_a1`, `wavenet_a2`, `lstm`) and an explicit 1.0% promotion margin rule across optimization tiers.
- **Engine Dependency Pinning:** Added `variables.env` as the authoritative single source of truth for upstream/sibling `NeuralAmpModeler-rs` repository, release tag (`v2.0.0`), and pinned commit hash (`2cd14a6f8e01482440d96c32fa3911c8f6583685`) for reproducible builds.
- **Rust QA Receipts:** Long-phase receipt tooling in Rust (`pipe_long_receipt`) with `run_id` support (flags or `NAM_RUN_ID`), unified phase logging, typed validation failure reasons, and automated script-budget validation.
- **Linux on ARM (ARM64 / aarch64) Platform Documentation:** Documented native Linux on ARM platform status across `README.md` and `docs/architecture.md`. Clarified that the project maintains continuous integration on native ARM64 runners (`ubuntu-26.04-arm`) with blocking compilation and lint gates (`linux-arm64-lints`) and theoretical software compatibility, while formal bare-metal real-time audio testing and latency certification remain pending.
- **CI Provenance & Matrix:** Independent lint and quick workflow jobs, automated engine provenance logging, and adjusted long-workflow timeouts with always-uploaded gate diagnostics.

### Changed [1.1.0]

- **PipeWire Host & Control Plane Architecture:** Decoupled PipeWire host initialization, stream lifecycle, and format negotiation into dedicated modules (`spa_format.rs`, `stream_status.rs`, `os_swap.rs`, `slimmable_swap.rs`), replacing polling sleeps with deterministic event-driven control plane wakeups (`ControlPlaneWakeup`).
- **CLI Domain Extraction:** Refactored CLI argument parsing by separating domain models, path expansion, and argument validation into `src/standalone/cli_domain.rs`, streamlining `src/standalone/cli.rs`.
- **Release Pipeline Hardening:** Updated `utils/build-release.sh` to enforce ceremony proofs under `--strict`, handle automated fallback to PGO-only when BOLT does not satisfy the 1.0% promotion margin, and ensure non-recompiling `--certify` receipt reconciliation.
- **Recording & Storage Robustness:** Hardened `RecordingPool` slot pre-allocation, disk space monitoring, and chunk finalization in `src/recording/disk.rs` with safe error handling and clean teardown on termination signals (`SIGINT`, `SIGTERM`).
- **Host Memory Guard:** Enforced execution under the project's memory guard (`.agents/scripts/mem-guard.sh`) across test and build scripts to protect the host against out-of-memory (OOM) conditions.
- **CPU Headroom Documentation:** Updated CPU headroom benchmarks and PipeWire host execution metrics in `README.md` reflecting optimized cycle consumption under modern workloads.

### Fixed [1.1.0]

- **Recording Pool Pointer Safety:** Removed nullable raw pointer (`*mut u8`) in `RecordingPool` blocks (`src/recording/pool.rs`), substituting safe references and hardened slot validation (#2).
- **Endurance Soak Test Baseline & SPSC Command Pre-Drain:** In `tests/common/swap.rs` and `tests/endurance.rs`, pre-drained pending in-flight SPSC commands before running validation callbacks to eliminate command contention/drops, and restored the canonical quiescent baseline topology (Linear A/B, CabSim None, Oversample Off) before taking final steady-state telemetry samples, ensuring first-to-last delta evaluates true memory drift rather than active model working set differences.
- **RT Environment Evidence in Long Receipts:** Updated `utils/tests-long.sh` to extract and propagate calibrated RT environment metadata (`env=...`) from phase logs into long audit text receipt lines (`PHASE<n>: ... env=...`) for end-to-end execution provenance.
- **Long Audit Evidence:** Partial `--only` runs write sibling receipts and preserve previous full audit evidence; full runs are validated via `pipe_long_receipt validate --strict`, and failed phases emit a typed `reason`.
- **PipeWire Capabilities:** Live and recording tests fail closed instead of silently passing when the PipeWire daemon, `pw-play`, or `io_uring` is unavailable; the long audit suite declares those absences as typed GAPs before running.
- **Release Provenance & Packaging:** Enforced that `--certify` never recompiles, the verifier is located without parsing help text, build and long `run_id` values are reconciled through `target/logs/build-run-id`, stale release receipts are purged before a failed certification, and `LICENSE.txt` plus the engine `NOTICE` are bundled into distribution tarballs and Flatpak.
- **CI Provenance & Matrix:** Hosted long workflow runs exclude isolated hardware phases (`rt-deadline`, `rt-jitter`) while declaring explicit GAPs, MSRV job records and uploads provenance logs, and workflow steps use pinned engine references.
- **RT Log-Ring Robustness:** Hardened atomic ring buffer boundaries and log redirection (`RtLogRing`) to prevent buffer overflows, torn log lines, or dropped logs under high-throughput logging.
- **Cab-sim Latency Tests:** Consumer tests now align with the engine's constant partition-sized priming latency (the ESR stream is compared after the full partition prefix).
- **Telemetry Startup:** Suppressed false-positive telemetry startup drops during stream initialization and updated Flatpak PipeWire permissions.

---

## [1.0.0] - 2026-10-20

### Added [1.0.0]

- **Native PipeWire Standalone Host:** Direct integration with `libpipewire-0.3` client API, processing audio over PipeWire multimedia graphs at quantum buffer sizes down to 64 samples (1.33 ms).
- **Core Neural Engine Integration:** Embedded [`NeuralAmpModeler-rs`](https://github.com/fabiohl/NeuralAmpModeler-rs) DSP core with full support for:
  - WaveNet models (A1 and A2 standard and lite profiles).
  - LSTM models (1-layer and 2-layer recurrent network topologies).
  - ConvNet and Linear FIR architectures.
  - Multi-profile `.namb` bundles.
- **Strict Real-Time Safety Guarantees:**
  - Zero heap allocations (`malloc`, `free`, `Box`, `Vec`, `String`, `Arc`) on the audio callback thread.
  - Zero mutexes, `RwLock`s, or blocking system calls on the audio loop.
  - SPSC lock-free ringbuffers and command channels with asynchronous 3-tier Garbage Collection (GC) off-RT.
  - Zero `log::*` invocations on the hot-path, utilizing atomic bitmasks (`RtStatusFlags`) and pre-allocated SPSC log rings.
  - Panic containment via `std::panic::catch_unwind` guards around RT callbacks, preventing process crashes and unfinalized recordings.
- **Cabinet IR UPOLS Convolution:**
  - Partitioned FFT convolution engine for cabinet impulse responses (`.wav`).
  - Configurable partition sizes (`32`, `64`, `128`, `256` samples) via `--cabsim-partition`, allowing custom balance between algorithmic latency and CPU utilization.
  - Optional cabinet loading mode (`--cab-optional`) to warn and continue without cab-sim if an IR is missing or unloadable.
- **Asynchronous 32-Bit Float Stereo WAV Recording:**
  - Powered by Linux `io_uring` (`tokio-uring`) without thread contention or hot-path latency spikes.
  - Automatic 4 GiB RIFF file segmentation (`_partN.wav`).
  - Noise gate silence trimming: only enqueues audio blocks when the silence gate is open.
  - Periodic WAV header sync every 3 seconds for crash durability.
  - Free disk space monitoring and low-space warnings (1 GiB / 5 min thresholds).
  - Clean finalization on termination signals (`SIGINT`, `SIGTERM`) with extended grace window (5 s).
- **Anti-Aliasing Polyphase Oversampling:**
  - Half-band polyphase FIR oversampling (`2x`, `4x`, and `off`) around the non-linear neural inference stage.
- **Adaptive Compute (Auto-Slimming):**
  - Host CPU pressure detection dynamically downgrades multi-profile `.namb` models (`--slim auto`) to prevent buffer underruns during CPU spikes.
- **Production SIMD Vectorization:**
  - Strict `x86-64-v3` architecture baseline (AVX2, FMA, BMI2) achieving < 3% DSP load of a 1.33 ms quantum.
- **Linux Low-Latency OS & Kernel Tuning:**
  - PM QoS DMA latency pinning to 0 µs (`/dev/cpu_dma_latency`).
  - Dynamic `/proc/interrupts` IRQ profiling with automatic audio thread pinning to the least-interrupted CPU core.
  - Memory locking via `mlockall(MCL_CURRENT | MCL_FUTURE)`.
  - Transparent Huge Pages (THP) disablement (`PR_SET_THP_DISABLE`).
- **Resilient Backend Lifecycle:**
  - Bounded automatic reconnection engine handling PipeWire daemon restarts (`systemctl --user restart pipewire`), suspend/resume cycles, and interface unplugging with progressive exponential backoff (6 attempts, 250 ms → 4 s, ~11.75 s budget).
  - Fail-fast mode (`--fail-fast`) and configurable reconnect budget (`--reconnect-timeout`).
- **Command-Line Interface (CLI):**
  - Polymorphic silence gate (`--gate MODE`): `on` (-70 dBFS default with 10 dB Schmitt hysteresis), `off` (pass-through), or explicit dBFS threshold (`[-96, -20] dBFS`).
  - Input staging (`-i, --input-gain`) and output staging (`-o, --output-gain`) from -20.0 to +20.0 dB.
  - Activation precision selection: `standard` (exact) vs `fast` (Padé polynomial approximations).
  - Diagnostic bundles (`--diagnose` and `--diagnose-full`) for technical support and troubleshooting.
  - Classified exit codes (`0` Clean, `1` Forced/generic, `2` Usage, `3` Model, `4` Cab IR, `5` Backend, `6` Recording).
- **Multi-Stage Compiler Optimization Pipeline (`utils/build-release.sh`):**
  - Profile-Guided Optimization (PGO) over multi-topology neural workloads.
  - Machine code basic block reordering via LLVM BOLT.
  - Automated generation of filtered DSP hotspot reports (`target/dsp_hotpath.asm`).
- **Flatpak Distribution (`io.github.fabiohl.NAMAudioPipe`):**
  - Standalone desktop application targeting `org.freedesktop.Platform` 25.08.
  - Scoped low-latency audio permissions (`--filesystem=xdg-run/pipewire-0`, `--socket=pulseaudio`, `--share=ipc`, `--device=all`, `--filesystem=home:ro`, `--filesystem=xdg-music`).
  - XDG desktop integration and complete hicolor icon theme hierarchy (SVG + 64x64 to 512x512 PNGs).

[Unreleased]: https://github.com/fabiohl/NAM-Audio-Pipe/compare/v1.1.0...HEAD

[1.1.0]: https://github.com/fabiohl/NAM-Audio-Pipe/compare/v1.0.0...v1.1.0

[1.0.0]: https://github.com/fabiohl/NAM-Audio-Pipe/releases/tag/v1.0.0
