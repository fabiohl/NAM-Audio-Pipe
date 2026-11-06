<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (c) 2026 Fábio Henrique de Lima Silva (fhl.bsb@gmail.com) All rights reserved.
-->

# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0] - 2026-11-06

### Added [1.1.0]

- **Linux on ARM (ARM64 / aarch64) Platform Documentation:** Documented native Linux on ARM platform status across `README.md` and `docs/architecture.md`. Clarified that the project maintains continuous integration on native ARM64 runners (`ubuntu-26.04-arm`) with blocking compilation and lint gates (`linux-arm64-lints`) and theoretical software compatibility, while formal bare-metal real-time audio testing and latency certification remain pending.
- **PipeWire Latency Reporting:** Stream properties and latency reporting driven by the real PipeWire graph.
- **Rust QA Receipts:** Long-phase receipt tooling in Rust with `run_id` support (flags or `NAM_RUN_ID`), unified phase logging and script-budget validation.
- **CI Provenance:** Independent lint and quick jobs, engine provenance logging and an adjusted long-workflow timeout.
- **A/B Optimization Ceremony Tooling:** Added `utils/ab-opt-ceremony.sh` and the `pipe_ab_opt` comparison binary backed by `src/receipt/ab_opt.rs`, generating `ab-opt-ceremony-receipt.json` with median P99 cycle evaluation across neural topologies (`wavenet_a1`, `wavenet_a2`, `lstm`) and an explicit 1.0% promotion margin rule.
- **PipeWire SPA Latency Architecture Decision Record:** Added `docs/latency-spa-adr.md` defining PipeWire SPA latency accounting (`SPA_PARAM_Latency`), sample-rate vs nanosecond evaluation invariants, UPOLS partition alignment, and live graph latency publication.
- **Engine Dependency Pinning:** Added `variables.env` as the authoritative single source of truth for upstream/sibling `NeuralAmpModeler-rs` engine repository, release tag (`v2.0.0`), and pinned commit hash.

### Changed [1.1.0]

- **Receipt and Workflow Architecture:** Migrated the QA script architecture to Rust-based receipts and workflows; cleaned up RT log-ring tests and modularized test log utilities.
- **Recording and Standalone Modules:** Improved recording, receipt and standalone module structure with hardened error handling.
- **CPU Headroom Documentation:** Updated CPU headroom benchmarks and PipeWire host execution metrics.
- **Memory Guard:** Test and build scripts run under the project's memory guard to protect the host from OOM.
- **PipeWire Host & Control Plane Architecture:** Decoupled PipeWire host initialization and stream negotiation into dedicated modules (`spa_format.rs`, `stream_status.rs`, `os_swap.rs`, `slimmable_swap.rs`), replacing polling sleeps with deterministic event-driven control plane wakeups (`ControlPlaneWakeup`).
- **CLI Domain Extraction:** Refactored CLI argument parsing by moving domain models, path expansion, and argument validation into `src/standalone/cli_domain.rs`, streamlining `src/standalone/cli.rs`.
- **Release Pipeline Hardening:** Updated `utils/build-release.sh` to enforce ceremony proofs under `--strict`, handle fallback to PGO-only when BOLT does not meet the promotion margin, and ensure non-recompiling `--certify` receipt reconciliation.

### Fixed [1.1.0]

- **Telemetry Startup:** Suppressed false-positive telemetry startup drops and updated the Flatpak PipeWire permissions.
- **Cab-sim Latency Tests:** Consumer tests now follow the engine's constant partition-sized priming latency (the ESR stream is compared after the full partition prefix).
- **Recording Pool Pointer Safety:** Removed nullable raw pointer (`*mut u8`) in `RecordingPool` blocks (`src/recording/pool.rs`), substituting safe references and hardened slot validation (#2).
- **Long Audit Evidence:** Partial `--only` runs write sibling receipts and preserve previous full audit evidence; full runs are validated via `pipe_long_receipt validate --strict`, and failed phases emit a typed `reason`.
- **PipeWire Capabilities:** Live and recording tests fail closed instead of silently passing when the PipeWire daemon, `pw-play`, or `io_uring` is unavailable; the long audit suite declares those absences as typed GAPs before running.
- **Release Provenance & Packaging:** `--certify` never recompiles, the verifier is located without parsing help text, build and long `run_id` values are reconciled through `target/logs/build-run-id`, stale release receipts are purged before a failed certification, and `LICENSE.txt` plus the engine `NOTICE` are bundled into distribution tarballs and Flatpak.
- **CI Provenance & Matrix:** Hosted long workflow runs exclude isolated hardware phases (`rt-deadline`, `rt-jitter`), MSRV job records and uploads provenance logs, and workflow steps use pinned engine references.
- **RT Log-Ring Robustness:** Hardened atomic ring buffer boundaries and log redirection to prevent buffer overflows or corrupted log lines under high-throughput logging.

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
